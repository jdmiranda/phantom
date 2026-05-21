//! Settings panel UI — overlay for viewing and editing Phantom settings.
//!
//! A full-screen overlay toggled with Ctrl+, (comma). Arrow keys navigate
//! sections and items; left/right adjusts values. Changes are applied live
//! and can be persisted to the config file on Escape (auto-save).
//! Press `R` to revert all settings to their compiled-in defaults.

use crate::settings::{AgentSettings, PhantomSettings};

/// Settings panel state.
pub(crate) struct SettingsPanel {
    pub open: bool,
    pub selected_section: usize,
    pub selected_item: usize,
    pub sections: Vec<SettingsSection>,
    /// Non-empty when the most recent value change produced a validation error.
    pub validation_error: Option<String>,
}

pub(crate) struct SettingsSection {
    pub name: &'static str,
    pub items: Vec<SettingsItem>,
}

pub(crate) struct SettingsItem {
    pub label: &'static str,
    pub kind: SettingsKind,
}

pub(crate) enum SettingsKind {
    Choice {
        options: Vec<&'static str>,
        current: usize,
    },
    Float {
        min: f32,
        max: f32,
        step: f32,
        value: f32,
    },
    /// Editable text field. The stored value is opaque to the panel.
    Text { value: String },
    /// Integer slider displayed as a numeric value between `min` and `max`.
    IntSlider { min: u32, max: u32, value: u32 },
}

/// Snapshot of all settings values read from the panel UI.
pub(crate) struct SettingsSnapshot {
    pub theme_name: String,
    pub font_size: f32,
    pub scanline_intensity: f32,
    pub bloom_intensity: f32,
    pub chromatic_aberration: f32,
    pub curvature: f32,
    pub vignette_intensity: f32,
    pub noise_intensity: f32,
    // AI & Agent fields
    pub api_key_env_var: String,
    pub shell: String,
    pub agent_timeout_seconds: u32,
    pub max_concurrent_agents: u32,
}

/// Inputs for building the section list from current app state.
pub(crate) struct CurrentValues {
    pub theme_name: String,
    pub font_size: f32,
    pub scanline_intensity: f32,
    pub bloom_intensity: f32,
    pub chromatic_aberration: f32,
    pub curvature: f32,
    pub vignette_intensity: f32,
    pub noise_intensity: f32,
    // AI & Agent fields
    pub api_key_env_var: String,
    pub shell: String,
    pub agent_timeout_seconds: u32,
    pub max_concurrent_agents: u32,
}

impl CurrentValues {
    /// Build `CurrentValues` from application defaults (no live app state needed).
    pub fn from_defaults() -> Self {
        let settings = PhantomSettings::default();
        Self {
            theme_name: settings.theme.clone(),
            font_size: settings.font_size,
            scanline_intensity: settings.crt.scanline_intensity,
            bloom_intensity: settings.crt.bloom_intensity,
            chromatic_aberration: settings.crt.chromatic_aberration,
            curvature: settings.crt.curvature,
            vignette_intensity: settings.crt.vignette_intensity,
            noise_intensity: settings.crt.noise_intensity,
            api_key_env_var: settings.agents.api_key_env_var,
            shell: settings.agents.shell,
            agent_timeout_seconds: settings.agents.agent_timeout_seconds,
            max_concurrent_agents: settings.agents.max_concurrent_agents,
        }
    }
}

const THEME_OPTIONS: &[&str] = &["phosphor", "amber", "ice", "blood", "vapor", "pipboy"];

