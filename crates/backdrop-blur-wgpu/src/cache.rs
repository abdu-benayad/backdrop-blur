//! Pure, GPU-free policy + keys for the wgpu backend: the ping-pong cache key, the physical
//! radius → Gaussian-kernel resolution, and the target-format allowlist. These are the
//! default-tier-testable heart; the GPU resources they key live in [`crate::WgpuBlur`].

use backdrop_blur_core::Region;
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

/// Map target-rect uv `[0,1]` onto the blurred scratch, which holds the **clipped** source region.
/// Returns `(offset, scale)` so that `scratch_uv = offset + target_uv * scale`. It is the identity
/// when the source region was fully in-bounds (`clipped == source_region`), and an inset otherwise —
/// the composite samples through this with `ClampToEdge`, so a source region clipped at a screen
/// edge still registers 1:1 with the content behind the glass instead of being stretched.
pub(crate) fn backdrop_uv_remap(source_region: &Region, clipped: &Region) -> ([f32; 2], [f32; 2]) {
    let [sx, sy] = [
        source_region.origin[0] as f32,
        source_region.origin[1] as f32,
    ];
    let [sw, sh] = [source_region.size[0] as f32, source_region.size[1] as f32];
    let [cx, cy] = [clipped.origin[0] as f32, clipped.origin[1] as f32];
    let [cw, ch] = [clipped.size[0] as f32, clipped.size[1] as f32];
    ([(sx - cx) / cw, (sy - cy) / ch], [sw / cw, sh / ch])
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

    fn region(origin: [u32; 2], size: [u32; 2]) -> Region {
        Region {
            origin,
            size,
            scale: backdrop_blur_core::Scale::new(1.0),
        }
    }

    #[test]
    fn backdrop_uv_remap_is_identity_when_unclipped() {
        let r = region([50, 50], [100, 100]);
        let (offset, scale) = backdrop_uv_remap(&r, &r);
        assert!(close(offset[0], 0.0) && close(offset[1], 0.0));
        assert!(close(scale[0], 1.0) && close(scale[1], 1.0));
    }

    #[test]
    fn backdrop_uv_remap_insets_a_right_clipped_region() {
        // source runs 40px off the right edge; clip_to keeps the origin and shrinks width 100→60.
        let source = region([100, 50], [100, 80]);
        let clipped = region([100, 50], [60, 80]);
        let (offset, scale) = backdrop_uv_remap(&source, &clipped);
        // Origin is preserved by clip_to, so the offset is zero; only the scale compensates, so
        // target uv 1.0 maps past scratch uv 1.0 (the clipped-off part) and ClampToEdge holds it.
        assert!(close(offset[0], 0.0) && close(offset[1], 0.0));
        assert!(close(scale[0], 100.0 / 60.0) && close(scale[1], 1.0));
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
