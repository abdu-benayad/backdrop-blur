//! The return half of the seam contract. [`BlurError`] is a `thiserror` enum, but because
//! core has **no GPU dependency** it cannot name a backend error type (`wgpu::*`/`glow::*`
//! live in the GPU crates core forbids). It therefore carries a **boxed trait-object source**
//! ([`BackendError`]) — still a typed `Error` value that composes with `?` and `#[source]`,
//! never a flattened `String` model (DESIGN §4.5).
//!
//! A zero-sized/offscreen region is a **no-op**, not an error (`prepare` returns `Ok(None)`),
//! so there is deliberately no `ZeroSizedRegion` variant.

use crate::gl_region::GlRegion;

/// The boxed, typed source a backend attaches to a [`BlurError`]. Core cannot name
/// `wgpu::Error`/`glow` errors, so it accepts any `Send + Sync` standard error.
pub type BackendError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Everything that can go wrong producing a frosted surface. Each `Display` is a complete
/// sentence; `ResourceCreation.stage` localizes a 3 AM kiosk failure to the exact resource
/// that died.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlurError {
    /// A GPU resource could not be created during `prepare`.
    #[error("failed to create the {stage} while preparing the blur")]
    ResourceCreation {
        /// Which resource failed.
        stage: BlurStage,
        /// The backend's underlying error.
        #[source]
        source: BackendError,
    },

    /// The device ran out of memory allocating a GPU resource on the own-loop (wgpu) path, captured
    /// by an `OutOfMemory` error scope at the creating call. Distinct from the internal-invariant
    /// [`ResourceCreation`](Self::ResourceCreation) assertions. Carries no resource stage: an
    /// out-of-memory is a machine-state condition, and the recovery contract does not branch on which
    /// resource failed. **Native-only:** on wasm this is not captured and reaches wgpu's default
    /// (panicking) handler. Covers `backdrop-blur`'s own creations only — allocations inside
    /// `egui_wgpu` are outside this crate's reach. Recoverable: do not present the frame, re-request a
    /// repaint, and retry unfrosted or shed surfaces.
    #[error("the device ran out of memory allocating a blur resource")]
    DeviceOutOfMemory {
        /// The backend's underlying out-of-memory error.
        #[source]
        source: BackendError,
    },

    /// The caller's target color format is not on the backend's supported-composite allowlist.
    /// Distinct from a backend's own must-match-format validation (DESIGN §4.4/§4.5).
    #[error("target format {format} is not a supported render target for the blur composite")]
    UnsupportedTarget {
        /// The rejected format, captured as text at the backend boundary because core cannot
        /// name `wgpu::TextureFormat` (a deliberate `String` exception, documented in DESIGN §4.5).
        format: String,
    },

    /// The grab-pass backend could not produce a sampleable source from the live framebuffer.
    /// (Reserved for the deferred glow path; the socket exists now so adding it is not a core
    /// rewrite.)
    #[error("the grab source could not be produced from the framebuffer for region {region}")]
    GrabFailed {
        /// The region the grab was attempted for. A [`GlRegion`] (GL bottom-left), **not** a
        /// reinterpreted [`Region`]: this is a human-facing error, and `GlRegion`'s `Display`
        /// marks the origin bottom-left so a debugger cannot misread it against `Region`'s
        /// top-left convention.
        ///
        /// [`Region`]: crate::Region
        region: GlRegion,
        /// The backend's underlying error.
        #[source]
        source: BackendError,
    },

    /// The live GL context lacks a capability the grab-pass backend requires (too-old GL/GLES,
    /// a missing float-render extension). Raised at backend construction, before any frame.
    #[error("the GL context does not support the blur backend: {detail}")]
    UnsupportedContext {
        /// What was required vs. found, captured as text because core cannot name a `glow`
        /// version/extension type (the same documented `String` exception as
        /// [`UnsupportedTarget`](Self::UnsupportedTarget), DESIGN §4.5).
        detail: String,
    },
}

