//! `phantom loop` CLI surface — the production entry-point for running
//! repo-scoped autonomous loops from the command line.
//!
//! This module is the C3 binder of issue #650. It glues together the
//! pieces that landed in C1 (TOML spec parsing) and C2 (the
//! `LoopRunner` FSM + queue registry + dispatcher trait) into a
//! user-facing subcommand:
//!
//! ```text
//! phantom loop run    --repo <path> --loops <name1>,<name2>,...
//! phantom loop list   [--repo <path>]
//! phantom loop status [--repo <path>]
//! phantom loop stop   --loop <name>
//! ```
//!
//! # Topology of `phantom loop run`
//!
//! 1. Parse flags via [`clap`].
//! 2. Run pre-flight gates:
//!    [`phantom_loop::check_gh_binary`],
//!    [`phantom_loop::check_gh_auth`],
//!    [`phantom_loop::check_mcp_collisions`], and acquire a
//!    [`phantom_loop::RunLock`] at `<repo>/.phantom/loops/.runlock`.
//! 3. Discover and parse loop specs from `<repo>/.phantom/loops/*.toml`.
//! 4. Build one [`phantom_loop::LoopRunner`] per requested name, each
//!    wired to the shared [`phantom_loop::SubstrateAgentDispatcher`].
//! 5. Spawn each runner as a `tokio::spawn(...)` task; register the
//!    `JoinHandle` in a process-global [`phantom_loop::LoopRegistry`].
//! 6. Block on `tokio::signal::ctrl_c()`. On Ctrl-C, abort every
//!    registered loop, drop the runlock, and exit.
//!
//! # Headless agent driver
//!
//! Running real Claude-backed loop agents in the GUI app requires
//! `phantom-app::App::update` to drain
//! [`phantom_agents::composer_tools::SpawnSubagentQueue`] every frame and
//! materialise each request into an agent pane backed by a real chat
//! backend. That path requires winit, wgpu, layout, and scene state — far
//! heavier than the CLI surface the user expects from `phantom loop run`.
//!
//! Instead, the CLI boots a headless [`phantom_loop::SubstrateDriver`]
//! that drains the same queue from an async tick loop, drives each request
//! through a pluggable [`phantom_loop::SubstrateBackend`] (the production
//! [`phantom_loop::ChatBackedSubstrateBackend`] wraps the real Claude /
//! OpenAI API; tests substitute a mock), and emits
//! `phantom_protocol::Event::AgentTaskComplete` onto a tokio mpsc bus the
//! [`phantom_loop::SubstrateCompletionRouter`] subscribes to. This closes
//! the substrate loop without any GUI dependencies — the same `LoopRunner`
//! FSM that runs in the App now runs end-to-end from the CLI.
//!
//! # Headless brain
//!
//! `phantom loop run` also boots the [`phantom_brain`] ambient OODA loop
//! alongside the substrate driver. The brain's self-improvement reconciler
//! polls a default set of [`phantom_brain::goal_source::GoalSource`]s
//! (`GhIssueGoalSource` against `jdmiranda/phantom` plus a CI-failure source)
//! every 60 s and, on a candidate clearing all gates, emits
//! [`phantom_brain::events::AiAction::EnqueueLoopMessage`]. A small forwarder
//! thread drains the brain's action receiver and hands every action to a
//! [`phantom_loop::LoopQueueActionHandler`] — the bridge that pushes
//! `EnqueueLoopMessage` payloads onto the shared `LoopQueueRegistry`. The
//! `implementer-queue` consumer loop then pops each message, the
//! `SubstrateAgentDispatcher` spawns the agent, and the substrate driver
//! drives it to completion.
//!
//! The brain boot defaults to ON. Pass `--no-self-improve` to disable the
//! reconciler entirely (no goal sources polled, no auto-enqueue) — useful
//! when running pure cross-loop pipelines that do not want the brain to
//! second-source the queue.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Result, bail};
use phantom_agents::composer_tools::new_spawn_subagent_queue;
use phantom_brain::brain::{BrainConfig, BrainHandle, spawn_brain};
use phantom_brain::goal_source::{GhCiFailureGoalSource, GhIssueGoalSource, GoalSource};
use phantom_brain::self_improvement::{SelfImprovementConfig, SelfImprovementState};
use phantom_loop::{
    ChatBackedSubstrateBackend, LoopHandle, LoopId, LoopQueueActionHandler, LoopQueueRegistry,
    LoopRegistry, LoopRunner, LoopSource, LoopSourceSpec, LoopSpec, LoopStatus,
    SubstrateAgentDispatcher, SubstrateBackend, SubstrateDriver,
};

