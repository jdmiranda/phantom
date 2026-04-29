//! Configuration loading for Phantom.
//!
//! Reads `~/.config/phantom/config.toml` (or `$XDG_CONFIG_HOME/phantom/config.toml`)
//! and applies overrides to the default theme and shader parameters.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use log::{debug, info, warn};

use phantom_ui::themes::{self, Theme};

/// User-configurable settings loaded from TOML.
#[derive(Debug, Clone)]
pub struct PhantomConfig {
    /// Which built-in theme to use.
    pub theme_name: String,
    /// Font size in points.
    pub font_size: f32,
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
            shader_overrides: ShaderOverrides::default(),
            skip_boot: false,
            demo_mode: false,
            nlp_llm_enabled: true,
        }
    }
}

impl PhantomConfig {
    /// Load config from the standard path, or return defaults if not found.
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

        for line in toml_str.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"');

                match key {
                    "theme" => config.theme_name = value.to_string(),
                    "font_size" => {
                        if let Ok(v) = value.parse::<f32>() {
                            config.font_size = v;
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
                    _ => {
                        warn!("Unknown config key: {key}");
                    }
                }
            }
        }

        Ok(config)
    }

    /// Resolve the theme: load the named built-in, then apply shader overrides.
    pub fn resolve_theme(&self) -> Theme {
        let mut theme = themes::builtin_by_name(&self.theme_name)
            .unwrap_or_else(|| {
                warn!("Unknown theme '{}', falling back to phosphor", self.theme_name);
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
    pub fn nlp_llm_enabled(&self) -> bool {
        self.nlp_llm_enabled
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
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_skip_boot_is_false() {
        let config = PhantomConfig::default();
        assert!(!config.skip_boot, "cold-launch default must not skip boot");
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
        assert!(!config.skip_boot);
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
}
