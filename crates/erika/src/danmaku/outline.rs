//! liberika danmaku outline profiles and raster quantization.
//!
//! Layout and rendering must use the same resolved width so collision bounds
//! match the glyphs drawn on screen. Keep every profile tuning constant here.

const BASE_WIDTH_RATIO: f32 = 0.06;
const BASE_WIDTH_MIN_PX: f32 = 1.0;
const BASE_WIDTH_MAX_PX: f32 = 2.6;
const FINE_PROFILE_SCALE: f32 = 0.8;
const THICK_PROFILE_SCALE: f32 = 2.0;
const NORMAL_PROFILE_THRESHOLD: f32 = 1.5;
const THICK_PROFILE_THRESHOLD: f32 = 2.5;
const MAX_RASTER_RADIUS_PX: f32 = 16.0;

/// Resolves the public outline profile to a physical-pixel width.
///
/// Profiles are 0 = off, 1 = fine, 2 = normal, and 3 = thick; values above 3
/// clamp to thick. Normal and thick reproduce the widths liberika's former
/// continuous multiplier produced at 1.0 and 2.0, so a host that used to send
/// `outline_width: 1.0` must now send `2.0` to keep the same stroke.
pub(super) fn resolve_width_px(font_size: f32, profile: f32) -> f32 {
    if !profile.is_finite() || profile <= 0.0 {
        return 0.0;
    }

    let base_width = (font_size * BASE_WIDTH_RATIO).clamp(BASE_WIDTH_MIN_PX, BASE_WIDTH_MAX_PX);
    if profile < NORMAL_PROFILE_THRESHOLD {
        base_width * FINE_PROFILE_SCALE
    } else if profile < THICK_PROFILE_THRESHOLD {
        base_width
    } else {
        base_width * THICK_PROFILE_SCALE
    }
}

/// Quantizes a resolved physical-pixel width for the alpha-mask dilator.
pub(super) fn raster_radius(width_px: f32) -> u16 {
    if !width_px.is_finite() {
        return 0;
    }
    width_px.round().clamp(0.0, MAX_RASTER_RADIUS_PX) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The legacy resolver this profile table replaced.
    fn legacy_width_px(font_size: f32, multiplier: f32) -> f32 {
        let multiplier = multiplier.clamp(0.0, 4.0);
        if multiplier <= 0.0 || !multiplier.is_finite() {
            return 0.0;
        }
        (font_size * BASE_WIDTH_RATIO).clamp(BASE_WIDTH_MIN_PX, BASE_WIDTH_MAX_PX) * multiplier
    }

    #[test]
    fn normal_and_thick_reproduce_the_legacy_multipliers() {
        for font_size in [20.0f32, 25.0, 30.0, 40.0, 60.0] {
            assert_eq!(
                resolve_width_px(font_size, 2.0),
                legacy_width_px(font_size, 1.0)
            );
            assert_eq!(
                resolve_width_px(font_size, 3.0),
                legacy_width_px(font_size, 2.0)
            );
            assert_eq!(
                raster_radius(resolve_width_px(font_size, 3.0)),
                raster_radius(legacy_width_px(font_size, 2.0))
            );
        }
    }

    #[test]
    fn profile_clamps_and_rejects_non_finite_input() {
        assert_eq!(resolve_width_px(30.0, 0.0), 0.0);
        assert_eq!(resolve_width_px(30.0, -1.0), 0.0);
        assert_eq!(resolve_width_px(30.0, f32::NAN), 0.0);
        // At or beyond thick the ladder saturates instead of growing.
        assert_eq!(resolve_width_px(30.0, 4.0), resolve_width_px(30.0, 3.0));
        // A non-finite width means "no usable outline", never a giant one.
        assert_eq!(raster_radius(f32::NAN), 0);
        assert_eq!(raster_radius(f32::INFINITY), 0);
        assert_eq!(raster_radius(1_000.0), MAX_RASTER_RADIUS_PX as u16);
    }
}
