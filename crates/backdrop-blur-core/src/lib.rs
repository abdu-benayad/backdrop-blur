//! `backdrop-blur-core` — the backend-agnostic heart of [`backdrop-blur`].
//!
//! This crate owns the *vocabulary* of frosted glass and the *seam* every GPU backend
//! implements, and nothing else. It has **no GPU dependency**, is fully headless-testable,
//! and forbids `unsafe`. That makes it the one crate in the workspace that cannot break a
//! backend: the material/geometry types, the [`BlurError`] model, the liveness policy, and
//! the backdrop-blur seam trait all live here, while `wgpu`/`glow` resource types stay out.
//!
//! # The shape of a blur
//!
//! A caller describes a frosted surface with a [`BlurRequest`] — *where* the backdrop lives
//! and the surface goes ([`Region`]s in physical pixels), and *what kind of glass* it is
//! ([`BlurStrength`], [`Tint`], [`CornerRadius`]). Core resolves the algorithm-agnostic parts
//! (a physical blur radius via [`BlurStrength::to_physical_radius`]; a clamped
//! [`ResolvedMask`]); the backend resolves the algorithm-specific parts (kernel offsets,
//! pipelines) and does the GPU work.
//!
//! See `docs/DESIGN.md` (§4 is the load-bearing type design) and `docs/IMPL.md` for the
//! rationale and build sequence.
//!
//! [`backdrop-blur`]: https://github.com/abdu-benayad/backdrop-blur
#![forbid(unsafe_code)]

mod error;
mod geometry;
mod liveness;
mod material;
mod seam;

pub use error::{BackendError, BlurError, BlurStage};
pub use geometry::{BlurRequest, Region, ResolvedMask, Scale};
pub use liveness::RepaintPolicy;
pub use material::{BlurStrength, CornerRadius, LinearRgba, Tint};
pub use seam::BackdropBlur;
