//! Fleet orchestrator entry point.
//!
//! Boots ONE shared `LoopQueueRegistry`, ONE shared `SubstrateDriver`, ONE
//! shared brain, and N hosted [`phantom_adapter::AppAdapter`] instances —
//! all in a single Phantom process.
//!
//! # Topology
//!
//! ```text
//! FleetRunner
//!     │
//!     ├── LoopQueueRegistry        (Arc, shared by every loop / builder)
//!     ├── SpawnSubagentQueue       (Arc, single substrate queue)
//!     ├── SubstrateAgentDispatcher (wraps the spawn queue)
//!     ├── SubstrateDriver          (drains the spawn queue, runs agents)
//!     ├── BrainHandle              (one shared brain with self-improvement)
//!     │   └── goal_sources         (one per builder target slug)
//!     │
//!     └── N hosted AppAdapter tasks
//!         ├── builder: jdmiranda/phantom
//!         ├── builder: jdmiranda/badass-cli
//!         └── loop:    /some/dir/.phantom/loops
//! ```
//!
//! ## Shared-brain rationale (MVP)
//!
//! We deliberately boot **one** brain across every hosted app rather than
//! one-brain-per-app:
//!
//! 1. **Single audit log.** Every action dispatched by the fleet routes
//!    through the same brain action receiver. Operators can `tail -f`
//!    one log to see what every hosted builder did.
//! 2. **Natural rate-limiting.** The brain's quiet-threshold and chattiness
//!    decay are global; an over-eager builder can't drown out a quieter one
//!    because they share the same scorer.
//! 3. **Goal aggregation.** Each [`AppKind::Builder`] entry appends its
//!    target slug to the shared brain's `goal_sources` list; the
//!    reconciler then polls every target uniformly.
//! 4. **Simpler shutdown.** One `BrainHandle::shutdown` on Ctrl-C beats
//!    N shutdowns racing against each other.
//!
//! The trade-off is that one brain panic-restart pauses every hosted app's
//! brain-driven path. We accept that because the brain's
//! `brain_supervised` wrapper already restarts on panic without dropping
//! the channel, so the pause window is small (sub-second).
//!
//! # phantom-builder integration shim
//!
//! Feature-gated behind `builder-apps`. When the feature is off, an
//! [`AppKind::Builder`] entry parses fine but [`run_fleet`] returns a
//! [`crate::FleetError::Unsupported`] error pointing at the rebuild command.
//! Once `phantom-builder` lands, flipping the feature flag in the parent
//! `Cargo.toml` is the entire integration cost.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use phantom_adapter::AppAdapter;
use phantom_agents::composer_tools::new_spawn_subagent_queue;
use phantom_brain::brain::{BrainConfig, BrainHandle, spawn_brain};
use phantom_brain::goal_source::{GhCiFailureGoalSource, GhIssueGoalSource, GoalSource};
use phantom_brain::self_improvement::{SelfImprovementConfig, SelfImprovementState};
use phantom_loop::{
    ChatBackedSubstrateBackend, LoopQueueActionHandler, LoopQueueRegistry,
    SubstrateAgentDispatcher, SubstrateBackend, SubstrateDriver,
};

use crate::app_kind::{AppKind, CustomAppSpec};
use crate::error::{FleetError, FleetResult};
use crate::registry::{FleetAppHandle, FleetAppStatus, FleetRegistry};
use crate::spec::FleetSpec;

/// Per-app boot hook the runner calls for each [`AppKind`] entry.
///
/// The hook is the integration shim — it receives the shared queues +
/// dispatcher, returns an `AppAdapter` boxed and ready to register, plus a
/// lifecycle function the runner spawns as a tokio task. Errors funnel into
/// [`FleetError::Unsupported`] so missing-feature paths produce a clean
/// operator-visible message.
pub type AppFactoryResult = FleetResult<(Box<dyn AppAdapter>, BoxedLifecycle)>;

/// The lifecycle function the fleet runs for one hosted app.
///
/// Receives an [`AppShutdown`] handle so the lifecycle can cooperate with
/// Ctrl-C: when shutdown fires, the function should return promptly. The
/// returned `String` is recorded as the app's stop reason in the registry.
pub type BoxedLifecycle = Box<
    dyn FnOnce(AppShutdown) -> futures_box::BoxFut + Send + 'static,
