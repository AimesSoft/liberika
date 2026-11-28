//! Perceptual gamut mapping via a precomputed IPT-space 3D LUT, following
//! libplacebo's `pl_gamut_map_perceptual` LUT layout (the default gamut map
//! of libplacebo/mpv). The mapping converts a color from the source primaries
//! into the destination primaries *inside* IPT and then rolls off
//! out-of-gamut chroma towards the destination gamut, protecting in-gamut
//! colors (dead zone) with a Möbius soft clip.
//!
//! The chroma rolloff is currently a simplified dead-zone blend plus
//! softclip rather than libplacebo's full per-hue boundary search, so highly
//! saturated BT.2020 colors can diverge slightly from mpv.
//!
//! Because the boundary search is expensive, libplacebo precomputes it once
//! into a 3D LUT over the IPT intensity I, the IPT chroma magnitude C and the
//! IPT hue angle h; the renderers sample it per pixel. This module generates
//! the same LUT lattice on the CPU (48 x 32 x 256 = 393216 texels), and the
//! shaders do the lookup with the same index mapping.
//!
//! Note: libplacebo currently *samples the perceptual LUT in ICh space* with
//! the I channel *in absolute PQ units* over the target display range
//! `[min_luma, max_luma]`. We reproduce that layout exactly so the WGSL/MSL
//! and HLSL samplers match the reference pixels.

use crate::core::ColorPrimaries;
use crate::renderer::pipeline::{RgbMatrix, ipt_rgb2lms_matrix};

/// LUT dimensions, matching libplacebo's `pl_color_map_default_params`
/// `lut3d_size = {48, 32, 256}` (I, C, h).
pub const LUT_SIZE_I: usize = 48;
pub const LUT_SIZE_C: usize = 32;
pub const LUT_SIZE_H: usize = 256;
/// Per-texel component count (RGB with a padded w).
pub const LUT_COMPONENTS: usize = 4;

const PQ_M1: f32 = 2610.0 / 4096.0 * 1.0 / 4.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

#[allow(dead_code)]
/// 4% crosstalk HPE matrix shared with the tone map.
fn hpe_crosstalk() -> RgbMatrix {
    let c = 0.04_f32;
    RgbMatrix::new([
        [1.0 - 2.0 * c, c, c],
        [c, 1.0 - 2.0 * c, c],
        [c, c, 1.0 - 2.0 * c],
    ])
}

fn rgb2lms(primaries: ColorPrimaries) -> RgbMatrix {
    ipt_rgb2lms_matrix(primaries)
}

#[allow(dead_code)]
fn lms2rgb(primaries: ColorPrimaries) -> RgbMatrix {
    rgb2lms(primaries).inverse()
}

/// Perceptual gamut mapping parameters that change how the LUT is computed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GamutLutParams {
    /// Source primaries of the tone-mapped (still source-referred) signal.
    pub source: ColorPrimaries,
    /// Destination (display) primaries.
    pub target: ColorPrimaries,
    /// Minimum display luminance in PQ code (absolute), i.e. libplacebo's
    /// `gamut.min_luma`: the target's black point, which is also the tone
    /// map's output minimum (0 for HDR/EDR targets, `peak / contrast` for SDR
    /// ones).
    pub min_luma: f32,
    /// Maximum display luminance in PQ code (libplacebo's `gamut.max_luma`,
    /// the tone map's output peak).
    pub max_luma: f32,
}

/// A generated 3D perceptual gamut LUT. Texels are (I, P, T) triples with a
/// padded component, normalized to [0, 1] like libplacebo's uint16 upload
/// (`I`, `P + 0.5`, `T + 0.5` scaled).
#[derive(Debug, Clone, PartialEq)]
pub struct GamutLut {
    /// size = LUT_SIZE_I * LUT_SIZE_C * LUT_SIZE_H, stored as float triples
    /// (R=IPT.I, G=IPT.P + 0.5, B=IPT.T + 0.5) in [0, 1].
    pub texels: Vec<[f32; 3]>,
    /// `min_luma`/`max_luma` in PQ that the LUT's I axis spans.
    pub params: GamutLutParams,
}

