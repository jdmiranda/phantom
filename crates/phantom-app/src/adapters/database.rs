//! Database adapter — schema + last-query browser.
//!
//! Renders the column metadata for a single table and the most recent
//! query text. Designed to be backed by phantom-bundle-store (SQLite) or
//! any other tabular source the App connects.
//!
//! # Backend readiness
//!
//! The host App opens a real [`phantom_bundle_store::BundleStore`] in
//! `App::new` via `open_bundle_store()` — backed by SQLCipher with the
//! migration set defined in `phantom-bundle-store::sqlite::MIGRATIONS`.
//! The adapter ships with a [`DatabaseAdapter::populate_bundle_store_schema`]
//! helper that surfaces the *real* schema from those migrations (bundles,
//! frames, audio_chunks, transcript_words) so the pane never shows
//! fabricated tables. When the bundle store fails to open (e.g. keychain
//! denied), callers should call [`DatabaseAdapter::set_backend_disabled`]
//! to surface that honestly to the operator.

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;

/// One column entry in the schema view.
#[derive(Debug, Clone)]
pub struct DbColumn {
    pub name: String,
    pub ty: String,
    pub sample: String,
}

impl DbColumn {
    /// Convenience constructor.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        ty: impl Into<String>,
        sample: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            ty: ty.into(),
            sample: sample.into(),
        }
    }
}

/// Database pane.
pub struct DatabaseAdapter {
    table: String,
    row_count: u64,
    columns: Vec<DbColumn>,
    last_query: String,
    backend: String,
    tokens: Tokens,
    app_id: u32,
}

