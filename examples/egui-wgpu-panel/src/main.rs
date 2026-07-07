//! Own-loop demo: a frosted-glass panel over animating content.
//!
//! Drives `egui-winit` + `egui-wgpu` directly (not eframe) and uses
//! [`backdrop_blur_egui::OwnLoopRenderer`] to frost a panel over a moving backdrop.
//!
//! Controls: **Space** toggles the blur, **Up/Down** change its radius. The window title shows
//! the per-frame blur cost.
//!
//! Run with a display: `cargo run -p egui-wgpu-panel` (needs a GPU/compositor).
#![expect(
    clippy::print_stderr,
    reason = "demo binary; stderr reports setup failures"
)]

use std::sync::Arc;
use std::time::Instant;

use backdrop_blur_egui::{
    BlurRadius, CornerRadius, FrameInput, LinearRgba, OwnLoopRenderer, Presence, RepaintPolicy,
    ScreenDescriptor, Surface, Tint, WgpuBlur,
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

/// Everything created once the event loop resumes (we have a window).
struct Gpu {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    own_loop: OwnLoopRenderer,
    blur: WgpuBlur,
}

struct App {
    gpu: Option<Gpu>,
    start: Instant,
    blur_on: bool,
    blur_radius: f32,
}

impl App {
    fn new() -> Self {
        Self {
            gpu: None,
            start: Instant::now(),
            blur_on: true,
            blur_radius: 16.0,
        }
    }
}

/// Pick a backdrop-blur-supported swapchain format (the non-sRGB Unorm ones egui writes gamma
/// into), or `None` if the surface offers none — the adapter only composites into those.
fn choose_format(caps: &wgpu::SurfaceCapabilities) -> Option<wgpu::TextureFormat> {
    caps.formats
        .iter()
        .copied()
        .find(|f| backdrop_blur_egui::is_supported_target(*f))
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes().with_title("backdrop-blur — frosted panel"),
                )
                .expect("create window"),
        );

        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(Arc::clone(&window))
            .expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("an adapter compatible with the window surface");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("egui-wgpu-panel device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            trace: wgpu::Trace::Off,
        }))
        .expect("device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let Some(format) = choose_format(&caps) else {
            eprintln!("no backdrop-blur-supported (non-sRGB Unorm) swapchain format; exiting");
            event_loop.exit();
            return;
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let own_loop = OwnLoopRenderer::new(&device, format).expect("choose_format returns a supported target");
        let blur = WgpuBlur::new(&device);

        self.gpu = Some(Gpu {
            window,
            surface,
            device,
            queue,
            config,
            egui_ctx,
            egui_state,
            own_loop,
            blur,
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        let _ = gpu.egui_state.on_window_event(&gpu.window, &event);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                gpu.config.width = size.width.max(1);
                gpu.config.height = size.height.max(1);
                gpu.surface.configure(&gpu.device, &gpu.config);
                gpu.window.request_redraw();
            }
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } if key_event.state.is_pressed() => {
                match key_event.logical_key {
                    Key::Named(NamedKey::Space) => self.blur_on = !self.blur_on,
                    Key::Named(NamedKey::ArrowUp) => {
                        self.blur_radius = (self.blur_radius + 2.0).min(64.0)
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        self.blur_radius = (self.blur_radius - 2.0).max(0.0)
                    }
                    _ => {}
                }
                gpu.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.render();
            }
            // The resume signal after occlusion: the acquire path stays quiet while occluded.
            WindowEvent::Occluded(false) => gpu.window.request_redraw(),
            _ => {}
        }
    }
}

