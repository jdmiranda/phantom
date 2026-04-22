use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use phantom_app::app::App;
use phantom_app::config::PhantomConfig;
use phantom_renderer::gpu::GpuContext;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowAttributes, WindowId},
};

mod headless;

struct Phantom {
    window: Option<Arc<Window>>,
    app: Option<App>,
    config: PhantomConfig,
    supervisor_socket: Option<PathBuf>,
    modifiers: winit::event::Modifiers,
}

impl Phantom {
    fn new(config: PhantomConfig, supervisor_socket: Option<PathBuf>) -> Self {
        Self {
            window: None,
            app: None,
            config,
            supervisor_socket,
            modifiers: winit::event::Modifiers::default(),
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
            .with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));

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

        let scale_factor = window.scale_factor() as f32;
        log::info!("Display scale factor: {scale_factor}");

        match App::with_config_scaled(gpu, self.config.clone(), self.supervisor_socket.as_deref(), scale_factor) {
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
                    app.handle_key_with_mods(event, self.modifiers);
                    if app.should_quit() {
                        event_loop.exit();
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
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

fn print_help() {
    println!(
        r#"PHANTOM v0.1.0 — AI-native terminal emulator

USAGE:
    phantom [OPTIONS]

OPTIONS:
    --headless               Run in headless REPL mode (no window, no GPU)
    --theme <NAME>          Theme: phosphor, amber, ice, blood, vapor
    --font-size <PT>        Font size in points (default: 14.0)
    --scanlines <0.0-1.0>   Scanline intensity
    --bloom <0.0-1.0>       Bloom/glow intensity
    --aberration <0.0-1.0>  Chromatic aberration
    --curvature <0.0-1.0>   CRT barrel distortion
    --vignette <0.0-1.0>    Vignette intensity
    --noise <0.0-1.0>       Film grain intensity
    --no-boot               Skip the boot sequence
    --init-config            Write default config to ~/.config/phantom/config.toml
    --help                   Print this help message

CONFIG:
    ~/.config/phantom/config.toml

EXAMPLES:
    phantom --theme amber --curvature 0.1
    phantom --bloom 0 --scanlines 0 --curvature 0
    phantom --theme ice --font-size 16"#
    );
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Quick exits
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    if args.iter().any(|a| a == "--init-config") {
        let path = PhantomConfig::write_default()?;
        println!("Wrote default config to {}", path.display());
        return Ok(());
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Load config file, then apply CLI overrides
    let mut config = PhantomConfig::load();
    let mut headless = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--headless" => {
                headless = true;
            }
            "--theme" => {
                i += 1;
                if i < args.len() {
                    config.theme_name = args[i].clone();
                }
            }
            "--font-size" => {
                i += 1;
                if i < args.len() {
                    if let Ok(v) = args[i].parse::<f32>() {
                        config.font_size = v;
                    }
                }
            }
            "--scanlines" => {
                i += 1;
                if i < args.len() {
                    if let Ok(v) = args[i].parse::<f32>() {
                        config.shader_overrides.scanline_intensity = Some(v);
                    }
                }
            }
            "--bloom" => {
                i += 1;
                if i < args.len() {
                    if let Ok(v) = args[i].parse::<f32>() {
                        config.shader_overrides.bloom_intensity = Some(v);
                    }
                }
            }
            "--aberration" => {
                i += 1;
                if i < args.len() {
                    if let Ok(v) = args[i].parse::<f32>() {
                        config.shader_overrides.chromatic_aberration = Some(v);
                    }
                }
            }
            "--curvature" => {
                i += 1;
                if i < args.len() {
                    if let Ok(v) = args[i].parse::<f32>() {
                        config.shader_overrides.curvature = Some(v);
                    }
                }
            }
            "--vignette" => {
                i += 1;
                if i < args.len() {
                    if let Ok(v) = args[i].parse::<f32>() {
                        config.shader_overrides.vignette_intensity = Some(v);
                    }
                }
            }
            "--noise" => {
                i += 1;
                if i < args.len() {
                    if let Ok(v) = args[i].parse::<f32>() {
                        config.shader_overrides.noise_intensity = Some(v);
                    }
                }
            }
            "--no-boot" => {
                config.skip_boot = true;
            }
            _ => {
                eprintln!("Unknown option: {}", args[i]);
                print_help();
                std::process::exit(1);
            }
        }
        i += 1;
    }

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

    // -- Headless mode --
    if headless {
        log::info!("Starting headless REPL mode");
        return headless::run_headless(config);
    }

    // -- Detect supervisor mode --
    let supervisor_socket = std::env::var("PHANTOM_SUPERVISOR_SOCK")
        .ok()
        .map(PathBuf::from);

    if let Some(ref sock) = supervisor_socket {
        log::info!("Supervisor mode: socket at {}", sock.display());
    }

    let event_loop = EventLoop::new()?;
    let mut app = Phantom::new(config, supervisor_socket);
    event_loop.run_app(&mut app)?;
    Ok(())
}
