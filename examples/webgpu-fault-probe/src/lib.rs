//! webgpu-fault-probe — the executed browser check on the own-loop web fault design
//! (`own-loop-wasm-device-fault-async`). It drives the **real** `backdrop-blur-wgpu` crate on a
//! real WebGPU device and demonstrates, in order:
//!
//! 1. async construction (`WgpuBlur::new` + `prewarm_composite`) succeeds on the WebGPU
//!    dispatch — the previously-shipped guard panicked unconditionally here;
//! 2. a clean frame prepares/records/submits with `take_fault() == None`;
//! 3. sustained pressure — **accumulating distinct `PingPongKey` scratch chains** inside the
//!    retention window (a blur region can never out-size its source and no single texture may
//!    exceed `max_texture_dimension_2d`, so this accumulation is the code path's real OOM
//!    shape) — panics nowhere and surfaces a `DeviceOutOfMemory` fault report within a bounded
//!    number of frames;
//! 4. recovery — pressure drops, fault-driven invalidation recreates the slots, reads go clean;
//! 5. the device survives throughout (`set_device_lost_callback` never fires).
//!
//! Run with trunk + the flagged-Chrome recipe recorded on the issue.

use std::sync::atomic::{AtomicBool, Ordering};

use backdrop_blur_core::{
    BackdropBlur, BlurRadius, BlurRequest, CornerRadius, LinearRgba, Presence, Region, Scale, Tint,
};
use backdrop_blur_wgpu::{SourceColorSpace, SourceView, WgpuBlur};
use wasm_bindgen::prelude::*;

/// Set by the device-lost callback; phase 5 asserts it stayed false. A process-global atomic
/// because `set_device_lost_callback` demands a `Send` closure even on the single-threaded wasm
/// runtime.
static DEVICE_LOST: AtomicBool = AtomicBool::new(false);

const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// Append a line to the on-page log and the browser console.
fn log(message: &str) {
    web_sys::console::log_1(&message.into());
    if let Some(pre) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("log"))
    {
        let existing = pre.text_content().unwrap_or_default();
        pre.set_text_content(Some(&format!("{existing}{message}\n")));
    }
}

/// Yield to the browser event loop for `ms`, letting deferred error-scope pops resolve.
async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        web_sys::window()
            .expect("probe runs in a window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .expect("setTimeout is available");
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Drive one seam frame: a square blur region of `region_side` sampled from the probe's source,
/// composited into the probe's offscreen target, submitted. Radius 4 keeps every frame on the
/// Gaussian path, so each distinct `region_side` is a distinct ping-pong `PingPongKey`.
fn drive_frame(
    blur: &mut WgpuBlur,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    source_texture: &wgpu::Texture,
    source_side: u32,
    target_view: &wgpu::TextureView,
    region_side: u32,
) -> Result<bool, String> {
    let source = SourceView {
        view: source_texture.create_view(&wgpu::TextureViewDescriptor::default()),
        size: [source_side, source_side],
        color_space: SourceColorSpace::GammaSrgb,
    };
    let region = Region {
        origin: [0, 0],
        size: [region_side, region_side],
        scale: Scale::new(1.0),
    };
    let request = BlurRequest {
        source_region: region,
        target_rect: Region {
            origin: [0, 0],
            size: [512, 512],
            scale: Scale::new(1.0),
        },
        blur_radius: BlurRadius::new(4.0),
        tint: Tint::new(LinearRgba::new(0.0, 0.0, 0.0, 0.1)),
        corner_radius: CornerRadius::new(8.0),
        presence: Presence::default(),
    };
    let prepared = blur
        .prepare(device, queue, &source, TARGET_FORMAT, &request)
        .map_err(|e| format!("prepare returned Err (unexpected on the web path): {e}"))?;
    match prepared {
        Some(prepared) => {
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("probe frame"),
            });
            blur.record(&mut encoder, target_view, prepared)
                .map_err(|e| format!("record returned Err: {e}"))?;
            queue.submit(std::iter::once(encoder.finish()));
            Ok(true)
        }
        None => Ok(false),
    }
}

#[wasm_bindgen(start)]
pub async fn run() {
    console_error_panic_hook::set_once();
    match probe().await {
        Ok(()) => log("PROBE: PASS (5/5 phases)"),
        Err(e) => log(&format!("PROBE: FAIL — {e}")),
    }
}

