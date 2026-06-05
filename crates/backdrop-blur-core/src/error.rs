//! The return half of the seam contract. [`BlurError`] is a `thiserror` enum, but because
//! core has **no GPU dependency** it cannot name a backend error type (`wgpu::*`/`glow::*`
//! live in the GPU crates core forbids). It therefore carries a **boxed trait-object source**
//! ([`BackendError`]) — still a typed `Error` value that composes with `?` and `#[source]`,
//! never a flattened `String` model (DESIGN §4.5).
//!
//! A zero-sized/offscreen region is a **no-op**, not an error (`prepare` returns `Ok(None)`),
//! so there is deliberately no `ZeroSizedRegion` variant.

use crate::geometry::Region;

/// The boxed, typed source a backend attaches to a [`BlurError`]. Core cannot name
/// `wgpu::Error`/`glow` errors, so it accepts any `Send + Sync` standard error.
pub type BackendError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Everything that can go wrong producing a frosted surface. Each `Display` is a complete
/// sentence; `ResourceCreation.stage` localizes a 3 AM kiosk failure to the exact resource
/// that died.
#[derive(Debug, thiserror::Error)]
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
        /// The region the grab was attempted for.
        region: Region,
        /// The backend's underlying error.
        #[source]
        source: BackendError,
    },
}

/// Which GPU resource a [`BlurError::ResourceCreation`] refers to — named so a failure points
/// at one resource, not "something in prepare".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        };
        f.write_str(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Region, Scale};

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
    fn grab_failed_display_includes_the_region() {
        let err = BlurError::GrabFailed {
            region: Region {
                origin: [0, 0],
                size: [10, 10],
                scale: Scale::default(),
            },
            source: "no framebuffer".into(),
        };
        assert!(err.to_string().contains("region"));
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
        ] {
            assert!(!stage.to_string().is_empty());
        }
    }
}