/// Top-level dispatcher: `phantom loop <subcommand> ...`
///
/// Mirrors the `Some("auth")` block in `main.rs`. Called from `main.rs`
/// when `argv[1] == "loop"`.
pub fn run_loop_subcommand(args: &[String]) -> Result<()> {
    match args.get(2).map(String::as_str) {
        Some("run") => run_command(&args[2..]),
        Some("list") => list_command(&args[2..]),
        Some("status") => status_command(&args[2..]),
        Some("stop") => stop_command(&args[2..]),
        _ => {
            print_loop_help();
            Ok(())
        }
    }
}

/// Print the human-readable usage banner.
fn print_loop_help() {
    eprintln!(
        "phantom loop — run repo-scoped autonomous loops\n\
         \n\
         USAGE:\n\
             phantom loop run    --repo <path> --loops <name1>,<name2>,...\n\
             phantom loop list   [--repo <path>]\n\
             phantom loop status\n\
             phantom loop stop   --loop <name>\n\
         \n\
         Loop specs live at <repo>/.phantom/loops/<name>.toml.\n\
         See crates/phantom-loop/src/lib.rs for the TOML schema.\n"
    );
}

// ---------------------------------------------------------------------------
// `phantom loop run`
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Parser)]
#[command(name = "phantom loop run")]
struct RunArgs {
    /// Repository root path. Loop specs are discovered at
    /// `<repo>/.phantom/loops/*.toml`.
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Comma-separated list of loop names (by `id` field) to start.
    #[arg(long)]
    loops: String,

    /// Disable the brain's self-improvement reconciler. Default is ON: the
    /// brain polls `gh issue list` and `gh run list` against
    /// `jdmiranda/phantom` every 60 s and auto-enqueues candidate goals onto
    /// the `implementer-queue`. Pass this flag to run only the loop pipeline
    /// without any brain-driven goal injection.
    #[arg(long)]
    no_self_improve: bool,

    /// Override the GitHub repo the brain's self-improvement sources query.
    /// Defaults to `jdmiranda/phantom`. Ignored when `--no-self-improve`
    /// is set.
    #[arg(long, default_value = "jdmiranda/phantom")]
    self_improve_repo: String,

    /// Override the queue name the brain enqueues candidates to. Default
    /// `implementer-queue` (legacy direct-to-implementer routing). Set to
    /// `triage-queue` to route through the triager loop, which classifies
    /// each candidate (close / comment / research / refine / implement)
    /// before paying for an implementer agent.
    #[arg(long, default_value = "implementer-queue")]
    brain_queue: String,

    /// Bounded-mode: shut the daemon down after the brain has enqueued
    /// `N` loop messages. Counts increments at the
    /// `LoopQueueActionHandler::enqueue_loop_message` boundary — every
    /// brain decision that would otherwise spawn an agent. Set to a small
    /// number (e.g. `--max-iterations 1`) to validate the end-to-end loop
    /// against the real API without an open-ended spend.
    #[arg(long)]
    max_iterations: Option<usize>,

    /// Bounded-mode: shut the daemon down after `T` minutes of wall-clock
    /// runtime. Triggered by a tokio timer spawned at boot; runs in parallel
    /// with the ctrl-c handler and `--max-iterations`, whichever fires
    /// first wins.
    #[arg(long)]
    max_runtime_min: Option<u64>,

    /// Bounded-mode: brain still polls, scores, audits, and emits actions —
    /// but the `LoopQueueActionHandler::enqueue_loop_message` body logs and
    /// counts the would-be enqueue and SKIPS the push onto the registry.
    /// No queue drains, no agent spawns, no API tokens spent. The audit log
    /// captures every brain decision so you can review scoring quality
    /// before authorising a live run.
    #[arg(long)]
    dry_run: bool,
}

