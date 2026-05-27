//! Configuration loading for Phantom.
//!
//! Reads `~/.config/phantom/config.toml` (or `$XDG_CONFIG_HOME/phantom/config.toml`)
//! and applies overrides to the default theme and shader parameters.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use log::{debug, info, warn};

use phantom_ui::themes::{self, Theme};

/// A single MCP server to connect on startup.
///
/// Configured under `[mcp_servers.<name>]` table headers in the TOML config.
/// Each block must provide `url`. `enabled` defaults to `true`.
///
/// Example:
/// ```toml
/// [mcp_servers.my-server]
/// url = "ws://localhost:8765"
/// enabled = true
/// ```
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// The logical name used for this server in the tool registry.
    pub name: String,
    /// WebSocket URL for the MCP server (`ws://` or `wss://`).
    pub url: String,
    /// When `false` the server is skipped during discovery. Defaults to `true`.
    pub enabled: bool,
}

/// User-configurable settings loaded from TOML.
#[derive(Debug, Clone)]
pub struct PhantomConfig {
    /// Which built-in theme to use.
    pub theme_name: String,
    /// Font size in points.
    pub font_size: f32,
    /// Optional font family name (e.g. "Fira Code"). When `None`, the system
    /// monospace font is used.
    pub font_family: Option<String>,
    /// Shader param overrides (applied on top of the theme defaults).
    pub shader_overrides: ShaderOverrides,
    /// Skip the boot sequence and go straight to terminal.
    pub skip_boot: bool,
    /// Demo mode: auto-skip boot, spawn an example agent on first frame.
    pub demo_mode: bool,
    /// Enable the LLM-backed NLP translate fallback.
    ///
    /// When `true` (default) and `ANTHROPIC_API_KEY` is set, natural-language
    /// commands that the heuristic interpreter can't classify are sent to the
    /// Claude API for structured intent extraction. Set to `false` (or
    /// `nlp_llm = false` / `nlp_llm = 0` in the config file) to disable the
    /// network call and fall back directly to spawning an agent.
    pub(crate) nlp_llm_enabled: bool,
    /// Privacy mode — when `true`, all cloud API calls (Claude, OpenAI-compat)
    /// are blocked at the application level.
    ///
    /// Set via `privacy_mode = true` in `~/.config/phantom/config.toml`
    /// or toggled at runtime with `privacy on` / `privacy off`.
    /// Local backends (Ollama, heuristic) are unaffected.
    pub privacy_mode: bool,
    /// Offline mode — when `true`, only local backends (Ollama, heuristic) are
    /// used. Cloud backends are filtered out at routing time.
    ///
    /// Set via `offline_mode = true` in `~/.config/phantom/config.toml`
    /// or toggled at runtime with `offline on` / `offline off`.
    /// Can also be auto-enabled after 3 consecutive cloud backend failures.
    pub offline_mode: bool,
    /// Preferred AI provider for the brain router.
    ///
    /// When set, the brain router promotes the named backend to the front of the
    /// cascade so it is tried first for every task tier it supports. Valid values
    /// are any profile ID registered in the [`ProviderCatalog`]:
    /// `"claude-default"`, `"claude-fast"`, `"ollama-phi3.5"`, `"ollama-llama3"`,
    /// or any custom profile added at runtime.
    ///
    /// Set via `preferred_provider = "claude-fast"` in
    /// `~/.config/phantom/config.toml`. When absent or `None`, the default
    /// cascade order (heuristic → ollama → claude) is used.
    pub preferred_provider: Option<String>,
    /// Start in borderless fullscreen mode.
    ///
    /// Set via `fullscreen = true` in `~/.config/phantom/config.toml` or
    /// the `--fullscreen` CLI flag. Can be toggled at runtime with F11 /
    /// Cmd+Enter regardless of this initial value.
    pub fullscreen: bool,
    /// Per-category notification sound paths.
    ///
    /// Keys correspond to [`crate::notifications::Severity`] names in lowercase:
    /// `"info"`, `"warn"`, `"danger"`. Values are optional file paths to `.wav`
    /// or `.mp3` audio files.
    ///
    /// - Missing key → use the default system sound for that category.
    /// - `Some(path)` → play that audio file.
    /// - `None` (or empty string `""` in TOML) → silent for that category.
    ///
    /// Configure in `~/.config/phantom/config.toml`:
    ///
    /// ```toml
    /// [notification_sounds]
    /// info = "/path/to/info.wav"
    /// warn = "/path/to/warn.wav"
    /// danger = ""  # empty string = silent
    /// ```
    pub notification_sounds: HashMap<String, Option<String>>,
    /// MCP servers to auto-connect on startup.
    ///
    /// Each entry is parsed from a `[mcp_servers.<name>]` TOML table.
    /// Empty by default. Servers with `enabled = false` are skipped during
    /// discovery.
    pub mcp_servers: Vec<McpServerConfig>,
}

