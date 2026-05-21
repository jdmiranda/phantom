//! `phantom builder` CLI surface — production entry-point for pointing
//! Phantom at any GitHub repo and having it eat the open issues
//! autonomously.
//!
//! Mirrors the shape of [`crate::loop_cli`]:
//!
//! ```text
//! phantom builder run <owner>/<repo> [--repo-path <path>]
//!                                    [--trust-band 0|1|2|3]
//!                                    [--loops <names>]
//!                                    [--dry-run]
//!                                    [--max-prs-per-hour N]
//!                                    [--max-concurrent N]
//!                                    [--label-filter L1,L2,...]
//! phantom builder list
//! phantom builder status <owner>/<repo>
//! ```
//!
//! The heavy lifting lives in [`phantom_builder`]: this module is a thin
//! shim that parses CLI flags, constructs a [`phantom_builder::BuilderConfig`],
//! and hands it to [`phantom_builder::Builder::run`].

use std::path::PathBuf;

use anyhow::{Result, bail};
use phantom_builder::{
    Builder, BuilderConfig, BuilderSafetyConfig, TrustBandConfig,
};

/// Top-level dispatcher: `phantom builder <subcommand> ...`. Called from
/// `main.rs` when `argv[1] == "builder"`.
pub fn run_builder_subcommand(args: &[String]) -> Result<()> {
    match args.get(2).map(String::as_str) {
        Some("run") => run_command(&args[2..]),
        Some("list") => list_command(),
        Some("status") => status_command(&args[2..]),
        _ => {
            print_builder_help();
            Ok(())
        }
    }
}

/// Print the human-readable usage banner.
fn print_builder_help() {
    eprintln!(
        "phantom builder — point Phantom at any GitHub repo and eat all the issues\n\
         \n\
         USAGE:\n\
             phantom builder run    <owner>/<repo> [flags]\n\
             phantom builder list\n\
             phantom builder status <owner>/<repo>\n\
         \n\
         FLAGS for `run`:\n\
             --repo-path <path>          override local checkout (default: ~/.phantom/builds/<o>-<r>)\n\
             --trust-band 0|1|2|3        0=suggestion-only, 1=conservative (default), 2=standard, 3=aggressive\n\
             --loops a,b,c               comma-separated loop names (default: the canonical four)\n\
             --dry-run                   score + log but never enqueue\n\
             --max-prs-per-hour N        absolute per-hour cap (default 5)\n\
             --max-concurrent N          max simultaneous agents (default 2)\n\
             --label-filter L1,L2,...    only consider issues with these labels (default: ALL open issues)\n"
    );
}

// ---------------------------------------------------------------------------
// `phantom builder run`
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Parser)]
#[command(name = "phantom builder run")]
struct RunArgs {
    /// `owner/repo` slug of the target repository.
    target: String,

    /// Use an existing local checkout instead of cloning.
    #[arg(long)]
    repo_path: Option<PathBuf>,

    /// Trust band the brain operates at. 0 = suggestion-only, 1 = conservative
    /// (default), 2 = standard, 3 = aggressive.
    #[arg(long, default_value_t = 1)]
    trust_band: u8,

    /// Comma-separated list of loop names to start. Defaults to the canonical
    /// four-loop pipeline (pr_finder_review, pr_finder_impl, reviewer,
    /// implementer).
    #[arg(long)]
    loops: Option<String>,

    /// Dry-run mode: brain scores and logs but never enqueues.
    #[arg(long)]
    dry_run: bool,

    /// Absolute per-hour cap on auto-enqueues.
    #[arg(long, default_value_t = 5)]
    max_prs_per_hour: u32,

    /// Maximum simultaneous agents across every loop.
    #[arg(long, default_value_t = 2)]
    max_concurrent: u8,

    /// Comma-separated GitHub issue labels to filter on (default: ALL open
    /// issues).
    #[arg(long)]
    label_filter: Option<String>,
}

