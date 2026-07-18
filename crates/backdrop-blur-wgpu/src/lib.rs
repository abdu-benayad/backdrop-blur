//! `backdrop-blur-wgpu` — the wgpu backend for [`backdrop_blur_core`]'s frosted-glass seam.
//!
//! It implements [`BackdropBlur`] with a safe, WGSL pipeline: a **separable Gaussian** blur for
//! small radii and **dual-Kawase** (down/up-sample, the production-compositor algorithm) for large
//! radii, selected by a radius threshold, followed by a tinted, rounded-rect-masked composite. The
//! crate is `#![forbid(unsafe_code)]`; the only place that *could* want `unsafe` — the GPU-uniform
//! `Pod` impls — uses bytemuck derives.
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
//! wgpu resource creation (textures, buffers, pipelines, bind groups) does **not** return `Result`;
//! a fault is reported out-of-band through the device's error handler. This backend wraps every
//! creation it performs in a per-call [`OutOfMemory`](wgpu::ErrorFilter::OutOfMemory) error scope and
//! maps a captured allocation failure to [`BlurError::DeviceOutOfMemory`] (non-fatal kinds: the
//! sampler, and the primary allocations of buffers/textures) or [`BlurError::DeviceLost`] (fatal
//! kinds: layouts, shader modules, pipelines, bind groups — wgpu-core `device.lose()`s on their
//! rejected allocation, so the device is already gone when the error is returned). On the **native**
//! backend the fault is recorded synchronously during the create call, so the scope is read without
//! blocking, and the result is checked (`?`) before the handle is consumed — so an out-of-memory
//! handle never cascades into an uncatchable `Validation` error (**native-only**, see
//! [`BlurError::DeviceOutOfMemory`]).
//! Genuine validation/internal faults are crate bugs (this crate builds its own descriptors) and are
//! deliberately left to wgpu's default panic handler. The other error returned synchronously is
//! [`BlurError::UnsupportedTarget`], checked against an allowlist before any GPU call.
//!
//! [`prepare`]: BackdropBlur::prepare
//! [`backdrop_blur_core`]: backdrop_blur_core
#![forbid(unsafe_code)]

use std::collections::HashMap;

use backdrop_blur_core::{BackdropBlur, BlurError, BlurRequest, BlurStage, ResolvedMask};

mod cache;
mod uniforms;

use cache::{
    PingPongKey, RETENTION_FRAMES, SCRATCH_FORMAT, TargetEncoding, backdrop_uv_remap,
    composite_encode_srgb, evict_decision, kawase_halfpixel, kawase_level_size, resolve_gaussian,
    resolve_kawase_levels, use_dual_kawase,
};
use uniforms::{CompositeParams, GaussianParams, KawaseParams};

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

/// The owned, per-call handle from `prepare` to `record`: the resolved blur (which algorithm +
/// its bind groups), the composite bind group, and `generation` (so `record` can debug-assert the
/// serial prepare→record contract — a stale handle would alias clobbered scratch, K1). The bind
/// groups already hold their textures/uniforms via wgpu's internal refcounting.
pub struct WgpuPrepared {
    target_format: wgpu::TextureFormat,
    generation: u64,
    blur: PreparedBlur,
    composite_bind: wgpu::BindGroup,
}

/// The resolved blur for one surface: either the separable Gaussian (small radius) or dual-Kawase
/// (large radius). Each variant carries the bind groups its `record` passes replay; the keyed
/// scratch they target lives in [`WgpuBlur`].
enum PreparedBlur {
    /// Horizontal then vertical Gaussian into the 2-texture ping-pong; composite samples B.
    Gaussian {
        key: PingPongKey,
        horizontal_bind: wgpu::BindGroup,
        vertical_bind: wgpu::BindGroup,
    },
    /// Prefilter (decode+remap into mip 0) → `N` downsamples → `N` upsamples back to mip 0;
    /// composite samples mip 0.
    DualKawase {
        key: PingPongKey,
        prefilter_bind: wgpu::BindGroup,
        down_binds: Vec<wgpu::BindGroup>,
        up_binds: Vec<wgpu::BindGroup>,
    },
}

