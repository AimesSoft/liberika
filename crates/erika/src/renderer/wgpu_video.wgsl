// WGSL port of the Metal `VIDEO_SHADER_SOURCE` in `renderer/metal/apple.rs`.
// Kept line-for-line equivalent so the wgpu backend produces the same pixels as
// the native Metal renderer for a given frame and color pipeline.

struct VideoUniforms {
    is_p010: u32,
    full_range: u32,
    source_transfer: u32,
    target_transfer: u32,
    tone_map: u32,
    edr_output: u32,
    input_mode: u32,
    scene_linear: u32,
    nits: vec4<f32>,
    luma_coefficients: vec4<f32>,
    gamut_matrix_rows: array<vec4<f32>, 3>,
    ipt_matrix_rows: array<vec4<f32>, 9>,
    tone_map_extra: vec4<f32>,
    tone_map_coeffs: vec4<f32>,
    gamut_lut_enabled: u32,
    _gamut_primaries: u32,
    _gamut_reserved0: u32,
    _gamut_reserved1: u32,
    dovi_flags: vec4<f32>,
    dovi_pivots: array<vec4<f32>, 6>,
    dovi_bounds: array<vec4<f32>, 3>,
    dovi_coefficients: array<vec4<f32>, 24>,
    dovi_mmr: array<vec4<f32>, 144>,
    dovi_nonlinear_matrix: array<vec4<f32>, 3>,
    dovi_nonlinear_offset: vec4<f32>,
    dovi_lms_matrix: array<vec4<f32>, 3>,
};

@group(0) @binding(0) var<uniform> uniforms: VideoUniforms;
@group(0) @binding(1) var luma_texture: texture_2d<f32>;
@group(0) @binding(2) var chroma_texture: texture_2d<f32>;
@group(0) @binding(3) var video_sampler: sampler;
@group(0) @binding(4) var gamut_lut: texture_3d<f32>;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
};

fn source_peak_nits() -> f32 {
    return max(uniforms.nits.x, 1.0);
}

fn target_peak_nits() -> f32 {
    return max(uniforms.nits.y, 1.0);
}

fn source_reference_white_nits() -> f32 {
    return max(uniforms.nits.z, 1.0);
}

fn target_reference_white_nits() -> f32 {
    return max(uniforms.nits.w, 1.0);
}

fn pq_eotf(encoded: f32) -> f32 {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = pow(max(encoded, 0.0), 1.0 / m2);
    let num = max(p - c1, 0.0);
    let den = max(c2 - c3 * p, 0.000001);
    return pow(num / den, 1.0 / m1);
}

fn pq_inverse_eotf(normalized_nits: f32) -> f32 {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let p = pow(clamp(normalized_nits, 0.0, 1.0), m1);
    return pow((c1 + c2 * p) / max(1.0 + c3 * p, 0.000001), m2);
}

// BT.2100 HLG inverse OETF: nonlinear signal E' to scene linear light in
// [0, 1]. Mirrors the Rust reference implementation in
// `renderer/pipeline.rs` tests (`hlg_inverse_oetf`).
fn hlg_inverse_oetf(encoded: f32) -> f32 {
    let a = 0.17883277;
    let b = 0.28466892;
    let c = 0.55991073;
    let e = max(encoded, 0.0);
    if (e <= 0.5) {
        return e * e / 3.0;
    }
    return (exp((e - c) / a) + b) / 12.0;
}

fn transfer_to_source_reference_linear(rgb_in: vec3<f32>) -> vec3<f32> {
    let rgb = max(rgb_in, vec3<f32>(0.0));
    if (uniforms.source_transfer == 3u) {
        let pq_absolute_peak_nits = 10000.0;
        return vec3<f32>(pq_eotf(rgb.r), pq_eotf(rgb.g), pq_eotf(rgb.b))
            * (pq_absolute_peak_nits / source_reference_white_nits());
    }
    if (uniforms.source_transfer == 4u) {
        // HLG: inverse OETF to scene linear, then the BT.2100 OOTF (system
        // gamma 1.2 at the 1000 nit nominal peak) to display linear,
        // normalized to source reference white like the PQ branch above.
        let hlg_nominal_peak_nits = 1000.0;
        let hlg_system_gamma = 1.2;
        let scene = vec3<f32>(
            hlg_inverse_oetf(rgb.r),
            hlg_inverse_oetf(rgb.g),
            hlg_inverse_oetf(rgb.b)
        );
        let scene_luma = max(dot(uniforms.luma_coefficients.xyz, scene), 0.000001);
        return scene * (hlg_nominal_peak_nits * pow(scene_luma, hlg_system_gamma - 1.0)
            / source_reference_white_nits());
    }
    if (uniforms.source_transfer == 1u) {
        return pow(rgb, vec3<f32>(2.2));
    }
    if (uniforms.source_transfer == 2u) {
        return pow(rgb, vec3<f32>(2.4));
    }
    return rgb;
}

