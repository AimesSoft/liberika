//! Subtitle raster resolution, independent of video decoding and font size.
use crate::overlay::OverlayViewport;
use std::time::Duration;

pub(super) struct SubtitleResolution {
    pub(super) source: Option<OverlayViewport>,
    enabled: bool,
    reduced: bool,
    samples: u32,
    elapsed: Duration,
}

impl Default for SubtitleResolution {
    fn default() -> Self {
        Self {
            source: None,
            enabled: true,
            reduced: false,
            samples: 0,
            elapsed: Duration::ZERO,
        }
    }
}

impl SubtitleResolution {
    pub(super) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.reset_budget();
        }
    }

    pub(super) fn reset_budget(&mut self) {
        self.reduced = false;
        self.samples = 0;
        self.elapsed = Duration::ZERO;
    }

    pub(super) fn viewport(&self, source: OverlayViewport) -> OverlayViewport {
        // Bound glyph/atlas growth before asking libass to allocate. 1080p
        // subtitles use a 4K canvas; large sources retain their native detail.
        if !self.enabled
            || self.reduced
            || source.width > 2048
            || source.height > 2048
            || u64::from(source.width) * u64::from(source.height) > 2048 * 1080
        {
            return source;
        }
        OverlayViewport::new(source.width * 2, source.height * 2)
    }

    /// A cold glyph or a single slow frame must not turn the option off.
    /// Fall back for this media only if sixty renders average over 4 ms.
    pub(super) fn record(&mut self, elapsed: Duration) -> bool {
        self.samples += 1;
        self.elapsed += elapsed;
        if self.samples < 60 {
            return false;
        }
        self.reduced = self.elapsed > Duration::from_millis(4 * u64::from(self.samples));
        self.samples = 0;
        self.elapsed = Duration::ZERO;
        self.reduced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_double_subtitle_resolution_and_bounds_allocations() {
        let mut quality = SubtitleResolution::default();
        let source = OverlayViewport::new(1920, 1080);
        assert_eq!(quality.viewport(source), OverlayViewport::new(3840, 2160));
        let large = OverlayViewport::new(3840, 2160);
        assert_eq!(quality.viewport(large), large);
        quality.set_enabled(false);
        assert_eq!(quality.viewport(source), source);
        quality.set_enabled(true);
        assert_eq!(quality.viewport(source), OverlayViewport::new(3840, 2160));
    }

    #[test]
    fn sustained_cost_reduces_quality_until_reset_but_a_cold_frame_does_not() {
        let mut quality = SubtitleResolution::default();
        quality.set_enabled(true);
        let source = OverlayViewport::new(1920, 1080);
        quality.record(Duration::from_millis(20));
        for _ in 1..60 {
            assert!(!quality.record(Duration::from_millis(1)));
        }
        assert_ne!(quality.viewport(source), source);
        for _ in 0..59 {
            assert!(!quality.record(Duration::from_millis(5)));
        }
        assert!(quality.record(Duration::from_millis(5)));
        assert_eq!(quality.viewport(source), source);
        quality.reset_budget();
        assert_ne!(quality.viewport(source), source);
    }
}
