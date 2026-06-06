//! The seam — the traits each GPU backend implements.
//!
//! [`BackdropBlur`] is the universal seam: **two-phase** (`prepare` then `record`) because the
//! backends demand it. wgpu uploads uniforms/textures through its **Queue** (not the encoder),
//! so the upload phase needs the queue and the record phase needs only the encoder; glow is
//! immediate-mode (`prepare` uploads via the context, `record` draws). Every backend implements
//! this trait, and it is **total** — it contains no method a given backend cannot perform.
//!
//! [`GrabPass`] is a **separate, additive** trait for the grab-pass family only. The own-loop
//! path (wgpu) hands the host's already-sampleable intermediate straight to `prepare` and never
//! grabs, so forcing a `grab_source` onto every backend would make wgpu stub an impossible
//! method (it cannot fabricate a source from nothing). Instead the grab-pass (glow) backend
//! *additionally* implements `GrabPass` to blit a sampleable source out of a live framebuffer.
//! This is the socket that keeps glow additive (DESIGN §4.4).
//!
//! The associated types are each backend's resource universe — distinct per backend, which is
//! exactly why these traits are **not object-safe** and backends are **separate crates** (the
//! `wgpu-types` → `wgpu-hal` model). Dispatch is static, monomorphized.
//!
//! # Gate verdict (IMPL §1d): the seam is kept — now proven by the real backend
//!
//! Before committing to this seam, the divergent backend was first sketched against it in a
//! compile-only `examples/glow-gate` crate (now retired). That gate has been **superseded by the
//! real implementation**: [`backdrop-blur-glow`] implements both `BackdropBlur` and `GrabPass`
//! with live, `unsafe` GL and a full Tier-1 readback suite, so the seam is proven by working code
//! rather than an `unimplemented!()` sketch. Each associated type binds to a real glow type
//! (`Device`/`Encoder` → `glow::Context`, `SourceTexture` → the grab source, `Target` →
//! `Option<glow::Framebuffer>` — the live draw FBO, `None` = the default framebuffer); the one `()`
//! (`Queue`) is honest because glow uploads through its context rather than a queue, and
//! `TargetFormat` is the **framebuffer size** (the composite viewport) — not a color format: the
//! composite needs the full draw-target size for its full-framebuffer `glViewport`, and the encode
//! bit is still derived at draw time from the live context's `GL_FRAMEBUFFER_SRGB` state, not from a
//! format token. v1 therefore ships **with** these traits, not a concrete one-backend pair.
//!
//! [`backdrop-blur-glow`]: https://github.com/abdu-benayad/backdrop-blur

use crate::{BlurError, BlurRequest, GlRegion};

/// Implemented once per GPU backend (`WgpuBlur` in v1; `GlowBlur` later). The implementor holds
/// the backend's cached resources — per-`(size, levels)` ping-pong chains and the pipelines —
/// across frames, so repeated frosted surfaces do not rebuild them.
///
/// # Lifecycle contract (v1)
///
/// - **Serial `prepare` → `record` per surface.** v1 scope is a single frosted surface over a
///   once-rendered backdrop; because the ping-pong scratch is shared, two surfaces are not
///   prepared-then-both-recorded — each is prepared and recorded before the next. The
///   [`Prepared`](Self::Prepared) handle is **owned** (it borrows nothing from the blurrer), so
///   the contract does not rely on "record immediately follows prepare"; genuine multi-surface
///   batching is deferred future work. Because `Prepared` is owned, the types permit (but the
///   contract forbids) preparing two surfaces against the shared scratch before recording
///   either — the backend should debug-assert against an outstanding handle (K1).
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
    /// A sampleable backdrop (`wgpu::TextureView`; `glow::Texture`). Own-loop backends receive
    /// it from the host; grab-pass backends produce it via [`GrabPass::grab_source`].
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

    /// **Phase 1** — holds the device + queue. Allocates and keys the ping-pong chain, lazily
    /// builds and caches pipelines (the fixed-scratch down/up pipelines once; the composite
    /// pipeline per `target_format`), and resolves the payload into an owned
    /// [`Prepared`](Self::Prepared).
    ///
    /// Returns `Ok(None)` when `request.source_region` clips to nothing against `source` — a
    /// zero-area or fully-offscreen region (see [`Region::clip_to`]). That is a **no-op**, valid
    /// input rather than an error; `record` is then simply not called. Returns `Err` only on a
    /// real GPU fault.
    ///
    /// [`Region::clip_to`]: crate::Region::clip_to
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

/// The **grab-pass** socket, implemented *in addition to* [`BackdropBlur`] only by backends that
/// must extract a sampleable backdrop from a live framebuffer (glow; the deferred mainstream-egui
/// path). Own-loop backends (wgpu) do **not** implement this — they receive an already-sampleable
/// source — which is why it is a separate trait rather than a method every backend must stub.
pub trait GrabPass: BackdropBlur {
    /// The live framebuffer to grab from (a glow framebuffer). Distinct from
    /// [`BackdropBlur::Target`]; it is the *read* source, not the composite destination.
    type Framebuffer;

    /// Produce a sampleable [`SourceTexture`](BackdropBlur::SourceTexture) by blitting (and
    /// MSAA-resolving) the `region` out of the live `framebuffer` — backend-specific GL the host
    /// cannot do generically.
    ///
    /// `region` is a [`GlRegion`] — **already in GL bottom-left coordinates** — so `grab_source`
    /// performs **no** read-origin flip. This is a deliberate divergence from the v1 seam, where
    /// this doc placed the bottom-left↔top-left flip *inside* `grab_source`: the adapter now
    /// derives the region from egui's bottom-origin `from_bottom_px` and builds the whole
    /// [`BlurRequest`] bottom-left (DESIGN §5), so a flip here would be a *double* flip. The
    /// y-orientation is carried by the type, not by an internal arithmetic step (see [`GlRegion`]).
    fn grab_source(
        &mut self,
        device: &Self::Device,
        queue: &Self::Queue,
        framebuffer: &Self::Framebuffer,
        region: GlRegion,
    ) -> Result<Self::SourceTexture, BlurError>;
}