/// Optional overrides for shader parameters. `None` means use theme default.
#[derive(Debug, Clone, Default)]
pub struct ShaderOverrides {
    pub scanline_intensity: Option<f32>,
    pub bloom_intensity: Option<f32>,
    pub chromatic_aberration: Option<f32>,
    pub curvature: Option<f32>,
    pub vignette_intensity: Option<f32>,
    pub noise_intensity: Option<f32>,
}

impl Default for PhantomConfig {
    fn default() -> Self {
        Self {
            theme_name: "phosphor".to_string(),
            font_size: 14.0,
            font_family: None,
            shader_overrides: ShaderOverrides::default(),
            // Boot cinematic is opt-in (--boot). Default skip keeps fast
            // iteration the common case; preserve the user's
            // `feedback_phantom_boot` preference (dismiss boot for speed).
            // The `--boot` CLI flag and `skip_boot = false` in TOML both
            // override this to opt the cinematic back in.
            skip_boot: true,
            demo_mode: false,
            nlp_llm_enabled: true,
            privacy_mode: false,
            offline_mode: false,
            preferred_provider: None,
            // Phantom IS the AI; the AI deserves the whole screen. Changed
            // 2026-05-20 — see `feedback_agent_is_primary` memory entry.
            fullscreen: true,
            notification_sounds: HashMap::new(),
            mcp_servers: Vec::new(),
        }
    }
}

