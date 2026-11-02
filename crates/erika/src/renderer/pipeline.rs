use crate::core::{ColorPrimaries, TransferFunction};

pub const VIDEO_INPUT_MODE_MASK: u32 = 0xff;
pub const VIDEO_INPUT_PACKED_ALPHA_RIGHT: u32 = 1 << 8;
/// P010 little-endian bytes in RG8 (Y) and RGBA8 (UV), reconstructed on the GPU.
pub const VIDEO_INPUT_P010_BYTES: u32 = 1 << 9;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrMetadata {
    pub mastering_display: Option<MasteringDisplayMetadata>,
    pub content_light: Option<ContentLightMetadata>,
}

impl HdrMetadata {
    pub fn new(
        mastering_display: Option<MasteringDisplayMetadata>,
        content_light: Option<ContentLightMetadata>,
    ) -> Self {
        Self {
            mastering_display,
            content_light,
        }
    }

    pub fn nominal_peak_nits(self) -> Option<f32> {
        let mastering = self.mastering_display.and_then(|m| m.max_luminance_nits());
        let cll = self
            .content_light
            .and_then(|c| c.max_content_light_level_nits());
        match (mastering, cll) {
            (Some(m), Some(c)) => {
                if c >= 10.0 {
                    Some(m.min(c))
                } else {
                    Some(m)
                }
            }
            (Some(m), None) => Some(m),
            (None, Some(c)) => Some(c),
            (None, None) => None,
        }
    }

    pub fn max_frame_average_light_level_nits(self) -> Option<f32> {
        self.content_light
            .and_then(|metadata| metadata.max_frame_average_light_level_nits())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasteringDisplayMetadata {
    pub display_primaries: Option<[Chromaticity; 3]>,
    pub white_point: Option<Chromaticity>,
    pub min_luminance_nits: Option<f32>,
    pub max_luminance_nits: Option<f32>,
}

impl MasteringDisplayMetadata {
    pub fn max_luminance_nits(self) -> Option<f32> {
        self.max_luminance_nits
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContentLightMetadata {
    pub max_content_light_level_nits: u32,
    pub max_frame_average_light_level_nits: u32,
}

impl ContentLightMetadata {
    pub fn max_content_light_level_nits(self) -> Option<f32> {
        if self.max_content_light_level_nits == 0 {
            None
        } else {
            Some(self.max_content_light_level_nits as f32)
        }
    }

    pub fn max_frame_average_light_level_nits(self) -> Option<f32> {
        if self.max_frame_average_light_level_nits == 0 {
            None
        } else {
            Some(self.max_frame_average_light_level_nits as f32)
        }
    }
}

/// Maximum number of reshaping pieces per component in a Dolby Vision RPU
/// (`AV_DOVI_MAX_PIECES` in FFmpeg, `num_pivots - 1` segments).
pub const DOVI_MAX_PIECES: usize = 8;
/// Maximum number of MMR orders per piece (FFmpeg allows 1..=3).
pub const DOVI_MAX_MMR_ORDER: usize = 3;
/// Number of coefficients per MMR order: 3 linear terms plus the 4 cross
/// products (x·y, x·z, y·z, x·y·z).
pub const DOVI_MMR_COEFFS: usize = 7;

/// One component's reshaping curve, converted from the RPU's fixed-point
/// representation into shader-ready floats. Pivots are normalized to the
/// base-layer signal range [0, 1] and coefficients by `2^-coef_log2_denom`,
/// exactly like libplacebo's `pl_map_dovi_metadata`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DoviComponentCurve {
    /// 0 when this component carries no reshaping, otherwise 2..=9.
    pub num_pivots: u8,
    /// Sorted ascending, normalized to [0, 1]. Only the first `num_pivots`
    /// entries are meaningful.
    pub pivots: [f32; DOVI_MAX_PIECES + 1],
    /// Polynomial coefficients per segment (x^0, x^1, x^2). Segments above
    /// `poly_order` are zero-filled.
    pub poly_coeffs: [[f32; 3]; DOVI_MAX_PIECES],
    /// Per segment: 0 selects the polynomial, 1..=3 selects MMR of that order.
    pub mmr_orders: [u8; DOVI_MAX_PIECES],
    pub mmr_constants: [f32; DOVI_MAX_PIECES],
    pub mmr_coeffs: [[[f32; DOVI_MMR_COEFFS]; DOVI_MAX_MMR_ORDER]; DOVI_MAX_PIECES],
}

impl Default for DoviComponentCurve {
    fn default() -> Self {
        Self {
            num_pivots: 0,
            pivots: [0.0; DOVI_MAX_PIECES + 1],
            poly_coeffs: [[0.0; 3]; DOVI_MAX_PIECES],
            mmr_orders: [0; DOVI_MAX_PIECES],
            mmr_constants: [0.0; DOVI_MAX_PIECES],
            mmr_coeffs: [[[0.0; DOVI_MMR_COEFFS]; DOVI_MAX_MMR_ORDER]; DOVI_MAX_PIECES],
        }
    }
}

/// Per-frame Dolby Vision dynamic metadata level 1: 12-bit PQ codes of the
/// frame's black, peak, and average luminance. The frame peak replaces the
/// static mastering peak (`source_max_pq`) in the tone map, matching
/// libplacebo's handling of the RPU's CIE-Y metadata (`pl_map_dovi_metadata` /
/// `pl_hdr_metadata_from_dovi_rpu`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoviFramePq {
    pub min_pq: u16,
    pub max_pq: u16,
    pub avg_pq: u16,
}

/// Per-frame Dolby Vision RPU payload copied out of the decoder's
/// `AV_FRAME_DATA_DOVI_METADATA` side data before the frame is retired.
///
/// The `nonlinear_matrix` is the RPU's "ycc_to_rgb" transform applied to the
/// reshaped (still PQ-encoded) signal; `rgb_to_lms` is the RPU's mastering
/// transform whose inverse converts PQ-linearized LMS back to BT.2020 RGB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DoviSourceMetadata {
    /// Per-component reshaping curves (luma, Cb, Cr).
    pub reshaping: [DoviComponentCurve; 3],
    /// RPU's ycc_to_rgb matrix applied to reshaped nonlinear signal.
    pub nonlinear_matrix: RgbMatrix,
    /// RPU signal offsets applied before the nonlinear matrix.
    pub nonlinear_offset: [f32; 3],
    /// RPU's rgb_to_lms mastering transform (inverted after PQ linearization).
    pub rgb_to_lms: RgbMatrix,
    /// 12-bit PQ code of the mastering display's black level (typically 0-100).
    pub source_min_pq: u16,
    /// 12-bit PQ code of the mastering display's peak level (typically 2000-4000 nits).
    pub source_max_pq: u16,
    /// Per-frame level 1 brightness metadata (dynamic DM block), or `None`
    /// when the RPU carries no usable L1 block.
    pub l1: Option<DoviFramePq>,
}

/// BT.2100 PQ constants shared by the code <-> nits conversions below.
const PQ_M1: f32 = 0.1593017578125;
const PQ_M2: f32 = 78.84375;
const PQ_C1: f32 = 0.8359375;
const PQ_C2: f32 = 18.8515625;
const PQ_C3: f32 = 18.6875;

/// Decodes a 12-bit PQ code value into absolute nits, matching the PQ EOTF
/// used by the video shaders.
pub fn pq_code_to_nits(code: u16) -> f32 {
    if code == 0 {
        return 0.0;
    }
    let encoded = f32::from(code.min(4095)) / 4095.0;
    nits_from_pq_code(encoded)
}

/// PQ OETF: absolute luminance in nits to a PQ code over the 10 000-nit scale
/// (0.0 = black, 1.0 = 10 000 nits). Inverse of [`nits_from_pq_code`].
pub(crate) fn pq_code_from_nits(nits: f32) -> f32 {
    if !nits.is_finite() || nits <= 0.0 {
        return 0.0;
    }
    let p = (nits / 10_000.0).clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p)).powf(PQ_M2)
}

/// PQ EOTF: a PQ code in [0, 1] over the 10 000-nit scale to absolute nits.
pub(crate) fn nits_from_pq_code(code: f32) -> f32 {
    if !code.is_finite() || code <= 0.0 {
        return 0.0;
    }
    let p = code.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
    let num = (p - PQ_C1).max(0.0);
    let den = (PQ_C2 - PQ_C3 * p).max(0.000001);
    10_000.0 * (num / den).powf(1.0 / PQ_M1)
}

/// Inverse of the no-crosstalk BT.2020-referred HPE RGB->LMS transform that
/// the RPU's `rgb_to_lms` output is fed into (hard-coded by libplacebo as
/// `dovi_lms2rgb`).
const DOVI_HPE_LMS_TO_RGB: RgbMatrix = RgbMatrix::new([
    [3.06441879, -2.16597676, 0.10155818],
    [-0.65612108, 1.78554118, -0.12943749],
    [0.01736321, -0.04725154, 1.03004253],
]);

/// The composite LMS->RGB matrix applied after PQ linearization of a reshaped
/// Dolby Vision signal: the fixed HPE inverse multiplied by the RPU's
/// `rgb_to_lms` matrix, matching libplacebo's `dovi_lms2rgb` composition.
pub fn dovi_lms_to_rgb_matrix(rgb_to_lms: RgbMatrix) -> RgbMatrix {
    DOVI_HPE_LMS_TO_RGB.mul(rgb_to_lms)
}

/// Shader uniform block for Dolby Vision reshaping. All values are
/// vec4-aligned so the block can be appended to the shared video uniform
/// buffer across the WGSL, Metal and HLSL backends without packing tricks.
///
/// **Size**: ~3KB total (144 vec4s for MMR + overhead). Modern GPUs support
/// this easily, but older mobile devices may have uniform buffer limits around
/// 16KB - this uses ~20% of that budget.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "wgpu", derive(bytemuck::Pod, bytemuck::Zeroable))]
pub struct DoviUniforms {
    /// x = 1.0 when the source is RPU-mapped; y/z/w = per-component segment
    /// counts (`num_pivots - 1`, 0 when the component carries no curve).
    pub flags: [f32; 4],
    /// Interior pivots per component (two rows each, segments - 1 values,
    /// padded with a quasi-infinite sentinel like libplacebo).
    pub pivots: [[f32; 4]; 6],
    /// Per-component `[first_pivot, last_pivot]` output clamp.
    pub bounds: [[f32; 4]; 3],
    /// Per-segment payload `[c0, c1, c2, kind]`: kind == 0 is the polynomial
    /// `(c2·s + c1)·s + c0`, kind 1..=3 is MMR of that order with constant
    /// `c0` and packed rows starting at offset `c1`.
    pub coefficients: [[f32; 4]; 3 * DOVI_MAX_PIECES],
    /// Packed MMR rows per component (two vec4 rows per order), addressed by
    /// the segment's `c1` offset.
    pub mmr: [[f32; 4]; 3 * 2 * DOVI_MAX_MMR_ORDER * DOVI_MAX_PIECES],
    /// RPU "ycc_to_rgb" rows applied to the reshaped nonlinear signal.
    pub nonlinear_matrix: [[f32; 4]; 3],
    /// RPU signal offsets, pre-scaled so the shader's normalized 8-bit or
    /// P010 samples subtract exactly (libplacebo folds 2^bits/(2^bits-1)).
    pub nonlinear_offset: [f32; 4],
    /// Composite LMS->RGB rows applied after PQ linearization.
    pub lms_matrix: [[f32; 4]; 3],
}

const DOVI_PIVOT_SENTINEL: f32 = 1e9;
/// RPU offsets are rational /1024-style values while shader samples are
/// normalized by n/(2^bits - 1). `DoviUniforms::of_for_representation`
/// applies the matching
/// 2^bits/(2^bits-1) correction for the actual uploaded representation.
impl DoviUniforms {
    pub const fn disabled() -> Self {
        Self {
            flags: [0.0; 4],
            pivots: [[0.0; 4]; 6],
            bounds: [[0.0; 4]; 3],
            coefficients: [[0.0; 4]; 3 * DOVI_MAX_PIECES],
            mmr: [[0.0; 4]; 3 * 2 * DOVI_MAX_MMR_ORDER * DOVI_MAX_PIECES],
            nonlinear_matrix: [[0.0; 4]; 3],
            nonlinear_offset: [0.0; 4],
            lms_matrix: [[0.0; 4]; 3],
        }
    }

    /// Builds uniforms using the historical 10-bit DV signal representation.
    /// New callers with an explicit uploaded format should use
    /// [`Self::of_for_representation`].
    pub fn of(source: &SourceColorState) -> Self {
        Self::of_for_representation(source, true)
    }

