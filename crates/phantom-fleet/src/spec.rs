//! [`FleetSpec`] TOML schema and loader.
//!
//! A fleet config tells the orchestrator which apps to host inside one
//! Phantom process. The default location is `~/.phantom/fleet.toml`; the
//! `phantom fleet ...` CLI accepts `--config <path>` to override.
//!
//! # Example
//!
//! ```toml
//! [[apps]]
//! kind = "builder"
//! slug = "jdmiranda/phantom"
//! trust_band = 1
//! loops = ["pr_finder_review", "pr_finder_impl", "reviewer", "implementer"]
//! max_prs_per_hour = 5
//!
//! [[apps]]
//! kind = "builder"
//! slug = "jdmiranda/badass-cli"
//! trust_band = 0
//! loops = ["pr_finder_review", "reviewer"]
//! dry_run = true
//!
//! [[apps]]
//! kind = "loop"
//! spec_dir = "/path/to/somewhere/.phantom/loops"
//! loops = ["custom_loop"]
//!
//! [shared]
//! brain_self_improve = true
//! event_log = "~/.phantom/fleet-events.jsonl"
//! ```
//!
//! Use [`FleetSpec::load`] to read + parse a config file or
//! [`FleetSpec::parse_str`] to load one from an in-memory string.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app_kind::AppKind;
use crate::error::{FleetError, FleetResult};

/// Top-level fleet configuration.
///
/// Mirrors the TOML shape documented in the module docs. The `apps` list is
/// the only required field — `shared` defaults to its [`SharedFleetSettings`]
/// default if omitted entirely.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FleetSpec {
    /// Apps to instantiate. Each entry becomes one [`phantom_adapter::AppAdapter`]
    /// running concurrently on the shared substrate.
    #[serde(default)]
    pub apps: Vec<AppKind>,

    /// Fleet-wide settings: brain self-improvement toggle, shared event log
    /// path, etc.
    #[serde(default)]
    pub shared: SharedFleetSettings,
}

/// Fleet-wide settings affecting every hosted app.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SharedFleetSettings {
    /// Whether the shared brain runs its self-improvement reconciler. Default
    /// `true` so builders get auto-discovered goals from each target repo's
    /// open issues + CI failures without further configuration.
    #[serde(default = "default_true")]
    pub brain_self_improve: bool,

    /// Optional path to a JSONL event log written by the fleet event bus
    /// forwarder. `None` (the default) disables the log entirely. Tildes are
    /// not expanded by this crate — the CLI may pre-expand if needed.
    #[serde(default)]
    pub event_log: Option<PathBuf>,
}

impl Default for SharedFleetSettings {
    fn default() -> Self {
        Self {
            brain_self_improve: true,
            event_log: None,
        }
    }
}

const fn default_true() -> bool {
    true
}

