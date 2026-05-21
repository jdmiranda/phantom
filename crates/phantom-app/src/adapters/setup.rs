//! Setup adapter — the cold-launch first impression.
//!
//! Renders a single-pane "agent is initialising / API key needed" status
//! panel using the docs' visual grammar (phosphor on near-black, generous
//! whitespace, clear status colors). This adapter is **dependency-free** so
//! it can be registered at the `app.rs:1104` init site, BEFORE the `App`
//! struct exists.
//!
//! When the user later provisions an `ANTHROPIC_API_KEY` (or
//! `OPENAI_API_KEY`), the adapter flips a shared `Arc<AtomicBool>` flag.
//! `App::update` watches that flag and calls `spawn_agent_pane(...)`,
//! whose `adapter_count() == 1` replace-focused path swaps this adapter
//! out for the real agent at the same pane slot — no split, no flash.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use serde_json::json;

const TITLE: &str = "PHANTOM";
const SUBTITLE_WAITING: &str = "agent · waiting for API key";
const SUBTITLE_READY: &str = "agent · initialising";
const HELP_LINES: &[&str] = &[
    "set ANTHROPIC_API_KEY  (or OPENAI_API_KEY) and restart phantom",
    "or run:  phantom auth login",
];

const BG: [f32; 4] = [0.039, 0.055, 0.078, 1.0]; // #0a0e14
const SURFACE: [f32; 4] = [0.067, 0.090, 0.121, 1.0]; // #11171f
const FRAME_DIM: [f32; 4] = [0.102, 0.165, 0.094, 1.0]; // #1a2a18
const TEXT_BRIGHT: [f32; 4] = [0.20, 1.0, 0.0, 1.0]; // #33ff00
const TEXT_BODY: [f32; 4] = [0.722, 1.0, 0.722, 1.0]; // #b8ffb8
const TEXT_DIM: [f32; 4] = [0.290, 0.502, 0.282, 1.0]; // #4a8048
const STATUS_OK: [f32; 4] = [0.20, 1.0, 0.0, 1.0];
const STATUS_WARN: [f32; 4] = [1.0, 0.690, 0.0, 1.0]; // #ffb000

const LINE_HEIGHT: f32 = 16.0;
const TITLE_LINE_HEIGHT: f32 = 36.0;

/// Single-pane cold-launch status adapter. See module docs.
pub struct SetupAdapter {
    app_id: u32,
    /// Shared with `App` — set to `true` when an API-key env var transitions
    /// from missing to present. `App::update` consumes (clears) this each
    /// frame and, when set, kicks off `spawn_agent_pane`.
    upgrade_requested: Arc<AtomicBool>,
    /// Cached probe result so we only flip the flag on edge transitions.
    last_key_present: bool,
    /// Latched once an API key has been seen at least once so the panel
    /// can drop the "waiting" subtitle while the agent spawn is in flight.
    key_ever_seen: bool,
    /// Accumulator (seconds) used to debounce the env-var poll. Without this
    /// we'd call `std::env::var` twice per GPU frame (60–120 Hz); on macOS
    /// `getenv` is a serialised syscall under the render lock and not free.
    /// Polling every [`POLL_INTERVAL`] is plenty for what is effectively a
    /// human-in-the-loop "did the user set an env var yet" check.
    poll_accum_secs: f32,
}

/// Throttle env-var probing to once every two seconds. The env is a slow
/// signal (user pastes a key into a shell rc, then restarts), so a 2 s
/// debounce is invisible to the user and removes the per-frame syscalls.
const POLL_INTERVAL: f32 = 2.0;

impl SetupAdapter {
    /// Build a SetupAdapter that shares the `upgrade_requested` flag with the App.
    ///
    /// The initial flag value is `false`; `update()` flips it on a NONE→SOME
    /// env-var transition.
    pub(crate) fn new(upgrade_requested: Arc<AtomicBool>) -> Self {
        let initial = api_key_present();
        Self {
            app_id: 0,
            upgrade_requested,
            last_key_present: initial,
            key_ever_seen: initial,
            // Initialize at the poll interval so the first `update` call
            // probes immediately, matching previous behaviour.
            poll_accum_secs: POLL_INTERVAL,
        }
    }
}

