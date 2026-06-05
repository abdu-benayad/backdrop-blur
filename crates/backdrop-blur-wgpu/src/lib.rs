//! `backdrop-blur-wgpu` — the wgpu backend for [`backdrop_blur_core`]'s frosted-glass seam.
//!
//! It implements [`BackdropBlur`] with a safe, WGSL pipeline: a separable Gaussian blur (the
//! proven first-pixel path; the dual-Kawase down/up filter is a later, gated increment) followed
//! by a tinted, rounded-rect-masked composite. The crate is `#![forbid(unsafe_code)]`; the only
//! place that *could* want `unsafe` — the GPU-uniform `Pod` impls — uses bytemuck derives.
//!
//! # What the host provides
//!
//! The own-loop host renders its UI into an offscreen intermediate and hands it to [`prepare`]
//! as a [`SourceView`] — the texture view **plus its size and color space**, because a
//! `wgpu::TextureView` exposes neither and the backend needs both (the size to clip/scale, the
//! color space to know whether to sRGB-decode on sample — egui renders gamma-encoded, egui#3168).
//! The host owns the final [`Target`](BackdropBlur::Target); the backend owns only the internal
//! ping-pong scratch.
//!
//! # On error handling
//!
//! wgpu resource creation (textures, buffers, pipelines) does **not** return `Result` — OOM and
//! validation faults surface through the device's error handler, not synchronously — so this
//! backend cannot map them to [`BlurError::ResourceCreation`] without an async error-scope pass
//! (a candidate refinement). The error it *does* return synchronously is
//! [`BlurError::UnsupportedTarget`], checked against an allowlist before any GPU call.
//!
//! [`prepare`]: BackdropBlur::prepare
//! [`backdrop_blur_core`]: backdrop_blur_core
#![forbid(unsafe_code)]

use std::collections::HashMap;

use backdrop_blur_core::{BackdropBlur, BlurError, BlurRequest, BlurStage, ResolvedMask};
use wgpu::util::DeviceExt as _;

mod cache;
mod uniforms;

use cache::{
    PingPongKey, SCRATCH_FORMAT, backdrop_uv_remap, composite_encode_srgb, resolve_gaussian,
};
use uniforms::{CompositeParams, GaussianParams};

/// The color space of the backdrop the host hands in. egui renders **gamma-encoded** regardless
/// of texture format (egui#3168), so its intermediate is [`GammaSrgb`](Self::GammaSrgb) and must
/// be decoded before the linear-light convolution. A host that renders linear uses [`Linear`].
///
/// [`Linear`]: Self::Linear
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceColorSpace {
    /// sRGB gamma-encoded values (egui). Decoded to linear on sample.
    GammaSrgb,
    /// Already linear-light values. Sampled as-is.
    Linear,
}

/// The backdrop source: a sampleable view plus the two things a `wgpu::TextureView` cannot tell
/// the backend on its own — the texture's pixel size and its color space. The host constructs
/// one per frame from its offscreen intermediate; it owns the view for the call's duration.
pub struct SourceView {
    /// A sampleable view of the host's intermediate texture.
    pub view: wgpu::TextureView,
    /// The intermediate's `[width, height]` in physical pixels.
    pub size: [u32; 2],
    /// Whether the intermediate holds gamma-encoded or linear values.
    pub color_space: SourceColorSpace,
}

/// The owned, per-call handle from `prepare` to `record`: which scratch chain and composite
/// pipeline to use, plus the three bind groups (which already hold their textures/uniforms via
/// wgpu's internal refcounting). `generation` stamps the prepare so `record` can debug-assert
/// the v1 serial prepare→record contract (a stale handle would alias clobbered scratch — K1).
pub struct WgpuPrepared {
    key: PingPongKey,
    target_format: wgpu::TextureFormat,
    generation: u64,
    horizontal_bind: wgpu::BindGroup,
    vertical_bind: wgpu::BindGroup,
    composite_bind: wgpu::BindGroup,
}

/// One ping-pong scratch chain: two `Rgba16Float` texture views at the clipped source-region
/// size. The horizontal pass writes A (`views[0]`), the vertical pass writes B (`views[1]`), the
/// composite samples B. Only the views are stored — a `wgpu::TextureView` keeps its parent
/// texture alive by refcount, so the textures need no separate field.
struct ScratchChain {
    views: [wgpu::TextureView; 2],
}

/// The wgpu implementation of [`BackdropBlur`]. Holds the fixed pipeline machinery (bind-group
/// layout, sampler, Gaussian pipeline) and the per-`(size)` scratch + per-target-format composite
/// caches, so repeated frosted surfaces reuse them.
pub struct WgpuBlur {
    pipeline_layout: wgpu::PipelineLayout,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    gaussian_pipeline: wgpu::RenderPipeline,
    composite_shader: wgpu::ShaderModule,
    composite_pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    scratch: HashMap<PingPongKey, ScratchChain>,
    /// Bumped each `prepare`; stamped into [`WgpuPrepared`] so `record` can detect a stale handle.
    generation: u64,
}

// --- Constructors ---