>;

mod futures_box {
    use std::future::Future;
    use std::pin::Pin;
    pub type BoxFut = Pin<Box<dyn Future<Output = String> + Send + 'static>>;
}

/// Shutdown handle passed to each hosted app's lifecycle function.
///
/// The runner trips this when Ctrl-C fires (or the runner is dropped).
/// Lifecycles poll `is_cancelled()` to know when to wind down.
#[derive(Clone)]
pub struct AppShutdown {
    inner: Arc<Mutex<bool>>,
}

impl AppShutdown {
    /// Construct a fresh shutdown handle, not yet cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(false)),
        }
    }

    /// Flip the cancelled bit. Idempotent.
    pub fn cancel(&self) {
        if let Ok(mut b) = self.inner.lock() {
            *b = true;
        }
    }

    /// Whether [`Self::cancel`] has been called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.lock().map(|b| *b).unwrap_or(true)
    }
}

impl Default for AppShutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// Factory callback for [`AppKind::Custom`] entries. Receives the variant's
/// `app_type` tag and `params` JSON; returns an adapter + lifecycle.
pub type CustomFactory = Arc<
    dyn Fn(&CustomAppSpec, &FleetContext) -> AppFactoryResult + Send + Sync + 'static,
>;

/// Shared infrastructure handed to every per-app factory.
///
/// Reference-counted Arcs so a factory can hold a handle for the lifetime of
/// its hosted app without re-resolving the registry on every push/pop.
pub struct FleetContext {
    /// Shared cross-loop queue registry. Every hosted app pushes/pops here.
    pub queues: Arc<LoopQueueRegistry>,

    /// Shared agent dispatcher. Builders that spawn loops via the same
    /// dispatcher pick up automatic completion routing.
    pub dispatcher: Arc<SubstrateAgentDispatcher>,

    /// Shared brain event sender. Hosted apps can fan events in.
    pub brain_event_tx: std::sync::mpsc::Sender<phantom_brain::events::AiEvent>,
}

/// The fleet runner. Build with [`FleetRunner::new`], add custom factories
/// via [`Self::register_custom_factory`], then drive with [`Self::run`].
pub struct FleetRunner {
    spec: FleetSpec,
    custom_factories: HashMap<String, CustomFactory>,
}

impl FleetRunner {
    /// Build a runner around `spec`.
    #[must_use]
    pub fn new(spec: FleetSpec) -> Self {
        Self {
            spec,
            custom_factories: HashMap::new(),
        }
    }

    /// Register a factory for an `AppKind::Custom` variant matching
    /// `app_type`. Returns the runner for chaining.
    #[must_use]
    pub fn register_custom_factory(
        mut self,
        app_type: impl Into<String>,
        factory: CustomFactory,
    ) -> Self {
        self.custom_factories.insert(app_type.into(), factory);
        self
    }

    /// Get an inspector handle for read-only operations like
    /// `phantom fleet list`. The runner stays usable after this call.
    #[must_use]
    pub fn spec(&self) -> &FleetSpec {
        &self.spec
    }

