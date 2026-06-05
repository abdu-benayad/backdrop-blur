//! The material — *what kind of glass*. The knobs a caller turns to describe a frosted
//! surface: how much blur, what tint film, how round the corners. All logical (in points);
//! the backend resolves them to physical pixels against a region's [`Scale`].

use crate::geometry::Scale;

/// Blur radius in **logical points**.
///
/// Core resolves this to a physical-pixel radius ([`Self::to_physical_radius`]) — the one
/// algorithm-*agnostic* step. The mapping from that radius to algorithm-specific parameters
/// (a separable-Gaussian sigma, or dual-Kawase levels + per-pass sampling offsets) lives in
/// the **backend**, not here: there is no closed-form map, and the parameters differ per
/// algorithm (DESIGN §4.2). Keeping only the agnostic resolution in core is why core has no
/// notion of "levels" — that is the wgpu crate's, pinned by its own offset test (IMPL §2b′).
///
/// Non-negative by construction: a negative radius is meaningless, so [`Self::new`] clamps to
/// `0` (which the backend reads as "no blur"), keeping the type total.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlurStrength(f32);

impl BlurStrength {
    /// Construct from logical points, clamping negatives to `0`.
    pub fn new(points: f32) -> Self {
        Self(points.max(0.0))
    }

    /// The blur radius in logical points (always `>= 0`).
    pub fn points(self) -> f32 {
        self.0
    }

    /// Logical points × the region's scale = physical-pixel blur radius. The single
    /// algorithm-agnostic resolution; the backend maps this radius onto its own kernel.
    pub fn to_physical_radius(self, scale: Scale) -> f32 {
        self.0 * scale.factor()
    }
}

/// A straight-alpha color in **linear light**. RGB are linear (already gamma-decoded); alpha
/// is coverage (never gamma-encoded). The blur convolution runs in linear light, so a tint
/// authored in sRGB must be decoded first — that is exactly what [`Self::from_srgb_unmultiplied`]
/// does, so callers never hand the backend gamma-encoded tint values (DESIGN §4.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearRgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl LinearRgba {
    /// Build from channels that are *already* linear.
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Decode straight-alpha sRGB bytes to linear light: RGB through the sRGB EOTF, alpha
    /// linearly. This is the gamma decode the linear-space convolution requires.
    pub fn from_srgb_unmultiplied(rgba: [u8; 4]) -> Self {
        let [r, g, b, a] = rgba;
        Self {
            r: srgb_to_linear(f32::from(r) / 255.0),
            g: srgb_to_linear(f32::from(g) / 255.0),
            b: srgb_to_linear(f32::from(b) / 255.0),
            a: f32::from(a) / 255.0,
        }
    }
}

/// The sRGB electro-optical transfer function (gamma → linear) for one normalized channel.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The glass film painted over the blurred backdrop. The wrapped color is linear-light; its
/// alpha is the **film opacity** (how much of the tint shows over the blur).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tint(LinearRgba);

impl Tint {
    /// Wrap an already-linear color as the film.
    pub fn new(color: LinearRgba) -> Self {
        Self(color)
    }

    /// Convenience: a film authored as straight-alpha sRGB bytes, decoded to linear.
    pub fn from_srgb_unmultiplied(rgba: [u8; 4]) -> Self {
        Self(LinearRgba::from_srgb_unmultiplied(rgba))
    }

    /// The film color in linear light.
    pub fn color(self) -> LinearRgba {
        self.0
    }
}

/// Corner radius in **logical points**. Resolves (× the target region's [`Scale`]) to a
/// physical-pixel radius, clamped so it can never overshoot the surface (the clamp lives in
/// [`crate::ResolvedMask::from_target`]). Non-negative by construction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CornerRadius(f32);

impl CornerRadius {
    /// Construct from logical points, clamping negatives to `0` (square corners).
    pub fn new(points: f32) -> Self {
        Self(points.max(0.0))
    }

    /// The corner radius in logical points (always `>= 0`).
    pub fn points(self) -> f32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn blur_strength_new_clamps_negative_to_zero() {
        assert_eq!(BlurStrength::new(-3.0).points(), 0.0);
        assert_eq!(BlurStrength::new(8.0).points(), 8.0);
    }

    #[test]
    fn to_physical_radius_multiplies_by_scale() {
        let r = BlurStrength::new(8.0).to_physical_radius(Scale::new(2.0));
        assert!(close(r, 16.0));
    }

    #[test]
    fn to_physical_radius_of_zero_strength_is_zero() {
        let r = BlurStrength::new(0.0).to_physical_radius(Scale::new(3.0));
        assert!(close(r, 0.0));
    }

    #[test]
    fn corner_radius_new_clamps_negative_to_zero() {
        assert_eq!(CornerRadius::new(-1.0).points(), 0.0);
        assert_eq!(CornerRadius::new(12.0).points(), 12.0);
    }

    #[test]
    fn from_srgb_unmultiplied_maps_endpoints_exactly() {
        let black = LinearRgba::from_srgb_unmultiplied([0, 0, 0, 255]);
        assert!(close(black.r, 0.0) && close(black.g, 0.0) && close(black.b, 0.0));
        assert!(close(black.a, 1.0));

        let white = LinearRgba::from_srgb_unmultiplied([255, 255, 255, 255]);
        assert!(close(white.r, 1.0) && close(white.g, 1.0) && close(white.b, 1.0));
    }

    #[test]
    fn from_srgb_unmultiplied_decodes_midtone_through_eotf() {
        // sRGB 188/255 ≈ 0.737 gamma → ≈ 0.502 linear (the classic "perceptual half").
        let mid = LinearRgba::from_srgb_unmultiplied([188, 188, 188, 128]);
        assert!(close(mid.r, 0.502_886_5));
        // Alpha is linear, not gamma-decoded.
        assert!(close(mid.a, 128.0 / 255.0));
    }

    #[test]
    fn from_srgb_unmultiplied_uses_linear_segment_near_black() {
        // Below the 0.04045 knee the transfer is the linear c/12.92 segment.
        let dark = LinearRgba::from_srgb_unmultiplied([2, 2, 2, 255]);
        let expected = (2.0 / 255.0) / 12.92;
        assert!(close(dark.r, expected));
    }

    #[test]
    fn tint_from_srgb_decodes_its_wrapped_color() {
        let tint = Tint::from_srgb_unmultiplied([255, 255, 255, 64]);
        assert!(close(tint.color().r, 1.0));
        assert!(close(tint.color().a, 64.0 / 255.0));
    }
}
