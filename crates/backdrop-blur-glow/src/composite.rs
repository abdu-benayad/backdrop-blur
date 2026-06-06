//! The composite pass: paint the frosted surface over the whole framebuffer. This is a **rewrite,
//! not a port** of wgpu's composite (DESIGN §10/§2f) — same SDF + `backdrop_uv_remap` math, but
//! three load-bearing GL specifics the wgpu render-pass abstraction handled implicitly:
//!
//! 1. **Full-framebuffer viewport.** egui pre-sets the GL viewport to the *panel rect* before the
//!    paint callback. A full-screen NDC triangle under a panel-sized viewport rasterizes only
//!    inside the panel — every fragment `coverage == 1` and the outer half of the analytic AA band
//!    is never generated, silently killing straight-edge AA. So the composite sets
//!    `glViewport(0, 0, fb_w, fb_h)` (overriding egui's), making `gl_FragCoord` carry true
//!    full-framebuffer window coords that match the bottom-left `rect_origin` uniform.
//! 2. **Scissor disabled** for the whole draw, so the AA band is not clipped to egui's per-
//!    primitive scissor box.
//! 3. **Premultiplied blend.** `composite.frag` emits premultiplied alpha (`rgb·a`, `a`, where
//!    `a = coverage · opacity` — the rounded-rect coverage scaled by the surface-global fade),
//!    paired here with `glBlendFunc(ONE, ONE_MINUS_SRC_ALPHA)`.
//!
//! The encode bit is **not** a `wgpu::TextureFormat` allowlist (glow never sees the `u32` internal
//! format): on native it is `!glIsEnabled(GL_FRAMEBUFFER_SRGB)`. `GL_FRAMEBUFFER_SRGB` is **global**
//! enable state (not per-FBO), so the query is bind-independent; sampling it after the target bind is
//! merely defensive ordering, and the encode decision is driven by that global enable (DESIGN §10).

use crate::program::Programs;
use backdrop_blur_core::{LinearRgba, ResolvedMask};
use glow::HasContext;

/// The resolved, GPU-free composite inputs `prepare` computes and `record` replays — all in **GL
/// bottom-left framebuffer pixels** (DESIGN §5). Owned (`Copy`); borrows nothing.
#[derive(Clone, Copy)]
pub(crate) struct CompositeParams {
    /// Target rect origin, bottom-left framebuffer px.
    pub(crate) rect_origin_px: [f32; 2],
    /// Target rect size, framebuffer px.
    pub(crate) rect_size_px: [f32; 2],
    /// The glass film: linear RGB, straight alpha (`a` = film opacity).
    pub(crate) tint: LinearRgba,
    /// Map target-rect uv `[0,1]` onto the clipped blurred scratch (`backdrop_uv_remap`).
    pub(crate) backdrop_uv_offset: [f32; 2],
    pub(crate) backdrop_uv_scale: [f32; 2],
    /// The clamped corner radius, framebuffer px (from [`ResolvedMask`]).
    pub(crate) corner_radius_px: f32,
    /// The full framebuffer size in px — the composite viewport (`glViewport(0,0,fb_w,fb_h)`), so
    /// `gl_FragCoord` spans the whole attachment and the outer AA band is generated.
    pub(crate) framebuffer_size: [u32; 2],
    /// Surface-global fade `[0, 1]` — scales the premultiplied output (both rgb and alpha), so the
    /// surface dissolves to the untouched destination as it goes to 0.
    pub(crate) opacity: f32,
}

impl CompositeParams {
    /// Assemble from the resolved mask, tint, and bottom-left target rect/backdrop remap.
    #[expect(
        clippy::too_many_arguments,
        reason = "flat composite inputs; field-per-arg is the point"
    )]
    pub(crate) fn new(
        rect_origin_px: [f32; 2],
        rect_size_px: [f32; 2],
        tint: LinearRgba,
        backdrop_uv_offset: [f32; 2],
        backdrop_uv_scale: [f32; 2],
        mask: ResolvedMask,
        framebuffer_size: [u32; 2],
        opacity: f32,
    ) -> Self {
        Self {
            rect_origin_px,
            rect_size_px,
            tint,
            backdrop_uv_offset,
            backdrop_uv_scale,
            corner_radius_px: mask.corner_radius_px,
            framebuffer_size,
            opacity,
        }
    }
}

