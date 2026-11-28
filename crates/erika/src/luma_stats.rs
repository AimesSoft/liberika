//! Frame-luminance statistics feeding the tone map's scene-adaptive pivot.
//!
//! mpv/libplacebo measure per-frame average/peak brightness on the GPU
//! (`--hdr-compute-peak`) and feed it to the tone-map curve, which is what
//! makes HDR10 (static-metadata-only) content behave like Dolby Vision's
//! per-frame L1. This module computes the same signals on the CPU for
//! software-decoded frames, sampling a sparse grid of rows/columns so a 4K
//! frame costs well under a millisecond, then smooths them with the same
//! IIR filter libplacebo uses (τ = smoothing_period).
//!
//! Only the luma plane is needed: HDR10 software frames arrive as NV12
//! (8-bit), `yuv420p10le` (10-bit packed LE, code in bits `[9:0]`), or P010
//! (10-bit MSB-aligned in 16-bit LE). The view reports which packing it
//! carries; treating `yuv420p10le` as P010 would measure near-black. The
//! frame's color range decides the sample normalization — HDR10 streams are
//! normally limited (TV) range, so measuring them as full range would lift
//! black and compress the highlights before the PQ re-encode.

/// How many rows/columns of the frame are sampled (stride = dim / SAMPLES).
const SAMPLES: usize = 48;
/// libplacebo `pl_peak_detect_default_params.smoothing_period` (frames).
const SMOOTHING_PERIOD: f32 = 20.0;

const PQ_M1: f32 = 2610.0 / 4096.0 * 1.0 / 4.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

/// One frame's measured luminance in PQ code (12-bit-ish precision).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameLumaStats {
    /// Mean luma in PQ code over the sampled grid.
    pub avg_pq: f32,
    /// Maximum sampled luma in PQ code.
    pub max_pq: f32,
}

/// Stateful IIR smoothing of the per-frame signals, mirroring libplacebo's
/// `update_peak_buf`: `state += coeff * (measured - state)` with
/// `coeff = 1 - exp(-1/smoothing_period)`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LumaSmoother {
    pub avg_pq: f32,
    pub max_pq: f32,
    /// 0 until the first frame is pushed.
    initialized: bool,
}

impl LumaSmoother {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a frame's measurements into the running estimate and return the
    /// smoothed (avg, max) in PQ code.
    pub fn push(&mut self, measured: FrameLumaStats) -> (f32, f32) {
        let coeff = 1.0 - (-1.0 / SMOOTHING_PERIOD).exp();
        if !self.initialized {
            self.avg_pq = measured.avg_pq;
            self.max_pq = measured.max_pq;
            self.initialized = true;
        } else {
            self.avg_pq += coeff * (measured.avg_pq - self.avg_pq);
            self.max_pq += coeff * (measured.max_pq - self.max_pq);
        }
        (self.avg_pq, self.max_pq)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Convert a normalized luma sample in [0, 1] (after range expansion and
/// bit-depth normalization) to its PQ code.
fn pq_code_of_luma(luma: f32) -> f32 {
    let p = luma.clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p)).powf(PQ_M2)
}

/// Convert a normalized PQ-encoded luma sample to a PQ code over 10 k nits.
/// The luma plane of an HDR10 frame holds PQ-encoded Y'; decode it to linear
/// nits first (PQ EOTF), then re-encode so the average is perceptual.
fn sample_code(normalized: f32) -> f32 {
    let linear_nits = pq_eotf(normalized.clamp(0.0, 1.0)) * 10000.0;
    pq_code_of_luma(linear_nits / 10000.0)
}

/// Measure a strided luma plane in place (see
/// `crate::ffmpeg::Frame::luma_plane_view`): 8-bit samples, or 10-bit samples
/// in either the `yuv420p10le` packed layout or the P010 MSB-aligned layout,
/// sampled on the same sparse grid as the packed variants. `full_range`
/// selects the sample normalization: full-range planes normalize by the code
/// maximum, limited (TV) planes by the legal 16..235 / 64..940 span, exactly
/// like the shaders' `expand_ycbcr_range`. This is the presenter's per-frame
/// path — repacking the whole frame with `to_planar_frame` would copy
/// megabytes to sample a 48x48 grid.
pub fn measure_luma_plane_view(
    view: &crate::ffmpeg::LumaPlaneView<'_>,
    full_range: bool,
) -> Option<FrameLumaStats> {
    let w = view.width() as usize;
    let h = view.height() as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let is_10bit = view.is_10bit();
    let sample_bytes = if is_10bit { 2 } else { 1 };
    let x_step = (w / SAMPLES).max(1);
    let y_step = (h / SAMPLES).max(1);
    let mut sum = 0.0_f64;
    let mut max_code = 0.0_f32;
    let mut count = 0_u64;
    let mut row_index = 0usize;
    while row_index < h {
        let Some(row) = view.row(row_index) else {
            break;
        };
        let sample_count = row.len() / sample_bytes;
        let mut x = 0usize;
        while x < sample_count {
            let sample = if is_10bit {
                let raw = u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]);
                // yuv420p10le keeps the code in bits [9:0]; p010le left-aligns
                // it in [15:6]. Treating both as P010 crushes software Main10.
                view.sample_layout()
                    .decode_10bit(raw)
                    .expect("10-bit layout decodes a 10-bit code") as f32
            } else {
                row[x] as f32
            };
            let normalized = expand_luma_sample(sample, is_10bit, full_range);
            let code = sample_code(normalized);
            sum += code as f64;
            if code > max_code {
                max_code = code;
            }
            count += 1;
            x += x_step;
        }
        row_index += y_step;
    }
    if count == 0 {
        return None;
    }
    Some(FrameLumaStats {
        avg_pq: (sum / count as f64) as f32,
        max_pq: max_code,
    })
}

