//! AMD FidelityFX Super Resolution 1.0 (EASU + RCAS) on a single channel.
//!
//! Port of `ffx_fsr1.h` (Copyright (c) 2021 Advanced Micro Devices, Inc.,
//! MIT license, see `assets/fsr1/LICENSE.FSR1`) reduced to luma: the edge
//! analysis uses the channel itself instead of an RGB luma approximation,
//! and the approximate reciprocals are exact. [`easu`] and [`rcas`] are the
//! CPU references for the GPU kernels; [`HLSL`] is the D3D11 kernel, which
//! fuses both passes through `groupshared` memory so the EASU result never
//! leaves the chip.

/// RCAS sharpness in stops (0 = maximum). AMD's and mpv's default is 0.2.
pub const DEFAULT_SHARPNESS_STOPS: f32 = 0.2;

const RCAS_LIMIT: f32 = 0.25 - 1.0 / 16.0;

/// Linear RCAS strength for a sharpness in stops.
pub fn rcas_strength(stops: f32) -> f32 {
    (-stops).exp2()
}

/// Edge-adaptive spatial upscale of `input` (`width x height`, row-major) to
/// `out_width x out_height`.
pub fn easu(
    input: &[f32],
    width: usize,
    height: usize,
    out_width: usize,
    out_height: usize,
) -> Vec<f32> {
    let at = |x: isize, y: isize| -> f32 {
        let x = x.clamp(0, width as isize - 1) as usize;
        let y = y.clamp(0, height as isize - 1) as usize;
        input[y * width + x]
    };
    let scale_x = width as f32 / out_width as f32;
    let scale_y = height as f32 / out_height as f32;
    let mut out = vec![0.0f32; out_width * out_height];
    for oy in 0..out_height {
        for ox in 0..out_width {
            let px = (ox as f32 + 0.5) * scale_x - 0.5;
            let py = (oy as f32 + 0.5) * scale_y - 0.5;
            let fx = px.floor();
            let fy = py.floor();
            let (ix, iy) = (fx as isize, fy as isize);
            let tap = |dx: isize, dy: isize| at(ix + dx, iy + dy);
            out[oy * out_width + ox] = easu_pixel(px - fx, py - fy, &tap);
        }
    }
    out
}

/// One EASU output from the 12-tap neighborhood of `f` (`tap(0, 0)`).
pub fn easu_pixel(ppx: f32, ppy: f32, tap: &dyn Fn(isize, isize) -> f32) -> f32 {
    //    b c
    //  e f g h
    //  i j k l
    //    n o
    let (b, c) = (tap(0, -1), tap(1, -1));
    let (e, f, g, h) = (tap(-1, 0), tap(0, 0), tap(1, 0), tap(2, 0));
    let (i, j, k, l) = (tap(-1, 1), tap(0, 1), tap(1, 1), tap(2, 1));
    let (n, o) = (tap(0, 2), tap(1, 2));

    let mut dir = [0.0f32; 2];
    let mut len = 0.0f32;
    let mut set = |w: f32, la: f32, lb: f32, lc: f32, ld: f32, le: f32| {
        let dc = ld - lc;
        let cb = lc - lb;
        let len_x = rcp_or_zero(dc.abs().max(cb.abs()));
        let dir_x = ld - lb;
        dir[0] += dir_x * w;
        let len_x = (dir_x.abs() * len_x).clamp(0.0, 1.0);
        len += len_x * len_x * w;
        let ec = le - lc;
        let ca = lc - la;
        let len_y = rcp_or_zero(ec.abs().max(ca.abs()));
        let dir_y = le - la;
        dir[1] += dir_y * w;
        let len_y = (dir_y.abs() * len_y).clamp(0.0, 1.0);
        len += len_y * len_y * w;
    };
    set((1.0 - ppx) * (1.0 - ppy), b, e, f, g, j);
    set(ppx * (1.0 - ppy), c, f, g, h, k);
    set((1.0 - ppx) * ppy, f, i, j, k, n);
    set(ppx * ppy, g, j, k, l, o);

    let dir_r = dir[0] * dir[0] + dir[1] * dir[1];
    let zero = dir_r < 1.0 / 32768.0;
    let dir_r = if zero { 1.0 } else { 1.0 / dir_r.sqrt() };
    if zero {
        dir[0] = 1.0;
    }
    dir[0] *= dir_r;
    dir[1] *= dir_r;
    let len = len * 0.5;
    let len = len * len;
    let stretch = (dir[0] * dir[0] + dir[1] * dir[1]) / dir[0].abs().max(dir[1].abs());
    let len2 = [1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len];
    let lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    let clp = 1.0 / lob;

    let mut acc = 0.0f32;
    let mut weight = 0.0f32;
    let mut accumulate = |ox: f32, oy: f32, value: f32| {
        let ox = ox - ppx;
        let oy = oy - ppy;
        let vx = (ox * dir[0] + oy * dir[1]) * len2[0];
        let vy = (ox * -dir[1] + oy * dir[0]) * len2[1];
        let d2 = (vx * vx + vy * vy).min(clp);
        let wb = 2.0 / 5.0 * d2 - 1.0;
        let wa = lob * d2 - 1.0;
        let wb = 25.0 / 16.0 * wb * wb - (25.0 / 16.0 - 1.0);
        let w = wb * wa * wa;
        acc += value * w;
        weight += w;
    };
    accumulate(0.0, -1.0, b);
    accumulate(1.0, -1.0, c);
    accumulate(-1.0, 1.0, i);
    accumulate(0.0, 1.0, j);
    accumulate(0.0, 0.0, f);
    accumulate(-1.0, 0.0, e);
    accumulate(1.0, 1.0, k);
    accumulate(2.0, 1.0, l);
    accumulate(2.0, 0.0, h);
    accumulate(1.0, 0.0, g);
    accumulate(1.0, 2.0, o);
    accumulate(0.0, 2.0, n);
    let min4 = f.min(g).min(j).min(k);
    let max4 = f.max(g).max(j).max(k);
    (acc / weight).clamp(min4, max4)
}