fn run_command(args: &[String]) -> Result<()> {
    use clap::Parser;

    // Initialise structured logging early so brain ticks, dispatch decisions,
    // and effect runs all surface to stderr under the user's RUST_LOG filter.
    // `try_init` swallows the "already initialised" path so re-running the
    // subcommand in the same process (tests) is a no-op rather than a panic.
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .format_timestamp_millis()
    .try_init();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    // We expect args[0] == "run"; strip it before clap sees the slice so
    // clap's positional handling matches the documented usage.
    let parsed = if args.first().map(String::as_str) == Some("run") {
        RunArgs::parse_from(std::iter::once("phantom loop run").chain(args[1..].iter().map(String::as_str)))
    } else {
        RunArgs::parse_from(std::iter::once("phantom loop run").chain(args.iter().map(String::as_str)))
    };

    let repo = canonicalize_or_self(&parsed.repo);
    let names: Vec<&str> = parsed.loops.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if names.is_empty() {
        bail!("--loops must list at least one loop name (got empty)");
    }

    // -- Pre-flight gates ------------------------------------------------
    eprintln!("phantom loop run: preflight checks");
    phantom_loop::check_gh_binary()?;
    eprintln!("  ok   gh binary present");
    phantom_loop::check_gh_auth()?;
    eprintln!("  ok   gh authenticated");
    // No MCP registry in the CLI path — pass an empty iterator. The check
    // still rejects if a future wire-up plumbs reserved names through.
    let no_mcp: Vec<&str> = Vec::new();
    phantom_loop::check_mcp_collisions(no_mcp)?;
    eprintln!("  ok   no MCP collisions on reserved tool names");
    let runlock = phantom_loop::RunLock::acquire(&repo)?;
    eprintln!("  ok   acquired {}", runlock.path().display());

    // -- Discover specs --------------------------------------------------
    let specs_dir = repo.join(".phantom").join("loops");
    let all_specs = discover_specs(&specs_dir)?;
    if all_specs.is_empty() {
        bail!(
            "no loop specs found at {}/*.toml — create one or run \
             `phantom loop list` from inside a repo with loops",
            specs_dir.display()
        );
    }

    // Filter to the requested names by spec id.
    let mut targeted: Vec<(LoopSpec, Option<phantom_loop::ExitSchema>)> = Vec::new();
    for name in &names {
        match all_specs.iter().find(|(s, _)| s.id == *name) {
            Some((s, schema)) => targeted.push((s.clone(), schema.clone())),
            None => bail!(
                "no loop named `{name}` at {} (found ids: {})",
                specs_dir.display(),
                all_specs
                    .iter()
                    .map(|(s, _)| s.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    eprintln!(
        "phantom loop run: starting {} loop(s): {}",
        targeted.len(),
        targeted.iter().map(|(s, _)| s.id.as_str()).collect::<Vec<_>>().join(", ")
    );

    // -- Build runtime ---------------------------------------------------
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let registry = Arc::new(LoopRegistry::new());
    let queues = Arc::new(LoopQueueRegistry::new());
    let spawn_queue = new_spawn_subagent_queue();
    let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(
        spawn_queue.clone(),
    ));

    // Headless substrate driver: drains the spawn queue and runs each
    // request through a real chat backend, mirroring what
    // `phantom-app::App::update` does inside the GUI app. The driver emits
    // `Event::AgentTaskComplete` onto an in-process tokio mpsc bus; a
    // forwarder task pipes each event into the dispatcher's completion
    // router so the runner's pending oneshot resolves.
    let (event_tx, mut event_rx) =
        tokio::sync::mpsc::channel::<phantom_protocol::Event>(64);
    let backend: Arc<dyn SubstrateBackend> = Arc::new(ChatBackedSubstrateBackend::default());
    let driver = SubstrateDriver::new(spawn_queue.clone(), backend, event_tx);
    let router = dispatcher.completion_router();

    // Stamp every requested loop as a tokio task on the runtime.
    let registry_clone = Arc::clone(&registry);
    let queues_clone = Arc::clone(&queues);
    let dispatcher_clone: Arc<dyn phantom_loop::AgentDispatcher> = dispatcher.clone();
    let driver_handle = runtime.block_on(async move {
        // Spawn the driver tick loop and the event-forwarder task on the
        // same runtime as the runners.
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
                // The runner returns a stop reason on terminal state.
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
            eprintln!("  started {id} (spec_id ↑ above)");
        }
        anyhow::Ok(driver_join)
    })?;

    // -- Brain boot ------------------------------------------------------
    //
    // The brain runs on its own OS thread (`spawn_brain` builds a
    // `std::thread`, not a tokio task) and emits actions onto a
    // `std::sync::mpsc` channel. We pair it with a long-running
    // `tokio::task::spawn_blocking` forwarder that pulls actions off the
    // channel and dispatches each one through
    // `AiAction::execute(&mut LoopQueueActionHandler)`. The handler pushes
    // every `EnqueueLoopMessage` onto the shared `LoopQueueRegistry` — the
    // same registry the `implementer` loop's `LoopMessageQueueSource` pops
    // from. That closes the brain ↔ loop bridge end-to-end without GUI deps.
    let brain_state = if parsed.no_self_improve {
        eprintln!("phantom loop run: self-improvement disabled (--no-self-improve)");
        None
    } else {
        eprintln!(
            "phantom loop run: booting brain with self-improvement against {}",
            parsed.self_improve_repo
        );
        let project_dir = repo.to_string_lossy().to_string();
        // Master kill switch defaults to OFF in the brain crate; the CLI is
        // the operator's explicit opt-in surface so flip it on here.
        let state = SelfImprovementState::new(SelfImprovementConfig {
            enabled: true,
            queue_name: parsed.brain_queue.clone(),
            ..Default::default()
        });
        eprintln!(
            "phantom loop run: brain will enqueue to `{}`",
            parsed.brain_queue
        );
        let goal_sources: Vec<Box<dyn GoalSource>> = vec![
            Box::new(GhIssueGoalSource::new(parsed.self_improve_repo.clone(), None)),
            Box::new(GhCiFailureGoalSource::new(
                parsed.self_improve_repo.clone(),
                None,
            )),
        ];
        let brain: BrainHandle = spawn_brain(BrainConfig {
            project_dir,
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

        // Forwarder: drain brain actions on a blocking thread and route
        // every one through the `LoopQueueActionHandler`. The thread owns
        // the `BrainHandle` for its lifetime, so the handle's `Drop` (which
        // sends `AiEvent::Shutdown` and joins the brain OS thread) runs
        // only at process teardown. That keeps the brain alive for the
        // full `phantom loop run` session without further plumbing.
        //
        // We spawn this as a tokio blocking task rather than a bare
        // `std::thread` so the runtime knows about it; tokio's runtime
        // drop sequence then waits for it on Ctrl-C teardown alongside the
        // other tasks.
        let queues_for_brain = Arc::clone(&queues);
        let enqueue_counter =
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_for_handler = std::sync::Arc::clone(&enqueue_counter);
        let dry_run = parsed.dry_run;
        let _forwarder_handle = runtime.spawn_blocking(move || {
            let mut handler = LoopQueueActionHandler::new(queues_for_brain)
                .with_dry_run(dry_run)
                .with_enqueue_counter(counter_for_handler);
            loop {
                match brain.try_recv_action() {
                    Some(action) => action.execute(&mut handler),
                    None => std::thread::sleep(std::time::Duration::from_millis(100)),
                }
            }
        });
        Some(enqueue_counter)
    };

    if parsed.dry_run {
        eprintln!("phantom loop run: DRY-RUN — brain runs but enqueues are suppressed (no API spend)");
    }
    if let Some(n) = parsed.max_iterations {
        eprintln!("phantom loop run: bounded — will exit after {n} brain enqueue(s)");
    }
    if let Some(t) = parsed.max_runtime_min {
        eprintln!("phantom loop run: bounded — will exit after {t} minute(s) of runtime");
    }
    eprintln!("phantom loop run: all loops started. Press Ctrl-C to stop.");

    // Shutdown is now sourced from multiple events: ctrl-c (the existing
    // path), an optional wall-clock timer (`--max-runtime-min`), and an
    // optional enqueue-count watcher (`--max-iterations`). Whichever fires
    // first wakes the block_on closure, which then tears the loops down
    // in the same order as before.
    let max_iterations = parsed.max_iterations;
    let max_runtime_min = parsed.max_runtime_min;
    let counter_for_watcher = brain_state.clone();
    runtime.block_on(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        // Wall-clock timer: resolves into a non-firing future when the flag
        // is absent, so the select! arm never wins in that case.
        let runtime_timer: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            match max_runtime_min {
                Some(min) => Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(min * 60)).await;
                }),
                None => Box::pin(std::future::pending::<()>()),
            };
        // Counter watcher: only fires when `--max-iterations` is set AND we
        // actually have a counter handle (i.e. self-improvement is enabled).
        let counter_watch: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            match (max_iterations, counter_for_watcher) {
                (Some(limit), Some(counter)) if limit > 0 => Box::pin(async move {
                    loop {
                        let n = counter.load(std::sync::atomic::Ordering::SeqCst);
                        if n >= limit {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                }),
                _ => Box::pin(std::future::pending::<()>()),
            };

        tokio::select! {
            res = ctrl_c => {
                if let Err(e) = res {
                    eprintln!("phantom loop run: ctrl-c handler error: {e}");
                } else {
                    eprintln!("phantom loop run: Ctrl-C received");
                }
            }
            _ = runtime_timer => {
                eprintln!("phantom loop run: max-runtime reached, stopping");
            }
            _ = counter_watch => {
                eprintln!("phantom loop run: max-iterations reached, stopping");
            }
        }

        eprintln!("phantom loop run: stopping all loops");
        for snap in registry.list() {
            let _ = registry.stop(snap.id);
        }
        driver_handle.abort();
        if brain_state.is_some() {
            eprintln!("phantom loop run: brain forwarder will exit on process teardown");
        }
    });

    // Drop the lock explicitly so the message lines up after the loop teardown.
    drop(runlock);
    eprintln!("phantom loop run: done");
    Ok(())
}

/// Build the appropriate [`LoopSource`] for a spec. C3 supports every
/// variant of [`LoopSourceSpec`] except `GhPr`'s exotic predicate fields,
/// which are passed through to the source for client-side filtering.
fn build_source(
    spec: &LoopSpec,
    queues: &Arc<LoopQueueRegistry>,
) -> Result<Box<dyn LoopSource>> {
    let source: Box<dyn LoopSource> = match &spec.source {
        LoopSourceSpec::Cron { interval_seconds } => {
            Box::new(phantom_loop::CronSource::from_seconds(*interval_seconds))
        }
        LoopSourceSpec::Queue { name } => {
            Box::new(phantom_loop::LoopMessageQueueSource::new(queues, name))
        }
        LoopSourceSpec::GhIssues { repo, label, query } => {
            Box::new(phantom_loop::GhIssueQueueSource::new(
                repo.clone(),
                label.clone(),
                query.clone(),
            ))
        }
        LoopSourceSpec::GhPr { repo, predicate } => {
            Box::new(phantom_loop::GhPrReviewQueueSource::new(
                repo.clone(),
                predicate.clone(),
            ))
        }
    };
    Ok(source)
}

/// Resolve `path` to an absolute path. Falls back to the original when
/// canonicalize fails (e.g. path does not yet exist).
fn canonicalize_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------------------
// `phantom loop list`
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Parser)]
#[command(name = "phantom loop list")]
struct ListArgs {
    #[arg(long, default_value = ".")]
    repo: PathBuf,
}