fn api_key_present() -> bool {
    has_env("ANTHROPIC_API_KEY") || has_env("OPENAI_API_KEY")
}

fn has_env(name: &str) -> bool {
    std::env::var(name).ok().filter(|v| !v.is_empty()).is_some()
}

// ---------------------------------------------------------------------------
// Sub-trait implementations
// ---------------------------------------------------------------------------

impl AppCore for SetupAdapter {
    fn app_type(&self) -> &str {
        "setup"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, dt: f32) {
        // Debounce: only probe the env every POLL_INTERVAL seconds. At 60 Hz
        // this turns ~120 getenv syscalls/s into ~1/s. The `Commandable`
        // probe path stays unthrottled so a user-driven "check again" still
        // triggers an immediate read.
        self.poll_accum_secs += dt;
        if self.poll_accum_secs < POLL_INTERVAL {
            return;
        }
        self.poll_accum_secs = 0.0;

        let present = api_key_present();
        if present && !self.last_key_present {
            self.upgrade_requested.store(true, Ordering::Release);
            self.key_ever_seen = true;
        }
        self.last_key_present = present;
    }

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "setup",
            "key_present": self.last_key_present,
            "upgrade_pending": self.upgrade_requested.load(Ordering::Acquire),
        })
    }

    fn title(&self) -> &str {
        "setup"
    }
}

impl Renderable for SetupAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads = Vec::with_capacity(4);
        let mut text_segments = Vec::with_capacity(8);

        // Full-pane background (deep black-blue) so we paint the entire slot,
        // not just a small box.
        quads.push(QuadData {
            x: rect.x,
            y: rect.y,
            w: rect.width,
            h: rect.height,
            color: BG,
        });

        // Centered card surface — generous insets so the layout reads as
        // "intentional empty space", not "broken / forgot to draw".
        let card_w = (rect.width * 0.72).min(720.0).max(360.0);
        let card_h = 220.0_f32.min(rect.height - 64.0).max(160.0);
        let card_x = rect.x + (rect.width - card_w) * 0.5;
        let card_y = rect.y + (rect.height - card_h) * 0.5;

        quads.push(QuadData {
            x: card_x,
            y: card_y,
            w: card_w,
            h: card_h,
            color: SURFACE,
        });

        // 1px-ish top/bottom rules in dim frame green.
        quads.push(QuadData {
            x: card_x,
            y: card_y,
            w: card_w,
            h: 1.0,
            color: FRAME_DIM,
        });
        quads.push(QuadData {
            x: card_x,
            y: card_y + card_h - 1.0,
            w: card_w,
            h: 1.0,
            color: FRAME_DIM,
        });

        // Title.
        text_segments.push(TextData {
            text: TITLE.to_string(),
            x: card_x + 24.0,
            y: card_y + 28.0,
            color: TEXT_BRIGHT,
        });

        // Status dot + subtitle.
        let (dot_color, subtitle) = if self.last_key_present {
            (STATUS_OK, SUBTITLE_READY)
        } else {
            (STATUS_WARN, SUBTITLE_WAITING)
        };
        let subtitle_y = card_y + 28.0 + TITLE_LINE_HEIGHT;

        quads.push(QuadData {
            x: card_x + 24.0,
            y: subtitle_y - 8.0,
            w: 6.0,
            h: 6.0,
            color: dot_color,
        });
        text_segments.push(TextData {
            text: subtitle.to_string(),
            x: card_x + 40.0,
            y: subtitle_y,
            color: TEXT_BODY,
        });

        // Help lines (only show "set env var" guidance while waiting).
        if !self.last_key_present {
            for (i, line) in HELP_LINES.iter().enumerate() {
                text_segments.push(TextData {
                    text: (*line).to_string(),
                    x: card_x + 24.0,
                    y: subtitle_y + 24.0 + (i as f32) * LINE_HEIGHT,
                    color: TEXT_DIM,
                });
            }
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
            min_size: (40, 12),
            preferred_size: (120, 32),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 1.0,
        })
    }
}