fn rcp_or_zero(value: f32) -> f32 {
    if value > 0.0 { 1.0 / value } else { 0.0 }
}

/// Robust contrast-adaptive sharpening of a `width x height` image.
pub fn rcas(input: &[f32], width: usize, height: usize, strength: f32) -> Vec<f32> {
    let at = |x: isize, y: isize| -> f32 {
        let x = x.clamp(0, width as isize - 1) as usize;
        let y = y.clamp(0, height as isize - 1) as usize;
        input[y * width + x]
    };
    let mut out = vec![0.0f32; width * height];
    for y in 0..height as isize {
        for x in 0..width as isize {
            out[y as usize * width + x as usize] = rcas_pixel(
                at(x, y - 1),
                at(x - 1, y),
                at(x, y),
                at(x + 1, y),
                at(x, y + 1),
                strength,
            );
        }
    }
    out
}

/// RCAS on the plus-shaped neighborhood `b` (up), `d`, `e` (center), `f`,
/// `h` (down).
pub fn rcas_pixel(b: f32, d: f32, e: f32, f: f32, h: f32, strength: f32) -> f32 {
    let mn4 = b.min(d).min(f).min(h);
    let mx4 = b.max(d).max(f).max(h);
    let hit_min = mn4.min(e) / (4.0 * mx4).max(1.0 / 65536.0);
    let hit_max = (1.0 - mx4.max(e)) / (4.0 * mn4 - 4.0).min(-1.0 / 65536.0);
    let lobe = (-hit_min).max(hit_max).clamp(-RCAS_LIMIT, 0.0) * strength;
    (lobe * (b + d + f + h) + e) / (4.0 * lobe + 1.0)
}

/// Fused EASU + RCAS (`cs_5_0`): 256 threads cover a 64x64 output tile.
/// Four gathers supply EASU, whose quadratic distance coefficients are shared
/// across its 12 taps. The EASU tile stays in `groupshared` for RCAS.
pub const HLSL: &str = r#"
cbuffer Fsr : register(b0) {
    float4 scale;   // xy: input/output size ratio, zw: 0.5 * ratio - 0.5
    int4 sizes;     // xy: input size, zw: output size
    float4 sharp;   // x: RCAS strength (0 disables), yz: inverse input size
};
Texture2DArray<float> t_luma : register(t0);
SamplerState s_clamp : register(s0);
RWTexture2D<float> u_out : register(u0);

#ifndef TILE
#define TILE 16
#endif
#ifndef PX
#define PX 4
#define PY 4
#endif
#ifndef FSR_GATHER
#define FSR_GATHER 1
#endif
#ifndef FSR_FAST_RCP
#define FSR_FAST_RCP 1
#endif
#define BX (TILE * PX)
#define BY (TILE * PY)
#define RING (BX + 2)
groupshared float lds[RING * (BY + 2)];

float tap(int2 p) {
    return t_luma.Load(int4(clamp(p, int2(0, 0), sizes.xy - 1), 0, 0));
}

float reciprocal(float v) {
#if FSR_FAST_RCP
    return rcp(v);
#else
    return 1.0 / v;
#endif
}

