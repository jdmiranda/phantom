//! Debug draw manager — queued primitives for visual debugging.
//!
//! Any code can push debug primitives (lines, rects, text). The renderer
//! drains the queue at end-of-frame. Primitives have a lifetime that decays
//! each frame; expired ones are removed automatically.

/// 2D vector for screen-space operations.
#[derive(Debug, Clone, Copy)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// Debug draw primitive types.
#[derive(Debug, Clone)]
pub enum DebugPrimitive {
    /// Screen-space line between two points.
    Line { from: Vec2, to: Vec2 },
    /// Screen-space axis-aligned rectangle (outline).
    Rect { min: Vec2, max: Vec2 },
    /// Screen-space filled rectangle.
    FilledRect { min: Vec2, max: Vec2 },
    /// Screen-space crosshair marker.
    Cross { at: Vec2, size: f32 },
    /// Screen-space text label.
    Text { at: Vec2, text: String },
    /// Screen-space circle (outline).
    Circle { center: Vec2, radius: f32 },
}

/// Drawing options for a debug primitive.
#[derive(Debug, Clone)]
pub struct DrawOptions {
    pub color: [f32; 4],
    pub line_width: f32,
}

impl Default for DrawOptions {
    fn default() -> Self {
        Self {
            color: [0.0, 1.0, 0.0, 1.0], // green
            line_width: 1.0,
        }
    }
}

impl DrawOptions {
    pub fn color(mut self, color: [f32; 4]) -> Self {
        self.color = color;
        self
    }

    pub fn line_width(mut self, width: f32) -> Self {
        self.line_width = width;
        self
    }

    pub fn red() -> Self {
        Self {
            color: [1.0, 0.0, 0.0, 1.0],
            ..Default::default()
        }
    }

    pub fn green() -> Self {
        Self::default()
    }

    pub fn blue() -> Self {
        Self {
            color: [0.0, 0.0, 1.0, 1.0],
            ..Default::default()
        }
    }

    pub fn yellow() -> Self {
        Self {
            color: [1.0, 1.0, 0.0, 1.0],
            ..Default::default()
        }
    }

    pub fn white() -> Self {
        Self {
            color: [1.0, 1.0, 1.0, 1.0],
            ..Default::default()
        }
    }

    pub fn cyan() -> Self {
        Self {
            color: [0.0, 1.0, 1.0, 1.0],
            ..Default::default()
        }
    }
}

struct QueuedPrimitive {
    primitive: DebugPrimitive,
    options: DrawOptions,
    remaining_lifetime: f32, // seconds; 0.0 = single frame
}

const MAX_DEBUG_PRIMITIVES: usize = 4096;

/// Manager that collects debug draw commands and produces renderable output.
pub struct DebugDrawManager {
    queue: Vec<QueuedPrimitive>,
    enabled: bool,
}

impl DebugDrawManager {
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            enabled: false,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Add a screen-space line.
    pub fn add_line(&mut self, from: Vec2, to: Vec2, opts: DrawOptions, lifetime: f32) {
        if !self.enabled {
            return;
        }
        self.push(DebugPrimitive::Line { from, to }, opts, lifetime);
    }

    /// Add a screen-space axis-aligned rectangle (outline).
    pub fn add_rect(&mut self, min: Vec2, max: Vec2, opts: DrawOptions, lifetime: f32) {
        if !self.enabled {
            return;
        }
        self.push(DebugPrimitive::Rect { min, max }, opts, lifetime);
    }

    /// Add a screen-space filled rectangle.
    pub fn add_filled_rect(&mut self, min: Vec2, max: Vec2, opts: DrawOptions, lifetime: f32) {
        if !self.enabled {
            return;
        }
        self.push(DebugPrimitive::FilledRect { min, max }, opts, lifetime);
    }

    /// Add a screen-space crosshair marker.
    pub fn add_cross(&mut self, at: Vec2, size: f32, opts: DrawOptions, lifetime: f32) {
        if !self.enabled {
            return;
        }
        self.push(DebugPrimitive::Cross { at, size }, opts, lifetime);
    }

    /// Add a screen-space text label.
    pub fn add_text(&mut self, at: Vec2, text: &str, opts: DrawOptions, lifetime: f32) {
        if !self.enabled {
            return;
        }
        self.push(
            DebugPrimitive::Text {
                at,
                text: text.to_string(),
            },
            opts,
            lifetime,
        );
    }

    /// Add a screen-space circle (outline).
    pub fn add_circle(&mut self, center: Vec2, radius: f32, opts: DrawOptions, lifetime: f32) {
        if !self.enabled {
            return;
        }
        self.push(DebugPrimitive::Circle { center, radius }, opts, lifetime);
    }

