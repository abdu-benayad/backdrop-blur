// Composite the frosted surface: sample the blurred backdrop, evaluate the rounded-rect SDF
// from the resolved mask, lay the tint film over it, and (per target format) re-encode to the
// target's color space. Drawn into the target_rect via a viewport, blended over whatever the
// host already put in the target (LoadOp::Load).
//
// The blur ran in linear light; `encode_srgb` re-encodes linear→sRGB for gamma targets
// (Rgba8Unorm/Bgra8Unorm, the egui case). For `*Srgb` targets the hardware encodes on write,
// and for float targets no encode is wanted — both set `encode_srgb = 0`.

struct CompositeParams {
    half_extents: vec2<f32>,   // target rect half-size, in physical px
    corner_radius_px: f32,     // clamped, in physical px
    encode_srgb: u32,          // 1 = manually linear→sRGB encode the output
    tint: vec4<f32>,           // linear, straight alpha (alpha = film opacity)
    rect_size: vec2<f32>,      // target rect size in physical px (for the SDF + edge AA)
    _pad: vec2<f32>,
};

@group(0) @binding(0) var blurred_tex: texture_2d<f32>;
@group(0) @binding(1) var blurred_samp: sampler;
@group(0) @binding(2) var<uniform> params: CompositeParams;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var out: VsOut;
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    out.uv = vec2<f32>(x, y);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

// Signed distance to a rounded rectangle centered at the origin (negative inside).
fn sd_rounded_rect(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let cutoff = c <= vec3<f32>(0.0031308);
    let low = c * 12.92;
    let high = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(high, low, cutoff);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let blurred = textureSampleLevel(blurred_tex, blurred_samp, in.uv, 0.0);

    // Rounded-rect coverage, in pixel space, centered in the rect. 1px AA band at the edge.
    let p = (in.uv - vec2<f32>(0.5)) * params.rect_size;
    let d = sd_rounded_rect(p, params.half_extents, params.corner_radius_px);
    let coverage = 1.0 - smoothstep(0.0, 1.0, d);

    // Tint film over the blurred backdrop (straight-alpha "over", in linear light).
    var rgb = mix(blurred.rgb, params.tint.rgb, params.tint.a);
    if (params.encode_srgb == 1u) {
        rgb = linear_to_srgb(rgb);
    }
    return vec4<f32>(rgb, coverage);
}