float divide(float numerator, float denominator) {
#if FSR_FAST_RCP
    return numerator * rcp(denominator);
#else
    return numerator / denominator;
#endif
}

float rcp_or_zero(float v) { return v > 0.0 ? reciprocal(v) : 0.0; }

void easu_set(inout float2 dir, inout float len, float w, float la, float lb, float lc, float ld, float le) {
    float dc = ld - lc;
    float cb = lc - lb;
    float lenX = rcp_or_zero(max(abs(dc), abs(cb)));
    float dirX = ld - lb;
    dir.x += dirX * w;
    lenX = saturate(abs(dirX) * lenX);
    len += lenX * lenX * w;
    float ec = le - lc;
    float ca = lc - la;
    float lenY = rcp_or_zero(max(abs(ec), abs(ca)));
    float dirY = le - la;
    dir.y += dirY * w;
    lenY = saturate(abs(dirY) * lenY);
    len += lenY * lenY * w;
}

// The anisotropic squared distance is a quadratic form. Computing its
// coefficients once avoids rotating and stretching each of the 12 taps.
void easu_tap(inout float acc, inout float aw, float2 off, float3 metric, float lob, float clp, float c) {
    float d2 = min(metric.x * off.x * off.x + metric.y * off.x * off.y + metric.z * off.y * off.y, clp);
    float wB = 2.0 / 5.0 * d2 - 1.0;
    float wA = lob * d2 - 1.0;
    wB *= wB;
    wA *= wA;
    wB = 25.0 / 16.0 * wB - (25.0 / 16.0 - 1.0);
    float w = wB * wA;
    acc += c * w;
    aw += w;
}