fn source_reference_to_nits(rgb: vec3<f32>) -> vec3<f32> {
    return max(rgb, vec3<f32>(0.0)) * source_reference_white_nits();
}

fn pq_code(nits: f32) -> f32 {
    return pq_inverse_eotf(clamp(nits, 0.0, 10000.0) / 10000.0);
}

fn nits_from_pq(code: f32) -> f32 {
    return 10000.0 * pq_eotf(clamp(code, 0.0, 1.0));
}

// libplacebo pl_smoothstep with arbitrary edge order (WGSL `smoothstep` has
// undefined results when edge0 >= edge1, and libplacebo's knee tuning term
// deliberately uses reversed edges).
fn sstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// libplacebo st2094_pick_knee evaluated on absolute PQ codes. The source
// pivot follows the scene average luminance when known and stays within
// [10%, 80%] of the range; the destination pivot rescales it into the output
// range and then adapts towards the 1:1 line (knee_adaptation 0.4).
fn st2094_pick_knee(src_min: f32, src_max: f32, src_avg: f32, dst_min: f32, dst_max: f32) -> vec2<f32> {
    let knee_adaptation = 0.4;
    let min_knee = 0.1;
    let max_knee = 0.8;
    let def_knee = 0.4;
    let src_knee_min = mix(src_min, src_max, min_knee);
    let src_knee_max = mix(src_min, src_max, max_knee);
    let dst_knee_min = mix(dst_min, dst_max, min_knee);
    let dst_knee_max = mix(dst_min, dst_max, max_knee);
    let fallback = mix(src_min, src_max, def_knee);
    let src_knee = clamp(select(fallback, src_avg, src_avg > 0.0), src_knee_min, src_knee_max);
    let knee_t = (src_knee - src_min) / max(src_max - src_min, 0.000001);
    let adapted = mix(dst_min, dst_max, knee_t);
    let tuning = 1.0 - sstep(max_knee, def_knee, knee_t) * sstep(min_knee, def_knee, knee_t);
    let adaptation = mix(knee_adaptation, 1.0, tuning);
    let dst_knee = clamp(mix(src_knee, adapted, adaptation), dst_knee_min, dst_knee_max);
    return vec2<f32>(src_knee, clamp(dst_knee, dst_knee_min, dst_knee_max));
}