    /// Builds uniforms for the concrete plane representation used by the
    /// renderer (`P010` when `is_p010` is true, otherwise 8-bit `NV12`).
    pub fn of_for_representation(source: &SourceColorState, is_p010: bool) -> Self {
        let Some(dovi) = &source.dovi else {
            return Self::disabled();
        };
        let mut uniforms = Self::disabled();
        uniforms.flags[0] = 1.0;
        for (component, curve) in dovi.reshaping.iter().enumerate() {
            if curve.num_pivots < 2 {
                continue;
            }
            let segments = (curve.num_pivots - 1) as usize;
            uniforms.flags[1 + component] = segments as f32;
            let mut interior = [DOVI_PIVOT_SENTINEL; DOVI_MAX_PIECES];
            interior[..segments - 1].copy_from_slice(&curve.pivots[1..segments]);
            uniforms.pivots[2 * component] = [interior[0], interior[1], interior[2], interior[3]];
            uniforms.pivots[2 * component + 1] =
                [interior[4], interior[5], interior[6], interior[7]];
            uniforms.bounds[component] = [
                curve.pivots[0].min(curve.pivots[segments]),
                curve.pivots[0].max(curve.pivots[segments]),
                0.0,
                0.0,
            ];

            let mut mmr_row = 0usize;
            for (segment, &kind) in curve.mmr_orders[..segments].iter().enumerate() {
                let slot = DOVI_MAX_PIECES * component + segment;
                if kind == 0 {
                    uniforms.coefficients[slot] = [
                        curve.poly_coeffs[segment][0],
                        curve.poly_coeffs[segment][1],
                        curve.poly_coeffs[segment][2],
                        0.0,
                    ];
                    continue;
                }
                let order = (kind as usize).min(DOVI_MAX_MMR_ORDER);
                let orders = &curve.mmr_coeffs[segment][..order];
                for (index, coefficients) in orders.iter().enumerate() {
                    let row =
                        DOVI_MAX_PIECES * 2 * DOVI_MAX_MMR_ORDER * component + mmr_row + 2 * index;
                    uniforms.mmr[row] = [coefficients[0], coefficients[1], coefficients[2], 0.0];
                    uniforms.mmr[row + 1] = [
                        coefficients[3],
                        coefficients[4],
                        coefficients[5],
                        coefficients[6],
                    ];
                }
                uniforms.coefficients[slot] = [
                    curve.mmr_constants[segment],
                    mmr_row as f32,
                    0.0,
                    kind as f32,
                ];
                mmr_row += 2 * order;
            }
        }
        uniforms.nonlinear_matrix = dovi.nonlinear_matrix.row4s();
        let signal_bits = if is_p010 { 10 } else { 8 };
        let signal_max = ((1_u32 << signal_bits) - 1) as f32;
        let signal_offset_scale = (1_u32 << signal_bits) as f32 / signal_max;
        uniforms.nonlinear_offset = [
            dovi.nonlinear_offset[0] * signal_offset_scale,
            dovi.nonlinear_offset[1] * signal_offset_scale,
            dovi.nonlinear_offset[2] * signal_offset_scale,
            0.0,
        ];
        uniforms.lms_matrix = dovi_lms_to_rgb_matrix(dovi.rgb_to_lms).row4s();
        uniforms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRange {
    Unspecified,
    Limited,
    Full,
}

impl Default for ColorRange {
    fn default() -> Self {
        Self::Unspecified
    }
}

impl ColorRange {
    pub fn resolve(self, fallback: Self) -> Self {
        match self {
            Self::Unspecified => fallback,
            _ => self,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixCoefficients {
    Unspecified,
    Identity,
    Bt601,
    Bt709,
    Bt2020NonConstantLuminance,
}

impl Default for MatrixCoefficients {
    fn default() -> Self {
        Self::Unspecified
    }
}

impl MatrixCoefficients {
    pub fn resolve(self, primaries: ColorPrimaries) -> Self {
        if self != Self::Unspecified {
            return self;
        }
        match primaries {
            ColorPrimaries::Bt2020 => Self::Bt2020NonConstantLuminance,
            ColorPrimaries::Bt709 | ColorPrimaries::DisplayP3 => Self::Bt709,
            ColorPrimaries::Unknown => Self::Bt709,
        }
    }

    pub fn luma_coefficients(self, primaries: ColorPrimaries) -> LumaCoefficients {
        match self.resolve(primaries) {
            Self::Bt601 => LumaCoefficients::new(0.2990, 0.5870, 0.1140),
            Self::Bt2020NonConstantLuminance => LumaCoefficients::new(0.2627, 0.6780, 0.0593),
            Self::Identity | Self::Bt709 | Self::Unspecified => {
                LumaCoefficients::new(0.2126, 0.7152, 0.0722)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LumaCoefficients {
    pub kr: f32,
    pub kg: f32,
    pub kb: f32,
}

impl LumaCoefficients {
    pub const fn new(kr: f32, kg: f32, kb: f32) -> Self {
        Self { kr, kg, kb }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticity {
    pub x: f32,
    pub y: f32,
}

impl Chromaticity {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrimariesCoordinates {
    pub red: Chromaticity,
    pub green: Chromaticity,
    pub blue: Chromaticity,
    pub white: Chromaticity,
}

impl PrimariesCoordinates {
    pub const fn new(
        red: Chromaticity,
        green: Chromaticity,
        blue: Chromaticity,
        white: Chromaticity,
    ) -> Self {
        Self {
            red,
            green,
            blue,
            white,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RgbMatrix {
    rows: [[f32; 3]; 3],
}

impl RgbMatrix {
    pub const fn new(rows: [[f32; 3]; 3]) -> Self {
        Self { rows }
    }

    pub const fn identity() -> Self {
        Self::new([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]])
    }

    pub fn rows(self) -> [[f32; 3]; 3] {
        self.rows
    }

    pub fn row4s(self) -> [[f32; 4]; 3] {
        [
            [self.rows[0][0], self.rows[0][1], self.rows[0][2], 0.0],
            [self.rows[1][0], self.rows[1][1], self.rows[1][2], 0.0],
            [self.rows[2][0], self.rows[2][1], self.rows[2][2], 0.0],
        ]
    }

    pub(crate) fn mul(self, rhs: Self) -> Self {
        let mut rows = [[0.0; 3]; 3];
        for (row_index, row) in rows.iter_mut().enumerate() {
            for (col_index, value) in row.iter_mut().enumerate() {
                *value = self.rows[row_index][0] * rhs.rows[0][col_index]
                    + self.rows[row_index][1] * rhs.rows[1][col_index]
                    + self.rows[row_index][2] * rhs.rows[2][col_index];
            }
        }
        Self::new(rows)
    }

    pub(crate) fn mul_vec(self, value: [f32; 3]) -> [f32; 3] {
        [
            self.rows[0][0] * value[0] + self.rows[0][1] * value[1] + self.rows[0][2] * value[2],
            self.rows[1][0] * value[0] + self.rows[1][1] * value[1] + self.rows[1][2] * value[2],
            self.rows[2][0] * value[0] + self.rows[2][1] * value[1] + self.rows[2][2] * value[2],
        ]
    }

    pub(crate) fn inverse(self) -> Self {
        let m = self.rows;
        let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        let inv_det = 1.0 / det;
        Self::new([
            [
                (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det,
                (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det,
                (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det,
            ],
            [
                (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det,
                (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det,
                (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det,
            ],
            [
                (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det,
                (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det,
                (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det,
            ],
        ])
    }
}

pub fn primaries_coordinates(primaries: ColorPrimaries) -> PrimariesCoordinates {
    match resolve_primaries(primaries) {
        ColorPrimaries::Bt2020 => PrimariesCoordinates::new(
            Chromaticity::new(0.708, 0.292),
            Chromaticity::new(0.170, 0.797),
            Chromaticity::new(0.131, 0.046),
            D65_WHITE,
        ),
        ColorPrimaries::DisplayP3 => PrimariesCoordinates::new(
            Chromaticity::new(0.680, 0.320),
            Chromaticity::new(0.265, 0.690),
            Chromaticity::new(0.150, 0.060),
            D65_WHITE,
        ),
        ColorPrimaries::Bt709 | ColorPrimaries::Unknown => PrimariesCoordinates::new(
            Chromaticity::new(0.640, 0.330),
            Chromaticity::new(0.300, 0.600),
            Chromaticity::new(0.150, 0.060),
            D65_WHITE,
        ),
    }
}

pub fn rgb_to_xyz_matrix(primaries: ColorPrimaries) -> RgbMatrix {
    let coords = primaries_coordinates(primaries);
    let red = xy_to_xyz(coords.red);
    let green = xy_to_xyz(coords.green);
    let blue = xy_to_xyz(coords.blue);
    let white = xy_to_xyz(coords.white);
    let unscaled = RgbMatrix::new([
        [red[0], green[0], blue[0]],
        [red[1], green[1], blue[1]],
        [red[2], green[2], blue[2]],
    ]);
    let scale = unscaled.inverse().mul_vec(white);
    RgbMatrix::new([
        [red[0] * scale[0], green[0] * scale[1], blue[0] * scale[2]],
        [red[1] * scale[0], green[1] * scale[1], blue[1] * scale[2]],
        [red[2] * scale[0], green[2] * scale[1], blue[2] * scale[2]],
    ])
}

pub fn xyz_to_rgb_matrix(primaries: ColorPrimaries) -> RgbMatrix {
    rgb_to_xyz_matrix(primaries).inverse()
}

pub fn source_to_target_rgb_matrix(source: ColorPrimaries, target: ColorPrimaries) -> RgbMatrix {
    let source = resolve_primaries(source);
    let target = resolve_primaries(target);
    if source == target {
        return RgbMatrix::identity();
    }
    xyz_to_rgb_matrix(target).mul(rgb_to_xyz_matrix(source))
}

/// RGB (source primaries, absolute D65-referred linear) → HPE-LMS, ported
/// from libplacebo's `pl_ipt_rgb2lms`: a 4% crosstalk mix of HPE XYZ→LMS
/// (D65) applied to the primaries RGB→XYZ matrix. The codebase's primaries
/// are all D65 so the chromatic-adaptation step to D65 is the identity and
/// is omitted. Tone mapping runs in this LMS-PQ-IPT space (see the shader
/// `tone_map_nits`), which converts primaries while mapping the intensity
/// axis instead of running a separate gamut matrix.
pub fn ipt_rgb2lms_matrix(primaries: ColorPrimaries) -> RgbMatrix {
    const HPE: [[f32; 3]; 3] = [
        [0.40024, 0.70760, -0.08081],
        [-0.22630, 1.16532, 0.04570],
        [0.00000, 0.00000, 0.91822],
    ];
    let c = 0.04_f32;
    let crosstalk = RgbMatrix::new([
        [1.0 - 2.0 * c, c, c],
        [c, 1.0 - 2.0 * c, c],
        [c, c, 1.0 - 2.0 * c],
    ]);
    crosstalk
        .mul(RgbMatrix::new(HPE))
        .mul(rgb_to_xyz_matrix(primaries))
}

/// Inverse of [`ipt_rgb2lms_matrix`] for the *target* primaries; this is
/// what converts the tone-mapped LMS signal back to display RGB.
pub fn ipt_lms2rgb_matrix(primaries: ColorPrimaries) -> RgbMatrix {
    ipt_rgb2lms_matrix(primaries).inverse()
}

const D65_WHITE: Chromaticity = Chromaticity::new(0.3127, 0.3290);

fn resolve_primaries(primaries: ColorPrimaries) -> ColorPrimaries {
    match primaries {
        ColorPrimaries::Unknown => ColorPrimaries::Bt709,
        _ => primaries,
    }
}

fn primaries_code(primaries: ColorPrimaries) -> u32 {
    match resolve_primaries(primaries) {
        ColorPrimaries::Bt709 => 0,
        ColorPrimaries::Bt2020 => 1,
        ColorPrimaries::DisplayP3 => 2,
        ColorPrimaries::Unknown => 0,
    }
}

#[allow(dead_code)]
pub(crate) fn code_to_primaries(code: u32) -> ColorPrimaries {
    match code {
        1 => ColorPrimaries::Bt2020,
        2 => ColorPrimaries::DisplayP3,
        _ => ColorPrimaries::Bt709,
    }
}

/// Source and target primaries (resolved) for the perceptual gamut LUT key,
/// packed into the uniforms' reserved word.
pub(crate) fn gamut_primaries_code(source: ColorPrimaries, target: ColorPrimaries) -> u32 {
    (primaries_code(source) << 8) | primaries_code(target)
}

fn xy_to_xyz(value: Chromaticity) -> [f32; 3] {
    [value.x / value.y, 1.0, (1.0 - value.x - value.y) / value.y]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneMapOperator {
    Clip,
    /// Reinhard curve (libplacebo `pl_tone_map_reinhard`, output-relative).
    Reinhard,
    /// Möbius transform with a linear region below the knee (libplacebo
    /// `pl_tone_map_mobius`; `curve_param` is the knee, default 0.3).
    Mobius,
    /// ITU-R BT.2390 EETF with black-point compensation (libplacebo
    /// `pl_tone_map_bt2390`; `curve_param` is the knee offset, default 1.0).
    Bt2390,
    /// Perceptually linear single-pivot polynomial, the default in libplacebo
    /// and mpv's gpu-next renderer (`curve_param` is the slope contrast,
    /// default 0.30; the pivot follows the scene average luminance).
    Spline,
    /// ITU-R BT.2446 method A (log-domain Weber compression), described by
    /// mpv as the recommended curve for well-mastered content.
    Bt2446a,
    /// SMPTE ST 2094-10 Annex B.2, the DolbyVision dynamic-metadata curve;
    /// coefficients are solved per frame from the scene pivot.
    St209410,
}

impl Default for ToneMapOperator {
    fn default() -> Self {
        // Aligns with broadcast standard ITU-R BT.2390 and mpv's default
        // tone-mapping curve: BT.2390 preserves 1:1 luminance for diffuse content
        // below the knee without underexposing midtones, and rolls off highlights smoothly.
        Self::Bt2390
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalerKernel {
    Nearest,
    Bilinear,
    Bicubic,
    Lanczos3,
}

/// Video super-resolution applied before plane sampling.
///
/// Network and spatial modes upscale the decoded Y plane; chroma keeps its
/// source resolution and is reconstructed by the regular scaler. Platform
/// modes hand the whole frame to a system scaler (D3D11 video processor
/// extensions, VideoToolbox, MetalFX) and fall back to native sampling where
/// the platform has no such scaler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum LumaUpscalerMode {
    #[default]
    Off,
    /// ArtCNN C4F16 (~12K parameters), lightweight real-time anime doubler.
    ArtCnnC4F16,
    /// ArtCNN C4F16 DS, denoising and sharpening for degraded anime sources.
    ArtCnnC4F16Ds,
    /// ArtCNN C4F32 (~48K parameters), higher quality real-time doubler.
    ArtCnnC4F32,
    /// ArtCNN C4F16 DN, denoising variant.
    ArtCnnC4F16Dn,
    /// ArtCNN C4F32 DS, denoising and sharpening variant of C4F32.
    ArtCnnC4F32Ds,
    /// ArtCNN C4F32 DN, denoising variant of C4F32.
    ArtCnnC4F32Dn,
    /// ArtCNN R8F64 (~1M parameters). Only practical on matrix-unit backends
    /// and SD/720p sources; bundled on desktop targets only.
    ArtCnnR8F64,
    /// Anime4K v3.2 Upscale CNN x2, sizes S through UL.
    Anime4kS,
    Anime4kM,
    Anime4kL,
    Anime4kVl,
    Anime4kUl,
    /// Anime4K v3.2 Upscale+Denoise CNN x2, sizes S through UL.
    Anime4kDenoiseS,
    Anime4kDenoiseM,
    Anime4kDenoiseL,
    Anime4kDenoiseVl,
    Anime4kDenoiseUl,
    /// AMD FidelityFX Super Resolution 1.0 (EASU + RCAS) on luma; content
    /// agnostic and cheap enough for every GPU.
    Fsr1,
    /// The platform's video super-resolution: NVIDIA RTX Video / Intel VSR
    /// through the D3D11 video processor, VideoToolbox super resolution on
    /// Apple platforms.
    PlatformVideo,
    /// MetalFX spatial scaling (Apple only).
    MetalFxSpatial,
    /// A network loaded at runtime (for example FSRCNNX or ArtCNN R16F96
    /// converted to an Erika model file); see `custom_upscaler_model`.
    Custom,
}

/// Content and execution family of a [`LumaUpscalerMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpscalerFamily {
    Off,
    /// A convolutional network executed by Erika's own inference engine.
    Network,
    /// A non-neural spatial shader.
    Spatial,
    /// A system scaler outside Erika's inference engine.
    Platform,
}

impl LumaUpscalerMode {
    pub const ALL: [Self; 22] = [
        Self::Off,
        Self::ArtCnnC4F16,
        Self::ArtCnnC4F16Ds,
        Self::ArtCnnC4F32,
        Self::ArtCnnC4F16Dn,
        Self::ArtCnnC4F32Ds,
        Self::ArtCnnC4F32Dn,
        Self::ArtCnnR8F64,
        Self::Anime4kS,
        Self::Anime4kM,
        Self::Anime4kL,
        Self::Anime4kVl,
        Self::Anime4kUl,
        Self::Anime4kDenoiseS,
        Self::Anime4kDenoiseM,
        Self::Anime4kDenoiseL,
        Self::Anime4kDenoiseVl,
        Self::Anime4kDenoiseUl,
        Self::Fsr1,
        Self::PlatformVideo,
        Self::MetalFxSpatial,
        Self::Custom,
    ];

    pub fn is_enabled(self) -> bool {
        self != Self::Off
    }

    pub fn family(self) -> UpscalerFamily {
        match self {
            Self::Off => UpscalerFamily::Off,
            Self::Fsr1 => UpscalerFamily::Spatial,
            Self::PlatformVideo | Self::MetalFxSpatial => UpscalerFamily::Platform,
            _ => UpscalerFamily::Network,
        }
    }

    /// Stable identifier used in diagnostics and logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ArtCnnC4F16 => "artcnn_c4f16",
            Self::ArtCnnC4F16Ds => "artcnn_c4f16_ds",
            Self::ArtCnnC4F32 => "artcnn_c4f32",
            Self::ArtCnnC4F16Dn => "artcnn_c4f16_dn",
            Self::ArtCnnC4F32Ds => "artcnn_c4f32_ds",
            Self::ArtCnnC4F32Dn => "artcnn_c4f32_dn",
            Self::ArtCnnR8F64 => "artcnn_r8f64",
            Self::Anime4kS => "anime4k_s",
            Self::Anime4kM => "anime4k_m",
            Self::Anime4kL => "anime4k_l",
            Self::Anime4kVl => "anime4k_vl",
            Self::Anime4kUl => "anime4k_ul",
            Self::Anime4kDenoiseS => "anime4k_denoise_s",
            Self::Anime4kDenoiseM => "anime4k_denoise_m",
            Self::Anime4kDenoiseL => "anime4k_denoise_l",
            Self::Anime4kDenoiseVl => "anime4k_denoise_vl",
            Self::Anime4kDenoiseUl => "anime4k_denoise_ul",
            Self::Fsr1 => "fsr1",
            Self::PlatformVideo => "platform_video",
            Self::MetalFxSpatial => "metalfx_spatial",
            Self::Custom => "custom",
        }
    }

    /// The original three ArtCNN modes have a hand-written matrix kernel on
    /// Apple platforms and the legacy wgpu path.
    pub fn is_legacy_artcnn(self) -> bool {
        matches!(
            self,
            Self::ArtCnnC4F16 | Self::ArtCnnC4F16Ds | Self::ArtCnnC4F32
        )
    }
}

impl Default for ScalerKernel {
    fn default() -> Self {
        Self::Bilinear
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceColorState {
    pub primaries: ColorPrimaries,
    pub transfer: TransferFunction,
    pub matrix: MatrixCoefficients,
    pub range: ColorRange,
    pub hdr_metadata: Option<HdrMetadata>,
    pub dovi: Option<DoviSourceMetadata>,
    /// Measured per-frame scene-average luminance (nits) from CPU frame
    /// statistics (HDR10 without Dolby Vision L1). Precedes the L1 average
    /// when driving the tone-map pivot.
    pub measured_scene_avg_nits: Option<f32>,
    pub nominal_peak_nits: f32,
    pub reference_white_nits: f32,
}

impl SourceColorState {
    pub fn new(primaries: ColorPrimaries, transfer: TransferFunction) -> Self {
        Self {
            primaries,
            transfer,
            matrix: MatrixCoefficients::default(),
            range: ColorRange::default(),
            hdr_metadata: None,
            dovi: None,
            measured_scene_avg_nits: None,
            nominal_peak_nits: nominal_peak_for_transfer(transfer),
            reference_white_nits: reference_white_for_transfer(transfer),
        }
    }

    pub fn range(mut self, range: ColorRange) -> Self {
        self.range = range;
        self
    }

    pub fn matrix(mut self, matrix: MatrixCoefficients) -> Self {
        self.matrix = matrix;
        self
    }

    pub fn nominal_peak_nits(mut self, peak: f32) -> Self {
        self.nominal_peak_nits = peak.max(1.0);
        self
    }

    pub fn hdr_metadata(mut self, metadata: Option<HdrMetadata>) -> Self {
        if let Some(peak) = metadata.and_then(HdrMetadata::nominal_peak_nits) {
            self.nominal_peak_nits = peak.max(1.0);
        }
        self.hdr_metadata = metadata;
        self
    }

    /// Attaches per-frame Dolby Vision RPU metadata. The RPU describes an
    /// IPT/LMS signal referred to BT.2020 with a PQ transfer, so the primaries
    /// and transfer are forced to BT.2020/PQ (matching libplacebo's
    /// `pl_map_avdovi_metadata`). The RPU's `source_min_pq`/`source_max_pq`
    /// replace static mastering luminance when present, while ordinary display
    /// primaries and content-light metadata are retained. When the frame's
    /// dynamic L1 block is present its frame peak replaces `source_max_pq`
    /// for tone mapping (libplacebo uses the RPU's CIE-Y metadata the same
    /// way); the static mastering display peak stays on the L0 value so
    /// output-mode negotiation never reacts to per-frame brightness. Forcing
    /// the transfer also repairs streams whose VUI tags are missing entirely.
    pub fn dovi(mut self, metadata: Option<DoviSourceMetadata>) -> Self {
        if let Some(dovi) = metadata {
            self.primaries = ColorPrimaries::Bt2020;
            self.transfer = TransferFunction::Pq;
            self.reference_white_nits = reference_white_for_transfer(self.transfer).max(1.0);
            let min_luminance = (dovi.source_min_pq != 0)
                .then(|| pq_code_to_nits(dovi.source_min_pq))
                .filter(|value| value.is_finite() && *value >= 0.0);
            let frame_peak = dovi
                .l1
                .filter(|l1| l1.max_pq != 0)
                .map(|l1| pq_code_to_nits(l1.max_pq));
            let mastering_peak = pq_code_to_nits(dovi.source_max_pq);
            let peak = frame_peak.unwrap_or(mastering_peak);
            if peak > 0.0 {
                self.nominal_peak_nits = peak.max(1.0);
            } else if self.nominal_peak_nits <= self.reference_white_nits {
                self.nominal_peak_nits = nominal_peak_for_transfer(self.transfer);
            }
            // Keep ordinary mastering primaries/content-light metadata, but
            // prefer the RPU's source luminance bounds when present. This lets
            // native HDR10 outputs carry Dolby Vision black-level metadata too.
            // The mastering peak stays on the static L0 value (never the
            // per-frame L1 peak), so output-mode negotiation does not react
            // to frame-by-frame brightness.
            if min_luminance.is_some()
                || (peak.is_finite() && peak > 0.0)
                || self.hdr_metadata.is_some()
            {
                let mut hdr = self
                    .hdr_metadata
                    .unwrap_or_else(|| HdrMetadata::new(None, None));
                let mut mastering = hdr.mastering_display.unwrap_or(MasteringDisplayMetadata {
                    display_primaries: None,
                    white_point: None,
                    min_luminance_nits: None,
                    max_luminance_nits: None,
                });
                if let Some(min_luminance) = min_luminance {
                    mastering.min_luminance_nits = Some(min_luminance);
                }
                if mastering_peak.is_finite() && mastering_peak > 0.0 {
                    mastering.max_luminance_nits = Some(mastering_peak);
                }
                hdr.mastering_display = Some(mastering);
                self.hdr_metadata = Some(hdr);
            }
            self.dovi = Some(dovi);
        } else {
            self.dovi = None;
        }
        self
    }

    /// Attach a measured scene-average luminance from frame statistics. Only
    /// meaningful for HDR10 (PQ) sources without Dolby Vision L1 metadata;
    /// the value drives the tone-map pivot exactly like L1's avg would.
    pub fn measured_scene_avg_nits(mut self, scene_avg_nits: Option<f32>) -> Self {
        if let Some(value) = scene_avg_nits {
            if value.is_finite() && value > 0.0 {
                self.measured_scene_avg_nits = Some(value);
                return self;
            }
        }
        self.measured_scene_avg_nits = None;
        self
    }

    pub fn reference_white_nits(mut self, white: f32) -> Self {
        self.reference_white_nits = white.max(1.0);
        self
    }

    pub fn is_hdr(&self) -> bool {
        matches!(self.transfer, TransferFunction::Pq | TransferFunction::Hlg)
            || self.nominal_peak_nits > self.reference_white_nits * 1.5
    }
}

impl Default for SourceColorState {
    fn default() -> Self {
        Self::new(ColorPrimaries::Unknown, TransferFunction::Unknown)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TargetColorState {
    pub primaries: ColorPrimaries,
    pub transfer: TransferFunction,
    pub peak_nits: f32,
    pub reference_white_nits: f32,
    pub edr_headroom: f32,
}

impl TargetColorState {
    pub fn sdr(primaries: ColorPrimaries) -> Self {
        Self {
            primaries,
            transfer: TransferFunction::Srgb,
            peak_nits: 100.0,
            reference_white_nits: 100.0,
            edr_headroom: 1.0,
        }
    }

    /// SDR target for sources that require tone mapping (HDR → SDR). The tone
    /// map peak and the encode reference white follow the HDR reference white
    /// convention (BT.2408: 203 nits), matching libplacebo/mpv — NOT the
    /// 100-nit SDR mastering value, which pushes the whole picture into the
    /// top of the display range (measured: same frame, our render p50=89 vs
    /// mpv 67 with identical source content).
    pub fn sdr_tone_map_target(primaries: ColorPrimaries) -> Self {
        Self {
            primaries,
            transfer: TransferFunction::Srgb,
            peak_nits: 203.0,
            reference_white_nits: 203.0,
            edr_headroom: 1.0,
        }
    }

    pub fn apple_edr(primaries: ColorPrimaries, headroom: f32) -> Self {
        Self::extended_linear(primaries, 203.0, headroom)
    }

    pub fn extended_linear(
        primaries: ColorPrimaries,
        reference_white_nits: f32,
        headroom: f32,
    ) -> Self {
        let reference_white_nits = reference_white_nits.max(1.0);
        let headroom = headroom.max(1.0);
        Self {
            primaries,
            transfer: TransferFunction::Srgb,
            peak_nits: reference_white_nits * headroom,
            reference_white_nits,
            edr_headroom: headroom,
        }
    }

    pub fn hdr10(primaries: ColorPrimaries) -> Self {
        Self {
            primaries,
            transfer: TransferFunction::Pq,
            peak_nits: 10_000.0,
            reference_white_nits: 203.0,
            edr_headroom: 1.0,
        }
    }
}

impl Default for TargetColorState {
    fn default() -> Self {
        Self::sdr(ColorPrimaries::Bt709)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneMapConfig {
    pub operator: ToneMapOperator,
    /// Per-operator curve parameter that mirrors libplacebo's
    /// `pl_tone_map_constants` field the operator uses: spline contrast,
    /// ST 2094-10 knee adaptation, BT.2390 knee offset, Mobius knee, Reinhard
    /// contrast. A value of 0 selects the operator's default (see
    /// `default_curve_param`). The shaders read it from `tone_map_extra.x`;
    /// ST 2094-10 instead folds it into the coefficients solved on the CPU.
    pub curve_param: f32,
    /// Target display contrast used for black-point compensation
    /// (`--target-contrast` in mpv). 0 selects the automatic value: 1000:1 for
    /// SDR targets, infinite (0 black) for HDR/EDR targets.
    pub contrast_ratio: f32,
}

impl Default for ToneMapConfig {
    fn default() -> Self {
        Self {
            // Follow the operator enum's default so a default-operator change
            // actually reaches the pipeline (a hardcoded value here silently
            // overrides it).
            operator: ToneMapOperator::default(),
            curve_param: 0.0,
            contrast_ratio: 0.0,
        }
    }
}

impl ToneMapConfig {
    /// The effective per-operator curve parameter, substituting the operator
    /// default for 0. Mirrors libplacebo's per-function `param_def` where
    /// mpv overrides it (spline contrast 0.30, mobius knee 0.30).
    pub fn effective_curve_param(self) -> f32 {
        if self.curve_param > 0.0 {
            self.curve_param
        } else {
            default_curve_param(self.operator)
        }
    }
}

fn default_curve_param(operator: ToneMapOperator) -> f32 {
    match operator {
        ToneMapOperator::Clip => 1.0,
        ToneMapOperator::Reinhard => 0.5,
        ToneMapOperator::Mobius => 0.3,
        ToneMapOperator::Bt2390 => 1.0,
        ToneMapOperator::Spline => 0.30,
        ToneMapOperator::Bt2446a => 0.0,
        // libplacebo's `pl_tone_map_st2094_10.param_def` (knee adaptation).
        // mpv's manual documents 1.0 for the same knob; follow libplacebo.
        ToneMapOperator::St209410 => 0.7,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScalerConfig {
    pub kernel: ScalerKernel,
    pub radius: f32,
}

impl Default for ScalerConfig {
    fn default() -> Self {
        Self {
            kernel: ScalerKernel::Bilinear,
            radius: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderPassKind {
    ImportFrame,
    NeuralUpscale,
    PlaneSampling,
    ChromaReconstruction,
    DoviReshape,
    TransferDecode,
    GamutMap,
    ToneMap,
    Scale,
    OverlayComposite,
    Dither,
    OutputTransform,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderPass {
    pub kind: RenderPassKind,
    pub label: &'static str,
}

impl RenderPass {
    pub const fn new(kind: RenderPassKind, label: &'static str) -> Self {
        Self { kind, label }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenderGraph {
    passes: Vec<RenderPass>,
}

impl RenderGraph {
    pub fn new() -> Self {
        Self { passes: Vec::new() }
    }

    pub fn push(&mut self, pass: RenderPass) {
        self.passes.push(pass);
    }

    pub fn passes(&self) -> &[RenderPass] {
        &self.passes
    }

    pub fn contains(&self, kind: RenderPassKind) -> bool {
        self.passes.iter().any(|pass| pass.kind == kind)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoRenderPipeline {
    pub source: SourceColorState,
    pub target: TargetColorState,
    pub tone_map: ToneMapConfig,
    pub scaler: ScalerConfig,
    pub luma_upscaler: LumaUpscalerMode,
    pub graph: RenderGraph,
}

impl VideoRenderPipeline {
    pub fn new(source: SourceColorState, target: TargetColorState) -> Self {
        let tone_map = ToneMapConfig::default();
        let scaler = ScalerConfig::default();
        let luma_upscaler = LumaUpscalerMode::default();
        let graph = build_graph(source, target, scaler, luma_upscaler);
        Self {
            source,
            target,
            tone_map,
            scaler,
            luma_upscaler,
            graph,
        }
    }

    pub fn sdr_default() -> Self {
        Self::new(SourceColorState::default(), TargetColorState::default())
    }

    pub fn with_target(mut self, target: TargetColorState) -> Self {
        self.target = target;
        self.graph = build_graph(self.source, self.target, self.scaler, self.luma_upscaler);
        self
    }

    pub fn with_luma_upscaler(mut self, mode: LumaUpscalerMode) -> Self {
        self.luma_upscaler = mode;
        self.graph = build_graph(self.source, self.target, self.scaler, self.luma_upscaler);
        self
    }

    pub fn requires_tone_mapping(&self) -> bool {
        requires_tone_mapping(self.source, self.target)
    }

    pub fn luma_coefficients(&self) -> LumaCoefficients {
        self.source.matrix.luma_coefficients(self.source.primaries)
    }

    pub fn requires_gamut_mapping(&self) -> bool {
        requires_gamut_mapping(self.source, self.target)
    }

    pub fn gamut_matrix(&self) -> RgbMatrix {
        source_to_target_rgb_matrix(self.source.primaries, self.target.primaries)
    }

    /// Rows 0-2: source-primaries RGB→LMS; rows 3-5: source-primaries
    /// LMS→RGB; rows 6-8: target-primaries LMS→RGB. Tone mapping and
    /// gamut mapping run in a unified single IPT pass (rows 0-2 in, optional
    /// 3D LUT in IPT, rows 6-8 out to target primaries) — mirroring libplacebo.
    pub fn ipt_matrix_rows(&self) -> [[f32; 4]; 9] {
        let mut rows = [[0.0; 4]; 9];
        let source = ipt_rgb2lms_matrix(self.source.primaries);
        for (index, row) in source
            .rows()
            .iter()
            .chain(source.inverse().rows().iter())
            .chain(ipt_lms2rgb_matrix(self.target.primaries).rows().iter())
            .enumerate()
        {
            rows[index] = [row[0], row[1], row[2], 0.0];
        }
        rows
    }

    /// [curve parameter, scene average nits, target black nits, 0] for the
    /// shaders' `tone_map_extra` uniform.
    pub fn tone_map_extra(&self) -> [f32; 4] {
        [
            self.tone_map.effective_curve_param(),
            source_scene_avg_nits(&self.source),
            // Black-point compensation only applies when the tone map is
            // actually active; applying it to SDR->SDR would crush near-black
            // content (mpv skips BPC when `pl_tone_map_params_noop`).
            if requires_tone_mapping(self.source, self.target) {
                target_black_nits(self.target, self.tone_map.contrast_ratio)
            } else {
                0.0
            },
            0.0,
        ]
    }

    /// SMPTE ST 2094-10 coefficients for the shaders' `tone_map_coeffs`
    /// uniform; zeros when the operator is inactive.
    /// Packed (source << 8 | target) resolved-primaries codes used to key
    /// the perceptual gamut LUT cache and stored in `_gamut_reserved`.
    pub fn gamut_primaries_code(&self) -> u32 {
        gamut_primaries_code(self.source.primaries, self.target.primaries)
    }

    pub fn tone_map_coeffs(&self) -> [f32; 4] {
        st2094_10_coefficients_for(self)
    }

    /// Whether the perceptual gamut-mapping LUT is needed: HDR sources
    /// tone-mapped to a smaller gamut get the LUT; SDR passthrough and
    /// same-gamut rendering keep the fast path (gamut_compress only).
    pub fn gamut_lut_active(&self) -> bool {
        requires_tone_mapping(self.source, self.target)
            && resolve_primaries(self.source.primaries) != resolve_primaries(self.target.primaries)
    }

    /// Operator code for the shaders' `tone_map` uniform.
    ///
    /// Zero (Clip, an identity curve) whenever this pipeline does not tone
    /// map, so a Reinhard/Mobius selection cannot alter SDR->SDR content.
    /// The configured operator would otherwise run unconditionally: the
    /// shaders only branch on the code, and `tone_map_extra`'s black point
    /// is already gated the same way.
    pub fn tone_map_uniform_code(&self) -> u32 {
        if requires_tone_mapping(self.source, self.target) {
            tone_map_code(self.tone_map.operator)
        } else {
            tone_map_code(ToneMapOperator::Clip)
        }
    }
}

impl Default for VideoRenderPipeline {
    fn default() -> Self {
        Self::sdr_default()
    }
}

/// Fragment-shader uniforms shared by the video sampling shaders.
///
/// Backends may wrap this with presentation-only fields (for example Metal's
/// target rect), but the color/HDR/gamut payload is generated here so platform
/// renderers do not each invent their own color contract.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "wgpu", derive(bytemuck::Pod, bytemuck::Zeroable))]
pub struct VideoUniforms {
    pub is_p010: u32,
    pub full_range: u32,
    pub source_transfer: u32,
    pub target_transfer: u32,
    pub tone_map: u32,
    pub edr_output: u32,
    /// 0 samples Y + interleaved CbCr planes; 1 samples an already converted
    /// nonlinear RGB texture; 2 samples ArtCNN's packed 2x luma output plus a
    /// normal CbCr plane; 3 applies that packed luma as detail to an already
    /// converted nonlinear RGB texture. Packed RGBA texels store the top-left,
    /// top-right, bottom-left and bottom-right luma subpixels. All modes retain
    /// the common transfer/gamut/tone-map handling.
    pub input_mode: u32,
    /// Leaves the shader output in target-reference-linear space so a backend
    /// can composite overlays before applying the output transfer function.
    pub scene_linear: u32,
    pub nits: [f32; 4],
    pub luma_coefficients: [f32; 4],
    pub gamut_matrix_rows: [[f32; 4]; 3],
    /// Rows 0-2: source-primaries RGB → HPE-LMS; rows 3-5: target-primaries
    /// LMS → RGB (see [`ipt_rgb2lms_matrix`]/[`ipt_lms2rgb_matrix`]). The
    /// shader runs tone mapping in LMS-PQ-IPT space so primaries convert as
    /// part of the map instead of a separate gamut matrix.
    pub ipt_matrix_rows: [[f32; 4]; 9],
    /// x: per-operator curve parameter (see `ToneMapConfig::curve_param`;
    /// unused by ST 2094-10, which solves its curve on the CPU);
    /// y: per-frame scene average luminance in nits (Dolby Vision L1 avg,
    /// 0 = unknown); z: target black point in nits
    /// (`ToneMapConfig::contrast_ratio`); w: reserved.
    pub tone_map_extra: [f32; 4],
    /// SMPTE ST 2094-10 tone-map coefficients (c1, c2, c3) solved per frame
    /// on the CPU; zero unless the ST2094-10 operator is active.
    pub tone_map_coeffs: [f32; 4],
    /// 1 when the perceptual gamut-mapping 3D LUT is bound and the shader
    /// must sample it after tone mapping; 0 keeps the fast gamut_compress.
    pub gamut_lut_enabled: u32,
    /// Packed (source << 8 | target) resolved primaries for the LUT cache.
    pub _gamut_primaries: u32,
    /// Reserved for future per-LUT scaling; keeps the structure padded.
    pub _gamut_reserved0: u32,
    pub _gamut_reserved1: u32,
    /// Dolby Vision reshaping payload; inert unless `flags[0]` is set.
    pub dovi: DoviUniforms,
}

impl VideoUniforms {
    pub fn from_pipeline(pipeline: &VideoRenderPipeline, is_p010: bool, edr_output: bool) -> Self {
        let luma = pipeline.luma_coefficients();
        Self {
            is_p010: u32::from(is_p010),
            full_range: u32::from(matches!(pipeline.source.range, ColorRange::Full)),
            source_transfer: transfer_code(pipeline.source.transfer),
            target_transfer: transfer_code(pipeline.target.transfer),
            tone_map: pipeline.tone_map_uniform_code(),
            edr_output: u32::from(edr_output),
            input_mode: 0,
            scene_linear: 0,
            nits: [
                pipeline.source.nominal_peak_nits,
                pipeline.target.peak_nits,
                pipeline.source.reference_white_nits,
                pipeline.target.reference_white_nits,
            ],
            luma_coefficients: [luma.kr, luma.kg, luma.kb, 0.0],
            gamut_matrix_rows: pipeline.gamut_matrix().row4s(),
            ipt_matrix_rows: pipeline.ipt_matrix_rows(),
            tone_map_extra: pipeline.tone_map_extra(),
            tone_map_coeffs: pipeline.tone_map_coeffs(),
            gamut_lut_enabled: u32::from(pipeline.gamut_lut_active()),
            _gamut_primaries: gamut_primaries_code(
                pipeline.source.primaries,
                pipeline.target.primaries,
            ),
            _gamut_reserved0: 0,
            _gamut_reserved1: 0,
            dovi: DoviUniforms::of_for_representation(&pipeline.source, is_p010),
        }
    }
}

/// Per-frame average luminance from the Dolby Vision L1 block in nits; 0 when
/// absent (libplacebo then falls back to the default knee fraction).
fn dovi_frame_average_nits(dovi: &DoviSourceMetadata) -> Option<f32> {
    let l1 = dovi.l1?;
    if l1.avg_pq == 0 {
        return None;
    }
    let nits = pq_code_to_nits(l1.avg_pq);
    (nits.is_finite() && nits > 0.0).then_some(nits)
}

/// The scene-average luminance (nits) driving the tone-map pivot: the
/// presenter's measured average first, then the Dolby Vision L1 average,
/// then static MaxFALL, 0 when nothing is known.
fn source_scene_avg_nits(source: &SourceColorState) -> f32 {
    source
        .measured_scene_avg_nits
        .or_else(|| source.dovi.as_ref().and_then(dovi_frame_average_nits))
        .or_else(|| {
            source
                .hdr_metadata
                .and_then(|hdr| hdr.max_frame_average_light_level_nits())
        })
        .unwrap_or(0.0)
}

/// Target black point for black-point compensation: mpv's `--target-contrast`
/// auto value is 1000:1 for SDR targets and infinite (0 black) for HDR/EDR
/// targets, which the encode stage maps back onto code 0.
fn target_black_nits(target: TargetColorState, contrast_ratio: f32) -> f32 {
    if target.edr_headroom > 1.0 || target.transfer == TransferFunction::Pq {
        return 0.0;
    }
    let ratio = if contrast_ratio > 0.0 {
        contrast_ratio
    } else {
        1000.0
    };
    target.peak_nits / ratio
}

/// Per-frame ST 2094-10 coefficients for the current source/target
/// luminance envelope, following libplacebo's `pl_tone_map_st2094_10`: the
/// rational Möbius curve passes through (input min, output min), the scene
/// knee and (input max, output max), all in absolute nits. The knee itself is
/// picked in the PQ domain (libplacebo rescales internally even though
/// ST 2094-10 is a NITS-scaled function), and `ToneMapConfig::curve_param`
/// tunes libplacebo's `knee_adaptation` for this operator.
fn st2094_10_coefficients_for(pipeline: &VideoRenderPipeline) -> [f32; 4] {
    if pipeline.tone_map.operator != ToneMapOperator::St209410 {
        return [0.0; 4];
    }
    let input_avg = source_scene_avg_nits(&pipeline.source);
    let output_min = target_black_nits(pipeline.target, pipeline.tone_map.contrast_ratio);
    let (src_knee, dst_knee) = st2094_pick_knee_nits(
        0.0,
        pipeline.source.nominal_peak_nits,
        input_avg,
        output_min,
        pipeline.target.peak_nits,
        pipeline.tone_map.effective_curve_param(),
    );
    solve_st2094_10(
        0.0,
        src_knee,
        pipeline.source.nominal_peak_nits,
        output_min,
        dst_knee,
        pipeline.target.peak_nits,
    )
}

/// The exact libplacebo `st2094_pick_knee` in the **PQ-code domain** (0.0 =
/// black, 1.0 = 10 000 nits). libplacebo always evaluates this in PQ, whatever
/// the tone-map function's own scaling is; callers holding absolute nits must
/// use [`st2094_pick_knee_nits`]. Constants: knee_adaptation 0.4 (the
/// `pl_tone_map_constants` default), knee_min 0.1, knee_max 0.8, knee_default
/// 0.4. Returns the source pivot and the adapted destination pivot.
pub fn st2094_pick_knee(
    input_min: f32,
    input_max: f32,
    input_avg: f32,
    output_min: f32,
    output_max: f32,
) -> (f32, f32) {
    st2094_pick_knee_impl(
        input_min,
        input_max,
        input_avg,
        output_min,
        output_max,
        KNEE_ADAPTATION_DEFAULT,
    )
}

/// [`st2094_pick_knee`] for callers holding absolute nits: the knee is picked
/// in the PQ domain and rescaled back, mirroring libplacebo's
/// `pl_hdr_rescale(input_scaling, PL_HDR_PQ, ..)` round trip inside
/// `st2094_pick_knee`. `knee_adaptation` is libplacebo's
/// `pl_tone_map_constants.knee_adaptation`, which for ST 2094-10 is what
/// `--tone-mapping-param` tunes.
pub fn st2094_pick_knee_nits(
    input_min: f32,
    input_max: f32,
    input_avg: f32,
    output_min: f32,
    output_max: f32,
    knee_adaptation: f32,
) -> (f32, f32) {
    let (src_knee, dst_knee) = st2094_pick_knee_impl(
        pq_code_from_nits(input_min),
        pq_code_from_nits(input_max),
        pq_code_from_nits(input_avg),
        pq_code_from_nits(output_min),
        pq_code_from_nits(output_max),
        knee_adaptation,
    );
    (nits_from_pq_code(src_knee), nits_from_pq_code(dst_knee))
}

const KNEE_ADAPTATION_DEFAULT: f32 = 0.4;
const KNEE_MIN: f32 = 0.1;
const KNEE_MAX: f32 = 0.8;
const KNEE_DEFAULT: f32 = 0.4;

fn st2094_pick_knee_impl(
    input_min: f32,
    input_max: f32,
    input_avg: f32,
    output_min: f32,
    output_max: f32,
    knee_adaptation: f32,
) -> (f32, f32) {
    let mix = |a: f32, b: f32, x: f32| x * b + (1.0 - x) * a;
    let src_knee_min = mix(input_min, input_max, KNEE_MIN);
    let src_knee_max = mix(input_min, input_max, KNEE_MAX);
    let dst_knee_min = mix(output_min, output_max, KNEE_MIN);
    let dst_knee_max = mix(output_min, output_max, KNEE_MAX);
    let fallback = mix(input_min, input_max, KNEE_DEFAULT);
    let src_knee =
        if input_avg > 0.0 { input_avg } else { fallback }.clamp(src_knee_min, src_knee_max);
    let target = (src_knee - input_min) / (input_max - input_min).max(1e-6);
    let adapted = mix(output_min, output_max, target);
    let smooth = |edge0: f32, edge1: f32, x: f32| {
        let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let tuning =
        1.0 - smooth(KNEE_MAX, KNEE_DEFAULT, target) * smooth(KNEE_MIN, KNEE_DEFAULT, target);
    let adaptation = mix(knee_adaptation.clamp(0.0, 1.0), 1.0, tuning);
    let dst_knee = mix(src_knee, adapted, adaptation).clamp(dst_knee_min, dst_knee_max);
    (src_knee, dst_knee)
}

/// Solve the ST 2094-10 rational curve y = (c1 + c2 x) / (1 + c3 x) through
/// (x1,y1), (x2,y2) and (x3,y3) with Cramer's rule on the linear system
/// `c1 + xi*c2 - yi*xi*c3 = yi`.
///
/// A degenerate anchor set (duplicated/collinear points) has no unique
/// solution; libplacebo divides by the zero determinant and produces
/// non-finite coefficients, which the shader would render as a black or NaN
/// frame. Fall back to the identity curve (c1 = 0, c2 = 1, c3 = 0 — the `Clip`
/// operator) so a malformed envelope degrades to no tone mapping instead.
fn solve_st2094_10(x1: f32, x2: f32, x3: f32, y1: f32, y2: f32, y3: f32) -> [f32; 4] {
    const IDENTITY: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
    let base = [
        [1.0, x1, -y1 * x1],
        [1.0, x2, -y2 * x2],
        [1.0, x3, -y3 * x3],
    ];
    let det = |a: [[f32; 3]; 3]| {
        a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
            - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
            + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
    };
    let with_column = |column: usize, replacement: [f32; 3]| {
        let mut m = base;
        for row in 0..3 {
            m[row][column] = replacement[row];
        }
        m
    };
    let den = det(base);
    if !den.is_finite() || den.abs() < 1e-12 {
        return IDENTITY;
    }
    let rhs = [y1, y2, y3];
    let coefficients = [
        det(with_column(0, rhs)) / den,
        det(with_column(1, rhs)) / den,
        det(with_column(2, rhs)) / den,
        0.0,
    ];
    if coefficients[..3].iter().all(|value| value.is_finite()) {
        coefficients
    } else {
        IDENTITY
    }
}

impl VideoUniforms {
    pub fn rgb_texture_input(mut self) -> Self {
        self.input_mode = (self.input_mode & !VIDEO_INPUT_MODE_MASK) | 1;
        self
    }

    /// Updates the decoded sample representation while keeping Dolby Vision
    /// signal offsets in the same normalized domain as the texture samples.
    /// This matters when a 10-bit P010 frame is down-converted to 8-bit NV12.
    pub fn with_p010_representation(mut self, is_p010: bool) -> Self {
        let old_bits = if self.is_p010 != 0 { 10 } else { 8 };
        let new_bits = if is_p010 { 10 } else { 8 };
        if old_bits != new_bits {
            let old_scale = (1_u32 << old_bits) as f32 / ((1_u32 << old_bits) - 1) as f32;
            let new_scale = (1_u32 << new_bits) as f32 / ((1_u32 << new_bits) - 1) as f32;
            let ratio = new_scale / old_scale;
            for offset in &mut self.dovi.nonlinear_offset[..3] {
                *offset *= ratio;
            }
        }
        self.is_p010 = u32::from(is_p010);
        self
    }

    pub fn packed_d2s_luma_input(mut self) -> Self {
        self.input_mode = (self.input_mode & !VIDEO_INPUT_MODE_MASK) | 2;
        self
    }

    pub fn packed_d2s_rgb_detail_input(mut self) -> Self {
        self.input_mode = (self.input_mode & !VIDEO_INPUT_MODE_MASK) | 3;
        self
    }

    pub fn packed_alpha_right(mut self, enabled: bool) -> Self {
        if enabled {
            self.input_mode |= VIDEO_INPUT_PACKED_ALPHA_RIGHT;
        } else {
            self.input_mode &= !VIDEO_INPUT_PACKED_ALPHA_RIGHT;
        }
        self
    }

    pub fn has_packed_alpha_right(self) -> bool {
        self.input_mode & VIDEO_INPUT_PACKED_ALPHA_RIGHT != 0
    }

    pub fn p010_byte_planes(mut self, enabled: bool) -> Self {
        if enabled {
            self.input_mode |= VIDEO_INPUT_P010_BYTES;
        } else {
            self.input_mode &= !VIDEO_INPUT_P010_BYTES;
        }
        self
    }

    pub fn has_p010_byte_planes(self) -> bool {
        self.input_mode & VIDEO_INPUT_P010_BYTES != 0
    }

    pub fn scene_linear_output(mut self) -> Self {
        self.scene_linear = 1;
        self
    }
}

fn transfer_code(transfer: TransferFunction) -> u32 {
    match transfer {
        TransferFunction::Srgb => 1,
        TransferFunction::Bt1886 => 2,
        TransferFunction::Pq => 3,
        TransferFunction::Hlg => 4,
        TransferFunction::Unknown => 1,
    }
}

fn tone_map_code(operator: ToneMapOperator) -> u32 {
    match operator {
        ToneMapOperator::Clip => 0,
        ToneMapOperator::Reinhard => 1,
        ToneMapOperator::Mobius => 2,
        ToneMapOperator::Bt2390 => 3,
        ToneMapOperator::Spline => 4,
        ToneMapOperator::Bt2446a => 5,
        ToneMapOperator::St209410 => 6,
    }
}

fn build_graph(
    source: SourceColorState,
    target: TargetColorState,
    scaler: ScalerConfig,
    luma_upscaler: LumaUpscalerMode,
) -> RenderGraph {
    let mut graph = RenderGraph::new();
    graph.push(RenderPass::new(RenderPassKind::ImportFrame, "import frame"));
    if luma_upscaler.is_enabled() {
        graph.push(RenderPass::new(
            RenderPassKind::NeuralUpscale,
            "neural luma upscale",
        ));
    }
    graph.push(RenderPass::new(
        RenderPassKind::PlaneSampling,
        "sample YCbCr planes",
    ));
    graph.push(RenderPass::new(
        RenderPassKind::ChromaReconstruction,
        "reconstruct chroma",
    ));
    if source.dovi.is_some() {
        graph.push(RenderPass::new(
            RenderPassKind::DoviReshape,
            "reshape dolby vision signal",
        ));
    }
    graph.push(RenderPass::new(
        RenderPassKind::TransferDecode,
        "decode transfer function",
    ));
    if requires_gamut_mapping(source, target) {
        graph.push(RenderPass::new(RenderPassKind::GamutMap, "map gamut"));
    }
    if requires_tone_mapping(source, target) {
        graph.push(RenderPass::new(RenderPassKind::ToneMap, "tone map"));
    }
    if scaler.kernel != ScalerKernel::Nearest {
        graph.push(RenderPass::new(RenderPassKind::Scale, "scale"));
    }
    graph.push(RenderPass::new(
        RenderPassKind::OverlayComposite,
        "composite overlays",
    ));
    graph.push(RenderPass::new(RenderPassKind::Dither, "dither"));
    graph.push(RenderPass::new(
        RenderPassKind::OutputTransform,
        "output transform",
    ));
    graph
}

/// Peak luminance that is stable frame to frame: the mastering display (L0)
/// peak when the container/RPU provides one, otherwise the source's nominal
/// peak. Pass selection and output-mode decisions must use this instead of
/// `nominal_peak_nits`, which Dolby Vision replaces with the per-frame L1 peak
/// — a dark scene must not silently disable the tone map, the black-point
/// compensation, or the perceptual gamut LUT for one frame.
fn static_source_peak_nits(source: &SourceColorState) -> f32 {
    source
        .hdr_metadata
        .and_then(|hdr| hdr.mastering_display)
        .and_then(|mastering| mastering.max_luminance_nits)
        .filter(|peak| peak.is_finite() && *peak > 0.0)
        .unwrap_or(source.nominal_peak_nits)
}

fn requires_tone_mapping(source: SourceColorState, target: TargetColorState) -> bool {
    if !source.is_hdr() {
        return false;
    }
    static_source_peak_nits(&source) > target.peak_nits * 1.05
}

fn requires_gamut_mapping(source: SourceColorState, target: TargetColorState) -> bool {
    resolve_primaries(source.primaries) != resolve_primaries(target.primaries)
}

fn nominal_peak_for_transfer(transfer: TransferFunction) -> f32 {
    match transfer {
        TransferFunction::Pq => 1000.0,
        TransferFunction::Hlg => 1000.0,
        TransferFunction::Srgb | TransferFunction::Bt1886 => 100.0,
        TransferFunction::Unknown => 100.0,
    }
}

fn reference_white_for_transfer(transfer: TransferFunction) -> f32 {
    match transfer {
        TransferFunction::Pq | TransferFunction::Hlg => 203.0,
        TransferFunction::Srgb | TransferFunction::Bt1886 | TransferFunction::Unknown => 100.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference implementation of the BT.2100 HLG inverse OETF used by the
    /// video shaders' `transfer_to_source_reference_linear` (code 4). Nonlinear
    /// signal E' maps to scene linear light in [0, 1]. Constants are spelled
    /// exactly as in BT.2100 (and the shaders), hence the precision allow.
    #[allow(clippy::excessive_precision)]
    fn hlg_inverse_oetf(encoded: f32) -> f32 {
        let a = 0.17883277_f32;
        let b = 0.28466892_f32;
        let c = 0.55991073_f32;
        let e = encoded.max(0.0);
        if e <= 0.5 {
            e * e / 3.0
        } else {
            (((e - c) / a).exp() + b) / 12.0
        }
    }

    /// Reference implementation of the shaders' full HLG decode: inverse OETF
    /// to scene light, then the BT.2100 OOTF (system gamma 1.2 at the 1000 nit
    /// nominal peak), normalized to source reference white exactly like the PQ
    /// branch of the same shader function.
    fn hlg_encoded_to_source_reference_linear(
        encoded: [f32; 3],
        luma: LumaCoefficients,
        reference_white_nits: f32,
    ) -> [f32; 3] {
        let hlg_nominal_peak_nits = 1000.0_f32;
        let hlg_system_gamma = 1.2_f32;
        let scene = [
            hlg_inverse_oetf(encoded[0]),
            hlg_inverse_oetf(encoded[1]),
            hlg_inverse_oetf(encoded[2]),
        ];
        let scene_luma = (luma.kr * scene[0] + luma.kg * scene[1] + luma.kb * scene[2]).max(1e-6);
        let scale = hlg_nominal_peak_nits * scene_luma.powf(hlg_system_gamma - 1.0)
            / reference_white_nits.max(1.0);
        [scene[0] * scale, scene[1] * scale, scene[2] * scale]
    }

    #[test]
    #[allow(clippy::excessive_precision)]
    fn hlg_inverse_oetf_matches_bt2100_anchors() {
        assert!(hlg_inverse_oetf(0.0).abs() < 1e-7);
        // E' = 0.5 sits exactly at the 1/12 scene light scale.
        assert!((hlg_inverse_oetf(0.5) - 1.0 / 12.0).abs() < 1e-6);
        // The quadratic and exponential branches are continuous at E' = 0.5.
        let upper_at_half = (((0.5_f32 - 0.55991073) / 0.17883277).exp() + 0.28466892) / 12.0;
        assert!((hlg_inverse_oetf(0.5) - upper_at_half).abs() < 1e-5);
        // Full-scale signal decodes to unit scene light.
        assert!((hlg_inverse_oetf(1.0) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn hlg_full_scale_white_reaches_nominal_peak_over_reference_white() {
        let luma = MatrixCoefficients::Bt2020NonConstantLuminance
            .luma_coefficients(ColorPrimaries::Bt2020);

        let rgb = hlg_encoded_to_source_reference_linear([1.0, 1.0, 1.0], luma, 203.0);

        for channel in rgb {
            assert!(
                (channel - 1000.0 / 203.0).abs() < 0.01,
                "expected {channel} to be near the 1000 nit peak over 203 nit reference white"
            );
        }
    }

    #[test]
    fn hlg_reference_white_signal_lands_at_reference_white() {
        // BT.2408: the 75% HLG signal displays at roughly 203 nits on the
        // 1000 nit nominal display, i.e. 1.0 in source-reference-linear terms.
        let luma = MatrixCoefficients::Bt2020NonConstantLuminance
            .luma_coefficients(ColorPrimaries::Bt2020);

        let rgb = hlg_encoded_to_source_reference_linear([0.75, 0.75, 0.75], luma, 203.0);

        for channel in rgb {
            assert!(
                (channel - 1.0).abs() < 0.005,
                "expected {channel} to be near 1.0 (203 nits over 203 nit reference white)"
            );
        }
    }

    #[test]
    fn hlg_source_uniforms_use_code_4_and_thousand_nit_peak() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Hlg);
        let target = TargetColorState::sdr(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);

        let uniforms = VideoUniforms::from_pipeline(&pipeline, false, false);

        assert!(source.is_hdr());
        assert_eq!(uniforms.source_transfer, 4);
        assert_eq!(uniforms.nits[0], 1000.0);
        assert_eq!(uniforms.nits[2], 203.0);
        assert!(pipeline.requires_tone_mapping());
    }

    #[test]
    fn hlg_decode_formula_is_identical_across_video_shaders() {
        let shaders = [
            include_str!("wgpu_video.wgsl"),
            include_str!("metal/apple.rs"),
            include_str!("d3d11.rs"),
        ];
        for shader in shaders {
            assert!(shader.contains("hlg_inverse_oetf"));
            assert!(shader.contains("0.17883277"));
            assert!(shader.contains("0.28466892"));
            assert!(shader.contains("0.55991073"));
            assert!(shader.contains("e * e / 3.0"));
            assert!(shader.contains("(exp((e - c) / a) + b) / 12.0"));
            assert!(shader.contains("source_transfer == 4"));
            assert!(shader.contains("hlg_nominal_peak_nits = 1000.0"));
            assert!(shader.contains("hlg_system_gamma = 1.2"));
            assert!(shader.contains("pow(scene_luma, hlg_system_gamma - 1.0)"));
        }
    }

    /// PQ-code helpers matching the shaders; see `wgpu_video.wgsl`.
    fn pq_code(nits: f32) -> f32 {
        let m1 = 0.1593017578125_f32;
        let m2 = 78.84375_f32;
        let c1 = 0.8359375_f32;
        let c2 = 18.8515625_f32;
        let c3 = 18.6875_f32;
        let p = (nits / 10000.0).clamp(0.0, 1.0).powf(m1);
        ((c1 + c2 * p) / (1.0 + c3 * p).max(0.000_001)).powf(m2)
    }

    fn nits_from_pq(code: f32) -> f32 {
        let m1 = 0.1593017578125_f32;
        let m2 = 78.84375_f32;
        let c1 = 0.8359375_f32;
        let c2 = 18.8515625_f32;
        let c3 = 18.6875_f32;
        let p = code.clamp(0.0, 1.0).powf(1.0 / m2);
        let num = (p - c1).max(0.0);
        let den = (c2 - c3 * p).max(0.000_001);
        10000.0 * (num / den).powf(1.0 / m1)
    }

    /// Reference implementation of the shaders' `tone_map_curve_pq` spline
    /// branch (tone_map code 4): the single-pivot polynomial from libplacebo's
    /// `pl_tone_map_spline`, where the pivot follows the scene average. An
    /// unknown average (0) takes the shaders' fallback of
    /// `clamp(0.4 * src_peak, 100, 400)` nits before the knee pick.
    fn spline_pq(
        x: f32,
        src_peak_nits: f32,
        src_avg_nits: f32,
        dst_peak_nits: f32,
        dst_black_nits: f32,
        contrast: f32,
    ) -> f32 {
        let in_min = 0.0;
        let in_max = pq_code(src_peak_nits).max(0.000_001);
        let out_min = pq_code(dst_black_nits);
        let out_max = pq_code(dst_peak_nits).max(0.000_001);
        let fallback_avg = (0.4 * src_peak_nits).clamp(100.0, 400.0);
        let effective_src_avg = if src_avg_nits > 0.0 {
            src_avg_nits
        } else {
            fallback_avg
        };
        let (src_pivot, dst_pivot) =
            st2094_pick_knee(in_min, in_max, pq_code(effective_src_avg), out_min, out_max);
        let slope0 = (dst_pivot - out_min) / (src_pivot - in_min).max(0.000_001);
        let ratio = (1.5 * (in_max / out_max - 1.0)).clamp(0.2, 1.2);
        let slope = slope0.powf((1.0 - contrast) * ratio);
        let (in_min0, in_max0) = (in_min - src_pivot, in_max - src_pivot);
        let (out_min0, out_max0) = (out_min - dst_pivot, out_max - dst_pivot);
        let pa = (out_min0 - slope * in_min0) / (in_min0 * in_min0);
        let qa = (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0 * in_max0);
        let qb = -3.0 * (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0);
        let xr = x.clamp(in_min, in_max) - src_pivot;
        let mapped = if xr > 0.0 {
            ((qa * xr + qb) * xr + slope) * xr
        } else {
            (pa * xr + slope) * xr
        };
        mapped + dst_pivot
    }

    #[test]
    fn spline_curve_anchors_and_monotonicity() {
        let (src_peak, dst_peak, black) = (1000.0_f32, 100.0_f32, 0.0_f32);
        let contrast = 0.3_f32;
        // Endpoints are exact by construction: the quadratic anchors the lower
        // endpoint, the cubic anchors the source peak on the target peak.
        let lo = spline_pq(0.0, src_peak, 0.0, dst_peak, black, contrast);
        assert!(nits_from_pq(lo).abs() < 1e-3, "black maps to {lo} PQ");
        let hi = spline_pq(pq_code(src_peak), src_peak, 0.0, dst_peak, black, contrast);
        assert!(nits_from_pq(hi).abs() - dst_peak < 0.05, "peak = {hi} PQ");
        // Monotonic and bounded.
        let mut previous = -1.0_f32;
        for step in 0..=100 {
            let x = pq_code(src_peak) * step as f32 / 100.0;
            let mapped = spline_pq(x, src_peak, 0.0, dst_peak, black, contrast);
            assert!(mapped >= previous, "not monotonic at {step}");
            assert!(mapped <= pq_code(dst_peak) + 1e-3);
            previous = mapped;
        }
        // 10:1 compression puts 100-nit diffuse white below half of the
        // mastered value. Without metadata the shaders fall back to a
        // 400-nit average (0.4 * 1000 clamped), so the unknown-metadata
        // curve is the darker one.
        let diffuse = spline_pq(pq_code(100.0), src_peak, 0.0, dst_peak, black, contrast);
        let diffuse_nits = nits_from_pq(diffuse);
        let diffuse_known = spline_pq(pq_code(100.0), src_peak, 100.0, dst_peak, black, contrast);
        let diffuse_known_nits = nits_from_pq(diffuse_known);
        assert!(
            diffuse_nits > 10.0 && diffuse_nits < 60.0,
            "diffuse = {diffuse_nits}"
        );
        assert!(
            diffuse_nits < diffuse_known_nits,
            "fallback avg {diffuse_nits} must sit below the 100-nit-avg curve {diffuse_known_nits}"
        );
    }

    #[test]
    fn spline_knee_follows_scene_average_brightness() {
        // With a known scene average the pivot tracks the content, so a dark
        // scene gets a lower source knee than a bright one (both stay within
        // the [10%, 80%] of range clamp).
        let (src_peak, dst_peak, black) = (1000.0_f32, 100.0_f32, 0.0_f32);
        let dark_avg = 100.0_f32;
        let bright_avg = 400.0_f32;
        let dark_pivot = st2094_pick_knee(
            0.0,
            pq_code(src_peak),
            pq_code(dark_avg),
            pq_code(black),
            pq_code(dst_peak),
        )
        .0;
        let bright_pivot = st2094_pick_knee(
            0.0,
            pq_code(src_peak),
            pq_code(bright_avg),
            pq_code(black),
            pq_code(dst_peak),
        )
        .0;
        assert!(
            dark_pivot < bright_pivot,
            "dark {dark_pivot} vs bright {bright_pivot}"
        );
        // Without metadata the pivot is the 40% default mix.
        let default_pivot = st2094_pick_knee(
            0.0,
            pq_code(src_peak),
            0.0,
            pq_code(black),
            pq_code(dst_peak),
        )
        .0;
        assert!((default_pivot - pq_code(src_peak) * 0.4).abs() < 1e-4);
    }

    #[test]
    fn pick_knee_clamps_stay_inside_the_fraction_range() {
        // The pivot selection happens in the space the curve operates in
        // (PQ for the shaders' spline, nits for ST 2094-10), so a mid-average
        // input never escapes the [10%, 80%] of range clamp.
        let (src_pq, dst_pq) = st2094_pick_knee(
            0.0,
            pq_code(1000.0),
            pq_code(500.0),
            pq_code(0.0),
            pq_code(100.0),
        );
        assert!(src_pq >= pq_code(1000.0) * 0.1 - 1e-4);
        assert!(src_pq <= pq_code(1000.0) * 0.8 + 1e-4);
        assert!(dst_pq >= pq_code(100.0) * 0.1 - 1e-4);
        assert!(dst_pq <= pq_code(100.0) * 0.8 + 1e-4);
        // An out-of-range average clamps to the same fraction window.
        let clamped = st2094_pick_knee(
            0.0,
            pq_code(1000.0),
            pq_code(9999.0),
            pq_code(0.0),
            pq_code(100.0),
        );
        assert!((clamped.0 - pq_code(1000.0) * 0.8).abs() < 1e-4);
    }

    /// Reference implementation of the shaders' BT.2390 branch with the
    /// libplacebo black-point compensation (tone_map code 3).
    fn bt2390_pq(
        x: f32,
        src_peak_nits: f32,
        dst_peak_nits: f32,
        dst_black_nits: f32,
        knee_offset: f32,
    ) -> f32 {
        let in_max = pq_code(src_peak_nits).max(0.000_001);
        let out_min = pq_code(dst_black_nits);
        let out_max = pq_code(dst_peak_nits).max(0.000_001);
        let max_lum = (out_max / in_max).clamp(0.0, 1.0);
        let min_lum = out_min / in_max;
        let ks = (1.0 + knee_offset) * max_lum - knee_offset;
        let bp = (1.0 / min_lum.max(0.000_001)).min(4.0);
        let mut u = x.clamp(0.0, in_max) / in_max;
        if ks < 1.0 && u > ks {
            let tb = (u - ks) / (1.0 - ks);
            let tb2 = tb * tb;
            let tb3 = tb2 * tb;
            u = (2.0 * tb3 - 3.0 * tb2 + 1.0) * ks
                + (tb3 - 2.0 * tb2 + tb) * (1.0 - ks)
                + (-2.0 * tb3 + 3.0 * tb2) * max_lum;
        }
        if u < 1.0 {
            u = u + min_lum * (1.0 - u).powf(bp);
            let gain = if max_lum < 1.0 {
                1.0 / (1.0 + min_lum / max_lum * (1.0 - max_lum).powf(bp))
            } else {
                1.0
            };
            u = gain * (u - min_lum) + min_lum;
        }
        u * in_max
    }

    /// Reference implementation of the shaders' `tone_map_curve_pq` Reinhard
    /// branch (tone_map code 1).
    ///
    /// libplacebo evaluates this curve in the linear PL_HDR_NORM domain while
    /// the PQ-domain operators (BT.2390, spline, ST 2094-10) work on encoded
    /// values, so the reference rescales out of PQ codes before the curve and
    /// re-encodes the result. Normalization cancels: `pl_tone_map_reinhard`
    /// uses ratios and differences of same-unit linear values, so absolute
    /// nits are equivalent to PL_HDR_NORM here.
    fn reinhard_nits(
        x: f32,
        src_peak_nits: f32,
        dst_peak_nits: f32,
        dst_black_nits: f32,
        contrast: f32,
    ) -> f32 {
        let in_max = src_peak_nits.max(0.000_001);
        let out_min = dst_black_nits;
        let out_range = (dst_peak_nits - dst_black_nits).max(0.000_001);
        let peak = in_max / out_range;
        let contrast = if contrast > 0.0 { contrast } else { 0.5 };
        let offset = (1.0 - contrast) / contrast.max(0.000_001);
        let scale = (peak + offset) / peak;
        let t = nits_from_pq(x).clamp(0.0, in_max) / out_range;
        let mapped = t / (t + offset) * scale;
        pq_code(mapped * out_range + out_min)
    }

    /// Reference implementation of the shaders' `tone_map_curve_pq` Mobius
    /// branch (tone_map code 2); linear domain as in [`reinhard_nits`]. The
    /// knee `knee` is relative to the output range, so the 1:1 region ends at
    /// `dst_black + knee * (dst_peak - dst_black)` nits.
    fn mobius_nits(
        x: f32,
        src_peak_nits: f32,
        dst_peak_nits: f32,
        dst_black_nits: f32,
        knee: f32,
    ) -> f32 {
        let in_max = src_peak_nits.max(0.000_001);
        let out_min = dst_black_nits;
        let out_range = (dst_peak_nits - dst_black_nits).max(0.000_001);
        let peak = in_max / out_range;
        let j = if knee > 0.0 { knee } else { 0.3 };
        let a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
        let b = (j * j - 2.0 * j * peak + peak) / (peak - 1.0).max(0.000_001);
        let scale = (b * b + 2.0 * b * j + j * j) / (b - a);
        let t = nits_from_pq(x).clamp(0.0, in_max) / out_range;
        let mapped = if t > j { scale * (t + a) / (t + b) } else { t };
        pq_code(mapped * out_range + out_min)
    }

    #[test]
    fn reinhard_and_mobius_run_in_linear_luminance() {
        // The PR review measured a 1000->203 nit target with a near-zero
        // black and the Mobius knee at 0.3: a 50-nit patch must pass through
        // untouched, because the 1:1 region ends at 0.3 * 203 = 60.9 nits.
        // Evaluating the curve on PQ codes instead pushed the knee far lower
        // and darkened that patch to ~31.3 nits.
        let (source_peak, target_peak, black) = (1000.0_f32, 203.0_f32, 0.0_f32);
        let knee_nits = 0.3 * (target_peak - black);
        assert!((knee_nits - 60.9).abs() < 0.05, "knee = {knee_nits}");

        let below_knee = nits_from_pq(mobius_nits(
            pq_code(50.0),
            source_peak,
            target_peak,
            black,
            0.3,
        ));
        assert!(
            (below_knee - 50.0).abs() < 0.5,
            "50-nit patch below the knee = {below_knee} nits"
        );

        // Both curves anchor the source peak on the target peak.
        for mapped in [
            nits_from_pq(reinhard_nits(
                pq_code(source_peak),
                source_peak,
                target_peak,
                black,
                0.0,
            )),
            nits_from_pq(mobius_nits(
                pq_code(source_peak),
                source_peak,
                target_peak,
                black,
                0.3,
            )),
        ] {
            assert!(
                (mapped - target_peak).abs() < 0.5,
                "peak maps to {mapped} nits"
            );
        }

        // Reinhard compresses the whole range, so it must darken the 50-nit
        // patch slightly and stay monotonic.
        let reinhard_50 = nits_from_pq(reinhard_nits(
            pq_code(50.0),
            source_peak,
            target_peak,
            black,
            0.0,
        ));
        assert!(
            reinhard_50 > 40.0 && reinhard_50 < 50.0,
            "reinhard 50-nit patch = {reinhard_50} nits"
        );

        for curve in [0, 1] {
            let mut previous = -1.0_f32;
            for step in 0..=100 {
                let x = pq_code(source_peak) * step as f32 / 100.0;
                let mapped = if curve == 0 {
                    reinhard_nits(x, source_peak, target_peak, black, 0.0)
                } else {
                    mobius_nits(x, source_peak, target_peak, black, 0.3)
                };
                assert!(
                    mapped >= previous,
                    "curve {curve} step {step} is not monotonic"
                );
                previous = mapped;
            }
        }
    }

    #[test]
    fn tone_map_uniform_code_skips_the_curve_for_unmapped_content() {
        // SDR -> SDR needs no tone mapping, so the shader must receive Clip
        // (an identity curve) rather than the selected operator: running
        // Reinhard here brightened an SDR 100-nit grey from ~130 to ~175 in
        // the review's readback.
        let source = SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Srgb);
        let target = TargetColorState::sdr(ColorPrimaries::Bt709);
        let mut pipeline = VideoRenderPipeline::new(source, target);
        assert!(!pipeline.requires_tone_mapping());
        pipeline.tone_map.operator = ToneMapOperator::Reinhard;
        assert_eq!(
            pipeline.tone_map_uniform_code(),
            tone_map_code(ToneMapOperator::Clip)
        );
        assert_eq!(
            VideoUniforms::from_pipeline(&pipeline, false, false).tone_map,
            0,
            "the shader must see the identity curve"
        );

        // An HDR source that overshoots the target keeps the operator so the
        // curve actually runs.
        let hdr_source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let mut hdr_pipeline = VideoRenderPipeline::new(
            hdr_source,
            TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709),
        );
        assert!(hdr_pipeline.requires_tone_mapping());
        hdr_pipeline.tone_map.operator = ToneMapOperator::Reinhard;
        assert_eq!(
            hdr_pipeline.tone_map_uniform_code(),
            tone_map_code(ToneMapOperator::Reinhard)
        );
    }

    #[test]
    fn bt2390_curve_anchors_and_monotonicity() {
        let (source_peak, target_peak, black) = (1000.0_f32, 100.0_f32, 0.203_f32);
        // Black maps to the compensated target black (0.203 nit), which the
        // encode stage maps back onto code 0.
        let black_mapped = nits_from_pq(bt2390_pq(0.0, source_peak, target_peak, black, 1.0));
        assert!(
            (black_mapped - black).abs() < 0.05,
            "black = {black_mapped}"
        );
        let peak = nits_from_pq(bt2390_pq(
            pq_code(source_peak),
            source_peak,
            target_peak,
            black,
            1.0,
        ));
        // Black-point compensation perturbs the exact peak slightly; the
        // endpoint stays within half a nit of the target.
        assert!((peak - target_peak).abs() < 0.5, "peak = {peak}");
        let mut previous = -1.0_f32;
        for step in 0..=100 {
            let x = pq_code(source_peak) * step as f32 / 100.0;
            let mapped = nits_from_pq(bt2390_pq(x, source_peak, target_peak, black, 1.0));
            assert!(mapped >= previous);
            previous = mapped;
        }
        // BPC compensates: for a target with 0 black the curve would be
        // unchanged, with 0.203 it lifts dark steps slightly.
        let with_bpc = bt2390_pq(pq_code(1.0), source_peak, target_peak, 0.203, 1.0);
        let without_bpc = bt2390_pq(pq_code(1.0), source_peak, target_peak, 0.0, 1.0);
        assert!(with_bpc > without_bpc);
    }

    #[test]
    fn bt2390_hdr10_to_sdr_matches_broadcast_diffuse_white() {
        let (source_peak, target_peak, black) = (1000.0_f32, 203.0_f32, 0.0_f32);
        let diffuse_white_mapped = nits_from_pq(bt2390_pq(
            pq_code(203.0),
            source_peak,
            target_peak,
            black,
            1.0,
        ));
        // On a 203-nit SDR tone-map target, 203 nits diffuse white must map
        // to >= 130 nits (sRGB value >= 210, matching Infuse/MPV and well above 188.1).
        assert!(
            diffuse_white_mapped >= 130.0,
            "mapped diffuse white: {diffuse_white_mapped} nits"
        );
    }

    /// Reference implementation of the shaders' BT.2446 method A branch
    /// (tone_map code 5), evaluated in nits.
    fn bt2446a_nits(
        x_nits: f32,
        src_peak_nits: f32,
        dst_peak_nits: f32,
        dst_black_nits: f32,
    ) -> f32 {
        let phdr = 1.0 + 32.0 * (src_peak_nits / 10000.0).powf(1.0 / 2.4);
        let psdr = 1.0 + 32.0 * (dst_peak_nits / 10000.0).powf(1.0 / 2.4);
        let mut t = (x_nits.clamp(0.0, src_peak_nits) / src_peak_nits).powf(1.0 / 2.4);
        t = (1.0 + (phdr - 1.0) * t).ln() / phdr.ln();
        t = if t <= 0.7399 {
            1.0770 * t
        } else if t < 0.9909 {
            (-1.1510 * t + 2.7811) * t - 0.6302
        } else {
            0.5 * t + 0.5
        };
        t = (psdr.powf(t) - 1.0) / (psdr - 1.0);
        let lb = dst_black_nits.max(0.0).powf(1.0 / 2.4);
        let lw = dst_peak_nits.max(0.0).powf(1.0 / 2.4);
        ((lw - lb) * t + lb).powf(2.4)
    }

    #[test]
    fn bt2446a_curve_anchors_and_monotonicity() {
        let (src_peak, dst_peak, black) = (1000.0_f32, 100.0_f32, 0.203_f32);
        let lo = bt2446a_nits(0.0, src_peak, dst_peak, black);
        assert!((lo - black).abs() < 0.05, "black = {lo}");
        let hi = bt2446a_nits(src_peak, src_peak, dst_peak, black);
        assert!((hi - dst_peak).abs() < 0.05, "peak = {hi}");
        let mut previous = -1.0_f32;
        for step in 0..=100 {
            let mapped = bt2446a_nits(src_peak * step as f32 / 100.0, src_peak, dst_peak, black);
            assert!(mapped >= previous);
            previous = mapped;
        }
    }

    #[test]
    fn st2094_10_coefficients_interpolate_the_three_anchors() {
        let (src_peak, dst_peak, black) = (1000.0_f32, 100.0_f32, 0.203_f32);
        let (src_knee, dst_knee) =
            st2094_pick_knee_nits(0.0, src_peak, 250.0, black, dst_peak, 0.4);
        let [c1, c2, c3, _] = solve_st2094_10(0.0, src_knee, src_peak, black, dst_knee, dst_peak);
        let eval = |x_nits: f32| (c1 + c2 * x_nits) / (1.0 + c3 * x_nits);
        assert!((eval(0.0) - black).abs() < 1e-3);
        assert!((eval(src_knee) - dst_knee).abs() < 1e-2);
        assert!((eval(src_peak) - dst_peak).abs() < 1e-2);
        let mut previous = -1.0_f32;
        for step in 0..=100 {
            let mapped = eval(src_peak * step as f32 / 100.0);
            assert!(mapped >= previous);
            previous = mapped;
        }
    }

    #[test]
    fn st2094_10_degenerate_anchors_fall_back_to_the_identity_curve() {
        // Duplicated or non-finite anchors have no unique solution. The curve
        // must degrade to Clip (y = x); returning zero coefficients would make
        // the shader map every pixel to PQ 0, i.e. a black frame.
        let identity = [0.0, 1.0, 0.0, 0.0];
        assert_eq!(solve_st2094_10(0.0, 0.0, 0.0, 0.0, 1.0, 1.0), identity);
        assert_eq!(solve_st2094_10(0.0, 1.0, 1.0, 0.0, 1.0, 1.0), identity);
        assert_eq!(solve_st2094_10(f32::NAN, 1.0, 2.0, 0.0, 1.0, 2.0), identity);
        assert_eq!(
            solve_st2094_10(0.0, 1.0, 2.0, f32::INFINITY, 1.0, 2.0),
            identity
        );
        let [c1, c2, c3, _] = identity;
        let eval = |x: f32| (c1 + c2 * x) / (1.0 + c3 * x);
        assert!((eval(0.25) - 0.25).abs() < 1e-6);
        assert!((eval(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn st2094_10_knee_follows_measured_scene_average_for_hdr10() {
        // ST 2094-10 must use the same scene-average fallback chain as the
        // spline pivot (measured average, then DV L1, then MaxFALL), so an
        // HDR10 stream with measured luma gets a content-following knee
        // instead of the 40% default.
        let target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let mut config = ToneMapConfig::default();
        config.operator = ToneMapOperator::St209410;

        let dark = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .measured_scene_avg_nits(Some(60.0));
        let bright = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .measured_scene_avg_nits(Some(400.0));
        let dark = VideoRenderPipeline {
            tone_map: config,
            ..VideoRenderPipeline::new(dark, target)
        };
        let bright = VideoRenderPipeline {
            tone_map: config,
            ..VideoRenderPipeline::new(bright, target)
        };
        let dark_knee = st2094_pick_knee_nits(
            0.0,
            dark.source.nominal_peak_nits,
            source_scene_avg_nits(&dark.source),
            0.203,
            dark.target.peak_nits,
            0.4,
        )
        .0;
        let bright_knee = st2094_pick_knee_nits(
            0.0,
            bright.source.nominal_peak_nits,
            source_scene_avg_nits(&bright.source),
            0.203,
            bright.target.peak_nits,
            0.4,
        )
        .0;
        assert!(
            dark_knee < bright_knee,
            "dark {dark_knee} vs bright {bright_knee}"
        );

        // MaxFALL is the third rung of the fallback chain.
        let metadata = HdrMetadata::new(
            None,
            Some(ContentLightMetadata {
                max_content_light_level_nits: 1000,
                max_frame_average_light_level_nits: 239,
            }),
        );
        let fall = VideoRenderPipeline {
            tone_map: config,
            ..VideoRenderPipeline::new(
                SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
                    .hdr_metadata(Some(metadata)),
                target,
            )
        };
        assert!((source_scene_avg_nits(&fall.source) - 239.0).abs() < 1e-3);
    }

    #[test]
    fn st2094_pick_knee_nits_picks_the_knee_in_pq_space() {
        // libplacebo always picks the ST 2094 knee in the PQ domain, even for
        // the NITS-scaled functions. A 50-nit average on a 4000-nit source must
        // land near 50 nits, not on the linear-space 10% floor (400 nits) that
        // a nits-domain evaluation would clamp to.
        let (src_knee, _) = st2094_pick_knee_nits(0.0, 4000.0, 50.0, 0.0, 203.0, 0.4);
        assert!(
            (src_knee - 50.0).abs() < 2.0,
            "knee should follow the scene average in nits: {src_knee}"
        );
        // The nits wrapper is exactly the PQ-domain call with a rescale.
        let (pq_src, pq_dst) = st2094_pick_knee(
            0.0,
            pq_code_from_nits(4000.0),
            pq_code_from_nits(50.0),
            0.0,
            pq_code_from_nits(203.0),
        );
        let (nits_src, nits_dst) = st2094_pick_knee_nits(0.0, 4000.0, 50.0, 0.0, 203.0, 0.4);
        assert!((nits_from_pq_code(pq_src) - nits_src).abs() < 1e-3);
        assert!((nits_from_pq_code(pq_dst) - nits_dst).abs() < 1e-3);
    }

    #[test]
    fn st2094_10_curve_param_tunes_the_knee_adaptation() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .measured_scene_avg_nits(Some(120.0));
        let target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let coefficients = |curve_param: f32| {
            let config = ToneMapConfig {
                operator: ToneMapOperator::St209410,
                curve_param,
                ..ToneMapConfig::default()
            };
            st2094_10_coefficients_for(&VideoRenderPipeline {
                tone_map: config,
                ..VideoRenderPipeline::new(source, target)
            })
        };
        let low = coefficients(0.1);
        let high = coefficients(1.0);
        assert_ne!(
            low, high,
            "curve_param must reach the ST 2094-10 knee adaptation"
        );
        // 0 resolves to libplacebo's `pl_tone_map_st2094_10.param_def`.
        assert_eq!(coefficients(0.0), coefficients(0.7));
    }

    #[test]
    fn dovi_l1_peak_does_not_toggle_the_tone_map_or_gamut_lut() {
        // `nominal_peak_nits` carries the per-frame L1 peak for Dolby Vision
        // content. Pass selection must stay on the static L0 peak so a dark
        // scene cannot drop the tone map, the black-point compensation, or the
        // perceptual gamut LUT for a single frame.
        let target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let dark = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq).dovi(Some(
            sample_dovi_metadata_with_l1(0, pq_code_12(150.0), pq_code_12(80.0)),
        ));
        let bright =
            SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq).dovi(Some(
                sample_dovi_metadata_with_l1(0, pq_code_12(2200.0), pq_code_12(1800.0)),
            ));
        assert!(
            dark.nominal_peak_nits < 200.0,
            "dark frame L1 peak should sit below the SDR target peak"
        );
        let dark = VideoRenderPipeline::new(dark, target);
        let bright = VideoRenderPipeline::new(bright, target);
        assert!(dark.requires_tone_mapping());
        assert!(bright.requires_tone_mapping());
        assert!(dark.gamut_lut_active());
        assert!(bright.gamut_lut_active());
        assert_eq!(dark.tone_map_extra()[2], bright.tone_map_extra()[2]);
    }

    #[test]
    fn tone_map_operator_defaults_and_codes() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .reference_white_nits(203.0);
        let target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);
        assert_eq!(pipeline.tone_map.operator, ToneMapOperator::Bt2390);
        assert_eq!(tone_map_code(pipeline.tone_map.operator), 3);
        // The curve parameter 0 resolves to the operator default.
        assert!((pipeline.tone_map.effective_curve_param() - 1.0).abs() < 1e-6);
        // Default contrast is 1000:1 for SDR targets -> 203 / 1000 black.
        let extra = pipeline.tone_map_extra();
        assert!((extra[2] - 0.203).abs() < 1e-4, "black = {}", extra[2]);
        assert_eq!(extra[0], 1.0);
    }

    #[test]
    fn hdr_output_target_black_is_zero() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let target = TargetColorState::hdr10(ColorPrimaries::Bt2020);
        let extra = VideoRenderPipeline::new(source, target).tone_map_extra();
        assert_eq!(extra[2], 0.0);
        let edr = VideoRenderPipeline::new(
            source,
            TargetColorState::apple_edr(ColorPrimaries::Bt2020, 2.0),
        )
        .tone_map_extra();
        assert_eq!(edr[2], 0.0);
        // An explicit contrast ratio overrides the automatic default.
        let target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let mut config = ToneMapConfig::default();
        config.contrast_ratio = 500.0;
        let pipeline = VideoRenderPipeline::new(source, target);
        let custom = VideoRenderPipeline {
            tone_map: config,
            ..pipeline
        };
        assert!((custom.tone_map_extra()[2] - 203.0 / 500.0).abs() < 1e-4);
    }

    #[test]
    fn sdr_to_sdr_black_point_is_zero() {
        // Black-point compensation must never run on SDR->SDR (near-black
        // content would crush); the tone-map-only guard zeroes the black.
        let source = SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Srgb);
        let target = TargetColorState::sdr(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);
        assert!(!pipeline.requires_tone_mapping());
        assert_eq!(pipeline.tone_map_extra()[2], 0.0);
        // An explicit contrast ratio is still ignored on the inactive path.
        let mut config = ToneMapConfig::default();
        config.contrast_ratio = 500.0;
        let custom = VideoRenderPipeline {
            tone_map: config,
            ..pipeline
        };
        assert_eq!(custom.tone_map_extra()[2], 0.0);
    }

    #[test]
    fn ipt_matrices_match_libplacebo_values_and_white_is_invariant() {
        // Values computed independently from libplacebo's pl_ipt_rgb2lms
        // (4% crosstalk mix of HPE XYZ->LMS times the primaries RGB->XYZ).
        let bt709 = ipt_rgb2lms_matrix(ColorPrimaries::Bt709);
        let expected709 = [
            [0.2957641, 0.6230725, 0.0811667],
            [0.1561920, 0.7272516, 0.1165579],
            [0.0351023, 0.1565899, 0.8083030],
        ];
        for (row, expected) in bt709.rows().iter().zip(expected709) {
            for (value, expected) in row.iter().zip(expected) {
                assert!((value - expected).abs() < 1e-5, "{value} != {expected}");
            }
        }
        // D65 white maps to equal LMS across primaries and inverts back.
        for primaries in [
            ColorPrimaries::Bt709,
            ColorPrimaries::Bt2020,
            ColorPrimaries::DisplayP3,
        ] {
            let matrix = ipt_rgb2lms_matrix(primaries);
            let lms = matrix.mul_vec([1.0, 1.0, 1.0]);
            for value in lms {
                assert!((value - 1.0).abs() < 1e-3, "white -> {lms:?}");
            }
            let inverse = ipt_lms2rgb_matrix(primaries);
            let back = inverse.mul_vec(lms);
            for value in back {
                assert!((value - 1.0).abs() < 1e-3, "roundtrip -> {back:?}");
            }
        }
    }

    #[test]
    fn tone_map_pipeline_is_present_across_video_shaders() {
        let shaders = [
            include_str!("wgpu_video.wgsl"),
            include_str!("metal/apple.rs"),
            include_str!("d3d11.rs"),
        ];
        for shader in shaders {
            assert!(shader.contains("st2094_pick_knee"));
            assert!(shader.contains("tone_map_curve_pq"));
            assert!(shader.contains("ipt_matrix_rows"));
            assert!(shader.contains("tone_map_extra"));
            assert!(shader.contains("tone_map_coeffs"));
            assert!(shader.contains("0.0975689"));
            assert!(shader.contains("tone_map == 4"));
            assert!(shader.contains("tone_map == 5"));
            assert!(shader.contains("SMPTE ST 2094-10"));
            assert!(shader.contains("0.7399"));
            // The IPT path applies the primaries conversion inside the tone
            // map, so the old separate matrix call is gone.
            assert!(!shader.contains("rgb = apply_gamut_map(rgb)"));
        }
    }

    #[test]
    fn measured_scene_avg_precedes_dovi_l1_and_drives_the_pivot() {
        let mut source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        // HDR10 without L1: attach a measured average, tone_map_extra.y follows.
        source = source.measured_scene_avg_nits(Some(120.0));
        let target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);
        let extra = pipeline.tone_map_extra();
        assert!((extra[1] - 120.0).abs() < 1e-3, "scene avg {}", extra[1]);
        // A non-finite / zero value clears the channel.
        let cleared_source = source.measured_scene_avg_nits(Some(0.0));
        assert_eq!(cleared_source.measured_scene_avg_nits, None);
        let cleared = VideoRenderPipeline::new(cleared_source, target);
        assert_eq!(cleared.tone_map_extra()[1], 0.0);
    }

    #[test]
    fn gamut_lut_active_only_for_tone_mapped_wide_gamut_sources() {
        // BT.2020 PQ -> BT.709 SDR needs the perceptual LUT.
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);
        assert!(pipeline.gamut_lut_active());
        assert_eq!(
            VideoUniforms::from_pipeline(&pipeline, false, false).gamut_lut_enabled,
            1
        );
        // Same-gamut HDR tone map keeps the fast path.
        let same = VideoRenderPipeline::new(
            source,
            TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt2020),
        );
        assert!(!same.gamut_lut_active());
        // SDR -> SDR passthrough never maps.
        let sdr = VideoRenderPipeline::new(
            SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Srgb),
            TargetColorState::sdr(ColorPrimaries::Bt709),
        );
        assert!(!sdr.gamut_lut_active());
        // HDR10 native output (PQ target, display does the mapping) needs no
        // gamut LUT either.
        let hdr10 =
            VideoRenderPipeline::new(source, TargetColorState::hdr10(ColorPrimaries::Bt2020));
        assert!(!hdr10.gamut_lut_active());
    }

    #[test]
    fn gamut_lut_primaries_code_round_trips() {
        let code = gamut_primaries_code(ColorPrimaries::Bt2020, ColorPrimaries::Bt709);
        assert_eq!(code >> 8, 1);
        assert_eq!(code & 0xff, 0);
        let p3 = gamut_primaries_code(ColorPrimaries::Bt709, ColorPrimaries::DisplayP3);
        assert_eq!(p3 >> 8, 0);
        assert_eq!(p3 & 0xff, 2);
    }

    #[test]
    fn gamut_lut_sampling_is_present_across_video_shaders() {
        let shaders = [
            include_str!("wgpu_video.wgsl"),
            include_str!("metal/apple.rs"),
            include_str!("d3d11.rs"),
        ];
        for shader in shaders {
            assert!(shader.contains("gamut_lut_enabled"));
            assert!(shader.contains("0.5 + 0.5 * atan2"));
            assert!(shader.contains("ipt_matrix_rows[6].xyz"));
            assert!(shader.contains("sampled.y - 0.5"));
            // The LUT path replaces the fast gamut_compress.
            assert!(shader.contains("gamut_compress"));
        }
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn wgsl_video_shader_parses_with_naga() {
        // The wgpu backend compiles the WGSL only at render time; parse it
        // here so syntax regressions fail in tests instead of on-device.
        let source = include_str!("wgpu_video.wgsl");
        wgpu::naga::front::wgsl::parse_str(source)
            .unwrap_or_else(|error| panic!("invalid WGSL: {error}"));
    }

    /// Reference implementation of the shaders' `gamut_compress`: colors the
    /// linear gamut matrix pushes out of the target gamut are blended
    /// towards their naively-clipped version by an out-of-gamut smoothstep.
    /// Slightly-out colors stay nearly intact; strongly-out BT.2020
    /// primaries land on the pure target primary with their hue intact —
    /// luma blending would instead pull primary red towards grey and turn
    /// it pink. Matches mpv's perceptual gamut mapping behavior.
    fn gamut_compress(rgb: [f32; 3]) -> [f32; 3] {
        let lo = rgb[0].min(rgb[1]).min(rgb[2]);
        let outness = (-lo).max(0.0);
        let x = (outness / 1.0).clamp(0.0, 1.0);
        let k = x * x * (3.0 - 2.0 * x);
        let mix = |a: f32, b: f32| a + (b - a) * k;
        [
            mix(rgb[0], rgb[0].clamp(0.0, 1.0)),
            mix(rgb[1], rgb[1].clamp(0.0, 1.0)),
            mix(rgb[2], rgb[2].clamp(0.0, 1.0)),
        ]
    }

    #[test]
    fn gamut_compress_preserves_hue_of_out_of_gamut_primaries() {
        // In-gamut colors pass through untouched.
        assert_eq!(gamut_compress([0.2, 0.7, 0.3]), [0.2, 0.7, 0.3]);
        // A saturated BT.2020 teal-green that the gamut matrix pushes out of
        // gamut: the compression never pushes a channel further out, and the
        // channel ordering (hue) is preserved — hard clipping would have
        // zeroed red/blue and turned it neon. (Residual negative components
        // are clamped at encode time, as libplacebo clamps to its gamut
        // floor.)
        let out_of_gamut = [-0.08_f32, 0.9, -0.04];
        let mapped = gamut_compress(out_of_gamut);
        assert!(mapped[0] > out_of_gamut[0] && mapped[2] > out_of_gamut[2]);
        assert!(mapped[1] > mapped[2] && mapped[2] > mapped[0]);
        // A strongly-out primary red (negative green and blue) maps to a
        // hue-pure red: green and blue are compressed to ~0 together, and
        // crucially blue is not lifted towards the luma grey — that is what
        // turned wide-gamut red pink under luma blending.
        let primary_red = [1.1_f32, -0.22, -0.07];
        let red = gamut_compress(primary_red);
        assert!(red[1] < 0.01 && red[2] < 0.01, "red = {red:?}");
        assert!(red[0] > 0.9, "red = {red:?}");
        // Compression is monotonic: an out-of-gamut excursion of 1.0 maps fully
        // onto the clip (k = 1), while mild excursions keep most of their
        // range.
        assert_eq!(gamut_compress([-1.0_f32, 0.9, -0.5]), [0.0, 0.9, 0.0]);
        let mild = gamut_compress([-0.1_f32, 0.9, -0.05]);
        assert!(mild[0] > -0.1 && mild[2] > -0.05);
    }

    #[test]
    fn gamut_compress_formula_is_present_across_video_shaders() {
        let shaders = [
            include_str!("wgpu_video.wgsl"),
            include_str!("metal/apple.rs"),
            include_str!("d3d11.rs"),
        ];
        for shader in shaders {
            assert!(shader.contains("gamut_compress"));
            assert!(shader.contains("smoothstep(0.0, 1.0, outness)"));
            assert!(shader.contains("rgb = gamut_compress(rgb)"));
        }
    }

    #[test]
    fn overlay_shaders_handle_sdr_ui_for_hdr_targets() {
        let metal = include_str!("metal/apple.rs");
        let d3d11 = include_str!("d3d11.rs");
        assert!(metal.contains("float3 sdr_ui_color_to_target_output"));
        assert!(metal.contains("pq_inverse_eotf(nits.r / pq_absolute_peak_nits)"));

        // D3D11 composites into an FP16 reference-linear target, then applies
        // PQ once in a full-screen encode pass after alpha blending.
        assert!(d3d11.contains("if (ui_nits.y > 0.0)"));
        assert!(d3d11.contains("max(ui_nits.x, 1.0) / max(ui_nits.y, 1.0)"));
        assert!(d3d11.contains("float4 encode_ps_main"));
        assert!(d3d11.contains("if (scene_linear != 0u)"));
        assert!(d3d11.contains("PSSetShaderResources(0, Some(&[None]))"));
    }

    #[test]
    fn hdr_pq_to_sdr_builds_tone_mapping_graph() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let target = TargetColorState::sdr(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);

        assert!(pipeline.requires_tone_mapping());
        assert!(pipeline.graph.contains(RenderPassKind::TransferDecode));
        assert!(pipeline.graph.contains(RenderPassKind::GamutMap));
        assert!(pipeline.graph.contains(RenderPassKind::ToneMap));
        assert!(pipeline.graph.contains(RenderPassKind::OutputTransform));
    }

    #[test]
    fn hdr_pq_to_hdr10_keeps_absolute_pq_without_tone_mapping() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let target = TargetColorState::hdr10(ColorPrimaries::Bt2020);
        let pipeline = VideoRenderPipeline::new(source, target);

        assert_eq!(pipeline.target.transfer, TransferFunction::Pq);
        assert_eq!(pipeline.target.reference_white_nits, 203.0);
        assert!(!pipeline.requires_tone_mapping());
        assert!(!pipeline.graph.contains(RenderPassKind::GamutMap));
        assert!(!pipeline.graph.contains(RenderPassKind::ToneMap));
    }

    #[test]
    fn sdr_bt709_to_sdr_skips_tone_mapping() {
        let source = SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Srgb);
        let target = TargetColorState::sdr(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);

        assert!(!pipeline.requires_tone_mapping());
        assert!(!pipeline.graph.contains(RenderPassKind::ToneMap));
    }

    #[test]
    fn unknown_sdr_source_uses_sdr_reference_white() {
        let source = SourceColorState::new(ColorPrimaries::Unknown, TransferFunction::Unknown);

        assert_eq!(source.nominal_peak_nits, 100.0);
        assert_eq!(source.reference_white_nits, 100.0);
        assert!(!source.is_hdr());
    }

    #[test]
    fn retargeting_pipeline_preserves_render_options_and_rebuilds_graph() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let mut pipeline =
            VideoRenderPipeline::new(source, TargetColorState::sdr(ColorPrimaries::Bt709));
        pipeline.tone_map.operator = ToneMapOperator::Clip;
        pipeline.scaler.kernel = ScalerKernel::Nearest;

        let pipeline =
            pipeline.with_target(TargetColorState::apple_edr(ColorPrimaries::Bt709, 4.0));

        assert_eq!(pipeline.target.edr_headroom, 4.0);
        assert_eq!(pipeline.tone_map.operator, ToneMapOperator::Clip);
        assert_eq!(pipeline.scaler.kernel, ScalerKernel::Nearest);
        assert!(!pipeline.graph.contains(RenderPassKind::Scale));
    }

    #[test]
    fn luma_upscaler_adds_neural_upscale_pass() {
        let pipeline = VideoRenderPipeline::sdr_default();
        assert!(!pipeline.graph.contains(RenderPassKind::NeuralUpscale));

        let pipeline = pipeline.with_luma_upscaler(LumaUpscalerMode::ArtCnnC4F16);

        assert_eq!(pipeline.luma_upscaler, LumaUpscalerMode::ArtCnnC4F16);
        assert!(pipeline.graph.contains(RenderPassKind::NeuralUpscale));
    }

    #[test]
    fn matrix_defaults_follow_primaries() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let coeffs = source.matrix.luma_coefficients(source.primaries);

        assert!((coeffs.kr - 0.2627).abs() < 0.0001);
        assert!((coeffs.kg - 0.6780).abs() < 0.0001);
        assert!((coeffs.kb - 0.0593).abs() < 0.0001);
    }

    #[test]
    fn color_range_resolves_unspecified_to_fallback() {
        assert_eq!(
            ColorRange::Unspecified.resolve(ColorRange::Limited),
            ColorRange::Limited
        );
        assert_eq!(
            ColorRange::Full.resolve(ColorRange::Limited),
            ColorRange::Full
        );
    }

    #[test]
    fn hdr_metadata_prefers_mastering_display_peak() {
        let metadata = HdrMetadata::new(
            Some(MasteringDisplayMetadata {
                display_primaries: None,
                white_point: None,
                min_luminance_nits: Some(0.005),
                max_luminance_nits: Some(1000.0),
            }),
            Some(ContentLightMetadata {
                max_content_light_level_nits: 4000,
                max_frame_average_light_level_nits: 450,
            }),
        );

        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .hdr_metadata(Some(metadata));

        assert_eq!(metadata.nominal_peak_nits(), Some(1000.0));
        assert_eq!(source.nominal_peak_nits, 1000.0);
        assert_eq!(source.hdr_metadata, Some(metadata));
    }

    #[test]
    fn hdr_metadata_falls_back_to_max_cll_when_mastering_peak_is_missing() {
        let metadata = HdrMetadata::new(
            Some(MasteringDisplayMetadata {
                display_primaries: None,
                white_point: None,
                min_luminance_nits: Some(0.005),
                max_luminance_nits: None,
            }),
            Some(ContentLightMetadata {
                max_content_light_level_nits: 4000,
                max_frame_average_light_level_nits: 450,
            }),
        );

        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .hdr_metadata(Some(metadata));

        assert_eq!(metadata.nominal_peak_nits(), Some(4000.0));
        assert_eq!(source.nominal_peak_nits, 4000.0);
    }

    #[test]
    fn hdr_metadata_falls_back_to_mastering_peak_when_max_cll_is_missing() {
        let metadata = HdrMetadata::new(
            Some(MasteringDisplayMetadata {
                display_primaries: None,
                white_point: None,
                min_luminance_nits: Some(0.005),
                max_luminance_nits: Some(1000.0),
            }),
            Some(ContentLightMetadata {
                max_content_light_level_nits: 0,
                max_frame_average_light_level_nits: 450,
            }),
        );

        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .hdr_metadata(Some(metadata));

        assert_eq!(metadata.nominal_peak_nits(), Some(1000.0));
        assert_eq!(source.nominal_peak_nits, 1000.0);
    }

    #[test]
    fn hdr_metadata_nominal_peak_bounds_by_max_cll_when_lower_than_mastering() {
        let metadata = HdrMetadata::new(
            Some(MasteringDisplayMetadata {
                display_primaries: None,
                white_point: None,
                min_luminance_nits: Some(0.005),
                max_luminance_nits: Some(1000.0),
            }),
            Some(ContentLightMetadata {
                max_content_light_level_nits: 528,
                max_frame_average_light_level_nits: 239,
            }),
        );

        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .hdr_metadata(Some(metadata));

        assert_eq!(metadata.nominal_peak_nits(), Some(528.0));
        assert_eq!(source.nominal_peak_nits, 528.0);
    }

    #[test]
    fn tone_map_extra_falls_back_to_max_fall_when_unmeasured() {
        let metadata = HdrMetadata::new(
            Some(MasteringDisplayMetadata {
                display_primaries: None,
                white_point: None,
                min_luminance_nits: Some(0.005),
                max_luminance_nits: Some(1000.0),
            }),
            Some(ContentLightMetadata {
                max_content_light_level_nits: 528,
                max_frame_average_light_level_nits: 239,
            }),
        );

        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .hdr_metadata(Some(metadata));
        let target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);
        let extra = pipeline.tone_map_extra();
        assert_eq!(extra[1], 239.0);
    }

    #[test]
    fn bt709_to_bt709_gamut_matrix_is_identity() {
        let matrix = source_to_target_rgb_matrix(ColorPrimaries::Bt709, ColorPrimaries::Bt709);
        assert_matrix_close(matrix.rows(), RgbMatrix::identity().rows(), 0.00001);
    }

    #[test]
    fn unknown_primaries_fall_back_to_bt709_for_gamut_matrix() {
        let matrix = source_to_target_rgb_matrix(ColorPrimaries::Unknown, ColorPrimaries::Bt709);
        assert_matrix_close(matrix.rows(), RgbMatrix::identity().rows(), 0.00001);
    }

    #[test]
    fn bt2020_to_bt709_gamut_matrix_is_stable() {
        let matrix = source_to_target_rgb_matrix(ColorPrimaries::Bt2020, ColorPrimaries::Bt709);

        assert_matrix_close(
            matrix.rows(),
            [
                [1.66049, -0.58764, -0.07285],
                [-0.12455, 1.13290, -0.00835],
                [-0.01815, -0.10058, 1.11873],
            ],
            0.0002,
        );
    }

    #[test]
    fn display_p3_to_bt709_gamut_matrix_is_stable() {
        let matrix = source_to_target_rgb_matrix(ColorPrimaries::DisplayP3, ColorPrimaries::Bt709);

        assert_matrix_close(
            matrix.rows(),
            [
                [1.22494, -0.22494, 0.0],
                [-0.04206, 1.04206, 0.0],
                [-0.01964, -0.07864, 1.09827],
            ],
            0.0002,
        );
    }

    #[test]
    fn pipeline_reports_gamut_mapping_when_primaries_differ() {
        let pipeline = VideoRenderPipeline::new(
            SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq),
            TargetColorState::sdr(ColorPrimaries::Bt709),
        );

        assert!(pipeline.requires_gamut_mapping());
        assert!(pipeline.graph.contains(RenderPassKind::GamutMap));
    }

    #[test]
    fn packed_alpha_flag_survives_source_texture_mode_changes() {
        let pipeline = VideoRenderPipeline::sdr_default();
        let uniforms = VideoUniforms::from_pipeline(&pipeline, false, false)
            .packed_alpha_right(true)
            .rgb_texture_input();

        assert!(uniforms.has_packed_alpha_right());
        assert_eq!(uniforms.input_mode & VIDEO_INPUT_MODE_MASK, 1);
        assert_eq!(
            uniforms.input_mode & VIDEO_INPUT_PACKED_ALPHA_RIGHT,
            VIDEO_INPUT_PACKED_ALPHA_RIGHT,
        );
        assert!(!uniforms.packed_alpha_right(false).has_packed_alpha_right());
    }

    /// A curve shaped like the RPU's default luma mapping: two polynomial
    /// segments split at pivot 0.25, then one MMR segment of order 2.
    fn sample_dovi_metadata() -> DoviSourceMetadata {
        let mut reshaping = [DoviComponentCurve::default(); 3];
        reshaping[0].num_pivots = 4;
        reshaping[0].pivots = [0.0, 0.25, 0.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        reshaping[0].poly_coeffs[0] = [0.0, 0.5, 0.0];
        reshaping[0].poly_coeffs[1] = [1.0, 1.0, 0.5];
        reshaping[0].mmr_orders[2] = 2;
        reshaping[0].mmr_constants[2] = 0.25;
        reshaping[0].mmr_coeffs[2][0] = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7];
        reshaping[0].mmr_coeffs[2][1] = [0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.4];
        DoviSourceMetadata {
            reshaping,
            nonlinear_matrix: RgbMatrix::new([
                [1.0, 0.5, 0.25],
                [0.75, 1.0, 0.125],
                [0.0625, 0.03125, 1.0],
            ]),
            nonlinear_offset: [0.25, 0.5, 0.5],
            rgb_to_lms: RgbMatrix::new([
                [0.356742, 0.592257, 0.051081],
                [0.156705, 0.747860, 0.095435],
                [0.0, 0.041455, 0.958545],
            ]),
            source_min_pq: 62,
            source_max_pq: 3079,
            l1: None,
        }
    }

    #[test]
    fn dovi_source_uses_per_frame_l1_peak_when_present() {
        let mut source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        source = source.dovi(Some(sample_dovi_metadata_with_l1(1500, 2200, 1800)));

        assert_eq!(
            source.nominal_peak_nits,
            pq_code_to_nits(2200).max(1.0),
            "per-frame L1 peak must replace the static RPU peak for tone mapping"
        );
        // The static mastering display metadata keeps the L0 peak so
        // output-mode negotiation stays stable frame to frame.
        let mastering = source.hdr_metadata.unwrap().mastering_display.unwrap();
        assert_eq!(mastering.max_luminance_nits, Some(pq_code_to_nits(3079)));
    }

    #[test]
    fn dovi_source_falls_back_to_static_peak_when_l1_is_absent() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .dovi(Some(sample_dovi_metadata_with_l1(0, 0, 0)));
        assert_eq!(
            source.nominal_peak_nits,
            pq_code_to_nits(3079).max(1.0),
            "an all-zero L1 block must not replace the static RPU peak"
        );

        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .dovi(Some(sample_dovi_metadata()));
        assert_eq!(source.nominal_peak_nits, pq_code_to_nits(3079).max(1.0));
    }

    fn sample_dovi_metadata_with_l1(min_pq: u16, max_pq: u16, avg_pq: u16) -> DoviSourceMetadata {
        let mut metadata = sample_dovi_metadata();
        metadata.l1 = Some(DoviFramePq {
            min_pq,
            max_pq,
            avg_pq,
        });
        metadata
    }

    /// 12-bit PQ code for an absolute luminance, for building RPU L1 blocks.
    fn pq_code_12(nits: f32) -> u16 {
        (pq_code_from_nits(nits) * 4095.0)
            .round()
            .clamp(0.0, 4095.0) as u16
    }

    #[test]
    fn dovi_uniforms_are_disabled_without_metadata() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        assert_eq!(
            DoviUniforms::of_for_representation(&source, false),
            DoviUniforms::disabled()
        );
        assert_eq!(DoviUniforms::disabled().flags[0], 0.0);

        let target = TargetColorState::sdr(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);
        let uniforms = VideoUniforms::from_pipeline(&pipeline, false, false).dovi;

        assert_eq!(uniforms, DoviUniforms::disabled());
        assert_eq!(uniforms.flags[0], 0.0);
    }

    #[test]
    fn dovi_uniforms_pack_pivots_poly_and_mmr() {
        let metadata = sample_dovi_metadata();
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .dovi(Some(metadata));
        let uniforms = DoviUniforms::of_for_representation(&source, true);

        assert_eq!(uniforms.flags, [1.0, 3.0, 0.0, 0.0]);
        // Interior pivots skip the endpoints; padding gets the sentinel.
        assert_eq!(
            uniforms.pivots[0],
            [0.25, 0.5, DOVI_PIVOT_SENTINEL, DOVI_PIVOT_SENTINEL]
        );
        assert_eq!(uniforms.pivots[1], [DOVI_PIVOT_SENTINEL; 4]);
        assert_eq!(uniforms.bounds[0], [0.0, 1.0, 0.0, 0.0]);
        assert_eq!(uniforms.coefficients[0], [0.0, 0.5, 0.0, 0.0]);
        assert_eq!(uniforms.coefficients[1], [1.0, 1.0, 0.5, 0.0]);
        // MMR rows start after two polynomial segments; the order rides in w.
        assert_eq!(uniforms.coefficients[2], [0.25, 0.0, 0.0, 2.0]);
        assert_eq!(uniforms.mmr[0], [0.1, 0.2, 0.3, 0.0]);
        assert_eq!(uniforms.mmr[1], [0.4, 0.5, 0.6, 0.7]);
        assert_eq!(uniforms.mmr[2], [0.8, 0.9, 1.0, 0.0]);
        assert_eq!(uniforms.mmr[3], [1.1, 1.2, 1.3, 1.4]);
        assert_eq!(uniforms.mmr[4], [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn dovi_uniforms_apply_signal_offset_correction() {
        let metadata = sample_dovi_metadata();
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .dovi(Some(metadata));
        let uniforms = DoviUniforms::of_for_representation(&source, true);
        let correction = 1024.0_f32 / 1023.0;

        assert!((uniforms.nonlinear_offset[0] - 0.25 * correction).abs() < 1e-6);
        assert!((uniforms.nonlinear_offset[1] - 0.5 * correction).abs() < 1e-6);
        assert_eq!(uniforms.nonlinear_matrix[0], [1.0, 0.5, 0.25, 0.0]);
    }

    #[test]
    fn dovi_uniforms_use_the_uploaded_sample_depth_for_offsets() {
        let metadata = sample_dovi_metadata();
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .dovi(Some(metadata));
        let p010 = DoviUniforms::of_for_representation(&source, true);
        let nv12 = DoviUniforms::of_for_representation(&source, false);
        assert!((p010.nonlinear_offset[0] - 0.25 * 1024.0 / 1023.0).abs() < 1e-6);
        assert!((nv12.nonlinear_offset[0] - 0.25 * 256.0 / 255.0).abs() < 1e-6);

        let uniforms = VideoUniforms::from_pipeline(
            &VideoRenderPipeline::new(source, TargetColorState::sdr(ColorPrimaries::Bt709)),
            true,
            false,
        );
        let converted = uniforms.with_p010_representation(false);
        assert_eq!(converted.is_p010, 0);
        assert!((converted.dovi.nonlinear_offset[0] - nv12.nonlinear_offset[0]).abs() < 1e-6);
    }

    #[test]
    fn dovi_lms_matrix_composite_matches_libplacebo_default() {
        // libplacebo composites its hard-coded HPE LMS->RGB matrix with the
        // RPU's rgb_to_lms rows; for the RPU default matrix the product is
        // this near-diagonal, white-preserving transform.
        let matrix = dovi_lms_to_rgb_matrix(RgbMatrix::new([
            [5845.0 / 16384.0, 9702.0 / 16384.0, 837.0 / 16384.0],
            [2568.0 / 16384.0, 12256.0 / 16384.0, 1561.0 / 16384.0],
            [0.0, 679.0 / 16384.0, 15705.0 / 16384.0],
        ]));

        let expected = [
            [0.753741425, 0.198592403, 0.047534181],
            [0.045791140, 0.941773555, 0.012526896],
            [-0.001211792, 0.017623405, 0.983739703],
        ];
        for (row, expected_row) in matrix.rows().iter().zip(expected) {
            for (value, expected_value) in row.iter().zip(expected_row) {
                assert!((value - expected_value).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn dovi_source_uses_rpu_peak_and_bt2020_primaries() {
        let metadata = sample_dovi_metadata();
        let source = SourceColorState::new(ColorPrimaries::DisplayP3, TransferFunction::Pq)
            .hdr_metadata(Some(HdrMetadata::new(
                Some(MasteringDisplayMetadata {
                    display_primaries: None,
                    white_point: None,
                    min_luminance_nits: Some(0.005),
                    max_luminance_nits: Some(4000.0),
                }),
                None,
            )))
            .dovi(Some(metadata));

        // PQ code 3079 is the 12-bit encoding of ~1000 nits.
        assert!((source.nominal_peak_nits - 1000.0).abs() < 5.0);
        assert!(
            (source
                .hdr_metadata
                .unwrap()
                .mastering_display
                .unwrap()
                .min_luminance_nits
                .unwrap()
                - 0.005)
                .abs()
                < 0.0001
        );
        assert_eq!(source.primaries, ColorPrimaries::Bt2020);
        assert!(source.is_hdr());
        assert_eq!(pq_code_to_nits(0), 0.0);
    }

    #[test]
    fn dovi_source_peak_falls_back_to_pq_default_when_rpu_max_pq_is_zero() {
        let mut metadata = sample_dovi_metadata();
        metadata.source_max_pq = 0;
        let source = SourceColorState::new(ColorPrimaries::Unknown, TransferFunction::Unknown)
            .dovi(Some(metadata));

        assert_eq!(source.transfer, TransferFunction::Pq);
        assert_eq!(source.reference_white_nits, 203.0);
        assert_eq!(source.nominal_peak_nits, 1000.0);
        assert!(source.nominal_peak_nits > source.reference_white_nits);
    }

    #[test]
    fn dovi_source_forces_pq_when_stream_tags_are_missing() {
        // libplacebo forces BT.2020/PQ from the RPU because P5/P8 VUI tags are
        // unreliable; without this an unspecified trc would decode the
        // reshaped PQ signal with an sRGB gamma.
        let source = SourceColorState::new(ColorPrimaries::Unknown, TransferFunction::Unknown)
            .dovi(Some(sample_dovi_metadata()));

        assert_eq!(source.transfer, TransferFunction::Pq);
        assert_eq!(source.primaries, ColorPrimaries::Bt2020);
        assert_eq!(source.reference_white_nits, 203.0);
        assert_eq!(transfer_code(source.transfer), 3);
        assert!(source.is_hdr());
    }

    #[test]
    fn dovi_source_adds_reshape_pass_and_tone_maps_to_sdr() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .dovi(Some(sample_dovi_metadata()));
        let pipeline =
            VideoRenderPipeline::new(source, TargetColorState::sdr(ColorPrimaries::Bt709));

        assert!(pipeline.graph.contains(RenderPassKind::DoviReshape));
        assert!(pipeline.requires_tone_mapping());
        assert!(pipeline.requires_gamut_mapping());

        let pipeline =
            VideoRenderPipeline::new(source, TargetColorState::hdr10(ColorPrimaries::Bt2020));
        assert!(!pipeline.requires_tone_mapping());
    }

    #[test]
    fn dovi_formulas_are_present_across_video_shaders() {
        let shaders = [
            include_str!("wgpu_video.wgsl"),
            include_str!("metal/apple.rs"),
            include_str!("d3d11.rs"),
        ];
        for shader in shaders {
            assert!(shader.contains("dovi_flags"));
            assert!(shader.contains("dovi_pivots"));
            assert!(shader.contains("dovi_bounds"));
            assert!(shader.contains("dovi_coefficients"));
            assert!(shader.contains("dovi_mmr"));
            assert!(shader.contains("dovi_nonlinear_matrix"));
            assert!(shader.contains("dovi_nonlinear_offset"));
            assert!(shader.contains("dovi_lms_matrix"));
            assert!(shader.contains("dovi_reshaped_signal"));
            assert!(shader.contains("dovi_signal_to_pq_rgb"));
            assert!(shader.contains("dovi_lms_to_rgb"));
        }
    }

    #[test]
    fn dovi_sdr_target_configures_bt709_output_matrix() {
        let metadata = sample_dovi_metadata();
        let source = SourceColorState::default().dovi(Some(metadata));
        let target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let pipeline = VideoRenderPipeline::new(source, target);
        let uniforms = VideoUniforms::from_pipeline(&pipeline, false, false);

        // Verify that ipt_matrix_rows[6..8] are properly configured to convert
        // LMS directly to the display target primaries (BT.709), matching libplacebo.
        let lms_to_target_r = [
            uniforms.ipt_matrix_rows[6][0],
            uniforms.ipt_matrix_rows[6][1],
            uniforms.ipt_matrix_rows[6][2],
        ];
        // Target is BT.709: row 6 dot [1, 1, 1] should equal 1.0 (white point preservation)
        let white_sum = lms_to_target_r[0] + lms_to_target_r[1] + lms_to_target_r[2];
        assert!(
            (white_sum - 1.0).abs() < 1e-3,
            "White point sum was {white_sum}"
        );
    }

    fn assert_matrix_close(actual: [[f32; 3]; 3], expected: [[f32; 3]; 3], epsilon: f32) {
        for row in 0..3 {
            for col in 0..3 {
                assert!(
                    (actual[row][col] - expected[row][col]).abs() <= epsilon,
                    "matrix[{row}][{col}] expected {}, got {}",
                    expected[row][col],
                    actual[row][col]
                );
            }
        }
    }
}
