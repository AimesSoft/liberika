use std::ffi::c_void;
use std::time::Duration;

use crate::core::{
    ColorPrimaries, FlutterTextureKind, LumaUpscalerBackendStatus, PlatformSurface, PlayerError,
    PlayerVideoFrame, RenderFrameContext, RendererBackend, RendererFrameCapture,
    RendererResourceStats, RendererRuntimeStats, Result, SurfaceMetrics, TransferFunction,
};
use crate::danmaku::DanmakuRenderPlan;
use crate::ffmpeg::{Frame, PlanarFrame};
use crate::overlay::OverlayFrame;
pub use crate::renderer::pipeline::LumaUpscalerMode;
use crate::renderer::pipeline::{
    ColorRange, DoviSourceMetadata, HdrMetadata, MatrixCoefficients, SourceColorState,
    VideoRenderPipeline,
};
use crate::trace;

pub use crate::renderer::output::OutputMode as MetalOutputMode;
use crate::renderer::output::{
    ActiveOutputEncoding, OutputFallbackReason, OutputRuntimeStatus, OutputSurfaceFormat,
};

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
mod apple;
// Public for integration tests (numeric verification against ONNX
// references); not part of the stable API surface.
#[doc(hidden)]
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub mod upscaler;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClearColor {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
    pub alpha: f64,
}

impl ClearColor {
    pub fn black() -> Self {
        Self {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        }
    }

    pub fn animated(time_seconds: f64) -> Self {
        Self {
            red: time_seconds.sin() * 0.5 + 0.5,
            green: (time_seconds * 0.73).sin() * 0.5 + 0.5,
            blue: (time_seconds * 1.37).cos() * 0.5 + 0.5,
            alpha: 1.0,
        }
    }
}

pub struct MetalRenderer {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    inner: apple::MetalRendererImpl,
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
    _unsupported: (),
    current_frame: Option<ImportedVideoFrame>,
    current_frame_visible: bool,
    current_media_time: Duration,
    current_generation: u64,
    upload_counter: u64,
    software_upload_counter: u64,
    output_mode: MetalOutputMode,
    /// Presents skipped because the drawable pool was drained (nil
    /// `nextDrawable`). Obtaining nil can still take up to one second with
    /// `allowsNextDrawableTimeout` enabled. Count skips without changing C stats.
    present_backpressure_skips: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetalRendererConfig {
    pub output_mode: MetalOutputMode,
    pub luma_upscaler: LumaUpscalerMode,
    pub video_alpha_mode: VideoAlphaMode,
    /// How long the Direct3D 11 renderer waits for the GPU to finish with a
    /// decoded frame before handing it back to the decoder, when it draws
    /// into a compositor-owned surface. A hardware decoder's work shares
    /// the wait, which after a seek can take hundreds of milliseconds: a
    /// host rendering on its UI thread keeps it short, one rendering on a
    /// thread of its own can wait that out.
    pub compositor_handoff_timeout: Duration,
}

/// Describes how a decoded video frame carries transparency.
///
/// `PackedAlphaRight` expects a side-by-side frame: the left half contains
/// colour and the right half contains a grayscale alpha mask. Renderers expose
/// the left half as the logical video size and reconstruct premultiplied alpha
/// in the presentation shader.
#[repr(i32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VideoAlphaMode {
    #[default]
    Opaque = 0,
    PackedAlphaRight = 1,
}

impl VideoAlphaMode {
    pub fn from_raw(value: i32) -> Self {
        match value {
            1 => Self::PackedAlphaRight,
            _ => Self::Opaque,
        }
    }

    pub fn has_alpha(self) -> bool {
        matches!(self, Self::PackedAlphaRight)
    }

    pub fn logical_width(self, encoded_width: u32) -> u32 {
        match self {
            Self::Opaque => encoded_width,
            Self::PackedAlphaRight => (encoded_width / 2).max(1),
        }
    }
}

impl Default for MetalRendererConfig {
    fn default() -> Self {
        Self {
            output_mode: MetalOutputMode::default(),
            luma_upscaler: LumaUpscalerMode::default(),
            video_alpha_mode: VideoAlphaMode::default(),
            compositor_handoff_timeout: Duration::from_millis(100),
        }
    }
}

#[allow(dead_code)]
pub(crate) fn metal_drawable_pixel_format(mode: MetalOutputMode) -> MetalDrawablePixelFormat {
    match mode {
        MetalOutputMode::Sdr | MetalOutputMode::Auto { .. } => MetalDrawablePixelFormat::Bgra8Unorm,
        MetalOutputMode::AppleEdr { .. } | MetalOutputMode::ExtendedLinear { .. } => {
            MetalDrawablePixelFormat::Rgba16Float
        }
    }
}