    /// Validate the spec without booting anything. Returns the list of
    /// per-app errors that would prevent boot.
    #[must_use]
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for (i, app) in self.spec.apps.iter().enumerate() {
            match app {
                AppKind::Builder(_) => {
                    if !builder_feature_enabled() {
                        errors.push(format!(
                            "apps[{i}]: builder entry requires --features builder-apps; \
                             rebuild phantom-fleet with that feature once phantom-builder \
                             is merged"
                        ));
                    }
                }
                AppKind::Loop(l) => {
                    if !l.spec_dir.is_dir() {
                        errors.push(format!(
                            "apps[{i}]: loop spec_dir {} does not exist or is not a directory",
                            l.spec_dir.display()
                        ));
                    }
                }
                AppKind::Custom(c) => {
                    if !self.custom_factories.contains_key(&c.app_type) {
                        errors.push(format!(
                            "apps[{i}]: no registered factory for custom app_type '{}'",
                            c.app_type
                        ));
                    }
                }
            }
        }
        errors
    }

    /// Run the fleet to completion (Ctrl-C / explicit shutdown).
    ///
    /// Boots all hosted apps as tokio tasks on the current runtime, then
    /// awaits `tokio::signal::ctrl_c()`. On Ctrl-C, signals every lifecycle
    /// to wind down, aborts straggler tasks, and returns cleanly.
    pub async fn run(self) -> FleetResult<()> {
        let validation_errors = self.validate();
        if !validation_errors.is_empty() {
            return Err(FleetError::Unsupported(validation_errors.join("; ")));
        }

        // -- Boot shared infrastructure -------------------------------------
        let queues = Arc::new(LoopQueueRegistry::new());
        let spawn_queue = new_spawn_subagent_queue();
        let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(
            spawn_queue.clone(),
        ));
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::channel::<phantom_protocol::Event>(64);
        let backend: Arc<dyn SubstrateBackend> = Arc::new(ChatBackedSubstrateBackend::default());
        let driver = SubstrateDriver::new(spawn_queue.clone(), backend, event_tx);
        let router = dispatcher.completion_router();

        // Forwarder: pipe substrate events into the completion router so
        // every hosted app's dispatched agent resolves its oneshot when the
        // driver finishes.
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                router.on_completion(event);
            }
        });
        let driver_handle = driver.run();

        // Brain: one shared instance for the entire fleet. Self-improvement
        // goal sources are aggregated across every builder slug.
        let (brain_opt, brain_event_tx) = if self.spec.shared.brain_self_improve {
            let (brain, sender) = spawn_shared_brain(&self.spec);
            (Some(brain), sender)
        } else {
            // Off-mode: still wire a dead-letter sender so factories don't
            // need to branch on the brain being absent.
            let (tx, _rx) = std::sync::mpsc::channel();
            (None, tx)
        };

        // Brain action forwarder: every action lands on the shared queue
        // registry via the LoopQueueActionHandler. We hold the handle so the
        // brain doesn't shut down until the runner returns.
        let _brain_forwarder =
            brain_opt.map(|brain| spawn_brain_forwarder(brain, Arc::clone(&queues)));

        // -- Build hosted apps ----------------------------------------------
        let ctx = FleetContext {
            queues: Arc::clone(&queues),
            dispatcher: Arc::clone(&dispatcher),
            brain_event_tx,
        };

        let registry = Arc::new(FleetRegistry::new());
        let shutdown = AppShutdown::new();

        for (i, app) in self.spec.apps.iter().enumerate() {
            let label = label_for(app, i);
            let (adapter, lifecycle) = self.build_app(app, &ctx)?;
            tracing::info!(
                fleet.app = %label,
                "phantom-fleet: booted hosted app `{}` (app_type = `{}`)",
                label,
                adapter.app_type()
            );
            // The adapter is registered for callers that want to call into
            // it later (e.g. via the bus); we intentionally drop it after
            // logging because the lifecycle owns the operational handle.
            // Future v2: thread the adapter through to a phantom-adapter
            // AppRegistry for visual hosting.
            drop(adapter);

            let id = registry.alloc_id();
            let status = Arc::new(Mutex::new(FleetAppStatus::Running));
            let status_for_task = Arc::clone(&status);
            let shutdown_for_task = shutdown.clone();

            let join = tokio::spawn(async move {
                let reason = lifecycle(shutdown_for_task).await;
                if let Ok(mut s) = status_for_task.lock() {
                    *s = FleetAppStatus::Stopped(reason);
                }
            });

            registry.register(
                id,
                FleetAppHandle {
                    label,
                    status,
                    registered_at: SystemTime::now(),
                    join_handle: Some(join),
                },
            );
        }

        tracing::info!(
            "phantom-fleet: {} apps running. Press Ctrl-C to stop.",
            registry.len()
        );

        // -- Block on Ctrl-C -------------------------------------------------
        match tokio::signal::ctrl_c().await {
            Ok(()) => tracing::info!("phantom-fleet: Ctrl-C received, shutting down"),
            Err(e) => tracing::warn!(error = %e, "phantom-fleet: ctrl-c handler error"),
        }

        // Signal every lifecycle to wind down, then abort their tasks if
        // they don't return promptly. The abort is the safety net — well-
        // behaved lifecycles return on the first `is_cancelled()` poll.
        shutdown.cancel();
        // Give cooperative shutdowns a brief grace window. Most lifecycles
        // exit within a few ticks; 200ms is plenty for the common case.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        registry.abort_all();
        driver_handle.abort();

        tracing::info!("phantom-fleet: shutdown complete");
        Ok(())
    }

    /// Build one hosted app from a [`FleetSpec`] entry.
    fn build_app(
        &self,
        app: &AppKind,
        ctx: &FleetContext,
    ) -> AppFactoryResult {
        match app {
            AppKind::Builder(b) => build_builder_app(b, ctx),
            AppKind::Loop(l) => build_loop_app(l, ctx),
            AppKind::Custom(c) => {
                let factory = self.custom_factories.get(&c.app_type).ok_or_else(|| {
                    FleetError::Unsupported(format!(
                        "no factory registered for custom app_type '{}' — \
                         call FleetRunner::register_custom_factory before run()",
                        c.app_type
                    ))
                })?;
                factory(c, ctx)
            }
        }
    }
}