/// The two same-size `Rgba16Float` ping-pong views for the Gaussian path: the horizontal pass
/// writes A (`views[0]`), the vertical pass writes B (`views[1]`), the composite samples B. Only
/// the views are stored — a `wgpu::TextureView` keeps its parent texture alive by refcount.
struct ScratchChain {
    views: [wgpu::TextureView; 2],
    /// The frame this chain was last touched by `ensure_scratch`; drives last-frame-used eviction.
    last_used_frame: u64,
}

/// A dual-Kawase mip pyramid plus the frame it was last used: `N + 1` decreasing-size views (level 0
/// = full clipped size). The `last_used_frame` drives last-frame-used eviction, exactly as
/// [`ScratchChain`] does for the Gaussian path.
struct PyramidChain {
    views: Vec<wgpu::TextureView>,
    /// The frame this chain was last touched by `ensure_pyramid`; drives last-frame-used eviction.
    last_used_frame: u64,
}

/// The wgpu implementation of [`BackdropBlur`]. Holds the fixed pipeline machinery (bind-group
/// layout, sampler, Gaussian/downsample/upsample pipelines) and the per-`(size)` scratch
/// (Gaussian ping-pong + dual-Kawase pyramid) + per-target-format composite caches, so repeated
/// frosted surfaces reuse them.
pub struct WgpuBlur {
    pipeline_layout: wgpu::PipelineLayout,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    gaussian_pipeline: wgpu::RenderPipeline,
    downsample_pipeline: wgpu::RenderPipeline,
    upsample_pipeline: wgpu::RenderPipeline,
    composite_shader: wgpu::ShaderModule,
    composite_pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    scratch: HashMap<PingPongKey, ScratchChain>,
    /// Dual-Kawase mip pyramids: `N + 1` decreasing-size views, level 0 = full clipped size.
    pyramids: HashMap<PingPongKey, PyramidChain>,
    /// Advanced once per `prepare` ([`Self::begin_frame`]); the "now" last-frame-used eviction
    /// compares each chain's `last_used_frame` against, so a resized/moved surface's old-size chains
    /// are dropped instead of accumulating one per distinct size forever.
    frame: u64,
    /// Bumped each `prepare`; stamped into [`WgpuPrepared`] so `record` can detect a stale handle.
    generation: u64,
}

// --- Constructors ---

