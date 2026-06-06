#![cfg(feature = "gl-snapshots")]
//! Tier-1: the glow backend exercised against a **real** surfaceless GL context (the shared
//! `gl_harness`). Gated behind `gl-snapshots`, so plain `cargo test --workspace` never compiles or
//! runs it — it is *absent* on a non-GL runner, not skipped (IMPL §14). Grows one test per build
//! step: 2b (programs link) here; grab / blur / composite land in 2c–2g.

mod gl_harness;

use gl_harness::{headless_gl, read_rgba8};
use glow::HasContext;

use backdrop_blur_glow::GlowBlur;

/// Harness self-check: the surfaceless context clears an FBO to a known color and reads it back.
/// Isolates an EGL/harness failure from a backend-logic failure — if this fails, the problem is the
/// context, not the blur code.
#[test]
fn harness_context_clears_and_reads_back() {
    let gl = headless_gl();
    let (w, h) = (64_i32, 64_i32);
    // SAFETY: standard FBO setup on the current context; every handle is created and freed here, and
    // the framebuffer completeness is asserted before reading.
    unsafe {
        let fbo = gl.create_framebuffer().expect("create_framebuffer");
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        let tex = gl.create_texture().expect("create_texture");
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w,
            h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        assert_eq!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER),
            glow::FRAMEBUFFER_COMPLETE,
            "harness FBO incomplete"
        );
        gl.viewport(0, 0, w, h);
        gl.clear_color(0.2, 0.4, 0.6, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);

        let px = read_rgba8(&gl, 32, 32); // ~ (51, 102, 153, 255), +/-1 for 8-bit rounding
        assert!(
            (px[0] as i32 - 51).abs() <= 1
                && (px[1] as i32 - 102).abs() <= 1
                && (px[2] as i32 - 153).abs() <= 1
                && px[3] == 255,
            "clear-color readback was {px:?}"
        );

        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.delete_framebuffer(fbo);
        gl.delete_texture(tex);
    }
}

/// 2b: `GlowBlur::new` compiles + links every blur/composite program and creates the shared VAO on a
/// real GL context (the GLSL actually compiles under the resolved `#version` header — the Tier-0
/// gamma test can't catch a syntax error the driver would). `destroy` then frees them, and a second
/// `destroy` is an idempotent no-op.
#[test]
fn glow_blur_builds_all_programs_on_a_real_context() {
    let gl = headless_gl();
    let mut blur = GlowBlur::new(&gl)
        .expect("GlowBlur::new should compile+link all programs on a GL 3.3 context");
    blur.destroy(&gl);
    blur.destroy(&gl); // idempotent — must not double-free or panic
}
