use anyhow::Result;
use phantom_app::app::App;
use phantom_renderer::gpu::GpuContext;
use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowAttributes, WindowId},
};

struct Phantom {
    window: Option<Arc<Window>>,
    app: Option<App>,
}

impl Phantom {
    fn new() -> Self {
        Self {
            window: None,
            app: None,
        }
    }
}

impl ApplicationHandler for Phantom {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = WindowAttributes::default()
            .with_title("PHANTOM v0.1.0")
            .with_inner_size(winit::dpi::LogicalSize::new(1200, 800));

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("Failed to create window"),
        );

        let gpu = match GpuContext::new(window.clone()) {
            Ok(gpu) => {
                log::info!(
                    "GPU initialized: {}x{}",
                    gpu.surface_config.width,
                    gpu.surface_config.height
                );
                gpu
            }
            Err(e) => {
                log::error!("Failed to initialize GPU: {e}");
                event_loop.exit();
                return;
            }
        };

        match App::new(gpu) {
            Ok(app) => {
                self.app = Some(app);
            }
            Err(e) => {
                log::error!("Failed to initialize Phantom: {e}");
                event_loop.exit();
                return;
            }
        }

        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                log::info!("Window closed. Shutting down.");
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                if let Some(app) = &mut self.app {
                    app.handle_resize(new_size.width, new_size.height);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(app) = &mut self.app {
                    app.handle_key(event);
                    if app.should_quit() {
                        event_loop.exit();
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                if let Some(app) = &mut self.app {
                    app.handle_modifiers(modifiers);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(app) = &mut self.app {
                    app.update();
                    if let Err(e) = app.render() {
                        log::error!("Render error: {e}");
                    }
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!(
        r#"
 ██████╗ ██╗  ██╗ █████╗ ███╗   ██╗████████╗ ██████╗ ███╗   ███╗
 ██╔══██╗██║  ██║██╔══██╗████╗  ██║╚══██╔══╝██╔═══██╗████╗ ████║
 ██████╔╝███████║███████║██╔██╗ ██║   ██║   ██║   ██║██╔████╔██║
 ██╔═══╝ ██╔══██║██╔══██║██║╚██╗██║   ██║   ██║   ██║██║╚██╔╝██║
 ██║     ██║  ██║██║  ██║██║ ╚████║   ██║   ╚██████╔╝██║ ╚═╝ ██║
 ╚═╝     ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝   ╚═╝    ╚═════╝ ╚═╝     ╚═╝
                        v0.1.0
"#
    );

    let event_loop = EventLoop::new()?;
    let mut app = Phantom::new();
    event_loop.run_app(&mut app)?;
    Ok(())
}