impl SettingsPanel {
    pub fn new() -> Self {
        let defaults = CurrentValues::from_defaults();
        Self {
            open: false,
            selected_section: 0,
            selected_item: 0,
            sections: Self::build_sections(&defaults),
            validation_error: None,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// Reload the section list from live app values.
    #[allow(dead_code)]
    pub fn load_from(&mut self, values: &CurrentValues) {
        self.sections = Self::build_sections(values);
        self.validation_error = None;
    }

    /// Reset all settings to compiled-in defaults and rebuild sections.
    pub fn revert_to_defaults(&mut self) {
        let defaults = CurrentValues::from_defaults();
        self.sections = Self::build_sections(&defaults);
        self.selected_section = 0;
        self.selected_item = 0;
        self.validation_error = None;
    }

    fn build_sections(v: &CurrentValues) -> Vec<SettingsSection> {
        let theme_idx = THEME_OPTIONS
            .iter()
            .position(|t| t.eq_ignore_ascii_case(&v.theme_name))
            .unwrap_or(0);

        vec![
            SettingsSection {
                name: "Theme",
                items: vec![
                    SettingsItem {
                        label: "Color Theme",
                        kind: SettingsKind::Choice {
                            options: THEME_OPTIONS.to_vec(),
                            current: theme_idx,
                        },
                    },
                    SettingsItem {
                        label: "Font Size",
                        kind: SettingsKind::Float {
                            min: 8.0,
                            max: 32.0,
                            step: 1.0,
                            value: v.font_size,
                        },
                    },
                ],
            },
            SettingsSection {
                name: "CRT Effects",
                items: vec![
                    SettingsItem {
                        label: "Scanlines",
                        kind: SettingsKind::Float {
                            min: 0.0,
                            max: 1.0,
                            step: 0.01,
                            value: v.scanline_intensity,
                        },
                    },
                    SettingsItem {
                        label: "Bloom",
                        kind: SettingsKind::Float {
                            min: 0.0,
                            max: 1.0,
                            step: 0.01,
                            value: v.bloom_intensity,
                        },
                    },
                    SettingsItem {
                        label: "Aberration",
                        kind: SettingsKind::Float {
                            min: 0.0,
                            max: 0.20,
                            step: 0.005,
                            value: v.chromatic_aberration,
                        },
                    },
                    SettingsItem {
                        label: "Curvature",
                        kind: SettingsKind::Float {
                            min: 0.0,
                            max: 0.5,
                            step: 0.01,
                            value: v.curvature,
                        },
                    },
                    SettingsItem {
                        label: "Vignette",
                        kind: SettingsKind::Float {
                            min: 0.0,
                            max: 1.0,
                            step: 0.01,
                            value: v.vignette_intensity,
                        },
                    },
                    SettingsItem {
                        label: "Noise",
                        kind: SettingsKind::Float {
                            min: 0.0,
                            max: 0.5,
                            step: 0.01,
                            value: v.noise_intensity,
                        },
                    },
                ],
            },
            SettingsSection {
                name: "AI & Agents",
                items: vec![
                    SettingsItem {
                        label: "API Key Env Var",
                        kind: SettingsKind::Text {
                            value: v.api_key_env_var.clone(),
                        },
                    },
                    SettingsItem {
                        label: "Shell",
                        kind: SettingsKind::Text {
                            value: v.shell.clone(),
                        },
                    },
                    SettingsItem {
                        label: "Agent Timeout",
                        kind: SettingsKind::IntSlider {
                            min: 10,
                            max: 300,
                            value: v.agent_timeout_seconds.clamp(10, 300),
                        },
                    },
                    SettingsItem {
                        label: "Max Agents",
                        kind: SettingsKind::IntSlider {
                            min: 1,
                            max: 10,
                            value: v.max_concurrent_agents.clamp(1, 10),
                        },
                    },
                ],
            },
        ]
    }

    /// Navigate to next item (wraps within current section).
    pub fn next_item(&mut self) {
        if let Some(section) = self.sections.get(self.selected_section)
            && !section.items.is_empty()
        {
            self.selected_item = (self.selected_item + 1) % section.items.len();
        }
    }

    /// Navigate to previous item.
    pub fn prev_item(&mut self) {
        if let Some(section) = self.sections.get(self.selected_section)
            && !section.items.is_empty()
        {
            self.selected_item =
                (self.selected_item + section.items.len() - 1) % section.items.len();
        }
    }

    /// Switch to next section.
    pub fn next_section(&mut self) {
        if !self.sections.is_empty() {
            self.selected_section = (self.selected_section + 1) % self.sections.len();
            self.selected_item = 0;
        }
    }

    /// Switch to previous section.
    #[allow(dead_code)]
    pub fn prev_section(&mut self) {
        if !self.sections.is_empty() {
            self.selected_section =
                (self.selected_section + self.sections.len() - 1) % self.sections.len();
            self.selected_item = 0;
        }
    }

    /// Adjust the selected item's value (positive = increase).
    ///
    /// For `Text` items left/right does nothing (editing is not yet implemented
    /// in this panel; the field is read-only from the keyboard at this stage).
    /// For `IntSlider` the value is stepped and then clamped to `[min, max]`.
    pub fn adjust(&mut self, delta: f32) {
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return;
        };
        let Some(item) = section.items.get_mut(self.selected_item) else {
            return;
        };
        match &mut item.kind {
            SettingsKind::Choice { options, current } => {
                if delta > 0.0 {
                    *current = (*current + 1) % options.len();
                } else {
                    *current = (*current + options.len() - 1) % options.len();
                }
            }
            SettingsKind::Float {
                min,
                max,
                step,
                value,
            } => {
                *value = (*value + *step * delta.signum()).clamp(*min, *max);
            }
            SettingsKind::IntSlider { min, max, value } => {
                if delta > 0.0 {
                    *value = (*value + 1).min(*max);
                } else {
                    *value = value.saturating_sub(1).max(*min);
                }
            }
            SettingsKind::Text { .. } => {
                // Text fields are not adjusted with left/right arrow keys.
            }
        }

        // Run per-field validation after the adjustment.
        self.validate_current_item();
    }

    /// Validate the currently selected item and update `validation_error`.
    fn validate_current_item(&mut self) {
        let Some(section) = self.sections.get(self.selected_section) else {
            return;
        };
        let Some(item) = section.items.get(self.selected_item) else {
            return;
        };

        let error = match (item.label, &item.kind) {
            ("Shell", SettingsKind::Text { value }) => {
                if !std::path::Path::new(value).exists() {
                    Some(format!("Shell path does not exist: {value}"))
                } else {
                    None
                }
            }
            ("Agent Timeout", SettingsKind::IntSlider { min, max, value }) => {
                if *value < *min || *value > *max {
                    Some(format!("Timeout must be {min}–{max} seconds (got {value})"))
                } else {
                    None
                }
            }
            _ => None,
        };

        self.validation_error = error;
    }

    /// Export current UI values to a snapshot.
    pub fn to_snapshot(&self) -> SettingsSnapshot {
        let defaults = PhantomSettings::default();
        let agent_defaults = AgentSettings::default();

        let mut snap = SettingsSnapshot {
            theme_name: defaults.theme.clone(),
            font_size: defaults.font_size,
            scanline_intensity: defaults.crt.scanline_intensity,
            bloom_intensity: defaults.crt.bloom_intensity,
            chromatic_aberration: defaults.crt.chromatic_aberration,
            curvature: defaults.crt.curvature,
            vignette_intensity: defaults.crt.vignette_intensity,
            noise_intensity: defaults.crt.noise_intensity,
            api_key_env_var: agent_defaults.api_key_env_var,
            shell: agent_defaults.shell,
            agent_timeout_seconds: agent_defaults.agent_timeout_seconds,
            max_concurrent_agents: agent_defaults.max_concurrent_agents,
        };

        for section in &self.sections {
            for item in &section.items {
                match (item.label, &item.kind) {
                    ("Color Theme", SettingsKind::Choice { options, current }) => {
                        snap.theme_name = options[*current].to_string();
                    }
                    ("Font Size", SettingsKind::Float { value, .. }) => {
                        snap.font_size = *value;
                    }
                    ("Scanlines", SettingsKind::Float { value, .. }) => {
                        snap.scanline_intensity = *value;
                    }
                    ("Bloom", SettingsKind::Float { value, .. }) => {
                        snap.bloom_intensity = *value;
                    }
                    ("Aberration", SettingsKind::Float { value, .. }) => {
                        snap.chromatic_aberration = *value;
                    }
                    ("Curvature", SettingsKind::Float { value, .. }) => {
                        snap.curvature = *value;
                    }
                    ("Vignette", SettingsKind::Float { value, .. }) => {
                        snap.vignette_intensity = *value;
                    }
                    ("Noise", SettingsKind::Float { value, .. }) => {
                        snap.noise_intensity = *value;
                    }
                    ("API Key Env Var", SettingsKind::Text { value }) => {
                        snap.api_key_env_var = value.clone();
                    }
                    ("Shell", SettingsKind::Text { value }) => {
                        snap.shell = value.clone();
                    }
                    ("Agent Timeout", SettingsKind::IntSlider { value, .. }) => {
                        snap.agent_timeout_seconds = (*value).clamp(10, 300);
                    }
                    ("Max Agents", SettingsKind::IntSlider { value, .. }) => {
                        snap.max_concurrent_agents = (*value).clamp(1, 10);
                    }
                    _ => {}
                }
            }
        }
        snap
    }

    /// Format the display value for an item.
    pub fn display_value(kind: &SettingsKind) -> String {
        match kind {
            SettingsKind::Choice { options, current } => options[*current].to_string(),
            SettingsKind::Float { value, step, .. } => {
                if *step >= 1.0 {
                    format!("{:.0}", value)
                } else if *step >= 0.01 {
                    format!("{:.2}", value)
                } else {
                    format!("{:.3}", value)
                }
            }
            SettingsKind::Text { value } => value.clone(),
            SettingsKind::IntSlider { value, .. } => format!("{}", value),
        }
    }

    /// Build a normalized bar (0.0..1.0) for float and int-slider settings.
    pub fn bar_fraction(kind: &SettingsKind) -> Option<f32> {
        match kind {
            SettingsKind::Float {
                min, max, value, ..
            } => {
                let range = max - min;
                if range > 0.0 {
                    Some((*value - *min) / range)
                } else {
                    Some(0.0)
                }
            }
            SettingsKind::IntSlider { min, max, value } => {
                let range = max - min;
                if range > 0 {
                    Some((*value - *min) as f32 / range as f32)
                } else {
                    Some(0.0)
                }
            }
            _ => None,
        }
    }
}