impl GamutLut {
    /// Generate the perceptual LUT, mirroring libplacebo's
    /// `pl_gamut_map_perceptual` evaluated over the lattice texels.
    ///
    /// libplacebo caches the per-hue boundary peak (`saturate`); we instead
    /// precompute the source/destination peaks for each of the `LUT_SIZE_H`
    /// hue slices once, which the texel loop then reuses — same math, no
    /// repeated golden-section searches.
    pub fn generate(params: GamutLutParams) -> Self {
        let src = GamutState::new(params.source, params);
        let dst = GamutState::new(params.target, params);
        let mut texels = Vec::with_capacity(LUT_SIZE_I * LUT_SIZE_C * LUT_SIZE_H);
        // Lattice order must match the 3D texture addressing used by the
        // shaders: width = I (48, stride 1), height = C (32, stride width),
        // depth = h (256, stride width * height). The loop runs h outer,
        // C middle, I inner so that width (I) is contiguous in memory, and
        // the expensive per-hue `saturate` search runs 256 times instead of
        // 393,216 times.
        for hx in 0..LUT_SIZE_H {
            let h = -std::f32::consts::PI
                + 2.0 * std::f32::consts::PI * hx as f32 / (LUT_SIZE_H - 1) as f32;
            let src_peak = saturate(h, &src);
            let dst_peak = saturate(h, &dst);
            let max_c = src_peak[1].max(dst_peak[1]).max(1e-9);
            let soft = Softclip {
                knee: 0.70,
                _desat: 0.35,
            };
            for cx in 0..LUT_SIZE_C {
                let c = 0.5 * cx as f32 / (LUT_SIZE_C - 1) as f32;
                for ix in 0..LUT_SIZE_I {
                    let i = params.min_luma
                        + (params.max_luma - params.min_luma) * ix as f32 / (LUT_SIZE_I - 1) as f32;
                    let mapped = perceptual_map_at(
                        [i, c, h],
                        &src,
                        &dst,
                        src_peak,
                        dst_peak,
                        max_c,
                        &soft,
                        params,
                    );
                    texels.push([mapped[0], mapped[1] + 0.5, mapped[2] + 0.5]);
                }
            }
        }
        Self { texels, params }
    }
}

/// A `GamutLut::generate` running on a background thread, shared by the
/// Metal/wgpu/D3D11 renderers. Generation costs hundreds of milliseconds
/// (393k texels of PQ roundtrips plus 256 hue-boundary searches); running it
/// on the render thread stalls the first HDR wide-gamut frame. The renderer
/// polls `poll()` each frame and keeps the fast `gamut_compress` path until
/// the LUT lands.
pub struct GamutLutJob {
    params: GamutLutParams,
    result: std::sync::mpsc::Receiver<GamutLut>,
}

impl GamutLutJob {
    /// Spawn generation for `params`. The thread is detached: it writes into
    /// the channel and exits, so dropping the job simply discards an unused
    /// result.
    pub fn spawn(params: GamutLutParams) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("erika-gamut-lut".to_string())
            .spawn(move || {
                let lut = GamutLut::generate(params);
                let _ = sender.send(lut);
            })
            .expect("spawn gamut LUT generation thread");
        Self {
            params,
            result: receiver,
        }
    }

    /// The parameters this job is generating for.
    pub fn params(&self) -> GamutLutParams {
        self.params
    }

    /// Take the finished LUT when generation has completed.
    pub fn poll(&self) -> Option<GamutLut> {
        self.result.try_recv().ok()
    }
}

#[cfg(test)]
mod job_tests {
    use super::*;