    /// Decay lifetimes and remove expired primitives. Returns the primitives
    /// to render this frame (including ones expiring this frame).
    pub fn flush(&mut self, dt: f32) -> Vec<(DebugPrimitive, DrawOptions)> {
        if !self.enabled {
            self.queue.clear();
            return Vec::new();
        }

        // Drain the queue, emit all primitives, and re-insert survivors.
        let items: Vec<QueuedPrimitive> = self.queue.drain(..).collect();
        let mut output = Vec::with_capacity(items.len());
        for mut item in items {
            output.push((item.primitive.clone(), item.options.clone()));
            item.remaining_lifetime -= dt;
            if item.remaining_lifetime > 0.0 {
                self.queue.push(item);
            }
        }

        output
    }

    /// Remove all queued primitives.
    pub fn clear(&mut self) {
        self.queue.clear();
    }

    /// Number of primitives currently in the queue.
    pub fn primitive_count(&self) -> usize {
        self.queue.len()
    }

    fn push(&mut self, primitive: DebugPrimitive, options: DrawOptions, lifetime: f32) {
        if self.queue.len() >= MAX_DEBUG_PRIMITIVES {
            return;
        }
        self.queue.push(QueuedPrimitive {
            primitive,
            options,
            remaining_lifetime: if lifetime <= 0.0 {
                f32::EPSILON
            } else {
                lifetime
            },
        });
    }
}

impl Default for DebugDrawManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_manager_ignores_adds() {
        let mut mgr = DebugDrawManager::new(); // starts disabled
        mgr.add_line(
            Vec2::new(0.0, 0.0),
            Vec2::new(1.0, 1.0),
            DrawOptions::green(),
            1.0,
        );
        assert_eq!(mgr.primitive_count(), 0);
    }

    #[test]
    fn enabled_manager_queues_primitives() {
        let mut mgr = DebugDrawManager::new();
        mgr.set_enabled(true);
        mgr.add_line(
            Vec2::new(0.0, 0.0),
            Vec2::new(1.0, 1.0),
            DrawOptions::green(),
            1.0,
        );
        mgr.add_rect(
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 10.0),
            DrawOptions::red(),
            0.5,
        );
        assert_eq!(mgr.primitive_count(), 2);
    }

    #[test]
    fn flush_returns_all_and_decays() {
        let mut mgr = DebugDrawManager::new();
        mgr.set_enabled(true);
        mgr.add_line(
            Vec2::new(0.0, 0.0),
            Vec2::new(1.0, 1.0),
            DrawOptions::green(),
            0.5,
        );
        mgr.add_text(
            Vec2::new(5.0, 5.0),
            "debug",
            DrawOptions::white(),
            1.0,
        );

        let output = mgr.flush(0.3);
        assert_eq!(output.len(), 2);
        // After flush with 0.3s, the 0.5s line has 0.2s left, the 1.0s text has 0.7s
        assert_eq!(mgr.primitive_count(), 2);

        let output = mgr.flush(0.3);
        assert_eq!(output.len(), 2);
        // Now the line (0.2 - 0.3 = -0.1) is expired, text (0.7 - 0.3 = 0.4) remains
        assert_eq!(mgr.primitive_count(), 1);
    }

    #[test]
    fn single_frame_primitives_expire_immediately() {
        let mut mgr = DebugDrawManager::new();
        mgr.set_enabled(true);
        mgr.add_cross(Vec2::new(5.0, 5.0), 10.0, DrawOptions::yellow(), 0.0);

        let output = mgr.flush(0.016);
        assert_eq!(output.len(), 1);
        // Expired after one flush
        assert_eq!(mgr.primitive_count(), 0);
    }

    #[test]
    fn clear_empties_queue() {
        let mut mgr = DebugDrawManager::new();
        mgr.set_enabled(true);
        mgr.add_line(
            Vec2::new(0.0, 0.0),
            Vec2::new(1.0, 1.0),
            DrawOptions::green(),
            10.0,
        );
        mgr.clear();
        assert_eq!(mgr.primitive_count(), 0);
    }

    #[test]
    fn draw_options_builders() {
        let opts = DrawOptions::red().line_width(2.0);
        assert_eq!(opts.color, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(opts.line_width, 2.0);
    }

    #[test]
    fn disabled_flush_clears_queue() {
        let mut mgr = DebugDrawManager::new();
        mgr.set_enabled(true);
        mgr.add_line(
            Vec2::new(0.0, 0.0),
            Vec2::new(1.0, 1.0),
            DrawOptions::green(),
            10.0,
        );
        mgr.set_enabled(false);
        let output = mgr.flush(0.016);
        assert!(output.is_empty());
        assert_eq!(mgr.primitive_count(), 0);
    }
}