/// Normalize one luma code to [0, 1] in the encoded domain, applying the
/// limited-range expansion when the plane is TV range.
fn expand_luma_sample(sample: f32, is_10bit: bool, full_range: bool) -> f32 {
    let (black, span, peak) = if is_10bit {
        (64.0, 876.0, 1023.0)
    } else {
        (16.0, 219.0, 255.0)
    };
    if full_range {
        return (sample / peak).clamp(0.0, 1.0);
    }
    ((sample - black) / span).clamp(0.0, 1.0)
}

fn pq_eotf(code: f32) -> f32 {
    let p = code.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
    let num = (p - PQ_C1).max(0.0);
    let den = (PQ_C2 - PQ_C3 * p).max(1e-9);
    (num / den).powf(1.0 / PQ_M1)
}

/// Convert a PQ code back to nits (for diagnostics and for feeding the
/// `tone_map_extra.y` scene-average slot, which expects nits).
pub fn pq_code_to_nits(code: f32) -> f32 {
    10000.0 * pq_eotf(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::LumaPlaneView;

    fn view(luma: &[u8], w: u32, h: u32, is_10bit: bool) -> LumaPlaneView<'_> {
        LumaPlaneView::from_packed(luma, w, h, is_10bit).unwrap()
    }

    #[test]
    fn nv12_black_frame_measures_near_zero() {
        let (w, h) = (1920_u32, 1080_u32);
        let luma = vec![0_u8; (w * h) as usize];
        let stats = measure_luma_plane_view(&view(&luma, w, h, false), true).unwrap();
        assert!(pq_code_to_nits(stats.avg_pq) < 0.5, "avg {}", stats.avg_pq);
        assert!(pq_code_to_nits(stats.max_pq) < 0.5, "max {}", stats.max_pq);
    }

    #[test]
    fn nv12_full_white_measures_ten_k_nits() {
        let (w, h) = (1920_u32, 1080_u32);
        let luma = vec![255_u8; (w * h) as usize];
        let stats = measure_luma_plane_view(&view(&luma, w, h, false), true).unwrap();
        assert!((pq_code_to_nits(stats.avg_pq) - 10000.0).abs() < 20.0);
        assert!((pq_code_to_nits(stats.max_pq) - 10000.0).abs() < 20.0);
    }

    #[test]
    fn p010_white_measures_ten_k_nits() {
        let (w, h) = (1920_u32, 1080_u32);
        let mut luma = Vec::with_capacity((w * h * 2) as usize);
        for _ in 0..(w * h) as usize {
            luma.extend_from_slice(&(1023_u16 << 6).to_le_bytes());
        }
        let stats = measure_luma_plane_view(&view(&luma, w, h, true), true).unwrap();
        assert!((pq_code_to_nits(stats.avg_pq) - 10000.0).abs() < 20.0);
    }

    #[test]
    fn yuv420p10le_white_measures_ten_k_nits() {
        // Software Main10 packs the 10-bit code in bits [9:0], not P010's
        // [15:6]. Measuring a full-range white plane as P010 would yield
        // sample 15 and a near-black scene average.
        let (w, h) = (1920_u32, 1080_u32);
        let mut luma = Vec::with_capacity((w * h * 2) as usize);
        for _ in 0..(w * h) as usize {
            luma.extend_from_slice(&1023_u16.to_le_bytes());
        }
        let packed = LumaPlaneView::from_packed_with_layout(
            &luma,
            w,
            h,
            crate::ffmpeg::LumaSampleLayout::Packed10,
        )
        .unwrap();
        let stats = measure_luma_plane_view(&packed, true).unwrap();
        assert!(
            (pq_code_to_nits(stats.avg_pq) - 10000.0).abs() < 20.0,
            "avg {}",
            stats.avg_pq
        );
    }

    #[test]
    fn yuv420p10le_limited_white_hits_ten_k_nits() {
        // Limited-range white (940) must expand to the code peak, not be
        // crushed by a mistaken >> 6.
        let (w, h) = (64_u32, 64_u32);
        let mut luma = Vec::with_capacity((w * h * 2) as usize);
        for _ in 0..(w * h) as usize {
            luma.extend_from_slice(&940_u16.to_le_bytes());
        }
        let packed = LumaPlaneView::from_packed_with_layout(
            &luma,
            w,
            h,
            crate::ffmpeg::LumaSampleLayout::Packed10,
        )
        .unwrap();
        let stats = measure_luma_plane_view(&packed, false).unwrap();
        assert!(
            (pq_code_to_nits(stats.avg_pq) - 10000.0).abs() < 20.0,
            "avg {}",
            stats.avg_pq
        );
    }

    #[test]
    fn strided_rows_measure_like_packed_rows() {
        // A stride larger than the visible row (decoder alignment padding)
        // must not change the measurement.
        let (w, h) = (64_u32, 48_u32);
        let row_bytes = w as usize;
        let stride = row_bytes + 64;
        let mut padded = vec![255_u8; stride * h as usize]; // padding bright
        for y in 0..h as usize {
            for x in 0..row_bytes {
                padded[y * stride + x] = 16;
            }
        }
        let strided_view = LumaPlaneView::from_strided(&padded, stride, w, h, false);
        // Same visible samples, tightly packed.
        let packed: Vec<u8> = (0..h as usize)
            .flat_map(|y| padded[y * stride..y * stride + row_bytes].to_vec())
            .collect();
        let strided = measure_luma_plane_view(&strided_view, true).unwrap();
        let packed_stats = measure_luma_plane_view(&view(&packed, w, h, false), true).unwrap();
        assert!((strided.avg_pq - packed_stats.avg_pq).abs() < 1e-6);
        assert!((strided.max_pq - packed_stats.max_pq).abs() < 1e-6);
        assert!(
            pq_code_to_nits(strided.max_pq) < 100.0,
            "padding leaked: {}",
            pq_code_to_nits(strided.max_pq)
        );
    }

    #[test]
    fn bright_highlights_raise_the_peak_but_not_the_mean() {
        let (w, h) = (1920_u32, 1080_u32);
        // Mostly dim (16/255) with a small bright region (255).
        let mut luma = vec![16_u8; (w * h) as usize];
        for y in (h / 4)..(h / 2) {
            for x in (w / 4)..(w / 2) {
                luma[(y * w + x) as usize] = 255;
            }
        }
        let stats = measure_luma_plane_view(&view(&luma, w, h, false), true).unwrap();
        let avg = pq_code_to_nits(stats.avg_pq);
        let max = pq_code_to_nits(stats.max_pq);
        assert!(avg < 100.0, "avg {avg}");
        assert!(max > 9000.0, "max {max}");
    }

    #[test]
    fn limited_range_black_and_white_hit_the_code_ends() {
        // HDR10 streams are normally limited (TV) range: 64 is the 10-bit PQ
        // black code and 940 is the 10 000-nit code, so measuring them as full
        // range would lift black and clip the highlights before the PQ
        // re-encode.
        let (w, h) = (1920_u32, 1080_u32);
        let mut black = Vec::with_capacity((w * h * 2) as usize);
        for _ in 0..(w * h) as usize {
            black.extend_from_slice(&(64_u16 << 6).to_le_bytes());
        }
        let stats = measure_luma_plane_view(&view(&black, w, h, true), false).unwrap();
        assert!(
            pq_code_to_nits(stats.avg_pq) < 0.5,
            "avg {} -> {} nits",
            stats.avg_pq,
            pq_code_to_nits(stats.avg_pq)
        );

        let mut white = Vec::with_capacity((w * h * 2) as usize);
        for _ in 0..(w * h) as usize {
            white.extend_from_slice(&(940_u16 << 6).to_le_bytes());
        }
        let stats = measure_luma_plane_view(&view(&white, w, h, true), false).unwrap();
        assert!(
            (pq_code_to_nits(stats.max_pq) - 10000.0).abs() < 20.0,
            "limited-range white should reach 10 k nits: {}",
            pq_code_to_nits(stats.max_pq)
        );
    }

    #[test]
    fn limited_range_mid_gray_matches_the_shader_expansion() {
        // 10-bit limited-range 512 expands to (512 - 64) / 876, the same
        // normalization `expand_ycbcr_range` applies in the video shaders.
        let (w, h) = (16_u32, 16_u32);
        let mut luma = Vec::with_capacity((w * h * 2) as usize);
        for _ in 0..(w * h) as usize {
            luma.extend_from_slice(&(512_u16 << 6).to_le_bytes());
        }
        let stats = measure_luma_plane_view(&view(&luma, w, h, true), false).unwrap();
        let expected = (512.0 - 64.0) / 876.0;
        let code = sample_code(expected);
        assert!((stats.avg_pq - code).abs() < 1e-5, "avg {}", stats.avg_pq);
        // The full-range reading is visibly different, so the range matters.
        let full = measure_luma_plane_view(&view(&luma, w, h, true), true).unwrap();
        assert!((full.avg_pq - stats.avg_pq).abs() > 1e-3);
    }

    #[test]
    fn smoother_converges_and_tracks_step() {
        let mut smoother = LumaSmoother::new();
        let dim = FrameLumaStats {
            avg_pq: 0.2,
            max_pq: 0.5,
        };
        // First sample initializes directly.
        let (avg, max) = smoother.push(dim);
        assert_eq!(avg, 0.2);
        assert_eq!(max, 0.5);
        // A sustained new level converges asymptotically (τ=20).
        let bright = FrameLumaStats {
            avg_pq: 0.6,
            max_pq: 0.9,
        };
        let mut last = 0.0_f32;
        for _ in 0..300 {
            let (avg, _) = smoother.push(bright);
            last = avg;
        }
        assert!((last - 0.6).abs() < 0.01, "converged avg {last}");
        // A single bright frame only nudges the estimate a little.
        let mut s2 = LumaSmoother::new();
        s2.push(dim);
        s2.push(bright);
        let (avg, max) = (s2.avg_pq, s2.max_pq);
        assert!(avg < 0.25 && avg > 0.2, "avg {avg}");
        assert!(max < 0.55 && max > 0.5, "max {max}");
    }

    #[test]
    fn reset_clears_state() {
        let mut smoother = LumaSmoother::new();
        smoother.push(FrameLumaStats {
            avg_pq: 0.8,
            max_pq: 0.9,
        });
        smoother.reset();
        assert_eq!(smoother, LumaSmoother::default());
    }
}
