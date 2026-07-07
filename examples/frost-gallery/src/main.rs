//! Headless frost gallery.
//!
//! Renders the own-loop demo scene — the same drifting colored blobs the winit example animates —
//! through the real [`OwnLoopRenderer`] on a software (lavapipe) device, reads the target back, and
//! writes PNGs. It is the *viewable* counterpart to `egui-wgpu-panel`: no window, no compositor,
//! just image files showing the frosted glass at a range of blur radii (Gaussian small-radius
//! through dual-Kawase large-radius) and two tints (dark glass, light frost).
//!
//! Run on a Vulkan software-rasterizer host (this repo's gated GPU tier):
//! ```text
//! VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json WGPU_BACKEND=vulkan \
//!   cargo run --manifest-path examples/frost-gallery/Cargo.toml
//! ```
//! PNGs land in `examples/frost-gallery/out/`.
#![expect(
    clippy::print_stdout,
    reason = "demo binary; stdout reports the written gallery paths"
)]

use std::fs;
use std::path::Path;

use backdrop_blur_egui::{
    BlurRadius, CornerRadius, FrameInput, LinearRgba, Opacity, OwnLoopRenderer, RepaintPolicy,
    ScreenDescriptor, Surface, Tint, WgpuBlur,
};

/// Gallery canvas, in physical pixels (pixels-per-point = 1).
const W: u32 = 900;
const H: u32 = 560;
/// The non-sRGB Unorm target the own-loop adapter composites into (egui writes gamma into it).
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// A fixed instant of the drift animation, chosen so the blobs sit pleasantly behind the panel.
const SCENE_T: f32 = 2.4;

/// One frosted panel to render over the scene: a name for the output file and the surface itself.
struct Variant {
    name: &'static str,
    surface: Option<Surface>,
}

fn main() {
    let (device, queue) = software_device();
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("out");
    fs::create_dir_all(&out_dir).expect("create gallery out/ dir");

    for variant in variants() {
        let pixels = render(&device, &queue, variant.surface.as_slice());
        let path = out_dir.join(format!("{}.png", variant.name));
        write_png(&path, &pixels);
        println!("wrote {}", path.display());
    }

    println!(
        "\n{} PNGs in {} — open them, or run `montage` for a contact sheet.",
        variants().len(),
        out_dir.display()
    );
}

/// The gallery: the bare backdrop, then the same scene frosted at rising blur radii and two tints.
/// The radii straddle the 16 px Gaussian→dual-Kawase threshold so both algorithms are on display.
fn variants() -> Vec<Variant> {
    let dark = LinearRgba::new(0.04, 0.05, 0.08, 0.34);
    let light = LinearRgba::new(0.90, 0.92, 0.96, 0.45);
    // Scene-ID strings keep the legacy "strength" word to match this gallery's committed shots
    // (`examples/frost-gallery/out/*.png`) — renaming them would desync the filenames from the
    // checked-in reference images.
    vec![
        Variant {
            name: "00-backdrop",
            surface: None,
        },
        frosted("01-dark-strength-04", dark, 4.0),
        frosted("02-dark-strength-12", dark, 12.0),
        frosted("03-dark-strength-20", dark, 20.0),
        frosted("04-dark-strength-40", dark, 40.0),
        frosted("05-dark-strength-64", dark, 64.0),
        frosted("06-light-strength-32", light, 32.0),
    ]
}

/// A centered frosted panel over the scene at the given tint and blur radius.
fn frosted(name: &'static str, tint: LinearRgba, blur_radius: f32) -> Variant {
    let panel_size = egui::vec2(W as f32 * 0.56, H as f32 * 0.52);
    let center = egui::pos2(W as f32 * 0.5, H as f32 * 0.5);
    Variant {
        name,
        surface: Some(Surface {
            rect: egui::Rect::from_center_size(center, panel_size),
            blur_radius: BlurRadius::new(blur_radius),
            tint: Tint::new(tint),
            corner_radius: CornerRadius::new(28.0),
            opacity: Opacity::default(),
            repaint: RepaintPolicy::Static,
        }),
    }
}

/// Render one egui frame (the scene) plus the given surfaces through the own-loop adapter and read
/// the target texture back as tight RGBA8 rows.
fn render(device: &wgpu::Device, queue: &wgpu::Queue, surfaces: &[Surface]) -> Vec<u8> {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(W as f32, H as f32),
        )),
        ..Default::default()
    };
    let output = ctx.run_ui(raw, |ui| paint_backdrop(ui, SCENE_T));
    let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("gallery target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let mut adapter =
        OwnLoopRenderer::new(device, FORMAT).expect("Rgba8Unorm is a supported target");
    let mut blur = WgpuBlur::new(device);
    adapter
        .render_frame(
            device,
            queue,
            &ctx,
            &mut blur,
            FrameInput {
                target: &target_view,
                paint_jobs: &jobs,
                textures_delta: &output.textures_delta,
                screen: ScreenDescriptor {
                    size_in_pixels: [W, H],
                    pixels_per_point: 1.0,
                },
            },
            surfaces,
        )
        .expect("render_frame succeeds on the software device");

    read_back(device, queue, &target)
}

/// Paint the animating backdrop: drifting colored blobs the frosted panel will blur. Mirrors the
/// winit example's scene so the gallery shows exactly what the live demo does.
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
}

/// A LowPower software adapter (lavapipe via `VK_ICD_FILENAMES`), no surface — headless.
fn software_device() -> (wgpu::Device, wgpu::Queue) {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("a Vulkan adapter (lavapipe via VK_ICD_FILENAMES) is required for the gallery");
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("frost-gallery device"),
        required_features: wgpu::Features::empty(),
        required_limits: adapter.limits(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        trace: wgpu::Trace::Off,
    }))
    .expect("device creation on the software adapter")
}

/// Copy the target texture to a buffer and return tight (unpadded) RGBA8 rows.
fn read_back(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Vec<u8> {
    let unpadded = W * 4;
    let padded =
        unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("gallery readback"),
        size: u64::from(padded * H),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        tx.send(res).expect("map channel send")
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll");
    rx.recv().expect("map callback").expect("buffer map");

    let mapped = slice.get_mapped_range();
    let mut tight = vec![0u8; (unpadded * H) as usize];
    for y in 0..H as usize {
        let src = y * padded as usize;
        let dst = y * unpadded as usize;
        tight[dst..dst + unpadded as usize].copy_from_slice(&mapped[src..src + unpadded as usize]);
    }
    tight
}

/// Write tight RGBA8 rows as an sRGB PNG. The target is non-sRGB Unorm holding gamma-encoded values
/// (the composite re-encoded linear→sRGB on write), so the bytes are already display-ready.
fn write_png(path: &Path, pixels: &[u8]) {
    let file = fs::File::create(path).expect("create png file");
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("write png header")
        .write_image_data(pixels)
        .expect("write png data");
}
