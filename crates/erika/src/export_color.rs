//! The players' colour path for HDR pictures, evaluated on the CPU for
//! headless exports.
//!
//! Erika shows an HDR source on an SDR output through the shared
//! [`VideoRenderPipeline`] and the video fragment shader
//! (`renderer/wgpu_video.wgsl`, kept equivalent to Metal's
//! `VIDEO_SHADER_SOURCE`): the PQ EOTF (or the HLG inverse OETF and BT.2100
//! OOTF) to absolute nits, the ITU-R BT.2390 EETF on the intensity axis of
//! LMS-PQ-IPT against the 203-nit SDR target of BT.2408
//! ([`OutputDescription::tone_map_target_for`]), black-point compensation for
//! a 1000:1 target, libplacebo's perceptual gamut LUT for BT.2020 to BT.709,
//! and a gamma 2.2 encode. A frame export of an HDR source runs the same
//! uniforms through the same arithmetic per pixel here, so the exported
//! picture is the one Erika shows on an SDR display instead of PQ or HLG code
//! values read as SDR (a grey, flat picture). SDR sources keep the plain
//! swscale conversion.

use std::sync::{Arc, Mutex};
use std::thread;

use erika_ffmpeg_sys as sys;

use crate::ffmpeg::Frame;
use crate::renderer::gamut::{
    GamutLut, GamutLutParams, LUT_SIZE_C, LUT_SIZE_H, LUT_SIZE_I, f32_to_f16,
};
use crate::renderer::output::OutputDescription;
use crate::renderer::pipeline::{
    ColorRange, SourceColorState, VideoRenderPipeline, VideoUniforms, code_to_primaries,
};

/// How an HDR frame is rendered for an SDR picture: the uniforms Erika's
/// renderer would bind for it, and the gamut LUT when one is active.
pub(crate) struct SdrRendition {
    uniforms: VideoUniforms,
    lut: Option<Arc<GamutLut>>,
    dovi: bool,
    bits: u32,
}

/// The colour state the renderers build for a decoded frame
/// (`source_color_for_player_frame` in the wgpu renderer, the Metal frame
/// import), without the presenter's measured scene average: the default
/// BT.2390 curve does not use it.
pub(crate) fn source_color(frame: &Frame) -> SourceColorState {
    SourceColorState::new(frame.color_primaries(), frame.transfer_function())
        .range(frame.color_range().resolve(ColorRange::Limited))
        .matrix(frame.matrix_coefficients())
        .hdr_metadata(frame.hdr_metadata())
        .dovi(frame.dovi_metadata())
}

impl SdrRendition {
    /// The rendition for `frame` when it is HDR (PQ or HLG transfer, a peak
    /// above SDR, or Dolby Vision metadata); `None` for SDR frames.
    pub(crate) fn for_frame(frame: &Frame) -> Option<Self> {
        let source = source_color(frame);
        Self::for_source(source, frame_bit_depth(frame))
    }

    pub(crate) fn for_source(source: SourceColorState, bits: u32) -> Option<Self> {
        if !source.is_hdr() && source.dovi.is_none() {
            return None;
        }
        let target = OutputDescription::sdr().tone_map_target_for(&source);
        let pipeline = VideoRenderPipeline::new(source, target);
        let uniforms = VideoUniforms::from_pipeline(&pipeline, bits > 8, false);
        let lut = (uniforms.gamut_lut_enabled != 0).then(|| {
            cached_gamut_lut(GamutLutParams {
                source: code_to_primaries(uniforms._gamut_primaries >> 8),
                target: code_to_primaries(uniforms._gamut_primaries & 0xff),
                min_luma: pq_code(uniforms.tone_map_extra[2]),
                max_luma: pq_code(target_peak_nits(&uniforms)),
            })
        });
        Some(Self {
            uniforms,
            lut,
            dovi: source.dovi.is_some(),
            bits,
        })
    }

