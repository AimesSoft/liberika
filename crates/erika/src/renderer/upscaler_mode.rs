//! Video super-resolution mode identifiers shared by native and WebAssembly builds.

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

    /// Stable identifier used in diagnostics, logs and cross-SDK configs.
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

    /// Parses a mode label (case-insensitive) or C ABI numeric code (`ErikaLumaUpscalerMode`).
    pub fn parse(input: &str) -> Option<Self> {
        let trimmed = input.trim();
        if let Ok(code) = trimmed.parse::<i32>() {
            return Self::from_code(code);
        }
        let lower = trimmed.to_ascii_lowercase();
        Self::ALL.into_iter().find(|mode| mode.label() == lower)
    }

    /// Matches the numeric values of `ErikaLumaUpscalerMode` in `erika.h`.
    pub fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            0 => Self::Off,
            1 => Self::ArtCnnC4F16,
            2 => Self::ArtCnnC4F32,
            3 => Self::ArtCnnC4F16Ds,
            4 => Self::ArtCnnC4F16Dn,
            5 => Self::ArtCnnC4F32Ds,
            6 => Self::ArtCnnC4F32Dn,
            7 => Self::ArtCnnR8F64,
            16 => Self::Anime4kS,
            17 => Self::Anime4kM,
            18 => Self::Anime4kL,
            19 => Self::Anime4kVl,
            20 => Self::Anime4kUl,
            21 => Self::Anime4kDenoiseS,
            22 => Self::Anime4kDenoiseM,
            23 => Self::Anime4kDenoiseL,
            24 => Self::Anime4kDenoiseVl,
            25 => Self::Anime4kDenoiseUl,
            32 => Self::Fsr1,
            48 => Self::PlatformVideo,
            49 => Self::MetalFxSpatial,
            64 => Self::Custom,
            _ => return None,
        })
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
