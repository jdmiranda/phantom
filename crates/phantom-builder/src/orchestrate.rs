//! Builder orchestration.
//!
//! This module is the glue layer that turns a [`crate::BuilderConfig`] into a
//! running pipeline. The work splits into six phases:
//!
//! 1. Clone or attach a local checkout via
//!    [`crate::clone::ensure_local_checkout`].
//! 2. Drop the default four-loop pipeline into `<repo>/.phantom/loops/` via
//!    [`crate::templates::write_default_specs`].
//! 3. Run the loop pre-flight gates (`gh` binary, `gh auth`, MCP names).
//! 4. Construct the loop registry, queue registry, substrate dispatcher, and
//!    substrate driver. Reuses [`phantom_loop`] primitives 1-for-1; nothing
//!    is reimplemented here.
//! 5. Construct the brain handle with a self-improvement reconciler keyed to
//!    the target slug and the safety caps.
//! 6. Spawn the action forwarder — either
//!    [`phantom_loop::LoopQueueActionHandler`] (normal mode) or
//!    [`crate::safety::DryRunActionHandler`] (dry-run mode).
//!
//! Tests inject a custom [`phantom_loop::SubstrateBackend`] and replace the
//! default goal sources via [`BuilderHooks`]. The CLI never touches these
//! hooks — it always builds the production wiring.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use phantom_agents::composer_tools::new_spawn_subagent_queue;
use phantom_brain::brain::{BrainConfig, BrainHandle, spawn_brain};
use phantom_brain::goal_source::{GhCiFailureGoalSource, GhIssueGoalSource, GoalSource};
use phantom_brain::self_improvement::{
    SelfImprovementConfig, SelfImprovementState, TrustBudget,
};
use phantom_loop::{
    ChatBackedSubstrateBackend, LoopHandle, LoopQueueActionHandler, LoopQueueRegistry,
    LoopRegistry, LoopRunner, LoopSource, LoopSourceSpec, LoopSpec, LoopStatus,
    SubstrateAgentDispatcher, SubstrateBackend, SubstrateDriver,
};

use crate::{BuilderConfig, BuilderError};

// ---------------------------------------------------------------------------
// BuilderResult — bookkeeping returned to the caller
// ---------------------------------------------------------------------------

/// Outcome of a [`Builder::run`] invocation.
#[derive(Debug)]
pub struct BuilderResult {
    /// Absolute path to the working copy the builder operated on.
    pub repo_path: PathBuf,
    /// Paths of the loop specs the builder seeded (or skipped because they
    /// already existed).
    pub seeded_specs: Vec<PathBuf>,
    /// Number of loops successfully started.
    pub started_loops: usize,
}

// ---------------------------------------------------------------------------
// BuilderHooks — test injection surface
// ---------------------------------------------------------------------------

/// Optional dependency-injection seams used by integration tests.
///
/// Production callers leave this at [`BuilderHooks::default`]. Tests
/// substitute:
///
/// - `substrate_backend`: a [`phantom_loop::MockSubstrateBackend`] so spawned
///   agents return canned outcomes instead of hitting Claude.
/// - `goal_sources`: a pre-built `Vec<Box<dyn GoalSource>>` with stub `gh`
///   runners that return canned issues / CI failures. When `None`, the
///   orchestrator builds the production sources via [`default_goal_sources`].
/// - `skip_preflight`: when true, skip the `gh` binary / auth / runlock
///   gates. Production sets this to false; tests set it to true so the
///   smoke test does not require `gh` installed.
#[derive(Default)]
pub struct BuilderHooks {
    pub substrate_backend: Option<Arc<dyn SubstrateBackend>>,
    pub goal_sources: Option<Vec<Box<dyn GoalSource>>>,
    pub skip_preflight: bool,
}

// ---------------------------------------------------------------------------
// Builder — top-level orchestrator
// ---------------------------------------------------------------------------

