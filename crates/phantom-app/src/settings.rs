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
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".config/phantom/settings.toml")
    }

    /// Load settings from the default path, falling back to defaults.
    pub fn load() -> Self {
        Self::load_from(&Self::default_path())
    }

    /// Load settings from a specific path, falling back to defaults.
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

    /// Save settings to a specific path.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(path, contents)?;
        Ok(())
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

        let mut settings = PhantomSettings::default();
        settings.theme = "amber".into();
        settings.font_size = 20.0;
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
}