async fn probe() -> Result<(), String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::BROWSER_WEBGPU,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        })
        .await
        .map_err(|e| format!("no WebGPU adapter (is the page a secure context?): {e}"))?;
    let info = adapter.get_info();
    log(&format!(
        "adapter: {} — {:?} / {:?}",
        info.name, info.device_type, info.backend
    ));
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("webgpu-fault-probe device"),
            ..Default::default()
        })
        .await
        .map_err(|e| format!("request_device failed: {e}"))?;
    device.set_device_lost_callback(|reason, message| {
        DEVICE_LOST.store(true, Ordering::SeqCst);
        web_sys::console::error_1(&format!("DEVICE LOST ({reason:?}): {message}").into());
    });

    let max_dim = device.limits().max_texture_dimension_2d;
    let source_side = max_dim.min(8192);
    log(&format!(
        "limits: max_texture_dimension_2d = {max_dim}; probe source = {source_side}x{source_side}"
    ));

    // Phase 1 — async construction, check-before-consume.
    let mut blur = WgpuBlur::new(&device)
        .await
        .map_err(|e| format!("phase 1: WgpuBlur::new: {e}"))?;
    blur.prewarm_composite(&device, TARGET_FORMAT)
        .await
        .map_err(|e| format!("phase 1: prewarm_composite: {e}"))?;
    log("PHASE 1 PASS: async construction + composite prewarm on the WebGPU dispatch");

    // Probe-owned source (sampled) and offscreen target (composited into).
    let source_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe source"),
        size: wgpu::Extent3d {
            width: source_side,
            height: source_side,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let target_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe target"),
        size: wgpu::Extent3d {
            width: 1024,
            height: 1024,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let target_view = target_texture.create_view(&wgpu::TextureViewDescriptor::default());

    // Phase 2 — one clean small frame.
    let recorded = drive_frame(
        &mut blur,
        &device,
        &queue,
        &source_texture,
        source_side,
        &target_view,
        64,
    )
    .map_err(|e| format!("phase 2: {e}"))?;
    if !recorded {
        return Err("phase 2: the 64px frame unexpectedly prepared to a no-op".to_owned());
    }
    sleep_ms(150).await;
    if let Some(report) = blur.take_fault() {
        return Err(format!(
            "phase 2: clean frame reported a fault: {} (slot {:?}, occurrences {})",
            report.error, report.slot, report.occurrences
        ));
    }
    log("PHASE 2 PASS: clean frame prepared/recorded/submitted; take_fault() == None");

    // Phase 3 — accumulate distinct near-max scratch chains inside the retention window. Each
    // side is a distinct PingPongKey; one Gaussian chain at 8192 px is 2 × 8192² × 8 B
    // (Rgba16Float ping-pong) ≈ 1 GiB, so a handful of live keys exhausts a ~4 GiB device
    // without any single texture exceeding the dimension limit.
    let pressure_sides: Vec<u32> = (0..6).map(|i| source_side - 32 * i).collect();
    log(&format!(
        "phase 3: cycling region sides {pressure_sides:?} (each ≈ {:.2} GiB of ping-pong scratch)",
        f64::from(source_side) * f64::from(source_side) * 8.0 * 2.0 / (1024.0 * 1024.0 * 1024.0)
    ));
    let mut fault_report = None;
    let mut frames_driven = 0u32;
    'pressure: for _round in 0..10 {
        for &side in &pressure_sides {
            drive_frame(
                &mut blur,
                &device,
                &queue,
                &source_texture,
                source_side,
                &target_view,
                side,
            )
            .map_err(|e| format!("phase 3 (frame {frames_driven}): {e}"))?;
            frames_driven += 1;
            sleep_ms(40).await;
            if let Some(report) = blur.take_fault() {
                fault_report = Some(report);
                break 'pressure;
            }
        }
    }
    let Some(report) = fault_report else {
        return Err(format!(
            "phase 3: no fault reported after {frames_driven} pressure frames"
        ));
    };
    log(&format!(
        "PHASE 3 PASS: no panic under pressure; frame {frames_driven} surfaced: {} \
         (slot {:?}, occurrences {}, source: {})",
        report.error,
        report.slot,
        report.occurrences,
        std::error::Error::source(&report.error)
            .map(ToString::to_string)
            .unwrap_or_default()
    ));

    // Phase 4 — recovery: back to small regions; the big chains age out of retention, the
    // faulted slots recreate via fault-driven invalidation, and reads go clean.
    let mut clean_streak = 0u32;
    for frame in 0..40 {
        drive_frame(
            &mut blur,
            &device,
            &queue,
            &source_texture,
            source_side,
            &target_view,
            64,
        )
        .map_err(|e| format!("phase 4 (frame {frame}): {e}"))?;
        sleep_ms(40).await;
        match blur.take_fault() {
            Some(_) => clean_streak = 0,
            None => clean_streak += 1,
        }
        if clean_streak >= 8 {
            break;
        }
    }
    if clean_streak < 8 {
        return Err("phase 4: reads never settled clean after pressure dropped".to_owned());
    }
    log("PHASE 4 PASS: recovery — 8 consecutive clean frames after pressure dropped");

    // Phase 5 — the device survived every phase (fact 2 re-verified through the Rust wrapper).
    sleep_ms(250).await;
    if DEVICE_LOST.load(Ordering::SeqCst) {
        return Err("phase 5: the device-lost callback fired".to_owned());
    }
    log("PHASE 5 PASS: device survived all phases; device-lost callback never fired");

    Ok(())
}
