//! `phantom fleet` CLI surface — the production entry-point for the
//! "app of apps" meta-orchestrator (see [`phantom_fleet`]).
//!
//! Mirrors the structure of [`crate::loop_cli`] and [`crate::auth_cli`]:
//! the top-level `phantom` binary detects `argv[1] == "fleet"` and hands
//! the rest to [`run_fleet_subcommand`].
//!
//! ```text
//! phantom fleet run      [--config <path>]
//! phantom fleet list     [--config <path>]
//! phantom fleet validate [--config <path>]
//! phantom fleet init     [--config <path>]
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use phantom_fleet::{AppKind, FleetRunner, FleetSpec};

/// Top-level dispatcher: `phantom fleet <subcommand> ...`
///
/// Called from `main.rs` when `argv[1] == "fleet"`.
pub fn run_fleet_subcommand(args: &[String]) -> Result<()> {
    match args.get(2).map(String::as_str) {
        Some("run") => run_command(&args[2..]),
        Some("list") => list_command(&args[2..]),
        Some("validate") => validate_command(&args[2..]),
        Some("init") => init_command(&args[2..]),
        _ => {
            print_fleet_help();
            Ok(())
        }
    }
}

fn print_fleet_help() {
    eprintln!(
        "phantom fleet — run the 'app of apps' meta-orchestrator\n\
         \n\
         USAGE:\n\
             phantom fleet run      [--config <path>]\n\
             phantom fleet list     [--config <path>]\n\
             phantom fleet validate [--config <path>]\n\
             phantom fleet init     [--config <path>]\n\
         \n\
         The default config path is ~/.phantom/fleet.toml. See\n\
         phantom-fleet/src/spec.rs for the TOML schema.\n"
    );
}

// ---------------------------------------------------------------------------
// `phantom fleet run`
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Parser)]
#[command(name = "phantom fleet run")]
struct RunArgs {
    /// Fleet config path. Defaults to `~/.phantom/fleet.toml`.
    #[arg(long)]
    config: Option<PathBuf>,
}

fn run_command(args: &[String]) -> Result<()> {
    use clap::Parser;
    let parsed = if args.first().map(String::as_str) == Some("run") {
        RunArgs::parse_from(
            std::iter::once("phantom fleet run").chain(args[1..].iter().map(String::as_str)),
        )
    } else {
        RunArgs::parse_from(
            std::iter::once("phantom fleet run").chain(args.iter().map(String::as_str)),
        )
    };

    let path = resolve_config_path(parsed.config.as_deref())?;
    eprintln!("phantom fleet run: loading config from {}", path.display());
    let spec = FleetSpec::load(&path)?;
    eprintln!(
        "phantom fleet run: spec contains {} apps; brain_self_improve = {}",
        spec.apps.len(),
        spec.shared.brain_self_improve
    );

    // Build a multi-thread runtime and drive the runner on it. This is the
    // same shape as `loop_cli::run_command` so behavior is consistent
    // across the two CLIs.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        match phantom_fleet::run_fleet(spec).await {
            Ok(()) => Ok::<(), anyhow::Error>(()),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// `phantom fleet list`
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Parser)]
#[command(name = "phantom fleet list")]
struct ListArgs {
    #[arg(long)]
    config: Option<PathBuf>,
}

