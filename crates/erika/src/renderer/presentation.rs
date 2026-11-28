//! Backend-independent video presentation geometry.
//!
//! Renderers consume the same aspect-fit rectangle and source-to-output
//! mapping so video, subtitles, and danmaku stay aligned across Metal,
//! Direct3D 11, and wgpu surfaces.

#[cfg(any(target_os = "windows", feature = "wgpu"))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PresentationRect {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PresentationLayout {
    source_width: f32,
    source_height: f32,
    drawable_width: f32,
    drawable_height: f32,
    pub(crate) target_rect: [f32; 4],
}

impl PresentationLayout {
    pub(crate) fn aspect_fit(
        source_width: u32,
        source_height: u32,
        drawable_width: u32,
        drawable_height: u32,
    ) -> Self {
        let source_width = source_width.max(1) as f64;
        let source_height = source_height.max(1) as f64;
        let drawable_width = drawable_width.max(1) as f64;
        let drawable_height = drawable_height.max(1) as f64;
        let scale = (drawable_width / source_width).min(drawable_height / source_height);
        let width = (source_width * scale).min(drawable_width);
        let height = (source_height * scale).min(drawable_height);
        let x = (drawable_width - width) * 0.5;
        let y = (drawable_height - height) * 0.5;
        Self {
            source_width: source_width as f32,
            source_height: source_height as f32,
            drawable_width: drawable_width as f32,
            drawable_height: drawable_height as f32,
            target_rect: [
                x as f32,
                y as f32,
                width.max(1.0) as f32,
                height.max(1.0) as f32,
            ],
        }
    }

    #[cfg(any(target_os = "windows", feature = "wgpu"))]
    pub(crate) fn presentation_rect(self) -> PresentationRect {
        PresentationRect {
            x: self.target_rect[0],
            y: self.target_rect[1],
            width: self.target_rect[2],
            height: self.target_rect[3],
        }
    }

    /// Map a different raster coordinate space into the same displayed video
    /// rectangle, including its letterboxing and any custom aspect ratio.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "ios", target_os = "tvos")),
        allow(dead_code)
    )]
    pub(crate) fn with_source_size(self, width: u32, height: u32) -> Self {
        Self {
            source_width: width.max(1) as f32,
            source_height: height.max(1) as f32,
            ..self
        }
    }

    #[cfg_attr(
        not(any(target_os = "macos", target_os = "ios", target_os = "tvos")),
        allow(dead_code)
    )]
    pub(crate) fn is_source_upscaled(self) -> bool {
        self.target_rect[2] > self.source_width
    }

    #[cfg_attr(
        not(any(target_os = "macos", target_os = "ios", target_os = "tvos")),
        allow(dead_code)
    )]
    pub(crate) fn video_viewport(self) -> [f32; 4] {
        [self.drawable_width, self.drawable_height, 0.0, 0.0]
    }

    #[cfg_attr(
        not(any(target_os = "macos", target_os = "ios", target_os = "tvos")),
        allow(dead_code)
    )]
    pub(crate) fn overlay_viewport(self) -> [f32; 2] {
        [self.drawable_width, self.drawable_height]
    }

    #[cfg_attr(
        not(any(target_os = "macos", target_os = "ios", target_os = "tvos")),
        allow(dead_code)
    )]
    pub(crate) fn map_source_rect(self, x: f32, y: f32, width: f32, height: f32) -> [f32; 4] {
        let scale_x = self.target_rect[2] / self.source_width;
        let scale_y = self.target_rect[3] / self.source_height;
        [
            self.target_rect[0] + x * scale_x,
            self.target_rect[1] + y * scale_y,
            width * scale_x,
            height * scale_y,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_rect_close(actual: [f32; 4], expected: [f32; 4]) {
        for (actual, expected) in actual.into_iter().zip(expected) {
            assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
        }
    }

    #[test]
    fn aspect_fit_letterboxes_and_pillarboxes() {
        assert_rect_close(
            PresentationLayout::aspect_fit(1920, 1080, 1000, 1000).target_rect,
            [0.0, 218.75, 1000.0, 562.5],
        );
        assert_rect_close(
            PresentationLayout::aspect_fit(1080, 1920, 1000, 1000).target_rect,
            [218.75, 0.0, 562.5, 1000.0],
        );
    }

    #[test]
    fn source_rect_maps_into_fitted_video_area() {
        let layout = PresentationLayout::aspect_fit(1920, 1080, 1000, 1000);
        assert_rect_close(
            layout.map_source_rect(960.0, 540.0, 192.0, 108.0),
            [500.0, 500.0, 100.0, 56.25],
        );
        assert_eq!(layout.overlay_viewport(), [1000.0, 1000.0]);
        assert_eq!(layout.video_viewport(), [1000.0, 1000.0, 0.0, 0.0]);
    }

    #[test]
    fn source_upscaling_query_tracks_presented_width() {
        assert!(PresentationLayout::aspect_fit(1920, 1080, 3840, 2160).is_source_upscaled());
        assert!(!PresentationLayout::aspect_fit(1920, 1080, 1920, 1080).is_source_upscaled());
        assert!(!PresentationLayout::aspect_fit(1920, 1080, 1000, 1000).is_source_upscaled());
    }

    #[test]
    fn supersampled_subtitles_keep_their_displayed_size_and_position() {
        let video = PresentationLayout::aspect_fit(1920, 1080, 1000, 1000);
        let subtitles = video.with_source_size(3840, 2160);
        assert_eq!(subtitles.target_rect, video.target_rect);
        assert_rect_close(
            subtitles.map_source_rect(1200.0, 1920.0, 1440.0, 120.0),
            video.map_source_rect(600.0, 960.0, 720.0, 60.0),
        );
    }

    #[test]
    fn zero_dimensions_are_normalized_for_gpu_viewports() {
        assert_eq!(
            PresentationLayout::aspect_fit(0, 0, 0, 0).target_rect,
            [0.0, 0.0, 1.0, 1.0],
        );
    }
}