    /// Scales `frame` to `width` x `height` (YCbCr at the source's bit depth,
    /// bicubic, no colour conversion) and renders every pixel to RGBA8 with
    /// alpha 255.
    pub(crate) fn render(&self, frame: &Frame, width: u32, height: u32) -> Result<Vec<u8>, String> {
        let planes = scale_to_yuv444(frame, self.bits, width, height)?;
        let mut rgba = vec![0_u8; width as usize * height as usize * 4];
        let row_bytes = width as usize * 4;
        let workers = thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, 16)
            .min(height as usize);
        let rows_per_worker = (height as usize).div_ceil(workers.max(1));
        thread::scope(|scope| {
            for (chunk_index, chunk) in rgba.chunks_mut(rows_per_worker * row_bytes).enumerate() {
                let planes = &planes;
                scope.spawn(move || {
                    let first_row = chunk_index * rows_per_worker;
                    for (offset, out_row) in chunk.chunks_exact_mut(row_bytes).enumerate() {
                        let row = first_row + offset;
                        for (column, pixel) in out_row.chunks_exact_mut(4).enumerate() {
                            let (y, cb, cr) = planes.sample(column, row);
                            let rgb = self.shade(y, cb, cr);
                            pixel[0] = unorm8(rgb[0]);
                            pixel[1] = unorm8(rgb[1]);
                            pixel[2] = unorm8(rgb[2]);
                            pixel[3] = 255;
                        }
                    }
                });
            }
        });
        Ok(rgba)
    }

    /// One pixel through the fragment shader (`erika_video_fragment`, plane
    /// input). `y`, `cb` and `cr` are code values over `2^bits - 1`, the
    /// normalization the shader reconstructs from its textures.
    pub(crate) fn shade(&self, y: f32, cb: f32, cr: f32) -> [f32; 3] {
        let u = &self.uniforms;
        let mut rgb = if self.dovi {
            dovi_signal_to_pq_rgb(u, [y, cb, cr])
        } else {
            let (y, cbcr) = expand_ycbcr_range(u, self.bits, y, [cb, cr]);
            let kr = u.luma_coefficients[0];
            let kg = u.luma_coefficients[1].max(0.000_001);
            let kb = u.luma_coefficients[2];
            let r = y + 2.0 * (1.0 - kr) * cbcr[1];
            let b = y + 2.0 * (1.0 - kb) * cbcr[0];
            let g = (y - kr * r - kb * b) / kg;
            [r, g, b]
        };
        rgb = transfer_to_source_reference_linear(u, rgb);
        if self.dovi {
            rgb = mat_rows(&u.dovi.lms_matrix, rgb);
        }
        rgb = source_reference_to_nits(u, rgb);
        rgb = tone_map_nits(u, self.lut.as_deref(), rgb);
        rgb = target_nits_to_reference_linear(u, rgb);
        rgb = gamut_compress(rgb);
        rgb = target_reference_linear_to_output(u, rgb);
        rgb.map(|value| value.clamp(0.0, 1.0))
    }
}

fn unorm8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Bits per component the frame's samples are rendered at: its pixel
/// format's depth rounded up to 8, 10, 12 or 16 (8 when unknown).
fn frame_bit_depth(frame: &Frame) -> u32 {
    let descriptor = unsafe { sys::av_pix_fmt_desc_get((*frame.as_ptr()).format) };
    let depth = if descriptor.is_null() {
        8
    } else {
        unsafe { (*descriptor).comp[0].depth as u32 }
    };
    match depth {
        0..=8 => 8,
        9..=10 => 10,
        11..=12 => 12,
        _ => 16,
    }
}

/// The LUT for `params` as the renderers sample it: they upload it as an
/// RGBA16F texture (`pack_rgba16f`), so the texels are rounded to half
/// precision here too. Generated once per process for the latest two
/// parameter sets: generation costs a few hundred milliseconds.
fn cached_gamut_lut(params: GamutLutParams) -> Arc<GamutLut> {
    static CACHE: Mutex<Vec<(GamutLutParams, Arc<GamutLut>)>> = Mutex::new(Vec::new());
    if let Some(lut) = CACHE.lock().ok().and_then(|cache| {
        cache
            .iter()
            .find(|(key, _)| *key == params)
            .map(|(_, lut)| lut.clone())
    }) {
        return lut;
    }
    let mut lut = GamutLut::generate(params);
    for texel in &mut lut.texels {
        *texel = texel.map(|value| f16_to_f32(f32_to_f16(value)));
    }
    let lut = Arc::new(lut);
    if let Ok(mut cache) = CACHE.lock() {
        cache.retain(|(key, _)| *key != params);
        cache.push((params, lut.clone()));
        if cache.len() > 2 {
            cache.remove(0);
        }
    }
    lut
}

/// IEEE 754 binary16 to f32.
fn f16_to_f32(half: u16) -> f32 {
    let sign = if half & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((half >> 10) & 0x1f);
    let mantissa = f32::from(half & 0x3ff);
    match exponent {
        0 => sign * mantissa * 2.0_f32.powi(-24),
        0x1f if mantissa == 0.0 => sign * f32::INFINITY,
        0x1f => f32::NAN,
        _ => sign * (1.0 + mantissa / 1024.0) * 2.0_f32.powi(exponent - 15),
    }
}

/// YCbCr 4:4:4 code values at the output size, at the source's bit depth.
struct Yuv444 {
    width: usize,
    /// `2^bits - 1`: what the shader's normalized samples are code values
    /// over (P010's MSB-aligned codes included, after its 65535/65472 fix).
    max_code: f32,
    y: Vec<u16>,
    cb: Vec<u16>,
    cr: Vec<u16>,
}

impl Yuv444 {
    fn sample(&self, column: usize, row: usize) -> (f32, f32, f32) {
        let index = row * self.width + column;
        let scale = 1.0 / self.max_code;
        (
            self.y[index] as f32 * scale,
            self.cb[index] as f32 * scale,
            self.cr[index] as f32 * scale,
        )
    }
}