    #[test]
    fn job_produces_the_same_lut_as_inline_generation() {
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.0,
            max_luma: 0.75,
        };
        let job = GamutLutJob::spawn(params);
        let lut = loop {
            if let Some(lut) = job.poll() {
                break lut;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(lut.params, params);
        assert_eq!(lut.texels.len(), LUT_SIZE_I * LUT_SIZE_C * LUT_SIZE_H);
        // Polling after completion yields nothing further.
        assert!(job.poll().is_none());
    }
}

fn pq_oetf(x: f32) -> f32 {
    let x = x.max(0.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * x) / (1.0 + PQ_C3 * x)).powf(PQ_M2)
}

fn pq_eotf(x: f32) -> f32 {
    let x = x.max(0.0).powf(1.0 / PQ_M2);
    let num = (x - PQ_C1).max(0.0);
    let den = (PQ_C2 - PQ_C3 * x).max(1e-9);
    (num / den).powf(1.0 / PQ_M1)
}

fn rgb2ipt(rgb: [f32; 3], gamut: &GamutState) -> [f32; 3] {
    let lms = gamut.rgb2lms.mul_vec(rgb);
    let lp = pq_oetf(lms[0]);
    let mp = pq_oetf(lms[1]);
    let sp = pq_oetf(lms[2]);
    [
        0.4000 * lp + 0.4000 * mp + 0.2000 * sp,
        4.4550 * lp - 4.8510 * mp + 0.3960 * sp,
        0.8056 * lp + 0.3572 * mp - 1.1628 * sp,
    ]
}

fn ipt2rgb(ipt: [f32; 3], gamut: &GamutState) -> [f32; 3] {
    let lp = ipt[0] + 0.0975689 * ipt[1] + 0.205226 * ipt[2];
    let mp = ipt[0] - 0.1138760 * ipt[1] + 0.133217 * ipt[2];
    let sp = ipt[0] + 0.0326151 * ipt[1] - 0.676887 * ipt[2];
    let l = pq_eotf(lp);
    let m = pq_eotf(mp);
    let s = pq_eotf(sp);
    gamut.lms2rgb.mul_vec([l, m, s])
}

/// Perceptual map of one IPT color given as (I in PQ code, chroma, hue),
/// mirroring libplacebo's `perceptual()` body with the per-hue peaks already
/// computed. `src`/`dst` are the gamut states, `src_peak`/`dst_peak` the
/// maximally-saturated boundary colors at this hue, `max_c` their chroma
/// max (the dead-zone denominator).
fn perceptual_map_at(
    ich: [f32; 3],
    src: &GamutState,
    dst: &GamutState,
    _src_peak: [f32; 3],
    _dst_peak: [f32; 3],
    max_c: f32,
    soft: &Softclip,
    _params: GamutLutParams,
) -> [f32; 3] {
    let ipt_in = ich2ipt(ich);
    let mapped = rgb2ipt(ipt2rgb(ipt_in, src), dst);

    // Protect in-gamut region: blend only colors whose chroma exceeds the
    // perceptual dead zone (30% of the peak), scaling to full strength.
    let deadzone = 0.30_f32;
    let strength = 0.80_f32;
    let k = pl_smoothstep(deadzone, 1.0, ich[1] / max_c) * strength;
    let ipt = [
        ipt_in[0] + (mapped[0] - ipt_in[0]) * k,
        ipt_in[1] + (mapped[1] - ipt_in[1]) * k,
        ipt_in[2] + (mapped[2] - ipt_in[2]) * k,
    ];

    let rgb = ipt2rgb(ipt, dst);
    let max_rgb = rgb[0].max(rgb[1]).max(rgb[2]);
    let out = [
        softclip(rgb[0], max_rgb, dst.max_rgb, soft).max(dst.min_rgb),
        softclip(rgb[1], max_rgb, dst.max_rgb, soft).max(dst.min_rgb),
        softclip(rgb[2], max_rgb, dst.max_rgb, soft).max(dst.min_rgb),
    ];
    rgb2ipt(out, dst)
}

