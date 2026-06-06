//! GL context capabilities the backend resolves once, at construction. [`classify`] is **pure**
//! (a function of the driver strings) so it is fully Tier-0 testable with no GL; [`probe`] is the
//! thin `unsafe` wrapper that reads those strings off a live context and hands them to `classify`.
//!
//! Two capabilities drive real decisions: the **shader dialect** ([`ShaderClass`]) selects the
//! `#version` header the one GLSL source is compiled under (DESIGN §8), and the **renderable float
//! format** ([`RenderableFloat`]) decides whether the linear-HDR scratch is `RGBA16F` or falls
//! back to `sRGB8_ALPHA8` on a WebGL2/GLES context lacking `EXT_color_buffer_float` (DESIGN §9).

use glow::HasContext;

/// The GLSL dialect the shaders compile under — desktop vs embedded, which pick the `#version`
/// header (DESIGN §8). Mirrors `egui_glow`'s `ShaderVersion` split rather than reusing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderClass {
    /// Desktop OpenGL (3.3+): `#version 140`.
    GlDesktop,
    /// GLES 3.0 / WebGL2: `#version 300 es` + `precision highp` qualifiers.
    Es300,
}

/// The color-renderable float format for the linear scratch/grab textures. `RGBA16F` is preferred
/// (linear HDR); the `sRGB8_ALPHA8` fallback keeps the blur renderable on a WebGL2/GLES context
/// without `EXT_color_buffer_float` (no HDR headroom, but correct).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderableFloat {
    /// `RGBA16F` — linear HDR, color-renderable. Desktop GL 3.0+ always; GLES3/WebGL2 with
    /// `EXT_color_buffer_float`.
    Rgba16F,
    /// `sRGB8_ALPHA8` fallback when float-render is unavailable.
    Srgb8Rgba8,
}

/// The capabilities of a live GL context, resolved once at construction and read back per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlProfile {
    /// Which `#version` dialect the shaders compile under.
    pub shader_class: ShaderClass,
    /// Whether this is an embedded context (GLES / WebGL2) vs desktop GL.
    pub embedded: bool,
    /// The scratch/grab float format this context can render to.
    pub renderable_float: RenderableFloat,
    /// The default framebuffer's sample count (`GL_SAMPLES`); `> 0` means the grab must resolve
    /// MSAA before sampling (DESIGN §11, IMPL §2c). Clamped non-negative.
    pub samples: i32,
}

impl GlProfile {
    /// Read the capabilities off a **live, current** GL context.
    pub fn probe(gl: &glow::Context) -> Self {
        // SAFETY: `gl` is a live, current GL context (the backend's construction-time contract —
        // probe runs only from the host's eframe/test context while it is current). These are
        // pure state queries: `get_parameter_string`/`get_parameter_i32` read driver strings and
        // integers, taking no caller pointers and mutating no GL state, so they are sound for any
        // current context.
        let (version, samples) = unsafe {
            (
                gl.get_parameter_string(glow::VERSION),
                gl.get_parameter_i32(glow::SAMPLES),
            )
        };
        // `supported_extensions` is glow's safe, cached accessor (no GL call).
        let extensions: Vec<&str> = gl
            .supported_extensions()
            .iter()
            .map(String::as_str)
            .collect();
        classify(&version, &extensions, samples)
    }
}

/// Classify a context from its `GL_VERSION` string, extension set, and sample count — **pure**, so
/// every branch is Tier-0 testable. The GLSL version is not an input: the dialect follows directly
/// from whether the context is embedded (`OpenGL ES` in the version string, which also covers a
/// WebGL2 `"WebGL 2.0 (OpenGL ES 3.0 …)"` string), and the `#version` header is then fixed.
pub fn classify(version: &str, extensions: &[&str], samples: i32) -> GlProfile {
    let embedded = version.contains("OpenGL ES");
    let shader_class = if embedded {
        ShaderClass::Es300
    } else {
        ShaderClass::GlDesktop
    };
    // RGBA16F is color-renderable in desktop GL 3.0+ unconditionally. On GLES3/WebGL2 it requires
    // EXT_color_buffer_float; without it, fall back to sRGB8_ALPHA8 (renderable everywhere). The
    // extension may appear with or without the `GL_` prefix depending on the driver.
    let has_float_render = extensions
        .iter()
        .any(|e| *e == "EXT_color_buffer_float" || *e == "GL_EXT_color_buffer_float");
    let renderable_float = if !embedded || has_float_render {
        RenderableFloat::Rgba16F
    } else {
        RenderableFloat::Srgb8Rgba8
    };
    GlProfile {
        shader_class,
        embedded,
        renderable_float,
        samples: samples.max(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_gl_is_not_embedded_and_renders_rgba16f() {
        let p = classify("4.6.0 NVIDIA 535.288.01", &[], 0);
        assert_eq!(p.shader_class, ShaderClass::GlDesktop);
        assert!(!p.embedded);
        // Desktop GL renders RGBA16F with no extension needed.
        assert_eq!(p.renderable_float, RenderableFloat::Rgba16F);
    }

    #[test]
    fn gles3_without_the_float_extension_falls_back_to_srgb8() {
        let p = classify("OpenGL ES 3.0 Mesa", &[], 0);
        assert_eq!(p.shader_class, ShaderClass::Es300);
        assert!(p.embedded);
        assert_eq!(p.renderable_float, RenderableFloat::Srgb8Rgba8);
    }

    #[test]
    fn gles3_with_the_float_extension_renders_rgba16f() {
        let p = classify("OpenGL ES 3.2 NVIDIA", &["EXT_color_buffer_float"], 0);
        assert_eq!(p.renderable_float, RenderableFloat::Rgba16F);
        let prefixed = classify("OpenGL ES 3.2 NVIDIA", &["GL_EXT_color_buffer_float"], 0);
        assert_eq!(prefixed.renderable_float, RenderableFloat::Rgba16F);
    }

    #[test]
    fn webgl2_version_string_is_classified_embedded() {
        // glow's web backend reports VERSION as "WebGL 2.0 (OpenGL ES 3.0 Chromium)".
        let p = classify(
            "WebGL 2.0 (OpenGL ES 3.0 Chromium)",
            &["EXT_color_buffer_float"],
            0,
        );
        assert_eq!(p.shader_class, ShaderClass::Es300);
        assert!(p.embedded);
        assert_eq!(p.renderable_float, RenderableFloat::Rgba16F);
    }

    #[test]
    fn samples_pass_through_clamped_non_negative() {
        assert_eq!(classify("4.6.0", &[], 4).samples, 4);
        // A driver returning -1 (no MSAA query support) clamps to 0 (no resolve).
        assert_eq!(classify("4.6.0", &[], -1).samples, 0);
    }
}