// The tone-map curve evaluated on the IPT intensity axis (PQ codes),
// mirroring libplacebo's tone-map functions. `param` is the per-operator
// curve parameter from ToneMapConfig::curve_param (0 = operator default).
fn tone_map_curve_pq(x_in: f32, param: f32) -> f32 {
    let src_peak = source_peak_nits();
    let dst_peak = target_peak_nits();
    let src_avg = uniforms.tone_map_extra.y;
    let dst_black = uniforms.tone_map_extra.z;
    let in_min = 0.0;
    let in_max = max(pq_code(src_peak), 0.000001);
    let out_min = pq_code(dst_black);
    let out_max = max(pq_code(dst_peak), 0.000001);
    let out_range = max(out_max - out_min, 0.000001);
    let x = clamp(x_in, in_min, in_max);
    if (uniforms.tone_map == 0u) {
        // Clip: values within the source range pass through untouched.
        return x;
    }
    if (uniforms.tone_map == 1u) {
        // Reinhard (output-relative, libplacebo pl_tone_map_reinhard).
        //
        // libplacebo evaluates this curve in the linear PL_HDR_NORM domain
        // and only the PQ-domain operators (BT.2390, spline, ST 2094-10)
        // work on encoded values. Running it on PQ codes bends the curve's
        // meaning: against a 1000->203 nit target mid-tones darken badly, and
        // the Mobius knee lands far below its intended position. Rescale to
        // linear nits here and encode back at the end.
        let in_max_nits = max(src_peak, 0.000001);
        let out_min_nits = dst_black;
        let out_range_nits = max(dst_peak - dst_black, 0.000001);
        let peak = in_max_nits / out_range_nits;
        let contrast = select(0.5, param, param > 0.0);
        let offset = (1.0 - contrast) / max(contrast, 0.000001);
        let scale = (peak + offset) / peak;
        let t = clamp(nits_from_pq(x), 0.0, in_max_nits) / out_range_nits;
        let mapped = t / (t + offset) * scale;
        return pq_code(mapped * out_range_nits + out_min_nits);
    }
    if (uniforms.tone_map == 2u) {
        // Mobius: Möbius transform with a 1:1 linear region below the knee,
        // also evaluated in linear nits (see the Reinhard note above). The
        // knee j is relative to the output range, so the linear region ends
        // at dst_black + j * (dst_peak - dst_black).
        let in_max_nits = max(src_peak, 0.000001);
        let out_min_nits = dst_black;
        let out_range_nits = max(dst_peak - dst_black, 0.000001);
        let peak = in_max_nits / out_range_nits;
        let j = select(0.3, param, param > 0.0);
        let a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
        let b = (j * j - 2.0 * j * peak + peak) / max(peak - 1.0, 0.000001);
        let scale = (b * b + 2.0 * b * j + j * j) / (b - a);
        let t = clamp(nits_from_pq(x), 0.0, in_max_nits) / out_range_nits;
        let mapped = select(t, scale * (t + a) / (t + b), t > j);
        return pq_code(mapped * out_range_nits + out_min_nits);
    }
    if (uniforms.tone_map == 3u) {
        // ITU-R BT.2390 EETF with black-point compensation (the libplacebo
        // version also compensates target black; the earlier port skipped it).
        let knee_offset = select(1.0, param, param > 0.0);
        let max_lum = clamp(out_max / in_max, 0.0, 1.0);
        let min_lum = out_min / in_max;
        let ks = (1.0 + knee_offset) * max_lum - knee_offset;
        let bp = min(max(1.0 / max(min_lum, 0.000001), 0.0), 4.0);
        var u = x / in_max;
        if (ks < 1.0 && u > ks) {
            let tb = (u - ks) / (1.0 - ks);
            let tb2 = tb * tb;
            let tb3 = tb2 * tb;
            u = (2.0 * tb3 - 3.0 * tb2 + 1.0) * ks
                + (tb3 - 2.0 * tb2 + tb) * (1.0 - ks)
                + (-2.0 * tb3 + 3.0 * tb2) * max_lum;
        }
        if (u < 1.0) {
            u = u + min_lum * pow(1.0 - u, bp);
            let gain = select(1.0, 1.0 / (1.0 + min_lum / max_lum * pow(1.0 - max_lum, bp)), max_lum < 1.0);
            u = gain * (u - min_lum) + min_lum;
        }
        return u * in_max;
    }
    if (uniforms.tone_map == 4u) {
        // Spline: perceptually linear single-pivot polynomial, the default
        // tone map of libplacebo and mpv's gpu-next renderer.
        let contrast = select(0.3, param, param > 0.0);
        let fallback_avg = clamp(0.4 * src_peak, 100.0, 400.0);
        let effective_src_avg = select(fallback_avg, src_avg, src_avg > 0.0);
        let knee = st2094_pick_knee(
            in_min,
            in_max,
            pq_code(effective_src_avg),
            out_min,
            out_max
        );
        let src_pivot = knee.x;
        let dst_pivot = knee.y;
        let slope0 = (dst_pivot - out_min) / max(src_pivot - in_min, 0.000001);
        let ratio = clamp(1.5 * (in_max / out_max - 1.0), 0.2, 1.2);
        let slope = pow(slope0, (1.0 - contrast) * ratio);
        let in_min0 = in_min - src_pivot;
        let in_max0 = in_max - src_pivot;
        let out_min0 = out_min - dst_pivot;
        let out_max0 = out_max - dst_pivot;
        let pa = (out_min0 - slope * in_min0) / (in_min0 * in_min0);
        let qa = (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0 * in_max0);
        let qb = -3.0 * (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0);
        let xr = x - src_pivot;
        let mapped = select((pa * xr + slope) * xr, ((qa * xr + qb) * xr + slope) * xr, xr > 0.0);
        return mapped + dst_pivot;
    }
    if (uniforms.tone_map == 5u) {
        // ITU-R BT.2446 method A: Weber-law log compression from the source
        // peak envelope and a standardized S-curve (mpv's recommended curve
        // for well-mastered content).
        let phdr = 1.0 + 32.0 * pow(src_peak / 10000.0, 1.0 / 2.4);
        let psdr = 1.0 + 32.0 * pow(dst_peak / 10000.0, 1.0 / 2.4);
        var t = pow(nits_from_pq(x) / max(src_peak, 0.000001), 1.0 / 2.4);
        t = log(1.0 + (phdr - 1.0) * t) / log(phdr);
        if (t <= 0.7399) {
            t = 1.0770 * t;
        } else if (t < 0.9909) {
            t = (-1.1510 * t + 2.7811) * t - 0.6302;
        } else {
            t = 0.5 * t + 0.5;
        }
        t = (pow(psdr, t) - 1.0) / (psdr - 1.0);
        // BT.1886 EOTF from the target black point and peak.
        let lb = pow(max(dst_black, 0.0), 1.0 / 2.4);
        let lw = pow(max(dst_peak, 0.0), 1.0 / 2.4);
        return pq_code(pow((lw - lb) * t + lb, 2.4));
    }
    // SMPTE ST 2094-10 (DolbyVision's dynamic-metadata curve): rational
    // Möbius interpolation in absolute nits; coefficients are solved per
    // frame on the CPU from the same scene pivot.
    let c1 = uniforms.tone_map_coeffs.x;
    let c2 = uniforms.tone_map_coeffs.y;
    let c3 = uniforms.tone_map_coeffs.z;
    let x_nits = nits_from_pq(x);
    let y_nits = (c1 + c2 * x_nits) / max(1.0 + c3 * x_nits, 0.000001);
    return pq_code(clamp(y_nits, 0.0, 10000.0));
}