fn softclip(value: f32, source: f32, target: f32, c: &Softclip) -> f32 {
    if target == 0.0 {
        return 0.0;
    }
    let peak = source / target;
    let x = (value / target).min(peak);
    if x <= c.knee || peak <= 1.0 {
        return value;
    }
    let j = c.knee;
    let a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
    let b = (j * j - 2.0 * j * peak + peak) / (peak - 1.0).max(1e-6);
    let scale = (b * b + 2.0 * b * j + j * j) / (b - a);
    scale * (x + a) / (x + b) * target
}

struct Softclip {
    knee: f32,
    _desat: f32,
}

fn pl_smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

struct GamutState {
    rgb2lms: RgbMatrix,
    lms2rgb: RgbMatrix,
    min_rgb: f32,
    max_rgb: f32,
    min_luma: f32,
    max_luma: f32,
}

impl GamutState {
    fn new(primaries: ColorPrimaries, params: GamutLutParams) -> Self {
        let m = rgb2lms(primaries);
        Self {
            lms2rgb: m.inverse(),
            rgb2lms: m,
            min_rgb: pq_eotf(params.min_luma) - 1e-6,
            max_rgb: pq_eotf(params.max_luma) + 1e-6,
            min_luma: params.min_luma,
            max_luma: params.max_luma,
        }
    }
}

#[allow(dead_code)]
fn ipt2ich(ipt: [f32; 3]) -> [f32; 3] {
    [
        ipt[0],
        (ipt[1] * ipt[1] + ipt[2] * ipt[2]).sqrt(),
        ipt[2].atan2(ipt[1]),
    ]
}

fn ich2ipt(ich: [f32; 3]) -> [f32; 3] {
    [ich[0], ich[1] * ich[2].cos(), ich[1] * ich[2].sin()]
}

/// Returns the maximally saturated in-gamut color of `gamut` at `hue`,
/// using a golden-section search over I with a bounded binary search for the
/// C boundary (mirrors libplacebo's `saturate`).
fn saturate(hue: f32, gamut: &GamutState) -> [f32; 3] {
    let inv_phi = 0.618_033_988_749_894_8_f32;
    let inv_phi2 = 0.381_966_011_250_105_15_f32;

    // Golden-section bracket over I, keeping the full (I, C, h) points so
    // each iteration re-bounds the boundary search like the C version.
    let (mut lo_i, mut lo_c) = (gamut.min_luma, 0.0_f32);
    let (hi_i, mut hi_c) = (gamut.max_luma, 0.0_f32);
    let mut de = hi_i - lo_i;
    let mut a = [lo_i + inv_phi2 * de, 0.0, hue];
    let mut b = [lo_i + inv_phi * de, 0.0, hue];
    a[1] = desat_bounded(a[0], hue, 0.0, 0.5, gamut)[1];
    b[1] = desat_bounded(b[0], hue, 0.0, 0.5, gamut)[1];

    while de > 5e-5 {
        de *= inv_phi;
        if a[1] > b[1] {
            hi_c = b[1];
            b = a;
            a[0] = lo_i + inv_phi2 * de;
            a[1] = desat_bounded(a[0], hue, lo_c - 5e-5, 0.5, gamut)[1];
        } else {
            lo_i = a[0];
            lo_c = a[1];
            a = b;
            b[0] = lo_i + inv_phi * de;
            b[1] = desat_bounded(b[0], hue, hi_c - 5e-5, 0.5, gamut)[1];
        }
    }

    if a[1] > b[1] {
        [a[0], a[1], hue]
    } else {
        [b[0], b[1], hue]
    }
}