fn list_command(args: &[String]) -> Result<()> {
    use clap::Parser;
    let parsed = if args.first().map(String::as_str) == Some("list") {
        ListArgs::parse_from(
            std::iter::once("phantom fleet list").chain(args[1..].iter().map(String::as_str)),
        )
    } else {
        ListArgs::parse_from(
            std::iter::once("phantom fleet list").chain(args.iter().map(String::as_str)),
        )
    };

    let path = resolve_config_path(parsed.config.as_deref())?;
    let spec = FleetSpec::load(&path)?;
    println!("Fleet apps configured in {}:", path.display());
    for (i, app) in spec.apps.iter().enumerate() {
        let line = match app {
            AppKind::Builder(b) => format!(
                "  [{i}] builder  slug={} trust_band={} loops={:?} dry_run={}",
                b.slug, b.trust_band, b.loops, b.dry_run
            ),
            AppKind::Loop(l) => format!(
                "  [{i}] loop     spec_dir={} loops={:?}",
                l.spec_dir.display(),
                l.loops
            ),
            AppKind::Custom(c) => {
                format!("  [{i}] custom   type={} params={}", c.app_type, c.params)
            }
        };
        println!("{line}");
    }
    println!(
        "Shared: brain_self_improve={} event_log={:?}",
        spec.shared.brain_self_improve, spec.shared.event_log
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `phantom fleet validate`
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Parser)]
#[command(name = "phantom fleet validate")]
struct ValidateArgs {
    #[arg(long)]
    config: Option<PathBuf>,
}

fn validate_command(args: &[String]) -> Result<()> {
    use clap::Parser;
    let parsed = if args.first().map(String::as_str) == Some("validate") {
        ValidateArgs::parse_from(
            std::iter::once("phantom fleet validate")
                .chain(args[1..].iter().map(String::as_str)),
        )
    } else {
        ValidateArgs::parse_from(
            std::iter::once("phantom fleet validate")
                .chain(args.iter().map(String::as_str)),
        )
    };
    let path = resolve_config_path(parsed.config.as_deref())?;
    let spec = FleetSpec::load(&path)?;
    let runner = FleetRunner::new(spec);
    let errors = runner.validate();
    if errors.is_empty() {
        println!("phantom fleet validate: OK ({} apps)", runner.spec().apps.len());
        Ok(())
    } else {
        eprintln!("phantom fleet validate: {} error(s) found:", errors.len());
        for e in &errors {
            eprintln!("  - {e}");
        }
        bail!("validation failed");
    }
}

// ---------------------------------------------------------------------------
// `phantom fleet init`
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Parser)]
#[command(name = "phantom fleet init")]
struct InitArgs {
    #[arg(long)]
    config: Option<PathBuf>,
}

fn init_command(args: &[String]) -> Result<()> {
    use clap::Parser;
    let parsed = if args.first().map(String::as_str) == Some("init") {
        InitArgs::parse_from(
            std::iter::once("phantom fleet init").chain(args[1..].iter().map(String::as_str)),
        )
    } else {
        InitArgs::parse_from(
            std::iter::once("phantom fleet init").chain(args.iter().map(String::as_str)),
        )
    };
    let path = resolve_config_path(parsed.config.as_deref())?;
    if path.exists() {
        bail!(
            "phantom fleet init: refusing to overwrite existing config at {}",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let spec = FleetSpec::default_example();
    fs::write(&path, spec.to_toml())?;
    println!("phantom fleet init: wrote default config to {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the path to the fleet config, defaulting to
/// `~/.phantom/fleet.toml` (with `~` expanded against `$HOME`).
fn resolve_config_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME env var unset"))?;
    Ok(PathBuf::from(home).join(".phantom").join("fleet.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_config_path_uses_override_when_provided() {
        let p = Path::new("/tmp/foo/fleet.toml");
        let resolved = resolve_config_path(Some(p)).unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/foo/fleet.toml"));
    }

    #[test]
    fn resolve_config_path_defaults_to_dot_phantom() {
        // Test the default path shape; doesn't depend on the real $HOME being
        // present at any particular value.
        // SAFETY: $HOME is a process-global; we set it for this test and
        // restore it after to avoid leaking state to other tests in the
        // same process.
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/tmp/fakehome");
        }
        let resolved = resolve_config_path(None).unwrap();
        assert_eq!(
            resolved,
            PathBuf::from("/tmp/fakehome/.phantom/fleet.toml")
        );
        if let Some(prev) = prev {
            unsafe {
                std::env::set_var("HOME", prev);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
    }
}