impl PhantomConfig {
    /// Load config from the standard path, or return defaults if not found.
    #[must_use]
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(config) => {
                info!("Loaded config from {}", config_path().display());
                config
            }
            Err(e) => {
                debug!("Using default config: {e}");
                Self::default()
            }
        }
    }

    fn try_load() -> Result<Self> {
        let path = config_path();
        let content = fs::read_to_string(&path)?;
        Self::parse(&content)
    }

    fn parse(toml_str: &str) -> Result<Self> {
        let mut config = Self::default();
        // Track whether we're inside the [notification_sounds] section.
        let mut in_notification_sounds = false;

        // Track the current `[mcp_servers.<name>]` block being parsed.
        let mut current_mcp_server: Option<McpServerConfig> = None;
        // Whether we are inside an [mcp_servers.*] section.
        let mut in_mcp_section = false;

        for line in toml_str.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // TOML section header — detect [notification_sounds], [mcp_servers.<name>],
            // and any other section that would end them.
            if line.starts_with('[') {
                in_notification_sounds = line == "[notification_sounds]";

                // Flush any in-progress mcp_server block.
                if let Some(server) = current_mcp_server.take() {
                    config.mcp_servers.push(server);
                }

                // `[mcp_servers.<name>]`
                if line.starts_with("[mcp_servers.") && line.ends_with(']') {
                    let name = &line["[mcp_servers.".len()..line.len() - 1];
                    current_mcp_server = Some(McpServerConfig {
                        name: name.to_string(),
                        url: String::new(),
                        enabled: true,
                    });
                    in_mcp_section = true;
                } else {
                    in_mcp_section = false;
                }
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"');

                if in_notification_sounds {
                    // Keys: "info", "warn", "danger" (maps to Severity variants).
                    // Empty string → None (silent); any other string → Some(path).
                    let sound = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                    config.notification_sounds.insert(key.to_string(), sound);
                    continue;
                }

                // Key-value pairs inside an mcp_servers block.
                if in_mcp_section {
                    if let Some(ref mut server) = current_mcp_server {
                        match key {
                            "url" => server.url = value.to_string(),
                            "enabled" => {
                                server.enabled = !matches!(value, "false" | "0" | "no");
                            }
                            _ => {
                                warn!("Unknown mcp_servers key: {key}");
                            }
                        }
                    }
                    continue;
                }

                match key {
                    "theme" => config.theme_name = value.to_string(),
                    "font_size" => {
                        if let Ok(v) = value.parse::<f32>() {
                            config.font_size = v;
                        }
                    }
                    "font_family" => {
                        if !value.is_empty() {
                            config.font_family = Some(value.to_string());
                        }
                    }
                    "scanline_intensity" => {
                        if let Ok(v) = value.parse::<f32>() {
                            config.shader_overrides.scanline_intensity = Some(v);
                        }
                    }
                    "bloom_intensity" => {
                        if let Ok(v) = value.parse::<f32>() {
                            config.shader_overrides.bloom_intensity = Some(v);
                        }
                    }
                    "chromatic_aberration" => {
                        if let Ok(v) = value.parse::<f32>() {
                            config.shader_overrides.chromatic_aberration = Some(v);
                        }
                    }
                    "curvature" => {
                        if let Ok(v) = value.parse::<f32>() {
                            config.shader_overrides.curvature = Some(v);
                        }
                    }
                    "vignette_intensity" => {
                        if let Ok(v) = value.parse::<f32>() {
                            config.shader_overrides.vignette_intensity = Some(v);
                        }
                    }
                    "noise_intensity" => {
                        if let Ok(v) = value.parse::<f32>() {
                            config.shader_overrides.noise_intensity = Some(v);
                        }
                    }
                    "skip_boot" => {
                        config.skip_boot = matches!(value, "true" | "1" | "yes");
                    }
                    "demo_mode" => {
                        config.demo_mode = matches!(value, "true" | "1" | "yes");
                    }
                    "nlp_llm" => {
                        // Opt-out: `nlp_llm = false` / `0` / `no` disables the
                        // LLM translate call; anything else leaves it enabled.
                        config.nlp_llm_enabled = !matches!(value, "false" | "0" | "no");
                    }
                    "privacy_mode" => {
                        config.privacy_mode = matches!(value, "true" | "1" | "yes");
                    }
                    "offline_mode" => {
                        config.offline_mode = matches!(value, "true" | "1" | "yes");
                    }
                    "preferred_provider" => {
                        // Accept any non-empty string; validated at router construction time.
                        if !value.is_empty() {
                            config.preferred_provider = Some(value.to_string());
                        }
                    }
                    "fullscreen" => {
                        config.fullscreen = matches!(value, "true" | "1" | "yes");
                    }
                    _ => {
                        warn!("Unknown config key: {key}");
                    }
                }
            }
        }

        // Flush the last mcp_server block if any.
        if let Some(server) = current_mcp_server.take() {
            config.mcp_servers.push(server);
        }

        Ok(config)
    }

    /// Resolve the theme: load the named built-in, then apply shader overrides.
    #[must_use]
    pub fn resolve_theme(&self) -> Theme {
        let mut theme = themes::builtin_by_name(&self.theme_name).unwrap_or_else(|| {
            warn!(
                "Unknown theme '{}', falling back to phosphor",
                self.theme_name
            );
            themes::phosphor()
        });

        // Apply shader overrides.
        let sp = &mut theme.shader_params;
        if let Some(v) = self.shader_overrides.scanline_intensity {
            sp.scanline_intensity = v;
        }
        if let Some(v) = self.shader_overrides.bloom_intensity {
            sp.bloom_intensity = v;
        }
        if let Some(v) = self.shader_overrides.chromatic_aberration {
            sp.chromatic_aberration = v;
        }
        if let Some(v) = self.shader_overrides.curvature {
            sp.curvature = v;
        }
        if let Some(v) = self.shader_overrides.vignette_intensity {
            sp.vignette_intensity = v;
        }
        if let Some(v) = self.shader_overrides.noise_intensity {
            sp.noise_intensity = v;
        }

        theme
    }

    /// Returns whether the LLM-backed NLP translate fallback is enabled.
    ///
    /// Use this accessor instead of accessing `nlp_llm_enabled` directly from
    /// outside the crate.
    #[must_use]
    pub fn nlp_llm_enabled(&self) -> bool {
        self.nlp_llm_enabled
    }

    /// Returns whether privacy mode is enabled.
    #[must_use]
    pub fn privacy_mode(&self) -> bool {
        self.privacy_mode
    }

    /// Returns whether offline mode is enabled.
    #[must_use]
    pub fn offline_mode(&self) -> bool {
        self.offline_mode
    }

    /// Returns the preferred AI provider ID, if configured.
    ///
    /// When `Some`, the brain router promotes the named backend to the front of
    /// the cascade. `None` means use default cascade order.
    #[must_use]
    pub fn preferred_provider(&self) -> Option<&str> {
        self.preferred_provider.as_deref()
    }

    /// Look up the configured sound path for a notification category.
    ///
    /// `category` should be `"info"`, `"warn"`, or `"danger"` — matching the
    /// lowercase name of the [`crate::notifications::Severity`] variant.
    ///
    /// Returns:
    /// - `None` if the category has no entry (use the default system sound).
    /// - `None` if the entry is an empty string `""` in TOML (explicit silence).
    /// - `Some(path)` if a custom sound file was configured.
    #[must_use]
    pub fn notification_sound_for(&self, category: &str) -> Option<&str> {
        self.notification_sounds
            .get(category)
            .and_then(|v| v.as_deref())
    }

    /// Write a default config file to the standard path.
    pub fn write_default() -> Result<PathBuf> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, DEFAULT_CONFIG)?;
        Ok(path)
    }
}

