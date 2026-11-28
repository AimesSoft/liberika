#[cfg(feature = "libass")]
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(feature = "libass")]
use crate::subtitle::{
    LibassRenderConfig, LibassSubtitleRenderer, SubtitleError, SubtitleRenderRequest,
    SubtitleRenderer,
};
use crate::subtitle::{
    Result as SubtitleResult, SubtitleAlphaBitmap, SubtitleBitmapPlane, SubtitleFrameChange,
    SubtitleRenderOutput, SubtitleRendererCore, SubtitleTimeline, SubtitleViewport,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayViewport {
    pub width: u32,
    pub height: u32,
}

impl OverlayViewport {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverlayFrame {
    pub pts: Duration,
    pub viewport: OverlayViewport,
    pub subtitle_planes: Vec<SubtitleBitmapPlane>,
    pub subtitle_alpha_planes: Vec<SubtitleAlphaBitmap>,
    pub subtitle_changed: bool,
}

impl OverlayFrame {
    pub fn is_empty(&self) -> bool {
        self.subtitle_planes.is_empty() && self.subtitle_alpha_planes.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct OverlayTimeline {
    subtitles: Option<OverlaySubtitleRenderer>,
}

#[derive(Debug, Clone)]
enum OverlaySubtitleRenderer {
    DebugTimeline(SubtitleRendererCore),
    #[cfg(feature = "libass")]
    Libass(Arc<Mutex<LibassSubtitleRenderer>>),
}

impl OverlaySubtitleRenderer {
    fn render(
        &mut self,
        pts: Duration,
        viewport: OverlayViewport,
    ) -> SubtitleResult<(SubtitleRenderOutput, bool)> {
        match self {
            Self::DebugTimeline(renderer) => {
                let result =
                    renderer.render(pts, SubtitleViewport::new(viewport.width, viewport.height));
                Ok((
                    SubtitleRenderOutput::Rgba(result.frame),
                    result.change == SubtitleFrameChange::Changed,
                ))
            }
            #[cfg(feature = "libass")]
            Self::Libass(renderer) => {
                let mut renderer = renderer
                    .lock()
                    .map_err(|_| SubtitleError::Libass("renderer lock poisoned".to_string()))?;
                let output = renderer.render(SubtitleRenderRequest::new(
                    pts,
                    viewport.width,
                    viewport.height,
                ))?;
                let changed = match &output {
                    SubtitleRenderOutput::Rgba(_) => true,
                    SubtitleRenderOutput::Alpha(bitmaps) => bitmaps.changed,
                };
                Ok((output, changed))
            }
        }
    }
}

impl OverlayTimeline {
    pub fn new() -> Self {
        Self { subtitles: None }
    }

    pub fn with_subtitles(mut self, subtitles: SubtitleTimeline) -> Self {
        self.subtitles = Some(OverlaySubtitleRenderer::DebugTimeline(
            SubtitleRendererCore::new_debug(subtitles),
        ));
        self
    }

    #[cfg(feature = "libass")]
    pub fn with_ass_subtitles(
        mut self,
        script: impl AsRef<[u8]>,
        config: LibassRenderConfig,
    ) -> SubtitleResult<Self> {
        self.subtitles = Some(OverlaySubtitleRenderer::Libass(Arc::new(Mutex::new(
            LibassSubtitleRenderer::from_ass_script(script, config)?,
        ))));
        Ok(self)
    }

    #[cfg(feature = "libass")]
    pub fn with_libass_renderer(mut self, renderer: LibassSubtitleRenderer) -> Self {
        self.subtitles = Some(OverlaySubtitleRenderer::Libass(Arc::new(Mutex::new(
            renderer,
        ))));
        self
    }

    pub fn render(&mut self, pts: Duration, viewport: OverlayViewport) -> OverlayFrame {
        let subtitle_result = match self
            .subtitles
            .as_mut()
            .map(|renderer| renderer.render(pts, viewport))
        {
            Some(Ok((SubtitleRenderOutput::Rgba(frame), changed))) => {
                (frame.planes, Vec::new(), changed)
            }
            Some(Ok((SubtitleRenderOutput::Alpha(bitmaps), changed))) => {
                (Vec::new(), bitmaps.parts, changed)
            }
            Some(Err(error)) => {
                crate::trace::diagnostic_at(
                    crate::trace::LogLevel::Warn,
                    format!("Erika overlay subtitle render failed: {error}"),
                );
                (Vec::new(), Vec::new(), true)
            }
            None => (Vec::new(), Vec::new(), false),
        };
        let (subtitle_planes, subtitle_alpha_planes, subtitle_changed) = subtitle_result;

        OverlayFrame {
            pts,
            viewport,
            subtitle_planes,
            subtitle_alpha_planes,
            subtitle_changed,
        }
    }
}

impl Default for OverlayTimeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::{SubtitleCue, SubtitleTimeline};

    #[cfg(feature = "libass")]
    const SIMPLE_ASS_SCRIPT: &str = r#"[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,32,&H00FFFFFF,&H000000FF,&H80000000,&H80000000,0,0,0,0,100,100,0,0,1,2,0,2,20,20,24,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,Overlay libass
"#;

    #[test]
    fn overlay_timeline_renders_subtitles_only() {
        let subtitles = SubtitleTimeline::new(vec![SubtitleCue {
            start: Duration::from_secs(1),
            end: Duration::from_secs(3),
            text: "hello".to_string(),
        }]);

        let mut timeline = OverlayTimeline::default().with_subtitles(subtitles);
        let frame = timeline.render(Duration::from_secs(2), OverlayViewport::new(640, 360));

        assert!(!frame.is_empty());
        assert_eq!(frame.subtitle_planes.len(), 1);
        assert!(frame.subtitle_alpha_planes.is_empty());
        assert!(frame.subtitle_changed);
    }

    #[test]
    fn overlay_timeline_reports_unchanged_debug_subtitles() {
        let subtitles = SubtitleTimeline::new(vec![SubtitleCue {
            start: Duration::from_secs(1),
            end: Duration::from_secs(3),
            text: "hello".to_string(),
        }]);
        let mut timeline = OverlayTimeline::default().with_subtitles(subtitles);

        let first = timeline.render(Duration::from_secs(2), OverlayViewport::new(640, 360));
        let second = timeline.render(Duration::from_secs(2), OverlayViewport::new(640, 360));

        assert!(first.subtitle_changed);
        assert!(!second.subtitle_changed);
    }

    #[cfg(feature = "libass")]
    #[test]
    fn overlay_timeline_renders_libass_subtitles() {
        let mut timeline = OverlayTimeline::default()
            .with_ass_subtitles(SIMPLE_ASS_SCRIPT, LibassRenderConfig::default())
            .unwrap();

        let frame = timeline.render(Duration::from_millis(500), OverlayViewport::new(640, 360));

        assert_eq!(frame.pts, Duration::from_millis(500));
        assert_eq!(frame.viewport, OverlayViewport::new(640, 360));
        assert!(frame.subtitle_planes.is_empty());
        assert!(!frame.subtitle_alpha_planes.is_empty());
        assert!(frame.subtitle_changed);
        assert!(
            frame
                .subtitle_alpha_planes
                .iter()
                .all(SubtitleAlphaBitmap::is_valid)
        );
    }
}
