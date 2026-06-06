//! `backdrop-blur-egui` — the egui adapter for the **own-loop** frosted-glass path.
//!
//! For a host that drives `egui-winit` + `egui-wgpu` directly (not eframe), [`OwnLoopRenderer`]
//! renders the egui UI into an offscreen intermediate and the display target, then blurs a region
//! of the intermediate and composites a frosted [`Surface`] over the target — all on one command
//! encoder with a single submit, in the order that does not panic (DESIGN §6).
//!
//! The crate owns only a surface's *background*. The surface's content, foreground, and
//! accessibility stay the host's: a frosted [`Surface`] is a post-render composite, never an egui
//! widget, so it adds nothing to the AccessKit tree.
//!
//! The crate is **feature-split** into two adapter paths over one shared [`Surface`] vocabulary:
//! `own-loop` (default; the egui-wgpu path here) and `grab-pass` (the eframe-on-glow path, added
//! in the glow increment). A kiosk/grab-pass build activates neither wgpu nor egui-wgpu — the
//! own-loop deps are optional and gated. The mainstream `eframe`-on-glow backend itself is the
//! separate `backdrop-blur-glow` crate.
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