fn run_command(args: &[String]) -> Result<()> {
    use clap::Parser;
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
    let parsed = if args.first().map(String::as_str) == Some("run") {
        RunArgs::parse_from(
            std::iter::once("phantom builder run").chain(args[1..].iter().map(String::as_str)),
        )
    } else {
        RunArgs::parse_from(
            std::iter::once("phantom builder run").chain(args.iter().map(String::as_str)),
        )
    };

    let trust_band = match parsed.trust_band {
        0 => TrustBandConfig::SuggestionOnly,
        1 => TrustBandConfig::Conservative,
        2 => TrustBandConfig::Standard,
        3 => TrustBandConfig::Aggressive,
        other => bail!("invalid --trust-band {other}; expected 0, 1, 2, or 3"),
    };

    let loops = parsed
        .loops
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| {
            vec![
                "pr_finder_review".to_string(),
                "pr_finder_impl".to_string(),
                "reviewer".to_string(),
                "implementer".to_string(),
            ]
        });

    let label_filter = parsed.label_filter.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect::<Vec<_>>()
    });

    let cfg = BuilderConfig {
        target_slug: parsed.target.clone(),
        repo_path: parsed.repo_path,
        trust_band,
        label_filter,
        safety: BuilderSafetyConfig {
            max_prs_per_hour: parsed.max_prs_per_hour,
            max_concurrent_agents: parsed.max_concurrent,
            dry_run: parsed.dry_run,
        },
        loops,
    };

    eprintln!(
        "phantom builder run: target={} band={:?} dry-run={} max-prs/h={} max-concurrent={}",
        cfg.target_slug,
        cfg.trust_band,
        cfg.safety.dry_run,
        cfg.safety.max_prs_per_hour,
        cfg.safety.max_concurrent_agents,
    );

    let result = Builder::new(cfg).run().map_err(|e| anyhow::anyhow!(e))?;
    eprintln!(
        "phantom builder run: done. checkout={} seeded={} started={}",
        result.repo_path.display(),
        result.seeded_specs.len(),
        result.started_loops,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `phantom builder list`
// ---------------------------------------------------------------------------

fn list_command() -> Result<()> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => bail!("could not resolve $HOME"),
    };
    let builds = home.join(".phantom").join("builds");
    if !builds.exists() {
        eprintln!("phantom builder list: no builds yet ({} missing)", builds.display());
        return Ok(());
    }
    eprintln!("phantom builder list: builds under {}:", builds.display());
    let mut found_any = false;
    for entry in std::fs::read_dir(&builds)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let lock = path.join(".phantom").join("loops").join(".runlock");
        let live = if lock.exists() { "[runlock present]" } else { "" };
        println!("  {name:<40} {live}");
        found_any = true;
    }
    if !found_any {
        eprintln!("  (no build directories found)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `phantom builder status`
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Parser)]
#[command(name = "phantom builder status")]
struct StatusArgs {
    /// `owner/repo` slug to check.
    target: String,
}

fn status_command(args: &[String]) -> Result<()> {
    use clap::Parser;
    let parsed = if args.first().map(String::as_str) == Some("status") {
        StatusArgs::parse_from(
            std::iter::once("phantom builder status").chain(args[1..].iter().map(String::as_str)),
        )
    } else {
        StatusArgs::parse_from(
            std::iter::once("phantom builder status").chain(args.iter().map(String::as_str)),
        )
    };

    let (owner, repo) = match parsed.target.split_once('/') {
        Some(p) => p,
        None => bail!("invalid slug `{}` — expected owner/repo", parsed.target),
    };
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => bail!("could not resolve $HOME"),
    };
    let dir = home.join(".phantom").join("builds").join(format!("{owner}-{repo}"));
    if !dir.exists() {
        eprintln!(
            "phantom builder status: no build directory at {} — run `phantom builder run {}` first",
            dir.display(),
            parsed.target
        );
        return Ok(());
    }
    let lock = dir.join(".phantom").join("loops").join(".runlock");
    let live = lock.exists();
    eprintln!("phantom builder status: {}", parsed.target);
    eprintln!("  checkout: {}", dir.display());
    eprintln!(
        "  runlock:  {} {}",
        lock.display(),
        if live { "(present — a builder run may be active)" } else { "(absent)" }
    );
    // Cross-process status is not implemented; mirror loop_cli's behavior.
    eprintln!(
        "  note: cross-process status reporting is not yet implemented. \
         Inspect the running `phantom builder run` process's stderr for \
         per-tick status lines."
    );
    Ok(())
}