/// Find the gamut boundary at luminance `i` and hue `h` within `[cmin, cmax]`.
fn desat_bounded(i: f32, h: f32, cmin: f32, cmax: f32, gamut: &GamutState) -> [f32; 3] {
    if i <= gamut.min_luma {
        return [gamut.min_luma, 0.0, h];
    }
    if i >= gamut.max_luma {
        return [gamut.max_luma, 0.0, h];
    }
    let max_di = i * 5e-5;
    let mut lo = cmin;
    let mut hi = cmax;
    loop {
        let c = (lo + hi) / 2.0;
        if ingamut(ich2ipt([i, c, h]), gamut) {
            lo = c;
        } else {
            hi = c;
        }
        if hi - lo <= max_di {
            return [i, (lo + hi) / 2.0, h];
        }
    }
}

fn ingamut(ipt: [f32; 3], gamut: &GamutState) -> bool {
    let rgb = ipt2rgb(ipt, gamut);
    rgb[0] >= gamut.min_rgb
        && rgb[0] <= gamut.max_rgb
        && rgb[1] >= gamut.min_rgb
        && rgb[1] <= gamut.max_rgb
        && rgb[2] >= gamut.min_rgb
        && rgb[2] <= gamut.max_rgb
}

/// Convert an f32 to IEEE-754 binary16 (round-to-nearest-even) for packing
/// RGBA16F textures. The gamut LUT texels carry values in [-0.5, 1.0], so a
/// half-float texture is exact enough and is filterable on every backend
/// (Rgba32Float is not filterable on wgpu).
pub fn f32_to_f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = (bits >> 16) & 0x8000;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7fffff;
    if exp == 0xff {
        return (sign | 0x7c00 | u32::from(mant != 0)) as u16;
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return (sign | 0x7c00) as u16; // overflow -> inf
    }
    if e <= 0 {
        return if e <= -10 {
            sign as u16 // underflow -> signed zero
        } else {
            let m = mant | 0x800000;
            let shift = (14 - e) as u32;
            let rounded = (m >> shift) + u32::from((m >> (shift - 1)) & 1 == 1);
            (sign | rounded) as u16
        };
    }
    let rounded = mant + 0x1000 + ((mant >> 13) & 1);
    let mut e = e as u32;
    if rounded & 0x800000 != 0 {
        e += 1;
    }
    (sign | (e << 10) | ((rounded >> 13) & 0x3ff)) as u16
}