#[allow(dead_code)]
pub(crate) fn metal_target_color(
    mode: MetalOutputMode,
    source: SourceColorState,
) -> crate::renderer::pipeline::TargetColorState {
    match mode {
        MetalOutputMode::Sdr | MetalOutputMode::Auto { .. } => {
            if source.is_hdr() {
                crate::renderer::pipeline::TargetColorState::sdr_tone_map_target(
                    ColorPrimaries::Bt709,
                )
            } else {
                crate::renderer::pipeline::TargetColorState::sdr(ColorPrimaries::Bt709)
            }
        }
        MetalOutputMode::AppleEdr { headroom } | MetalOutputMode::ExtendedLinear { headroom } => {
            #[cfg(any(target_os = "ios", target_os = "tvos"))]
            {
                let _ = source;
                let headroom = headroom.max(1.0);
                return crate::renderer::pipeline::TargetColorState {
                    primaries: ColorPrimaries::Bt709,
                    transfer: TransferFunction::Srgb,
                    peak_nits: 100.0 * headroom,
                    reference_white_nits: 100.0,
                    edr_headroom: headroom,
                };
            }

            #[cfg(not(any(target_os = "ios", target_os = "tvos")))]
            {
                let primaries = match (source.transfer, source.primaries) {
                    (TransferFunction::Pq, ColorPrimaries::Unknown) => ColorPrimaries::Bt2020,
                    (TransferFunction::Pq, primaries) => primaries,
                    _ => ColorPrimaries::Bt709,
                };
                let mut target =
                    crate::renderer::pipeline::TargetColorState::apple_edr(primaries, headroom);
                if matches!(source.transfer, TransferFunction::Pq) {
                    target.transfer = TransferFunction::Pq;
                    target.peak_nits = 10_000.0;
                    target.reference_white_nits = 203.0;
                } else {
                    target.peak_nits = 100.0 * headroom.max(1.0);
                    target.reference_white_nits = 100.0;
                }
                target
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetalDrawablePixelFormat {
    Bgra8Unorm,
    Rgba16Float,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetalRendererStats {
    pub drawable_width: u32,
    pub drawable_height: u32,
    pub rendered_frames: u64,
    pub prepared_overlay_frames: u64,
    pub prepared_overlay_subtitle_planes: u64,
    pub danmaku_passes: u64,
    pub danmaku_items: u64,
    pub overlay_alpha_atlas_uploads: u64,
    pub overlay_alpha_atlas_reuses: u64,
    pub last_danmaku_atlas_duration: Duration,
    pub last_danmaku_vertex_build_duration: Duration,
    pub last_danmaku_vertex_copy_duration: Duration,
    pub last_danmaku_encode_duration: Duration,
    pub last_danmaku_vertex_bytes: usize,
    pub last_danmaku_vertex_count: usize,
    pub upscaler_mode: LumaUpscalerMode,
    pub upscaler_backend: LumaUpscalerBackendStatus,
    pub upscaler_fallbacks: u64,
    pub upscaled_frames: u64,
    pub last_upscaler_encode_duration: Duration,
    pub last_gpu_duration: Duration,
    pub hdr_source_frames: u64,
    pub edr_rendered_frames: u64,
    pub sdr_tonemap_frames: u64,
    pub output_mode_switches: u64,
    /// Host headroom reports accepted by `set_output_headroom`.
    pub headroom_updates: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrameTextureSource {
    pub raw_pixel_buffer: *mut c_void,
    pub width: u32,
    pub height: u32,
}

impl VideoFrameTextureSource {
    pub fn new(raw_pixel_buffer: *mut c_void, width: u32, height: u32) -> Self {
        Self {
            raw_pixel_buffer,
            width,
            height,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportedVideoFormat {
    Nv12,
    P010,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedVideoFrameInfo {
    pub width: usize,
    pub height: usize,
    pub pixel_format: u32,
    pub pixel_format_fourcc: String,
    pub format: ImportedVideoFormat,
    pub full_range: bool,
    pub color_range: ColorRange,
    pub planes: Vec<ImportedVideoPlaneInfo>,
}

pub struct ImportedVideoFrame {
    info: ImportedVideoFrameInfo,
    source_color: SourceColorState,
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    inner: Option<apple::ImportedVideoFrameTextures>,
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
    _unsupported: (),
}

impl ImportedVideoFrame {
    pub fn info(&self) -> &ImportedVideoFrameInfo {
        &self.info
    }

    pub fn plane_count(&self) -> usize {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.as_ref().map_or(0, |inner| inner.plane_count())
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            0
        }
    }

    fn allocated_bytes(&self) -> u64 {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner
                .as_ref()
                .map_or(0, |inner| inner.allocated_bytes())
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            0
        }
    }

    pub fn source_color(&self) -> SourceColorState {
        self.source_color
    }

    pub fn set_source_color(&mut self, source: SourceColorState) {
        let import_range = self.info.color_range;
        let fallback = source.range.resolve(ColorRange::Limited);
        self.source_color = source.range(import_range.resolve(fallback));
    }

    pub fn set_source_color_metadata(
        &mut self,
        primaries: ColorPrimaries,
        transfer: TransferFunction,
        range: ColorRange,
        matrix: MatrixCoefficients,
        hdr_metadata: Option<HdrMetadata>,
        dovi_metadata: Option<DoviSourceMetadata>,
    ) {
        self.set_source_color(
            SourceColorState::new(primaries, transfer)
                .range(range)
                .matrix(matrix)
                .hdr_metadata(hdr_metadata)
                .dovi(dovi_metadata),
        );
    }

    /// Attach a presenter-measured scene-average luminance so the tone map's
    /// pivot follows the content like Dolby Vision L1 would. Only meaningful
    /// when the source carries no dynamic L1 metadata of its own.
    pub fn set_measured_scene_avg(&mut self, scene_avg_nits: Option<f32>) {
        if scene_avg_nits.is_none() {
            return;
        }
        let mut source = self.source_color();
        source = source.measured_scene_avg_nits(scene_avg_nits);
        self.set_source_color(source);
    }
}

pub struct VideoRenderFrame<'a> {
    pub frame: &'a ImportedVideoFrame,
    pub pipeline: VideoRenderPipeline,
    /// Identifies the decoded frame across render ticks. The same token means
    /// the luma plane is unchanged, letting the upscaler reuse its cached
    /// output when a frame is presented for several vsync ticks.
    pub frame_token: Option<u64>,
}

impl<'a> VideoRenderFrame<'a> {
    pub fn new(frame: &'a ImportedVideoFrame) -> Self {
        Self {
            frame,
            pipeline: VideoRenderPipeline::new(frame.source_color(), Default::default()),
            frame_token: None,
        }
    }

    pub fn frame_token(mut self, token: u64) -> Self {
        self.frame_token = Some(token);
        self
    }

    pub fn full_range(mut self, full_range: bool) -> Self {
        self.pipeline.source.range = color_range_from_import(full_range);
        self
    }

    pub fn source_color(mut self, primaries: ColorPrimaries, transfer: TransferFunction) -> Self {
        let range = self.pipeline.source.range;
        let matrix = self.pipeline.source.matrix;
        let target = self.pipeline.target;
        self.pipeline = VideoRenderPipeline::new(
            SourceColorState::new(primaries, transfer)
                .range(range)
                .matrix(matrix),
            target,
        );
        self
    }

    pub fn pipeline(mut self, pipeline: VideoRenderPipeline) -> Self {
        self.pipeline = pipeline;
        self
    }
}

fn color_range_from_import(full_range: bool) -> ColorRange {
    if full_range {
        ColorRange::Full
    } else {
        ColorRange::Limited
    }
}

pub struct OverlayRenderFrame<'a> {
    pub frame: &'a OverlayFrame,
}

impl<'a> OverlayRenderFrame<'a> {
    pub fn new(frame: &'a OverlayFrame) -> Self {
        Self { frame }
    }
}

pub struct DanmakuRenderFrame<'a> {
    pub plan: &'a DanmakuRenderPlan,
}

impl<'a> DanmakuRenderFrame<'a> {
    pub fn new(plan: &'a DanmakuRenderPlan) -> Self {
        Self { plan }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedOverlayFrameInfo {
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub subtitle_planes: usize,
    pub subtitle_pixels: usize,
    pub subtitle_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedVideoPlaneInfo {
    pub index: usize,
    pub width: usize,
    pub height: usize,
    pub metal_pixel_format: &'static str,
}

impl MetalRenderer {
    pub fn new() -> Result<Self> {
        Self::with_config(MetalRendererConfig::default())
    }

    /// Presents skipped because no drawable was available. Rust-internal
    /// observability for the `allowsNextDrawableTimeout` skip path; never
    /// surfaced through the C stats layout.
    pub fn present_backpressure_skips(&self) -> u64 {
        self.present_backpressure_skips
    }

    pub fn with_config(_config: MetalRendererConfig) -> Result<Self> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            Ok(Self {
                inner: apple::MetalRendererImpl::new(_config)?,
                current_frame: None,
                current_frame_visible: false,
                current_media_time: Duration::ZERO,
                current_generation: 1,
                upload_counter: 0,
                software_upload_counter: 0,
                output_mode: _config.output_mode,
                present_backpressure_skips: 0,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    pub unsafe fn attach_raw_layer(
        &mut self,
        layer: *mut c_void,
        metrics: SurfaceMetrics,
    ) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            unsafe { self.inner.attach_raw_layer(layer, metrics) }
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = (layer, metrics);
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    pub unsafe fn import_video_frame_textures(
        &mut self,
        source: VideoFrameTextureSource,
    ) -> Result<ImportedVideoFrame> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            let imported = unsafe { self.inner.import_video_frame_textures(source) }?;
            let source_color =
                SourceColorState::new(ColorPrimaries::Unknown, TransferFunction::Unknown)
                    .range(imported.info.color_range.resolve(ColorRange::Limited));
            Ok(ImportedVideoFrame {
                info: imported.info,
                source_color,
                inner: Some(imported.textures),
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = source;
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    pub fn render_video_frame(&mut self, frame: VideoRenderFrame<'_>) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.render_video_frame(frame)
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = frame;
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    pub fn render_video_frame_with_overlay(
        &mut self,
        frame: VideoRenderFrame<'_>,
        overlay: OverlayRenderFrame<'_>,
    ) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.render_video_frame_with_overlay(frame, overlay)
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = (frame, overlay);
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    pub fn render_video_frame_with_context(
        &mut self,
        frame: VideoRenderFrame<'_>,
        overlay: Option<OverlayRenderFrame<'_>>,
        danmaku: Option<DanmakuRenderFrame<'_>>,
    ) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner
                .render_video_frame_with_context(frame, overlay, danmaku)
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = (frame, overlay, danmaku);
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    pub fn capture_video_frame_rgba(
        &mut self,
        frame: VideoRenderFrame<'_>,
        overlay: Option<OverlayRenderFrame<'_>>,
        danmaku: Option<DanmakuRenderFrame<'_>>,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner
                .capture_video_frame_rgba(frame, overlay, danmaku, width, height)
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = (frame, overlay, danmaku, width, height);
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    pub fn render_overlay_frame(&mut self, overlay: OverlayRenderFrame<'_>) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.render_overlay_frame(overlay)
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = overlay;
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    pub fn prepare_overlay_frame(
        &mut self,
        frame: OverlayRenderFrame<'_>,
    ) -> Result<PreparedOverlayFrameInfo> {
        let info = inspect_overlay_frame(frame.frame)?;
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.record_prepared_overlay_frame(info);
        }
        Ok(info)
    }

    fn import_player_frame(&mut self, frame: &Frame) -> Result<ImportedVideoFrame> {
        let mut imported = if let Some(pixel_buffer) = frame.videotoolbox_pixel_buffer() {
            unsafe {
                self.import_video_frame_textures(VideoFrameTextureSource::new(
                    pixel_buffer.raw(),
                    pixel_buffer.width(),
                    pixel_buffer.height(),
                ))
            }?
        } else {
            let planar = frame.to_planar_frame().ok_or_else(|| {
                PlayerError::Renderer(format!(
                    "unsupported software video frame format {}",
                    frame
                        .pixel_format()
                        .unwrap_or_else(|| "unknown".to_string())
                ))
            })?;
            self.import_planar_frame(&planar, frame.color_range())?
        };
        imported.set_source_color_metadata(
            frame.color_primaries(),
            frame.transfer_function(),
            frame.color_range(),
            frame.matrix_coefficients(),
            frame.hdr_metadata(),
            frame.dovi_metadata(),
        );
        Ok(imported)
    }

    fn import_planar_frame(
        &mut self,
        frame: &PlanarFrame,
        color_range: ColorRange,
    ) -> Result<ImportedVideoFrame> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            let result = self
                .inner
                .upload_planar_video_frame(frame, color_range == ColorRange::Full)?;
            Ok(ImportedVideoFrame {
                info: result.info,
                source_color: SourceColorState::default(),
                inner: Some(result.textures),
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = (frame, color_range);
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    pub fn stats(&self) -> MetalRendererStats {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.stats()
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            MetalRendererStats::default()
        }
    }
}

fn inspect_overlay_frame(frame: &OverlayFrame) -> Result<PreparedOverlayFrameInfo> {
    let mut subtitle_pixels = 0usize;
    let mut subtitle_bytes = 0usize;

    for plane in &frame.subtitle_planes {
        let pixels = plane.width as usize * plane.height as usize;
        let bytes = pixels * 4;
        if plane.rgba.len() != bytes {
            return Err(crate::core::PlayerError::Renderer(format!(
                "subtitle plane has {} bytes, expected {bytes} for {}x{} RGBA",
                plane.rgba.len(),
                plane.width,
                plane.height
            )));
        }
        subtitle_pixels += pixels;
        subtitle_bytes += bytes;
    }
    for bitmap in &frame.subtitle_alpha_planes {
        if !bitmap.is_valid() {
            return Err(crate::core::PlayerError::Renderer(format!(
                "subtitle alpha bitmap has {} bytes, expected at least {} for {}x{} stride {}",
                bitmap.alpha.len(),
                bitmap.required_len(),
                bitmap.placement.width,
                bitmap.placement.height,
                bitmap.stride
            )));
        }
        subtitle_pixels += bitmap.placement.width as usize * bitmap.placement.height as usize;
        subtitle_bytes += bitmap.required_len();
    }

    Ok(PreparedOverlayFrameInfo {
        viewport_width: frame.viewport.width,
        viewport_height: frame.viewport.height,
        subtitle_planes: frame.subtitle_planes.len() + frame.subtitle_alpha_planes.len(),
        subtitle_pixels,
        subtitle_bytes,
    })
}

/// Turns a drained-drawable-pool failure into a skipped frame.
///
/// Every `CAMetalLayer.nextDrawable()` path reports a nil drawable as
/// [`PlayerError::RendererBackpressure`]. Clear and test-pattern frames carry
/// no decoded content, so dropping one is always cheaper than pushing a
/// transient lifecycle race across the C ABI as a terminal renderer error.
/// `skips` accumulates across every skipped present (video, clear, test) so
/// the behavior stays observable; see [`should_report_present_skip`].
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
fn skip_on_backpressure(stage: &str, result: Result<()>, skips: &mut u64) -> Result<()> {
    match result {
        Err(PlayerError::RendererBackpressure(reason)) => {
            *skips = skips.saturating_add(1);
            if should_report_present_skip(*skips) {
                trace::diagnostic(format!(
                    "[erika-render-trace] stage={stage} skipped reason={reason} count={skips}"
                ));
            }
            Ok(())
        }
        other => other,
    }
}

/// Report cadence for drained-drawable-pool skips: the first occurrence plus
/// every power-of-two count. A fullscreen Space transition can burn through
/// dozens of ticks while the compositor holds the pool; this keeps the
/// diagnostic visible without spamming a stall timeline.
pub(crate) fn should_report_present_skip(count: u64) -> bool {
    count == 1 || count.is_power_of_two()
}

pub fn fourcc_string(value: u32) -> String {
    let bytes = value.to_be_bytes();
    if bytes
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("0x{value:08x}")
    }
}

impl RendererBackend for MetalRenderer {
    fn attach_surface(&mut self, surface: PlatformSurface) -> Result<()> {
        match surface {
            PlatformSurface::Metal(handle) => unsafe {
                self.attach_raw_layer(handle.raw_layer as *mut c_void, handle.metrics)
            },
            PlatformSurface::Wgpu(_) => Err(crate::core::PlayerError::Renderer(
                "wgpu surface cannot be attached to MetalRenderer".to_string(),
            )),
            PlatformSurface::FlutterTexture(handle)
                if matches!(
                    handle.kind,
                    FlutterTextureKind::MacOsTextureRegistrar
                        | FlutterTextureKind::IosTextureRegistrar
                ) =>
            {
                #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
                {
                    self.inner.attach_flutter_texture(handle.metrics);
                    Ok(())
                }
                #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
                {
                    let _ = handle;
                    Err(PlayerError::Renderer(
                        "Apple Flutter textures require an Apple Metal renderer".to_string(),
                    ))
                }
            }
            PlatformSurface::FlutterTexture(handle) => Err(PlayerError::Renderer(format!(
                "Flutter texture kind {:?} cannot be attached to MetalRenderer",
                handle.kind
            ))),
        }
    }

    fn detach_surface(&mut self) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.detach_surface();
        }
        Ok(())
    }

    fn resize_surface(&mut self, metrics: SurfaceMetrics) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.resize_surface(metrics);
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = metrics;
        }
        Ok(())
    }

    fn clear_current_frame(&mut self) -> Result<()> {
        self.current_frame = None;
        self.current_frame_visible = false;
        self.current_media_time = Duration::ZERO;
        self.current_generation = 1;
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            if self.inner.has_surface() {
                skip_on_backpressure(
                    "clear_current_frame",
                    self.inner.render_clear(ClearColor::black()),
                    &mut self.present_backpressure_skips,
                )?;
            }
        }
        Ok(())
    }

    fn preserve_current_frame_for_transition(&mut self) -> Result<()> {
        Ok(())
    }

    fn render_test_frame(&mut self, time_seconds: f64) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            let started = std::time::Instant::now();
            skip_on_backpressure(
                "test_frame",
                self.inner.render_clear(ClearColor::animated(time_seconds)),
                &mut self.present_backpressure_skips,
            )
            .map(|result| {
                if trace::enabled() {
                    trace::log(format!(
                        "[erika-render-trace] stage=test_frame time_seconds={:.3} elapsed_ms={:.3}",
                        time_seconds,
                        started.elapsed().as_secs_f64() * 1000.0,
                    ));
                }
                result
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = time_seconds;
            Err(PlayerError::Renderer(
                "Metal renderer is only available on Apple platforms for v0".to_string(),
            ))
        }
    }

    fn upload_player_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        let started = std::time::Instant::now();
        let decoded = frame.frame.decoded_frame().ok_or_else(|| {
            PlayerError::Renderer(
                "Metal renderer received a non-VideoToolbox hardware payload".to_string(),
            )
        })?;
        let mut imported = self.import_player_frame(decoded)?;
        imported.set_measured_scene_avg(frame.scene_avg_nits);
        if !decoded.is_videotoolbox() {
            self.software_upload_counter = self.software_upload_counter.wrapping_add(1);
        }
        self.current_frame = Some(imported);
        self.current_frame_visible = true;
        self.current_media_time = frame.pts.unwrap_or(frame.media_time);
        self.current_generation = frame.generation.max(1);
        self.upload_counter = self.upload_counter.wrapping_add(1);
        if trace::enabled() {
            trace::log(format!(
                "[erika-render-trace] stage=upload_frame gen={} pts={} media={} late={} frame_token={} elapsed_ms={:.3} size={}x{}",
                frame.generation,
                frame
                    .pts
                    .map(|pts| format!("{:.3}", pts.as_secs_f64()))
                    .unwrap_or_else(|| "-".to_string()),
                format!("{:.3}", frame.media_time.as_secs_f64()),
                frame
                    .late_by
                    .map(|duration| format!("{:.3}", duration.as_secs_f64()))
                    .unwrap_or_else(|| "-".to_string()),
                self.upload_counter,
                started.elapsed().as_secs_f64() * 1000.0,
                frame.frame.width(),
                frame.frame.height(),
            ));
        }
        Ok(())
    }

    fn render_current_frame(&mut self, context: RenderFrameContext<'_>) -> Result<bool> {
        if !self.current_frame_visible {
            return Ok(false);
        }
        let Some(frame) = self.current_frame.take() else {
            if trace::enabled() {
                trace::log(format!(
                    "[erika-render-trace] stage=render_current_frame empty gen={} media={} output={}x{}",
                    context.generation,
                    trace::duration_label(Some(context.media_time)),
                    context.output_width,
                    context.output_height,
                ));
            }
            return Ok(false);
        };
        let started = std::time::Instant::now();
        let danmaku = context.danmaku.filter(|plan| {
            plan.generation == context.generation
                && (context.output_width == 0 || plan.viewport.width == context.output_width)
                && (context.output_height == 0 || plan.viewport.height == context.output_height)
        });
        let result = self.render_video_frame_with_context(
            VideoRenderFrame::new(&frame).frame_token(self.upload_counter),
            context.overlay.map(OverlayRenderFrame::new),
            danmaku.map(DanmakuRenderFrame::new),
        );
        self.current_frame = Some(frame);
        let rendered = match result {
            Ok(()) => Ok(true),
            Err(PlayerError::RendererBackpressure(reason)) => {
                self.present_backpressure_skips = self.present_backpressure_skips.saturating_add(1);
                if should_report_present_skip(self.present_backpressure_skips) {
                    trace::diagnostic(format!(
                        "[erika-render-trace] stage=render_current_frame skipped reason={reason} count={}",
                        self.present_backpressure_skips
                    ));
                }
                if trace::enabled() {
                    trace::log(format!(
                        "[erika-render-trace] stage=render_current_frame skipped reason={reason}"
                    ));
                }
                Ok(false)
            }
            Err(error) => Err(error),
        };
        if trace::enabled() {
            trace::log(format!(
                "[erika-render-trace] stage=render_current_frame gen={} media={} output={}x{} danmaku={} elapsed_ms={:.3} result={}",
                context.generation,
                trace::duration_label(Some(context.media_time)),
                context.output_width,
                context.output_height,
                danmaku.as_ref().map_or(0, |plan| plan.items.len()),
                started.elapsed().as_secs_f64() * 1000.0,
                rendered.as_ref().is_ok_and(|rendered| *rendered),
            ));
        }
        rendered
    }

    fn set_flutter_texture_buffer(
        &mut self,
        raw_texture: u64,
        width: u32,
        height: u32,
    ) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            unsafe {
                self.inner
                    .set_flutter_texture_buffer(raw_texture as *mut c_void, width, height)
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = (raw_texture, width, height);
            Err(PlayerError::Renderer(
                "Metal Flutter textures are only available on Apple platforms".to_string(),
            ))
        }
    }

    fn capture_current_frame(
        &mut self,
        context: RenderFrameContext<'_>,
        width: u32,
        height: u32,
    ) -> Result<Option<RendererFrameCapture>> {
        let Some(frame) = self.current_frame.take() else {
            return Ok(None);
        };
        if width == 0 || height == 0 {
            self.current_frame = Some(frame);
            return Err(PlayerError::Renderer(
                "capture size must be non-zero".to_string(),
            ));
        }
        let danmaku = context.danmaku.filter(|plan| {
            plan.generation == context.generation
                && plan.viewport.width == width
                && plan.viewport.height == height
        });
        let rgba = self.capture_video_frame_rgba(
            VideoRenderFrame::new(&frame).frame_token(self.upload_counter),
            context.overlay.map(OverlayRenderFrame::new),
            danmaku.map(DanmakuRenderFrame::new),
            width,
            height,
        );
        self.current_frame = Some(frame);
        let rgba = rgba?;
        Ok(Some(RendererFrameCapture {
            width,
            height,
            rgba,
        }))
    }

    fn runtime_stats(&self) -> RendererRuntimeStats {
        let stats = self.stats();
        RendererRuntimeStats {
            surface_width: stats.drawable_width,
            surface_height: stats.drawable_height,
            rendered_frames: stats.rendered_frames,
            offscreen_frames: 0,
            prepared_overlay_frames: stats.prepared_overlay_frames,
            prepared_overlay_subtitle_planes: stats.prepared_overlay_subtitle_planes,
            danmaku_passes: stats.danmaku_passes,
            danmaku_draw_items: stats.danmaku_items,
            overlay_alpha_atlas_uploads: stats.overlay_alpha_atlas_uploads,
            overlay_alpha_atlas_reuses: stats.overlay_alpha_atlas_reuses,
            last_danmaku_atlas_duration: stats.last_danmaku_atlas_duration,
            last_danmaku_vertex_build_duration: stats.last_danmaku_vertex_build_duration,
            last_danmaku_vertex_copy_duration: stats.last_danmaku_vertex_copy_duration,
            last_danmaku_encode_duration: stats.last_danmaku_encode_duration,
            last_danmaku_vertex_bytes: stats.last_danmaku_vertex_bytes,
            last_danmaku_vertex_count: stats.last_danmaku_vertex_count,
            upscaler_mode: stats.upscaler_mode,
            upscaler_backend: stats.upscaler_backend,
            upscaler_fallbacks: stats.upscaler_fallbacks,
            upscaled_frames: stats.upscaled_frames,
            last_upscaler_encode_duration: stats.last_upscaler_encode_duration,
            last_gpu_duration: stats.last_gpu_duration,
            attached: stats.drawable_width > 0 && stats.drawable_height > 0,
            software_video_frames: self.software_upload_counter,
            hardware_video_frames: self
                .upload_counter
                .saturating_sub(self.software_upload_counter),
            zero_copy_video_frames: self
                .upload_counter
                .saturating_sub(self.software_upload_counter),
            direct_zero_copy_video_frames: self
                .upload_counter
                .saturating_sub(self.software_upload_counter),
            shared_handle_video_frames: 0,
            cpu_video_frame_fallbacks: self.software_upload_counter,
            hdr_source_frames: stats.hdr_source_frames,
            hdr10_output_frames: 0,
            sdr_tonemap_frames: stats.sdr_tonemap_frames,
            hdr10_metadata_updates: 0,
            hdr10_metadata_failures: 0,
            hdr10_output_failures: 0,
            hdr10_output_active: false,
        }
    }

    fn resource_stats(&self) -> RendererResourceStats {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            let mut stats = self.inner.resource_stats();
            stats.video_frame_bytes = self
                .current_frame
                .as_ref()
                .map_or(0, ImportedVideoFrame::allocated_bytes);
            stats.renderer_tracked_bytes = stats
                .renderer_tracked_bytes
                .saturating_add(stats.video_frame_bytes);
            stats
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            RendererResourceStats::default()
        }
    }

    fn output_status(&self) -> OutputRuntimeStatus {
        let stats = self.stats();
        let attached = stats.drawable_width > 0 && stats.drawable_height > 0;
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        let active_output_mode = self.inner.active_output_mode();
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        let active_output_mode = self.output_mode.resolve_for_source(false);
        let extended = attached && active_output_mode.is_edr();
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        let is_hdr10_pq = attached && self.inner.is_hdr10_pq();
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        let is_hdr10_pq = false;
        OutputRuntimeStatus {
            requested_mode: self.output_mode,
            active_encoding: if is_hdr10_pq {
                ActiveOutputEncoding::Hdr10Pq
            } else if extended {
                ActiveOutputEncoding::AppleEdr
            } else {
                ActiveOutputEncoding::SdrSrgb
            },
            surface_format: if extended {
                OutputSurfaceFormat::SixteenBitFloat
            } else {
                OutputSurfaceFormat::EightBitUnorm
            },
            native_data_space: -1,
            requested_headroom: self.output_mode.headroom(),
            active_headroom: if is_hdr10_pq {
                10_000.0 / 203.0
            } else if extended {
                active_output_mode.headroom()
            } else {
                1.0
            },
            active_headroom_known: attached,
            extended_linear_active: extended && !is_hdr10_pq,
            fallback_reason: OutputFallbackReason::None,
            fallback_count: 0,
            data_space_failures: 0,
            headroom_updates: stats.headroom_updates,
            extended_linear_frames: stats.edr_rendered_frames,
        }
    }

    fn set_luma_upscaler(&mut self, mode: crate::renderer::pipeline::LumaUpscalerMode) {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.set_luma_upscaler(mode);
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = mode;
        }
    }

    /// Caches the display headroom the host resolved on the main thread.
    ///
    /// `MetalRendererImpl::select_output_mode_for_source` runs on the render
    /// thread and must not query AppKit, so the host owns the `NSView` /
    /// `NSScreen` lookup and pushes the result here.
    fn set_output_headroom(&mut self, headroom: f32, known: bool) {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        {
            self.inner.set_output_headroom(headroom, known);
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
        {
            let _ = (headroom, known);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::overlay::OverlayViewport;
    use crate::renderer::pipeline::{MatrixCoefficients, SourceColorState};
    use crate::subtitle::SubtitleBitmapPlane;

    #[test]
    fn present_skip_report_fires_once_then_on_power_of_two() {
        assert!(should_report_present_skip(1));
        assert!(!should_report_present_skip(3));
        assert!(should_report_present_skip(2));
        assert!(!should_report_present_skip(255));
        assert!(should_report_present_skip(256));
        assert!(should_report_present_skip(4096));
    }

    fn test_imported_frame(
        import_range: ColorRange,
        source_color: SourceColorState,
    ) -> ImportedVideoFrame {
        ImportedVideoFrame {
            info: ImportedVideoFrameInfo {
                width: 1920,
                height: 1080,
                pixel_format: 0,
                pixel_format_fourcc: "test".to_string(),
                format: ImportedVideoFormat::P010,
                full_range: matches!(import_range, ColorRange::Full),
                color_range: import_range,
                planes: Vec::new(),
            },
            source_color,
            #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
            inner: None,
            #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
            _unsupported: (),
        }
    }

    #[test]
    fn inspect_overlay_counts_subtitle_bytes() {
        let frame = OverlayFrame {
            pts: Duration::from_secs(1),
            viewport: OverlayViewport::new(640, 360),
            subtitle_planes: vec![SubtitleBitmapPlane::new(0, 0, 10, 4, vec![255; 10 * 4 * 4])],
            subtitle_alpha_planes: Vec::new(),
            subtitle_changed: true,
        };

        let info = inspect_overlay_frame(&frame).unwrap();

        assert_eq!(info.viewport_width, 640);
        assert_eq!(info.viewport_height, 360);
        assert_eq!(info.subtitle_planes, 1);
        assert_eq!(info.subtitle_pixels, 40);
        assert_eq!(info.subtitle_bytes, 160);
    }

    #[test]
    fn inspect_overlay_counts_alpha_bitmap_bytes() {
        let frame = OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(640, 360),
            subtitle_planes: Vec::new(),
            subtitle_alpha_planes: vec![crate::subtitle::SubtitleAlphaBitmap::new(
                crate::subtitle::SubtitleBitmapPlacement::new(4, 8, 3, 2),
                5,
                0xff00ffff,
                vec![255; 8],
            )],
            subtitle_changed: true,
        };

        let info = inspect_overlay_frame(&frame).unwrap();

        assert_eq!(info.subtitle_planes, 1);
        assert_eq!(info.subtitle_pixels, 6);
        assert_eq!(info.subtitle_bytes, 8);
    }

    #[test]
    fn inspect_overlay_rejects_malformed_alpha_bitmap() {
        let frame = OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(640, 360),
            subtitle_planes: Vec::new(),
            subtitle_alpha_planes: vec![crate::subtitle::SubtitleAlphaBitmap::new(
                crate::subtitle::SubtitleBitmapPlacement::new(4, 8, 3, 2),
                5,
                0xff00ffff,
                vec![255; 7],
            )],
            subtitle_changed: true,
        };

        assert!(inspect_overlay_frame(&frame).is_err());
    }

    #[test]
    fn inspect_overlay_rejects_malformed_rgba_plane() {
        let frame = OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(1, 1),
            subtitle_planes: vec![SubtitleBitmapPlane::new(0, 0, 2, 2, vec![0; 15])],
            subtitle_alpha_planes: Vec::new(),
            subtitle_changed: true,
        };

        assert!(inspect_overlay_frame(&frame).is_err());
    }

    #[test]
    fn metal_output_mode_maps_sdr_to_default_drawable_and_target() {
        let output = MetalOutputMode::default();

        assert_eq!(
            metal_drawable_pixel_format(output),
            MetalDrawablePixelFormat::Bgra8Unorm
        );
        assert!(!output.is_edr());

        let target = metal_target_color(output, SourceColorState::default());
        assert_eq!(target.primaries, ColorPrimaries::Bt709);
        assert_eq!(target.transfer, TransferFunction::Srgb);
        assert_eq!(target.peak_nits, 100.0);
        assert_eq!(target.edr_headroom, 1.0);
    }

    #[test]
    fn metal_auto_output_starts_sdr_and_promotes_for_hdr() {
        let automatic = MetalOutputMode::auto(4.0);

        assert_eq!(
            metal_drawable_pixel_format(automatic),
            MetalDrawablePixelFormat::Bgra8Unorm
        );
        assert_eq!(automatic.resolve_for_source(false), MetalOutputMode::Sdr);
        assert_eq!(
            automatic.resolve_for_source(true),
            MetalOutputMode::apple_edr(4.0)
        );
    }

    #[test]
    fn metal_output_mode_maps_apple_edr_to_float_drawable_and_headroom_target() {
        let output = MetalOutputMode::apple_edr(4.0);

        assert_eq!(
            metal_drawable_pixel_format(output),
            MetalDrawablePixelFormat::Rgba16Float
        );
        assert!(output.is_edr());

        let target = metal_target_color(output, SourceColorState::default());
        assert_eq!(target.primaries, ColorPrimaries::Bt709);
        assert_eq!(target.transfer, TransferFunction::Srgb);
        assert_eq!(target.peak_nits, 400.0);
        assert_eq!(target.reference_white_nits, 100.0);
        assert_eq!(target.edr_headroom, 4.0);
    }

    #[test]
    fn metal_output_mode_maps_pq_source_to_pq_edr_target() {
        let output = MetalOutputMode::apple_edr(4.0);
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .nominal_peak_nits(1200.0);

        let target = metal_target_color(output, source);

        assert_eq!(target.primaries, ColorPrimaries::Bt2020);
        assert_eq!(target.transfer, TransferFunction::Pq);
        assert_eq!(target.peak_nits, 10_000.0);
        assert_eq!(target.reference_white_nits, 203.0);
        assert_eq!(target.edr_headroom, 4.0);
    }

    #[test]
    fn metal_output_mode_clamps_edr_headroom_to_one() {
        let target = metal_target_color(
            MetalOutputMode::apple_edr(0.25),
            SourceColorState::default(),
        );

        assert_eq!(target.peak_nits, 100.0);
        assert_eq!(target.reference_white_nits, 100.0);
        assert_eq!(target.edr_headroom, 1.0);
    }

    #[test]
    fn video_render_frame_uses_imported_source_color() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .range(ColorRange::Limited)
            .matrix(MatrixCoefficients::Bt2020NonConstantLuminance);
        let frame = test_imported_frame(ColorRange::Limited, source);

        let render_frame = VideoRenderFrame::new(&frame);

        assert_eq!(
            render_frame.pipeline.source.primaries,
            ColorPrimaries::Bt2020
        );
        assert_eq!(render_frame.pipeline.source.transfer, TransferFunction::Pq);
        assert_eq!(render_frame.pipeline.source.range, ColorRange::Limited);
        assert_eq!(
            render_frame.pipeline.source.matrix,
            MatrixCoefficients::Bt2020NonConstantLuminance
        );
    }

    #[test]
    fn imported_frame_prefers_pixel_buffer_range_over_metadata() {
        let mut frame = test_imported_frame(
            ColorRange::Full,
            SourceColorState::new(ColorPrimaries::Unknown, TransferFunction::Unknown)
                .range(ColorRange::Full),
        );

        frame.set_source_color_metadata(
            ColorPrimaries::Bt709,
            TransferFunction::Srgb,
            ColorRange::Limited,
            MatrixCoefficients::Bt709,
            None,
            None,
        );

        assert_eq!(frame.source_color().range, ColorRange::Full);
        assert_eq!(frame.source_color().matrix, MatrixCoefficients::Bt709);
    }

    #[test]
    fn imported_frame_applies_hdr_metadata_peak() {
        let mut frame = test_imported_frame(
            ColorRange::Limited,
            SourceColorState::new(ColorPrimaries::Unknown, TransferFunction::Unknown),
        );
        let metadata = HdrMetadata::new(
            None,
            Some(crate::renderer::pipeline::ContentLightMetadata {
                max_content_light_level_nits: 4000,
                max_frame_average_light_level_nits: 450,
            }),
        );

        frame.set_source_color_metadata(
            ColorPrimaries::Bt2020,
            TransferFunction::Pq,
            ColorRange::Limited,
            MatrixCoefficients::Bt2020NonConstantLuminance,
            Some(metadata),
            None,
        );

        assert_eq!(frame.source_color().hdr_metadata, Some(metadata));
        assert_eq!(frame.source_color().nominal_peak_nits, 4000.0);
    }

    #[test]
    fn imported_frame_uses_metadata_range_when_import_unspecified() {
        let mut frame = test_imported_frame(
            ColorRange::Unspecified,
            SourceColorState::new(ColorPrimaries::Unknown, TransferFunction::Unknown)
                .range(ColorRange::Unspecified),
        );

        frame.set_source_color(
            SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Srgb)
                .range(ColorRange::Full),
        );

        assert_eq!(frame.source_color().range, ColorRange::Full);
    }
}