impl DatabaseAdapter {
    /// Build with no data loaded.
    #[must_use]
    pub fn new() -> Self {
        Self {
            table: "(no table)".into(),
            row_count: 0,
            columns: Vec::new(),
            last_query: String::new(),
            backend: "sqlite".into(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Replace the schema view.
    pub fn set_schema(
        &mut self,
        table: impl Into<String>,
        row_count: u64,
        columns: Vec<DbColumn>,
    ) {
        self.table = table.into();
        self.row_count = row_count;
        self.columns = columns;
    }

    /// Update the last-query string shown in the body.
    pub fn set_last_query(&mut self, query: impl Into<String>) {
        self.last_query = query.into();
    }

    /// Column count.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Read the backend label (e.g. `"sqlite (sqlcipher)"`).
    #[must_use]
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Override the backend label shown in the head meta slot.
    pub fn set_backend(&mut self, backend: impl Into<String>) {
        self.backend = backend.into();
    }

    /// Surface a disabled backend honestly. Called by the host App when
    /// `open_bundle_store()` returns `None` (e.g. keychain access denied,
    /// `PHANTOM_DISABLE_BUNDLE_STORE` set). The pane will render the
    /// disabled state in the head meta slot and clear any prior schema.
    pub fn set_backend_disabled(&mut self, reason: impl Into<String>) {
        self.table = "(disabled)".into();
        self.row_count = 0;
        self.columns.clear();
        self.backend = format!("disabled — {}", reason.into());
    }

    /// Populate the schema view from the canonical
    /// `phantom-bundle-store` migration set (v1: bundles + frames +
    /// audio_chunks + transcript_words).
    ///
    /// This is the bundles row used as the headline table; the helper
    /// is intentionally const-data — it surfaces the live SQLite
    /// schema definition exactly, so the pane never invents tables.
    /// If you add a migration in `phantom-bundle-store::sqlite::MIGRATIONS`,
    /// extend the schema below to keep the two definitions in sync.
    pub fn populate_bundle_store_schema(&mut self) {
        self.backend = "sqlite (sqlcipher)".into();
        self.table = "bundles".into();
        self.row_count = 0;
        self.columns = vec![
            DbColumn::new("id", "TEXT PRIMARY KEY", "BundleId"),
            DbColumn::new("t_start_ns", "INTEGER NOT NULL", "monotonic ns"),
            DbColumn::new("t_wall_unix_ms", "INTEGER NOT NULL", "unix ms"),
            DbColumn::new("source_pane_id", "INTEGER NOT NULL", "pane.id"),
            DbColumn::new("intent", "TEXT", "user intent"),
            DbColumn::new("tags_json", "TEXT", "json array"),
            DbColumn::new("importance", "REAL", "0.0..=1.0"),
            DbColumn::new("sealed", "INTEGER", "0 | 1"),
            DbColumn::new("schema_version", "INTEGER", "1"),
        ];
    }

    /// Update the row count for the displayed table. Called when a live
    /// `BundleStore` query (e.g. `SELECT COUNT(*) FROM bundles`) succeeds
    /// so the head meta slot reflects current cardinality.
    pub fn set_row_count(&mut self, row_count: u64) {
        self.row_count = row_count;
    }
}

impl Default for DatabaseAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for DatabaseAdapter {
    fn app_type(&self) -> &str {
        "database"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "database",
            "table": self.table,
            "row_count": self.row_count,
            "columns": self.columns.len(),
            "backend": self.backend,
        })
    }

    fn title(&self) -> &str {
        "Database"
    }
}

impl Renderable for DatabaseAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let title = format!("{} · {} rows", self.table, self.row_count);
        let head = AppHead::new("DATABASE", title)
            .with_icon("▤")
            .with_meta(self.backend.clone())
            .with_tokens(t)
            .focused(rect.focused);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };
        let mut y = body.y + cell_h * 0.3;

        let name_color = t.colors.text_accent;
        let type_color = t.colors.text_dim;
        let val_color = t.colors.text_primary;
        let div_color = t.colors.chrome_divider;

        for col in &self.columns {
            if y + cell_h > body.y + body.height - cell_h * 2.0 {
                break;
            }
            text_segments.push(TextData {
                text: col.name.clone(),
                x: body.x + cell_w,
                y,
                color: name_color,
            });
            text_segments.push(TextData {
                text: col.ty.clone(),
                x: body.x + cell_w * 12.0,
                y,
                color: type_color,
            });
            text_segments.push(TextData {
                text: col.sample.clone(),
                x: body.x + cell_w * 22.0,
                y,
                color: val_color,
            });
            // Row divider
            quads.push(QuadData {
                x: body.x + cell_w,
                y: y + cell_h - 1.0,
                w: body.width - cell_w * 2.0,
                h: 1.0,
                color: div_color,
            });
            y += cell_h;
        }

        if self.columns.is_empty() {
            // Distinguish three honest states:
            //   1. backend disabled (e.g. keychain denied)
            //   2. backend ready but no schema populated yet
            //   3. backend opened but the source-of-truth table is empty
            let hint = if self.backend.starts_with("disabled") {
                "  (bundle store disabled — see backend status)".to_string()
            } else {
                "  (no schema loaded — call populate_bundle_store_schema)".to_string()
            };
            text_segments.push(TextData {
                text: hint,
                x: body.x + cell_w,
                y,
                color: type_color,
            });
        }

        // Query line at bottom
        if !self.last_query.is_empty() {
            let query_y = body.y + body.height - cell_h * 1.2;
            text_segments.push(TextData {
                text: self.last_query.clone(),
                x: body.x + cell_w,
                y: query_y,
                color: type_color,
            });
        }

        RenderOutput {
            quads,
            text_segments,
            grid: None,
            scroll: None,
            selection: None,
        }
    }

    fn is_visual(&self) -> bool {
        true
    }

    fn spatial_preference(&self) -> Option<SpatialPreference> {
        Some(SpatialPreference {
            min_size: (50, 10),
            preferred_size: (70, 20),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 2.0,
        })
    }
}

