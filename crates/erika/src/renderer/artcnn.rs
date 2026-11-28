//! Platform-neutral ArtCNN model data, layout and execution policy.
//!
//! GPU backends own their shaders, resources and dispatch logic, while this
//! module owns the serialized C-series contract shared by those backends.

use crate::core::{PlayerError, Result};
use crate::renderer::pipeline::LumaUpscalerMode;

const BLOB_C4F16: &[u8] = include_bytes!("../../assets/artcnn/artcnn_c4f16.bin");
const BLOB_C4F16_DS: &[u8] = include_bytes!("../../assets/artcnn/artcnn_c4f16_ds.bin");
const BLOB_C4F32: &[u8] = include_bytes!("../../assets/artcnn/artcnn_c4f32.bin");

pub const BLOB_MAGIC: u32 = 0x4E4E_4341; // "ACNN"
pub const BLOB_VERSION: u32 = 1;
pub const BLOB_HEADER_BYTES: usize = 16;
pub const FEATURE_SLICE_WIDTH: usize = 4;
pub const CONVOLUTION_TAPS: usize = 9;
pub const MIDDLE_LAYER_COUNT: usize = 5;

const HALF4_BYTES: usize = 8;

/// Header stored at the start of every liberika ArtCNN weights blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtCnnBlobHeader {
    pub magic: u32,
    pub version: u32,
    pub feature_count: u32,
    pub reserved: u32,
}

impl ArtCnnBlobHeader {
    fn parse(blob: &[u8]) -> Result<Self> {
        if blob.len() < BLOB_HEADER_BYTES {
            return Err(unexpected_header());
        }
        let header = Self {
            magic: read_u32(blob, 0),
            version: read_u32(blob, 4),
            feature_count: read_u32(blob, 8),
            reserved: read_u32(blob, 12),
        };
        if header.magic != BLOB_MAGIC || header.version != BLOB_VERSION || header.reserved != 0 {
            return Err(unexpected_header());
        }
        Ok(header)
    }
}

/// Per-layer offsets into the canonical weights payload, in `half4` units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerOffsets {
    pub conv0_w: u32,
    pub conv0_b: u32,
    pub mid_w: [u32; MIDDLE_LAYER_COUNT],
    pub mid_b: [u32; MIDDLE_LAYER_COUNT],
    pub conv6_w: u32,
    pub conv6_b: u32,
}

impl LayerOffsets {
    pub fn for_slices(slices: u32) -> Self {
        let mut cursor = 0u32;
        let mut take = |len: u32| {
            let offset = cursor;
            cursor += len;
            offset
        };
        let conv0_w = take(slices * CONVOLUTION_TAPS as u32);
        let conv0_b = take(slices);
        let mut mid_w = [0u32; MIDDLE_LAYER_COUNT];
        let mut mid_b = [0u32; MIDDLE_LAYER_COUNT];
        for layer in 0..MIDDLE_LAYER_COUNT {
            mid_w[layer] = take(slices * CONVOLUTION_TAPS as u32 * slices * 4);
            mid_b[layer] = take(slices);
        }
        let conv6_w = take(CONVOLUTION_TAPS as u32 * slices * 4);
        let conv6_b = take(1);
        Self {
            conv0_w,
            conv0_b,
            mid_w,
            mid_b,
            conv6_w,
            conv6_b,
        }
    }

    pub fn total_half4(slices: u32) -> usize {
        let slices = slices as usize;
        slices * CONVOLUTION_TAPS
            + slices
            + MIDDLE_LAYER_COUNT
                * (slices * CONVOLUTION_TAPS * slices * FEATURE_SLICE_WIDTH + slices)
            + CONVOLUTION_TAPS * slices * FEATURE_SLICE_WIDTH
            + 1
    }
}

/// Canonical C-series tensor layout represented by one weights blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtCnnModelLayout {
    pub feature_count: usize,
    pub feature_slices: u32,
    pub layer_offsets: LayerOffsets,
    pub payload_half4: usize,
}

