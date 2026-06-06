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
mod program;

use backdrop_blur_core::{BlurError, BlurStage};
use glow::HasContext;
use program::Programs;

pub use profile::{GlProfile, RenderableFloat, ShaderClass};

/// The glow backend's cross-frame GL resources: the compiled blur/composite programs and the
/// shared fullscreen-triangle VAO (and, from later steps, the ping-pong scratch and grab
/// textures). Holds **no** `glow::Context` — the context is passed per call, so a `GlowBlur` is a
/// plain bag of GL handles and is `Send` (the host owns one per render thread).
///
/// # Teardown contract (DESIGN §11, Abdu-signed-off)
///
/// GL objects are freed only by [`GlowBlur::destroy`], which the host calls from
/// `eframe::App::on_exit` while the context is still current. [`Drop`] issues **no GL** — a
/// blurrer dropped without `destroy` `log::warn!`s and leaks rather than calling GL on a
/// possibly-destroyed context (undefined behavior). `destroy` is idempotent.
pub struct GlowBlur {
    #[expect(
        dead_code,
        reason = "resolved capabilities consumed by the grab (step 2c) and composite (step 2f)"
    )]
    profile: GlProfile,
    programs: Programs,
    /// The shared, empty VAO bound for every `gl_VertexID` fullscreen-triangle draw.
    vao: glow::VertexArray,
    /// Set by [`Self::destroy`]; gates `Drop`'s leak warning and makes `destroy` idempotent.
    destroyed: bool,
}

impl GlowBlur {
    /// Build the backend against a **live, current** GL context: probe its capabilities, compile +
    /// link the programs under the right shader dialect, and create the shared VAO. On any failure
    /// the partial GL state is cleaned up before returning.
    pub fn new(gl: &glow::Context) -> Result<Self, BlurError> {
        let profile = GlProfile::probe(gl);
        let programs = Programs::new(gl, &profile)?;
        // SAFETY: `gl` is current; `create_vertex_array` returns a fresh VAO or an error string,
        // taking no caller pointers.
        let vao = match unsafe { gl.create_vertex_array() } {
            Ok(vao) => vao,
            Err(e) => {
                programs.destroy(gl);
                return Err(BlurError::ResourceCreation {
                    stage: BlurStage::VertexArray,
                    source: e.into(),
                });
            }
        };
        Ok(Self {
            profile,
            programs,
            vao,
            destroyed: false,
        })
    }

    /// Free every GL object this backend owns. The host calls it from `on_exit` while the context
    /// is current (DESIGN §11). Idempotent: a second call is a no-op.
    pub fn destroy(&mut self, gl: &glow::Context) {
        if self.destroyed {
            return;
        }
        self.programs.destroy(gl);
        // SAFETY: `self.vao` was created by `new` on a context the caller guarantees is the same,
        // current one; it is deleted exactly once (the `destroyed` guard prevents a second delete).
        unsafe { gl.delete_vertex_array(self.vao) };
        self.destroyed = true;
    }
}

impl Drop for GlowBlur {
    fn drop(&mut self) {
        if !self.destroyed {
            // No GL here by contract — the context may already be gone. Warn (uncatchable) + leak.
            log::warn!(
                "GlowBlur dropped without destroy(&gl): GL objects leaked. Call \
                 GlowBlur::destroy from eframe::App::on_exit while the context is current \
                 (DESIGN §11)."
            );
        }
    }
}