/// Scales the frame's YCbCr planes to `width` x `height` as 4:4:4 at the
/// source's bit depth (8, 10, 12 or 16) with swscale (bicubic), so code
/// values stay codes and no bit-depth rescaling is involved. The output keeps
/// the source's range, matrix, primaries and transfer: swscale converts no
/// colour.
fn scale_to_yuv444(frame: &Frame, bits: u32, width: u32, height: u32) -> Result<Yuv444, String> {
    struct OwnedFrame(*mut sys::AVFrame);
    impl Drop for OwnedFrame {
        fn drop(&mut self) {
            unsafe { sys::av_frame_free(&mut self.0) };
        }
    }
    struct OwnedSws(*mut sys::SwsContext);
    impl Drop for OwnedSws {
        fn drop(&mut self) {
            unsafe { sys::sws_free_context(&mut self.0) };
        }
    }
    let check = |code: i32, operation: &str| {
        if code < 0 {
            Err(format!("{operation} failed ({code})"))
        } else {
            Ok(())
        }
    };
    let (format, out_bits) = match bits {
        0..=8 => (sys::AVPixelFormat_AV_PIX_FMT_YUV444P, 8),
        9..=10 => (sys::AVPixelFormat_AV_PIX_FMT_YUV444P10LE, 10),
        11..=12 => (sys::AVPixelFormat_AV_PIX_FMT_YUV444P12LE, 12),
        _ => (sys::AVPixelFormat_AV_PIX_FMT_YUV444P16LE, 16),
    };
    let source = OwnedFrame(unsafe { sys::av_frame_alloc() });
    let output = OwnedFrame(unsafe { sys::av_frame_alloc() });
    let scaler = OwnedSws(unsafe { sys::sws_alloc_context() });
    if source.0.is_null() || output.0.is_null() || scaler.0.is_null() {
        return Err("FFmpeg allocation failed".to_string());
    }
    unsafe {
        check(
            sys::av_frame_ref(source.0, frame.as_ptr()),
            "av_frame_ref(HDR export)",
        )?;
        if (*source.0).color_range == sys::AVColorRange_AVCOL_RANGE_UNSPECIFIED {
            (*source.0).color_range = sys::AVColorRange_AVCOL_RANGE_MPEG;
        }
        (*output.0).format = format;
        (*output.0).width = width as i32;
        (*output.0).height = height as i32;
        (*output.0).color_range = (*source.0).color_range;
        (*output.0).colorspace = (*source.0).colorspace;
        (*output.0).color_primaries = (*source.0).color_primaries;
        (*output.0).color_trc = (*source.0).color_trc;
        (*output.0).chroma_location = (*source.0).chroma_location;
        check(
            sys::av_frame_get_buffer(output.0, 0),
            "av_frame_get_buffer(HDR export)",
        )?;
        (*scaler.0).flags = (sys::SwsFlags_SWS_BICUBIC | sys::SwsFlags_SWS_ACCURATE_RND) as _;
        (*scaler.0).threads = 0;
        check(
            sys::sws_scale_frame(scaler.0, output.0, source.0),
            "sws_scale_frame(HDR export)",
        )?;
        let (w, h) = (width as usize, height as usize);
        let plane = |index: usize| -> Vec<u16> {
            let stride = (*output.0).linesize[index] as usize;
            let base = (*output.0).data[index];
            let mut values = Vec::with_capacity(w * h);
            for row in 0..h {
                let line = base.add(row * stride);
                if out_bits == 8 {
                    values.extend(
                        std::slice::from_raw_parts(line, w)
                            .iter()
                            .map(|&v| u16::from(v)),
                    );
                } else {
                    values.extend(
                        std::slice::from_raw_parts(line, w * 2)
                            .chunks_exact(2)
                            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]])),
                    );
                }
            }
            values
        };
        Ok(Yuv444 {
            width: w,
            max_code: ((1_u32 << out_bits) - 1) as f32,
            y: plane(0),
            cb: plane(1),
            cr: plane(2),
        })
    }
}

// ---------------------------------------------------------------------------
// The fragment shader, function for function (`renderer/wgpu_video.wgsl`).
// ---------------------------------------------------------------------------

fn source_peak_nits(u: &VideoUniforms) -> f32 {
    u.nits[0].max(1.0)
}

fn target_peak_nits(u: &VideoUniforms) -> f32 {
    u.nits[1].max(1.0)
}

fn source_reference_white_nits(u: &VideoUniforms) -> f32 {
    u.nits[2].max(1.0)
}

fn target_reference_white_nits(u: &VideoUniforms) -> f32 {
    u.nits[3].max(1.0)
}

// SMPTE ST 2084 constants; each is exact in f32.
const PQ_M1: f32 = 2610.0 / 16384.0;
const PQ_M2: f32 = 2523.0 / 32.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 128.0;
const PQ_C3: f32 = 2392.0 / 128.0;

fn pq_eotf(encoded: f32) -> f32 {
    let p = encoded.max(0.0).powf(1.0 / PQ_M2);
    let num = (p - PQ_C1).max(0.0);
    let den = (PQ_C2 - PQ_C3 * p).max(0.000_001);
    (num / den).powf(1.0 / PQ_M1)
}

fn pq_inverse_eotf(normalized_nits: f32) -> f32 {
    let p = normalized_nits.clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p).max(0.000_001)).powf(PQ_M2)
}