fn list_command(args: &[String]) -> Result<()> {
    use clap::Parser;
    let parsed = if args.first().map(String::as_str) == Some("list") {
        ListArgs::parse_from(std::iter::once("phantom loop list").chain(args[1..].iter().map(String::as_str)))
    } else {
        ListArgs::parse_from(std::iter::once("phantom loop list").chain(args.iter().map(String::as_str)))
    };
    let repo = canonicalize_or_self(&parsed.repo);
    let dir = repo.join(".phantom").join("loops");
    let specs = discover_specs(&dir)?;
    if specs.is_empty() {
        eprintln!("no loop specs at {}", dir.display());
        return Ok(());
    }
    println!("Loops at {}:", dir.display());
    for (spec, _) in specs {
        let source_kind = match spec.source {
            LoopSourceSpec::Cron { .. } => "cron",
            LoopSourceSpec::Queue { .. } => "queue",
            LoopSourceSpec::GhIssues { .. } => "gh_issues",
            LoopSourceSpec::GhPr { .. } => "gh_pr",
        };
        let agent_tag = if spec.agent.is_some() { "agent" } else { "agentless" };
        println!("  {:<20} [{source_kind}, {agent_tag}]", spec.id);
    }
    Ok(())
}

/// Walk `<dir>/*.toml` and parse each one with
/// [`phantom_loop::load_spec`]. Returns the list of (spec, compiled
/// schema) pairs sorted by spec id for stable output.
fn discover_specs(dir: &Path) -> Result<Vec<(LoopSpec, Option<phantom_loop::ExitSchema>)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        match phantom_loop::load_spec(&path) {
            Ok((spec, schema)) => out.push((spec, schema)),
            Err(e) => eprintln!(
                "phantom loop: failed to load {}: {e}",
                path.display()
            ),
        }
    }
    out.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    Ok(out)
}