/// Top-level builder orchestrator.
///
/// Constructed from a [`BuilderConfig`] and consumed by one of:
///
/// - [`Builder::run`] — production path; blocks on Ctrl-C.
/// - [`Builder::run_for_duration`] — test path; runs the runtime for a fixed
///   duration then tears down.
pub struct Builder {
    config: BuilderConfig,
    hooks: BuilderHooks,
}

impl Builder {
    /// Build with production hooks.
    #[must_use]
    pub fn new(config: BuilderConfig) -> Self {
        Self {
            config,
            hooks: BuilderHooks::default(),
        }
    }

    /// Build with explicit hooks. Tests use this; the CLI calls [`Self::new`].
    #[must_use]
    pub fn with_hooks(config: BuilderConfig, hooks: BuilderHooks) -> Self {
        Self { config, hooks }
    }

    /// Resolve a working-copy path for the target slug.
    pub fn resolve_path(&self) -> Result<PathBuf, BuilderError> {
        crate::clone::ensure_local_checkout(
            &self.config.target_slug,
            self.config.repo_path.as_deref(),
        )
    }

    /// Seed default loop specs onto disk.
    pub fn seed_specs(&self, repo_path: &std::path::Path) -> Result<Vec<PathBuf>, BuilderError> {
        crate::templates::write_default_specs(repo_path, &self.config.target_slug)
    }

    /// Production entry-point. Resolves the checkout, seeds the specs, boots
    /// the runtime + brain, and blocks on Ctrl-C.
    pub fn run(self) -> Result<BuilderResult, BuilderError> {
        let artifacts = self.run_inner()?;
        let runtime = artifacts
            .runtime
            .as_ref()
            .expect("runtime is None only after Drop runs");
        runtime.block_on(async {
            if let Err(e) = tokio::signal::ctrl_c().await {
                tracing::warn!("ctrl-c handler error: {e}");
            }
        });
        // Mirror what `RunArtifacts::Drop` would do, but capture the result
        // before the artifacts are consumed.
        let result_clone = BuilderResult {
            repo_path: artifacts.result.repo_path.clone(),
            seeded_specs: artifacts.result.seeded_specs.clone(),
            started_loops: artifacts.result.started_loops,
        };
        artifacts.request_stop();
        // artifacts dropped here → runtime shut down with timeout.
        drop(artifacts);
        Ok(result_clone)
    }

    /// Test entry-point. Runs the runtime for `duration` then tears down.
    /// Returns ownership of the queue registry plus the runtime itself so
    /// the test can poll queue depth before drop.
    ///
    /// This is the **synchronous** test entry-point: the call sleeps the
    /// current thread for `duration` while the builder's internal runtime
    /// drives the loop + brain on its own threads. Tests must invoke this
    /// from a plain `#[test]` (not `#[tokio::test]`) so that the builder's
    /// own multi-thread runtime is not nested inside an outer runtime.
    pub fn run_for_duration(
        self,
        duration: std::time::Duration,
    ) -> Result<RunArtifacts, BuilderError> {
        let artifacts = self.run_inner()?;
        // The runtime is alive on its own threads — sleep the caller and
        // then issue an explicit teardown.
        std::thread::sleep(duration);
        for snap in artifacts.registry.list() {
            let _ = artifacts.registry.stop(snap.id);
        }
        artifacts.driver_handle.abort();
        Ok(artifacts)
    }