#[allow(clippy::excessive_precision)]
fn hlg_inverse_oetf(encoded: f32) -> f32 {
    let a = 0.178_832_77_f32;
    let b = 0.284_668_92_f32;
    let c = 0.559_910_73_f32;
    let e = encoded.max(0.0);
    if e <= 0.5 {
        return e * e / 3.0;
    }
    (((e - c) / a).exp() + b) / 12.0
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn row3(row: &[f32; 4]) -> [f32; 3] {
    [row[0], row[1], row[2]]
}

fn mat_rows(rows: &[[f32; 4]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        dot3(row3(&rows[0]), v),
        dot3(row3(&rows[1]), v),
        dot3(row3(&rows[2]), v),
    ]
}

fn transfer_to_source_reference_linear(u: &VideoUniforms, rgb_in: [f32; 3]) -> [f32; 3] {
    let rgb = rgb_in.map(|value| value.max(0.0));
    match u.source_transfer {
        3 => {
            let scale = 10_000.0 / source_reference_white_nits(u);
            rgb.map(|value| pq_eotf(value) * scale)
        }
        4 => {
            // HLG: inverse OETF to scene light, then the BT.2100 OOTF (system
            // gamma 1.2 at the 1000-nit nominal peak).
            let scene = rgb.map(hlg_inverse_oetf);
            let luma = dot3(row3(&u.luma_coefficients), scene).max(0.000_001);
            let scale = 1000.0 * luma.powf(1.2 - 1.0) / source_reference_white_nits(u);
            scene.map(|value| value * scale)
        }
        1 => rgb.map(|value| value.powf(2.2)),
        2 => rgb.map(|value| value.powf(2.4)),
        _ => rgb,
    }
}

fn source_reference_to_nits(u: &VideoUniforms, rgb: [f32; 3]) -> [f32; 3] {
    let white = source_reference_white_nits(u);
    rgb.map(|value| value.max(0.0) * white)
}

fn pq_code(nits: f32) -> f32 {
    pq_inverse_eotf(nits.clamp(0.0, 10_000.0) / 10_000.0)
}

fn nits_from_pq(code: f32) -> f32 {
    10_000.0 * pq_eotf(code.clamp(0.0, 1.0))
}

fn sstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a * (1.0 - t) + b * t
}

fn st2094_pick_knee(
    src_min: f32,
    src_max: f32,
    src_avg: f32,
    dst_min: f32,
    dst_max: f32,
) -> [f32; 2] {
    let knee_adaptation = 0.4;
    let min_knee = 0.1;
    let max_knee = 0.8;
    let def_knee = 0.4;
    let src_knee_min = mix(src_min, src_max, min_knee);
    let src_knee_max = mix(src_min, src_max, max_knee);
    let dst_knee_min = mix(dst_min, dst_max, min_knee);
    let dst_knee_max = mix(dst_min, dst_max, max_knee);
    let fallback = mix(src_min, src_max, def_knee);
    let src_knee = if src_avg > 0.0 { src_avg } else { fallback }.clamp(src_knee_min, src_knee_max);
    let knee_t = (src_knee - src_min) / (src_max - src_min).max(0.000_001);
    let adapted = mix(dst_min, dst_max, knee_t);
    let tuning = 1.0 - sstep(max_knee, def_knee, knee_t) * sstep(min_knee, def_knee, knee_t);
    let adaptation = mix(knee_adaptation, 1.0, tuning);
    let dst_knee = mix(src_knee, adapted, adaptation).clamp(dst_knee_min, dst_knee_max);
    [src_knee, dst_knee.clamp(dst_knee_min, dst_knee_max)]
}