impl App {
    fn render(&mut self) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                // Retry with the fresh swapchain — without this, one failed acquire kills the
                // Wait-mode redraw chain.
                gpu.window.request_redraw();
                return;
            }
            // Occluded resumes via the `WindowEvent::Occluded(false)` handler; re-arming here
            // would busy-loop while hidden.
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let t = self.start.elapsed().as_secs_f32();
        let [w, h] = [gpu.config.width as f32, gpu.config.height as f32];
        let ppp = gpu.window.scale_factor() as f32;

        let raw_input = gpu.egui_state.take_egui_input(&gpu.window);
        let output = gpu.egui_ctx.run_ui(raw_input, |ui| {
            paint_backdrop(ui, t);
        });
        gpu.egui_state
            .handle_platform_output(&gpu.window, output.platform_output);
        let jobs = gpu
            .egui_ctx
            .tessellate(output.shapes, output.pixels_per_point);

        // A frosted panel centered in the window; Live, because the backdrop animates.
        let panel_size = egui::vec2(w / ppp * 0.5, h / ppp * 0.4);
        let center = egui::pos2(w / ppp * 0.5, h / ppp * 0.5);
        let panel = Surface {
            rect: egui::Rect::from_center_size(center, panel_size),
            blur_radius: BlurRadius::new(self.blur_radius),
            tint: Tint::new(LinearRgba::new(0.04, 0.05, 0.08, 0.35)),
            corner_radius: CornerRadius::new(24.0),
            presence: Presence::default(),
            repaint: RepaintPolicy::Live,
        };
        let surfaces: &[Surface] = if self.blur_on {
            std::slice::from_ref(&panel)
        } else {
            &[]
        };

        let blur_start = Instant::now();
        let result = gpu.own_loop.render_frame(
            &gpu.device,
            &gpu.queue,
            &gpu.egui_ctx,
            &mut gpu.blur,
            FrameInput {
                target: &view,
                paint_jobs: &jobs,
                textures_delta: &output.textures_delta,
                screen: ScreenDescriptor {
                    size_in_pixels: [gpu.config.width, gpu.config.height],
                    pixels_per_point: output.pixels_per_point,
                },
            },
            surfaces,
        );
        let frame_ms = blur_start.elapsed().as_secs_f32() * 1000.0;

        match result {
            Ok(()) => {
                frame.present();
                gpu.window.set_title(&format!(
                    "backdrop-blur — frosted panel  |  blur {}  radius {:.0}  |  {:.2} ms/frame",
                    if self.blur_on { "on" } else { "off" },
                    self.blur_radius,
                    frame_ms,
                ));
                // Live backdrop → keep animating.
                gpu.window.request_redraw();
            }
            Err(err) => {
                eprintln!("blur error: {err}");
                // A transient blur error must not freeze a Live animation (a persistent one
                // retries visibly with its eprintln — accepted for a demo over a silent freeze).
                gpu.window.request_redraw();
            }
        }
    }
}

/// Paint the animating backdrop: drifting colored blobs the frosted panel will blur.
fn paint_backdrop(ui: &mut egui::Ui, t: f32) {
    let painter = ui.painter();
    let rect = ui.max_rect();
    painter.rect_filled(
        rect,
        egui::CornerRadius::ZERO,
        egui::Color32::from_rgb(18, 20, 28),
    );
    let blobs = [
        (egui::Color32::from_rgb(220, 60, 80), 0.0_f32),
        (egui::Color32::from_rgb(60, 160, 220), 2.1),
        (egui::Color32::from_rgb(120, 220, 120), 4.2),
        (egui::Color32::from_rgb(230, 190, 60), 1.0),
    ];
    for (i, (color, phase)) in blobs.iter().enumerate() {
        let fx = 0.5 + 0.42 * (t * 0.6 + phase).sin();
        let fy = 0.5 + 0.40 * (t * 0.45 + phase * 1.7 + i as f32).cos();
        let center = rect.min + egui::vec2(rect.width() * fx, rect.height() * fy);
        painter.circle_filled(center, rect.width().min(rect.height()) * 0.18, *color);
    }
    painter.text(
        rect.min + egui::vec2(12.0, 12.0),
        egui::Align2::LEFT_TOP,
        "Space: toggle blur   Up/Down: radius",
        egui::FontId::proportional(16.0),
        egui::Color32::from_white_alpha(180),
    );
}

fn main() {
    let event_loop = EventLoop::new().expect("create event loop");
    event_loop
        .run_app(&mut App::new())
        .expect("run the winit app");
}
