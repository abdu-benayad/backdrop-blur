//! # The seam gate (IMPL §1d / §3)
//!
//! Before any wgpu code is written, this crate proves the [`BackdropBlur`] seam fits the
//! **genuinely divergent** backend — glow's immediate-mode GL — which is the seam's entire
//! justification. It is a *compile-only* artifact: every method body is `unimplemented!()`,
//! so the only thing under test is whether glow's real resource types satisfy the trait's
//! associated types and method signatures **with no extra method and no `()` standing in a
//! load-bearing slot**. The real, `unsafe` glow implementation is the deferred
//! `backdrop-blur-glow` crate; this is not it.
//!
//! The mapping comes straight from the proven spike (`abdu-egui-ui`'s `tooltip_blur_spike.rs`,
//! the working `GlBlur`: grab a region with `copy_tex_image_2d` → ping-pong blur → composite →
//! rebind egui's FBO).
//!
//! ## The §3 decision table — every cell maps
//!
//! | `BackdropBlur` item | glow (from the spike) | load-bearing? |
//! |---|---|---|
//! | `Device`        | `glow::Context`                       | yes |
//! | `Queue`         | `()` — glow uploads via the context   | **no** — honest `()`, not a stand-in |
//! | `Encoder`       | `glow::Context` — the immediate handle| yes (same type as `Device`: glow's reality) |
//! | `Framebuffer`   | `glow::Framebuffer` — the grab source  | yes |
//! | `SourceTexture` | `glow::Texture` — grabbed backdrop     | yes |
//! | `Target`        | `glow::Framebuffer` — composite dest   | yes |
//! | `TargetFormat`  | `u32` — a GLES internal-format enum    | yes |
//! | `Prepared`      | [`GlPrepared`] — resolved payload, OWNED| yes (resolves, does **not** upload — K2) |
//! | `grab_source`   | `copy_tex_image_2d`; bottom-left flip *inside* it | the K5 socket — no extra method |
//!
//! ## Verdict: **the trait fits — keep it.**
//!
//! Every associated type binds to a real glow type; the one `()` (Queue) is honest, because
//! glow has no separate upload queue (it uploads through the context). `Device` and `Encoder`
//! both binding to `glow::Context` is glow's immediate-mode reality, not a contortion. The
//! origin flip and the grab live entirely inside `grab_source`, so no extra trait method is
//! needed. Therefore v1 ships **with** the `BackdropBlur` trait (not a concrete one-backend
//! pair). This file compiling **is** that proof.
#![forbid(unsafe_code)] // The sketch has no bodies; the real GL `unsafe` lives in the deferred glow crate.

use backdrop_blur_core::{BackdropBlur, BlurError, BlurRequest, Region, ResolvedMask, Tint};

/// The cached, cross-frame glow resources (programs + per-size ping-pong scratch), mirroring
/// the spike's `GlBlur`. Fields are illustrative — bodies are `unimplemented!()`.
#[expect(
    dead_code,
    reason = "gate sketch: method bodies are unimplemented!(), so these resource fields are never read; they exist to show the type mapping is real"
)]
pub struct GlowBlur {
    /// The separable-blur / dual-Kawase program.
    blur_program: glow::Program,
    /// The tint + rounded-rect composite program.
    composite_program: glow::Program,
    /// Per-size ping-pong scratch chains — the resource keys `Prepared` refers to.
    scratch: Vec<ScratchChain>,
}

/// One ping-pong scratch chain, keyed by physical size.
#[expect(
    dead_code,
    reason = "gate sketch: illustrative resource key, never read because bodies are unimplemented!()"
)]
struct ScratchChain {
    size: [u32; 2],
    textures: [glow::Texture; 2],
    framebuffers: [glow::Framebuffer; 2],
}

/// The **owned** per-call handle glow's `prepare` resolves and `record` consumes. Critically,
/// it borrows nothing from [`GlowBlur`] and holds *resolved* values (offsets, tint, mask, rect)
/// plus resource keys (`glow::Texture` handles are `Copy`) — glow resolves here and binds the
/// uniforms at draw time in `record`, so there is no "upload" to hold (K2).
#[expect(
    dead_code,
    reason = "gate sketch: the resolved payload is never read because record's body is unimplemented!()"
)]
pub struct GlPrepared {
    /// The clamped rounded-rect mask for the composite shader.
    mask: ResolvedMask,
    /// The glass film.
    tint: Tint,
    /// Where to composite, in the target framebuffer.
    target_rect: Region,
    /// Resolved per-pass sampling offsets (the algorithm-specific part the backend owns).
    pass_offsets: Vec<f32>,
    /// Which scratch textures this surface blurs through (resource keys, not borrows).
    scratch: [glow::Texture; 2],
}

impl BackdropBlur for GlowBlur {
    type Device = glow::Context;
    type Queue = (); // glow uploads through the context; there is no separate queue.
    type Encoder = glow::Context; // the immediate-mode draw handle is the same context.
    type Framebuffer = glow::Framebuffer; // the live FBO to grab from.
    type SourceTexture = glow::Texture; // the grabbed, sampleable backdrop.
    type Target = glow::Framebuffer; // the composite destination FBO.
    type TargetFormat = u32; // a GLES internal-format enum (e.g. glow::RGBA8).
    type Prepared = GlPrepared;

    fn grab_source(
        &mut self,
        _device: &Self::Device,
        _queue: &Self::Queue,
        _framebuffer: &Self::Framebuffer,
        _region: Region,
    ) -> Result<Self::SourceTexture, BlurError> {
        // Spike: `copy_tex_image_2d` the region out of `framebuffer` into a grab texture; the
        // bottom-left→top-left origin flip lives HERE (K5), so no extra trait method is needed.
        unimplemented!("gate sketch — type mapping only; real GL lives in backdrop-blur-glow")
    }

    fn prepare(
        &mut self,
        _device: &Self::Device,
        _queue: &Self::Queue,
        _source: &Self::SourceTexture,
        _target_format: Self::TargetFormat,
        _request: &BlurRequest,
    ) -> Result<Option<Self::Prepared>, BlurError> {
        // Spike: resolve offsets/tint/mask/rect into `GlPrepared` and pick/allocate a scratch
        // chain. glow does NOT upload here — uniforms bind at draw time in `record` (K2).
        // Ok(None) for a zero-sized/offscreen `request.source_region`.
        unimplemented!("gate sketch — type mapping only; real GL lives in backdrop-blur-glow")
    }

    fn record(
        &self,
        _encoder: &mut Self::Encoder,
        _target: &Self::Target,
        _prepared: &Self::Prepared,
    ) -> Result<(), BlurError> {
        // Spike: bind program/scratch, draw down → up → composite into `target`, then restore
        // the GL state touched (bound FBO, viewport, blend func, texture units).
        unimplemented!("gate sketch — type mapping only; real GL lives in backdrop-blur-glow")
    }
}
