//! Gated GPU tier (IMPL §2b/§2c): the real end-to-end proof. Builds a `wgpu::Device` on the
//! software (lavapipe) adapter, frosts a panel over a hard-edged backdrop, reads the target
//! back, and asserts the three things that make it *frosted glass*: the backdrop edge got
//! blurred, content outside the panel is untouched, and the rounded corner is masked out.
//!
//! Runs only with `--features image-snapshots` on a host with a Vulkan software rasterizer.
//! The default `cargo test` compiles this file to nothing (the whole module is feature-gated),
//! so it never tries to create a GPU device on a GPU-less machine.
#![cfg(feature = "image-snapshots")]

use backdrop_blur_core::{
    BackdropBlur, BlurRequest, BlurStrength, CornerRadius, LinearRgba, Region, Scale, Tint,
};
use backdrop_blur_wgpu::{SourceColorSpace, SourceView, WgpuBlur};

const DIM: u32 = 200;
const EDGE_X: u32 = 100; // backdrop: red left of this, blue right of it

/// A headless device on the software adapter (lavapipe), so the test is deterministic and
/// needs no real GPU.
fn software_device() -> (wgpu::Device, wgpu::Queue) {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    // The gated tier pins VK_ICD_FILENAMES to lavapipe, so the only Vulkan adapter the loader
    // exposes is the software rasterizer; no `force_fallback_adapter` filtering is needed (and it
    // would wrongly exclude lavapipe, which Vulkan does not flag as a DX12-WARP-style fallback).
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("a Vulkan adapter (lavapipe via VK_ICD_FILENAMES) is required for the gated GPU tier");

    // Use the adapter's own limits so the request can never exceed what a software backend
    // (lavapipe Vulkan, or Mesa llvmpipe GL) actually provides.
    let limits = adapter.limits();
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("backdrop-blur test device"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        trace: wgpu::Trace::Off,
    }))
    .expect("device creation on the software adapter")
}

/// A `DIM×DIM` Rgba8Unorm texture filled with a hard vertical edge: red on the left, blue on
/// the right. Returned as `TEXTURE_BINDING | COPY_SRC | COPY_DST` so it can be sampled and copied.
fn backdrop_texture(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::Texture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("backdrop"),
        size: wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    let mut pixels = vec![0u8; (DIM * DIM * 4) as usize];
    for y in 0..DIM {
        for x in 0..DIM {
            let i = ((y * DIM + x) * 4) as usize;
            let [r, g, b] = if x < EDGE_X { [255, 0, 0] } else { [0, 0, 255] };
            pixels[i] = r;
            pixels[i + 1] = g;
            pixels[i + 2] = b;
            pixels[i + 3] = 255;
        }
    }

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(DIM * 4),
            rows_per_image: Some(DIM),
        },
        wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
    );
    texture
}

fn region(origin: [u32; 2], size: [u32; 2]) -> Region {
    Region {
        origin,
        size,
        scale: Scale::new(1.0),
    }
}

/// A `DIM×DIM` Rgba8Unorm texture filled with a single opaque gray.
fn flat_backdrop(device: &wgpu::Device, queue: &wgpu::Queue, gray: u8) -> wgpu::Texture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("flat backdrop"),
        size: wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let pixels = vec![gray; (DIM * DIM * 4) as usize]
        .iter()
        .enumerate()
        .map(|(i, _)| if i % 4 == 3 { 255 } else { gray })
        .collect::<Vec<u8>>();
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(DIM * 4),
            rows_per_image: Some(DIM),
        },
        wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
    );
    texture
}

