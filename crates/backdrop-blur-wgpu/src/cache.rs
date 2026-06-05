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

/// At/above this physical-pixel radius, dual-Kawase wins (downsampled, near-constant cost); below
/// it the separable Gaussian is just as good and cheaper to set up (research: the dual-filter
/// advantage needs a reasonably large radius, ≥ ~7px). The kept Gaussian path is the small-radius
/// fallback; dual-Kawase is the production algorithm for large/animated blur.
pub(crate) const KAWASE_THRESHOLD_PX: f32 = 16.0;

/// Max dual-Kawase mip depth, so a huge radius cannot build an unbounded pyramid (1/64 downscale).
pub(crate) const MAX_KAWASE_LEVELS: u32 = 6;

/// Whether `physical_radius` should use dual-Kawase (vs the Gaussian fallback).
pub(crate) fn use_dual_kawase(physical_radius: f32) -> bool {
    physical_radius >= KAWASE_THRESHOLD_PX
}

/// Dual-Kawase mip depth `N` for a physical radius. Each down/up pass ~doubles the effective
/// radius, so `N ≈ log2(radius)`, clamped to `[1, MAX_KAWASE_LEVELS]`. The pyramid then has
/// `N + 1` levels (level 0 = full, level `i` = `base >> i`).
pub(crate) fn resolve_kawase_levels(physical_radius: f32) -> u32 {
    let levels = physical_radius.max(2.0).log2().round() as i32;
    levels.clamp(1, MAX_KAWASE_LEVELS as i32) as u32
}

/// The size of mip level `level` for a pyramid whose level 0 is `base` (each level halves, floored
/// at 1px so a tall/thin region never collapses to zero).
pub(crate) fn kawase_level_size(base: [u32; 2], level: u32) -> [u32; 2] {
    [(base[0] >> level).max(1), (base[1] >> level).max(1)]
}

/// The half-texel sampling offset for a dual-Kawase pass that **samples** a texture of `size`
/// (KWin's `halfpixel` convention: `0.5 / size`).
pub(crate) fn kawase_halfpixel(size: [u32; 2]) -> [f32; 2] {
    [0.5 / size[0] as f32, 0.5 / size[1] as f32]
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
    fn use_dual_kawase_switches_at_the_threshold() {
        assert!(!use_dual_kawase(KAWASE_THRESHOLD_PX - 0.1));
        assert!(use_dual_kawase(KAWASE_THRESHOLD_PX));
        assert!(use_dual_kawase(40.0));
    }

    #[test]
    fn resolve_kawase_levels_grows_logarithmically_and_clamps() {
        assert_eq!(resolve_kawase_levels(16.0), 4); // log2(16) = 4
        assert_eq!(resolve_kawase_levels(32.0), 5);
        assert_eq!(resolve_kawase_levels(10000.0), MAX_KAWASE_LEVELS);
        assert_eq!(resolve_kawase_levels(2.0), 1); // clamped floor
    }

    #[test]
    fn kawase_level_size_halves_and_floors_at_one() {
        assert_eq!(kawase_level_size([200, 100], 0), [200, 100]);
        assert_eq!(kawase_level_size([200, 100], 1), [100, 50]);
        assert_eq!(kawase_level_size([200, 100], 2), [50, 25]);
        // A thin axis floors at 1 instead of collapsing to 0.
        assert_eq!(kawase_level_size([200, 1], 4), [12, 1]);
    }

    #[test]
    fn kawase_halfpixel_is_half_a_texel() {
        assert_eq!(kawase_halfpixel([100, 50]), [0.5 / 100.0, 0.5 / 50.0]);
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