fn tone_map_nits(nits: vec3<f32>) -> vec3<f32> {
    if (uniforms.target_transfer == 3u) {
        // HDR10 output: convert primaries by the gamut matrix and clamp to
        // the PQ range (no tone mapping; the display does the HDR mapping).
        return clamp(
            apply_gamut_map(max(nits, vec3<f32>(0.0)) / source_reference_white_nits())
                * source_reference_white_nits(),
            vec3<f32>(0.0),
            vec3<f32>(10000.0)
        );
    }
    // libplacebo tone map: RGB in source primaries (absolute nits) to
    // HPE-LMS, PQ-encode, IPT, map the intensity axis and apply the
    // hue-preserving chroma rule, then decode back to RGB in the *source*
    // primaries. The gamut LUT (below) performs the single source->target
    // primaries conversion.
    let rgb = max(nits, vec3<f32>(0.0));
    let lms = vec3<f32>(
        dot(uniforms.ipt_matrix_rows[0].xyz, rgb),
        dot(uniforms.ipt_matrix_rows[1].xyz, rgb),
        dot(uniforms.ipt_matrix_rows[2].xyz, rgb)
    );
    let lmspq = vec3<f32>(pq_code(lms.r), pq_code(lms.g), pq_code(lms.b));
    var ipt = vec3<f32>(
        dot(vec3<f32>(0.4, 0.4, 0.2), lmspq),
        dot(vec3<f32>(4.455, -4.851, 0.396), lmspq),
        dot(vec3<f32>(0.8056, 0.3572, -1.1628), lmspq)
    );
    let i_orig = ipt.x;
    ipt.x = tone_map_curve_pq(ipt.x, uniforms.tone_map_extra.x);
    // Libplacebo's chroma rule: clamp the saturation boost when brightening
    // and desaturate (by the cubic hull term) when the mapping darkens.
    let hull = vec2<f32>(i_orig, ipt.x);
    let hull_c = ((hull - vec2<f32>(6.0)) * hull + vec2<f32>(9.0)) * hull;
    let ratio = min(i_orig / max(ipt.x, 0.000001), hull_c.y / max(hull_c.x, 0.000001));
    ipt = vec3<f32>(ipt.x, ipt.y * ratio, ipt.z * ratio);

    if (uniforms.gamut_lut_enabled != 0u) {
        // I axis spans the target's [black, peak] in PQ codes, matching
        // libplacebo's gamut.min_luma/max_luma (tone_map_extra.z is the
        // target black in nits, the same value the LUT was generated for).
        let lut_min = pq_code(uniforms.tone_map_extra.z);
        let lut_max = max(pq_code(target_peak_nits()), 0.000001);
        let lut_range = max(lut_max - lut_min, 0.000001);
        let pos = vec3<f32>(
            clamp((ipt.x - lut_min) / lut_range, 0.0, 1.0),
            clamp(2.0 * length(ipt.yz), 0.0, 1.0),
            0.5 + 0.5 * atan2(ipt.z, ipt.y) / 3.14159265
        );
        // libplacebo's texel_scale: lattice position -> texel-center coordinate.
        let idx = vec3<f32>(
            pos.x * (47.0 / 48.0) + 0.5 / 48.0,
            pos.y * (31.0 / 32.0) + 0.5 / 32.0,
            pos.z * (255.0 / 256.0) + 0.5 / 256.0
        );
        let sampled = textureSample(gamut_lut, video_sampler, idx).xyz;
        ipt = vec3<f32>(sampled.x, sampled.y - 0.5, sampled.z - 0.5);
    }
    let lmspq_out = vec3<f32>(
        dot(vec3<f32>(1.0, 0.0975689, 0.205226), ipt),
        dot(vec3<f32>(1.0, -0.113876, 0.133217), ipt),
        dot(vec3<f32>(1.0, 0.0326151, -0.676887), ipt)
    );
    let lms_out = vec3<f32>(
        nits_from_pq(lmspq_out.r),
        nits_from_pq(lmspq_out.g),
        nits_from_pq(lmspq_out.b)
    );
    return vec3<f32>(
        dot(uniforms.ipt_matrix_rows[6].xyz, lms_out),
        dot(uniforms.ipt_matrix_rows[7].xyz, lms_out),
        dot(uniforms.ipt_matrix_rows[8].xyz, lms_out)
    );
}