/// `tone_map_curve_pq`: the configured operator on the IPT intensity axis
/// (PQ codes).
fn tone_map_curve_pq(u: &VideoUniforms, x_in: f32, param: f32) -> f32 {
    let src_peak = source_peak_nits(u);
    let dst_peak = target_peak_nits(u);
    let src_avg = u.tone_map_extra[1];
    let dst_black = u.tone_map_extra[2];
    let in_min = 0.0_f32;
    let in_max = pq_code(src_peak).max(0.000_001);
    let out_min = pq_code(dst_black);
    let out_max = pq_code(dst_peak).max(0.000_001);
    let x = x_in.clamp(in_min, in_max);
    match u.tone_map {
        0 => x,
        1 => {
            let in_max_nits = src_peak.max(0.000_001);
            let out_range_nits = (dst_peak - dst_black).max(0.000_001);
            let peak = in_max_nits / out_range_nits;
            let contrast = if param > 0.0 { param } else { 0.5 };
            let offset = (1.0 - contrast) / contrast.max(0.000_001);
            let scale = (peak + offset) / peak;
            let t = nits_from_pq(x).clamp(0.0, in_max_nits) / out_range_nits;
            let mapped = t / (t + offset) * scale;
            pq_code(mapped * out_range_nits + dst_black)
        }
        2 => {
            let in_max_nits = src_peak.max(0.000_001);
            let out_range_nits = (dst_peak - dst_black).max(0.000_001);
            let peak = in_max_nits / out_range_nits;
            let j = if param > 0.0 { param } else { 0.3 };
            let a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
            let b = (j * j - 2.0 * j * peak + peak) / (peak - 1.0).max(0.000_001);
            let scale = (b * b + 2.0 * b * j + j * j) / (b - a);
            let t = nits_from_pq(x).clamp(0.0, in_max_nits) / out_range_nits;
            let mapped = if t > j { scale * (t + a) / (t + b) } else { t };
            pq_code(mapped * out_range_nits + dst_black)
        }
        3 => {
            // ITU-R BT.2390 EETF with black-point compensation.
            let knee_offset = if param > 0.0 { param } else { 1.0 };
            let max_lum = (out_max / in_max).clamp(0.0, 1.0);
            let min_lum = out_min / in_max;
            let ks = (1.0 + knee_offset) * max_lum - knee_offset;
            let bp = (1.0 / min_lum.max(0.000_001)).clamp(0.0, 4.0);
            let mut v = x / in_max;
            if ks < 1.0 && v > ks {
                let tb = (v - ks) / (1.0 - ks);
                let tb2 = tb * tb;
                let tb3 = tb2 * tb;
                v = (2.0 * tb3 - 3.0 * tb2 + 1.0) * ks
                    + (tb3 - 2.0 * tb2 + tb) * (1.0 - ks)
                    + (-2.0 * tb3 + 3.0 * tb2) * max_lum;
            }
            if v < 1.0 {
                v += min_lum * (1.0 - v).powf(bp);
                let gain = if max_lum < 1.0 {
                    1.0 / (1.0 + min_lum / max_lum * (1.0 - max_lum).powf(bp))
                } else {
                    1.0
                };
                v = gain * (v - min_lum) + min_lum;
            }
            v * in_max
        }
        4 => {
            let contrast = if param > 0.0 { param } else { 0.3 };
            let fallback_avg = (0.4 * src_peak).clamp(100.0, 400.0);
            let effective_src_avg = if src_avg > 0.0 { src_avg } else { fallback_avg };
            let [src_pivot, dst_pivot] =
                st2094_pick_knee(in_min, in_max, pq_code(effective_src_avg), out_min, out_max);
            let slope0 = (dst_pivot - out_min) / (src_pivot - in_min).max(0.000_001);
            let ratio = (1.5 * (in_max / out_max - 1.0)).clamp(0.2, 1.2);
            let slope = slope0.powf((1.0 - contrast) * ratio);
            let in_min0 = in_min - src_pivot;
            let in_max0 = in_max - src_pivot;
            let out_min0 = out_min - dst_pivot;
            let out_max0 = out_max - dst_pivot;
            let pa = (out_min0 - slope * in_min0) / (in_min0 * in_min0);
            let qa = (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0 * in_max0);
            let qb = -3.0 * (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0);
            let xr = x - src_pivot;
            let mapped = if xr > 0.0 {
                ((qa * xr + qb) * xr + slope) * xr
            } else {
                (pa * xr + slope) * xr
            };
            mapped + dst_pivot
        }
        5 => {
            let phdr = 1.0 + 32.0 * (src_peak / 10_000.0).powf(1.0 / 2.4);
            let psdr = 1.0 + 32.0 * (dst_peak / 10_000.0).powf(1.0 / 2.4);
            let mut t = (nits_from_pq(x) / src_peak.max(0.000_001)).powf(1.0 / 2.4);
            t = (1.0 + (phdr - 1.0) * t).ln() / phdr.ln();
            if t <= 0.7399 {
                t *= 1.0770;
            } else if t < 0.9909 {
                t = (-1.1510 * t + 2.7811) * t - 0.6302;
            } else {
                t = 0.5 * t + 0.5;
            }
            t = (psdr.powf(t) - 1.0) / (psdr - 1.0);
            let lb = dst_black.max(0.0).powf(1.0 / 2.4);
            let lw = dst_peak.max(0.0).powf(1.0 / 2.4);
            pq_code(((lw - lb) * t + lb).powf(2.4))
        }
        _ => {
            let [c1, c2, c3, _] = u.tone_map_coeffs;
            let x_nits = nits_from_pq(x);
            let y_nits = (c1 + c2 * x_nits) / (1.0 + c3 * x_nits).max(0.000_001);
            pq_code(y_nits.clamp(0.0, 10_000.0))
        }
    }
}