float easu(int2 ip) {
    float2 pp = float2(ip) * scale.xy + scale.zw;
    float2 fp = floor(pp);
    pp -= fp;
    int2 f0 = int2(fp);
#if FSR_GATHER
    float2 uv = (fp + 1.0) * sharp.yz;
    float4 bc = t_luma.GatherRed(s_clamp, float3(uv + float2(0, -1) * sharp.yz, 0));
    float4 ef = t_luma.GatherRed(s_clamp, float3(uv + float2(-1, 0) * sharp.yz, 0));
    float4 gh = t_luma.GatherRed(s_clamp, float3(uv + float2(1, 0) * sharp.yz, 0));
    float4 jk = t_luma.GatherRed(s_clamp, float3(uv + float2(0, 1) * sharp.yz, 0));
    float b = bc.w, c = bc.z, e = ef.w, f = ef.z, g = gh.w, h = gh.z;
    float i = ef.x, j = jk.w, k = jk.z, l = gh.y, n = jk.x, o = jk.y;
#else
    float b = tap(f0 + int2(0, -1)), c = tap(f0 + int2(1, -1));
    float e = tap(f0 + int2(-1, 0)), f = tap(f0), g = tap(f0 + int2(1, 0)), h = tap(f0 + int2(2, 0));
    float i = tap(f0 + int2(-1, 1)), j = tap(f0 + int2(0, 1)), k = tap(f0 + int2(1, 1)), l = tap(f0 + int2(2, 1));
    float n = tap(f0 + int2(0, 2)), o = tap(f0 + int2(1, 2));
#endif
    float2 dir = 0.0;
    float len = 0.0;
    easu_set(dir, len, (1.0 - pp.x) * (1.0 - pp.y), b, e, f, g, j);
    easu_set(dir, len, pp.x * (1.0 - pp.y), c, f, g, h, k);
    easu_set(dir, len, (1.0 - pp.x) * pp.y, f, i, j, k, n);
    easu_set(dir, len, pp.x * pp.y, g, j, k, l, o);
    float dirR = dot(dir, dir);
    bool zro = dirR < 1.0 / 32768.0;
    dirR = zro ? 1.0 : rsqrt(dirR);
    dir.x = zro ? 1.0 : dir.x;
    dir *= dirR;
    len = len * 0.5;
    len *= len;
    float stretch = divide(dot(dir, dir), max(abs(dir.x), abs(dir.y)));
    float2 len2 = float2(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    float lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    float clp = reciprocal(lob);
    float2 ls = len2 * len2;
    float3 metric = float3(dir.x * dir.x * ls.x + dir.y * dir.y * ls.y,
        2.0 * dir.x * dir.y * (ls.x - ls.y),
        dir.y * dir.y * ls.x + dir.x * dir.x * ls.y);
    float acc = 0.0, aw = 0.0;
    easu_tap(acc, aw, float2(0.0, -1.0) - pp, metric, lob, clp, b);
    easu_tap(acc, aw, float2(1.0, -1.0) - pp, metric, lob, clp, c);
    easu_tap(acc, aw, float2(-1.0, 1.0) - pp, metric, lob, clp, i);
    easu_tap(acc, aw, float2(0.0, 1.0) - pp, metric, lob, clp, j);
    easu_tap(acc, aw, float2(0.0, 0.0) - pp, metric, lob, clp, f);
    easu_tap(acc, aw, float2(-1.0, 0.0) - pp, metric, lob, clp, e);
    easu_tap(acc, aw, float2(1.0, 1.0) - pp, metric, lob, clp, k);
    easu_tap(acc, aw, float2(2.0, 1.0) - pp, metric, lob, clp, l);
    easu_tap(acc, aw, float2(2.0, 0.0) - pp, metric, lob, clp, h);
    easu_tap(acc, aw, float2(1.0, 0.0) - pp, metric, lob, clp, g);
    easu_tap(acc, aw, float2(1.0, 2.0) - pp, metric, lob, clp, o);
    easu_tap(acc, aw, float2(0.0, 2.0) - pp, metric, lob, clp, n);
    float mn = min(min(f, g), min(j, k));
    float mx = max(max(f, g), max(j, k));
    return clamp(divide(acc, aw), mn, mx);
}

[numthreads(TILE, TILE, 1)]
void main(uint3 gtid : SV_GroupThreadID, uint3 gid : SV_GroupID) {
    int2 origin = int2(gid.xy) * int2(BX, BY) - 1;
    uint lin = gtid.y * TILE + gtid.x;
    for (uint index = lin; index < RING * (BY + 2); index += TILE * TILE) {
        int2 p = origin + int2(index % RING, index / RING);
        lds[index] = easu(clamp(p, int2(0, 0), sizes.zw - 1));
    }
    GroupMemoryBarrierWithGroupSync();
    [unroll] for (uint py = 0; py < PY; ++py) {
        [unroll] for (uint px = 0; px < PX; ++px) {
            uint2 local = gtid.xy + uint2(px, py) * TILE;
            int2 pix = int2(gid.xy) * int2(BX, BY) + int2(local);
            if (all(pix < sizes.zw)) {
                uint center = (local.y + 1) * RING + local.x + 1;
                float e = lds[center];
                if (sharp.x <= 0.0) {
                    u_out[pix] = e;
                } else {
                    float b = lds[center - RING], d = lds[center - 1];
                    float f = lds[center + 1], h = lds[center + RING];
                    float mn4 = min(min(b, d), min(f, h));
                    float mx4 = max(max(b, d), max(f, h));
                    float hitMin = divide(min(mn4, e), max(4.0 * mx4, 1.0 / 65536.0));
                    float hitMax = divide(1.0 - max(mx4, e), min(4.0 * mn4 - 4.0, -1.0 / 65536.0));
                    float lobe = max(-(0.25 - 1.0 / 16.0), min(max(-hitMin, hitMax), 0.0)) * sharp.x;
                    u_out[pix] = divide(lobe * (b + d + f + h) + e, 4.0 * lobe + 1.0);
                }
            }
        }
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::sr::reference::test_pattern;

    #[test]
    fn easu_is_exact_on_flat_input_and_bounded_elsewhere() {
        let flat = vec![0.4f32; 12 * 9];
        for value in easu(&flat, 12, 9, 29, 20) {
            assert!((value - 0.4).abs() < 1e-6);
        }
        let image = test_pattern(31, 17, 2);
        let out = easu(&image, 31, 17, 62, 34);
        assert!(out.iter().all(|value| (0.0..=1.0).contains(value)));
        // Upscaled mean stays close to the source mean.
        let mean = |values: &[f32]| values.iter().sum::<f32>() / values.len() as f32;
        assert!((mean(&out) - mean(&image)).abs() < 0.02);
    }

    #[test]
    fn rcas_preserves_flat_regions_and_sharpens_edges() {
        let flat = vec![0.3f32; 8 * 8];
        assert!(
            rcas(&flat, 8, 8, rcas_strength(0.2))
                .iter()
                .all(|value| (value - 0.3).abs() < 1e-6)
        );
        let edge: Vec<f32> = (0..64).map(|i| if i % 8 < 4 { 0.2 } else { 0.6 }).collect();
        let sharpened = rcas(&edge, 8, 8, rcas_strength(0.0));
        // The dark side of the edge gets darker, the bright side brighter.
        assert!(sharpened[3] < 0.2 && sharpened[4] > 0.6);
    }
}