fn apply_gamut_map(rgb: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(uniforms.gamut_matrix_rows[0].xyz, rgb),
        dot(uniforms.gamut_matrix_rows[1].xyz, rgb),
        dot(uniforms.gamut_matrix_rows[2].xyz, rgb)
    );
}

// Hue-preserving gamut mapping: the linear gamut matrix can push highly
// saturated wide-gamut colors outside the target gamut (negative
// components). Blending those towards luma shifts hue — BT.2020 primary
// red picks up blue and turns pink. Instead blend towards the naive clip
// by an out-of-gamut smoothstep factor: slightly-out colors stay nearly
// intact, strongly-out primaries land on the pure target primary with
// their hue intact, matching mpv's perceptual gamut handling. Brightness
// overshoot (> 1) is left for the tone map.
fn gamut_compress(rgb: vec3<f32>) -> vec3<f32> {
    let lo = min(rgb.r, min(rgb.g, rgb.b));
    let outness = max(-lo, 0.0);
    let k = smoothstep(0.0, 1.0, outness);
    return mix(rgb, clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), k);
}

fn target_nits_to_reference_linear(nits: vec3<f32>) -> vec3<f32> {
    // libplacebo's encode maps [target black, target peak] onto [0, 1] where
    // 1.0 is the target reference white, so the tone-map black-point
    // compensation lands back on true black instead of lifting it.
    let black = uniforms.tone_map_extra.z;
    let peak = target_peak_nits();
    let range = max(peak - black, 0.0001);
    return max(nits - vec3<f32>(black), vec3<f32>(0.0)) / range
        * (range / target_reference_white_nits());
}

fn target_reference_linear_to_output(rgb: vec3<f32>) -> vec3<f32> {
    if (uniforms.scene_linear != 0u) {
        return max(rgb, vec3<f32>(0.0));
    }
    if (uniforms.target_transfer == 3u) {
        let pq_absolute_peak_nits = 10000.0;
        let nits = max(rgb, vec3<f32>(0.0)) * target_reference_white_nits();
        return vec3<f32>(
            pq_inverse_eotf(nits.r / pq_absolute_peak_nits),
            pq_inverse_eotf(nits.g / pq_absolute_peak_nits),
            pq_inverse_eotf(nits.b / pq_absolute_peak_nits)
        );
    }
    if (uniforms.edr_output != 0u) {
        return max(rgb, vec3<f32>(0.0));
    }
    if (uniforms.target_transfer == 1u) {
        return pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2));
    }
    if (uniforms.target_transfer == 2u) {
        return pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4));
    }
    return rgb;
}

fn final_output(rgb: vec3<f32>, alpha: f32) -> vec4<f32> {
    var output_rgb: vec3<f32>;
    if (uniforms.scene_linear != 0u) {
        output_rgb = max(rgb, vec3<f32>(0.0)) * alpha;
        return vec4<f32>(output_rgb, alpha);
    }
    if (uniforms.target_transfer == 3u) {
        output_rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * alpha;
        return vec4<f32>(output_rgb, alpha);
    }
    if (uniforms.edr_output != 0u) {
        let headroom = max(target_peak_nits() / target_reference_white_nits(), 1.0);
        output_rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(headroom)) * alpha;
        return vec4<f32>(output_rgb, alpha);
    }
    output_rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * alpha;
    return vec4<f32>(output_rgb, alpha);
}