/// Standard config file path.
fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("phantom").join("config.toml")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".config")
            .join("phantom")
            .join("config.toml")
    } else {
        PathBuf::from("phantom.toml")
    }
}

const DEFAULT_CONFIG: &str = r#"# Phantom Terminal Configuration
# ================================

# Theme: phosphor, amber, ice, blood, vapor
theme = "phosphor"

# Font size in points
font_size = 14.0

# Font family (optional). Uses system monospace font when not set.
# font_family = "Fira Code"

# CRT Shader Parameters (0.0 - 1.0)
# Uncomment and adjust to override the theme defaults.
# scanline_intensity = 0.18
# bloom_intensity = 0.25
# chromatic_aberration = 0.04
# curvature = 0.06
# vignette_intensity = 0.20
# noise_intensity = 0.02

# Boot animation (also auto-skipped on session restore)
# skip_boot = false

# Privacy mode: block all cloud API calls (Claude, OpenAI-compat).
# Local backends (Ollama, heuristic) continue to work normally.
# Can also be toggled at runtime with `privacy on` / `privacy off`.
# privacy_mode = false

# Offline mode: use only local backends (Ollama, heuristic).
# Cloud backends are filtered out at routing time.
# Can also be toggled at runtime with `offline on` / `offline off`.
# offline_mode = false

# Start in borderless fullscreen mode.
# Can be toggled at runtime with F11 / Cmd+Enter.
# fullscreen = false

# Notification sounds (optional).
# Map severity categories (info / warn / danger) to audio file paths.
# Missing key = use the default system sound.
# Empty string = silent for that category.
# [notification_sounds]
# info = "/path/to/info.wav"
# warn = "/path/to/warn.wav"
# danger = ""