impl WgpuBlur {
    /// Build the fixed pipeline machinery. The Gaussian pipeline (always writing the internal
    /// scratch format) is built now; composite pipelines are built lazily per target format.
    pub fn new(device: &wgpu::Device) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("backdrop-blur bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("backdrop-blur pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("backdrop-blur sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let gaussian_shader =
            device.create_shader_module(wgpu::include_wgsl!("shaders/gaussian.wgsl"));
        let composite_shader =
            device.create_shader_module(wgpu::include_wgsl!("shaders/composite.wgsl"));

        let gaussian_pipeline = build_pipeline(
            device,
            &pipeline_layout,
            &gaussian_shader,
            SCRATCH_FORMAT,
            None,
        );

        Self {
            pipeline_layout,
            bind_group_layout,
            sampler,
            gaussian_pipeline,
            composite_shader,
            composite_pipelines: HashMap::new(),
            scratch: HashMap::new(),
            generation: 0,
        }
    }
}

// --- Internal resource management ---

impl WgpuBlur {
    /// Create the two scratch textures for `key` if not already cached.
    fn ensure_scratch(&mut self, device: &wgpu::Device, key: PingPongKey) {
        if self.scratch.contains_key(&key) {
            return;
        }
        let make = |label: &str| {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: key.size[0],
                    height: key.size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: SCRATCH_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            // The view keeps the texture alive by refcount; the texture handle can drop here.
            texture.create_view(&wgpu::TextureViewDescriptor::default())
        };
        let view_a = make("backdrop-blur scratch A");
        let view_b = make("backdrop-blur scratch B");
        self.scratch.insert(
            key,
            ScratchChain {
                views: [view_a, view_b],
            },
        );
    }

    /// Build and cache the composite pipeline for `format` if not already present.
    fn ensure_composite_pipeline(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) {
        if self.composite_pipelines.contains_key(&format) {
            return;
        }
        let pipeline = build_pipeline(
            device,
            &self.pipeline_layout,
            &self.composite_shader,
            format,
            Some(over_blend()),
        );
        self.composite_pipelines.insert(format, pipeline);
    }

    /// One bind group: a sampled texture view + the shared sampler + a uniform buffer.
    fn bind(
        &self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        uniform: &wgpu::Buffer,
        label: &str,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform.as_entire_binding(),
                },
            ],
        })
    }
}

// --- The seam ---

impl BackdropBlur for WgpuBlur {
    type Device = wgpu::Device;
    type Queue = wgpu::Queue;
    type Encoder = wgpu::CommandEncoder;
    type SourceTexture = SourceView;
    type Target = wgpu::TextureView;
    type TargetFormat = wgpu::TextureFormat;
    type Prepared = WgpuPrepared;

    fn prepare(
        &mut self,
        device: &Self::Device,
        _queue: &Self::Queue,
        source: &Self::SourceTexture,
        target_format: Self::TargetFormat,
        request: &BlurRequest,
    ) -> Result<Option<Self::Prepared>, BlurError> {
        let Some(clipped) = request.source_region.clip_to(source.size) else {
            return Ok(None); // zero-area or fully-offscreen region → no-op
        };

        let encode_srgb =
            composite_encode_srgb(target_format).ok_or_else(|| BlurError::UnsupportedTarget {
                format: format!("{target_format:?}"),
            })?;
        let decode_srgb = matches!(source.color_space, SourceColorSpace::GammaSrgb);

        let kernel = resolve_gaussian(request.physical_blur_radius());
        let key = PingPongKey {
            size: clipped.size,
            levels: 1,
        };
        self.ensure_scratch(device, key);
        self.ensure_composite_pipeline(device, target_format);

        // Uniforms. Pass 1 maps the scratch onto the source sub-rect and decodes; pass 2 samples
        // the full (linear) scratch A.
        let [source_w, source_h] = [source.size[0] as f32, source.size[1] as f32];
        let [clip_x, clip_y] = [clipped.origin[0] as f32, clipped.origin[1] as f32];
        let [clip_w, clip_h] = [clipped.size[0] as f32, clipped.size[1] as f32];

        let horizontal = GaussianParams::new(
            [clip_x / source_w, clip_y / source_h],
            [clip_w / source_w, clip_h / source_h],
            [1.0 / source_w, 1.0 / source_h],
            [1.0, 0.0],
            kernel.sigma,
            kernel.tap_radius,
            decode_srgb,
        );
        let vertical = GaussianParams::new(
            [0.0, 0.0],
            [1.0, 1.0],
            [1.0 / clip_w, 1.0 / clip_h],
            [0.0, 1.0],
            kernel.sigma,
            kernel.tap_radius,
            false,
        );

        // Composite. The shader draws over the whole target and derives each fragment from
        // @builtin(position), so it needs the target rect in framebuffer px. `backdrop_uv_*`
        // maps target-rect uv onto scratch B, which holds the CLIPPED source region — identity
        // when the source was fully in-bounds, an inset when it was clipped at an edge, so the
        // frosted backdrop stays registered 1:1 with the content behind the glass.
        let (backdrop_uv_offset, backdrop_uv_scale) =
            backdrop_uv_remap(&request.source_region, &clipped);

        let mask = ResolvedMask::from_target(&request.target_rect, request.corner_radius);
        let tint = request.tint.color();
        let composite = CompositeParams::new(
            [
                request.target_rect.origin[0] as f32,
                request.target_rect.origin[1] as f32,
            ],
            [
                request.target_rect.size[0] as f32,
                request.target_rect.size[1] as f32,
            ],
            [tint.r(), tint.g(), tint.b(), tint.a()],
            backdrop_uv_offset,
            backdrop_uv_scale,
            mask.corner_radius_px,
            encode_srgb,
        );

        let horizontal_buf = uniform_buffer(device, &horizontal, "backdrop-blur gaussian-h");
        let vertical_buf = uniform_buffer(device, &vertical, "backdrop-blur gaussian-v");
        let composite_buf = uniform_buffer(device, &composite, "backdrop-blur composite");

        // The scratch was just inserted by ensure_scratch; a miss here is an internal fault, so
        // return the same typed error `record` uses rather than panicking (the contract is total).
        let chain = self
            .scratch
            .get(&key)
            .ok_or_else(|| BlurError::ResourceCreation {
                stage: BlurStage::PingPongTexture,
                source: "scratch chain missing immediately after ensure_scratch".into(),
            })?;
        let horizontal_bind = self.bind(
            device,
            &source.view,
            &horizontal_buf,
            "backdrop-blur h-bind",
        );
        let vertical_bind = self.bind(
            device,
            &chain.views[0],
            &vertical_buf,
            "backdrop-blur v-bind",
        );
        let composite_bind = self.bind(
            device,
            &chain.views[1],
            &composite_buf,
            "backdrop-blur composite-bind",
        );

        self.generation += 1;
        Ok(Some(WgpuPrepared {
            key,
            target_format,
            generation: self.generation,
            horizontal_bind,
            vertical_bind,
            composite_bind,
        }))
    }