impl ArtCnnModelLayout {
    pub fn for_mode(mode: LumaUpscalerMode) -> Option<Self> {
        let feature_count = match mode {
            LumaUpscalerMode::ArtCnnC4F16 | LumaUpscalerMode::ArtCnnC4F16Ds => 16,
            LumaUpscalerMode::ArtCnnC4F32 => 32,
            _ => return None,
        };
        let feature_slices = (feature_count / FEATURE_SLICE_WIDTH) as u32;
        Some(Self {
            feature_count,
            feature_slices,
            layer_offsets: LayerOffsets::for_slices(feature_slices),
            payload_half4: LayerOffsets::total_half4(feature_slices),
        })
    }

    pub fn payload_bytes(self) -> usize {
        self.payload_half4 * HALF4_BYTES
    }
}

/// A validated view of one canonical ArtCNN model blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtCnnModel<'a> {
    pub mode: LumaUpscalerMode,
    pub header: ArtCnnBlobHeader,
    pub layout: ArtCnnModelLayout,
    pub payload: &'a [u8],
}

/// Selects and validates the embedded model for `mode`.
pub fn model_for_mode(mode: LumaUpscalerMode) -> Result<ArtCnnModel<'static>> {
    let blob = match mode {
        LumaUpscalerMode::ArtCnnC4F16 => Some(BLOB_C4F16),
        LumaUpscalerMode::ArtCnnC4F16Ds => Some(BLOB_C4F16_DS),
        LumaUpscalerMode::ArtCnnC4F32 => Some(BLOB_C4F32),
        _ => None,
    }
    .ok_or_else(no_weights)?;
    parse_model_blob(mode, blob)
}

/// Parses a blob against the layout selected by `mode`.
///
/// Keeping this independent of an embedded asset lets every GPU backend use
/// the same validation before allocating native resources.
pub fn parse_model_blob(mode: LumaUpscalerMode, blob: &[u8]) -> Result<ArtCnnModel<'_>> {
    let layout = ArtCnnModelLayout::for_mode(mode).ok_or_else(no_weights)?;
    let header = ArtCnnBlobHeader::parse(blob)?;
    if header.feature_count as usize != layout.feature_count {
        return Err(PlayerError::Renderer(format!(
            "ArtCNN weights blob feature count {} does not match mode {mode:?}",
            header.feature_count
        )));
    }
    let payload = &blob[BLOB_HEADER_BYTES..];
    let expected_bytes = layout.payload_bytes();
    if payload.len() != expected_bytes {
        return Err(PlayerError::Renderer(format!(
            "ArtCNN weights blob payload is {} bytes, expected {expected_bytes}",
            payload.len()
        )));
    }
    Ok(ArtCnnModel {
        mode,
        header,
        layout,
        payload,
    })
}

/// Backend-independent tuning parameters for the current ArtCNN model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtCnnExecutionPolicy {
    /// Source pixels produced by one scalar-kernel thread.
    pub scalar_block: (usize, usize),
    /// Eight-pixel fragments processed by one matrix-kernel strip.
    pub matrix_pixel_fragments: usize,
}

/// Process-level backend override shared by native ArtCNN implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtCnnBackendOverride {
    Scalar,
    Matrix,
}

impl ArtCnnBackendOverride {
    /// Reads `ERIKA_SR_BACKEND=scalar|matmul` using the historical spelling.
    pub fn from_environment() -> Option<Self> {
        std::env::var("ERIKA_SR_BACKEND")
            .ok()
            .as_deref()
            .and_then(Self::parse)
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "scalar" => Some(Self::Scalar),
            "matmul" => Some(Self::Matrix),
            _ => None,
        }
    }
}

impl ArtCnnExecutionPolicy {
    /// Resolves defaults plus the existing process-level tuning overrides.
    pub fn for_mode(mode: LumaUpscalerMode) -> Self {
        let scalar_block = std::env::var("ERIKA_SR_BLOCK").ok();
        let matrix_pixel_fragments = std::env::var("ERIKA_SR_PXF").ok();
        Self::from_overrides(
            mode,
            scalar_block.as_deref(),
            matrix_pixel_fragments.as_deref(),
        )
    }

