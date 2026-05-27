//! Database adapter — schema + last-query browser.
//!
//! Renders the column metadata for a single table and the most recent
//! query text. Designed to be backed by phantom-bundle-store (SQLite) or
//! any other tabular source the App connects.

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
            .with_tokens(t);
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
            text_segments.push(TextData {
                text: "  (no schema loaded)".to_string(),
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