/// Pack the f32 texel triples into interleaved RGBA16F little-endian bytes,
/// matching the `Rgba16Float` textures of the Metal/wgpu/D3D11 backends.
pub fn pack_rgba16f(texels: &[[f32; 3]], alpha: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(texels.len() * 8);
    for texel in texels {
        for channel in texel.iter().copied().chain(std::iter::once(alpha)) {
            bytes.extend_from_slice(&f32_to_f16(channel).to_le_bytes());
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_packing_roundtrips_and_matches_half_layout() {
        // Known IEEE-754 binary16 bit patterns.
        assert_eq!(f32_to_f16(0.0), 0x0000);
        assert_eq!(f32_to_f16(1.0), 0x3c00);
        assert_eq!(f32_to_f16(0.5), 0x3800);
        assert_eq!(f32_to_f16(-0.5), 0xb800);
        assert_eq!(f32_to_f16(10000.0), 0x70e2);
        assert_eq!(f32_to_f16(-1.0), 0xbc00);
        // Padded alpha keeps texel pitch at 8 bytes.
        let packed = pack_rgba16f(&[[0.0, 0.5, 1.0]], 1.0);
        assert_eq!(packed.len(), 8);
        assert_eq!(&packed[0..2], &0x0000_u16.to_le_bytes());
        assert_eq!(&packed[2..4], &0x3800_u16.to_le_bytes());
        assert_eq!(&packed[4..6], &0x3c00_u16.to_le_bytes());
        assert_eq!(&packed[6..8], &0x3c00_u16.to_le_bytes());
    }

    #[test]
    fn lut_generation_is_deterministic_and_bounded() {
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.0,
            max_luma: 1.0,
        };
        let a = GamutLut::generate(params);
        let b = GamutLut::generate(params);
        assert_eq!(a.texels.len(), LUT_SIZE_I * LUT_SIZE_C * LUT_SIZE_H);
        assert_eq!(a, b);
        for texel in &a.texels {
            assert!(
                texel[0] >= 0.0 && texel[0] <= 1.0,
                "I out of range: {texel:?}"
            );
            assert!(
                texel[1] >= 0.0 && texel[1] <= 1.0,
                "P out of range: {texel:?}"
            );
            assert!(
                texel[2] >= 0.0 && texel[2] <= 1.0,
                "T out of range: {texel:?}"
            );
        }
    }

    #[test]
    fn in_gamut_colors_stay_near_identity_in_dead_zone() {
        // A low-chroma, mid-intensity color (inside both gamuts) maps close
        // to itself: the dead-zone blend keeps k small.
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.0,
            max_luma: 1.0,
        };
        // I=0.5 (mid), C=0.02, h=0
        let src = GamutState::new(params.source, params);
        let dst = GamutState::new(params.target, params);
        let h = 0.0_f32;
        let src_peak = saturate(h, &src);
        let dst_peak = saturate(h, &dst);
        let soft = Softclip {
            knee: 0.70,
            _desat: 0.35,
        };
        let max_c = src_peak[1].max(dst_peak[1]).max(1e-9);
        let out = perceptual_map_at(
            [0.5, 0.02, h],
            &src,
            &dst,
            src_peak,
            dst_peak,
            max_c,
            &soft,
            params,
        );
        assert!(
            (out[0] - 0.5).abs() < 0.05 && (out[1] - 0.02).abs() < 0.1,
            "out={out:?}"
        );
    }

    #[test]
    fn saturated_wide_gamut_color_is_brought_in_gamut() {
        // BT.2020 primary green is far outside BT.709; the map must reduce
        // its chroma without flipping the hue wildly.
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.5,
            max_luma: 0.9,
        };
        // I at mid-display, high chroma, hue of ~primary green in IPT.
        let src = GamutState::new(params.source, params);
        let dst = GamutState::new(params.target, params);
        let h = 0.9_f32;
        let src_peak = saturate(h, &src);
        let dst_peak = saturate(h, &dst);
        let soft = Softclip {
            knee: 0.70,
            _desat: 0.35,
        };
        let max_c = src_peak[1].max(dst_peak[1]).max(1e-9);
        let out = perceptual_map_at(
            [0.7, 0.4, h],
            &src,
            &dst,
            src_peak,
            dst_peak,
            max_c,
            &soft,
            params,
        );
        let rgb = ipt2rgb([out[0], out[1], out[2]], &dst);
        assert!(
            rgb.iter().all(|v| *v >= -1e-4 && *v <= 1.0 + 1e-4),
            "still out of gamut: {rgb:?}"
        );
    }
}

#[cfg(test)]
mod anchor_audit {
    use super::*;
    use crate::core::ColorPrimaries;

    #[test]
    fn grayscale_anchors_survive_perceptual_map() {
        // Black/mid-gray/white (chroma ≈ 0) must keep chroma near zero after
        // the perceptual map; a LUT numeric bug shows up here first.
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.0,
            max_luma: 1.0,
        };
        let src = GamutState::new(params.source, params);
        let dst = GamutState::new(params.target, params);
        // IPT mid-gray: any I, P = T = 0
        for i in [0.0, 0.1, 0.5, 0.9] {
            let mapped = perceptual_map_at(
                [i, 0.0, 0.0],
                &src,
                &dst,
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0],
                1.0,
                &Softclip {
                    knee: 0.7,
                    _desat: 0.35,
                },
                params,
            );
            assert!(
                mapped[1].abs() < 1e-3 && mapped[2].abs() < 1e-3,
                "chroma leak on gray: {mapped:?}"
            );
        }
    }

    #[test]
    fn lut_gray_column_keeps_chroma_near_zero_and_rises_with_i() {
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.0,
            max_luma: 1.0,
        };
        let lut = GamutLut::generate(params);
        // C = 0 (chroma axis 0), mid hue: chroma should stay near zero and
        // I should increase along the lattice axis.
        let mut previous_i = None;
        for ix in [0, 8, 24, 47] {
            let idx = 128 * LUT_SIZE_C * LUT_SIZE_I + ix;
            let t = lut.texels[idx];
            // Texels pack (I, P + 0.5, T + 0.5); zero chroma is 0.5/0.5.
            assert!(
                (t[1] - 0.5).abs() < 1e-3 && (t[2] - 0.5).abs() < 1e-3,
                "chroma leak at I index {ix}: {t:?}"
            );
            if let Some(previous) = previous_i {
                let current = t[0];
                assert!(
                    current >= previous,
                    "I not monotone at index {ix}: {current} < {previous}"
                );
            }
            previous_i = Some(t[0]);
        }
    }
}

