//! The seam — the one trait each GPU backend implements. It is **two-phase** (`prepare` then
//! `record`) because the backends demand it: wgpu uploads uniforms/textures through its
//! **Queue** (not the encoder), so the upload phase needs the queue and the record phase needs
//! only the encoder; glow is immediate-mode (`prepare` grabs + uploads via the context,
//! `record` draws). A producer method `grab_source` reserves the glow grab-pass socket now,
//! so adding that backend later is not a core rewrite (DESIGN §4.4).
//!
//! The associated types are each backend's resource universe — distinct per backend, which is
//! exactly why the trait is **not object-safe** and backends are **separate crates** (the
//! `wgpu-types` → `wgpu-hal` model). Dispatch is static, monomorphized.
//!
//! # Gate verdict (IMPL §1d): the trait is kept
//!
//! Before committing to this seam, the divergent backend was sketched against it: the
//! `examples/glow-gate` crate maps the proven immediate-mode glow pipeline onto every
//! associated type and method here, and **compiles**. Each type binds to a real glow type
//! (`Device`/`Encoder` → `glow::Context`, `SourceTexture` → `glow::Texture`, …); the one `()`
//! (`Queue`) is honest because glow uploads through its context rather than a queue; and the
//! grab + origin flip live entirely inside `grab_source`, so no extra method is forced. v1
//! therefore ships **with** this trait, not a concrete one-backend pair. See that crate's
//! module docs for the full §3 decision table.

use crate::{BlurError, BlurRequest, Region};

/// Implemented once per GPU backend (`WgpuBlur` in v1; `GlowBlur` later). The implementor holds
/// the backend's cached resources — per-`(size, levels)` ping-pong chains and the pipelines —
/// across frames, so repeated frosted surfaces do not rebuild them.
///
/// # Lifecycle contract (v1)
///
/// - **Serial `prepare` → `record` per surface.** v1 scope is a single frosted surface over a
///   once-rendered backdrop; because the ping-pong scratch is shared, two
///   surfaces are not prepared-then-both-recorded — each is prepared and recorded before the
///   next. The [`Prepared`](Self::Prepared) handle is **owned** (it borrows nothing from the
///   blurrer), so the contract does not rely on "record immediately follows prepare"; genuine
///   multi-surface batching is deferred future work.
/// - **Single-threaded, frame-serial.** `prepare` takes `&mut self`, `record` takes `&self`.
///   The blurrer is **not** required to be `Send`/`Sync` in v1; a multi-threaded render loop
///   owns one per render thread.
pub trait BackdropBlur {
    /// The GPU device (`wgpu::Device`; `glow::Context`).
    type Device;
    /// The upload queue (`wgpu::Queue`; `()` for glow, which uploads via the context).
    type Queue;
    /// The command sink (`wgpu::CommandEncoder`; `glow::Context`, the immediate-mode handle).
    type Encoder;
    /// The grab source for the grab-pass path (a glow framebuffer; `()` and unused on wgpu).
    type Framebuffer;
    /// A sampleable backdrop (`wgpu::TextureView`; `glow::Texture`).
    type SourceTexture;
    /// The composite destination (`wgpu::TextureView`; a glow framebuffer).
    type Target;
    /// The target's color format (`wgpu::TextureFormat`; a GLES internal-format enum). Passed
    /// to `prepare` because wgpu bakes the fragment-target format into the composite pipeline
    /// at creation, so the pipeline is cached per format.
    type TargetFormat;
    /// An **owned**, opaque per-call handle carrying the resolved payload (kernel offsets,
    /// tint, [`ResolvedMask`](crate::ResolvedMask), target rect, and the resource keys) from
    /// `prepare` to `record`. Owned — it borrows nothing from the blurrer.
    type Prepared;

    /// Produce a sampleable backdrop source.
    ///
    /// - **Own-loop (wgpu):** the host's offscreen intermediate is already sampleable, so the
    ///   backend hands it through (`Framebuffer = ()` unused).
    /// - **Grab-pass (glow):** blit + MSAA-resolve out of the live `framebuffer` for `region` —
    ///   backend-specific GL the host cannot do generically. This is the socket that keeps the
    ///   glow backend purely additive.
    fn grab_source(
        &mut self,
        device: &Self::Device,
        queue: &Self::Queue,
        framebuffer: &Self::Framebuffer,
        region: Region,
    ) -> Result<Self::SourceTexture, BlurError>;

    /// **Phase 1** — holds the device + queue. Allocates and keys the ping-pong chain, lazily
    /// builds and caches pipelines (the fixed-scratch down/up pipelines once; the composite
    /// pipeline per `target_format`), and resolves the payload into an owned
    /// [`Prepared`](Self::Prepared).
    ///
    /// Returns `Ok(None)` when `request.source_region` is zero-sized or fully offscreen — a
    /// **no-op**, valid input rather than an error (see [`Region::is_empty_or_offscreen`]);
    /// `record` is then simply not called. Returns `Err` only on a real GPU fault.
    ///
    /// [`Region::is_empty_or_offscreen`]: crate::Region::is_empty_or_offscreen
    fn prepare(
        &mut self,
        device: &Self::Device,
        queue: &Self::Queue,
        source: &Self::SourceTexture,
        target_format: Self::TargetFormat,
        request: &BlurRequest,
    ) -> Result<Option<Self::Prepared>, BlurError>;

    /// **Phase 2** — holds only the encoder + target. Records downsample → upsample → composite
    /// for a `prepared` produced earlier in the same frame.
    ///
    /// `target` **must differ from** the `source` the matching `prepare` sampled: wgpu forbids
    /// sampling the texture a pass writes, so read-after-write within one surface is a contract
    /// violation, not a runtime fallback. The immediate-mode backend additionally restores any
    /// GL state it touched (bound FBO, viewport, blend func, texture units), leaving state as
    /// found.
    fn record(
        &self,
        encoder: &mut Self::Encoder,
        target: &Self::Target,
        prepared: &Self::Prepared,
    ) -> Result<(), BlurError>;
}
