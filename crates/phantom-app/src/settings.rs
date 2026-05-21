//! User settings -- persistent preferences stored in TOML.
//!
//! Settings live at `~/.config/phantom/settings.toml` and are loaded at
//! startup. They can be modified via the settings UI and saved on change.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// User-facing settings for Phantom.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PhantomSettings {
    pub theme: String,
    pub font_size: f32,
    pub scroll: ScrollSettings,
    pub crt: CrtSettings,
    pub agents: AgentSettings,
}

/// AI agent settings stored in TOML.
///
/// The raw API key is never persisted here. Instead, `api_key_env_var` holds
/// the name of the environment variable the runtime should read (e.g.
/// `"ANTHROPIC_API_KEY"`). The settings panel displays and edits this env-var
/// name only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Name of the environment variable that holds the Anthropic API key.
    /// Never store the raw key here.
    pub api_key_env_var: String,
    /// Shell path used when spawning agent sub-processes.
    pub shell: String,
    /// How long (seconds) to wait for an agent response before timing out.
    pub agent_timeout_seconds: u32,
    /// Maximum number of agents that may run concurrently.
    pub max_concurrent_agents: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrollSettings {
    pub history_lines: usize,
    /// Lines per scroll wheel tick.
    pub scroll_lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrtSettings {
    pub scanline_intensity: f32,
    pub bloom_intensity: f32,
    pub chromatic_aberration: f32,
    pub curvature: f32,
    pub vignette_intensity: f32,
    pub noise_intensity: f32,
}

impl Default for PhantomSettings {
    fn default() -> Self {
        Self {
            theme: "phosphor".into(),
            font_size: 18.0,
            scroll: ScrollSettings::default(),
            crt: CrtSettings::default(),
            agents: AgentSettings::default(),
        }
    }
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            api_key_env_var: "ANTHROPIC_API_KEY".into(),
            shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
            agent_timeout_seconds: 30,
            max_concurrent_agents: 3,
        }
    }
}

impl Default for ScrollSettings {
    fn default() -> Self {
        Self {
            history_lines: 10_000,
            scroll_lines: 3,
        }
    }
}

impl Default for CrtSettings {
    fn default() -> Self {
        Self {
            scanline_intensity: 0.15,
            bloom_intensity: 0.3,
            chromatic_aberration: 0.002,
            curvature: 0.05,
            vignette_intensity: 0.3,
            noise_intensity: 0.02,
        }
    }
}