#[cfg(test)]
mod layout_audit {
    use super::*;
    use crate::core::ColorPrimaries;

    #[test]
    fn texel_layout_is_i_major_within_h_slice() {
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.0,
            max_luma: 1.0,
        };
        let lut = GamutLut::generate(params);
        // Lattice order is h-outer / C-mid / I-inner: walking one I step must
        // stay inside the same (h, C) cell, so the mid-h gray column rises in
        // I and stays chroma-free, while the C=1 row starts after LUT_SIZE_I.
        let mid_h_start = 128 * LUT_SIZE_C * LUT_SIZE_I;
        let first_i = lut.texels[mid_h_start];
        let last_i = lut.texels[mid_h_start + LUT_SIZE_I - 1];
        assert!(
            last_i[0] >= first_i[0],
            "I axis must increase along the inner lattice: {first_i:?} -> {last_i:?}"
        );
        // Next C row begins exactly after one full I axis.
        let next_c = lut.texels[mid_h_start + LUT_SIZE_I];
        assert!(
            (next_c[1] - 0.5).abs() > (first_i[1] - 0.5).abs(),
            "C=0 and C=1 rows should differ in chroma packing: {first_i:?} vs {next_c:?}"
        );
    }
}

#[cfg(test)]
mod dim_audit {
    use super::*;
    use crate::core::ColorPrimaries;

    #[test]
    fn map_keeps_mid_brightness() {
        // Tone-mapped mid-grays must not collapse to black: I stays near the
        // input band even with a little chroma.
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.0,
            max_luma: pq_code_for_t(203.0),
        };
        let src = GamutState::new(params.source, params);
        let dst = GamutState::new(params.target, params);
        for (i, c) in [
            (0.3, 0.0),
            (0.3, 0.02),
            (0.3, 0.05),
            (0.3, 0.1),
            (0.5, 0.05),
        ] {
            let h = 1.0_f32;
            let sp = saturate(h, &src);
            let dp = saturate(h, &dst);
            let maxc = sp[1].max(dp[1]).max(1e-9);
            let out = perceptual_map_at(
                [i, c, h],
                &src,
                &dst,
                sp,
                dp,
                maxc,
                &Softclip {
                    knee: 0.7,
                    _desat: 0.35,
                },
                params,
            );
            assert!(
                out[0] > i * 0.5,
                "mid-gray I collapsed: in I={i} C={c} -> out {:?}",
                out
            );
            assert!(out[0].is_finite() && out[1].is_finite() && out[2].is_finite());
        }
    }

    fn pq_code_for_t(n: f32) -> f32 {
        let m1 = 0.1593017578125_f32;
        let m2 = 78.84375_f32;
        let c1 = 0.8359375_f32;
        let c2 = 18.8515625_f32;
        let c3 = 18.6875_f32;
        let p = (n / 10000.0).clamp(0.0, 1.0).powf(m1);
        ((c1 + c2 * p) / (1.0 + c3 * p).max(1e-6)).powf(m2)
    }
}
