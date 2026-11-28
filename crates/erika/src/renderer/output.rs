use crate::core::{ColorPrimaries, TransferFunction};
use crate::renderer::pipeline::{SourceColorState, TargetColorState};

/// Platform-neutral output request selected by the embedder.
///
/// `ExtendedLinear` means a floating-point, linear-light presentation path
/// whose values above `1.0` carry display headroom. Each backend must still
/// negotiate a matching native surface/color space; otherwise it must render
/// through the SDR description and report the fallback explicitly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputMode {
    Sdr,
    /// Legacy Apple API spelling retained for ABI compatibility.
    AppleEdr {
        headroom: f32,
    },
    ExtendedLinear {
        headroom: f32,
    },
    /// Selects SDR or Apple EDR from the decoded source color state. The
    /// renderer starts with an 8-bit SDR surface and promotes it only for a
    /// genuine HDR source (PQ, HLG, or HDR luminance metadata).
    Auto {
        headroom: f32,
    },
}

impl OutputMode {
    pub fn extended_linear(headroom: f32) -> Self {
        Self::ExtendedLinear {
            headroom: normalized_headroom(headroom),
        }
    }

    /// Backwards-compatible spelling for the original Apple-only C/Dart API.
    pub fn apple_edr(headroom: f32) -> Self {
        Self::AppleEdr {
            headroom: normalized_headroom(headroom),
        }
    }

    pub fn auto(headroom: f32) -> Self {
        Self::Auto {
            headroom: normalized_headroom(headroom),
        }
    }

    pub fn resolve_for_source(self, source_is_hdr: bool) -> Self {
        match self {
            Self::Auto { headroom } if source_is_hdr && headroom > 1.0 => Self::apple_edr(headroom),
            Self::Auto { .. } => Self::Sdr,
            explicit => explicit,
        }
    }

    pub fn is_edr(self) -> bool {
        matches!(self, Self::AppleEdr { .. } | Self::ExtendedLinear { .. })
    }

    pub fn is_android_extended_linear(self) -> bool {
        matches!(self, Self::ExtendedLinear { .. })
    }

    pub fn headroom(self) -> f32 {
        match self {
            Self::Sdr => 1.0,
            Self::AppleEdr { headroom }
            | Self::ExtendedLinear { headroom }
            | Self::Auto { headroom } => normalized_headroom(headroom),
        }
    }
}

impl Default for OutputMode {
    fn default() -> Self {
        Self::Sdr
    }
}

/// Clamp a resolved output mode to what the attached display can actually
/// present.
///
/// A compositor cannot give an EDR/PQ layer meaningful headroom on a display
/// without EDR support: the HDR signal gets forced into the SDR range and the
/// picture looks blown out. HDR sources therefore tone map to SDR there, and
/// EDR requests are capped at the display's real headroom.
pub fn clamp_output_mode_to_display(mode: OutputMode, display_headroom: f32) -> OutputMode {
    match mode {
        OutputMode::AppleEdr { headroom } => {
            if display_headroom <= 1.05 {
                OutputMode::Sdr
            } else {
                OutputMode::apple_edr(headroom.min(display_headroom))
            }
        }
        OutputMode::ExtendedLinear { headroom } => {
            OutputMode::extended_linear(headroom.min(display_headroom.max(1.0)))
        }
        other => other,
    }
}

