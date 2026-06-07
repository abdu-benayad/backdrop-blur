//! `backdrop-blur-egui` — the egui adapter for frosted glass, over **two paths** sharing one
//! [`Surface`] vocabulary.
//!
//! - **`grab-pass`** (the mainstream path: `eframe`-on-glow and the `cage` Wayland kiosk). The host
//!   owns the GL loop; `GrabPassRenderer` rides an egui **paint callback** that grabs the live
//!   framebuffer behind a surface, blurs it, and composites the frosted surface back. Build it once
//!   from `eframe::CreationContext::gl`, call `.frost(ui, surface)` per frame, and `.destroy(gl)` in
//!   `eframe::App::on_exit`. Pulls glow, never wgpu.
//! - **`own-loop`** (default feature). For a host driving `egui-winit` + `egui-wgpu` directly (not
//!   eframe), [`OwnLoopRenderer`] renders the UI into an offscreen intermediate, blurs a region of
//!   it, and composites a frosted [`Surface`] over the display target — one encoder, one submit, in
//!   the order that does not panic (DESIGN §6). Pulls the wgpu stack.
//!
//! Pick the path with a feature: a kiosk build is `--no-default-features --features grab-pass` and
//! compiles neither wgpu nor egui-wgpu; an own-loop build is the default. The backends themselves are
//! the separate `backdrop-blur-glow` / `backdrop-blur-wgpu` crates.
//!
//! The crate owns only a surface's *background*. The surface's content, foreground, and
//! accessibility stay the host's: a frosted [`Surface`] is a post-render composite, never an egui
//! widget, so it adds nothing to the AccessKit tree.
//!
//! # The three dials: blur, tint, opacity
//!
//! A frosted [`Surface`] mixes three **independent** knobs — conflating them is the most common
//! "my glass looks wrong":
//!
//! - **[`BlurStrength`]** — the blur *radius* in logical points. How smeared the backdrop is.
//!   `0` = no blur (a plain tinted pane).
//! - **[`Tint`]** — the glass *film* painted over the blur, a linear-light color whose **alpha is
//!   the film mix** (how much tint shows vs. how much blurred backdrop shows through). A *colored*
//!   tint composites in color, not black — author it as sRGB with [`Tint::from_srgb_unmultiplied`]
//!   so the linear decode is done for you. Alpha `0` = pure blur, no film; alpha `1` = the film is
//!   opaque and the blur is invisible under it.
//! - **[`Opacity`]** — the surface-global *presence* in `[0, 1]`, the whole frosted result blended
//!   over the destination. This is the **fade dial**: drive it per frame to dissolve glass in/out.
//!   Default `1.0`.
//!
//! Rule of thumb: blur sets the *texture*, tint-alpha sets the *material*, opacity sets the
//! *presence*. A barely-tinted heavy blur is clear vibrancy; a high tint-alpha is frosted/opaque
//! glass; opacity below `1` fades the entire thing.
//!
//! # Grab-pass contracts (read before calling `frost`)
//!
//! The grab-pass path samples the **live framebuffer** mid-frame, which makes draw order and fade
//! load-bearing in ways the types cannot enforce:
//!
//! 1. **Enqueue the frost *before* the surface's foreground.** The callback grabs whatever is in the
//!    framebuffer at its position — content drawn *before* it. Call `frost(ui, surface)` first, then
//!    paint the surface's own content (text, controls) **after**, so the foreground lands on top of
//!    the blur. Enqueue it too late and it grabs — and blurs away — your own content. There is no
//!    runtime guard for this; it is a hard ordering contract.
//! 2. **Fade with [`Opacity`], not `multiply_opacity`.** egui's `Ui::multiply_opacity` (and the
//!    `Opacity` style) **do not reach paint callbacks** — the standard fade silently no-ops on the
//!    blur. To dissolve frost in/out, drive the surface's `opacity` field ([`Opacity`]) per frame
//!    instead. This is the one egui trap that bites everyone; the [`Opacity`] dial is the supported
//!    escape hatch.
//! 3. **A dynamically-sized rect needs *last frame's* rect.** In immediate mode the surface's rect
//!    is only known *after* its content lays out, but the frost must be enqueued *before* the content
//!    paints (contract 1) — a chicken-and-egg. The worked pattern: stash the rect in egui temp memory
//!    keyed by an `Id`, frost **last frame's** rect at the top of this frame, then lay out the content
//!    and write back the rect for next frame. It is stable while the surface is open; the only
//!    artifact is one frame of staleness on a resize. (A first-class reserved-slot API that returns
//!    the callback `Shape` for `painter.set()` is planned; until then this is the recommendation.)
//! 4. **`GrabPassRenderer::took_effect` reports *ran*, not *composited*.** egui skips a
//!    fully-clipped callback, so a frosted surface is not guaranteed to paint; `took_effect` lets the
//!    host observe that the callback **fired**. It is set even when the region clipped to nothing or
//!    the frost errored — it answers "did egui invoke my callback this frame", not "did pixels
//!    change". Useful to confirm wiring; not a success signal.
#![forbid(unsafe_code)]

mod surface;

#[cfg(feature = "own-loop")]
mod own_loop;

#[cfg(feature = "grab-pass")]
mod grab_pass;

// Neutral spine — available on both paths: the glass material vocabulary (used in `Surface`) and
// the shared `Surface` type itself.
pub use backdrop_blur_core::{
    BlurStrength, CornerRadius, LinearRgba, Opacity, RepaintPolicy, Tint,
};
pub use surface::Surface;

// Own-loop path re-exports: the wgpu backend (`render_frame` drives it), the egui-wgpu screen
// descriptor (`FrameInput` carries it), and the renderer. Gated so a grab-pass-only build pulls
// none of the wgpu stack.
#[cfg(feature = "own-loop")]
pub use backdrop_blur_wgpu::{SourceColorSpace, SourceView, WgpuBlur};
#[cfg(feature = "own-loop")]
pub use egui_wgpu::ScreenDescriptor;
#[cfg(feature = "own-loop")]
pub use own_loop::{FrameInput, OwnLoopRenderer, is_supported_target, strongest_repaint};

// Grab-pass path: the eframe-on-glow adapter. Gated so an own-loop-only build pulls no glow/egui_glow.
#[cfg(feature = "grab-pass")]
pub use grab_pass::GrabPassRenderer;

// Re-export the exact `glow` this crate's public API ([`GrabPassRenderer::new`]/`destroy`) is typed
// against, so a consumer writes `backdrop_blur_egui::glow::Context` and is structurally pinned to the
// same `glow` as the adapter. Without this a consumer picks its own `glow` version; a skew from the
// one eframe hands back at `new` surfaces as a baffling "expected `glow::Context`, found
// `glow::Context`" with no breadcrumb. Re-exporting the crate (the eframe-ecosystem norm) turns the
// footgun into a compile-time guarantee.
#[cfg(feature = "grab-pass")]
pub use glow;
