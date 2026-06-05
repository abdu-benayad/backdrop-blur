//! The own-loop adapter: drive `egui-winit` + `egui-wgpu` directly (not eframe), render the UI
//! into an offscreen intermediate, then blur a region and composite the frosted surface into the
//! target — in the one order that does not panic (DESIGN §6, M4-corrected).

use backdrop_blur_core::{
    BackdropBlur, BlurError, BlurRequest, BlurStrength, CornerRadius, Region, RepaintPolicy, Scale,
    Tint,
};
use backdrop_blur_wgpu::{SourceColorSpace, SourceView, WgpuBlur};

/// A frosted surface to composite this frame: an egui-space rectangle (logical points) plus the
/// glass parameters and a liveness policy. v1 treats the backdrop directly behind the rect as the
/// blur source (`source_region == target_rect`).
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    /// Where the surface sits, in egui logical points.
    pub rect: egui::Rect,
    /// How much blur.
    pub strength: BlurStrength,
    /// The glass film.
    pub tint: Tint,
    /// How round the corners are.
    pub corner_radius: CornerRadius,
    /// How often the backdrop must be refreshed (drives `request_repaint`).
    pub repaint: RepaintPolicy,
}

impl Surface {
    /// Resolve to a physical-pixel [`BlurRequest`]. The egui rect (points) scales by
    /// `pixels_per_point`; the backdrop behind the surface is the same screen area.
    fn request(&self, pixels_per_point: f32) -> BlurRequest {
        let origin = [
            (self.rect.min.x * pixels_per_point).round().max(0.0) as u32,
            (self.rect.min.y * pixels_per_point).round().max(0.0) as u32,
        ];
        let size = [
            (self.rect.width() * pixels_per_point).round().max(0.0) as u32,
            (self.rect.height() * pixels_per_point).round().max(0.0) as u32,
        ];
        let region = Region {
            origin,
            size,
            scale: Scale::new(pixels_per_point),
        };
        BlurRequest {
            source_region: region,
            target_rect: region,
            strength: self.strength,
            tint: self.tint,
            corner_radius: self.corner_radius,
        }
    }
}

/// The strongest repaint obligation across a set of surfaces: `Live` wins, then the shortest
/// `Bounded` interval, else `Static`. The host applies this to its egui `Context`.
pub fn strongest_repaint(surfaces: &[Surface]) -> RepaintPolicy {
    surfaces
        .iter()
        .fold(RepaintPolicy::Static, |acc, s| match (acc, s.repaint) {
            (RepaintPolicy::Live, _) | (_, RepaintPolicy::Live) => RepaintPolicy::Live,
            (RepaintPolicy::Bounded(a), RepaintPolicy::Bounded(b)) => {
                RepaintPolicy::Bounded(a.min(b))
            }
            (RepaintPolicy::Bounded(d), RepaintPolicy::Static)
            | (RepaintPolicy::Static, RepaintPolicy::Bounded(d)) => RepaintPolicy::Bounded(d),
            (RepaintPolicy::Static, RepaintPolicy::Static) => RepaintPolicy::Static,
        })
}

/// The per-frame GPU handles a blur needs, bundled so the surface loop stays legible. Generic
/// over the backend `B`, so a test can build one entirely from `()` and run headlessly.
pub(crate) struct SeamContext<'a, B: BackdropBlur> {
    pub device: &'a B::Device,
    pub queue: &'a B::Queue,
    pub encoder: &'a mut B::Encoder,
    pub source: &'a B::SourceTexture,
    pub target: &'a B::Target,
    pub target_format: B::TargetFormat,
}

/// The backend-agnostic core of the adapter: for each surface, `prepare` the blur and `record` it
/// when the region is non-empty. Generic over the backend so it is exercised headlessly by a
/// recording fake in tests (the real egui rendering around it needs a GPU; this mapping does not).
///
/// Returns the number of surfaces that recorded a blur (a clipped-to-nothing surface prepares to
/// `Ok(None)` and records nothing).
pub(crate) fn composite_surfaces<B>(
    blur: &mut B,
    ctx: SeamContext<'_, B>,
    surfaces: &[Surface],
    pixels_per_point: f32,
) -> Result<usize, BlurError>
where
    B: BackdropBlur,
    B::TargetFormat: Copy,
{
    let mut recorded = 0;
    for surface in surfaces {
        let request = surface.request(pixels_per_point);
        if let Some(prepared) = blur.prepare(
            ctx.device,
            ctx.queue,
            ctx.source,
            ctx.target_format,
            &request,
        )? {
            blur.record(ctx.encoder, ctx.target, &prepared)?;
            recorded += 1;
        }
    }
    Ok(recorded)
}

