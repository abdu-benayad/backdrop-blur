//! `backdrop-blur-glow` — the **glow** (OpenGL 3.3 / GLES 3.0 / WebGL2) grab-pass backend for
//! [`backdrop-blur`]. It is the path the own-loop/wgpu backend never served: real frosted glass
//! for an `eframe`-on-glow app and the `cage` Wayland kiosk, where the host owns the GL loop and
//! the blur must **grab** a region of the live framebuffer rather than receive a ready-made
//! intermediate.
//!
//! # The one `unsafe` crate
//!
//! Every other crate in the workspace is `#![forbid(unsafe_code)]`. This one cannot be: glow's
//! API is `unsafe` end to end (raw GL is unsynchronized global state). The `unsafe` is
//! **quarantined here** and held to two rules Abdu signed off (DESIGN §11):
//!
//! - `#![deny(unsafe_op_in_unsafe_fn)]` — an `unsafe fn` body gets no free pass; every GL call
//!   still needs an explicit `unsafe` block with a `// SAFETY:` justification.
//! - `#![deny(clippy::undocumented_unsafe_blocks)]` — every `unsafe` block must carry that
//!   comment, so a missing justification fails the build rather than slips through review.
//! - **No GL in `Drop`.** GL objects are freed only by an explicit [`GlowBlur::destroy`] the host
//!   calls from `eframe::App::on_exit` (where the context is still current). `Drop` issues no GL —
//!   a dropped-without-destroy blurrer `log::warn!`s and leaks rather than calling GL on a
//!   possibly-gone context (undefined behavior).
//!
//! # Portability
//!
//! glow is build-script-free (runtime-loaded function pointers), so this crate **compiles on any
//! runner with no GL present** and is a normal workspace member. Everything that needs a live
//! context — the EGL-surfaceless native harness and every readback test — sits behind the
//! `gl-snapshots` feature, so plain `cargo test --workspace` runs only this crate's Tier-0 (pure)
//! tests and stays GPU-free.
//!
//! [`backdrop-blur`]: https://github.com/abdu-benayad/backdrop-blur
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod profile;

pub use profile::{GlProfile, RenderableFloat, ShaderClass};