    fn from_overrides(
        mode: LumaUpscalerMode,
        scalar_block: Option<&str>,
        matrix_pixel_fragments: Option<&str>,
    ) -> Self {
        let default_scalar_block = match mode {
            LumaUpscalerMode::ArtCnnC4F16 | LumaUpscalerMode::ArtCnnC4F16Ds => (2, 2),
            LumaUpscalerMode::ArtCnnC4F32 => (2, 1),
            _ => (1, 1),
        };
        Self {
            scalar_block: scalar_block
                .and_then(parse_scalar_block)
                .unwrap_or(default_scalar_block),
            matrix_pixel_fragments: matrix_pixel_fragments
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| (1..=8).contains(value))
                .unwrap_or(8),
        }
    }
}

fn parse_scalar_block(value: &str) -> Option<(usize, usize)> {
    let (x, y) = value.split_once('x')?;
    let (x, y) = (x.parse::<usize>().ok()?, y.parse::<usize>().ok()?);
    ((1..=4).contains(&x) && (1..=4).contains(&y)).then_some((x, y))
}

/// Tracks whether an upscaled output already belongs to a decoded frame.
///
/// `None` intentionally never caches: callers without a stable token retain
/// the old behavior of recomputing every presentation tick.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrameTokenCache {
    cached_token: Option<u64>,
}

impl FrameTokenCache {
    /// Returns `true` when the existing output belongs to `frame_token`.
    /// `None` is never considered a stable cache key.
    pub fn matches(&self, frame_token: Option<u64>) -> bool {
        frame_token.is_some() && frame_token == self.cached_token
    }

    /// Associates a successfully encoded output with `frame_token`.
    pub fn commit(&mut self, frame_token: Option<u64>) {
        self.cached_token = frame_token;
    }

    pub fn invalidate(&mut self) {
        self.cached_token = None;
    }
}

fn read_u32(blob: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        blob[offset..offset + 4]
            .try_into()
            .expect("validated header"),
    )
}

fn unexpected_header() -> PlayerError {
    PlayerError::Renderer("ArtCNN weights blob has an unexpected header".to_string())
}