/// The offscreen intermediate egui renders into (the blur source). Cached and resized to the
/// screen; its format matches the target so one `egui-wgpu::Renderer` serves both.
struct Intermediate {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: [u32; 2],
}

/// Drives one own-loop frame for an `egui-winit` + `egui-wgpu` host: it renders the egui UI into
/// the intermediate (the blur source) and the target (the display), then blurs and composites the
/// frosted surfaces over the target — all on one encoder with a single submit.
pub struct OwnLoopRenderer {
    renderer: egui_wgpu::Renderer,
    target_format: wgpu::TextureFormat,
    intermediate: Option<Intermediate>,
}

impl OwnLoopRenderer {
    /// Build the adapter for a host whose target (swapchain) has `target_format`. v1 expects a
    /// non-sRGB `Unorm` format (egui writes gamma-encoded values, egui#3168); the composite
    /// re-encodes to match.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let renderer =
            egui_wgpu::Renderer::new(device, target_format, egui_wgpu::RendererOptions::default());
        Self {
            renderer,
            target_format,
            intermediate: None,
        }
    }

    /// Recreate the intermediate if the screen size changed; return its view.
    fn intermediate_view(&mut self, device: &wgpu::Device, size: [u32; 2]) -> &wgpu::TextureView {
        let stale = self.intermediate.as_ref().is_none_or(|i| i.size != size);
        if stale {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("backdrop-blur egui intermediate"),
                size: wgpu::Extent3d {
                    width: size[0].max(1),
                    height: size[1].max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.target_format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.intermediate = Some(Intermediate {
                texture,
                view,
                size,
            });
        }
        &self
            .intermediate
            .as_ref()
            .expect("intermediate set above")
            .view
    }

    /// Render one frosted frame. `frame` carries the tessellated egui output + the target; the
    /// returned [`RepaintPolicy`] is the host's to apply to its `Context` (§4.6).
    pub fn render_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        blur: &mut WgpuBlur,
        frame: FrameInput<'_>,
        surfaces: &[Surface],
    ) -> Result<RepaintPolicy, BlurError> {
        // 1. Texture deltas first.
        for (id, delta) in &frame.textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("backdrop-blur own-loop frame"),
        });

        // 2. Upload vertex/index/uniform buffers; keep the returned command buffers to submit
        //    BEFORE the main encoder (egui-wgpu does not auto-submit them).
        let egui_buffers = self.renderer.update_buffers(
            device,
            queue,
            &mut encoder,
            frame.paint_jobs,
            &frame.screen,
        );

        // 3 + 4. Render egui into the intermediate (blur source) and into the target (display).
        //        Each render pass is scoped and dropped before the encoder is touched again — a
        //        live `forget_lifetime` pass plus an encoder op is a runtime panic (M4).
        let size = frame.screen.size_in_pixels;
        {
            let view = self.intermediate_view(device, size);
            let mut pass = begin_clear_pass(&mut encoder, view, "backdrop-blur egui→intermediate");
            self.renderer
                .render(&mut pass, frame.paint_jobs, &frame.screen);
        }
        {
            let mut pass =
                begin_clear_pass(&mut encoder, frame.target, "backdrop-blur egui→target");
            self.renderer
                .render(&mut pass, frame.paint_jobs, &frame.screen);
        }

        // 5. Blur + composite each surface, sampling the intermediate, writing the target.
        let source = SourceView {
            view: self
                .intermediate
                .as_ref()
                .expect("intermediate set above")
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default()),
            size,
            color_space: SourceColorSpace::GammaSrgb,
        };
        composite_surfaces(
            blur,
            SeamContext {
                device,
                queue,
                encoder: &mut encoder,
                source: &source,
                target: frame.target,
                target_format: self.target_format,
            },
            surfaces,
            frame.screen.pixels_per_point,
        )?;

        // 6. One submit: egui's upload buffers, then the main encoder.
        let main = encoder.finish();
        queue.submit(egui_buffers.into_iter().chain(std::iter::once(main)));

        // Free textures egui dropped this frame.
        for id in &frame.textures_delta.free {
            self.renderer.free_texture(id);
        }

        Ok(strongest_repaint(surfaces))
    }
}

