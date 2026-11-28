//! Bundled networks and the mode -> model mapping shared by all backends.

use std::sync::{Arc, Mutex, OnceLock};

use super::fold;
use super::model::SrModel;
use crate::core::{PlayerError, Result};
use crate::renderer::artcnn::model_for_mode;
use crate::renderer::pipeline::{LumaUpscalerMode, UpscalerFamily};

/// Anime4K shader paths (relative to bloc97/Anime4K's `glsl/`) and the asset
/// each converts to. See `assets/anime4k/README.md`.
pub const ANIME4K_SOURCES: [(&str, &str); 10] = [
    (
        "Upscale/Anime4K_Upscale_CNN_x2_S.glsl",
        "anime4k_upscale_s.esr",
    ),
    (
        "Upscale/Anime4K_Upscale_CNN_x2_M.glsl",
        "anime4k_upscale_m.esr",
    ),
    (
        "Upscale/Anime4K_Upscale_CNN_x2_L.glsl",
        "anime4k_upscale_l.esr",
    ),
    (
        "Upscale/Anime4K_Upscale_CNN_x2_VL.glsl",
        "anime4k_upscale_vl.esr",
    ),
    (
        "Upscale/Anime4K_Upscale_CNN_x2_UL.glsl",
        "anime4k_upscale_ul.esr",
    ),
    (
        "Upscale+Denoise/Anime4K_Upscale_Denoise_CNN_x2_S.glsl",
        "anime4k_upscale_denoise_s.esr",
    ),
    (
        "Upscale+Denoise/Anime4K_Upscale_Denoise_CNN_x2_M.glsl",
        "anime4k_upscale_denoise_m.esr",
    ),
    (
        "Upscale+Denoise/Anime4K_Upscale_Denoise_CNN_x2_L.glsl",
        "anime4k_upscale_denoise_l.esr",
    ),
    (
        "Upscale+Denoise/Anime4K_Upscale_Denoise_CNN_x2_VL.glsl",
        "anime4k_upscale_denoise_vl.esr",
    ),
    (
        "Upscale+Denoise/Anime4K_Upscale_Denoise_CNN_x2_UL.glsl",
        "anime4k_upscale_denoise_ul.esr",
    ),
];

fn bundled_asset(mode: LumaUpscalerMode) -> Option<&'static [u8]> {
    Some(match mode {
        LumaUpscalerMode::ArtCnnC4F16Dn => {
            include_bytes!("../../../assets/artcnn/artcnn_c4f16_dn.esr")
        }
        LumaUpscalerMode::ArtCnnC4F32Ds => {
            include_bytes!("../../../assets/artcnn/artcnn_c4f32_ds.esr")
        }
        LumaUpscalerMode::ArtCnnC4F32Dn => {
            include_bytes!("../../../assets/artcnn/artcnn_c4f32_dn.esr")
        }
        #[cfg(all(
            feature = "artcnn-r8f64",
            any(target_os = "windows", target_os = "macos", target_os = "linux")
        ))]
        LumaUpscalerMode::ArtCnnR8F64 => include_bytes!("../../../assets/artcnn/artcnn_r8f64.esr"),
        LumaUpscalerMode::Anime4kS => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_s.esr")
        }
        LumaUpscalerMode::Anime4kM => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_m.esr")
        }
        LumaUpscalerMode::Anime4kL => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_l.esr")
        }
        LumaUpscalerMode::Anime4kVl => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_vl.esr")
        }
        LumaUpscalerMode::Anime4kUl => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_ul.esr")
        }
        LumaUpscalerMode::Anime4kDenoiseS => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_denoise_s.esr")
        }
        LumaUpscalerMode::Anime4kDenoiseM => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_denoise_m.esr")
        }
        LumaUpscalerMode::Anime4kDenoiseL => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_denoise_l.esr")
        }
        LumaUpscalerMode::Anime4kDenoiseVl => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_denoise_vl.esr")
        }
        LumaUpscalerMode::Anime4kDenoiseUl => {
            include_bytes!("../../../assets/anime4k/anime4k_upscale_denoise_ul.esr")
        }
        _ => return None,
    })
}

type ModelCache = OnceLock<Mutex<Vec<(LumaUpscalerMode, Arc<SrModel>)>>>;

/// Returns the cached model for `mode`, building it on first use.
fn cached(
    cache: &'static ModelCache,
    mode: LumaUpscalerMode,
    build: impl FnOnce() -> Result<SrModel>,
) -> Result<Arc<SrModel>> {
    let cache = cache.get_or_init(Default::default);
    if let Some((_, model)) = cache
        .lock()
        .expect("model cache poisoned")
        .iter()
        .find(|(cached, _)| *cached == mode)
    {
        return Ok(model.clone());
    }
    let model = Arc::new(build()?);
    let mut entries = cache.lock().expect("model cache poisoned");
    // Another thread may have built it meanwhile; keep the first so every
    // caller shares one instance.
    if let Some((_, existing)) = entries.iter().find(|(cached, _)| *cached == mode) {
        return Ok(existing.clone());
    }
    entries.push((mode, model.clone()));
    Ok(model)
}

/// The network as published (no fusion). Used by the reference executor and
/// by backends that consume the canonical graph.
pub fn source_model(mode: LumaUpscalerMode) -> Result<Arc<SrModel>> {
    static CACHE: ModelCache = OnceLock::new();
    cached(&CACHE, mode, || load(mode))
}