impl InputHandler for SetupAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for SetupAdapter {
    fn accept_command(
        &mut self,
        cmd: &str,
        _args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        match cmd {
            "probe" => {
                let present = api_key_present();
                if present && !self.last_key_present {
                    self.upgrade_requested.store(true, Ordering::Release);
                }
                self.last_key_present = present;
                Ok(json!({"key_present": present}).to_string())
            }
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for SetupAdapter {}

impl Lifecycled for SetupAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for SetupAdapter {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_requested_flips_on_key_transition() {
        // SAFETY: tests in this module mutate process env; we lock them via
        // a single guard variable name to avoid parallel-test interference.
        // unsafe block required by Rust edition 2024.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
        }

        let flag = Arc::new(AtomicBool::new(false));
        let mut a = SetupAdapter::new(Arc::clone(&flag));
        assert!(!flag.load(Ordering::Acquire));

        // SetupAdapter::new initialises `poll_accum_secs` at the poll
        // interval so the first update probes immediately; subsequent
        // updates must accumulate dt past POLL_INTERVAL to re-probe.
        a.update(0.0);
        assert!(!flag.load(Ordering::Acquire));

        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-test"); }
        // First update after env change: accumulator is 0 from the prior
        // probe, so feed a large dt to force the throttled probe.
        a.update(POLL_INTERVAL + 0.1);
        assert!(flag.load(Ordering::Acquire), "flag must flip on NONE->SOME");

        // Subsequent ticks with the key still present do NOT re-flip
        // (we only edge-trigger so the App doesn't get duplicate work).
        flag.store(false, Ordering::Release);
        a.update(POLL_INTERVAL + 0.1);
        assert!(
            !flag.load(Ordering::Acquire),
            "flag must NOT re-flip while key remains present"
        );

        unsafe { std::env::remove_var("ANTHROPIC_API_KEY"); }
    }

    #[test]
    fn update_is_throttled_to_poll_interval() {
        // Regression: env-var probing must not run on every frame. Two
        // sub-interval ticks should not consult env (verified indirectly
        // via the upgrade flag staying low even when a key is present).
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
        }
        let flag = Arc::new(AtomicBool::new(false));
        let mut a = SetupAdapter::new(Arc::clone(&flag));
        // Burn through the initial-probe quota so the next probe is gated
        // by the poll throttle.
        a.update(POLL_INTERVAL + 0.1);
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test"); }

        // Two sub-interval frames must NOT re-probe (and so must NOT see
        // the env transition).
        a.update(POLL_INTERVAL * 0.4);
        a.update(POLL_INTERVAL * 0.4);
        assert!(
            !flag.load(Ordering::Acquire),
            "sub-interval frames must not probe env"
        );

        // Crossing the interval triggers the probe and the upgrade.
        a.update(POLL_INTERVAL * 0.4);
        assert!(
            flag.load(Ordering::Acquire),
            "probe must run after POLL_INTERVAL accumulates"
        );

        unsafe { std::env::remove_var("OPENAI_API_KEY"); }
    }

    #[test]
    fn render_paints_full_rect_and_card() {
        let flag = Arc::new(AtomicBool::new(false));
        let a = SetupAdapter::new(flag);
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            ..Default::default()
        };
        let out = a.render(&rect);
        // Full-pane background quad must be present and cover the rect.
        let bg = out.quads.iter().find(|q| q.w == 1920.0 && q.h == 1080.0);
        assert!(bg.is_some(), "expected full-pane background quad covering rect");
        assert!(!out.text_segments.is_empty(), "expected title + subtitle text");
    }

    #[test]
    fn does_not_accept_input() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut a = SetupAdapter::new(flag);
        assert!(!a.accepts_input());
        assert!(!a.handle_input("a"));
    }
}