/// Human-readable label for the registry's `list` output.
fn label_for(app: &AppKind, index: usize) -> String {
    match app {
        AppKind::Builder(b) => format!("builder:{}", b.slug),
        AppKind::Loop(l) => format!("loop:{}", l.spec_dir.display()),
        AppKind::Custom(c) => format!("custom:{}[{index}]", c.app_type),
    }
}

/// Spawn the shared brain. Returns the handle plus a clone of the event
/// sender so hosted apps can fan-in events without re-entering the brain
/// crate's API.
fn spawn_shared_brain(
    spec: &FleetSpec,
) -> (
    BrainHandle,
    std::sync::mpsc::Sender<phantom_brain::events::AiEvent>,
) {
    let state = SelfImprovementState::new(SelfImprovementConfig {
        enabled: true,
        ..Default::default()
    });

    // Build goal sources: one (issue + CI failure) source per builder slug.
    // Duplicate slugs are tolerated — `gh issue list` is the same query
    // either way; the brain's reconciler dedupes by issue id downstream.
    let mut goal_sources: Vec<Box<dyn GoalSource>> = Vec::new();
    for app in &spec.apps {
        if let AppKind::Builder(b) = app {
            goal_sources.push(Box::new(GhIssueGoalSource::new(b.slug.clone(), None)));
            goal_sources.push(Box::new(GhCiFailureGoalSource::new(b.slug.clone(), None)));
        }
    }
    // Fallback when the fleet has no builder entries: still poll the canonical
    // repo so the brain has *something* to reconcile against.
    if goal_sources.is_empty() {
        goal_sources.push(Box::new(GhIssueGoalSource::new(
            "jdmiranda/phantom".to_string(),
            None,
        )));
    }

    let brain = spawn_brain(BrainConfig {
        project_dir: std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string()),
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

    let sender = brain.event_sender();
    (brain, sender)
}

/// Spawn the long-running brain → loop-queue forwarder thread.
fn spawn_brain_forwarder(
    brain: BrainHandle,
    queues: Arc<LoopQueueRegistry>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("phantom-fleet-brain-forwarder".to_string())
        .spawn(move || {
            // Hold the brain handle for the thread's lifetime so it lives as
            // long as the fleet does. When the thread exits (on shutdown
            // hook drop), `brain` is dropped, which signals `Shutdown` to
            // the brain supervisor thread.
            let mut handler = LoopQueueActionHandler::new(queues);
            loop {
                match brain.try_recv_action() {
                    Some(action) => action.execute(&mut handler),
                    None => std::thread::sleep(std::time::Duration::from_millis(100)),
                }
            }
        })
        .expect("failed to spawn fleet brain forwarder thread")
}

// ---------------------------------------------------------------------------
// AppKind::Builder integration shim
// ---------------------------------------------------------------------------
//
// NOTE: the actual `phantom_builder` import lives behind `cfg(feature =
// "builder-apps")` so the workspace compiles even before the sibling
// agent's crate is added. The expected merge-time API is:
//
//     pub struct BuilderConfig {
//         pub slug: String,
//         pub trust_band: u8,
//         pub loops: Vec<String>,
//         pub max_prs_per_hour: Option<u32>,
//         pub dry_run: bool,
//         pub queues: Arc<LoopQueueRegistry>,
//         pub dispatcher: Arc<SubstrateAgentDispatcher>,
//     }
//
//     impl Builder {
//         pub fn new(config: BuilderConfig) -> Result<Builder, Error>;
//     }
//
//     impl phantom_adapter::AppAdapter for Builder { ... }
//
// If the sibling agent's surface diverges, only this function needs to
// change at merge time.

#[cfg(feature = "builder-apps")]
fn build_builder_app(
    b: &crate::app_kind::BuilderSpec,
    ctx: &FleetContext,
) -> AppFactoryResult {
    // Suppress unused-warning for `ctx` if the placeholder body below is
    // active. The merge-time fixup will use it.
    let _ = ctx;
    Err(FleetError::Unsupported(format!(
        "builder app `{}` requested with --features builder-apps enabled, but \
         the phantom-builder dependency has not yet been wired in. Re-read the \
         phantom-fleet Cargo.toml `phantom-builder` integration note and add \
         the dep + flip the feature payload before re-enabling.",
        b.slug
    )))
}

#[cfg(not(feature = "builder-apps"))]
fn build_builder_app(
    b: &crate::app_kind::BuilderSpec,
    _ctx: &FleetContext,
) -> AppFactoryResult {
    Err(FleetError::Unsupported(format!(
        "builder app `{}` requested but phantom-fleet was built without \
         the `builder-apps` feature. Rebuild with `cargo build -p \
         phantom-fleet --features builder-apps` once phantom-builder is \
         merged.",
        b.slug
    )))
}

// ---------------------------------------------------------------------------
// AppKind::Loop integration
// ---------------------------------------------------------------------------

fn build_loop_app(
    l: &crate::app_kind::LoopAppSpec,
    ctx: &FleetContext,
) -> AppFactoryResult {
    // Discover specs in the directory; filter to the names the entry lists
    // (or take all if the list is empty).
    let specs = discover_loop_specs(&l.spec_dir)?;
    let wanted: Vec<_> = if l.loops.is_empty() {
        specs
    } else {
        specs
            .into_iter()
            .filter(|(s, _)| l.loops.iter().any(|n| n == &s.id))
            .collect()
    };

    if wanted.is_empty() {
        return Err(FleetError::Unsupported(format!(
            "loop entry: no specs found at {} matching {:?}",
            l.spec_dir.display(),
            l.loops
        )));
    }

    let queues = Arc::clone(&ctx.queues);
    let dispatcher: Arc<dyn phantom_loop::AgentDispatcher> =
        Arc::clone(&ctx.dispatcher) as Arc<dyn phantom_loop::AgentDispatcher>;
    let spec_dir = l.spec_dir.clone();

    // The adapter is a minimal placeholder; it exists so callers can look up
    // this fleet entry through future visual hosts. The actual work happens
    // in the lifecycle.
    let adapter: Box<dyn AppAdapter> = Box::new(LoopHeadlessAdapter::new(format!(
        "loop:{}",
        spec_dir.display()
    )));

    let lifecycle: BoxedLifecycle = Box::new(move |sd| {
        Box::pin(async move {
            let mut tasks = Vec::new();
            for (spec, schema) in wanted {
                let source = match build_loop_source(&spec, &queues) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            spec_id = %spec.id,
                            error = %e,
                            "phantom-fleet: skipping loop spec; source unbuildable"
                        );
                        continue;
                    }
                };
                let runner = phantom_loop::LoopRunner::new(
                    Arc::new(spec),
                    schema,
                    source,
                    Arc::clone(&queues),
                    Arc::clone(&dispatcher),
                );
                tasks.push(tokio::spawn(async move {
                    let _ = runner.run().await;
                }));
            }
            // Poll for cancellation; on cancel, abort every loop's task.
            while !sd.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            for t in tasks {
                t.abort();
            }
            format!("loop:{} stopped cleanly", spec_dir.display())
        })
    });

    Ok((adapter, lifecycle))
}