/// Frost a panel over `backdrop` and read the target back. The panel covers the centre 100×100;
/// the target is seeded with the backdrop (the host blits intermediate→swapchain first).
fn frost_and_read(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    backdrop: &wgpu::Texture,
    strength: f32,
    tint: Tint,
) -> Vec<u8> {
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"),
        size: wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut blur = WgpuBlur::new(device);
    let panel = region([50, 50], [100, 100]);
    let request = BlurRequest {
        source_region: panel,
        target_rect: panel,
        strength: BlurStrength::new(strength),
        tint,
        corner_radius: CornerRadius::new(24.0),
    };
    let source = SourceView {
        view: backdrop.create_view(&wgpu::TextureViewDescriptor::default()),
        size: [DIM, DIM],
        color_space: SourceColorSpace::GammaSrgb,
    };
    let prepared = blur
        .prepare(
            device,
            queue,
            &source,
            wgpu::TextureFormat::Rgba8Unorm,
            &request,
        )
        .expect("prepare")
        .expect("non-empty region");
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("frame"),
    });
    encoder.copy_texture_to_texture(
        wgpu::TexelCopyTextureInfo {
            texture: backdrop,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
    );
    blur.record(&mut encoder, &target_view, &prepared)
        .expect("record");
    queue.submit([encoder.finish()]);
    read_back(device, queue, &target)
}

/// Read an Rgba8 texture back into a row-major `Vec<u8>` (tightly packed, `DIM*4` per row).
fn read_back(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Vec<u8> {
    let unpadded = DIM * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(padded * DIM),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("readback enc"),
    });
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
                rows_per_image: Some(DIM),
            },
        },
        wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        tx.send(res).expect("readback map channel send");
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll");
    rx.recv()
        .expect("map callback fired")
        .expect("buffer map succeeded");

    let mapped = slice.get_mapped_range();
    let mut tight = vec![0u8; (unpadded * DIM) as usize];
    for y in 0..DIM as usize {
        let src = y * padded as usize;
        let dst = y * unpadded as usize;
        tight[dst..dst + unpadded as usize].copy_from_slice(&mapped[src..src + unpadded as usize]);
    }
    tight
}

fn pixel(data: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * DIM + x) * 4) as usize;
    [data[i], data[i + 1], data[i + 2], data[i + 3]]
}

#[test]
fn frosted_panel_blurs_the_backdrop_inside_the_masked_rect() {
    let (device, queue) = software_device();
    let backdrop = backdrop_texture(&device, &queue);

    // The target starts as a copy of the backdrop (the host blits intermediate→swapchain before
    // compositing the surface), so anything the surface does NOT touch stays the backdrop.
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"),
        size: wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    let mut blur = WgpuBlur::new(&device);

    // Panel over the centre 100×100; its backdrop is the same screen area, spanning the edge.
    let panel = region([50, 50], [100, 100]);
    let request = BlurRequest {
        source_region: panel,
        target_rect: panel,
        strength: BlurStrength::new(10.0),
        tint: Tint::new(LinearRgba::new(0.0, 0.0, 0.0, 0.15)), // faint darkening film
        corner_radius: CornerRadius::new(24.0),
    };

    let source = SourceView {
        view: backdrop.create_view(&wgpu::TextureViewDescriptor::default()),
        size: [DIM, DIM],
        color_space: SourceColorSpace::GammaSrgb,
    };
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let prepared = blur
        .prepare(
            &device,
            &queue,
            &source,
            wgpu::TextureFormat::Rgba8Unorm,
            &request,
        )
        .expect("prepare succeeds")
        .expect("a non-empty region prepares a blur");

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("frame"),
    });
    // Seed the target with the backdrop, then composite the frosted panel over it.
    encoder.copy_texture_to_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &backdrop,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
    );
    blur.record(&mut encoder, &target_view, &prepared)
        .expect("record");
    queue.submit([encoder.finish()]);

    let out = read_back(&device, &queue, &target);

    // 1. Outside the panel entirely: untouched backdrop (pure red on the left).
    let [r, _, b, _] = pixel(&out, 10, 10);
    assert!(
        r > 200 && b < 40,
        "outside the panel must be untouched backdrop red, got r={r} b={b}"
    );

    // 2. The rounded corner is masked out: [54,54] is inside the panel rect but in the corner
    //    cutoff (radius 24 → arc centre [74,74], distance ≈28 > 24), so it shows the backdrop.
    let [r, _, b, _] = pixel(&out, 54, 54);
    assert!(
        r > 200 && b < 40,
        "the masked rounded corner must show backdrop red, got r={r} b={b}"
    );

    // 3. Panel centre sits on the backdrop's hard red/blue edge. Unblurred it would be pure blue
    //    (x=100 is the first blue column); a real blur bleeds red across, so BOTH channels are
    //    substantial. This is the proof the convolution ran.
    let [r, _, b, _] = pixel(&out, 100, 100);
    assert!(
        r > 30 && b > 30,
        "the panel centre must show a blurred red↔blue mix (both channels present), got r={r} b={b}"
    );

    // 4. Gamma round-trip: an interior, fully-covered pixel well inside the panel and 30px from
    //    the seam samples uniform red. The blur leaves it red (linear 1,0,0); the 15%-black tint
    //    film yields linear 0.85; the Rgba8Unorm target needs the manual linear→sRGB encode, so
    //    the readback is ~237 (0.930·255), NOT ~217 (0.85·255 — what a SKIPPED encode would give).
    //    This is the assertion that can actually fail on a gamma-encode bug.
    let [r, g, b, _] = pixel(&out, 70, 100);
    assert!(
        (230..=244).contains(&r) && g <= 6 && b <= 6,
        "interior red must round-trip through the linear→sRGB encode to ~237, got r={r} g={g} b={b}"
    );
}