struct RangeExpandedYCbCr {
    y: f32,
    cbcr: vec2<f32>,
};

fn expand_ycbcr_range(y_in: f32, cbcr_in: vec2<f32>) -> RangeExpandedYCbCr {
    var y = y_in;
    var cbcr = cbcr_in;
    if (uniforms.is_p010 != 0u) {
        // P010 stores 10-bit codes as code << 6 in a 16-bit UNORM texture.
        let p010_scale = 65535.0 / 65472.0;
        y *= p010_scale;
        cbcr *= p010_scale;
    }
    if (uniforms.full_range != 0u) {
        return RangeExpandedYCbCr(y, cbcr - vec2<f32>(0.5));
    }
    if (uniforms.is_p010 != 0u) {
        return RangeExpandedYCbCr(
            (y - (64.0 / 1023.0)) * (1023.0 / 876.0),
            (cbcr - vec2<f32>(512.0 / 1023.0)) * (1023.0 / 896.0),
        );
    }
    return RangeExpandedYCbCr(
        (y - (16.0 / 255.0)) * (255.0 / 219.0),
        (cbcr - vec2<f32>(128.0 / 255.0)) * (255.0 / 224.0),
    );
}

// Dolby Vision RPU reshaping, ported from libplacebo's `pl_shader_dovi_reshape`
// (the renderer behind mpv's Dolby Vision mapping). The base-layer signal is
// reshaped per component through piecewise polynomial/MMR curves selected by
// pivot comparison, where MMR coefficients mix all three raw components.
fn dovi_reshaped_signal(sig_in: vec3<f32>) -> vec3<f32> {
    let sig = clamp(sig_in, vec3<f32>(0.0), vec3<f32>(1.0));
    var result = sig;
    let flags = uniforms.dovi_flags;
    for (var c = 0u; c < 3u; c = c + 1u) {
        let segments = u32(flags[1u + c]);
        if (segments == 0u) {
            continue;
        }
        var s = result[c];
        var index = 0u;
        for (var i = 0u; i < 7u; i = i + 1u) {
            let pivot_row = uniforms.dovi_pivots[2u * c + i / 4u];
            let pivot = pivot_row[i % 4u];
            if (s >= pivot) {
                index = index + 1u;
            }
        }
        let coeff = uniforms.dovi_coefficients[8u * c + index];
        if (coeff.w < 0.5) {
            s = (coeff.z * s + coeff.y) * s + coeff.x;
        } else {
            let base = 48u * c + u32(coeff.y);
            let order = u32(coeff.w);
            let sig_x = vec4<f32>(
                sig.x * sig.y,
                sig.x * sig.z,
                sig.y * sig.z,
                sig.x * sig.y * sig.z
            );
            s = coeff.x;
            s = s + dot(uniforms.dovi_mmr[base].xyz, sig);
            s = s + dot(uniforms.dovi_mmr[base + 1u], sig_x);
            if (order >= 2u) {
                let sig2 = sig * sig;
                let sig_x2 = sig_x * sig_x;
                s = s + dot(uniforms.dovi_mmr[base + 2u].xyz, sig2);
                s = s + dot(uniforms.dovi_mmr[base + 3u], sig_x2);
                if (order >= 3u) {
                    s = s + dot(uniforms.dovi_mmr[base + 4u].xyz, sig2 * sig);
                    s = s + dot(uniforms.dovi_mmr[base + 5u], sig_x2 * sig_x);
                }
            }
        }
        let bounds = uniforms.dovi_bounds[c];
        result[c] = clamp(s, bounds.x, bounds.y);
    }
    return result;
}

// Reshaped nonlinear signal to PQ-encoded IPT via the RPU's ycc_to_rgb matrix
// and signal offsets. Applying the RPU offsets keeps integer offset codes
// exactly on sample codes (2^bits/(2^bits-1) folded in on the CPU).
fn dovi_signal_to_pq_rgb(sig: vec3<f32>) -> vec3<f32> {
    let reshaped = dovi_reshaped_signal(sig) - uniforms.dovi_nonlinear_offset.xyz;
    return vec3<f32>(
        dot(uniforms.dovi_nonlinear_matrix[0].xyz, reshaped),
        dot(uniforms.dovi_nonlinear_matrix[1].xyz, reshaped),
        dot(uniforms.dovi_nonlinear_matrix[2].xyz, reshaped)
    );
}