    fn record(
        &self,
        encoder: &mut Self::Encoder,
        target: &Self::Target,
        prepared: &Self::Prepared,
    ) -> Result<(), BlurError> {
        // v1 is serial prepare→record per surface: this must be the most recent prepare, or its
        // shared scratch has already been clobbered by a newer one (K1). Debug-only — release
        // builds trust the contract.
        debug_assert_eq!(
            prepared.generation, self.generation,
            "record called with a stale Prepared (a newer prepare clobbered the shared scratch); \
             v1 requires serial prepare→record per surface (K1)"
        );
        let chain = self
            .scratch
            .get(&prepared.key)
            .ok_or_else(|| BlurError::ResourceCreation {
                stage: BlurStage::PingPongTexture,
                source: "scratch chain missing at record (prepare not called, or evicted)".into(),
            })?;
        let composite_pipeline = self
            .composite_pipelines
            .get(&prepared.target_format)
            .ok_or_else(|| BlurError::ResourceCreation {
                stage: BlurStage::CompositePipeline,
                source: "composite pipeline missing at record".into(),
            })?;

        // Pass 1 (horizontal): source → scratch A.
        self.blur_pass(
            encoder,
            &chain.views[0],
            &prepared.horizontal_bind,
            "backdrop-blur h-pass",
        );
        // Pass 2 (vertical): scratch A → scratch B.
        self.blur_pass(
            encoder,
            &chain.views[1],
            &prepared.vertical_bind,
            "backdrop-blur v-pass",
        );

        // Composite: scratch B → target, over the WHOLE attachment (default viewport). The
        // rounded-rect coverage forms every edge, so straight sides are anti-aliased and an
        // off-target rect cannot trip scissor validation; coverage 0 outside the panel keeps
        // LoadOp::Load content untouched. (A scissor to the panel + AA margin is a future perf
        // optimization once the host threads the target size in.)
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("backdrop-blur composite-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(composite_pipeline);
        pass.set_bind_group(0, &prepared.composite_bind, &[]);
        pass.draw(0..3, 0..1);
        Ok(())
    }
}

// --- Pass + buffer helpers ---

/// Create a UNIFORM buffer initialized with `value`'s bytes.
fn uniform_buffer<T: bytemuck::Pod>(device: &wgpu::Device, value: &T, label: &str) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::bytes_of(value),
        usage: wgpu::BufferUsages::UNIFORM,
    })
}

impl WgpuBlur {
    /// A full-attachment Gaussian pass (replace, no blend): clears then draws the oversized
    /// triangle into `attachment` using `bind`.
    fn blur_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        attachment: &wgpu::TextureView,
        bind: &wgpu::BindGroup,
        label: &str,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: attachment,
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
        });
        pass.set_pipeline(&self.gaussian_pipeline);
        pass.set_bind_group(0, bind, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// Standard non-premultiplied "over" blend for the composite.
fn over_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

/// Build a render pipeline from a shader module (a `vs_main`/`fs_main` pair) writing `format`,
/// with optional blend. Used for both the Gaussian and the composite pipelines.
fn build_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("backdrop-blur pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}
