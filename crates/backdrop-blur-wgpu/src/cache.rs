//! Pure, GPU-free policy + keys for the wgpu backend: the ping-pong cache key, the physical
//! radius → Gaussian-kernel resolution, and the target-format allowlist. These are the
//! default-tier-testable heart; the GPU resources they key live in [`crate::WgpuBlur`].

use wgpu::TextureFormat;

/// The internal scratch format — linear HDR, filterable, usable as both a render target and a
/// sampled texture. Fixed for every blur chain; only the composite touches the caller's format.
pub(crate) const SCRATCH_FORMAT: TextureFormat = TextureFormat::Rgba16Float;

/// Upper bound on the separable-Gaussian tap radius, so a huge `BlurStrength` cannot blow up the
/// per-fragment loop. Tooltip/dialog blur sits far below this.
pub(crate) const MAX_GAUSSIAN_RADIUS: i32 = 64;

/// Keys a ping-pong scratch chain. `levels` is `1` for the separable Gaussian (no downsampling);
/// the field exists so the dual-Kawase increment can key its mip depth without a new type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PingPongKey {
    pub size: [u32; 2],
    pub levels: u32,
}

/// A resolved separable-Gaussian kernel: the standard deviation and the half-width (taps each
/// side of center). `tap_radius == 0` is a single-tap pass-through (no blur).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GaussianKernel {
    pub sigma: f32,
    pub tap_radius: i32,
}

/// Resolve a physical-pixel blur radius to a Gaussian kernel. `sigma ≈ radius / 3` (three sigma
/// spans the visual radius), floored at `0.5` so the shader's `i / sigma` is always finite even
/// at `tap_radius == 0`; the tap radius is the rounded physical radius, clamped to the max.
pub(crate) fn resolve_gaussian(physical_radius: f32) -> GaussianKernel {
    let tap_radius = (physical_radius.round() as i32).clamp(0, MAX_GAUSSIAN_RADIUS);
    let sigma = (physical_radius / 3.0).max(0.5);
    GaussianKernel { sigma, tap_radius }
}

/// Whether the composite shader must manually re-encode linear→sRGB for `format`, or `None` if
/// `format` is not a supported composite target (→ `BlurError::UnsupportedTarget`). `*Srgb`
/// targets encode in hardware; `Unorm` gamma targets (the egui case) need the manual encode;
/// float targets stay linear.
pub(crate) fn composite_encode_srgb(format: TextureFormat) -> Option<bool> {
    match format {
        TextureFormat::Rgba8UnormSrgb | TextureFormat::Bgra8UnormSrgb => Some(false),
        TextureFormat::Rgba8Unorm | TextureFormat::Bgra8Unorm => Some(true),
        TextureFormat::Rgba16Float => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn resolve_gaussian_zero_radius_is_single_tap_passthrough() {
        let k = resolve_gaussian(0.0);
        assert_eq!(k.tap_radius, 0);
        assert!(
            k.sigma > 0.0,
            "sigma must stay positive so i/sigma is finite"
        );
    }

    #[test]
    fn resolve_gaussian_sets_sigma_to_a_third_of_radius() {
        let k = resolve_gaussian(15.0);
        assert_eq!(k.tap_radius, 15);
        assert!(close(k.sigma, 5.0));
    }

    #[test]
    fn resolve_gaussian_clamps_tap_radius_to_max() {
        let k = resolve_gaussian(1000.0);
        assert_eq!(k.tap_radius, MAX_GAUSSIAN_RADIUS);
    }

    #[test]
    fn composite_encode_srgb_allowlist() {
        // Hardware-encoding sRGB targets.
        assert_eq!(
            composite_encode_srgb(TextureFormat::Rgba8UnormSrgb),
            Some(false)
        );
        assert_eq!(
            composite_encode_srgb(TextureFormat::Bgra8UnormSrgb),
            Some(false)
        );
        // Gamma Unorm targets (egui) need the manual encode.
        assert_eq!(composite_encode_srgb(TextureFormat::Rgba8Unorm), Some(true));
        assert_eq!(composite_encode_srgb(TextureFormat::Bgra8Unorm), Some(true));
        // Linear float target.
        assert_eq!(
            composite_encode_srgb(TextureFormat::Rgba16Float),
            Some(false)
        );
        // Unsupported.
        assert_eq!(composite_encode_srgb(TextureFormat::R8Unorm), None);
    }

    #[test]
    fn ping_pong_key_distinguishes_size_and_levels() {
        let a = PingPongKey {
            size: [100, 80],
            levels: 1,
        };
        let b = PingPongKey {
            size: [100, 80],
            levels: 2,
        };
        let c = PingPongKey {
            size: [80, 100],
            levels: 1,
        };
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            a,
            PingPongKey {
                size: [100, 80],
                levels: 1
            }
        );
    }
}
