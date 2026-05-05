use anyhow::Result;
use std::sync::Arc;
use wgpu::{
    Backends, Device, DeviceDescriptor, Features, Instance, InstanceDescriptor, Limits,
    MemoryHints, PowerPreference, Queue, RequestAdapterOptions, Surface, SurfaceConfiguration,
    TextureFormat, TextureUsages,
};
use winit::window::Window;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during GPU initialization.
#[derive(Debug)]
pub enum GpuError {
    /// The surface reported an empty list of supported texture formats.
    ///
    /// This should never happen on a functioning GPU/driver stack, but can
    /// occur on headless machines, CI environments, or with broken wgpu
    /// backends. Returning an error here is safer than an index panic.
    NoSupportedFormat,
    /// No adapter was found that satisfies the requested power preference and
    /// surface compatibility.
    NoAdapter,
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSupportedFormat => write!(
                f,
                "GPU surface reported no supported texture formats; \
                 check GPU drivers or wgpu backend"
            ),
            Self::NoAdapter => write!(
                f,
                "no suitable GPU adapter found; \
                 ensure a compatible GPU and up-to-date drivers are present"
            ),
        }
    }
}

impl std::error::Error for GpuError {}

/// Core GPU state — device, queue, surface, and configuration.
pub struct GpuContext {
    pub device: Device,
    pub queue: Queue,
    pub surface: Surface<'static>,
    pub surface_config: SurfaceConfiguration,
    pub format: TextureFormat,
}

impl GpuContext {
    /// Initialize wgpu with the given window. Prefers Metal on macOS, Vulkan on Linux.
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let instance = Instance::new(&InstanceDescriptor {
            backends: Backends::PRIMARY,
            ..Default::default()
        });

        let surface = instance.create_surface(window.clone())?;

        let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;

        log::info!("GPU adapter: {:?}", adapter.get_info());

        let (device, queue) = pollster::block_on(adapter.request_device(
            &DeviceDescriptor {
                label: Some("phantom-device"),
                required_features: Features::empty(),
                required_limits: Limits::default(),
                memory_hints: MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            },
        ))?;

        let surface_caps = surface.get_capabilities(&adapter);
        // Prefer an sRGB format; fall back to whatever the driver offers.
        // `formats` can be empty on headless/CI builds — return a typed error
        // instead of panicking with an index-out-of-bounds.
        let format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .or_else(|| surface_caps.formats.first().copied())
            .ok_or(GpuError::NoSupportedFormat)?;

        let size = window.inner_size();
        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        Ok(Self {
            device,
            queue,
            surface,
            surface_config,
            format,
        })
    }

    /// Handle window resize.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.surface_config.width = width;
            self.surface_config.height = height;
            self.surface.configure(&self.device, &self.surface_config);
        }
    }

    /// Render a single frame. For now, just clears to background color.
    pub fn render(&self, bg: [f64; 4]) -> Result<()> {
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&Default::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("phantom-encoder"),
            });

        // Clear to background color
        {
            let _render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0],
                            g: bg[1],
                            b: bg[2],
                            a: bg[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the format-selection logic returns `GpuError::NoSupportedFormat`
    /// when the formats list is empty.
    ///
    /// We cannot call `GpuContext::new` in a unit test (no real window), so
    /// this test directly exercises the same `.or_else().ok_or()` chain used
    /// in `GpuContext::new` to confirm the error path is reachable and typed
    /// correctly.
    #[test]
    fn gpu_init_returns_error_when_no_surface_formats() {
        let formats: Vec<TextureFormat> = vec![];

        let result: Result<TextureFormat, GpuError> = formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .or_else(|| formats.first().copied())
            .ok_or(GpuError::NoSupportedFormat);

        assert!(
            result.is_err(),
            "empty formats list must yield GpuError::NoSupportedFormat"
        );
        assert!(
            matches!(result.unwrap_err(), GpuError::NoSupportedFormat),
            "error variant must be NoSupportedFormat"
        );
    }

    /// Verify that an sRGB format is preferred when one is available.
    #[test]
    fn gpu_init_prefers_srgb_format() {
        let formats = vec![
            TextureFormat::Rgba8Unorm,
            TextureFormat::Rgba8UnormSrgb,
            TextureFormat::Bgra8Unorm,
        ];

        let format: Result<TextureFormat, GpuError> = formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .or_else(|| formats.first().copied())
            .ok_or(GpuError::NoSupportedFormat);

        assert!(format.is_ok(), "should find sRGB format");
        assert!(format.unwrap().is_srgb(), "selected format must be sRGB");
    }

    /// Verify that the first non-sRGB format is used when no sRGB format exists.
    #[test]
    fn gpu_init_falls_back_to_first_format_when_no_srgb() {
        let formats = vec![TextureFormat::Rgba8Unorm, TextureFormat::Bgra8Unorm];

        let format: Result<TextureFormat, GpuError> = formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .or_else(|| formats.first().copied())
            .ok_or(GpuError::NoSupportedFormat);

        assert!(format.is_ok(), "should fall back to first format");
        assert_eq!(
            format.unwrap(),
            TextureFormat::Rgba8Unorm,
            "fallback must be the first element"
        );
    }

    /// Verify `GpuError` variants implement `Display` and produce non-empty messages.
    #[test]
    fn gpu_error_display_is_non_empty() {
        let no_fmt = GpuError::NoSupportedFormat;
        assert!(!no_fmt.to_string().is_empty());

        let no_adapter = GpuError::NoAdapter;
        assert!(!no_adapter.to_string().is_empty());
    }
}
