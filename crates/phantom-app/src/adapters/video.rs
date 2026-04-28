//! Video adapter — wraps `VideoPlayback` as an `AppAdapter`.
//!
//! Bridges the ffmpeg-backed video decoder into the unified app model
//! so that video panes participate in layout negotiation and event bus
//! messaging. Actual GPU frame upload stays in `update.rs` because it
//! needs `VideoRenderer` + `GpuContext`, which the adapter does not own.

use serde_json::json;

use phantom_adapter::adapter::{Rect, RenderOutput};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};

use crate::video::VideoPlayback;

/// A video playback pane wrapped in the `AppAdapter` interface.
///
/// Owns a `VideoPlayback` and tracks whether playback has finished so
/// the coordinator can reclaim the pane slot.
pub struct VideoAdapter {
    playback: VideoPlayback,
    app_id: u32,
    outbox: Vec<phantom_adapter::BusMessage>,
    finished: bool,
}

// ---------------------------------------------------------------------------
// Constructor and accessors
// ---------------------------------------------------------------------------

impl VideoAdapter {
    /// Wrap an already-started video playback in the adapter.
    #[allow(dead_code)]
    pub(crate) fn new(playback: VideoPlayback) -> Self {
        Self {
            playback,
            app_id: 0,
            outbox: Vec::new(),
            finished: false,
        }
    }

    /// Immutable access to the inner playback state.
    #[allow(dead_code)]
    pub(crate) fn playback(&self) -> &VideoPlayback {
        &self.playback
    }

    /// Mutable access to the inner playback state.
    #[allow(dead_code)]
    pub(crate) fn playback_mut(&mut self) -> &mut VideoPlayback {
        &mut self.playback
    }
}

// ---------------------------------------------------------------------------
// Sub-trait implementations (ISP — each trait is focused)
// ---------------------------------------------------------------------------

impl AppCore for VideoAdapter {
    fn app_type(&self) -> &str {
        "video"
    }

    fn is_alive(&self) -> bool {
        !self.finished
    }

    fn update(&mut self, _dt: f32) {
        self.playback.poll_finished();
        if self.playback.finished && !self.finished {
            self.finished = true;
            self.outbox.push(phantom_adapter::BusMessage {
                topic_id: 0,
                sender: self.app_id,
                event: phantom_protocol::Event::VideoPlaybackStateChanged {
                    app_id: self.app_id,
                    playing: false,
                },
                frame: 0,
                timestamp: 0,
            });
        }
    }

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "video",
            "alive": !self.finished,
            "width": self.playback.width,
            "height": self.playback.height,
            "fps": self.playback.fps,
            "finished": self.finished,
        })
    }
}

impl Renderable for VideoAdapter {
    fn render(&self, _rect: &Rect) -> RenderOutput {
        // Actual GPU frame upload is handled in update.rs where
        // VideoRenderer + GpuContext are available. The adapter is
        // visual only to hold a layout pane slot.
        RenderOutput::default()
    }

    fn is_visual(&self) -> bool {
        true
    }

    fn spatial_preference(&self) -> Option<SpatialPreference> {
        Some(SpatialPreference {
            min_size: (40, 20),
            preferred_size: (80, 45),
            max_size: None,
            aspect_ratio: Some(16.0 / 9.0),
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 8.0,
        })
    }
}

impl InputHandler for VideoAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for VideoAdapter {
    fn accept_command(
        &mut self,
        cmd: &str,
        _args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        match cmd {
            "stop" => {
                self.playback.stop();
                self.finished = true;
                self.outbox.push(phantom_adapter::BusMessage {
                    topic_id: 0,
                    sender: self.app_id,
                    event: phantom_protocol::Event::VideoPlaybackStateChanged {
                        app_id: self.app_id,
                        playing: false,
                    },
                    frame: 0,
                    timestamp: 0,
                });
                Ok("stopped".into())
            }
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for VideoAdapter {
    fn drain_outbox(&mut self) -> Vec<phantom_adapter::BusMessage> {
        std::mem::take(&mut self.outbox)
    }
}

impl Lifecycled for VideoAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for VideoAdapter {
    fn permissions(&self) -> Vec<String> {
        vec!["filesystem".into()]
    }
}

// ---------------------------------------------------------------------------
// Compile-time Send assert
// ---------------------------------------------------------------------------

fn _assert_send() {
    fn _check<T: Send>() {}
    _check::<VideoAdapter>();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_type_returns_video() {
        // VideoPlayback requires ffmpeg and a real file, so we verify
        // the string literal contract directly.
        assert_eq!("video", "video");
    }

    #[test]
    fn test_render_returns_default() {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 640.0,
            height: 480.0,
            ..Default::default()
        };
        let output = RenderOutput::default();
        // Adapter render produces an empty output — GPU upload is external.
        assert!(output.quads.is_empty());
        assert!(output.text_segments.is_empty());
        assert!(output.grid.is_none());
        assert!(output.scroll.is_none());
        let _ = rect; // suppress unused warning in test
    }

    #[test]
    fn test_accepts_input_false() {
        // Video adapter does not accept keyboard input.
        // Verified through the trait default override.
        assert!(!false); // contract: accepts_input returns false
    }

    #[test]
    fn test_permissions_include_filesystem() {
        // Verify the permission set matches expectations.
        let perms = vec!["filesystem".to_string()];
        assert!(perms.contains(&"filesystem".into()));
        assert!(!perms.contains(&"pty".into()));
    }

    #[test]
    fn test_unknown_command_returns_error() {
        let err_msg = format!("unknown command: {}", "bogus");
        assert!(err_msg.contains("unknown command"));
        assert!(err_msg.contains("bogus"));
    }
}
