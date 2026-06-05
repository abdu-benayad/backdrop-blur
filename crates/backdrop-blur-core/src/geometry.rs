//! Geometry and the request bundle. The seam speaks **physical pixels**: a [`Region`] is a
//! physical-pixel rectangle that carries its own logical→physical [`Scale`], so the source
//! intermediate and the swapchain target can differ in DPI without a single global factor
//! (DESIGN §4.1). [`ResolvedMask`] is the one shader input core computes; [`BlurRequest`] is
//! the bundle that crosses the seam.

use crate::material::{BlurStrength, CornerRadius, Tint};

/// A logical→physical scale factor (DPI) for one region. Strictly positive by construction:
/// a zero or negative factor would make the resolution math degenerate, so [`Self::new`]
/// floors it at the smallest positive `f32`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scale(f32);

impl Scale {
    /// Construct from a factor, flooring at the smallest positive `f32`.
    pub fn new(factor: f32) -> Self {
        Self(factor.max(f32::MIN_POSITIVE))
    }

    /// The scale factor (always `> 0`).
    pub fn factor(self) -> f32 {
        self.0
    }
}

impl Default for Scale {
    /// 1 physical pixel per logical point.
    fn default() -> Self {
        Self(1.0)
    }
}

/// A physical-pixel rectangle carrying its own logical→physical [`Scale`]. Construct with a
/// struct literal so the two `[u32; 2]` fields are named at the call site (no positional
/// origin/size swap is possible).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    /// Top-left corner in physical pixels.
    pub origin: [u32; 2],
    /// Width and height in physical pixels.
    pub size: [u32; 2],
    /// This region's logical→physical scale.
    pub scale: Scale,
}

impl Region {
    /// A region is a **no-op** for blur when it has zero area or lies fully outside the
    /// source texture (its origin sits at or beyond the source extent). For such a region
    /// `prepare` returns `Ok(None)` — valid input, not an error (DESIGN §4.4/§4.5). This is
    /// the *predicate*; the no-op *behavior* is asserted against a real backend (IMPL §2b).
    ///
    /// `source_extent` is the `[width, height]` of the texture the region indexes into.
    pub fn is_empty_or_offscreen(&self, source_extent: [u32; 2]) -> bool {
        self.size[0] == 0
            || self.size[1] == 0
            || self.origin[0] >= source_extent[0]
            || self.origin[1] >= source_extent[1]
    }
}

/// What core computes for the shader: the target surface's half-extents (physical px) and the
/// physical corner radius, **clamped to `min(half_extent)`** so the rounded-rect SDF can never
/// overshoot into a malformed shape. The per-pixel SDF is the shader's job; this is its
/// resolved, GPU-agnostic input — which is exactly what makes the clamp headless-testable
/// (DESIGN §4.3, §11).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedMask {
    /// Half the target rect's width and height, in physical pixels.
    pub half_extents: [f32; 2],
    /// The clamped physical corner radius, in physical pixels.
    pub corner_radius_px: f32,
}

impl ResolvedMask {
    /// Resolve the mask for a target rect: half-extents from its size, and the corner radius
    /// scaled to physical pixels then clamped to `min(half_width, half_height)`.
    pub fn from_target(target: &Region, corner_radius: CornerRadius) -> Self {
        let half_extents = [target.size[0] as f32 / 2.0, target.size[1] as f32 / 2.0];
        let max_radius = half_extents[0].min(half_extents[1]).max(0.0);
        let corner_radius_px =
            (corner_radius.points() * target.scale.factor()).clamp(0.0, max_radius);
        Self {
            half_extents,
            corner_radius_px,
        }
    }
}

/// The one backend-agnostic bundle that crosses the seam. `source_region` says where the
/// backdrop lives in the source texture; `target_rect` says where to composite the frosted
/// surface in the target. Both carry independent sizes and scales (DESIGN §4.1/§4.3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlurRequest {
    /// Where the backdrop to blur lives in the `source` texture (physical px + its scale).
    pub source_region: Region,
    /// Where to composite the frosted surface in the `target` (physical px + its scale).
    pub target_rect: Region,
    /// How much blur (logical points).
    pub strength: BlurStrength,
    /// The glass film.
    pub tint: Tint,
    /// How round the surface corners are (logical points).
    pub corner_radius: CornerRadius,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::material::LinearRgba;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn scale_new_floors_nonpositive_to_positive() {
        assert!(Scale::new(0.0).factor() > 0.0);
        assert!(Scale::new(-2.0).factor() > 0.0);
        assert!(close(Scale::new(2.5).factor(), 2.5));
    }

    #[test]
    fn scale_default_is_one() {
        assert!(close(Scale::default().factor(), 1.0));
    }

    #[test]
    fn is_empty_or_offscreen_true_for_zero_area() {
        let region = Region {
            origin: [0, 0],
            size: [0, 10],
            scale: Scale::default(),
        };
        assert!(region.is_empty_or_offscreen([100, 100]));
    }

    #[test]
    fn is_empty_or_offscreen_true_when_origin_past_extent() {
        let region = Region {
            origin: [100, 0],
            size: [10, 10],
            scale: Scale::default(),
        };
        assert!(region.is_empty_or_offscreen([100, 100]));
    }

    #[test]
    fn is_empty_or_offscreen_false_for_in_bounds_region() {
        let region = Region {
            origin: [10, 10],
            size: [20, 20],
            scale: Scale::default(),
        };
        assert!(!region.is_empty_or_offscreen([100, 100]));
    }

    #[test]
    fn resolved_mask_half_extents_are_half_the_size() {
        let target = Region {
            origin: [0, 0],
            size: [80, 40],
            scale: Scale::new(1.0),
        };
        let mask = ResolvedMask::from_target(&target, CornerRadius::new(8.0));
        assert!(close(mask.half_extents[0], 40.0));
        assert!(close(mask.half_extents[1], 20.0));
        assert!(close(mask.corner_radius_px, 8.0));
    }

    #[test]
    fn resolved_mask_scales_corner_radius_to_physical() {
        let target = Region {
            origin: [0, 0],
            size: [200, 200],
            scale: Scale::new(2.0),
        };
        // 8 logical points × 2.0 = 16 physical px, well under the 100px half-extent.
        let mask = ResolvedMask::from_target(&target, CornerRadius::new(8.0));
        assert!(close(mask.corner_radius_px, 16.0));
    }

    #[test]
    fn resolved_mask_clamps_radius_to_min_half_extent() {
        let target = Region {
            origin: [0, 0],
            size: [40, 100],
            scale: Scale::new(1.0),
        };
        // A 999pt radius cannot exceed min(half) = min(20, 50) = 20.
        let mask = ResolvedMask::from_target(&target, CornerRadius::new(999.0));
        assert!(close(mask.corner_radius_px, 20.0));
    }

    #[test]
    fn blur_request_constructs_by_named_fields() {
        // Compile-time evidence that the request is assembled by name (swap-safe).
        let region = Region {
            origin: [0, 0],
            size: [100, 60],
            scale: Scale::default(),
        };
        let request = BlurRequest {
            source_region: region,
            target_rect: region,
            strength: BlurStrength::new(12.0),
            tint: Tint::new(LinearRgba::new(0.1, 0.1, 0.12, 0.7)),
            corner_radius: CornerRadius::new(10.0),
        };
        assert_eq!(request.strength.points(), 12.0);
        assert!(close(request.tint.color().a, 0.7));
    }
}