fn no_weights() -> PlayerError {
    PlayerError::Renderer("upscaler mode has no weights".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_models_match_the_canonical_layout() {
        let c4f16 = model_for_mode(LumaUpscalerMode::ArtCnnC4F16).unwrap();
        assert_eq!(c4f16.header.feature_count, 16);
        assert_eq!(c4f16.layout.feature_slices, 4);
        assert_eq!(c4f16.layout.payload_half4, 3_085);
        assert_eq!(c4f16.payload.len(), 24_680);
        assert_eq!(
            c4f16.layout.layer_offsets,
            LayerOffsets {
                conv0_w: 0,
                conv0_b: 36,
                mid_w: [40, 620, 1_200, 1_780, 2_360],
                mid_b: [616, 1_196, 1_776, 2_356, 2_936],
                conv6_w: 2_940,
                conv6_b: 3_084,
            }
        );

        let c4f16_ds = model_for_mode(LumaUpscalerMode::ArtCnnC4F16Ds).unwrap();
        assert_eq!(c4f16_ds.header.feature_count, 16);
        assert_eq!(c4f16_ds.layout, c4f16.layout);
        assert_eq!(c4f16_ds.payload.len(), 24_680);
        assert_ne!(c4f16_ds.payload, c4f16.payload);

        let c4f32 = model_for_mode(LumaUpscalerMode::ArtCnnC4F32).unwrap();
        assert_eq!(c4f32.header.feature_count, 32);
        assert_eq!(c4f32.layout.feature_slices, 8);
        assert_eq!(c4f32.layout.payload_half4, 11_929);
        assert_eq!(c4f32.payload.len(), 95_432);
        assert_eq!(c4f32.layout.layer_offsets.conv6_w, 11_640);
        assert_eq!(c4f32.layout.layer_offsets.conv6_b, 11_928);
    }

    #[test]
    fn blob_validation_rejects_each_incompatible_contract() {
        let valid = BLOB_C4F16;
        assert_eq!(
            parse_model_blob(LumaUpscalerMode::ArtCnnC4F16, &valid[..12]).unwrap_err(),
            unexpected_header()
        );

        let mut blob = valid.to_vec();
        blob[0] ^= 0xff;
        assert_eq!(
            parse_model_blob(LumaUpscalerMode::ArtCnnC4F16, &blob).unwrap_err(),
            unexpected_header()
        );

        let mut blob = valid.to_vec();
        blob[4..8].copy_from_slice(&(BLOB_VERSION + 1).to_le_bytes());
        assert_eq!(
            parse_model_blob(LumaUpscalerMode::ArtCnnC4F16, &blob).unwrap_err(),
            unexpected_header()
        );

        let mut blob = valid.to_vec();
        blob[12..16].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            parse_model_blob(LumaUpscalerMode::ArtCnnC4F16, &blob).unwrap_err(),
            unexpected_header()
        );

        let mut blob = valid.to_vec();
        blob[8..12].copy_from_slice(&32u32.to_le_bytes());
        let error = parse_model_blob(LumaUpscalerMode::ArtCnnC4F16, &blob).unwrap_err();
        assert!(error.to_string().contains("feature count 32"));

        let error = parse_model_blob(
            LumaUpscalerMode::ArtCnnC4F16,
            &valid[..valid.len() - HALF4_BYTES],
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("payload is 24672 bytes, expected 24680")
        );

        assert_eq!(
            model_for_mode(LumaUpscalerMode::Off).unwrap_err(),
            no_weights()
        );
    }

    #[test]
    fn execution_policy_preserves_defaults_and_bounded_overrides() {
        let c4f16 =
            ArtCnnExecutionPolicy::from_overrides(LumaUpscalerMode::ArtCnnC4F16, None, None);
        assert_eq!(c4f16.scalar_block, (2, 2));
        assert_eq!(c4f16.matrix_pixel_fragments, 8);

        let tuned = ArtCnnExecutionPolicy::from_overrides(
            LumaUpscalerMode::ArtCnnC4F32,
            Some("4x3"),
            Some("5"),
        );
        assert_eq!(tuned.scalar_block, (4, 3));
        assert_eq!(tuned.matrix_pixel_fragments, 5);

        let rejected = ArtCnnExecutionPolicy::from_overrides(
            LumaUpscalerMode::ArtCnnC4F32,
            Some("5x2"),
            Some("0"),
        );
        assert_eq!(rejected.scalar_block, (2, 1));
        assert_eq!(rejected.matrix_pixel_fragments, 8);

        assert_eq!(
            ArtCnnBackendOverride::parse("scalar"),
            Some(ArtCnnBackendOverride::Scalar)
        );
        assert_eq!(
            ArtCnnBackendOverride::parse("matmul"),
            Some(ArtCnnBackendOverride::Matrix)
        );
        assert_eq!(ArtCnnBackendOverride::parse("automatic"), None);
    }

    #[test]
    fn frame_token_cache_only_reuses_stable_some_tokens() {
        let mut cache = FrameTokenCache::default();
        assert!(!cache.matches(None));
        assert!(!cache.matches(Some(7)));

        // A failed encode does not change the key because commit is explicit.
        assert!(!cache.matches(Some(7)));
        cache.commit(Some(7));
        assert!(cache.matches(Some(7)));
        assert!(!cache.matches(Some(8)));

        cache.commit(Some(8));
        assert!(cache.matches(Some(8)));
        assert!(!cache.matches(None));
        cache.commit(None);
        assert!(!cache.matches(None));
        assert!(!cache.matches(Some(8)));

        cache.invalidate();
        assert!(!cache.matches(Some(8)));
    }
}