    /// Internal: build every runtime component and return them packaged in
    /// [`RunArtifacts`] so either entry-point can tear them down on its own
    /// terms.
    fn run_inner(self) -> Result<RunArtifacts, BuilderError> {
        let Builder { config, hooks } = self;
        let BuilderHooks {
            substrate_backend,
            goal_sources: hook_goal_sources,
            skip_preflight,
        } = hooks;

        // -- Phase 1: clone / attach ----------------------------------------
        let repo_path = crate::clone::ensure_local_checkout(
            &config.target_slug,
            config.repo_path.as_deref(),
        )?;
        tracing::info!(path = %repo_path.display(), "builder resolved local checkout");

        // -- Phase 2: seed specs --------------------------------------------
        let seeded_specs =
            crate::templates::write_default_specs(&repo_path, &config.target_slug)?;
        tracing::info!(count = seeded_specs.len(), "builder seeded loop specs");

        // -- Phase 3: preflight ---------------------------------------------
        if !skip_preflight {
            phantom_loop::check_gh_binary().map_err(|e| BuilderError::Preflight(e.to_string()))?;
            phantom_loop::check_gh_auth().map_err(|e| BuilderError::Preflight(e.to_string()))?;
            let no_mcp: Vec<&str> = Vec::new();
            phantom_loop::check_mcp_collisions(no_mcp)
                .map_err(|e| BuilderError::Preflight(e.to_string()))?;
        }

        // -- Phase 4: discover the seeded specs we plan to start ------------
        let specs_dir = repo_path.join(".phantom").join("loops");
        let all_specs = discover_specs(&specs_dir)?;
        if all_specs.is_empty() {
            return Err(BuilderError::Spec(format!(
                "no loop specs at {} after seeding — \
                 the templates module is likely misconfigured",
                specs_dir.display()
            )));
        }

        let mut targeted: Vec<(LoopSpec, Option<phantom_loop::ExitSchema>)> = Vec::new();
        for name in &config.loops {
            match all_specs.iter().find(|(s, _)| &s.id == name) {
                Some((s, schema)) => {
                    let mut spec = s.clone();
                    let cap = config.safety.max_concurrent_agents;
                    if spec.max_concurrent > cap {
                        spec.max_concurrent = cap.max(1);
                    }
                    targeted.push((spec, schema.clone()));
                }
                None => {
                    return Err(BuilderError::Spec(format!(
                        "requested loop `{name}` not found among seeded specs (have: {})",
                        all_specs
                            .iter()
                            .map(|(s, _)| s.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }

        // -- Phase 5: build the tokio runtime + loop runtime ----------------
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| BuilderError::Other(format!("tokio runtime build failed: {e}")))?;

        let registry = Arc::new(LoopRegistry::new());
        let queues = Arc::new(LoopQueueRegistry::new());
        let spawn_queue = new_spawn_subagent_queue();
        let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(
            spawn_queue.clone(),
        ));

        let backend: Arc<dyn SubstrateBackend> = substrate_backend.unwrap_or_else(|| {
            Arc::new(ChatBackedSubstrateBackend::default()) as Arc<dyn SubstrateBackend>
        });
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::channel::<phantom_protocol::Event>(64);
        let driver = SubstrateDriver::new(spawn_queue.clone(), backend, event_tx);
        let router = dispatcher.completion_router();

        let registry_clone = Arc::clone(&registry);
        let queues_clone = Arc::clone(&queues);
        let dispatcher_clone: Arc<dyn phantom_loop::AgentDispatcher> = dispatcher.clone();
        let driver_handle = runtime.block_on(async move {
            let driver_join = driver.run();
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    router.on_completion(event);
                }
            });

            for (spec, schema) in targeted {
                let id = registry_clone.alloc_id();
                let status = Arc::new(std::sync::Mutex::new(LoopStatus::Idle));
                let spec_id = spec.id.clone();
                let source = build_source(&spec, &queues_clone)?;
                let runner = LoopRunner::new(
                    Arc::new(spec),
                    schema,
                    source,
                    Arc::clone(&queues_clone),
                    Arc::clone(&dispatcher_clone),
                );
                let status_for_task = Arc::clone(&status);
                let join_handle = tokio::spawn(async move {
                    let reason = runner.run().await;
                    if let Ok(mut s) = status_for_task.lock() {
                        *s = LoopStatus::Stopped { reason };
                    }
                });
                registry_clone.register(
                    id,
                    LoopHandle {
                        spec_id,
                        status,
                        started_at: SystemTime::now(),
                        join_handle: Some(join_handle),
                    },
                );
            }
            Result::<_, BuilderError>::Ok(driver_join)
        })?;
        let started_loops = registry.list().len();

        // -- Phase 6: brain boot --------------------------------------------
        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dry_count = boot_brain(
            &config,
            hook_goal_sources,
            &repo_path,
            &runtime,
            Arc::clone(&queues),
            Arc::clone(&stop_flag),
        )?;

        Ok(RunArtifacts {
            result: BuilderResult {
                repo_path,
                seeded_specs,
                started_loops,
            },
            runtime: Some(runtime),
            registry,
            driver_handle,
            queues,
            dry_count,
            stop_flag,
        })
    }
}

// ---------------------------------------------------------------------------
// RunArtifacts — the test-visible bundle of running components
// ---------------------------------------------------------------------------

/// Bundle of everything `run_inner` constructs. The production path uses
/// only the `result` + `runtime` (the rest is held to keep the runtime alive
/// for the brain forwarder and substrate driver). Tests use every field.
pub struct RunArtifacts {
    /// Caller-visible summary.
    pub result: BuilderResult,
    /// The tokio runtime hosting every async task. Held by the caller so
    /// the runtime is not dropped (which would abort the brain forwarder).
    ///
    /// Wrapped in `Option` so [`Self::drop`] can `take()` it and shut it
    /// down with an explicit `shutdown_timeout` — otherwise the runtime's
    /// own `Drop` waits for spawn_blocking tasks that loop forever.
    pub runtime: Option<tokio::runtime::Runtime>,
    /// The loop registry — tests inspect started_loops; production teardown
    /// iterates this on Ctrl-C.
    pub registry: Arc<LoopRegistry>,
    /// Substrate driver tick task; aborted on teardown.
    pub driver_handle: tokio::task::JoinHandle<()>,
    /// Shared cross-loop queue registry. Tests assert what landed here.
    pub queues: Arc<LoopQueueRegistry>,
    /// When the brain was booted in dry-run mode, a shared counter the test
    /// can use to assert that exactly N would-be-enqueue events fired.
    /// `None` in normal mode.
    pub dry_count: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// Signal flag the brain action forwarder polls between
    /// `try_recv_action` checks. Setting it to `true` (via
    /// [`Self::request_stop`]) lets the forwarder loop exit cleanly so
    /// the runtime can shut down without waiting for the spawn_blocking
    /// task forever.
    pub stop_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl RunArtifacts {
    /// Signal the brain action forwarder to exit. Used by tests during
    /// teardown — call this before dropping the artifacts to avoid the
    /// runtime hanging on its spawn_blocking tasks.
    pub fn request_stop(&self) {
        self.stop_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.driver_handle.abort();
    }
}

impl Drop for RunArtifacts {
    fn drop(&mut self) {
        // Signal the forwarder, then shut down the runtime with a timeout
        // so the test does not hang if the forwarder is currently inside
        // a 50 ms sleep when stop_flag flips.
        self.stop_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.driver_handle.abort();
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_millis(500));
        }
    }
}