impl WgpuBlur {
    /// Build the fixed pipeline machinery. The Gaussian pipeline (always writing the internal
    /// scratch format) is built now; composite pipelines are built lazily per target format.
    ///
    /// Every creation is wrapped in a per-call [`OutOfMemory`](wgpu::ErrorFilter::OutOfMemory) error
    /// scope, so a device out-of-memory at construction returns an error instead of panicking
    /// (**native-only** — see the module-level error-handling note): [`BlurError::DeviceLost`] for
    /// the layout/shader/pipeline creations (fatal arms — wgpu has already invalidated the device),
    /// or [`BlurError::DeviceOutOfMemory`] for the sampler (the device survives). A genuine
    /// validation fault (a malformed shader or layout descriptor) is a crate bug and still panics via
    /// wgpu's default handler.
    pub fn new(device: &wgpu::Device) -> Result<Self, BlurError> {
        // wgpu-core 29.0.3: create_bind_group_layout routes solely through fatal handle_hal_error -> device.lose()
        let bind_group_layout = scoped_oom(device, OomOutcome::DeviceLost, || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
            })
        })?;

        // wgpu-core 29.0.3: create_pipeline_layout routes solely through fatal handle_hal_error -> device.lose()
        let pipeline_layout = scoped_oom(device, OomOutcome::DeviceLost, || {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("backdrop-blur pipeline layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            })
        })?;

        // wgpu-core 29.0.3: create_sampler non-fatal handler (resource.rs:2244)
        let sampler = scoped_oom(device, OomOutcome::Recoverable, || {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("backdrop-blur sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            })
        })?;

        // wgpu-core 29.0.3: create_shader_module routes solely through fatal handle_hal_error -> device.lose()
        let gaussian_shader = scoped_oom(device, OomOutcome::DeviceLost, || {
            device.create_shader_module(wgpu::include_wgsl!("shaders/gaussian.wgsl"))
        })?;
        let downsample_shader = scoped_oom(device, OomOutcome::DeviceLost, || {
            device.create_shader_module(wgpu::include_wgsl!("shaders/downsample.wgsl"))
        })?;
        let upsample_shader = scoped_oom(device, OomOutcome::DeviceLost, || {
            device.create_shader_module(wgpu::include_wgsl!("shaders/upsample.wgsl"))
        })?;
        let composite_shader = scoped_oom(device, OomOutcome::DeviceLost, || {
            device.create_shader_module(wgpu::include_wgsl!("shaders/composite.wgsl"))
        })?;

        // All blur passes write the internal scratch format with no blend; only the composite
        // matches the caller's format and blends.
        let gaussian_pipeline = build_pipeline(
            device,
            &pipeline_layout,
            &gaussian_shader,
            SCRATCH_FORMAT,
            None,
        )?;
        let downsample_pipeline = build_pipeline(
            device,
            &pipeline_layout,
            &downsample_shader,
            SCRATCH_FORMAT,
            None,
        )?;
        let upsample_pipeline = build_pipeline(
            device,
            &pipeline_layout,
            &upsample_shader,
            SCRATCH_FORMAT,
            None,
        )?;

        Ok(Self {
            pipeline_layout,
            bind_group_layout,
            sampler,
            gaussian_pipeline,
            downsample_pipeline,
            upsample_pipeline,
            composite_shader,
            composite_pipelines: HashMap::new(),
            scratch: HashMap::new(),
            pyramids: HashMap::new(),
            frame: 0,
            generation: 0,
        })
    }
}

// --- Internal resource management ---