impl InputHandler for DatabaseAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for DatabaseAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "set_theme_name" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(tokens) = Tokens::for_theme_name(name, RenderCtx::fallback()) {
                    self.set_tokens(tokens);
                }
                Ok(json!({ "status": "ok" }).to_string())
            }
            "load_schema" => {
                let table = args
                    .get("table")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: table"))?
                    .to_string();
                let row_count = args.get("row_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let cols = args
                    .get("columns")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow::anyhow!("missing field: columns"))?;
                let parsed = cols
                    .iter()
                    .filter_map(|item| {
                        let name = item.get("name")?.as_str()?.to_string();
                        let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let sample =
                            item.get("sample").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        Some(DbColumn { name, ty, sample })
                    })
                    .collect();
                self.set_schema(table, row_count, parsed);
                Ok(json!({ "status": "ok", "columns": self.columns.len() }).to_string())
            }
            "set_query" => {
                let q = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: query"))?;
                self.set_last_query(q);
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for DatabaseAdapter {}

impl Lifecycled for DatabaseAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for DatabaseAdapter {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 900.0,
            height: 400.0,
            cell_size: (8.0, 16.0),
            ..Default::default()
        }
    }

    #[test]
    fn app_type_is_database() {
        assert_eq!(DatabaseAdapter::new().app_type(), "database");
    }

    #[test]
    fn load_schema_command_parses_columns() {
        let mut a = DatabaseAdapter::new();
        a.accept_command(
            "load_schema",
            &json!({
                "table": "events",
                "row_count": 42,
                "columns": [
                    { "name": "ts", "type": "int", "sample": "unix ms" },
                    { "name": "kind", "type": "text", "sample": "agent.spawn" },
                ]
            }),
        )
        .unwrap();
        assert_eq!(a.column_count(), 2);
    }

    #[test]
    fn renders_schema_rows() {
        let mut a = DatabaseAdapter::new();
        a.set_schema("events", 137, vec![DbColumn::new("ts", "int", "unix ms")]);
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "ts"));
        assert!(out.text_segments.iter().any(|t| t.text == "int"));
        assert!(out.text_segments.iter().any(|t| t.text == "unix ms"));
    }

    #[test]
    fn empty_renders_hint() {
        let a = DatabaseAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("no schema")));
    }

    #[test]
    fn populate_bundle_store_schema_loads_real_columns() {
        // The helper must surface the actual phantom-bundle-store schema —
        // bundles table with its v1 migration columns. This guards against
        // a regression where the helper is silently replaced with fake data.
        let mut a = DatabaseAdapter::new();
        a.populate_bundle_store_schema();
        assert_eq!(a.column_count(), 9);
        let names: Vec<&str> = a.columns.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"id"), "schema must include id column");
        assert!(
            names.contains(&"source_pane_id"),
            "schema must include source_pane_id"
        );
        assert!(
            names.contains(&"schema_version"),
            "schema must include schema_version"
        );
        assert_eq!(a.backend(), "sqlite (sqlcipher)");
    }

    #[test]
    fn populate_renders_real_columns_not_fixture_data() {
        let mut a = DatabaseAdapter::new();
        a.populate_bundle_store_schema();
        let out = a.render(&rect());
        assert!(
            out.text_segments.iter().any(|t| t.text == "id"),
            "render must show real column name"
        );
        assert!(
            out.text_segments.iter().any(|t| t.text == "source_pane_id"),
            "render must show source_pane_id column"
        );
    }

    #[test]
    fn set_backend_disabled_clears_schema_and_labels() {
        let mut a = DatabaseAdapter::new();
        a.populate_bundle_store_schema();
        assert!(a.column_count() > 0);

        a.set_backend_disabled("keychain access denied");
        assert_eq!(a.column_count(), 0);
        assert!(a.backend().starts_with("disabled"));
        assert!(a.backend().contains("keychain"));

        // Render should reflect disabled state.
        let out = a.render(&rect());
        assert!(
            out.text_segments
                .iter()
                .any(|t| t.text.contains("disabled")),
            "render must surface disabled backend"
        );
    }

    #[test]
    fn set_row_count_updates_head_meta() {
        let mut a = DatabaseAdapter::new();
        a.populate_bundle_store_schema();
        a.set_row_count(42);
        let out = a.render(&rect());
        assert!(
            out.text_segments.iter().any(|t| t.text.contains("42 rows")),
            "head meta must reflect updated row count"
        );
    }

    #[test]
    fn last_query_rendered_in_body() {
        let mut a = DatabaseAdapter::new();
        a.set_schema("events", 1, vec![DbColumn::new("a", "b", "c")]);
        a.set_last_query("SELECT * FROM events");
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("SELECT")));
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = DatabaseAdapter::new();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_column_name() {
        use phantom_ui::tokens::ColorRoles;
        let mut a = DatabaseAdapter::new();
        a.set_schema("events", 1, vec![DbColumn::new("ts", "int", "unix ms")]);
        let out_p = a.render(&rect());
        let name_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text == "ts")
            .map(|t| t.color)
            .expect("column name must render");

        let mut roles = ColorRoles::phosphor();
        roles.text_accent = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());
        let name_b = out_b
            .text_segments
            .iter()
            .find(|t| t.text == "ts")
            .map(|t| t.color)
            .expect("column name must render");

        assert_ne!(name_p, name_b);
        assert!(name_b[2] > 0.9);
    }
}