/// Draw the composite into the currently-bound draw framebuffer (the captured target). The caller
/// (`record`) has already bound `target` as the draw FBO and saved every GL binding this perturbs;
/// this function sets the composite-specific state (viewport, scissor-off, premult blend, program,
/// uniforms, the blurred texture on unit 0, the shared VAO) and issues the triangle. The encode bit
/// reads the **global** `GL_FRAMEBUFFER_SRGB` enable (bind-independent state); sampling it after the
/// target bind is defensive, not load-bearing.
///
/// `blurred` is the final linear scratch the composite samples (Gaussian B, or Kawase mip 0).
pub(crate) fn draw(
    gl: &glow::Context,
    programs: &Programs,
    vao: glow::VertexArray,
    blurred: glow::Texture,
    params: &CompositeParams,
) {
    let program = programs.composite;
    let [fb_w, fb_h] = params.framebuffer_size;

    // SAFETY: the encode query, the uniform locations, and the draw all run on the current context
    // (record's contract) with a live, linked `composite` program and the live `blurred` texture +
    // shared `vao`. `glIsEnabled(GL_FRAMEBUFFER_SRGB)` reads a desktop-GL **global** enable (valid
    // here — the native harness/host is desktop GL; the enable is not per-FBO, so the read does not
    // depend on which FBO is bound); on web the wasm path must set encode=1 without this query
    // (FRAMEBUFFER_SRGB is not a WebGL2 enum), handled in the web cfg branch (none built here).
    // Every uniform location comes from this program; a `None` location is a documented no-op (an
    // optimizer may drop an unused uniform), so missing-location handling is not a fault. The
    // viewport/scissor/blend state the caller saved is restored by the caller after this returns.
    unsafe {
        // web: set encode_srgb = 1 unconditionally — GL_FRAMEBUFFER_SRGB is not a WebGL2 enum, so
        // glIsEnabled would raise GL_INVALID_ENUM and pollute the error state.
        let encode_srgb = !gl.is_enabled(glow::FRAMEBUFFER_SRGB);

        gl.viewport(0, 0, fb_w as i32, fb_h as i32);
        gl.disable(glow::SCISSOR_TEST);
        gl.enable(glow::BLEND);
        gl.blend_equation(glow::FUNC_ADD);
        gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);

        gl.use_program(Some(program));

        let loc = |name: &str| gl.get_uniform_location(program, name);
        gl.uniform_2_f32(
            loc("u_rect_origin_px").as_ref(),
            params.rect_origin_px[0],
            params.rect_origin_px[1],
        );
        gl.uniform_2_f32(
            loc("u_rect_size_px").as_ref(),
            params.rect_size_px[0],
            params.rect_size_px[1],
        );
        gl.uniform_4_f32(
            loc("u_tint").as_ref(),
            params.tint.r(),
            params.tint.g(),
            params.tint.b(),
            params.tint.a(),
        );
        gl.uniform_2_f32(
            loc("u_backdrop_uv_offset").as_ref(),
            params.backdrop_uv_offset[0],
            params.backdrop_uv_offset[1],
        );
        gl.uniform_2_f32(
            loc("u_backdrop_uv_scale").as_ref(),
            params.backdrop_uv_scale[0],
            params.backdrop_uv_scale[1],
        );
        gl.uniform_1_f32(loc("u_corner_radius_px").as_ref(), params.corner_radius_px);
        gl.uniform_1_i32(loc("u_encode_srgb").as_ref(), i32::from(encode_srgb));
        gl.uniform_1_f32(loc("u_opacity").as_ref(), params.opacity);

        // The blurred scratch on texture unit 0; the sampler uniform points there.
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(blurred));
        gl.uniform_1_i32(loc("u_blurred").as_ref(), 0);

        gl.bind_vertex_array(Some(vao));
        gl.draw_arrays(glow::TRIANGLES, 0, 3);
    }
}