impl WgpuBlur {
    /// Advance to the next frame and evict every scratch/pyramid chain untouched for
    /// [`RETENTION_FRAMES`]. Called once at the top of [`prepare`](BackdropBlur::prepare), before any
    /// `ensure_*`, so the chain a surface is about to use this frame is never evicted. Dropping a
    /// `HashMap` entry drops its `wgpu::TextureView`s, releasing the underlying textures by refcount
    /// — no explicit GPU free. The stale-set decision is the pure, core-shared [`evict_decision`].
    fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        let stale = evict_decision(
            self.scratch.iter().map(|(k, c)| (*k, c.last_used_frame)),
            self.frame,
            RETENTION_FRAMES,
        );
        for key in stale {
            self.scratch.remove(&key);
        }
        let stale = evict_decision(
            self.pyramids.iter().map(|(k, c)| (*k, c.last_used_frame)),
            self.frame,
            RETENTION_FRAMES,
        );
        for key in stale {
            self.pyramids.remove(&key);
        }
    }

    /// Create the two Gaussian ping-pong textures for `key` if not already cached, and mark the chain
    /// used this frame so eviction keeps it.
    fn ensure_scratch(&mut self, device: &wgpu::Device, key: PingPongKey) -> Result<(), BlurError> {
        if let Some(chain) = self.scratch.get_mut(&key) {
            chain.last_used_frame = self.frame;
            return Ok(());
        }
        let view_a = scratch_view(device, key.size, "backdrop-blur scratch A")?;
        let view_b = scratch_view(device, key.size, "backdrop-blur scratch B")?;
        self.scratch.insert(
            key,
            ScratchChain {
                views: [view_a, view_b],
                last_used_frame: self.frame,
            },
        );
        Ok(())
    }

    /// Create the dual-Kawase mip pyramid for `key` (`key.levels` = `N` → `N + 1` views, level 0
    /// at the full clipped size, level `i` halved) if not already cached.
    fn ensure_pyramid(&mut self, device: &wgpu::Device, key: PingPongKey) -> Result<(), BlurError> {
        if let Some(chain) = self.pyramids.get_mut(&key) {
            chain.last_used_frame = self.frame;
            return Ok(());
        }
        let views = (0..=key.levels)
            .map(|level| {
                let size = kawase_level_size(key.size, level);
                scratch_view(device, size, "backdrop-blur kawase mip")
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.pyramids.insert(
            key,
            PyramidChain {
                views,
                last_used_frame: self.frame,
            },
        );
        Ok(())
    }

    /// Build and cache the composite pipeline for `format` if not already present.
    fn ensure_composite_pipeline(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> Result<(), BlurError> {
        if self.composite_pipelines.contains_key(&format) {
            return Ok(());
        }
        let pipeline = build_pipeline(
            device,
            &self.pipeline_layout,
            &self.composite_shader,
            format,
            Some(over_blend()),
        )?;
        self.composite_pipelines.insert(format, pipeline);
        Ok(())
    }

    /// One bind group: a sampled texture view + the shared sampler + a uniform buffer.
    fn bind(
        &self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        uniform: &wgpu::Buffer,
        label: &str,
    ) -> Result<wgpu::BindGroup, BlurError> {
        // wgpu-core 29.0.3: create_bind_group routes solely through fatal handle_hal_error -> device.lose()
        scoped_oom(device, OomOutcome::DeviceLost, || {
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
        })
    }
}

// --- Test-support (gated) ---

#[cfg(feature = "image-snapshots")]
impl WgpuBlur {
    /// The number of cached scratch + pyramid chains. Exposed only under the `image-snapshots` test
    /// feature so the gated GPU tier can assert eviction actually bounds the cache as a surface is
    /// dragged/resized (the leak guard). Not part of the public API.
    pub fn cached_chain_count(&self) -> usize {
        self.scratch.len() + self.pyramids.len()
    }
}

// --- The seam ---

impl BackdropBlur for WgpuBlur {
    type Device = wgpu::Device;
    type Queue = wgpu::Queue;
    type CommandSink = wgpu::CommandEncoder;
    type SourceTexture = SourceView;
    type Target = wgpu::TextureView;
    type TargetSpec = wgpu::TextureFormat;
    type Prepared = WgpuPrepared;

    fn prepare(
        &mut self,
        device: &Self::Device,
        _queue: &Self::Queue,
        source: &Self::SourceTexture,
        target_spec: Self::TargetSpec,
        request: &BlurRequest,
    ) -> Result<Option<Self::Prepared>, BlurError> {
        let Some(clipped) = request.source_region.clip_to(source.size) else {
            return Ok(None); // zero-area or fully-offscreen region → no-op
        };
        // The target-side half of the same no-op: the composite shader divides by the target size
        // (composite.wgsl:57 — `rect_uv = (px - rect_origin_px) / rect_size_px`), so a zero dimension
        // would yield NaN/Inf UVs blended at ~50% alpha along a visible line. An empty target is the
        // same valid no-op as an empty source, not an error (DESIGN §9).
        if request.target_rect.is_empty() {
            return Ok(None);
        }

        // Advance the eviction clock and drop scratch chains untouched for RETENTION_FRAMES, before
        // ensuring this frame's chain — so a resized/moved surface's old-size chains are freed rather
        // than accumulating. Placed *after* the no-op guards, mirroring the glow backend (blur.rs:99):
        // the counter counts frosted frames, so a surface that clips to nothing does not age out a
        // chain it is about to reuse when it returns on-screen.
        self.begin_frame();

        let encode_srgb = matches!(
            composite_encode_srgb(target_spec).ok_or_else(|| BlurError::UnsupportedTarget {
                format: format!("{target_spec:?}"),
            })?,
            TargetEncoding::Srgb
        );
        let decode_srgb = matches!(source.color_space, SourceColorSpace::GammaSrgb);
        self.ensure_composite_pipeline(device, target_spec)?;

        let radius = request.physical_blur_radius();
        let [source_w, source_h] = [source.size[0] as f32, source.size[1] as f32];
        let [clip_x, clip_y] = [clipped.origin[0] as f32, clipped.origin[1] as f32];
        let [clip_w, clip_h] = [clipped.size[0] as f32, clipped.size[1] as f32];
        // Maps a full-scratch [0,1] onto the gamma source sub-rect (shared by the Gaussian
        // horizontal pass and the dual-Kawase prefilter, both of which sample the source).
        let remap_offset = [clip_x / source_w, clip_y / source_h];
        let remap_scale = [clip_w / source_w, clip_h / source_h];

        // The composite is identical for both algorithms; only the texture it samples differs
        // (Gaussian scratch B vs Kawase mip 0). `backdrop_uv_*` keeps a clipped source registered
        // 1:1 with the content behind the glass.
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
            request.presence.value(),
        );
        let composite_buf = uniform_buffer(device, &composite, "backdrop-blur composite")?;

        let (blur, composite_bind) = if use_dual_kawase(radius) {
            let levels = resolve_kawase_levels(radius);
            let key = PingPongKey {
                size: clipped.size,
                levels,
            };
            self.ensure_pyramid(device, key)?;
            let n = levels as usize;

            // Prefilter: source (gamma, sub-rect) → mip 0 (linear), via the Gaussian pipeline at
            // radius 0 — a pure decode + remap, no blur.
            let prefilter = GaussianParams::new(
                remap_offset,
                remap_scale,
                [1.0 / source_w, 1.0 / source_h],
                [1.0, 0.0],
                0.5,
                0,
                decode_srgb,
            );
            let prefilter_buf =
                uniform_buffer(device, &prefilter, "backdrop-blur kawase-prefilter")?;
            // Per-pass half-pixel offsets: each pass samples a known mip level.
            let down_bufs = (0..n)
                .map(|i| {
                    let hp = kawase_halfpixel(kawase_level_size(clipped.size, i as u32));
                    uniform_buffer(device, &KawaseParams::new(hp), "backdrop-blur kawase-down")
                })
                .collect::<Result<Vec<_>, _>>()?;
            let up_bufs = (0..n)
                .map(|j| {
                    let hp = kawase_halfpixel(kawase_level_size(clipped.size, (n - j) as u32));
                    uniform_buffer(device, &KawaseParams::new(hp), "backdrop-blur kawase-up")
                })
                .collect::<Result<Vec<_>, _>>()?;

            let pyramid = self
                .pyramids
                .get(&key)
                .ok_or_else(|| BlurError::ResourceCreation {
                    stage: BlurStage::PingPongTexture,
                    source: "kawase pyramid missing immediately after ensure_pyramid".into(),
                })?;
            let pyramid = &pyramid.views;
            let prefilter_bind = self.bind(
                device,
                &source.view,
                &prefilter_buf,
                "backdrop-blur prefilter-bind",
            )?;
            let down_binds = down_bufs
                .iter()
                .enumerate()
                .map(|(i, buf)| self.bind(device, &pyramid[i], buf, "backdrop-blur down-bind"))
                .collect::<Result<Vec<_>, _>>()?;
            let up_binds = up_bufs
                .iter()
                .enumerate()
                .map(|(j, buf)| self.bind(device, &pyramid[n - j], buf, "backdrop-blur up-bind"))
                .collect::<Result<Vec<_>, _>>()?;
            let composite_bind = self.bind(
                device,
                &pyramid[0],
                &composite_buf,
                "backdrop-blur composite-bind",
            )?;

            (
                PreparedBlur::DualKawase {
                    key,
                    prefilter_bind,
                    down_binds,
                    up_binds,
                },
                composite_bind,
            )
        } else {
            let kernel = resolve_gaussian(radius);
            let key = PingPongKey {
                size: clipped.size,
                levels: 1,
            };
            self.ensure_scratch(device, key)?;

            // Pass 1 maps the scratch onto the source sub-rect and decodes; pass 2 samples the
            // full (linear) scratch A.
            let horizontal = GaussianParams::new(
                remap_offset,
                remap_scale,
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
            let horizontal_buf = uniform_buffer(device, &horizontal, "backdrop-blur gaussian-h")?;
            let vertical_buf = uniform_buffer(device, &vertical, "backdrop-blur gaussian-v")?;

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
            )?;
            let vertical_bind = self.bind(
                device,
                &chain.views[0],
                &vertical_buf,
                "backdrop-blur v-bind",
            )?;
            let composite_bind = self.bind(
                device,
                &chain.views[1],
                &composite_buf,
                "backdrop-blur composite-bind",
            )?;

            (
                PreparedBlur::Gaussian {
                    key,
                    horizontal_bind,
                    vertical_bind,
                },
                composite_bind,
            )
        };

        self.generation += 1;
        Ok(Some(WgpuPrepared {
            target_format: target_spec,
            generation: self.generation,
            blur,
            composite_bind,
        }))
    }

    fn record(
        &self,
        sink: &mut Self::CommandSink,
        target: &Self::Target,
        prepared: Self::Prepared,
    ) -> Result<(), BlurError> {
        // `record` consumes the handle, so double-record is a compile error. The one hazard left
        // to guard: a newer prepare clobbered the shared scratch before this handle was recorded
        // (K1 serial contract). Debug-only — release builds trust the contract.
        debug_assert_eq!(
            prepared.generation, self.generation,
            "Prepared is stale: a newer prepare clobbered the shared scratch before this handle \
             was recorded; v1 requires serial prepare→record per surface (K1)"
        );
        let composite_pipeline = self
            .composite_pipelines
            .get(&prepared.target_format)
            .ok_or_else(|| BlurError::ResourceCreation {
                stage: BlurStage::CompositePipeline,
                source: "composite pipeline missing at record".into(),
            })?;

        // Blur into the scratch (Gaussian ping-pong, or the dual-Kawase pyramid).
        self.record_blur(sink, &prepared.blur)?;

        // Composite: the final blurred texture → target, over the WHOLE attachment (default
        // viewport). The
        // rounded-rect coverage forms every edge, so straight sides are anti-aliased and an
        // off-target rect cannot trip scissor validation; coverage 0 outside the panel keeps
        // LoadOp::Load content untouched. (A scissor to the panel + AA margin is a future perf
        // optimization once the host threads the target size in.)
        let mut pass = sink.begin_render_pass(&wgpu::RenderPassDescriptor {
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

// --- Error-scope + buffer helpers ---

/// Poll a future exactly once and return its output if it is already ready. On the native wgpu
/// backend an error scope's `pop()` future is always already-resolved (the fault was recorded
/// synchronously during the create call), so a single poll suffices — no executor, no blocking. A
/// `Pending` result means the non-native (deferred-promise) path, which this crate does not support.
fn poll_once<F: std::future::Future>(fut: F) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => Some(v),
        std::task::Poll::Pending => None,
    }
}

/// Whether a rejected allocation at a `scoped_oom` site leaves the device alive. wgpu-core routes
/// each creation's fault through one of two handlers: non-fatal (the allocation fails, the device
/// survives) or fatal (`device.lose()` before the error is returned — the device is permanently
/// invalid). Which handler fires is a wgpu-core internal invisible in the returned error, so every
/// call site must state its arm explicitly; the mapping is a static claim about wgpu-core 29.0.3,
/// guarded by the `wgpu_core_version_pin` tripwire test.
enum OomOutcome {
    /// The allocation's handler skips `lose()` on out-of-memory: report
    /// [`BlurError::DeviceOutOfMemory`], the device survives, the host may retry.
    Recoverable,
    /// The allocation's handler calls `lose()` before returning: report [`BlurError::DeviceLost`],
    /// the device is already gone at return, the host must tear down.
    DeviceLost,
}

/// Fold an error and its `source()` chain into one `": "`-joined string. `wgpu::Error`'s `Display`
/// is a bare constant (`"Out of Memory"`); the resource that faulted lives one level down, in
/// wgpu-core's `ContextError` source (the API call + descriptor label). A plain `String` boxed as
/// the backend-error source is chain-terminal, so the chain is flattened into the message here —
/// keeping that diagnostic while staying `Send + Sync` on wasm, where the live `wgpu::Error` is not.
fn describe(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut cause = err.source();
    while let Some(c) = cause {
        parts.push(c.to_string());
        cause = c.source();
    }
    parts.join(": ")
}

/// Run `create` inside an [`OutOfMemory`](wgpu::ErrorFilter::OutOfMemory) error scope; route a
/// captured allocation failure per `outcome` — [`BlurError::DeviceOutOfMemory`] where the device
/// survives the rejection, [`BlurError::DeviceLost`] where wgpu-core has already marked the device
/// invalid (`device.lose()`) inside the create call. On the `DeviceLost` arm the device is gone at
/// return; that is the capture instant, not a re-checked liveness status. Only out-of-memory is
/// scoped; validation and internal faults are crate bugs left to wgpu's default panic handler.
/// Native-only (see [`poll_once`]). The result MUST be checked (`?`) before it is consumed by any
/// later call — an out-of-memory handle consumed downstream raises an uncatchable `Validation`
/// error (wgpu's contagious invalidity). Push, create, and pop run on one thread with nothing
/// yielding between, because a [`wgpu::ErrorScopeGuard`] is thread-local (`!Send`).
fn scoped_oom<T>(
    device: &wgpu::Device,
    outcome: OomOutcome,
    create: impl FnOnce() -> T,
) -> Result<T, BlurError> {
    let scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let resource = create();
    match poll_once(scope.pop()) {
        Some(None) => Ok(resource),
        Some(Some(err)) => Err(match outcome {
            OomOutcome::Recoverable => BlurError::DeviceOutOfMemory {
                source: describe(&err).into(),
            },
            OomOutcome::DeviceLost => BlurError::DeviceLost {
                source: describe(&err).into(),
            },
        }),
        None => panic!(
            "backdrop-blur: OOM error scope did not resolve synchronously; native-only path (design v5)"
        ),
    }
}

/// Create a UNIFORM buffer initialized with `value`'s bytes, inside an `OutOfMemory` error scope.
///
/// Deliberately does **not** use `create_buffer_init`: that helper maps-at-creation and calls
/// `get_mapped_range_mut` on the returned buffer, which on an out-of-memory-invalidated buffer hits a
/// fatal panic path that bypasses the error scope entirely (before the scope is ever read). Instead
/// the mapped buffer is created inside the scope and checked (`?`), and only mapped/written on
/// success — so the `?` short-circuits before the fatal map on out-of-memory. The padding matches
/// `create_buffer_init` exactly (round up to `COPY_BUFFER_ALIGNMENT`, min one alignment).
fn uniform_buffer<T: bytemuck::Pod>(
    device: &wgpu::Device,
    value: &T,
    label: &str,
) -> Result<wgpu::Buffer, BlurError> {
    let contents = bytemuck::bytes_of(value);
    let unpadded_size = contents.len() as wgpu::BufferAddress;
    let align_mask = wgpu::COPY_BUFFER_ALIGNMENT - 1;
    let padded_size = ((unpadded_size + align_mask) & !align_mask).max(wgpu::COPY_BUFFER_ALIGNMENT);
    // MIXED: primary alloc non-fatal, but the mapped_at_creation path's internal StagingBuffer::new
    // is fatal on OOM; tagged Recoverable per approval decision (d), backstopped by the host's
    // device-lost callback — see the DeviceOutOfMemory rustdoc.
    let buffer = scoped_oom(device, OomOutcome::Recoverable, || {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: padded_size,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: true,
        })
    })?;
    buffer
        .get_mapped_range_mut(..)
        .slice(..unpadded_size as usize)
        .copy_from_slice(contents);
    buffer.unmap();
    Ok(buffer)
}

/// A `[width, height]` `SCRATCH_FORMAT` texture usable as both a sampled source and a render
/// target, returned as a view (the view keeps the texture alive by refcount). The texture creation
/// is scoped for out-of-memory; the view creation is not a memory allocation and is left unscoped
/// (a documented low-risk residual).
fn scratch_view(
    device: &wgpu::Device,
    size: [u32; 2],
    label: &str,
) -> Result<wgpu::TextureView, BlurError> {
    // MIXED: primary alloc non-fatal, but RENDER_ATTACHMENT's internal clear-view creation is
    // fatal on OOM; tagged Recoverable per approval decision (d), backstopped by the host's
    // device-lost callback — see the DeviceOutOfMemory rustdoc.
    let texture = scoped_oom(device, OomOutcome::Recoverable, || {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SCRATCH_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    })?;
    Ok(texture.create_view(&wgpu::TextureViewDescriptor::default()))
}

impl WgpuBlur {
    /// Replay a prepared blur's passes into its scratch: the Gaussian horizontal/vertical, or the
    /// dual-Kawase prefilter → downsamples → upsamples back to mip 0.
    fn record_blur(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        blur: &PreparedBlur,
    ) -> Result<(), BlurError> {
        let missing = || BlurError::ResourceCreation {
            stage: BlurStage::PingPongTexture,
            source: "scratch missing at record (prepare not called, or evicted)".into(),
        };
        match blur {
            PreparedBlur::Gaussian {
                key,
                horizontal_bind,
                vertical_bind,
            } => {
                let chain = self.scratch.get(key).ok_or_else(missing)?;
                self.blur_pass(
                    encoder,
                    &chain.views[0],
                    horizontal_bind,
                    &self.gaussian_pipeline,
                    "backdrop-blur h-pass",
                );
                self.blur_pass(
                    encoder,
                    &chain.views[1],
                    vertical_bind,
                    &self.gaussian_pipeline,
                    "backdrop-blur v-pass",
                );
            }
            PreparedBlur::DualKawase {
                key,
                prefilter_bind,
                down_binds,
                up_binds,
            } => {
                let pyramid = self.pyramids.get(key).ok_or_else(missing)?;
                let pyramid = &pyramid.views;
                let n = key.levels as usize;
                // Prefilter: source (gamma, sub-rect) → mip 0 (linear), via the Gaussian pipeline
                // at radius 0 (a pure decode + remap).
                self.blur_pass(
                    encoder,
                    &pyramid[0],
                    prefilter_bind,
                    &self.gaussian_pipeline,
                    "backdrop-blur kawase-prefilter",
                );
                // Downsample i: mip[i] → mip[i+1].
                for (i, bind) in down_binds.iter().enumerate() {
                    self.blur_pass(
                        encoder,
                        &pyramid[i + 1],
                        bind,
                        &self.downsample_pipeline,
                        "backdrop-blur kawase-down",
                    );
                }
                // Upsample j: mip[n-j] → mip[n-1-j], ending at mip 0.
                for (j, bind) in up_binds.iter().enumerate() {
                    self.blur_pass(
                        encoder,
                        &pyramid[n - 1 - j],
                        bind,
                        &self.upsample_pipeline,
                        "backdrop-blur kawase-up",
                    );
                }
            }
        }
        Ok(())
    }

    /// A full-attachment blur pass (replace, no blend): clears then draws the oversized triangle
    /// into `attachment` using `bind` and `pipeline`.
    fn blur_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        attachment: &wgpu::TextureView,
        bind: &wgpu::BindGroup,
        pipeline: &wgpu::RenderPipeline,
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
        pass.set_pipeline(pipeline);
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
/// with optional blend. Used for both the Gaussian and the composite pipelines. Scoped for
/// out-of-memory; a genuine validation fault (bad shader/layout) is a crate bug left to panic.
fn build_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
) -> Result<wgpu::RenderPipeline, BlurError> {
    // wgpu-core 29.0.3: create_render_pipeline routes solely through fatal handle_hal_error -> device.lose()
    scoped_oom(device, OomOutcome::DeviceLost, || {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_once_yields_a_ready_future() {
        // The native error-scope case: `pop()` is already resolved, so one poll returns its value.
        assert_eq!(poll_once(std::future::ready(7u32)), Some(7));
    }

    #[test]
    fn poll_once_reports_pending_as_none() {
        // The non-native (deferred) case `scoped_oom` panics on: a never-ready future polls to None.
        assert_eq!(poll_once(std::future::pending::<u32>()), None);
    }
}