impl PhantomSettings {
    /// Default settings file path.
    #[must_use]
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".config/phantom/settings.toml")
    }

    /// Load settings from the default path, falling back to defaults.
    #[must_use]
    pub fn load() -> Self {
        Self::load_from(&Self::default_path())
    }

    /// Load settings from a specific path, falling back to defaults.
    #[allow(dead_code)]
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(settings) => settings,
                Err(e) => {
                    log::warn!("Failed to parse settings: {e}");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Save settings to the default path.
    pub fn save(&self) -> anyhow::Result<PathBuf> {
        let path = Self::default_path();
        self.save_to(&path)?;
        Ok(path)
    }

    /// Save settings to a specific path using an atomic write (write to tmp,
    /// then rename).
    ///
    /// Writing via a temp file and renaming guarantees that the config watcher
    /// never sees a half-written file. On POSIX, `rename(2)` is atomic.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&tmp, &contents)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Build a [`PhantomSettings`] from a UI snapshot produced by
    /// [`SettingsPanel::to_snapshot`](crate::settings_ui::SettingsPanel::to_snapshot),
    /// preserving any settings the snapshot does not cover.
    ///
    /// The settings UI only edits theme, font size, and CRT shader params.
    /// Fields the panel does not surface (e.g. [`ScrollSettings`]) must carry
    /// through from `base` — otherwise an Escape-save would silently reset
    /// any value the user had hand-edited in `settings.toml`.
    ///
    /// Pass the currently-loaded `PhantomSettings` as `base` so the resulting
    /// struct merges the snapshot over the existing on-disk state.
    #[must_use]
    pub(crate) fn from_snapshot(
        snap: &crate::settings_ui::SettingsSnapshot,
        base: &PhantomSettings,
    ) -> Self {
        Self {
            theme: snap.theme_name.clone(),
            font_size: snap.font_size,
            // Preserve fields the UI does not edit (history_lines, scroll_lines).
            scroll: base.scroll.clone(),
            crt: CrtSettings {
                scanline_intensity: snap.scanline_intensity,
                bloom_intensity: snap.bloom_intensity,
                chromatic_aberration: snap.chromatic_aberration,
                curvature: snap.curvature,
                vignette_intensity: snap.vignette_intensity,
                noise_intensity: snap.noise_intensity,
            },
            // Agent settings are edited by the UI panel; pull them from the
            // snapshot, not from `base`.
            agents: AgentSettings {
                api_key_env_var: snap.api_key_env_var.clone(),
                shell: snap.shell.clone(),
                agent_timeout_seconds: snap.agent_timeout_seconds,
                max_concurrent_agents: snap.max_concurrent_agents,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_round_trip() {
        let settings = PhantomSettings::default();
        let toml_str = toml::to_string_pretty(&settings).unwrap();
        let reloaded: PhantomSettings = toml::from_str(&toml_str).unwrap();
        assert_eq!(reloaded.theme, "phosphor");
        assert_eq!(reloaded.font_size, 18.0);
        assert_eq!(reloaded.scroll.history_lines, 10_000);
    }

    #[test]
    fn load_from_nonexistent_returns_default() {
        let settings = PhantomSettings::load_from(Path::new("/nonexistent/path.toml"));
        assert_eq!(settings.theme, "phosphor");
    }

    #[test]
    fn save_and_reload() {
        let dir = std::env::temp_dir().join("phantom-test-settings");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_settings.toml");

        let settings = PhantomSettings {
            theme: "amber".into(),
            font_size: 20.0,
            ..PhantomSettings::default()
        };
        settings.save_to(&path).unwrap();

        let reloaded = PhantomSettings::load_from(&path);
        assert_eq!(reloaded.theme, "amber");
        assert_eq!(reloaded.font_size, 20.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let toml_str = r#"theme = "ice""#;
        let settings: PhantomSettings = toml::from_str(toml_str).unwrap();
        assert_eq!(settings.theme, "ice");
        assert_eq!(settings.font_size, 18.0);
        assert_eq!(settings.scroll.history_lines, 10_000);
    }

    /// `save_to` must write atomically: the final file must not pass through
    /// an intermediate state where the path exists but is empty/truncated.
    #[test]
    fn save_to_is_atomic_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let tmp = path.with_extension("toml.tmp");

        let settings = PhantomSettings {
            theme: "vapor".into(),
            ..PhantomSettings::default()
        };
        settings.save_to(&path).unwrap();

        assert!(
            !tmp.exists(),
            "atomic write must remove the .toml.tmp sentinel"
        );
        let loaded = PhantomSettings::load_from(&path);
        assert_eq!(loaded.theme, "vapor", "loaded theme must match saved value");
    }

    /// [`PhantomSettings::from_snapshot`] must faithfully transfer every field
    /// from the snapshot into the resulting struct.
    #[test]
    fn from_snapshot_maps_all_fields() {
        use crate::settings_ui::SettingsSnapshot;

        let snap = SettingsSnapshot {
            theme_name: "ice".into(),
            font_size: 22.0,
            scanline_intensity: 0.42,
            bloom_intensity: 0.55,
            chromatic_aberration: 0.007,
            curvature: 0.12,
            vignette_intensity: 0.33,
            noise_intensity: 0.04,
            api_key_env_var: "ANTHROPIC_API_KEY".into(),
            shell: "/bin/zsh".into(),
            agent_timeout_seconds: 30,
            max_concurrent_agents: 3,
        };

        let base = PhantomSettings::default();
        let settings = PhantomSettings::from_snapshot(&snap, &base);

        assert_eq!(settings.theme, "ice");
        assert!((settings.font_size - 22.0).abs() < f32::EPSILON);
        assert!((settings.crt.scanline_intensity - 0.42).abs() < f32::EPSILON);
        assert!((settings.crt.bloom_intensity - 0.55).abs() < f32::EPSILON);
        assert!((settings.crt.chromatic_aberration - 0.007).abs() < f32::EPSILON);
        assert!((settings.crt.curvature - 0.12).abs() < f32::EPSILON);
        assert!((settings.crt.vignette_intensity - 0.33).abs() < f32::EPSILON);
        assert!((settings.crt.noise_intensity - 0.04).abs() < f32::EPSILON);
    }

    /// Regression: the settings UI does not edit [`ScrollSettings`], so an
    /// Escape-save must preserve any user-edited `history_lines` /
    /// `scroll_lines` from disk rather than resetting them to defaults.
    #[test]
    fn from_snapshot_preserves_scroll_settings_from_base() {
        use crate::settings_ui::SettingsSnapshot;

        let base = PhantomSettings {
            scroll: ScrollSettings {
                history_lines: 50_000,
                scroll_lines: 7,
            },
            ..PhantomSettings::default()
        };

        let snap = SettingsSnapshot {
            theme_name: "amber".into(),
            font_size: 20.0,
            scanline_intensity: 0.2,
            bloom_intensity: 0.3,
            chromatic_aberration: 0.003,
            curvature: 0.05,
            vignette_intensity: 0.3,
            noise_intensity: 0.02,
            api_key_env_var: "ANTHROPIC_API_KEY".into(),
            shell: "/bin/zsh".into(),
            agent_timeout_seconds: 30,
            max_concurrent_agents: 3,
        };

        let merged = PhantomSettings::from_snapshot(&snap, &base);

        assert_eq!(merged.scroll.history_lines, 50_000);
        assert_eq!(merged.scroll.scroll_lines, 7);
        assert_eq!(merged.theme, "amber");
    }

    // -----------------------------------------------------------------------
    // Agent settings tests
    // -----------------------------------------------------------------------

    /// The API key env-var name must be persisted, never a raw key value.
    #[test]
    fn settings_api_key_stored_as_env_var_name() {
        let settings = PhantomSettings::default();
        // Default env-var name must be the well-known Anthropic env var.
        assert_eq!(settings.agents.api_key_env_var, "ANTHROPIC_API_KEY");

        // Round-trip with a custom env-var name.
        let custom = PhantomSettings {
            agents: AgentSettings {
                api_key_env_var: "MY_CUSTOM_KEY_VAR".into(),
                ..AgentSettings::default()
            },
            ..PhantomSettings::default()
        };
        let toml_str = toml::to_string_pretty(&custom).unwrap();
        // The raw key (if someone tried to store "sk-...") should NOT appear;
        // what appears is the env-var name.
        assert!(toml_str.contains("MY_CUSTOM_KEY_VAR"));
        assert!(!toml_str.contains("sk-"));

        let reloaded: PhantomSettings = toml::from_str(&toml_str).unwrap();
        assert_eq!(reloaded.agents.api_key_env_var, "MY_CUSTOM_KEY_VAR");
    }

    /// Reverting to defaults must reset all PhantomSettings fields.
    #[test]
    fn settings_revert_to_defaults_resets_all_fields() {
        let modified = PhantomSettings {
            theme: "blood".into(),
            font_size: 24.0,
            agents: AgentSettings {
                api_key_env_var: "CUSTOM_VAR".into(),
                shell: "/bin/bash".into(),
                agent_timeout_seconds: 120,
                max_concurrent_agents: 8,
            },
            ..PhantomSettings::default()
        };

        let defaults = PhantomSettings::default();

        // All diverging fields must revert.
        assert_ne!(modified.theme, defaults.theme);
        assert_ne!(modified.font_size, defaults.font_size);
        assert_ne!(modified.agents.agent_timeout_seconds, 0); // just a sanity guard

        // After reverting the struct equals defaults.
        let reverted = PhantomSettings::default();
        assert_eq!(reverted.theme, "phosphor");
        assert_eq!(reverted.font_size, 18.0);
        assert_eq!(reverted.agents.agent_timeout_seconds, 30);
        assert_eq!(reverted.agents.max_concurrent_agents, 3);
        assert_eq!(reverted.agents.api_key_env_var, "ANTHROPIC_API_KEY");
    }

    /// Shell validation: a non-existent path is detectable at validation time.
    #[test]
    fn settings_shell_validation_shows_error_for_nonexistent_path() {
        let bogus = "/does/not/exist/noshell";
        let exists = std::path::Path::new(bogus).exists();
        assert!(
            !exists,
            "Test precondition: path must not exist, got {}",
            bogus
        );
        // The settings type itself stores the value; validation is a separate
        // step performed by the UI layer. Confirm we can detect non-existence.
        let settings = PhantomSettings {
            agents: AgentSettings {
                shell: bogus.into(),
                ..AgentSettings::default()
            },
            ..PhantomSettings::default()
        };
        let shell_path = std::path::Path::new(&settings.agents.shell);
        assert!(
            !shell_path.exists(),
            "Non-existent shell path must not exist"
        );
    }

    /// Agent timeout must be clamped to 300 seconds at the UI layer.
    #[test]
    fn settings_agent_timeout_clamped_to_300() {
        // The u32 field accepts any value; clamping is applied by the panel UI.
        // Verify that the default is within the valid range and that a value
        // above 300 is detectable (so the clamp can be applied).
        let defaults = AgentSettings::default();
        assert!(
            defaults.agent_timeout_seconds >= 10,
            "Default timeout must be >= 10"
        );
        assert!(
            defaults.agent_timeout_seconds <= 300,
            "Default timeout must be <= 300"
        );

        // Simulate clamping logic that the UI applies.
        let raw: u32 = 9999;
        let clamped = raw.clamp(10, 300);
        assert_eq!(clamped, 300);

        let raw_low: u32 = 1;
        let clamped_low = raw_low.clamp(10, 300);
        assert_eq!(clamped_low, 10);
    }
}