impl FleetSpec {
    /// Load + parse a fleet config from the given path.
    ///
    /// # Errors
    ///
    /// Returns [`FleetError::ConfigRead`] if the file is unreadable or
    /// [`FleetError::ConfigParse`] if the TOML cannot be deserialised.
    pub fn load(path: &Path) -> FleetResult<Self> {
        let raw = fs::read_to_string(path).map_err(|source| FleetError::ConfigRead {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse_str(&raw)
    }

    /// Parse a fleet config from an in-memory string. Used by tests and the
    /// `fleet init` command's round-trip check.
    ///
    /// # Errors
    ///
    /// Returns [`FleetError::ConfigParse`] when the TOML is malformed or does
    /// not match the documented schema.
    pub fn parse_str(s: &str) -> FleetResult<Self> {
        let spec: Self = toml::from_str(s)?;
        Ok(spec)
    }

    /// Serialise this spec back to a pretty-printed TOML string. Used by
    /// `phantom fleet init` to write a starter config to disk.
    #[must_use]
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "fleet spec did not round-trip as TOML; emitting placeholder");
            "# fleet spec did not round-trip; check the spec shape\n".to_string()
        })
    }

    /// Build a default fleet spec with a single builder entry pointing at
    /// `jdmiranda/phantom`. Suitable as the seed for `phantom fleet init`.
    #[must_use]
    pub fn default_example() -> Self {
        Self {
            apps: vec![AppKind::Builder(crate::app_kind::BuilderSpec {
                slug: "jdmiranda/phantom".to_string(),
                trust_band: 0,
                loops: vec![
                    "pr_finder_review".to_string(),
                    "reviewer".to_string(),
                ],
                max_prs_per_hour: Some(5),
                dry_run: true,
                extra: serde_json::Map::new(),
            })],
            shared: SharedFleetSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_kind::{AppKind, BuilderSpec};

    #[test]
    fn parse_empty_spec_yields_no_apps() {
        let spec = FleetSpec::parse_str("").unwrap();
        assert!(spec.apps.is_empty());
        assert!(spec.shared.brain_self_improve);
        assert!(spec.shared.event_log.is_none());
    }

    #[test]
    fn parse_one_builder_app() {
        let s = r#"
[[apps]]
kind = "builder"
slug = "jdmiranda/phantom"
trust_band = 1
loops = ["reviewer", "implementer"]
max_prs_per_hour = 5
"#;
        let spec = FleetSpec::parse_str(s).unwrap();
        assert_eq!(spec.apps.len(), 1);
        match &spec.apps[0] {
            AppKind::Builder(b) => {
                assert_eq!(b.slug, "jdmiranda/phantom");
                assert_eq!(b.trust_band, 1);
                assert_eq!(b.loops, vec!["reviewer", "implementer"]);
                assert_eq!(b.max_prs_per_hour, Some(5));
                assert!(!b.dry_run);
            }
            other => panic!("expected Builder, got {other:?}"),
        }
    }

    #[test]
    fn parse_mixed_apps_and_shared_section() {
        let s = r#"
[[apps]]
kind = "builder"
slug = "jdmiranda/phantom"
trust_band = 1
loops = ["reviewer"]

[[apps]]
kind = "loop"
spec_dir = "/tmp/foo/.phantom/loops"
loops = ["custom"]

[shared]
brain_self_improve = false
event_log = "/tmp/fleet.jsonl"
"#;
        let spec = FleetSpec::parse_str(s).unwrap();
        assert_eq!(spec.apps.len(), 2);
        assert!(matches!(spec.apps[0], AppKind::Builder(_)));
        assert!(matches!(spec.apps[1], AppKind::Loop(_)));
        assert!(!spec.shared.brain_self_improve);
        assert_eq!(
            spec.shared.event_log.as_deref(),
            Some(std::path::Path::new("/tmp/fleet.jsonl"))
        );
    }

    #[test]
    fn default_example_round_trips_through_toml() {
        let spec = FleetSpec::default_example();
        let s = spec.to_toml();
        let reparsed = FleetSpec::parse_str(&s).unwrap();
        assert_eq!(reparsed.apps.len(), 1);
        assert!(matches!(reparsed.apps[0], AppKind::Builder(_)));
    }

    #[test]
    fn load_missing_file_returns_config_read_error() {
        let err = FleetSpec::load(std::path::Path::new("/does/not/exist.toml")).unwrap_err();
        assert!(matches!(err, FleetError::ConfigRead { .. }));
    }

    #[test]
    fn parse_invalid_toml_returns_config_parse_error() {
        let s = "this is = not valid toml [[";
        let err = FleetSpec::parse_str(s).unwrap_err();
        assert!(matches!(err, FleetError::ConfigParse(_)));
    }

    #[test]
    fn builder_extra_fields_round_trip() {
        // Future-compat: unknown keys go into `extra` rather than failing.
        let s = r#"
[[apps]]
kind = "builder"
slug = "jdmiranda/x"
trust_band = 0
loops = []
some_future_field = "v2-feature"
"#;
        let spec = FleetSpec::parse_str(s).unwrap();
        let AppKind::Builder(b) = &spec.apps[0] else {
            panic!("expected builder");
        };
        // Unknown fields land in `extra` because BuilderSpec uses
        // `#[serde(flatten)]` on a JSON map. Confirms forward compatibility.
        assert!(b.extra.contains_key("some_future_field"));
    }

    #[test]
    fn loop_spec_default_loops_is_empty() {
        let s = r#"
[[apps]]
kind = "loop"
spec_dir = "/tmp/p/.phantom/loops"
"#;
        let spec = FleetSpec::parse_str(s).unwrap();
        let AppKind::Loop(l) = &spec.apps[0] else {
            panic!("expected loop");
        };
        assert!(l.loops.is_empty());
        assert_eq!(l.spec_dir, PathBuf::from("/tmp/p/.phantom/loops"));
    }

    #[test]
    fn builder_spec_directly_unused_fields_stay_typed() {
        // Just a guard that the builder spec defaults compile and serialise.
        let b = BuilderSpec {
            slug: "x/y".to_string(),
            trust_band: 0,
            loops: vec!["a".to_string()],
            max_prs_per_hour: None,
            dry_run: false,
            extra: serde_json::Map::new(),
        };
        let s = toml::to_string(&b).unwrap();
        assert!(s.contains("slug"));
    }
}