/// The network after exact algebraic optimization; this is what GPU backends
/// execute.
pub fn execution_model(mode: LumaUpscalerMode) -> Result<Arc<SrModel>> {
    static CACHE: ModelCache = OnceLock::new();
    cached(&CACHE, mode, || {
        Ok(fold::optimize(source_model(mode)?.as_ref()))
    })
}

fn load(mode: LumaUpscalerMode) -> Result<SrModel> {
    if mode.family() != UpscalerFamily::Network || mode == LumaUpscalerMode::Custom {
        return Err(PlayerError::Renderer(format!(
            "upscaler mode {} has no bundled network",
            mode.label()
        )));
    }
    if mode.is_legacy_artcnn() {
        return super::artcnn::lower(mode.label(), model_for_mode(mode)?);
    }
    let bytes = bundled_asset(mode).ok_or_else(|| {
        PlayerError::Renderer(format!(
            "upscaler mode {} is not bundled on this platform",
            mode.label()
        ))
    })?;
    SrModel::from_bytes(bytes)
}

/// Parses a user-supplied network: an Erika `.esr` file or an mpv hook shader
/// of a supported dialect (FSRCNNX, Anime4K CNN).
pub fn load_custom(name: &str, bytes: &[u8]) -> Result<SrModel> {
    if bytes.starts_with(b"ESRM") {
        return SrModel::from_bytes(bytes);
    }
    let source = std::str::from_utf8(bytes).map_err(|_| {
        PlayerError::Renderer("custom upscaler model is neither .esr nor UTF-8 GLSL".to_string())
    })?;
    super::mpv::parse(name, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::sr::reference::{LumaRange, run, test_pattern};

    #[test]
    fn every_bundled_network_loads_and_validates() {
        for mode in LumaUpscalerMode::ALL {
            if mode.family() != UpscalerFamily::Network || mode == LumaUpscalerMode::Custom {
                assert!(source_model(mode).is_err());
                continue;
            }
            let model = source_model(mode).unwrap();
            let optimized = execution_model(mode).unwrap();
            eprintln!(
                "{:<22} layers {:>2} -> {:>2}, MAC/px {:>7} -> {:>7}, params {}",
                mode.label(),
                model.layers.len(),
                optimized.layers.len(),
                model.macs_per_pixel(),
                optimized.macs_per_pixel(),
                model.parameter_count(),
            );
            assert!(optimized.macs_per_pixel() <= model.macs_per_pixel());
        }
    }

    fn load_f32(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four bytes")))
            .collect()
    }

    fn assert_matches_onnx(
        mode: LumaUpscalerMode,
        input: &[u8],
        expected: &[u8],
        width: usize,
        height: usize,
    ) {
        for model in [source_model(mode).unwrap(), execution_model(mode).unwrap()] {
            let output = run(&model, &load_f32(input), width, height, LumaRange::Full);
            let expected = load_f32(expected);
            let (mut sum, mut max) = (0.0f64, 0.0f64);
            for (a, b) in output.iter().zip(&expected) {
                let error = f64::from((a - b).abs());
                sum += error;
                max = max.max(error);
            }
            let mae = sum / output.len() as f64;
            eprintln!("{}: mae {mae:.6} max {max:.6}", mode.label());
            assert!(
                mae < 1e-3 && max < 1.5e-2,
                "{} mae {mae} max {max}",
                mode.label()
            );
        }
    }

    #[test]
    fn exported_artcnn_networks_match_onnx() {
        assert_matches_onnx(
            LumaUpscalerMode::ArtCnnC4F16Dn,
            include_bytes!("../../../tests/data/artcnn/c4f16_dn/input_128x72.f32"),
            include_bytes!("../../../tests/data/artcnn/c4f16_dn/output_256x144.f32"),
            128,
            72,
        );
        assert_matches_onnx(
            LumaUpscalerMode::ArtCnnC4F32Ds,
            include_bytes!("../../../tests/data/artcnn/c4f32_ds/input_128x72.f32"),
            include_bytes!("../../../tests/data/artcnn/c4f32_ds/output_256x144.f32"),
            128,
            72,
        );
        assert_matches_onnx(
            LumaUpscalerMode::ArtCnnC4F32Dn,
            include_bytes!("../../../tests/data/artcnn/c4f32_dn/input_128x72.f32"),
            include_bytes!("../../../tests/data/artcnn/c4f32_dn/output_256x144.f32"),
            128,
            72,
        );
        assert_matches_onnx(
            LumaUpscalerMode::ArtCnnR8F64,
            include_bytes!("../../../tests/data/artcnn/r8f64/input_32x24.f32"),
            include_bytes!("../../../tests/data/artcnn/r8f64/output_64x48.f32"),
            32,
            24,
        );
    }

    #[test]
    fn optimized_networks_match_their_source_exactly() {
        for mode in LumaUpscalerMode::ALL {
            if mode.family() != UpscalerFamily::Network
                || mode == LumaUpscalerMode::Custom
                || mode == LumaUpscalerMode::ArtCnnR8F64
            {
                continue;
            }
            let source = source_model(mode).unwrap();
            let optimized = execution_model(mode).unwrap();
            for (width, height) in [(23, 17), (2, 2), (5, 3)] {
                let luma = test_pattern(width, height, 7);
                for range in [LumaRange::Full, LumaRange::Limited] {
                    let a = run(&source, &luma, width, height, range);
                    let b = run(&optimized, &luma, width, height, range);
                    let max = a
                        .iter()
                        .zip(&b)
                        .map(|(a, b)| (a - b).abs())
                        .fold(0.0f32, f32::max);
                    assert!(max < 2e-4, "{} {width}x{height}: {max}", mode.label());
                }
            }
        }
    }
}
