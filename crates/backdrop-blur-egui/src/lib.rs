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
//! Mainstream `eframe`-on-glow reach is a later, separate path (`backdrop-blur-glow`); this is the
//! own-loop/wgpu adapter only.
#![forbid(unsafe_code)]

mod own_loop;

// Re-export everything an own-loop consumer needs through this one facade crate: the glass
// material vocabulary (used in `Surface`), the wgpu backend (`render_frame` drives it), and the
// egui-wgpu screen descriptor (`FrameInput` carries it).
pub use backdrop_blur_core::{BlurStrength, CornerRadius, LinearRgba, RepaintPolicy, Tint};
pub use backdrop_blur_wgpu::{SourceColorSpace, SourceView, WgpuBlur};
pub use egui_wgpu::ScreenDescriptor;
pub use own_loop::{FrameInput, OwnLoopRenderer, Surface, is_supported_target, strongest_repaint};