/// Read every `*.toml` in `dir` and try parsing each as a [`phantom_loop::LoopSpec`].
/// Malformed files are skipped with a warning; the rest are returned in id
/// order.
fn discover_loop_specs(
    dir: &std::path::Path,
) -> FleetResult<Vec<(phantom_loop::LoopSpec, Option<phantom_loop::ExitSchema>)>> {
    if !dir.is_dir() {
        return Err(FleetError::Unsupported(format!(
            "loop spec_dir {} is not a directory",
            dir.display()
        )));
    }
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| FleetError::ConfigRead {
        path: dir.display().to_string(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| FleetError::ConfigRead {
            path: dir.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        match phantom_loop::load_spec(&path) {
            Ok((spec, schema)) => out.push((spec, schema)),
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "phantom-fleet: failed to load loop spec; skipping"
            ),
        }
    }
    out.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    Ok(out)
}

/// Mirror of `loop_cli::build_source`. Kept here so `phantom-fleet` does
/// not depend on the CLI module.
fn build_loop_source(
    spec: &phantom_loop::LoopSpec,
    queues: &Arc<LoopQueueRegistry>,
) -> FleetResult<Box<dyn phantom_loop::LoopSource>> {
    let source: Box<dyn phantom_loop::LoopSource> = match &spec.source {
        phantom_loop::LoopSourceSpec::Cron { interval_seconds } => {
            Box::new(phantom_loop::CronSource::from_seconds(*interval_seconds))
        }
        phantom_loop::LoopSourceSpec::Queue { name } => {
            Box::new(phantom_loop::LoopMessageQueueSource::new(queues, name))
        }
        phantom_loop::LoopSourceSpec::GhIssues { repo, label, query } => Box::new(
            phantom_loop::GhIssueQueueSource::new(repo.clone(), label.clone(), query.clone()),
        ),
        phantom_loop::LoopSourceSpec::GhPr { repo, predicate } => Box::new(
            phantom_loop::GhPrReviewQueueSource::new(repo.clone(), predicate.clone()),
        ),
    };
    Ok(source)
}

// ---------------------------------------------------------------------------
// Headless loop adapter
// ---------------------------------------------------------------------------

/// Minimal `AppAdapter` impl for the loop entry. The fleet doesn't render
/// anything; the adapter exists only to satisfy the
/// "everything is an app" contract.
struct LoopHeadlessAdapter {
    app_type: String,
    alive: bool,
}

impl LoopHeadlessAdapter {
    fn new(app_type: String) -> Self {
        Self {
            app_type,
            alive: true,
        }
    }
}

impl phantom_adapter::AppCore for LoopHeadlessAdapter {
    fn app_type(&self) -> &str {
        &self.app_type
    }
    fn is_alive(&self) -> bool {
        self.alive
    }
    fn update(&mut self, _dt: f32) {}
    fn get_state(&self) -> serde_json::Value {
        serde_json::json!({ "type": self.app_type, "alive": self.alive })
    }
}

impl phantom_adapter::Renderable for LoopHeadlessAdapter {
    fn render(&self, _rect: &phantom_adapter::Rect) -> phantom_adapter::RenderOutput {
        phantom_adapter::RenderOutput::default()
    }
    fn is_visual(&self) -> bool {
        false
    }
}

impl phantom_adapter::InputHandler for LoopHeadlessAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }
    fn accepts_input(&self) -> bool {
        false
    }
}