fn tone_map_nits(u: &VideoUniforms, lut: Option<&GamutLut>, nits: [f32; 3]) -> [f32; 3] {
    if u.target_transfer == 3 {
        let white = source_reference_white_nits(u);
        let mapped = mat_rows(
            &u.gamut_matrix_rows,
            nits.map(|value| value.max(0.0) / white),
        );
        return mapped.map(|value| (value * white).clamp(0.0, 10_000.0));
    }
    let rgb = nits.map(|value| value.max(0.0));
    let rows = &u.ipt_matrix_rows;
    let lms = [
        dot3(row3(&rows[0]), rgb),
        dot3(row3(&rows[1]), rgb),
        dot3(row3(&rows[2]), rgb),
    ];
    let lmspq = lms.map(pq_code);
    let mut ipt = [
        dot3([0.4, 0.4, 0.2], lmspq),
        dot3([4.455, -4.851, 0.396], lmspq),
        dot3([0.8056, 0.3572, -1.1628], lmspq),
    ];
    let i_orig = ipt[0];
    ipt[0] = tone_map_curve_pq(u, ipt[0], u.tone_map_extra[0]);
    let hull = |h: f32| ((h - 6.0) * h + 9.0) * h;
    let ratio = (i_orig / ipt[0].max(0.000_001)).min(hull(ipt[0]) / hull(i_orig).max(0.000_001));
    ipt[1] *= ratio;
    ipt[2] *= ratio;

    if let Some(lut) = lut.filter(|_| u.gamut_lut_enabled != 0) {
        let lut_min = pq_code(u.tone_map_extra[2]);
        let lut_max = pq_code(target_peak_nits(u)).max(0.000_001);
        let lut_range = (lut_max - lut_min).max(0.000_001);
        let position = [
            ((ipt[0] - lut_min) / lut_range).clamp(0.0, 1.0),
            (2.0 * (ipt[1] * ipt[1] + ipt[2] * ipt[2]).sqrt()).clamp(0.0, 1.0),
            0.5 + 0.5 * ipt[2].atan2(ipt[1]) / std::f32::consts::PI,
        ];
        let sampled = sample_gamut_lut(lut, position);
        ipt = [sampled[0], sampled[1] - 0.5, sampled[2] - 0.5];
    }
    let lmspq_out = [
        dot3([1.0, 0.097_568_9, 0.205_226], ipt),
        dot3([1.0, -0.113_876, 0.133_217], ipt),
        dot3([1.0, 0.032_615_1, -0.676_887], ipt),
    ];
    let lms_out = lmspq_out.map(nits_from_pq);
    [
        dot3(row3(&rows[6]), lms_out),
        dot3(row3(&rows[7]), lms_out),
        dot3(row3(&rows[8]), lms_out),
    ]
}

/// Trilinear lookup with clamp-to-edge addressing, as the renderers' linear
/// sampler does: the shader's texel-centre coordinate
/// `pos * (n - 1) / n + 0.5 / n` lands on lattice index `pos * (n - 1)`.
fn sample_gamut_lut(lut: &GamutLut, position: [f32; 3]) -> [f32; 3] {
    let sizes = [LUT_SIZE_I, LUT_SIZE_C, LUT_SIZE_H];
    let mut lower = [0_usize; 3];
    let mut upper = [0_usize; 3];
    let mut fraction = [0.0_f32; 3];
    for axis in 0..3 {
        let last = sizes[axis] - 1;
        let coordinate = position[axis].clamp(0.0, 1.0) * last as f32;
        let floor = coordinate.floor();
        lower[axis] = (floor as usize).min(last);
        upper[axis] = (lower[axis] + 1).min(last);
        fraction[axis] = coordinate - floor;
    }
    let texel = |i: usize, c: usize, h: usize| lut.texels[(h * LUT_SIZE_C + c) * LUT_SIZE_I + i];
    let mut result = [0.0_f32; 3];
    for (corner, weight) in [
        (
            (lower[0], lower[1], lower[2]),
            (1.0 - fraction[0]) * (1.0 - fraction[1]) * (1.0 - fraction[2]),
        ),
        (
            (upper[0], lower[1], lower[2]),
            fraction[0] * (1.0 - fraction[1]) * (1.0 - fraction[2]),
        ),
        (
            (lower[0], upper[1], lower[2]),
            (1.0 - fraction[0]) * fraction[1] * (1.0 - fraction[2]),
        ),
        (
            (upper[0], upper[1], lower[2]),
            fraction[0] * fraction[1] * (1.0 - fraction[2]),
        ),
        (
            (lower[0], lower[1], upper[2]),
            (1.0 - fraction[0]) * (1.0 - fraction[1]) * fraction[2],
        ),
        (
            (upper[0], lower[1], upper[2]),
            fraction[0] * (1.0 - fraction[1]) * fraction[2],
        ),
        (
            (lower[0], upper[1], upper[2]),
            (1.0 - fraction[0]) * fraction[1] * fraction[2],
        ),
        (
            (upper[0], upper[1], upper[2]),
            fraction[0] * fraction[1] * fraction[2],
        ),
    ] {
        let value = texel(corner.0, corner.1, corner.2);
        for channel in 0..3 {
            result[channel] += value[channel] * weight;
        }
    }
    result
}

fn gamut_compress(rgb: [f32; 3]) -> [f32; 3] {
    let lo = rgb[0].min(rgb[1]).min(rgb[2]);
    let outness = (-lo).max(0.0);
    let k = sstep(0.0, 1.0, outness);
    rgb.map(|value| mix(value, value.clamp(0.0, 1.0), k))
}

fn target_nits_to_reference_linear(u: &VideoUniforms, nits: [f32; 3]) -> [f32; 3] {
    let black = u.tone_map_extra[2];
    let peak = target_peak_nits(u);
    let range = (peak - black).max(0.0001);
    let white = target_reference_white_nits(u);
    nits.map(|value| (value - black).max(0.0) / range * (range / white))
}

fn target_reference_linear_to_output(u: &VideoUniforms, rgb: [f32; 3]) -> [f32; 3] {
    let rgb = rgb.map(|value| value.max(0.0));
    if u.target_transfer == 3 {
        let white = target_reference_white_nits(u);
        return rgb.map(|value| pq_inverse_eotf(value * white / 10_000.0));
    }
    if u.edr_output != 0 {
        return rgb;
    }
    match u.target_transfer {
        1 => rgb.map(|value| value.powf(1.0 / 2.2)),
        2 => rgb.map(|value| value.powf(1.0 / 2.4)),
        _ => rgb,
    }
}