#[test]
fn dual_kawase_preserves_energy_on_a_flat_backdrop() {
    // strength 30 (≥ the 16px threshold) takes the dual-Kawase path. Over a FLAT mid-gray backdrop
    // with no tint, an energy-preserving down/up filter (5-tap ÷8, 8-tap ÷12) must leave the gray
    // unchanged — a wrong-weight kernel that doesn't sum to 1 shifts the brightness and fails here.
    // This is the real guard the "non-trivial output" readback cannot give (IMPL §2b′).
    let (device, queue) = software_device();
    let gray = flat_backdrop(&device, &queue, 128);
    let out = frost_and_read(
        &device,
        &queue,
        &gray,
        30.0,
        Tint::new(LinearRgba::new(0.0, 0.0, 0.0, 0.0)),
    );

    // Panel centre, far from the panel edges: still ~128 (flat in, flat out).
    let [r, g, b, _] = pixel(&out, 100, 100);
    assert!(
        (120..=136).contains(&r) && (120..=136).contains(&g) && (120..=136).contains(&b),
        "dual-Kawase must preserve a flat backdrop's brightness, got r={r} g={g} b={b}"
    );
}

#[test]
fn dual_kawase_blurs_a_large_radius_edge() {
    // The hard red/blue edge under a large (dual-Kawase) blur: the panel centre, on the seam, is a
    // strong mix — proving the multi-level pyramid actually reaches across the seam (a zero/wrong
    // half-pixel offset would leave the seam sharp and the centre near-pure-blue).
    let (device, queue) = software_device();
    let edge = backdrop_texture(&device, &queue);
    let out = frost_and_read(
        &device,
        &queue,
        &edge,
        30.0,
        Tint::new(LinearRgba::new(0.0, 0.0, 0.0, 0.1)),
    );

    let [r, _, b, _] = pixel(&out, 100, 100);
    assert!(
        r > 40 && b > 40,
        "a large dual-Kawase blur must mix red and blue across the seam, got r={r} b={b}"
    );
    // Outside the panel stays untouched backdrop red.
    let [r, _, b, _] = pixel(&out, 10, 100);
    assert!(
        r > 200 && b < 40,
        "outside the panel must stay backdrop red, got r={r} b={b}"
    );
}
