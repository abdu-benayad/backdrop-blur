//! `backdrop-blur-core` — the backend-agnostic heart of [`backdrop-blur`].
//!
//! This crate owns the *vocabulary* of frosted glass and the *seam* every GPU backend
//! implements, and nothing else. It has **no GPU dependency**, is fully headless-testable,
//! and forbids `unsafe`. That makes it the one crate in the workspace that cannot break a
//! backend: the material/geometry types, the [`BlurError`] model, the liveness policy, and
//! the `BackdropBlur` trait all live here, while `wgpu`/`glow` resource types stay out.
//!
//! See `docs/DESIGN.md` (§4 is the load-bearing type design) and `docs/IMPL.md` for the
//! rationale and build sequence.
//!
//! [`backdrop-blur`]: https://github.com/abdu-benayad/backdrop-blur
#![forbid(unsafe_code)]