/// Limited-range expansion for a `bits`-bit signal normalized over
/// `2^bits - 1`; the shader's 8-bit and P010 (10-bit) constants are the
/// `bits` = 8 and 10 cases.
fn expand_ycbcr_range(u: &VideoUniforms, bits: u32, y: f32, cbcr: [f32; 2]) -> (f32, [f32; 2]) {
    if u.full_range != 0 {
        return (y, cbcr.map(|value| value - 0.5));
    }
    let max = ((1_u32 << bits) - 1) as f32;
    let shift = (1_u32 << (bits - 8)) as f32;
    let black = 16.0 * shift / max;
    let luma_span = 219.0 * shift / max;
    let chroma_zero = 128.0 * shift / max;
    let chroma_span = 224.0 * shift / max;
    (
        (y - black) / luma_span,
        cbcr.map(|value| (value - chroma_zero) / chroma_span),
    )
}

// Dolby Vision RPU reshaping (libplacebo `pl_shader_dovi_reshape`).

fn dovi_reshaped_signal(u: &VideoUniforms, sig_in: [f32; 3]) -> [f32; 3] {
    let sig = sig_in.map(|value| value.clamp(0.0, 1.0));
    let mut result = sig;
    let flags = u.dovi.flags;
    for c in 0..3 {
        let segments = flags[1 + c] as u32;
        if segments == 0 {
            continue;
        }
        let mut s = result[c];
        let mut index = 0_usize;
        for i in 0..7_usize {
            let pivot = u.dovi.pivots[2 * c + i / 4][i % 4];
            if s >= pivot {
                index += 1;
            }
        }
        let coeff = u.dovi.coefficients[8 * c + index];
        if coeff[3] < 0.5 {
            s = (coeff[2] * s + coeff[1]) * s + coeff[0];
        } else {
            let base = 48 * c + coeff[1] as usize;
            let order = coeff[3] as u32;
            let sig_x = [
                sig[0] * sig[1],
                sig[0] * sig[2],
                sig[1] * sig[2],
                sig[0] * sig[1] * sig[2],
            ];
            let dot4 = |row: &[f32; 4], v: [f32; 4]| {
                row[0] * v[0] + row[1] * v[1] + row[2] * v[2] + row[3] * v[3]
            };
            s = coeff[0];
            s += dot3(row3(&u.dovi.mmr[base]), sig);
            s += dot4(&u.dovi.mmr[base + 1], sig_x);
            if order >= 2 {
                let sig2 = sig.map(|value| value * value);
                let sig_x2 = sig_x.map(|value| value * value);
                s += dot3(row3(&u.dovi.mmr[base + 2]), sig2);
                s += dot4(&u.dovi.mmr[base + 3], sig_x2);
                if order >= 3 {
                    let sig3 = [sig2[0] * sig[0], sig2[1] * sig[1], sig2[2] * sig[2]];
                    let sig_x3 = [
                        sig_x2[0] * sig_x[0],
                        sig_x2[1] * sig_x[1],
                        sig_x2[2] * sig_x[2],
                        sig_x2[3] * sig_x[3],
                    ];
                    s += dot3(row3(&u.dovi.mmr[base + 4]), sig3);
                    s += dot4(&u.dovi.mmr[base + 5], sig_x3);
                }
            }
        }
        let bounds = u.dovi.bounds[c];
        result[c] = s.clamp(bounds[0], bounds[1]);
    }
    result
}