/// Which GPU resource a [`BlurError::ResourceCreation`] refers to — named so a failure points
/// at one resource, not "something in prepare".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlurStage {
    /// A ping-pong scratch texture in the blur chain.
    PingPongTexture,
    /// The downsample render pipeline.
    DownsamplePipeline,
    /// The upsample render pipeline.
    UpsamplePipeline,
    /// The final composite render pipeline (built per target format).
    CompositePipeline,
    /// The uniform buffer carrying the resolved mask/tint/offsets.
    UniformBuffer,
    /// A bind group wiring textures/uniforms to a pipeline.
    BindGroup,
    /// A shader stage failed to compile (the immediate-mode glow path: `glCompileShader`).
    ShaderCompile,
    /// A linked GL program failed to link its compiled stages (`glLinkProgram`).
    ProgramLink,
    /// A GL framebuffer object could not be created or was incomplete (grab / resolve / scratch).
    Framebuffer,
    /// A GL vertex array object (the shared fullscreen-triangle VAO) could not be created.
    VertexArray,
}

impl std::fmt::Display for BlurStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::PingPongTexture => "ping-pong scratch texture",
            Self::DownsamplePipeline => "downsample pipeline",
            Self::UpsamplePipeline => "upsample pipeline",
            Self::CompositePipeline => "composite pipeline",
            Self::UniformBuffer => "uniform buffer",
            Self::BindGroup => "bind group",
            Self::ShaderCompile => "shader",
            Self::ProgramLink => "shader program",
            Self::Framebuffer => "framebuffer",
            Self::VertexArray => "vertex array",
        };
        f.write_str(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Scale;

    #[test]
    fn resource_creation_display_names_the_stage() {
        let err = BlurError::ResourceCreation {
            stage: BlurStage::CompositePipeline,
            source: "device lost".into(),
        };
        assert_eq!(
            err.to_string(),
            "failed to create the composite pipeline while preparing the blur"
        );
    }

    #[test]
    fn unsupported_target_display_includes_the_format() {
        let err = BlurError::UnsupportedTarget {
            format: "Rgba8Snorm".to_owned(),
        };
        assert!(err.to_string().contains("Rgba8Snorm"));
    }

    #[test]
    fn error_source_chains_to_the_backend_error() {
        let err = BlurError::ResourceCreation {
            stage: BlurStage::PingPongTexture,
            source: "out of memory".into(),
        };
        let source = std::error::Error::source(&err).expect("a backend source is attached");
        assert_eq!(source.to_string(), "out of memory");
    }

    #[test]
    fn device_out_of_memory_display_and_source_chain() {
        let err = BlurError::DeviceOutOfMemory {
            source: "device out of memory".into(),
        };
        assert!(err.to_string().contains("ran out of memory"));
        let source = std::error::Error::source(&err).expect("a backend source is attached");
        assert_eq!(source.to_string(), "device out of memory");
    }

    #[test]
    fn grab_failed_display_includes_the_region() {
        let err = BlurError::GrabFailed {
            region: GlRegion::from_bottom_px([0, 0], [10, 10], Scale::default()),
            source: "no framebuffer".into(),
        };
        // The message embeds the bottom-left-marked region, so a debugger reads the orientation.
        assert!(err.to_string().contains("region"));
        assert!(err.to_string().contains("origin-bl"));
    }

    #[test]
    fn stage_display_covers_every_variant() {
        for stage in [
            BlurStage::PingPongTexture,
            BlurStage::DownsamplePipeline,
            BlurStage::UpsamplePipeline,
            BlurStage::CompositePipeline,
            BlurStage::UniformBuffer,
            BlurStage::BindGroup,
            BlurStage::ShaderCompile,
            BlurStage::ProgramLink,
            BlurStage::Framebuffer,
            BlurStage::VertexArray,
        ] {
            assert!(!stage.to_string().is_empty());
        }
    }

    #[test]
    fn unsupported_context_display_includes_the_detail() {
        let err = BlurError::UnsupportedContext {
            detail: "requires GL 3.3 / GLES 3.0, found GL 2.1".to_owned(),
        };
        assert!(
            err.to_string()
                .contains("requires GL 3.3 / GLES 3.0, found GL 2.1")
        );
    }
}
