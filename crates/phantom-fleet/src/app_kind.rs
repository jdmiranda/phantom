//! The tagged union of hosted app entries inside a [`crate::FleetSpec`].
//!
//! Each variant maps to one [`phantom_adapter::AppAdapter`] the fleet
//! instantiates at boot:
//!
//! - [`AppKind::Builder`] — wraps a `phantom-builder` per-repo instance.
//!   Only compiled when the `builder-apps` feature is on; otherwise the
//!   runner returns a clear `Unsupported` error so operators know to
//!   rebuild with the feature flag once the sibling agent's crate lands.
//! - [`AppKind::Loop`] — a directory of `phantom-loop` specs. The fleet
//!   reuses the existing [`phantom_loop::LoopRunner`] infrastructure and
//!   wires it to the shared queue registry + dispatcher.
//! - [`AppKind::Custom`] — reserved for in-process custom adapters supplied
//!   by callers (e.g. test mocks). The TOML loader accepts the entry but
//!   the runner has no built-in handling; callers register a builder via
//!   [`crate::FleetRunner::register_custom_factory`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Tagged union over the supported hosted-app entries.
///
/// Encoded in TOML with `kind` as the discriminator (snake_case variants):
///
/// ```toml
/// [[apps]]
/// kind = "builder"
/// slug = "jdmiranda/phantom"
/// trust_band = 1
/// loops = ["reviewer"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppKind {
    /// Per-repo autonomous builder. Reuses the `phantom-builder` crate when
    /// the `builder-apps` feature is enabled at compile time.
    Builder(BuilderSpec),

    /// A directory of `phantom-loop` specs to run inside this fleet.
    Loop(LoopAppSpec),

    /// Caller-supplied adapter. The fleet does nothing with this entry by
    /// default — callers register a factory at runtime to instantiate one.
    Custom(CustomAppSpec),
}

/// Builder app entry. Treated as a black box — the only assumption is that
/// `phantom-builder` exposes a constructor that accepts a config struct
/// derived from these fields and returns something implementing
/// [`phantom_adapter::AppAdapter`].
///
/// We do **not** reach into builder internals beyond that boundary. If the
/// sibling agent's API turns out different, only the integration shim in
/// [`crate::run`] needs to change.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BuilderSpec {
    /// GitHub `owner/repo` slug the builder targets.
    pub slug: String,

    /// Operator-chosen trust level the builder applies to its agents.
    /// Semantically identical to `phantom-agents`' `TrustBand` but kept as a
    /// raw `u8` here so the fleet does not need to depend on the trust enum.
    #[serde(default)]
    pub trust_band: u8,

    /// Names of the per-repo loops the builder should run. Empty means
    /// "every loop the builder discovers in its repo".
    #[serde(default)]
    pub loops: Vec<String>,

    /// Optional per-hour rate cap on agent-opened PRs. `None` defers to the
    /// builder's own default.
    #[serde(default)]
    pub max_prs_per_hour: Option<u32>,

    /// Don't actually merge anything — dry-run mode. Builder is expected to
    /// honour this even though the fleet doesn't enforce it.
    #[serde(default)]
    pub dry_run: bool,

    /// Forward-compatible bag for fields the current schema doesn't know
    /// about. Lets a future builder version add knobs without breaking the
    /// fleet's parse.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `phantom-loop` directory entry. Reuses existing loop infrastructure.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoopAppSpec {
    /// Directory containing the loop spec TOML files.
    pub spec_dir: PathBuf,

    /// Names (loop `id` field) of the specs to run from `spec_dir`. Empty
    /// means "run every spec in the directory".
    #[serde(default)]
    pub loops: Vec<String>,
}

/// Custom-app entry. The fleet runner does not instantiate anything for this
/// variant out of the box — operators supply a factory at runtime.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CustomAppSpec {
    /// Operator-chosen type tag the runner uses to look up a factory.
    #[serde(rename = "type")]
    pub app_type: String,

    /// Free-form params passed to the registered factory.
    #[serde(default)]
    pub params: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_spec_serialises_with_kind_tag() {
        let kind = AppKind::Builder(BuilderSpec {
            slug: "jdmiranda/phantom".to_string(),
            trust_band: 1,
            loops: vec!["reviewer".to_string()],
            max_prs_per_hour: Some(5),
            dry_run: true,
            extra: serde_json::Map::new(),
        });
        let s = toml::to_string(&kind).unwrap();
        assert!(s.contains("kind = \"builder\""), "got:\n{s}");
        assert!(s.contains("slug = \"jdmiranda/phantom\""));
    }

    #[test]
    fn loop_spec_parses_from_toml() {
        let s = r#"
kind = "loop"
spec_dir = "/tmp/proj/.phantom/loops"
loops = ["reviewer", "implementer"]
"#;
        let kind: AppKind = toml::from_str(s).unwrap();
        assert!(matches!(kind, AppKind::Loop(_)));
    }

    #[test]
    fn custom_spec_parses_with_params() {
        let s = r#"
kind = "custom"
type = "demo"

[params]
counter = 3
"#;
        let kind: AppKind = toml::from_str(s).unwrap();
        let AppKind::Custom(c) = kind else {
            panic!("expected custom");
        };
        assert_eq!(c.app_type, "demo");
        assert_eq!(c.params["counter"], 3);
    }
}