// ---------------------------------------------------------------------------
// Brain boot — separate function so it consumes hooks cleanly
// ---------------------------------------------------------------------------

/// Build the brain handle and start the action forwarder.
///
/// Returns the dry-run counter when the safety config is in dry-run mode, or
/// `None` otherwise.
fn boot_brain(
    config: &BuilderConfig,
    hook_goal_sources: Option<Vec<Box<dyn GoalSource>>>,
    repo_path: &std::path::Path,
    runtime: &tokio::runtime::Runtime,
    queues: Arc<LoopQueueRegistry>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
) -> Result<Option<Arc<std::sync::atomic::AtomicUsize>>, BuilderError> {
    if matches!(config.trust_band, crate::TrustBandConfig::SuggestionOnly) {
        tracing::info!("trust band is SuggestionOnly — skipping brain boot");
        return Ok(None);
    }

    let safety = &config.safety;
    let si_config = SelfImprovementConfig {
        enabled: true,
        per_hour: safety.max_prs_per_hour,
        ..Default::default()
    };
    let starting_budget = config.trust_band.starting_budget();
    let state = SelfImprovementState::with_trust_budget(
        si_config,
        TrustBudget::from_score(starting_budget),
    );

    let goal_sources = hook_goal_sources.unwrap_or_else(|| {
        default_goal_sources(&config.target_slug, config.label_filter.as_deref())
    });

    let brain: BrainHandle = spawn_brain(BrainConfig {
        project_dir: repo_path.to_string_lossy().to_string(),
        enable_suggestions: true,
        enable_memory: true,
        quiet_threshold: 0.5,
        router: None,
        catalog: None,
        privacy_mode: false,
        relay_inbound_rx: None,
        history_context: Vec::new(),
        self_improvement: Some(state),
        goal_sources,
    });

    let dry_run = safety.dry_run;
    let dry_counter = if dry_run {
        Some(Arc::new(std::sync::atomic::AtomicUsize::new(0)))
    } else {
        None
    };
    let counter_clone = dry_counter.clone();
    let queues_for_forwarder = Arc::clone(&queues);
    let stop_for_forwarder = Arc::clone(&stop_flag);
    runtime.spawn_blocking(move || {
        run_action_forwarder(
            brain,
            queues_for_forwarder,
            dry_run,
            counter_clone,
            stop_for_forwarder,
        );
    });

    Ok(dry_counter)
}

