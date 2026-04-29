//! Settings panel UI — overlay for viewing and editing Phantom settings.
//!
//! A full-screen overlay toggled with Ctrl+, (comma). Arrow keys navigate
//! sections and items; left/right adjusts values. Changes are applied live
//! and can be persisted to the config file on Escape (auto-save).

/// Settings panel state.
pub(crate) struct SettingsPanel {
    pub open: bool,
    pub selected_section: usize,
    pub selected_item: usize,
    pub sections: Vec<SettingsSection>,
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
}

const THEME_OPTIONS: &[&str] = &["phosphor", "amber", "ice", "blood", "vapor", "pipboy"];

impl SettingsPanel {
    pub fn new() -> Self {
        let defaults = CurrentValues {
            theme_name: "phosphor".into(),
            font_size: 14.0,
            scanline_intensity: 0.18,
            bloom_intensity: 0.25,
            chromatic_aberration: 0.04,
            curvature: 0.06,
            vignette_intensity: 0.20,
            noise_intensity: 0.02,
        };
        Self {
            open: false,
            selected_section: 0,
            selected_item: 0,
            sections: Self::build_sections(&defaults),
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// Reload the section list from live app values.
    #[allow(dead_code)]
    pub fn load_from(&mut self, values: &CurrentValues) {
        self.sections = Self::build_sections(values);
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
        ]
    }

    /// Navigate to next item (wraps within current section).
    pub fn next_item(&mut self) {
        if let Some(section) = self.sections.get(self.selected_section) {
            if !section.items.is_empty() {
                self.selected_item = (self.selected_item + 1) % section.items.len();
            }
        }
    }

    /// Navigate to previous item.
    pub fn prev_item(&mut self) {
        if let Some(section) = self.sections.get(self.selected_section) {
            if !section.items.is_empty() {
                self.selected_item =
                    (self.selected_item + section.items.len() - 1) % section.items.len();
            }
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
        }
    }

    /// Export current UI values to a snapshot.
    pub fn to_snapshot(&self) -> SettingsSnapshot {
        let mut snap = SettingsSnapshot {
            theme_name: "phosphor".into(),
            font_size: 14.0,
            scanline_intensity: 0.18,
            bloom_intensity: 0.25,
            chromatic_aberration: 0.04,
            curvature: 0.06,
            vignette_intensity: 0.20,
            noise_intensity: 0.02,
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
        }
    }

    /// Build a normalized bar (0.0..1.0) for a float setting.
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
            _ => None,
        }
    }
}