/// One frame's egui output plus where to draw it.
pub struct FrameInput<'a> {
    /// The display target (swapchain view); must have the adapter's `target_format`.
    pub target: &'a wgpu::TextureView,
    /// The tessellated egui primitives for this frame.
    pub paint_jobs: &'a [egui::ClippedPrimitive],
    /// The textures egui created/freed this frame.
    pub textures_delta: &'a egui::TexturesDelta,
    /// Screen size (physical px) + pixels-per-point.
    pub screen: egui_wgpu::ScreenDescriptor,
}

/// Begin a render pass that clears `view`, returning a `'static` pass (egui-wgpu's `render`
/// requires `RenderPass<'static>`). The caller must drop it before reusing the encoder.
fn begin_clear_pass(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    label: &str,
) -> wgpu::RenderPass<'static> {
    encoder
        .begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        })
        .forget_lifetime()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A recording fake backend: all associated types are `()`, so the surface→prepare/record
    /// wiring runs with no GPU. It returns `Ok(None)` for a zero-area region (the no-op), mirroring
    /// the real backend's clip behavior, so the test can assert "prepare always, record only when
    /// the region is non-empty".
    #[derive(Default)]
    struct RecordingBlur {
        events: RefCell<Vec<&'static str>>,
    }

    impl BackdropBlur for RecordingBlur {
        type Device = ();
        type Queue = ();
        type Encoder = ();
        type SourceTexture = ();
        type Target = ();
        type TargetFormat = ();
        type Prepared = ();

        fn prepare(
            &mut self,
            _device: &(),
            _queue: &(),
            _source: &(),
            _target_format: (),
            request: &BlurRequest,
        ) -> Result<Option<()>, BlurError> {
            self.events.borrow_mut().push("prepare");
            if request.source_region.size[0] == 0 || request.source_region.size[1] == 0 {
                Ok(None)
            } else {
                Ok(Some(()))
            }
        }

        fn record(&self, _encoder: &mut (), _target: &(), _prepared: &()) -> Result<(), BlurError> {
            self.events.borrow_mut().push("record");
            Ok(())
        }
    }

    fn surface(rect: egui::Rect) -> Surface {
        Surface {
            rect,
            strength: BlurStrength::new(8.0),
            tint: Tint::new(backdrop_blur_core::LinearRgba::new(0.0, 0.0, 0.0, 0.1)),
            corner_radius: CornerRadius::new(12.0),
            repaint: RepaintPolicy::Static,
        }
    }

    #[test]
    fn composite_surfaces_prepares_each_and_records_only_non_empty() {
        let mut blur = RecordingBlur::default();
        let surfaces = [
            surface(egui::Rect::from_min_size(
                egui::pos2(10.0, 10.0),
                egui::vec2(100.0, 60.0),
            )),
            surface(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(0.0, 0.0),
            )), // empty → no-op
            surface(egui::Rect::from_min_size(
                egui::pos2(50.0, 50.0),
                egui::vec2(80.0, 40.0),
            )),
        ];
        let recorded = composite_surfaces(
            &mut blur,
            SeamContext {
                device: &(),
                queue: &(),
                encoder: &mut (),
                source: &(),
                target: &(),
                target_format: (),
            },
            &surfaces,
            1.0,
        )
        .expect("the fake backend never errors");

        assert_eq!(recorded, 2);
        let events = blur.events.into_inner();
        assert_eq!(
            events.iter().filter(|e| **e == "prepare").count(),
            3,
            "prepare runs for every surface"
        );
        assert_eq!(
            events.iter().filter(|e| **e == "record").count(),
            2,
            "record skips the empty surface"
        );
        // Order proof: the empty surface prepares but does not record between the two real ones.
        assert_eq!(
            events,
            vec!["prepare", "record", "prepare", "prepare", "record"]
        );
    }

    #[test]
    fn strongest_repaint_prefers_live_then_shortest_bounded() {
        use std::time::Duration;
        let live = surface(egui::Rect::ZERO);
        let mut live = live;
        live.repaint = RepaintPolicy::Live;

        let mut bounded_long = surface(egui::Rect::ZERO);
        bounded_long.repaint = RepaintPolicy::Bounded(Duration::from_millis(500));
        let mut bounded_short = surface(egui::Rect::ZERO);
        bounded_short.repaint = RepaintPolicy::Bounded(Duration::from_millis(100));

        assert_eq!(strongest_repaint(&[]), RepaintPolicy::Static);
        assert_eq!(
            strongest_repaint(&[bounded_long, bounded_short]),
            RepaintPolicy::Bounded(Duration::from_millis(100))
        );
        assert_eq!(
            strongest_repaint(&[bounded_long, live]),
            RepaintPolicy::Live
        );
    }
}