impl phantom_adapter::Commandable for LoopHeadlessAdapter {
    fn accept_command(
        &mut self,
        cmd: &str,
        _args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        if cmd == "stop" {
            self.alive = false;
        }
        Ok(format!("executed:{cmd}"))
    }
    fn accepts_commands(&self) -> bool {
        false
    }
}

impl phantom_adapter::BusParticipant for LoopHeadlessAdapter {}
impl phantom_adapter::Lifecycled for LoopHeadlessAdapter {}
impl phantom_adapter::Permissioned for LoopHeadlessAdapter {}

// ---------------------------------------------------------------------------
// Convenience: top-level `run_fleet`
// ---------------------------------------------------------------------------

/// Convenience wrapper around [`FleetRunner::run`]. Most callers should
/// use this rather than constructing a [`FleetRunner`] explicitly unless
/// they need to register custom factories.
///
/// # Errors
///
/// Returns any [`FleetError`] that [`FleetRunner::run`] would surface.
pub async fn run_fleet(spec: FleetSpec) -> FleetResult<()> {
    FleetRunner::new(spec).run().await
}

/// Whether the `builder-apps` feature is enabled at compile time.
#[must_use]
pub fn builder_feature_enabled() -> bool {
    cfg!(feature = "builder-apps")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_kind::{AppKind, BuilderSpec, LoopAppSpec};

    #[test]
    fn validate_loop_entry_with_missing_dir_reports_error() {
        let spec = FleetSpec {
            apps: vec![AppKind::Loop(LoopAppSpec {
                spec_dir: std::path::PathBuf::from("/does/not/exist"),
                loops: vec![],
            })],
            shared: Default::default(),
        };
        let runner = FleetRunner::new(spec);
        let errors = runner.validate();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("does not exist"));
    }

    #[test]
    fn validate_custom_entry_without_factory_reports_error() {
        let spec = FleetSpec {
            apps: vec![AppKind::Custom(crate::app_kind::CustomAppSpec {
                app_type: "test".to_string(),
                params: serde_json::Value::Null,
            })],
            shared: Default::default(),
        };
        let runner = FleetRunner::new(spec);
        let errors = runner.validate();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("no registered factory"));
    }

    #[test]
    fn validate_custom_entry_with_factory_passes() {
        let spec = FleetSpec {
            apps: vec![AppKind::Custom(crate::app_kind::CustomAppSpec {
                app_type: "test".to_string(),
                params: serde_json::Value::Null,
            })],
            shared: Default::default(),
        };
        let runner = FleetRunner::new(spec).register_custom_factory(
            "test",
            Arc::new(|_c, _ctx| {
                Err(FleetError::Unsupported(
                    "factory never invoked in this test".to_string(),
                ))
            }),
        );
        let errors = runner.validate();
        assert!(errors.is_empty());
    }

    #[test]
    fn validate_builder_entry_without_feature_flag_reports_error() {
        // This test runs with the feature OFF in CI.
        if !builder_feature_enabled() {
            let spec = FleetSpec {
                apps: vec![AppKind::Builder(BuilderSpec {
                    slug: "test/x".to_string(),
                    trust_band: 0,
                    loops: vec![],
                    max_prs_per_hour: None,
                    dry_run: true,
                    extra: serde_json::Map::new(),
                })],
                shared: Default::default(),
            };
            let runner = FleetRunner::new(spec);
            let errors = runner.validate();
            assert_eq!(errors.len(), 1);
            assert!(errors[0].contains("builder-apps"));
        }
    }

    #[test]
    fn label_for_uses_slug_for_builder() {
        let app = AppKind::Builder(BuilderSpec {
            slug: "owner/repo".to_string(),
            trust_band: 0,
            loops: vec![],
            max_prs_per_hour: None,
            dry_run: false,
            extra: serde_json::Map::new(),
        });
        assert_eq!(label_for(&app, 0), "builder:owner/repo");
    }

    #[test]
    fn label_for_uses_path_for_loop() {
        let app = AppKind::Loop(LoopAppSpec {
            spec_dir: std::path::PathBuf::from("/tmp/p/.phantom/loops"),
            loops: vec![],
        });
        assert!(label_for(&app, 0).starts_with("loop:"));
    }

    #[test]
    fn app_shutdown_starts_uncancelled() {
        let sd = AppShutdown::new();
        assert!(!sd.is_cancelled());
        sd.cancel();
        assert!(sd.is_cancelled());
    }

    #[test]
    fn app_shutdown_clone_shares_state() {
        let sd = AppShutdown::new();
        let sd2 = sd.clone();
        sd.cancel();
        assert!(sd2.is_cancelled());
    }
}