/// Negotiate the output mode for a source against the display the playback
/// window is presented on.
///
/// `Auto` is the per-screen negotiation path: an HDR source promotes to EDR
/// using the presenting display's *real* headroom when the screen can present
/// EDR at all, and stays on the SDR tone mapping path when it cannot — so the
/// same embedder configuration yields HDR on an HDR display and correctly
/// tone-mapped SDR on a non-HDR one. The embedder's configured headroom (when
/// set above SDR white) caps the negotiated value. Explicit EDR/ExtendedLinear
/// requests are clamped to the display's capability instead.
pub fn negotiate_output_mode(
    requested: OutputMode,
    source_is_hdr: bool,
    display_headroom: f32,
) -> OutputMode {
    match requested {
        OutputMode::Auto { headroom } => {
            if !source_is_hdr || display_headroom <= 1.05 {
                OutputMode::Sdr
            } else {
                let cap = if headroom > 1.0 { headroom } else { f32::MAX };
                OutputMode::apple_edr(display_headroom.min(cap))
            }
        }
        other => clamp_output_mode_to_display(other, display_headroom),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputColorSpace {
    Srgb,
    ExtendedSrgbLinear,
    Bt2020Pq,
}

impl OutputColorSpace {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Srgb => "srgb",
            Self::ExtendedSrgbLinear => "extended-srgb-linear",
            Self::Bt2020Pq => "bt2020-pq",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSurfaceFormat {
    EightBitUnorm,
    TenBitUnorm,
    SixteenBitFloat,
}

impl Default for OutputSurfaceFormat {
    fn default() -> Self {
        Self::EightBitUnorm
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActiveOutputEncoding {
    #[default]
    SdrSrgb,
    AppleEdr,
    AndroidExtendedLinearScRgb,
    Hdr10Pq,
    LinuxExtendedLinearScRgb,
}

impl ActiveOutputEncoding {
    pub const fn label(self) -> &'static str {
        match self {
            Self::SdrSrgb => "sdr-srgb",
            Self::AppleEdr => "apple-edr",
            Self::AndroidExtendedLinearScRgb => "android-extended-linear-scrgb",
            Self::Hdr10Pq => "hdr10-pq",
            Self::LinuxExtendedLinearScRgb => "linux-extended-linear-scrgb",
        }
    }
}

/// Stable reason code explaining why a requested output mode is not active.
///
/// These values are part of the C/Dart ABI. Add new reasons at the end and do
/// not renumber existing variants.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFallbackReason {
    #[default]
    None = 0,
    DisplayHdrUnsupported = 1,
    HybridCompositionRequired = 2,
    WgpuBackendNotVulkan = 3,
    Rgba16FloatSurfaceFormatUnavailable = 4,
    NativeWindowDataSpaceApiUnavailable = 5,
    ScrgbDataSpaceVerificationFailed = 6,
    SurfaceConfigureFailed = 7,
    LegacyAppleEdrUnsupported = 8,
}

impl OutputFallbackReason {
    pub const fn from_raw(value: i32) -> Self {
        match value {
            1 => Self::DisplayHdrUnsupported,
            2 => Self::HybridCompositionRequired,
            3 => Self::WgpuBackendNotVulkan,
            4 => Self::Rgba16FloatSurfaceFormatUnavailable,
            5 => Self::NativeWindowDataSpaceApiUnavailable,
            6 => Self::ScrgbDataSpaceVerificationFailed,
            7 => Self::SurfaceConfigureFailed,
            8 => Self::LegacyAppleEdrUnsupported,
            _ => Self::None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::DisplayHdrUnsupported => "display_hdr_unsupported",
            Self::HybridCompositionRequired => "hybrid_composition_required",
            Self::WgpuBackendNotVulkan => "wgpu_backend_not_vulkan",
            Self::Rgba16FloatSurfaceFormatUnavailable => "rgba16float_surface_format_unavailable",
            Self::NativeWindowDataSpaceApiUnavailable => "native_window_dataspace_api_unavailable",
            Self::ScrgbDataSpaceVerificationFailed => "scrgb_dataspace_verification_failed",
            Self::SurfaceConfigureFailed => "surface_configure_failed",
            Self::LegacyAppleEdrUnsupported => "legacy_apple_edr_unsupported",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputRuntimeStatus {
    pub requested_mode: OutputMode,
    pub active_encoding: ActiveOutputEncoding,
    pub surface_format: OutputSurfaceFormat,
    pub native_data_space: i32,
    pub requested_headroom: f32,
    pub active_headroom: f32,
    pub active_headroom_known: bool,
    pub extended_linear_active: bool,
    pub fallback_reason: OutputFallbackReason,
    pub fallback_count: u64,
    pub data_space_failures: u64,
    pub headroom_updates: u64,
    pub extended_linear_frames: u64,
}

impl OutputRuntimeStatus {
    pub fn requested(mode: OutputMode) -> Self {
        Self {
            requested_mode: mode,
            requested_headroom: mode.headroom(),
            ..Self::default()
        }
    }
}

impl Default for OutputRuntimeStatus {
    fn default() -> Self {
        Self {
            requested_mode: OutputMode::Sdr,
            active_encoding: ActiveOutputEncoding::SdrSrgb,
            surface_format: OutputSurfaceFormat::EightBitUnorm,
            native_data_space: -1,
            requested_headroom: 1.0,
            active_headroom: 1.0,
            active_headroom_known: true,
            extended_linear_active: false,
            fallback_reason: OutputFallbackReason::None,
            fallback_count: 0,
            data_space_failures: 0,
            headroom_updates: 0,
            extended_linear_frames: 0,
        }
    }
}

impl OutputSurfaceFormat {
    pub const fn label(self) -> &'static str {
        match self {
            Self::EightBitUnorm => "8bit-unorm",
            Self::TenBitUnorm => "10bit-unorm",
            Self::SixteenBitFloat => "16bit-float",
        }
    }
}

/// Actual signal contract used by a renderer after native surface negotiation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputDescription {
    pub color_space: OutputColorSpace,
    pub surface_format: OutputSurfaceFormat,
    pub target: TargetColorState,
    pub extended_linear: bool,
}

impl OutputDescription {
    pub fn sdr() -> Self {
        Self {
            color_space: OutputColorSpace::Srgb,
            surface_format: OutputSurfaceFormat::EightBitUnorm,
            target: TargetColorState::sdr(ColorPrimaries::Bt709),
            extended_linear: false,
        }
    }

    pub fn extended_linear(headroom: f32) -> Self {
        let headroom = normalized_headroom(headroom);
        Self {
            color_space: OutputColorSpace::ExtendedSrgbLinear,
            surface_format: OutputSurfaceFormat::SixteenBitFloat,
            // Android/Vulkan EXTENDED_SRGB_LINEAR follows the scRGB convention:
            // a linear component value of 1.0 represents 80 cd/m².
            target: TargetColorState::extended_linear(ColorPrimaries::Bt709, 80.0, headroom),
            extended_linear: true,
        }
    }

    pub fn apple_edr(headroom: f32) -> Self {
        let headroom = normalized_headroom(headroom);
        Self {
            color_space: OutputColorSpace::ExtendedSrgbLinear,
            surface_format: OutputSurfaceFormat::SixteenBitFloat,
            target: TargetColorState::apple_edr(ColorPrimaries::Bt709, headroom),
            extended_linear: true,
        }
    }

    pub fn hdr10() -> Self {
        Self {
            color_space: OutputColorSpace::Bt2020Pq,
            surface_format: OutputSurfaceFormat::TenBitUnorm,
            target: TargetColorState::hdr10(ColorPrimaries::Bt2020),
            extended_linear: false,
        }
    }

    pub fn requested(mode: OutputMode) -> Self {
        match mode {
            OutputMode::Sdr => Self::sdr(),
            OutputMode::AppleEdr { headroom } => Self::apple_edr(headroom),
            OutputMode::ExtendedLinear { headroom } => Self::extended_linear(headroom),
            OutputMode::Auto { .. } => Self::sdr(),
        }
    }

    pub fn target_transfer(self) -> TransferFunction {
        self.target.transfer
    }

    /// The color target an HDR source should render against for this output.
    ///
    /// An HDR source on a plain SDR output is tone-mapped against the
    /// 203-nit HDR reference-white convention (`sdr_tone_map_target`) instead
    /// of the 100-nit SDR mastering target. Extended-linear (EDR/scRGB)
    /// outputs encode linear light above 1.0 and are *not* SDR outputs even
    /// though their transfer reads `Srgb`, so they keep their negotiated
    /// target; HDR10 PQ passthrough does its own conversion and is untouched.
    pub fn tone_map_target_for(self, source: &SourceColorState) -> TargetColorState {
        if source.is_hdr() && self.color_space == OutputColorSpace::Srgb {
            TargetColorState::sdr_tone_map_target(self.target.primaries)
        } else {
            self.target
        }
    }
}

fn normalized_headroom(headroom: f32) -> f32 {
    if headroom.is_finite() {
        headroom.clamp(1.0, 10_000.0)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_linear_description_is_float_sc_rgb() {
        let description = OutputDescription::requested(OutputMode::extended_linear(4.0));

        assert_eq!(
            description.color_space,
            OutputColorSpace::ExtendedSrgbLinear
        );
        assert_eq!(
            description.surface_format,
            OutputSurfaceFormat::SixteenBitFloat
        );
        assert_eq!(description.target.primaries, ColorPrimaries::Bt709);
        assert_eq!(description.target.transfer, TransferFunction::Srgb);
        assert_eq!(description.target.edr_headroom, 4.0);
        assert!(description.extended_linear);
    }

    #[test]
    fn tone_map_target_swaps_only_on_sdr_outputs() {
        let hdr = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let sdr = SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Srgb);

        // HDR source on an SDR output: the 203-nit tone-map target.
        let swapped = OutputDescription::sdr().tone_map_target_for(&hdr);
        assert_eq!(swapped.peak_nits, 203.0);
        assert_eq!(swapped.reference_white_nits, 203.0);

        // HDR source on extended-linear/EDR outputs: the negotiated target is
        // kept (its transfer is also Srgb, so the check must not key on it).
        for mode in [OutputMode::extended_linear(4.0), OutputMode::apple_edr(2.0)] {
            let description = OutputDescription::requested(mode);
            let kept = description.tone_map_target_for(&hdr);
            assert_eq!(kept, description.target, "mode {mode:?}");
            assert!(kept.peak_nits > 203.0);
        }

        // HDR10 PQ passthrough and SDR sources are untouched.
        let hdr10 = OutputDescription::hdr10().tone_map_target_for(&hdr);
        assert_eq!(hdr10, OutputDescription::hdr10().target);
        let sdr_target = OutputDescription::sdr().tone_map_target_for(&sdr);
        assert_eq!(sdr_target, OutputDescription::sdr().target);
    }

    #[test]
    fn output_mode_rejects_invalid_headroom() {
        assert_eq!(OutputMode::extended_linear(f32::NAN).headroom(), 1.0);
        assert_eq!(OutputMode::extended_linear(0.25).headroom(), 1.0);
        assert_eq!(OutputMode::extended_linear(20_000.0).headroom(), 10_000.0);
    }

    #[test]
    fn automatic_output_promotes_only_real_hdr_sources() {
        let automatic = OutputMode::auto(4.0);

        assert_eq!(automatic.resolve_for_source(false), OutputMode::Sdr);
        assert_eq!(
            automatic.resolve_for_source(true),
            OutputMode::apple_edr(4.0)
        );
        // An unconfigured (1.0) headroom keeps HDR sources on the SDR tone
        // mapping path; EDR promotion requires the embedder to actually
        // request headroom above SDR white.
        assert_eq!(
            OutputMode::auto(1.0).resolve_for_source(true),
            OutputMode::Sdr
        );
        assert_eq!(
            OutputMode::auto(1.0).resolve_for_source(false),
            OutputMode::Sdr
        );
        assert_eq!(
            OutputDescription::requested(automatic),
            OutputDescription::sdr()
        );
    }

    #[test]
    fn apple_and_android_extended_linear_keep_distinct_reference_white() {
        let apple = OutputDescription::requested(OutputMode::apple_edr(4.0));
        let android = OutputDescription::requested(OutputMode::extended_linear(4.0));

        assert_eq!(apple.target.reference_white_nits, 203.0);
        assert_eq!(android.target.reference_white_nits, 80.0);
        assert_eq!(apple.target.edr_headroom, 4.0);
        assert_eq!(android.target.edr_headroom, 4.0);
    }

    #[test]
    fn auto_negotiates_hdr_per_presenting_display() {
        // EDR display: an HDR source promotes to EDR using the display's real
        // headroom; an unconfigured host headroom (1.0) defers to the display.
        assert_eq!(
            negotiate_output_mode(OutputMode::auto(1.0), true, 2.0),
            OutputMode::apple_edr(2.0)
        );
        // A configured headroom above SDR white caps the negotiated value.
        assert_eq!(
            negotiate_output_mode(OutputMode::auto(1.5), true, 2.0),
            OutputMode::apple_edr(1.5)
        );
        // SDR display: HDR sources stay on the tone mapping path, however
        // large the embedder's headroom request was.
        assert_eq!(
            negotiate_output_mode(OutputMode::auto(4.0), true, 1.0),
            OutputMode::Sdr
        );
        // Non-HDR sources never promote, even on EDR displays.
        assert_eq!(
            negotiate_output_mode(OutputMode::auto(4.0), false, 2.0),
            OutputMode::Sdr
        );
    }

    #[test]
    fn edr_requests_fall_back_to_sdr_on_displays_without_edr() {
        let resolved = OutputMode::auto(4.0).resolve_for_source(true);

        assert_eq!(resolved, OutputMode::apple_edr(4.0));
        assert_eq!(clamp_output_mode_to_display(resolved, 1.0), OutputMode::Sdr);
        assert_eq!(
            clamp_output_mode_to_display(resolved, 1.04),
            OutputMode::Sdr
        );
    }

    #[test]
    fn edr_requests_are_capped_at_real_display_headroom() {
        let resolved = OutputMode::auto(4.0).resolve_for_source(true);

        assert_eq!(
            clamp_output_mode_to_display(resolved, 2.0),
            OutputMode::apple_edr(2.0)
        );
        // A request below the display's capability stays untouched.
        assert_eq!(
            clamp_output_mode_to_display(OutputMode::apple_edr(1.5), 3.0),
            OutputMode::apple_edr(1.5)
        );
    }

    #[test]
    fn extended_linear_requests_are_capped_but_survive_without_edr() {
        assert_eq!(
            clamp_output_mode_to_display(OutputMode::extended_linear(2.0), 1.0),
            OutputMode::extended_linear(1.0)
        );
        assert_eq!(
            clamp_output_mode_to_display(OutputMode::Sdr, 1.0),
            OutputMode::Sdr
        );
    }

    #[test]
    fn fallback_reason_codes_and_labels_are_stable() {
        let expected = [
            (OutputFallbackReason::None, 0, "none"),
            (
                OutputFallbackReason::DisplayHdrUnsupported,
                1,
                "display_hdr_unsupported",
            ),
            (
                OutputFallbackReason::HybridCompositionRequired,
                2,
                "hybrid_composition_required",
            ),
            (
                OutputFallbackReason::WgpuBackendNotVulkan,
                3,
                "wgpu_backend_not_vulkan",
            ),
            (
                OutputFallbackReason::Rgba16FloatSurfaceFormatUnavailable,
                4,
                "rgba16float_surface_format_unavailable",
            ),
            (
                OutputFallbackReason::NativeWindowDataSpaceApiUnavailable,
                5,
                "native_window_dataspace_api_unavailable",
            ),
            (
                OutputFallbackReason::ScrgbDataSpaceVerificationFailed,
                6,
                "scrgb_dataspace_verification_failed",
            ),
            (
                OutputFallbackReason::SurfaceConfigureFailed,
                7,
                "surface_configure_failed",
            ),
            (
                OutputFallbackReason::LegacyAppleEdrUnsupported,
                8,
                "legacy_apple_edr_unsupported",
            ),
        ];

        for (reason, code, label) in expected {
            assert_eq!(reason as i32, code);
            assert_eq!(OutputFallbackReason::from_raw(code), reason);
            assert_eq!(reason.label(), label);
        }
    }

    #[test]
    fn hdr10_description_remains_distinct_from_extended_linear() {
        let description = OutputDescription::hdr10();

        assert_eq!(description.color_space, OutputColorSpace::Bt2020Pq);
        assert_eq!(description.surface_format, OutputSurfaceFormat::TenBitUnorm);
        assert_eq!(description.target.transfer, TransferFunction::Pq);
        assert!(!description.extended_linear);
    }
}