// Linearized BT.2020-referred HPE LMS back to linear RGB, using the composite
// of the fixed HPE inverse with the RPU's rgb_to_lms matrix (premultiplied on
// the CPU, matching libplacebo's dovi_lms2rgb).
fn dovi_lms_to_rgb(linear: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(uniforms.dovi_lms_matrix[0].xyz, linear),
        dot(uniforms.dovi_lms_matrix[1].xyz, linear),
        dot(uniforms.dovi_lms_matrix[2].xyz, linear)
    );
}

fn packed_luma_texel(virtual_coord_in: vec2<i32>, virtual_size: vec2<i32>) -> f32 {
    let virtual_coord = clamp(virtual_coord_in, vec2<i32>(0), virtual_size - vec2<i32>(1));
    let packed_coord = virtual_coord / vec2<i32>(2);
    let packed = textureLoad(luma_texture, packed_coord, 0);
    let component = u32((virtual_coord.y & 1) * 2 + (virtual_coord.x & 1));
    if (component == 0u) {
        return packed.r;
    }
    if (component == 1u) {
        return packed.g;
    }
    if (component == 2u) {
        return packed.b;
    }
    return packed.a;
}

// ArtCNN stores the four DCR subpixels of a virtual 2W x 2H luma image in one
// RGBA texel. Reconstruct the same normalized-coordinate bilinear sample that
// a native 2W x 2H texture would provide, without allocating that large image.
fn sample_packed_luma(tex_coord: vec2<f32>) -> f32 {
    let packed_size = vec2<i32>(textureDimensions(luma_texture, 0));
    let virtual_size = packed_size * vec2<i32>(2);
    let sample_position = clamp(tex_coord, vec2<f32>(0.0), vec2<f32>(1.0))
        * vec2<f32>(virtual_size) - vec2<f32>(0.5);
    let lo = vec2<i32>(floor(sample_position));
    let fraction = fract(sample_position);
    let y0 = mix(
        packed_luma_texel(lo, virtual_size),
        packed_luma_texel(lo + vec2<i32>(1, 0), virtual_size),
        fraction.x,
    );
    let y1 = mix(
        packed_luma_texel(lo + vec2<i32>(0, 1), virtual_size),
        packed_luma_texel(lo + vec2<i32>(1, 1), virtual_size),
        fraction.x,
    );
    return mix(y0, y1, fraction.y);
}

fn p010_texel(plane: texture_2d<f32>, coord: vec2<i32>, size: vec2<i32>, byte_planes: bool) -> vec2<f32> {
    let texel = textureLoad(plane, clamp(coord, vec2<i32>(0), size - vec2<i32>(1)), 0);
    if (!byte_planes) {
        return texel.rg;
    }
    // RG8 carries Y's low/high bytes; RGBA8 carries U low/high, V low/high.
    // The second component is unused when sampling a luma plane.
    return (texel.rb + 256.0 * texel.ga) / 257.0;
}

fn sample_p010(plane: texture_2d<f32>, tex_coord: vec2<f32>, byte_planes: bool) -> vec2<f32> {
    let size = vec2<i32>(textureDimensions(plane, 0));
    let position = clamp(tex_coord, vec2<f32>(0.0), vec2<f32>(1.0))
        * vec2<f32>(size) - vec2<f32>(0.5);
    let lo = vec2<i32>(floor(position));
    let fraction = fract(position);
    // Reconstruct before interpolation. RG8 hardware filtering can round each
    // high byte to 8-bit precision. Native R16 filtering can also quantize the
    // interpolation weights. Use the same float interpolation for both P010
    // representations so HDR transfer functions do not amplify those errors.
    let top = mix(p010_texel(plane, lo, size, byte_planes),
        p010_texel(plane, lo + vec2<i32>(1, 0), size, byte_planes), fraction.x);
    let bottom = mix(p010_texel(plane, lo + vec2<i32>(0, 1), size, byte_planes),
        p010_texel(plane, lo + vec2<i32>(1, 1), size, byte_planes), fraction.x);
    return mix(top, bottom, fraction.y);
}

@vertex
fn erika_video_vertex(@builtin(vertex_index) vertex_id: u32) -> VertexOut {
    // Avoid dynamically indexing a function-local position array here. The
    // Android emulator's SwiftShader GLES 3.0 compiler accepts Naga's generated
    // GLSL for that pattern but rasterizes no vertices. These bits produce the
    // same three full-screen-triangle coordinates without an array lookup.
    let unit = vec2<f32>(
        f32(vertex_id & 1u),
        f32((vertex_id >> 1u) & 1u),
    );
    // Keep interface structs in SSA form. Naga 29 decorates these types with
    // offsets; recent SPIR-V validation rejects them in Function storage.
    return VertexOut(
        vec4<f32>(unit * 4.0 - vec2<f32>(1.0), 0.0, 1.0),
        vec2<f32>(unit.x * 2.0, 1.0 - unit.y * 2.0),
    );
}