// ---------------------------------------------------------------------------
// `phantom loop status`
// ---------------------------------------------------------------------------

fn status_command(_args: &[String]) -> Result<()> {
    // The CLI is single-process: a `phantom loop status` invocation in a
    // separate shell cannot inspect another `phantom loop run` instance's
    // registry. C3 ships this as a stub that prints the documented
    // limitation; cross-process status requires a per-repo socket or a
    // PID-file, both out of scope for the MVP.
    eprintln!(
        "phantom loop status: cross-process status reporting is not yet \
         implemented. Use `tail -f <repo>/.phantom/loops/.runlock` to confirm \
         a loop is running, and inspect the calling process's stderr for \
         the per-iteration status lines emitted by tracing."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `phantom loop stop`
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Parser)]
#[command(name = "phantom loop stop")]
struct StopArgs {
    /// Loop spec id to stop.
    #[arg(long)]
    r#loop: String,
}

fn stop_command(args: &[String]) -> Result<()> {
    use clap::Parser;
    let _parsed = if args.first().map(String::as_str) == Some("stop") {
        StopArgs::parse_from(std::iter::once("phantom loop stop").chain(args[1..].iter().map(String::as_str)))
    } else {
        StopArgs::parse_from(std::iter::once("phantom loop stop").chain(args.iter().map(String::as_str)))
    };
    // Same cross-process limitation as `status` — the `stop` command
    // would need to signal a separate process. Document and exit.
    eprintln!(
        "phantom loop stop: cross-process stop is not yet implemented. \
         Send SIGINT (Ctrl-C) to the running `phantom loop run` process \
         to stop every loop it owns."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers — make the loop spec `discover_specs` walker keep a stable
// LoopId for repeated invocations. Reserved for future per-run snapshotting.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn next_loop_id(registry: &LoopRegistry) -> LoopId {
    registry.alloc_id()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discover_specs_returns_empty_on_missing_dir() {
        let tmp = tempdir().unwrap();
        let specs = discover_specs(&tmp.path().join("missing")).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn discover_specs_finds_a_valid_toml_file() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".phantom").join("loops");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reviewer.toml"),
            r#"
id = "reviewer"

[source]
kind = "cron"
interval_seconds = 60
"#,
        )
        .unwrap();
        let specs = discover_specs(&dir).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].0.id, "reviewer");
    }

    #[test]
    fn discover_specs_ignores_non_toml_files() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".phantom").join("loops");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "stuff").unwrap();
        let specs = discover_specs(&dir).unwrap();
        assert_eq!(specs.len(), 0);
    }
}