# MCP servers to auto-connect on startup.
# Add one [mcp_servers.<name>] block per server.
# Example:
# [mcp_servers.my-server]
# url = "ws://localhost:8765"
# enabled = true
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_skip_boot_is_true() {
        // Boot cinematic is now opt-in via `--boot` / `skip_boot = false`.
        let config = PhantomConfig::default();
        assert!(
            config.skip_boot,
            "cold-launch default must skip the boot cinematic (opt-in via --boot)"
        );
    }

    #[test]
    fn parse_skip_boot_true() {
        let config = PhantomConfig::parse("skip_boot = true").unwrap();
        assert!(config.skip_boot);
    }

    #[test]
    fn parse_skip_boot_one() {
        let config = PhantomConfig::parse("skip_boot = 1").unwrap();
        assert!(config.skip_boot);
    }

    #[test]
    fn parse_skip_boot_false() {
        let config = PhantomConfig::parse("skip_boot = false").unwrap();
        assert!(!config.skip_boot);
    }

    #[test]
    fn parse_empty_config_yields_defaults() {
        let config = PhantomConfig::parse("").unwrap();
        assert!(config.skip_boot, "default skip_boot must be true (opt-in via --boot)");
        assert!(!config.demo_mode);
        assert_eq!(config.theme_name, "phosphor");
    }

    #[test]
    fn parse_ignores_comments_and_blank_lines() {
        let toml = "# This is a comment\n\nskip_boot = true\n";
        let config = PhantomConfig::parse(toml).unwrap();
        assert!(config.skip_boot);
    }

    #[test]
    fn parse_theme_and_font_size() {
        let toml = "theme = \"amber\"\nfont_size = 16.0\n";
        let config = PhantomConfig::parse(toml).unwrap();
        assert_eq!(config.theme_name, "amber");
        assert!((config.font_size - 16.0).abs() < f32::EPSILON);
    }

    #[test]
    fn nlp_llm_enabled_defaults_to_true() {
        let config = PhantomConfig::default();
        assert!(
            config.nlp_llm_enabled,
            "nlp_llm_enabled must default to true so the LLM path is on by default"
        );
    }

    #[test]
    fn parse_nlp_llm_false_disables_it() {
        let config = PhantomConfig::parse("nlp_llm = false").unwrap();
        assert!(!config.nlp_llm_enabled);
    }

    #[test]
    fn parse_nlp_llm_zero_disables_it() {
        let config = PhantomConfig::parse("nlp_llm = 0").unwrap();
        assert!(!config.nlp_llm_enabled);
    }

    #[test]
    fn parse_nlp_llm_true_stays_enabled() {
        let config = PhantomConfig::parse("nlp_llm = true").unwrap();
        assert!(config.nlp_llm_enabled);
    }

    #[test]
    fn privacy_mode_defaults_to_false() {
        let config = PhantomConfig::default();
        assert!(
            !config.privacy_mode,
            "privacy_mode must default to false so cloud calls work by default"
        );
    }

    #[test]
    fn parse_privacy_mode_true_enables_it() {
        let config = PhantomConfig::parse("privacy_mode = true").unwrap();
        assert!(config.privacy_mode);
    }

    #[test]
    fn parse_privacy_mode_one_enables_it() {
        let config = PhantomConfig::parse("privacy_mode = 1").unwrap();
        assert!(config.privacy_mode);
    }

    #[test]
    fn parse_privacy_mode_false_keeps_it_off() {
        let config = PhantomConfig::parse("privacy_mode = false").unwrap();
        assert!(!config.privacy_mode);
    }

    #[test]
    fn parse_empty_config_privacy_mode_is_false() {
        let config = PhantomConfig::parse("").unwrap();
        assert!(!config.privacy_mode);
    }

    // -----------------------------------------------------------------------
    // fullscreen
    // -----------------------------------------------------------------------

    #[test]
    fn default_fullscreen_is_true_for_agent_first_ux() {
        // Phantom IS the AI; the AI deserves the whole screen.  The cold-launch
        // first impression is a full-window agent (or SetupAdapter when no API
        // key is provisioned).  Users who want a windowed shell set
        // `fullscreen = false` in `~/.config/phantom/config.toml`.
        let config = PhantomConfig::default();
        assert!(
            config.fullscreen,
            "fullscreen must default to true so the agent owns the whole screen on cold launch"
        );
    }

    #[test]
    fn parse_fullscreen_true_enables_it() {
        let config = PhantomConfig::parse("fullscreen = true").unwrap();
        assert!(config.fullscreen);
    }

    #[test]
    fn parse_fullscreen_one_enables_it() {
        let config = PhantomConfig::parse("fullscreen = 1").unwrap();
        assert!(config.fullscreen);
    }

    #[test]
    fn parse_fullscreen_false_keeps_it_off() {
        let config = PhantomConfig::parse("fullscreen = false").unwrap();
        assert!(!config.fullscreen);
    }

    #[test]
    fn parse_empty_config_inherits_default_fullscreen_true() {
        // An empty config TOML inherits all defaults — including the
        // agent-first fullscreen=true cold-launch default.  See
        // `default_fullscreen_is_true_for_agent_first_ux`.
        let config = PhantomConfig::parse("").unwrap();
        assert!(config.fullscreen);
    }

    #[test]
    fn privacy_mode_accessor_matches_field() {
        let mut config = PhantomConfig::default();
        assert!(!config.privacy_mode());
        config.privacy_mode = true;
        assert!(config.privacy_mode());
    }

    #[test]
    fn offline_mode_defaults_to_false() {
        let config = PhantomConfig::default();
        assert!(
            !config.offline_mode,
            "offline_mode must default to false so cloud calls work by default"
        );
    }

    #[test]
    fn parse_offline_mode_true_enables_it() {
        let config = PhantomConfig::parse("offline_mode = true").unwrap();
        assert!(config.offline_mode);
    }

    #[test]
    fn parse_offline_mode_one_enables_it() {
        let config = PhantomConfig::parse("offline_mode = 1").unwrap();
        assert!(config.offline_mode);
    }

    #[test]
    fn parse_offline_mode_false_keeps_it_off() {
        let config = PhantomConfig::parse("offline_mode = false").unwrap();
        assert!(!config.offline_mode);
    }

    // -----------------------------------------------------------------------
    // preferred_provider — BrainConfig router wiring
    // -----------------------------------------------------------------------

    /// When `preferred_provider` is set in config, the resulting `RouterConfig`
    /// must promote the named backend so it appears before other non-heuristic
    /// backends in the cascade.
    ///
    /// This exercises the construction path used in `app.rs` without spinning
    /// up a real brain thread.
    #[test]
    fn brain_config_router_uses_preferred_provider_from_config() {
        use phantom_brain::router::{BackendKind, RouterConfig};

        let config = PhantomConfig::parse("preferred_provider = \"claude-fast\"").unwrap();
        assert_eq!(
            config.preferred_provider(),
            Some("claude-fast"),
            "config must expose the parsed preferred_provider"
        );

        // Replicate the construction logic from app.rs.
        let router_config = match config.preferred_provider() {
            Some(id) => RouterConfig::with_preferred_provider(id),
            None => RouterConfig::default(),
        };

        // The first non-heuristic backend must be the promoted Claude backend.
        let first_non_heuristic = router_config
            .backends
            .iter()
            .find(|b| b.name != "heuristic")
            .expect("must have at least one non-heuristic backend");

        assert!(
            matches!(first_non_heuristic.kind, BackendKind::Claude { .. }),
            "preferred_provider='claude-fast' must promote a Claude backend to front; got '{}'",
            first_non_heuristic.name
        );
    }

    /// When `preferred_provider` is absent, the router uses the default cascade
    /// order and still constructs without panicking.
    #[test]
    fn brain_config_router_is_none_without_preferred_provider() {
        use phantom_brain::router::RouterConfig;

        let config = PhantomConfig::parse("").unwrap();
        assert!(
            config.preferred_provider().is_none(),
            "empty config must have no preferred_provider"
        );

        // Replicate the construction logic from app.rs.
        let router_config = match config.preferred_provider() {
            Some(id) => RouterConfig::with_preferred_provider(id),
            None => RouterConfig::default(),
        };

        // Default cascade: first non-heuristic is Ollama (cost 0.0, before claude's 0.003).
        let first_non_heuristic = router_config
            .backends
            .iter()
            .find(|b| b.name != "heuristic")
            .expect("must have at least one non-heuristic backend");

        assert_eq!(
            first_non_heuristic.name, "ollama-phi3.5",
            "default cascade must keep ollama-phi3.5 before claude-sonnet"
        );
    }

    // ------------------------------------------------------------------
    // notification_sounds tests (issue #571)
    // ------------------------------------------------------------------

    #[test]
    fn default_notification_sounds_is_empty() {
        let config = PhantomConfig::default();
        assert!(
            config.notification_sounds.is_empty(),
            "default config must have no notification_sounds entries"
        );
        assert_eq!(
            config.notification_sound_for("info"),
            None,
            "missing key should return None (use default sound)"
        );
    }

    #[test]
    fn parse_notification_sounds_section() {
        let toml = "\
[notification_sounds]
info = \"/sounds/info.wav\"
warn = \"/sounds/warn.wav\"
danger = \"/sounds/danger.mp3\"
";
        let config = PhantomConfig::parse(toml).unwrap();
        assert_eq!(
            config.notification_sound_for("info"),
            Some("/sounds/info.wav"),
        );
        assert_eq!(
            config.notification_sound_for("warn"),
            Some("/sounds/warn.wav"),
        );
        assert_eq!(
            config.notification_sound_for("danger"),
            Some("/sounds/danger.mp3"),
        );
    }

    #[test]
    fn parse_notification_sounds_empty_string_is_silent() {
        // An empty string in TOML means "silence this category".
        // The accessor returns None to signal "no sound".
        let toml = "\
[notification_sounds]
danger = \"\"
";
        let config = PhantomConfig::parse(toml).unwrap();
        assert_eq!(
            config.notification_sound_for("danger"),
            None,
            "empty string in TOML must map to None (silent)"
        );
        // Other categories not listed → also None (use default).
        assert_eq!(config.notification_sound_for("info"), None);
    }

    #[test]
    fn parse_notification_sounds_does_not_affect_other_fields() {
        let toml = "\
skip_boot = true

[notification_sounds]
info = \"/sounds/ding.wav\"
";
        let config = PhantomConfig::parse(toml).unwrap();
        assert!(config.skip_boot, "skip_boot before the section must parse");
        assert_eq!(
            config.notification_sound_for("info"),
            Some("/sounds/ding.wav"),
        );
    }

    // ------------------------------------------------------------------
    // mcp_servers tests
    // ------------------------------------------------------------------

    #[test]
    fn mcp_servers_empty_by_default() {
        let config = PhantomConfig::default();
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn parse_single_mcp_server() {
        let toml = "[mcp_servers.my-server]\nurl = \"ws://localhost:8765\"\n";
        let config = PhantomConfig::parse(toml).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "my-server");
        assert_eq!(config.mcp_servers[0].url, "ws://localhost:8765");
        assert!(config.mcp_servers[0].enabled);
    }

    #[test]
    fn parse_mcp_server_disabled() {
        let toml = "[mcp_servers.dev]\nurl = \"ws://localhost:9000\"\nenabled = false\n";
        let config = PhantomConfig::parse(toml).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert!(!config.mcp_servers[0].enabled);
    }

    #[test]
    fn parse_multiple_mcp_servers() {
        let toml = "[mcp_servers.alpha]\nurl = \"ws://alpha:1\"\n\n[mcp_servers.beta]\nurl = \"ws://beta:2\"\n";
        let config = PhantomConfig::parse(toml).unwrap();
        assert_eq!(config.mcp_servers.len(), 2);
        assert_eq!(config.mcp_servers[0].name, "alpha");
        assert_eq!(config.mcp_servers[1].name, "beta");
    }

    #[test]
    fn parse_mcp_server_mixed_with_top_level_keys() {
        let toml = "theme = \"amber\"\n\n[mcp_servers.srv]\nurl = \"ws://srv:3\"\n";
        let config = PhantomConfig::parse(toml).unwrap();
        assert_eq!(config.theme_name, "amber");
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].url, "ws://srv:3");
    }

    #[test]
    fn parse_mcp_server_no_url_defaults_empty() {
        let toml = "[mcp_servers.empty]\n";
        let config = PhantomConfig::parse(toml).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].url, "");
    }
}