/// Build the production set of goal sources for a target slug.
///
/// The brain's `GhIssueGoalSource` takes a single optional label; when
/// `label_filter` is `None` no `--label` flag is passed to `gh issue list`
/// and every open issue becomes a candidate. The builder's whole point is
/// to eat every issue, so leaving this `None` is the recommended default.
/// When set, the first entry is used as the gh CLI filter; further entries
/// are ignored (a follow-up may extend `GhIssueGoalSource` to multi-label).
fn default_goal_sources(
    target_slug: &str,
    label_filter: Option<&[String]>,
) -> Vec<Box<dyn GoalSource>> {
    let issue_label = label_filter.and_then(|l| l.first().cloned());
    vec![
        Box::new(GhIssueGoalSource::new(target_slug.to_string(), issue_label)),
        Box::new(GhCiFailureGoalSource::new(target_slug.to_string(), None)),
    ]
}

/// Forward brain actions to the loop queue registry until the brain's
/// channel closes or `stop_flag` flips to `true`.
fn run_action_forwarder(
    brain: BrainHandle,
    queues: Arc<LoopQueueRegistry>,
    dry_run: bool,
    counter: Option<Arc<std::sync::atomic::AtomicUsize>>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    if dry_run {
        let mut handler = crate::safety::DryRunActionHandler::new();
        // Share the same counter Arc so the test can assert from outside.
        if let Some(ext) = counter {
            handler.enqueue_count = ext;
        }
        while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            match brain.try_recv_action() {
                Some(action) => action.execute(&mut handler),
                None => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    } else {
        let mut handler = LoopQueueActionHandler::new(queues);
        while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            match brain.try_recv_action() {
                Some(action) => action.execute(&mut handler),
                None => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    }
    tracing::info!("brain action forwarder stopping (stop_flag set)");
    // `brain` is dropped here → BrainHandle::Drop sends Shutdown + joins
    // the brain OS thread cleanly.
}

/// Local copy of `phantom::loop_cli::build_source` — the builder crate does
/// not depend on the `phantom` binary crate.
fn build_source(
    spec: &LoopSpec,
    queues: &Arc<LoopQueueRegistry>,
) -> Result<Box<dyn LoopSource>, BuilderError> {
    let source: Box<dyn LoopSource> = match &spec.source {
        LoopSourceSpec::Cron { interval_seconds } => {
            Box::new(phantom_loop::CronSource::from_seconds(*interval_seconds))
        }
        LoopSourceSpec::Queue { name } => {
            Box::new(phantom_loop::LoopMessageQueueSource::new(queues, name))
        }
        LoopSourceSpec::GhIssues { repo, label, query } => Box::new(
            phantom_loop::GhIssueQueueSource::new(repo.clone(), label.clone(), query.clone()),
        ),
        LoopSourceSpec::GhPr { repo, predicate } => Box::new(
            phantom_loop::GhPrReviewQueueSource::new(repo.clone(), predicate.clone()),
        ),
    };
    Ok(source)
}

/// Walk a directory for `*.toml` files and parse each as a loop spec.
fn discover_specs(
    dir: &std::path::Path,
) -> Result<Vec<(LoopSpec, Option<phantom_loop::ExitSchema>)>, BuilderError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|source| BuilderError::Io {
        path: dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| BuilderError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        match phantom_loop::load_spec(&path) {
            Ok((spec, schema)) => out.push((spec, schema)),
            Err(e) => {
                tracing::warn!(
                    "phantom-builder: failed to load {}: {e}",
                    path.display()
                );
            }
        }
    }
    out.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuilderSafetyConfig, TrustBandConfig};
    use tempfile::tempdir;

    #[test]
    fn resolve_path_uses_override_when_supplied() {
        let tmp = tempdir().unwrap();
        let cfg = BuilderConfig {
            target_slug: "foo/bar".into(),
            repo_path: Some(tmp.path().to_path_buf()),
            ..BuilderConfig::new("foo/bar")
        };
        let b = Builder::new(cfg);
        let path = b.resolve_path().unwrap();
        assert!(path.exists());
        assert!(path.is_absolute());
    }

    #[test]
    fn seed_specs_writes_all_four_with_target_slug() {
        let tmp = tempdir().unwrap();
        let b = Builder::new(BuilderConfig::new("alice/proj"));
        let written = b.seed_specs(tmp.path()).unwrap();
        assert_eq!(written.len(), 4);
        for p in &written {
            let body = std::fs::read_to_string(p).unwrap();
            assert!(body.contains("alice/proj"));
        }
    }

    #[test]
    fn default_goal_sources_count_is_two() {
        let v = default_goal_sources("o/r", Some(&["priority:high".to_string()]));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn discover_specs_handles_missing_dir() {
        let tmp = tempdir().unwrap();
        let specs = discover_specs(&tmp.path().join("missing")).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn discover_specs_finds_seeded_files() {
        let tmp = tempdir().unwrap();
        let b = Builder::new(BuilderConfig::new("o/r"));
        let _ = b.seed_specs(tmp.path()).unwrap();
        let dir = tmp.path().join(".phantom").join("loops");
        let specs = discover_specs(&dir).unwrap();
        assert_eq!(specs.len(), 4);
    }

    #[test]
    fn config_clamps_max_concurrent_when_safety_cap_is_lower() {
        let safety = BuilderSafetyConfig {
            max_concurrent_agents: 2,
            ..Default::default()
        };
        let cap = safety.max_concurrent_agents as u32;
        let mut spec_max = 5u32;
        if spec_max > cap {
            spec_max = cap.max(1);
        }
        assert_eq!(spec_max, 2);
    }

    #[test]
    fn trust_band_starting_budget_is_band_specific() {
        assert_eq!(TrustBandConfig::SuggestionOnly.starting_budget(), 0);
        assert!(TrustBandConfig::Conservative.starting_budget() <= 3);
        assert!((4..=9).contains(&TrustBandConfig::Standard.starting_budget()));
        assert!(TrustBandConfig::Aggressive.starting_budget() >= 10);
    }
}
