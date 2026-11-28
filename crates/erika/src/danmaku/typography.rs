//! liberika danmaku font-size policy.
//!
//! NipaPlay and Flutter express font size as pixels per em. `ab_glyph` uses a
//! different `PxScale`: the pixel height from the font ascent to its descent.
//! Keep that conversion here so measurement, kerning and rasterization all use
//! the same units.

use ab_glyph::{Font, PxScale};

pub(super) const DEFAULT_SOURCE_FONT_SIZE: f32 = 25.0;
pub(super) const DEFAULT_CONFIG_FONT_SIZE: f32 = 30.0;
pub(super) const DEFAULT_NATIVE_FONT_SIZE: f32 = DEFAULT_CONFIG_FONT_SIZE;

pub(super) fn effective_config_font_size(config_font_size: f32, scale_factor: f32) -> f32 {
    let logical = finite_or(config_font_size, DEFAULT_CONFIG_FONT_SIZE).max(1.0);
    let scale_factor = finite_or(scale_factor, 1.0).max(0.001);
    (logical * scale_factor).max(1.0)
}

pub(super) fn effective_font_size(
    source_font_size: f32,
    config_font_size: f32,
    scale_factor: f32,
) -> f32 {
    let base = effective_config_font_size(config_font_size, scale_factor);
    let source = finite_or(source_font_size, DEFAULT_SOURCE_FONT_SIZE);
    let reference_size = if source > 0.0 {
        base * (source / DEFAULT_SOURCE_FONT_SIZE)
    } else {
        base
    };
    reference_size.max(1.0)
}

/// Converts NipaPlay/Flutter pixels-per-em to `ab_glyph`'s text-height scale.
///
/// This is font-specific: fallback fonts can have a different ascent/descent
/// height relative to their em square, so callers must convert using the face
/// that will actually measure or rasterize the glyph.
pub(super) fn px_scale_for_em(font: &impl Font, px_per_em: f32) -> PxScale {
    let px_per_em = finite_or(px_per_em, DEFAULT_NATIVE_FONT_SIZE).max(1.0);
    let height = font.height_unscaled();
    let scale = font
        .units_per_em()
        .filter(|units_per_em| units_per_em.is_finite() && *units_per_em > 0.0)
        .map(|units_per_em| px_per_em * height / units_per_em)
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .unwrap_or(px_per_em);
    PxScale::from(scale)
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}