fn dovi_signal_to_pq_rgb(u: &VideoUniforms, sig: [f32; 3]) -> [f32; 3] {
    let reshaped = dovi_reshaped_signal(u, sig);
    let offset = u.dovi.nonlinear_offset;
    let shifted = [
        reshaped[0] - offset[0],
        reshaped[1] - offset[1],
        reshaped[2] - offset[2],
    ];
    mat_rows(&u.dovi.nonlinear_matrix, shifted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ColorPrimaries, TransferFunction};
    use crate::renderer::pipeline::{HdrMetadata, MasteringDisplayMetadata};

    fn hdr10(peak: f32) -> SourceColorState {
        SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .range(ColorRange::Limited)
            .hdr_metadata(Some(HdrMetadata::new(
                Some(MasteringDisplayMetadata {
                    display_primaries: None,
                    white_point: None,
                    min_luminance_nits: Some(0.005),
                    max_luminance_nits: Some(peak),
                }),
                None,
            )))
    }

    /// A 10-bit limited-range code for a PQ-encoded luminance, neutral chroma.
    fn neutral_pq(nits: f32) -> (f32, f32, f32) {
        let code = 64.0 + pq_code(nits) * 876.0;
        (code / 1023.0, 512.0 / 1023.0, 512.0 / 1023.0)
    }

    #[test]
    fn sdr_sources_are_left_to_swscale() {
        let sdr = SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Bt1886);
        assert!(SdrRendition::for_source(sdr, 8).is_none());
        assert!(SdrRendition::for_source(hdr10(1000.0), 10).is_some());
        let hlg = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Hlg);
        assert!(SdrRendition::for_source(hlg, 10).is_some());
    }

    #[test]
    fn pq_greys_stay_neutral_rise_monotonically_and_peak_reaches_white() {
        let rendition = SdrRendition::for_source(hdr10(1000.0), 10).unwrap();
        assert_eq!(rendition.uniforms.tone_map, 3, "BT.2390 is the operator");
        let mut previous = -1.0;
        for nits in [0.0, 0.5, 5.0, 20.0, 50.0, 100.0, 203.0, 400.0, 1000.0] {
            let (y, cb, cr) = neutral_pq(nits);
            let rgb = rendition.shade(y, cb, cr);
            assert!(
                (rgb[0] - rgb[1]).abs() < 0.01 && (rgb[1] - rgb[2]).abs() < 0.01,
                "{nits}: {rgb:?}"
            );
            assert!(rgb[1] > previous, "{nits} nits did not rise: {rgb:?}");
            previous = rgb[1];
        }
        let (y, cb, cr) = neutral_pq(0.0);
        assert!(rendition.shade(y, cb, cr)[1] < 0.01);
        // The mastering peak maps onto the 203-nit target peak: full white.
        let (y, cb, cr) = neutral_pq(1000.0);
        assert!(rendition.shade(y, cb, cr)[1] > 0.98);
    }

    #[test]
    fn tone_mapping_brightens_what_a_plain_conversion_shows_grey() {
        // Diffuse white of an HDR10 master (203 nits) is PQ code 0.58: read
        // as gamma-encoded SDR that is a mid grey; Erika's SDR rendition
        // shows it near the top of the range.
        let rendition = SdrRendition::for_source(hdr10(1000.0), 10).unwrap();
        let (y, cb, cr) = neutral_pq(203.0);
        let plain = (y - 64.0 / 1023.0) * (1023.0 / 876.0);
        let shown = rendition.shade(y, cb, cr)[1];
        assert!((plain - 0.58).abs() < 0.01, "{plain}");
        assert!(shown > 0.8, "{shown}");
    }

    #[test]
    fn hlg_reference_white_lands_near_sdr_white() {
        let hlg = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Hlg)
            .range(ColorRange::Limited);
        let rendition = SdrRendition::for_source(hlg, 10).unwrap();
        // BT.2408: HLG reference white is a 75% signal.
        let code = 64.0 + 0.75 * 876.0;
        let rgb = rendition.shade(code / 1023.0, 512.0 / 1023.0, 512.0 / 1023.0);
        assert!(rgb[1] > 0.75 && rgb[1] <= 1.0, "{rgb:?}");
    }

    #[test]
    fn saturated_bt2020_red_maps_into_bt709_without_turning_pink() {
        let rendition = SdrRendition::for_source(hdr10(1000.0), 10).unwrap();
        assert!(
            rendition.lut.is_some(),
            "BT.2020 to BT.709 uses the gamut LUT"
        );
        // BT.2020 primary red at 100 nits, encoded and converted to YCbCr.
        let r = pq_code(100.0);
        let (kr, kb) = (0.2627, 0.0593);
        let y = kr * r;
        let cb = (0.0 - y) / (2.0 * (1.0 - kb));
        let cr = (r - y) / (2.0 * (1.0 - kr));
        let rgb = rendition.shade(
            (64.0 + y * 876.0) / 1023.0,
            (512.0 + cb * 896.0) / 1023.0,
            (512.0 + cr * 896.0) / 1023.0,
        );
        assert!(rgb[0] > 0.6, "{rgb:?}");
        assert!(rgb[1] < 0.15 && rgb[2] < 0.15, "{rgb:?}");
    }

    #[test]
    fn half_precision_round_trips_like_the_texture_upload() {
        for value in [0.0_f32, 1.0, 0.5, -0.25, 0.077_8, 0.999_9, 6.1e-5] {
            let half = f32_to_f16(value);
            let back = f16_to_f32(half);
            assert!(
                (back - value).abs() <= value.abs() * 1e-3 + 1e-7,
                "{value} -> {back}"
            );
            assert_eq!(f32_to_f16(back), half);
        }
    }

    #[test]
    fn lut_lookup_hits_lattice_points_exactly() {
        let lut = cached_gamut_lut(GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: pq_code(0.203),
            max_luma: pq_code(203.0),
        });
        let index = |i: usize, c: usize, h: usize| (h * LUT_SIZE_C + c) * LUT_SIZE_I + i;
        for (i, c, h) in [(0, 0, 0), (47, 31, 255), (10, 5, 100)] {
            let position = [
                i as f32 / (LUT_SIZE_I - 1) as f32,
                c as f32 / (LUT_SIZE_C - 1) as f32,
                h as f32 / (LUT_SIZE_H - 1) as f32,
            ];
            let sampled = sample_gamut_lut(&lut, position);
            let expected = lut.texels[index(i, c, h)];
            for channel in 0..3 {
                assert!((sampled[channel] - expected[channel]).abs() < 1e-5);
            }
        }
    }
}