@fragment
fn erika_video_fragment(in: VertexOut) -> @location(0) vec4<f32> {
    let input_mode = uniforms.input_mode & 255u;
    let packed_alpha = (uniforms.input_mode & 256u) != 0u;
    var color_coord = in.tex_coord;
    if (packed_alpha) {
        color_coord.x *= 0.5;
    }
    let alpha_coord = vec2<f32>(0.5 + in.tex_coord.x * 0.5, in.tex_coord.y);
    let p010_bytes = (uniforms.input_mode & 512u) != 0u;
    let y_texel = textureSample(luma_texture, video_sampler, color_coord);
    var y_sample = y_texel.r;
    if (uniforms.is_p010 != 0u && input_mode == 0u) {
        y_sample = sample_p010(luma_texture, color_coord, p010_bytes).x;
    }
    if (input_mode == 2u) {
        y_sample = sample_packed_luma(color_coord);
    }
    let uv_texel = textureSample(chroma_texture, video_sampler, color_coord);
    var cbcr_sample = uv_texel.rg;
    if (uniforms.is_p010 != 0u) {
        cbcr_sample = sample_p010(chroma_texture, color_coord, p010_bytes);
    }
    var rgb: vec3<f32>;
    let dovi_enabled = uniforms.dovi_flags.x != 0.0;
    let dovi_ycbcr_input = dovi_enabled && (input_mode == 0u || input_mode == 2u || input_mode == 4u);
    if (dovi_ycbcr_input) {
        // The base layer carries the raw 12-bit DV signal (10-bit container,
        // full range); range expansion and the YCbCr matrix are replaced by
        // the RPU reshaping + ycc_to_rgb path.
        var sig = vec3<f32>(y_sample, cbcr_sample.x, cbcr_sample.y);
        if (uniforms.is_p010 != 0u) {
            sig *= 65535.0 / 65472.0;
        }
        rgb = dovi_signal_to_pq_rgb(sig);
    } else if (input_mode == 1u) {
        rgb = textureSample(luma_texture, video_sampler, color_coord).rgb;
    } else if (input_mode == 3u || input_mode == 5u) {
        let original_rgb = textureSample(chroma_texture, video_sampler, color_coord).rgb;
        let original_luma = dot(uniforms.luma_coefficients.xyz, original_rgb);
        var enhanced_luma = y_texel.r;
        if (input_mode == 3u) {
            enhanced_luma = sample_packed_luma(color_coord);
        }
        rgb = original_rgb + vec3<f32>(enhanced_luma - original_luma);
    } else {
        let expanded = expand_ycbcr_range(y_sample, cbcr_sample);
        let y = expanded.y;
        let cbcr = expanded.cbcr;

        let kr = uniforms.luma_coefficients.x;
        let kg = max(uniforms.luma_coefficients.y, 0.000001);
        let kb = uniforms.luma_coefficients.z;
        rgb.r = y + 2.0 * (1.0 - kr) * cbcr.y;
        rgb.b = y + 2.0 * (1.0 - kb) * cbcr.x;
        rgb.g = (y - kr * rgb.r - kb * rgb.b) / kg;
    }
    rgb = transfer_to_source_reference_linear(rgb);
    if (dovi_ycbcr_input) {
        rgb = dovi_lms_to_rgb(rgb);
    }
    rgb = source_reference_to_nits(rgb);
    rgb = tone_map_nits(rgb);
    rgb = target_nits_to_reference_linear(rgb);
    rgb = gamut_compress(rgb);
    rgb = target_reference_linear_to_output(rgb);
    var alpha = 1.0;
    if (packed_alpha) {
        let alpha_texel = textureSample(luma_texture, video_sampler, alpha_coord);
        var alpha_sample = alpha_texel.r;
        if (uniforms.is_p010 != 0u) {
            alpha_sample = sample_p010(luma_texture, alpha_coord, p010_bytes).x;
        }
        if (input_mode == 1u || input_mode == 3u) {
            alpha = clamp(alpha_sample, 0.0, 1.0);
        } else {
            alpha = clamp(expand_ycbcr_range(alpha_sample, vec2<f32>(0.5)).y, 0.0, 1.0);
        }
    }
    return final_output(rgb, alpha);
}
