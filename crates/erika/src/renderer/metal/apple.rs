use std::ffi::c_void;
use std::mem;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use crate::core::{PlayerError, Result};
use crate::ffmpeg::{PlanarFrame, PlanarPixelFormat};
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_core_foundation::CFRetained;
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{
    CGColorSpace, kCGColorSpaceDisplayP3_PQ, kCGColorSpaceExtendedLinearSRGB,
    kCGColorSpaceITUR_2100_PQ, kCGColorSpaceSRGB,
};
use objc2_core_video::kCVReturnSuccess;
use objc2_core_video::{
    CVImageBuffer, CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture,
};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane};
use objc2_core_video::{
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth,
};
use objc2_core_video::{
    CVPixelBufferGetWidthOfPlane, kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
};
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLClearColor, MTLCreateSystemDefaultDevice, MTLLoadAction,
    MTLOrigin, MTLPixelFormat, MTLRegion, MTLResourceOptions, MTLSize, MTLStorageMode,
    MTLStoreAction, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder,
    MTLCommandQueue, MTLDevice, MTLDrawable, MTLRenderPassDescriptor, MTLResource, MTLTexture,
};
use objc2_metal::{
    MTLLibrary, MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState,
};
use objc2_metal::{MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerState};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
#[cfg(any(target_os = "ios", target_os = "tvos"))]
use objc2_quartz_core::{kCAContentsFormatRGBA8Uint, kCAContentsFormatRGBA16Float};

use crate::core::{ColorPrimaries, RendererResourceStats, SurfaceMetrics, TransferFunction};
use crate::danmaku::{DanmakuAtlasUpdate, DanmakuGlyphAtlas, DanmakuRenderPlan};
use crate::renderer::gamut::{
    GamutLut, GamutLutJob, GamutLutParams, LUT_SIZE_C, LUT_SIZE_H, LUT_SIZE_I, pack_rgba16f,
};
use crate::renderer::metal::upscaler::LumaUpscaler;
use crate::renderer::metal::{
    ClearColor, DanmakuRenderFrame, ImportedVideoFormat, ImportedVideoFrameInfo,
    ImportedVideoPlaneInfo, MetalDrawablePixelFormat, MetalOutputMode, MetalRendererConfig,
    MetalRendererStats, OverlayRenderFrame, PreparedOverlayFrameInfo, VideoAlphaMode,
    VideoFrameTextureSource, VideoRenderFrame, fourcc_string, metal_drawable_pixel_format,
    metal_target_color,
};
use crate::renderer::output::negotiate_output_mode;
use crate::renderer::pipeline::{ColorRange, DoviUniforms, LumaUpscalerMode};
use crate::renderer::pipeline::{SourceColorState, TargetColorState, VideoRenderPipeline};
use crate::renderer::presentation::PresentationLayout as VideoPresentationLayout;
use crate::subtitle::{AssColor, SubtitleAlphaBitmap};
use crate::trace;

const CV_PIXEL_FORMAT_420_YP_CB_CR10_BI_PLANAR_VIDEO_RANGE: u32 =
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange;
const CV_PIXEL_FORMAT_420_YP_CB_CR10_BI_PLANAR_FULL_RANGE: u32 =
    kCVPixelFormatType_420YpCbCr10BiPlanarFullRange;
const CV_PIXEL_FORMAT_420_YP_CB_CR8_BI_PLANAR_VIDEO_RANGE: u32 =
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
const CV_PIXEL_FORMAT_420_YP_CB_CR8_BI_PLANAR_FULL_RANGE: u32 =
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;

pub struct ImportedVideoFrameTextures {
    #[allow(dead_code)]
    source_pixel_buffer: Option<CFRetained<CVPixelBuffer>>,
    planes: Vec<ImportedVideoPlaneTexture>,
}

impl ImportedVideoFrameTextures {
    pub fn plane_count(&self) -> usize {
        self.planes.len()
    }

    pub fn allocated_bytes(&self) -> u64 {
        self.planes.iter().fold(0, |total, plane| {
            total.saturating_add(plane.metal_texture.allocatedSize() as u64)
        })
    }
}

struct ImportedVideoPlaneTexture {
    #[allow(dead_code)]
    cv_texture: Option<CFRetained<CVMetalTexture>>,
    #[allow(dead_code)]
    metal_texture: Retained<ProtocolObject<dyn MTLTexture>>,
}

impl ImportedVideoFrameTextures {
    fn luma_texture(&self) -> Option<&ProtocolObject<dyn MTLTexture>> {
        self.planes
            .first()
            .map(|plane| plane.metal_texture.as_ref())
    }

    fn chroma_texture(&self) -> Option<&ProtocolObject<dyn MTLTexture>> {
        self.planes.get(1).map(|plane| plane.metal_texture.as_ref())
    }
}

pub struct ImportedVideoFrameResult {
    pub info: ImportedVideoFrameInfo,
    pub textures: ImportedVideoFrameTextures,
}

/// Identity of a perceptual gamut LUT: it is only valid for one
/// (source, target, target-black, target-peak) combination. The black/peak
/// pair sets the LUT's I range. A cached LUT whose key does not match the
/// current frame must never be bound — the shader would sample a LUT built for
/// a different display/gamut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GamutLutKey {
    source: u32,
    target: u32,
    target_black_pq: u32,
    target_peak_pq: u32,
}

impl GamutLutKey {
    /// The key for a frame's color pipeline. It deliberately reads only the
    /// static target/primaries state: per-frame content brightness (Dolby
    /// Vision L1, measured scene average) must not force LUT regeneration.
    fn for_pipeline(pipeline: &VideoRenderPipeline) -> Self {
        let packed = pipeline.gamut_primaries_code();
        let target_black_nits = pipeline.tone_map_extra()[2];
        Self {
            source: packed >> 8,
            target: packed & 0xff,
            target_black_pq: quantize_luma_pq(target_black_nits),
            target_peak_pq: quantize_luma_pq(pipeline.target.peak_nits),
        }
    }
}

/// Quantize a luminance (nits) to its PQ code for the LUT cache key. The key
/// only has to change when the LUT's I axis changes, so 16-bit PQ resolution
/// is ample (and resolves the sub-1-nit target blacks of SDR targets, which a
/// linear nits quantization would collapse together).
fn quantize_luma_pq(nits: f32) -> u32 {
    (pq_code_for_lut(nits) * 65535.0) as u32
}

/// Cache of the generated perceptual gamut LUT for [`GamutLutKey`].
struct GamutLutCache {
    key: GamutLutKey,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
}

pub struct MetalRendererImpl {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    requested_output_mode: MetalOutputMode,
    output_mode: MetalOutputMode,
    video_alpha_mode: VideoAlphaMode,
    drawable_pixel_format: MetalDrawablePixelFormat,
    layer: Option<Retained<CAMetalLayer>>,
    flutter_texture_attached: bool,
    flutter_texture: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    texture_cache: Option<CFRetained<CVMetalTextureCache>>,
    video_pipeline: Option<Retained<ProtocolObject<dyn MTLRenderPipelineState>>>,
    overlay_pipeline: Option<Retained<ProtocolObject<dyn MTLRenderPipelineState>>>,
    danmaku_batch_pipeline: Option<Retained<ProtocolObject<dyn MTLRenderPipelineState>>>,
    video_sampler: Option<Retained<ProtocolObject<dyn MTLSamplerState>>>,
    overlay_alpha_atlas_cache: Option<OverlayAlphaAtlasCache>,
    danmaku_alpha_atlas_cache: Option<DanmakuAlphaAtlasCache>,
    danmaku_vertex_buffers: Vec<DanmakuVertexBufferSlot>,
    danmaku_vertex_buffer_cursor: usize,
    danmaku_vertex_buffer_acquisitions: u64,
    upscaler: LumaUpscaler,
    last_submitted_command_buffer: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    pending_gpu_timing: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    stats: MetalRendererStats,
    layer_color_space_label: &'static str,
    /// Perceptual gamut LUT (3D RGBA16Float) cached per (source, target,
    /// peak) key; `None` when the fast path is in use.
    gamut_lut: Option<GamutLutCache>,
    /// Background generation for a cache miss; the fast `gamut_compress`
    /// path renders until the LUT lands.
    gamut_lut_job: Option<GamutLutJob>,
    dummy_gamut_lut: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    logged_first_video_frame: bool,
    /// EDR headroom of the display the layer is presented on, published by
    /// the host through `RendererBackend::set_output_headroom`.
    ///
    /// Read here instead of querying AppKit: `select_output_mode_for_source`
    /// runs on the render thread (CVDisplayLink or the host's render queue),
    /// where touching `NSView`/`NSWindow`/`NSApplication` violates AppKit's
    /// main-thread requirement. `None` means no host has reported a value
    /// yet, which negotiates as SDR — the same fallback this used when AppKit
    /// could not answer.
    reported_display_headroom: Option<f32>,
}

fn hdr_debug_enabled() -> bool {
    std::env::var("ERIKA_HDR_DEBUG")
        .ok()
        .map(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

fn pq_code_for_lut(nits: f32) -> f32 {
    let m1 = 0.1593017578125_f32;
    let m2 = 78.84375_f32;
    let c1 = 0.8359375_f32;
    let c2 = 18.8515625_f32;
    let c3 = 18.6875_f32;
    let p = (nits / 10000.0).clamp(0.0, 1.0).powf(m1);
    ((c1 + c2 * p) / (1.0 + c3 * p).max(0.000_001)).powf(m2)
}

fn code_to_primaries(code: u32) -> ColorPrimaries {
    match code {
        1 => ColorPrimaries::Bt2020,
        2 => ColorPrimaries::DisplayP3,
        _ => ColorPrimaries::Bt709,
    }
}

impl MetalRendererImpl {
    pub fn new(config: MetalRendererConfig) -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().ok_or_else(|| {
            PlayerError::Renderer("MTLCreateSystemDefaultDevice returned nil".to_string())
        })?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| PlayerError::Renderer("newCommandQueue returned nil".to_string()))?;
        let initial_output_mode = config.output_mode.resolve_for_source(false);
        Ok(Self {
            device,
            queue,
            requested_output_mode: config.output_mode,
            output_mode: initial_output_mode,
            video_alpha_mode: config.video_alpha_mode,
            drawable_pixel_format: metal_drawable_pixel_format(initial_output_mode),
            layer: None,
            flutter_texture_attached: false,
            flutter_texture: None,
            texture_cache: None,
            video_pipeline: None,
            overlay_pipeline: None,
            danmaku_batch_pipeline: None,
            video_sampler: None,
            overlay_alpha_atlas_cache: None,
            danmaku_alpha_atlas_cache: None,
            danmaku_vertex_buffers: Vec::new(),
            danmaku_vertex_buffer_cursor: 0,
            danmaku_vertex_buffer_acquisitions: 0,
            upscaler: {
                let mut upscaler = LumaUpscaler::default();
                upscaler.set_mode(config.luma_upscaler);
                upscaler
            },
            last_submitted_command_buffer: None,
            pending_gpu_timing: None,
            stats: MetalRendererStats::default(),
            layer_color_space_label: "unconfigured",
            gamut_lut: None,
            gamut_lut_job: None,
            dummy_gamut_lut: None,
            logged_first_video_frame: false,
            reported_display_headroom: None,
        })
    }

    pub unsafe fn attach_raw_layer(
        &mut self,
        layer: *mut c_void,
        metrics: SurfaceMetrics,
    ) -> Result<()> {
        if layer.is_null() {
            return Err(PlayerError::Renderer(
                "cannot attach null CAMetalLayer".to_string(),
            ));
        }
        let layer: Retained<CAMetalLayer> = unsafe { Retained::retain(layer.cast()) }
            .ok_or_else(|| PlayerError::Renderer("failed to retain CAMetalLayer".to_string()))?;
        // CAMetalLayer defaults to true. Restore a finite wait if a host-owned
        // layer disabled it: nextDrawable can wait up to one second, then return
        // nil, instead of waiting indefinitely. This is not a nonblocking call;
        // nil maps to RendererBackpressure.
        layer.setAllowsNextDrawableTimeout(true);
        layer.setDevice(Some(&*self.device));
        self.configure_layer_output(&layer);
        self.layer = Some(layer);
        self.flutter_texture_attached = false;
        self.flutter_texture = None;
        self.resize_surface(metrics);
        Ok(())
    }

    pub fn attach_flutter_texture(&mut self, metrics: SurfaceMetrics) {
        self.layer = None;
        self.flutter_texture_attached = true;
        self.flutter_texture = None;
        self.output_mode = MetalOutputMode::Sdr;
        self.drawable_pixel_format = MetalDrawablePixelFormat::Bgra8Unorm;
        self.video_pipeline = None;
        self.overlay_pipeline = None;
        self.danmaku_batch_pipeline = None;
        self.resize_surface(metrics);
    }

    pub unsafe fn set_flutter_texture_buffer(
        &mut self,
        raw_texture: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<()> {
        if !self.flutter_texture_attached {
            return Err(PlayerError::Renderer(
                "no Flutter texture surface attached".to_string(),
            ));
        }
        if raw_texture.is_null() || width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "Flutter Metal texture and dimensions must be non-zero".to_string(),
            ));
        }
        let texture: Retained<ProtocolObject<dyn MTLTexture>> = unsafe {
            Retained::retain(raw_texture.cast())
        }
        .ok_or_else(|| PlayerError::Renderer("failed to retain Flutter MTLTexture".to_string()))?;
        if texture.pixelFormat() != MTLPixelFormat::BGRA8Unorm {
            return Err(PlayerError::Renderer(format!(
                "Flutter MTLTexture must use BGRA8Unorm, got {:?}",
                texture.pixelFormat()
            )));
        }
        if texture.width() != width as usize || texture.height() != height as usize {
            return Err(PlayerError::Renderer(format!(
                "Flutter MTLTexture size {}x{} does not match {width}x{height}",
                texture.width(),
                texture.height()
            )));
        }
        self.stats.drawable_width = width;
        self.stats.drawable_height = height;
        self.flutter_texture = Some(texture);
        Ok(())
    }

    pub fn detach_surface(&mut self) {
        self.layer = None;
        self.flutter_texture_attached = false;
        self.flutter_texture = None;
    }

    pub fn resize_surface(&mut self, metrics: SurfaceMetrics) {
        let (width, height) = metrics.physical_size();
        let drawable_width = width as f64;
        let drawable_height = height as f64;
        self.stats.drawable_width = drawable_width.round() as u32;
        self.stats.drawable_height = drawable_height.round() as u32;
        if let Some(layer) = &self.layer {
            let size = CGSize::new(drawable_width, drawable_height);
            layer.setDrawableSize(size);
        }
        if self.flutter_texture_attached {
            self.flutter_texture = None;
        }
    }

    pub fn stats(&self) -> MetalRendererStats {
        let mut stats = self.stats;
        stats.upscaler_mode = self.upscaler.mode();
        stats.upscaler_backend = self.upscaler.active_backend();
        stats.upscaler_fallbacks = self.upscaler.auto_matmul_fallbacks();
        stats
    }

    pub fn resource_stats(&self) -> RendererResourceStats {
        let drawable_count = self.layer.as_ref().map_or(0, |layer| {
            layer.maximumDrawableCount().min(u32::MAX as usize) as u32
        });
        let drawable_bytes_per_pixel = match self.drawable_pixel_format {
            MetalDrawablePixelFormat::Bgra8Unorm => 4u64,
            MetalDrawablePixelFormat::Rgba16Float => 8u64,
        };
        let drawable_estimated_bytes = u64::from(self.stats.drawable_width)
            .saturating_mul(u64::from(self.stats.drawable_height))
            .saturating_mul(drawable_bytes_per_pixel)
            .saturating_mul(u64::from(drawable_count));
        let overlay_atlas_bytes = self
            .overlay_alpha_atlas_cache
            .as_ref()
            .map_or(0, |cache| cache.texture.allocatedSize() as u64);
        let danmaku_atlas_bytes = self.danmaku_alpha_atlas_cache.as_ref().map_or(0, |cache| {
            (cache.fill_texture.allocatedSize() as u64)
                .saturating_add(cache.outline_texture.allocatedSize() as u64)
        });
        let danmaku_vertex_buffer_bytes =
            self.danmaku_vertex_buffers
                .iter()
                .fold(0u64, |total, slot| {
                    total.saturating_add(
                        slot.buffer
                            .as_ref()
                            .map_or(0, |buffer| buffer.allocatedSize() as u64),
                    )
                });
        let upscaler_bytes = self.upscaler.allocated_bytes();
        let renderer_tracked_bytes = drawable_estimated_bytes
            .saturating_add(overlay_atlas_bytes)
            .saturating_add(danmaku_atlas_bytes)
            .saturating_add(danmaku_vertex_buffer_bytes)
            .saturating_add(upscaler_bytes);

        let device_recommended_working_set_bytes = if self
            .device
            .respondsToSelector(objc2::sel!(recommendedMaxWorkingSetSize))
        {
            self.device.recommendedMaxWorkingSetSize()
        } else {
            0
        };

        RendererResourceStats {
            device_current_allocated_bytes: self.device.currentAllocatedSize() as u64,
            device_recommended_working_set_bytes,
            drawable_estimated_bytes,
            overlay_atlas_bytes,
            danmaku_atlas_bytes,
            danmaku_vertex_buffer_bytes,
            upscaler_bytes,
            renderer_tracked_bytes,
            drawable_count,
            output_mode_switches: self.stats.output_mode_switches,
            ..RendererResourceStats::default()
        }
    }

    pub fn active_output_mode(&self) -> MetalOutputMode {
        self.output_mode
    }

    pub fn is_hdr10_pq(&self) -> bool {
        self.output_mode.is_edr()
            && matches!(
                self.layer_color_space_label,
                "itur-2100-pq" | "display-p3-pq"
            )
    }

    /// EDR headroom of the display the player window is presented on, as
    /// published by the host through `RendererBackend::set_output_headroom`.
    ///
    /// The host resolves this on the main thread (it owns the `NSView` and
    /// knows which screen the window is on) and pushes it here; querying
    /// AppKit from this renderer would run on the render thread and violate
    /// AppKit's main-thread requirement. No report yet negotiates as SDR.
    fn display_edr_headroom(&self) -> f32 {
        self.reported_display_headroom.unwrap_or(1.0)
    }

    /// Records a host-reported display headroom. `known == false` clears the
    /// cache so negotiation falls back to SDR until a real value arrives.
    pub fn set_output_headroom(&mut self, headroom: f32, known: bool) {
        let resolved = if known && headroom.is_finite() && headroom > 0.0 {
            Some(headroom)
        } else {
            None
        };
        if resolved != self.reported_display_headroom {
            self.reported_display_headroom = resolved;
            self.stats.headroom_updates = self.stats.headroom_updates.saturating_add(1);
        }
    }

    fn select_output_mode_for_source(&mut self, source: SourceColorState) {
        let source_is_hdr = source.is_hdr();
        if source_is_hdr {
            self.stats.hdr_source_frames = self.stats.hdr_source_frames.saturating_add(1);
        }
        // Flutter's macOS texture registrar currently consumes BGRA8
        // CVPixelBuffers, so this compositor path is explicitly SDR. HDR input
        // is tone-mapped rather than changing the external texture format.
        let selected = if self.flutter_texture_attached {
            MetalOutputMode::Sdr
        } else {
            #[cfg(target_os = "macos")]
            {
                negotiate_output_mode(
                    self.requested_output_mode,
                    source_is_hdr,
                    self.display_edr_headroom(),
                )
            }
            #[cfg(not(target_os = "macos"))]
            {
                self.requested_output_mode.resolve_for_source(source_is_hdr)
            }
        };
        if selected != self.output_mode {
            self.set_output_mode(selected);
        }
        if source_is_hdr && !self.output_mode.is_edr() {
            self.stats.sdr_tonemap_frames = self.stats.sdr_tonemap_frames.saturating_add(1);
        }
    }

    fn set_output_mode(&mut self, output_mode: MetalOutputMode) {
        if self.output_mode == output_mode {
            return;
        }
        // CAMetalLayer requires all drawables using the old pixel format to be
        // released before its format changes. Waiting for the most recently
        // submitted buffer is sufficient because this renderer uses one FIFO
        // Metal command queue.
        if let Some(submitted) = self.last_submitted_command_buffer.take() {
            submitted.waitUntilCompleted();
        }
        if let Some(pending) = self.pending_gpu_timing.take() {
            pending.waitUntilCompleted();
            if pending.status() == MTLCommandBufferStatus::Completed {
                let seconds = (pending.GPUEndTime() - pending.GPUStartTime()).max(0.0);
                self.stats.last_gpu_duration = Duration::from_secs_f64(seconds);
            }
        }
        let previous = self.output_mode;
        self.output_mode = output_mode;
        self.stats.output_mode_switches = self.stats.output_mode_switches.saturating_add(1);
        self.logged_first_video_frame = false;
        if let Some(layer) = self.layer.as_ref().cloned() {
            self.configure_layer_output(&layer);
        }
        if hdr_debug_enabled() {
            eprintln!(
                "ErikaHDR: automatic output switch requested={:?} previous={:?} active={:?}",
                self.requested_output_mode, previous, self.output_mode,
            );
        }
    }

    fn configure_layer_output(&mut self, layer: &CAMetalLayer) {
        self.drawable_pixel_format = metal_drawable_pixel_format(self.output_mode);
        layer.setPixelFormat(metal_pixel_format(self.drawable_pixel_format));
        configure_layer_dynamic_range(layer, self.output_mode.is_edr());
        let (color_space_name, color_space_label) = if self.output_mode.is_edr() {
            edr_layer_color_space(None)
        } else {
            (Some(unsafe { kCGColorSpaceSRGB }), "srgb")
        };
        if let Some(color_space) = CGColorSpace::with_name(color_space_name) {
            layer.setColorspace(Some(&color_space));
        }
        self.video_pipeline = None;
        self.overlay_pipeline = None;
        self.danmaku_batch_pipeline = None;
        self.overlay_alpha_atlas_cache = None;
        self.danmaku_vertex_buffers.clear();
        self.danmaku_vertex_buffer_cursor = 0;
        self.danmaku_vertex_buffer_acquisitions = 0;
        self.layer_color_space_label = color_space_label;
        if hdr_debug_enabled() {
            eprintln!(
                "ErikaHDR: renderer configure layer output mode={:?} drawable_format={:?} metal_pixel_format={:?} edr={} colorspace={}",
                self.output_mode,
                self.drawable_pixel_format,
                metal_pixel_format(self.drawable_pixel_format),
                self.output_mode.is_edr(),
                color_space_label,
            );
        }
    }

    fn configure_layer_source_color(
        &mut self,
        layer: &CAMetalLayer,
        source: crate::renderer::pipeline::SourceColorState,
    ) {
        #[cfg(any(target_os = "ios", target_os = "tvos"))]
        {
            let _ = layer;
            let _ = source;
            return;
        }

        #[cfg(not(any(target_os = "ios", target_os = "tvos")))]
        {
            if !self.output_mode.is_edr() {
                return;
            }
            let (color_space_name, color_space_label) = edr_layer_color_space(Some(source));
            if color_space_label == self.layer_color_space_label {
                return;
            }
            if let Some(color_space) = CGColorSpace::with_name(color_space_name) {
                layer.setColorspace(Some(&color_space));
                self.layer_color_space_label = color_space_label;
                if hdr_debug_enabled() {
                    eprintln!(
                        "ErikaHDR: renderer source colorspace updated source={:?}/{:?} colorspace={}",
                        source.primaries, source.transfer, color_space_label,
                    );
                }
            }
        }
    }

    pub fn record_prepared_overlay_frame(&mut self, info: PreparedOverlayFrameInfo) {
        self.stats.prepared_overlay_frames += 1;
        self.stats.prepared_overlay_subtitle_planes += info.subtitle_planes as u64;
    }

    pub fn has_surface(&self) -> bool {
        self.layer.is_some() || self.flutter_texture_attached
    }

    pub fn render_clear(&mut self, color: ClearColor) -> Result<()> {
        let started = Instant::now();
        let color = if self.video_alpha_mode.has_alpha() {
            ClearColor {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 0.0,
            }
        } else {
            color
        };

        unsafe {
            let (drawable, texture) = if let Some(layer) = &self.layer {
                let Some(drawable): Option<Retained<ProtocolObject<dyn CAMetalDrawable>>> =
                    layer.nextDrawable()
                else {
                    return Err(PlayerError::RendererBackpressure(
                        "CAMetalLayer nextDrawable returned nil".to_string(),
                    ));
                };
                let texture = drawable.texture();
                (Some(drawable), texture)
            } else if let Some(texture) = self.flutter_texture.as_ref().cloned() {
                (None, texture)
            } else {
                return Err(PlayerError::RendererBackpressure(
                    "Flutter texture buffer is not ready".to_string(),
                ));
            };

            let descriptor = MTLRenderPassDescriptor::new();
            let attachments = descriptor.colorAttachments();
            let attachment = attachments.objectAtIndexedSubscript(0);
            attachment.setTexture(Some(&*texture));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: color.red,
                green: color.green,
                blue: color.blue,
                alpha: color.alpha,
            });

            let Some(command_buffer) = self.queue.commandBuffer() else {
                return Err(PlayerError::Renderer(
                    "commandBuffer returned nil".to_string(),
                ));
            };
            let Some(encoder) = command_buffer.renderCommandEncoderWithDescriptor(&descriptor)
            else {
                return Err(PlayerError::Renderer(
                    "renderCommandEncoderWithDescriptor returned nil".to_string(),
                ));
            };
            encoder.endEncoding();
            if let Some(drawable) = &drawable {
                let drawable_ref: &ProtocolObject<dyn MTLDrawable> =
                    ProtocolObject::from_ref(&**drawable);
                command_buffer.presentDrawable(drawable_ref);
            }
            command_buffer.commit();
            if drawable.is_none() {
                command_buffer.waitUntilCompleted();
                if command_buffer.status() != MTLCommandBufferStatus::Completed {
                    return Err(PlayerError::Renderer(format!(
                        "Flutter texture clear failed with status {:?}",
                        command_buffer.status()
                    )));
                }
            }
            self.last_submitted_command_buffer = Some(command_buffer);
        }

        self.stats.rendered_frames += 1;
        if trace::enabled() {
            trace::log(format!(
                "[erika-render-trace] stage=clear elapsed_ms={:.3} color={:.3},{:.3},{:.3},{:.3}",
                started.elapsed().as_secs_f64() * 1000.0,
                color.red,
                color.green,
                color.blue,
                color.alpha,
            ));
        }

        Ok(())
    }

    /// Return a cached (or freshly generated) perceptual gamut LUT texture
    /// for the frame's color pipeline, or `None` when the fast path is used
    /// or the background generation is still pending. `Some` is returned only
    /// when the texture matches the frame's key; callers must mask
    /// `gamut_lut_enabled` off on `None` so the shader keeps the fast path
    /// instead of sampling the 1x1x1 placeholder.
    fn gamut_lut_texture(
        &mut self,
        frame: &VideoRenderFrame<'_>,
    ) -> Result<Option<Retained<ProtocolObject<dyn MTLTexture>>>> {
        if !frame.pipeline.gamut_lut_active() {
            return Ok(None);
        }
        let key = GamutLutKey::for_pipeline(&frame.pipeline);
        if let Some(cached) = &self.gamut_lut {
            if cached.key == key {
                return Ok(Some(cached.texture.clone()));
            }
        }
        let params = GamutLutParams {
            source: code_to_primaries(key.source),
            target: code_to_primaries(key.target),
            // Same target black the shader derives from tone_map_extra.z.
            min_luma: pq_code_for_lut(frame.pipeline.tone_map_extra()[2]),
            max_luma: pq_code_for_lut(frame.pipeline.target.peak_nits),
        };
        let job_params = self
            .gamut_lut_job
            .as_ref()
            .map(GamutLutJob::params)
            .filter(|job_params| *job_params == params);
        if job_params.is_none() {
            // First request (or the key changed): spawn generation and keep
            // the fast path for this frame.
            self.gamut_lut_job = Some(GamutLutJob::spawn(params));
            return Ok(None);
        }
        let Some(lut) = self.gamut_lut_job.as_ref().and_then(GamutLutJob::poll) else {
            return Ok(None);
        };
        self.gamut_lut_job = None;
        let texture = self.upload_gamut_lut(&lut)?;
        self.gamut_lut = Some(GamutLutCache {
            key,
            texture: texture.clone(),
        });
        Ok(Some(texture))
    }

    /// Pack (I, P+0.5, T+0.5) into a fresh 3D RGBA16Float texture.
    fn upload_gamut_lut(
        &mut self,
        lut: &GamutLut,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        // Metal lacks a 3D convenience constructor in this binding; build the
        // descriptor from the 2D factory and switch the type/depth.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA16Float,
                LUT_SIZE_I,
                LUT_SIZE_C,
                false,
            )
        };
        descriptor.setTextureType(MTLTextureType::Type3D);
        unsafe {
            descriptor.setDepth(LUT_SIZE_H);
        }
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setResourceOptions(MTLResourceOptions::StorageModeShared);
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| {
                PlayerError::Renderer(
                    "newTextureWithDescriptor (gamut LUT) returned nil".to_string(),
                )
            })?;
        let rgba16 = pack_rgba16f(&lut.texels, 1.0);
        let region = MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: objc2_metal::MTLSize {
                width: LUT_SIZE_I,
                height: LUT_SIZE_C,
                depth: LUT_SIZE_H,
            },
        };
        let bytes_per_row = LUT_SIZE_I * 4 * 2; // RGBA16F = 8 bytes/texel
        unsafe {
            texture.replaceRegion_mipmapLevel_slice_withBytes_bytesPerRow_bytesPerImage(
                region,
                0,
                0,
                NonNull::new(rgba16.as_ptr().cast::<c_void>().cast_mut())
                    .expect("gamut lut pointer is non-null"),
                bytes_per_row,
                LUT_SIZE_C * bytes_per_row, // one z-slice = width * bytes_per_row
            );
        }
        Ok(texture)
    }

    fn dummy_gamut_lut_texture(&mut self) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        if let Some(dummy) = &self.dummy_gamut_lut {
            return Ok(dummy.clone());
        }
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA16Float,
                1,
                1,
                false,
            )
        };
        descriptor.setTextureType(MTLTextureType::Type3D);
        unsafe {
            descriptor.setDepth(1);
        }
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setResourceOptions(MTLResourceOptions::StorageModeShared);
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| {
                PlayerError::Renderer(
                    "newTextureWithDescriptor (dummy gamut LUT) returned nil".to_string(),
                )
            })?;
        // 1 texel: (I=0, P=0, T=0) stored with P+0.5, T+0.5 -> [0.0, 0.5, 0.5, 1.0]
        let dummy_data: [u16; 4] = [0x0000, 0x3800, 0x3800, 0x3c00];
        let region = MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: objc2_metal::MTLSize {
                width: 1,
                height: 1,
                depth: 1,
            },
        };
        unsafe {
            texture.replaceRegion_mipmapLevel_slice_withBytes_bytesPerRow_bytesPerImage(
                region,
                0,
                0,
                NonNull::new(dummy_data.as_ptr().cast::<c_void>().cast_mut())
                    .expect("dummy lut pointer is non-null"),
                8,
                8,
            );
        }
        self.dummy_gamut_lut = Some(texture.clone());
        Ok(texture)
    }

    pub fn render_video_frame(&mut self, frame: VideoRenderFrame<'_>) -> Result<()> {
        self.render_video_frame_inner(frame, None, None)
    }

    pub fn render_video_frame_with_overlay(
        &mut self,
        frame: VideoRenderFrame<'_>,
        overlay: OverlayRenderFrame<'_>,
    ) -> Result<()> {
        self.render_video_frame_inner(frame, Some(overlay), None)
    }

    pub fn render_video_frame_with_context(
        &mut self,
        frame: VideoRenderFrame<'_>,
        overlay: Option<OverlayRenderFrame<'_>>,
        danmaku: Option<DanmakuRenderFrame<'_>>,
    ) -> Result<()> {
        self.render_video_frame_inner(frame, overlay, danmaku)
    }

    pub fn render_overlay_frame(&mut self, overlay: OverlayRenderFrame<'_>) -> Result<()> {
        unsafe {
            let (drawable, texture) = if let Some(layer) = &self.layer {
                let Some(drawable): Option<Retained<ProtocolObject<dyn CAMetalDrawable>>> =
                    layer.nextDrawable()
                else {
                    return Err(PlayerError::RendererBackpressure(
                        "CAMetalLayer nextDrawable returned nil".to_string(),
                    ));
                };
                let texture = drawable.texture();
                (Some(drawable), texture)
            } else if let Some(texture) = self.flutter_texture.as_ref().cloned() {
                (None, texture)
            } else {
                return Err(PlayerError::RendererBackpressure(
                    "Flutter texture buffer is not ready".to_string(),
                ));
            };

            let descriptor = MTLRenderPassDescriptor::new();
            let attachments = descriptor.colorAttachments();
            let attachment = attachments.objectAtIndexedSubscript(0);
            attachment.setTexture(Some(&*texture));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0,
            });

            let Some(command_buffer) = self.queue.commandBuffer() else {
                return Err(PlayerError::Renderer(
                    "commandBuffer returned nil".to_string(),
                ));
            };
            let Some(encoder) = command_buffer.renderCommandEncoderWithDescriptor(&descriptor)
            else {
                return Err(PlayerError::Renderer(
                    "renderCommandEncoderWithDescriptor returned nil".to_string(),
                ));
            };
            let layout = VideoPresentationLayout::aspect_fit(
                overlay.frame.viewport.width,
                overlay.frame.viewport.height,
                self.stats.drawable_width,
                self.stats.drawable_height,
            );
            self.draw_overlay_planes(
                &encoder,
                overlay,
                layout,
                metal_target_color(
                    self.output_mode,
                    crate::renderer::pipeline::SourceColorState::default(),
                ),
            )?;
            encoder.endEncoding();
            if let Some(drawable) = &drawable {
                let drawable_ref: &ProtocolObject<dyn MTLDrawable> =
                    ProtocolObject::from_ref(&**drawable);
                command_buffer.presentDrawable(drawable_ref);
            }
            command_buffer.commit();
            if drawable.is_none() {
                command_buffer.waitUntilCompleted();
                if command_buffer.status() != MTLCommandBufferStatus::Completed {
                    return Err(PlayerError::Renderer(format!(
                        "Flutter texture overlay failed with status {:?}",
                        command_buffer.status()
                    )));
                }
            }
            self.last_submitted_command_buffer = Some(command_buffer);
        }

        self.stats.rendered_frames += 1;
        Ok(())
    }

    fn render_video_frame_inner(
        &mut self,
        mut frame: VideoRenderFrame<'_>,
        overlay: Option<OverlayRenderFrame<'_>>,
        danmaku: Option<DanmakuRenderFrame<'_>>,
    ) -> Result<()> {
        let started = Instant::now();
        let danmaku_item_count = danmaku
            .as_ref()
            .map_or(0usize, |danmaku| danmaku.plan.items.len());
        let mut upscaled_luma_used = false;
        let has_overlay = overlay.is_some();
        self.stats.last_danmaku_atlas_duration = Duration::ZERO;
        self.stats.last_danmaku_vertex_build_duration = Duration::ZERO;
        self.stats.last_danmaku_vertex_copy_duration = Duration::ZERO;
        self.stats.last_danmaku_encode_duration = Duration::ZERO;
        self.stats.last_danmaku_vertex_bytes = 0;
        self.stats.last_danmaku_vertex_count = 0;
        let Some(textures) = frame.frame.inner.as_ref() else {
            return Err(PlayerError::Renderer(
                "imported video frame has no Metal textures".to_string(),
            ));
        };
        let Some(luma) = textures.luma_texture() else {
            return Err(PlayerError::Renderer(
                "imported video frame has no luma plane".to_string(),
            ));
        };
        let Some(chroma) = textures.chroma_texture() else {
            return Err(PlayerError::Renderer(
                "imported video frame has no chroma plane".to_string(),
            ));
        };

        let source_color = frame.pipeline.source;
        self.select_output_mode_for_source(source_color);
        if let Some(layer) = self.layer.as_ref().cloned() {
            self.configure_layer_source_color(&layer, source_color);
        }
        frame.pipeline = frame
            .pipeline
            .with_target(metal_target_color(self.output_mode, source_color));
        if !self.logged_first_video_frame {
            self.logged_first_video_frame = true;
            if hdr_debug_enabled() {
                let info = &frame.frame.info;
                eprintln!(
                    "ErikaHDR: first Metal video frame output_mode={:?} drawable_format={:?} layer_colorspace={} size={}x{} fourcc={} format={:?} full_range={} range={:?} primaries={:?} transfer={:?} matrix={:?} hdr_metadata={:?} source_peak_nits={:.1} source_white_nits={:.1} target={:?}",
                    self.output_mode,
                    self.drawable_pixel_format,
                    self.layer_color_space_label,
                    info.width,
                    info.height,
                    info.pixel_format_fourcc,
                    info.format,
                    info.full_range,
                    frame.pipeline.source.range,
                    frame.pipeline.source.primaries,
                    frame.pipeline.source.transfer,
                    frame.pipeline.source.matrix,
                    frame.pipeline.source.hdr_metadata,
                    frame.pipeline.source.nominal_peak_nits,
                    frame.pipeline.source.reference_white_nits,
                    frame.pipeline.target,
                );
            }
        }

        self.collect_gpu_timing();
        self.stats.last_upscaler_encode_duration = Duration::ZERO;

        let logical_width = self
            .video_alpha_mode
            .logical_width(frame.frame.info.width as u32);
        let layout = VideoPresentationLayout::aspect_fit(
            logical_width,
            frame.frame.info.height as u32,
            self.stats.drawable_width,
            self.stats.drawable_height,
        );
        // The neural doubler only pays off when the video is actually shown
        // larger than its source resolution.
        let upscale_requested = self.upscaler.mode().is_enabled()
            && layout.is_source_upscaled()
            && matches!(
                luma.pixelFormat(),
                MTLPixelFormat::R8Unorm | MTLPixelFormat::R16Unorm
            );

        unsafe {
            let pipeline = self.video_pipeline_state()?;
            let sampler = self.video_sampler_state()?;
            let Some(command_buffer) = self.queue.commandBuffer() else {
                return Err(PlayerError::Renderer(
                    "commandBuffer returned nil".to_string(),
                ));
            };

            let mut upscaled_luma = None;
            if upscale_requested {
                let encode_start = Instant::now();
                match self.upscaler.encode_with_token(
                    &self.device,
                    &command_buffer,
                    luma,
                    luma.pixelFormat(),
                    frame.frame_token,
                ) {
                    Ok(texture) => {
                        self.stats.last_upscaler_encode_duration = encode_start.elapsed();
                        upscaled_luma = texture;
                    }
                    Err(error) => {
                        eprintln!("Erika luma upscaler disabled after encode failure: {error}");
                        self.upscaler.set_mode(LumaUpscalerMode::Off);
                    }
                }
            }
            if upscaled_luma.is_some() {
                self.stats.upscaled_frames += 1;
                frame.pipeline = frame.pipeline.with_luma_upscaler(self.upscaler.mode());
                upscaled_luma_used = true;
            }
            let luma: &ProtocolObject<dyn MTLTexture> = upscaled_luma.as_deref().unwrap_or(luma);

            let (drawable, texture) = if let Some(layer) = &self.layer {
                let Some(drawable): Option<Retained<ProtocolObject<dyn CAMetalDrawable>>> =
                    layer.nextDrawable()
                else {
                    return Err(PlayerError::RendererBackpressure(
                        "CAMetalLayer nextDrawable returned nil".to_string(),
                    ));
                };
                let texture = drawable.texture();
                (Some(drawable), texture)
            } else if let Some(texture) = self.flutter_texture.as_ref().cloned() {
                (None, texture)
            } else {
                return Err(PlayerError::RendererBackpressure(
                    "Flutter texture buffer is not ready".to_string(),
                ));
            };

            let descriptor = MTLRenderPassDescriptor::new();
            let attachments = descriptor.colorAttachments();
            let attachment = attachments.objectAtIndexedSubscript(0);
            attachment.setTexture(Some(&*texture));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: if self.video_alpha_mode.has_alpha() {
                    0.0
                } else {
                    1.0
                },
            });

            let Some(encoder) = command_buffer.renderCommandEncoderWithDescriptor(&descriptor)
            else {
                return Err(PlayerError::Renderer(
                    "renderCommandEncoderWithDescriptor returned nil".to_string(),
                ));
            };
            // Resolve the LUT before building the uniforms: while a new key's
            // background generation is pending, the shader keeps the fast
            // gamut_compress path (the dummy texture holds texture 2).
            // Readiness comes from the returned texture, not from the cache:
            // a stale cache entry for another key must not enable the LUT.
            let cached_gamut_lut = self.gamut_lut_texture(&frame)?;
            let gamut_lut_ready = cached_gamut_lut.is_some();
            let gamut_lut = match cached_gamut_lut {
                Some(lut) => lut,
                None => self.dummy_gamut_lut_texture()?,
            };
            let uniforms = VideoUniforms {
                is_p010: matches!(frame.frame.info.format, ImportedVideoFormat::P010) as u32,
                full_range: matches!(frame.pipeline.source.range, ColorRange::Full) as u32,
                source_transfer: transfer_code(frame.pipeline.source.transfer),
                target_transfer: transfer_code(frame.pipeline.target.transfer),
                tone_map: frame.pipeline.tone_map_uniform_code(),
                edr_output: self.output_mode.is_edr() as u32,
                _reserved0: self.video_alpha_mode as u32,
                _reserved1: 0,
                rect: layout.target_rect,
                viewport: layout.video_viewport(),
                nits: [
                    frame.pipeline.source.nominal_peak_nits,
                    frame.pipeline.target.peak_nits,
                    frame.pipeline.source.reference_white_nits,
                    frame.pipeline.target.reference_white_nits,
                ],
                luma_coefficients: luma_coefficients(frame.pipeline.luma_coefficients()),
                gamut_matrix_rows: frame.pipeline.gamut_matrix().row4s(),
                ipt_matrix_rows: frame.pipeline.ipt_matrix_rows(),
                tone_map_extra: frame.pipeline.tone_map_extra(),
                tone_map_coeffs: frame.pipeline.tone_map_coeffs(),
                gamut_lut_enabled: if gamut_lut_ready {
                    frame.pipeline.gamut_lut_active() as u32
                } else {
                    0
                },
                gamut_primaries: frame.pipeline.gamut_primaries_code(),
                gamut_reserved0: 0,
                gamut_reserved1: 0,
                dovi: DoviUniforms::of_for_representation(
                    &frame.pipeline.source,
                    matches!(frame.frame.info.format, ImportedVideoFormat::P010),
                ),
            };
            encoder.setRenderPipelineState(&pipeline);
            encoder.setFragmentTexture_atIndex(Some(luma), 0);
            encoder.setFragmentTexture_atIndex(Some(chroma), 1);
            encoder.setFragmentTexture_atIndex(Some(&gamut_lut), 2);
            encoder.setFragmentSamplerState_atIndex(Some(&sampler), 0);
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(
                    (&uniforms as *const VideoUniforms)
                        .cast::<c_void>()
                        .cast_mut(),
                )
                .expect("uniform pointer is non-null"),
                mem::size_of::<VideoUniforms>(),
                0,
            );
            encoder.setFragmentBytes_length_atIndex(
                NonNull::new(
                    (&uniforms as *const VideoUniforms)
                        .cast::<c_void>()
                        .cast_mut(),
                )
                .expect("uniform pointer is non-null"),
                mem::size_of::<VideoUniforms>(),
                0,
            );
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);

            let target_color = frame.pipeline.target;
            if let Some(overlay) = overlay {
                self.draw_overlay_planes(&encoder, overlay, layout, target_color)?;
            }
            if let Some(danmaku) = danmaku.as_ref() {
                self.draw_danmaku_plan(
                    &encoder,
                    &command_buffer,
                    danmaku.plan,
                    layout,
                    target_color,
                )?;
            }

            encoder.endEncoding();
            if let Some(drawable) = &drawable {
                let drawable_ref: &ProtocolObject<dyn MTLDrawable> =
                    ProtocolObject::from_ref(&**drawable);
                command_buffer.presentDrawable(drawable_ref);
            }
            command_buffer.commit();
            if drawable.is_none() {
                command_buffer.waitUntilCompleted();
                if command_buffer.status() != MTLCommandBufferStatus::Completed {
                    return Err(PlayerError::Renderer(format!(
                        "Flutter texture frame failed with status {:?}",
                        command_buffer.status()
                    )));
                }
            }
            self.last_submitted_command_buffer = Some(command_buffer.clone());
            self.pending_gpu_timing = Some(command_buffer);
        }

        self.stats.rendered_frames += 1;
        if self.output_mode.is_edr() {
            self.stats.edr_rendered_frames = self.stats.edr_rendered_frames.saturating_add(1);
        }
        if danmaku_item_count > 0 {
            self.stats.danmaku_passes += 1;
            self.stats.danmaku_items += danmaku_item_count as u64;
        }
        if trace::enabled() {
            trace::log(format!(
                "[erika-render-trace] stage=video_frame elapsed_ms={:.3} gen={} size={}x{} upscaled={} overlay={} danmaku_items={} gpu_ms={:.3} upscaler_ms={:.3}",
                started.elapsed().as_secs_f64() * 1000.0,
                frame
                    .frame_token
                    .map(|token| token.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                frame.frame.info.width,
                frame.frame.info.height,
                upscale_requested && upscaled_luma_used,
                has_overlay,
                danmaku_item_count,
                self.stats.last_gpu_duration.as_secs_f64() * 1000.0,
                self.stats.last_upscaler_encode_duration.as_secs_f64() * 1000.0,
            ));
        }

        Ok(())
    }

    pub fn capture_video_frame_rgba(
        &mut self,
        mut frame: VideoRenderFrame<'_>,
        overlay: Option<OverlayRenderFrame<'_>>,
        danmaku: Option<DanmakuRenderFrame<'_>>,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        if width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "capture size must be non-zero".to_string(),
            ));
        }

        let Some(textures) = frame.frame.inner.as_ref() else {
            return Err(PlayerError::Renderer(
                "imported video frame has no Metal textures".to_string(),
            ));
        };
        let Some(luma) = textures.luma_texture() else {
            return Err(PlayerError::Renderer(
                "imported video frame has no luma plane".to_string(),
            ));
        };
        let Some(chroma) = textures.chroma_texture() else {
            return Err(PlayerError::Renderer(
                "imported video frame has no chroma plane".to_string(),
            ));
        };

        let source_color = frame.pipeline.source;
        // Captures are 8-bit SDR RGBA. Render through the SDR target so an
        // HDR source tone maps into [0,1] instead of writing extended-linear
        // values that the readback would clamp, and keep the packed-alpha
        // input mode so transparent-video captures rebuild their alpha.
        let capture_output_mode = MetalOutputMode::Sdr;
        frame.pipeline = frame
            .pipeline
            .with_target(metal_target_color(capture_output_mode, source_color));

        let layout = VideoPresentationLayout::aspect_fit(
            frame.frame.info.width as u32,
            frame.frame.info.height as u32,
            width,
            height,
        );

        unsafe {
            let capture_pixel_format = metal_pixel_format(MetalDrawablePixelFormat::Bgra8Unorm);
            let descriptor =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    capture_pixel_format,
                    width as usize,
                    height as usize,
                    false,
                );
            descriptor.setStorageMode(MTLStorageMode::Private);
            descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            let target = self
                .device
                .newTextureWithDescriptor(&descriptor)
                .ok_or_else(|| {
                    PlayerError::Renderer("capture texture allocation failed".to_string())
                })?;

            let Some(command_buffer) = self.queue.commandBuffer() else {
                return Err(PlayerError::Renderer(
                    "commandBuffer returned nil".to_string(),
                ));
            };

            let pipeline = self.video_pipeline_state()?;
            let sampler = self.video_sampler_state()?;
            let pass_descriptor = MTLRenderPassDescriptor::new();
            let attachments = pass_descriptor.colorAttachments();
            let attachment = attachments.objectAtIndexedSubscript(0);
            attachment.setTexture(Some(&*target));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0,
            });

            let Some(encoder) = command_buffer.renderCommandEncoderWithDescriptor(&pass_descriptor)
            else {
                return Err(PlayerError::Renderer(
                    "renderCommandEncoderWithDescriptor returned nil".to_string(),
                ));
            };

            // Resolve the LUT before building the uniforms (same gating as
            // the live path above).
            let cached_gamut_lut = self.gamut_lut_texture(&frame)?;
            let gamut_lut_ready = cached_gamut_lut.is_some();
            let gamut_lut = match cached_gamut_lut {
                Some(lut) => lut,
                None => self.dummy_gamut_lut_texture()?,
            };
            let uniforms = VideoUniforms {
                is_p010: matches!(frame.frame.info.format, ImportedVideoFormat::P010) as u32,
                full_range: matches!(frame.pipeline.source.range, ColorRange::Full) as u32,
                source_transfer: transfer_code(frame.pipeline.source.transfer),
                target_transfer: transfer_code(frame.pipeline.target.transfer),
                tone_map: frame.pipeline.tone_map_uniform_code(),
                edr_output: capture_output_mode.is_edr() as u32,
                _reserved0: self.video_alpha_mode as u32,
                _reserved1: 0,
                rect: layout.target_rect,
                viewport: layout.video_viewport(),
                nits: [
                    frame.pipeline.source.nominal_peak_nits,
                    frame.pipeline.target.peak_nits,
                    frame.pipeline.source.reference_white_nits,
                    frame.pipeline.target.reference_white_nits,
                ],
                luma_coefficients: luma_coefficients(frame.pipeline.luma_coefficients()),
                gamut_matrix_rows: frame.pipeline.gamut_matrix().row4s(),
                ipt_matrix_rows: frame.pipeline.ipt_matrix_rows(),
                tone_map_extra: frame.pipeline.tone_map_extra(),
                tone_map_coeffs: frame.pipeline.tone_map_coeffs(),
                gamut_lut_enabled: if gamut_lut_ready {
                    frame.pipeline.gamut_lut_active() as u32
                } else {
                    0
                },
                gamut_primaries: frame.pipeline.gamut_primaries_code(),
                gamut_reserved0: 0,
                gamut_reserved1: 0,
                dovi: DoviUniforms::of_for_representation(
                    &frame.pipeline.source,
                    matches!(frame.frame.info.format, ImportedVideoFormat::P010),
                ),
            };
            encoder.setRenderPipelineState(&pipeline);
            encoder.setFragmentTexture_atIndex(Some(luma), 0);
            encoder.setFragmentTexture_atIndex(Some(chroma), 1);
            encoder.setFragmentTexture_atIndex(Some(&gamut_lut), 2);
            encoder.setFragmentSamplerState_atIndex(Some(&sampler), 0);
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(
                    (&uniforms as *const VideoUniforms)
                        .cast::<c_void>()
                        .cast_mut(),
                )
                .expect("uniform pointer is non-null"),
                mem::size_of::<VideoUniforms>(),
                0,
            );
            encoder.setFragmentBytes_length_atIndex(
                NonNull::new(
                    (&uniforms as *const VideoUniforms)
                        .cast::<c_void>()
                        .cast_mut(),
                )
                .expect("uniform pointer is non-null"),
                mem::size_of::<VideoUniforms>(),
                0,
            );
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);

            let target_color = frame.pipeline.target;
            if let Some(overlay) = overlay {
                self.draw_overlay_planes(&encoder, overlay, layout, target_color)?;
            }
            if let Some(danmaku) = danmaku.as_ref() {
                self.draw_danmaku_plan(
                    &encoder,
                    &command_buffer,
                    danmaku.plan,
                    layout,
                    target_color,
                )?;
            }
            encoder.endEncoding();

            let bytes_per_pixel = 4usize; // capture target is BGRA8Unorm
            let row_bytes = width as usize * bytes_per_pixel;
            let buffer_len = row_bytes * height as usize;
            let readback = self
                .device
                .newBufferWithLength_options(buffer_len, MTLResourceOptions::StorageModeShared)
                .ok_or_else(|| {
                    PlayerError::Renderer("capture readback buffer allocation failed".to_string())
                })?;
            let Some(blit) = command_buffer.blitCommandEncoder() else {
                return Err(PlayerError::Renderer(
                    "blitCommandEncoder returned nil".to_string(),
                ));
            };
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                &target,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
                MTLSize {
                    width: width as usize,
                    height: height as usize,
                    depth: 1,
                },
                &readback,
                0,
                row_bytes,
                buffer_len,
            );
            blit.endEncoding();
            command_buffer.commit();
            command_buffer.waitUntilCompleted();
            if command_buffer.status() != MTLCommandBufferStatus::Completed {
                return Err(PlayerError::Renderer(format!(
                    "capture command buffer failed with status {:?}",
                    command_buffer.status()
                )));
            }

            let raw =
                std::slice::from_raw_parts(readback.contents().as_ptr().cast::<u8>(), buffer_len);
            Ok(convert_capture_to_rgba8(
                raw,
                width as usize,
                height as usize,
                MetalDrawablePixelFormat::Bgra8Unorm,
            ))
        }
    }

    pub fn set_luma_upscaler(&mut self, mode: LumaUpscalerMode) {
        self.upscaler.set_mode(mode);
    }

    /// Records the GPU time of the previous frame's command buffer once it
    /// has completed. The value therefore lags by one frame, which is fine
    /// for performance metrics.
    fn collect_gpu_timing(&mut self) {
        let Some(pending) = self.pending_gpu_timing.take() else {
            return;
        };
        if pending.status() == MTLCommandBufferStatus::Completed {
            let seconds = (pending.GPUEndTime() - pending.GPUStartTime()).max(0.0);
            self.stats.last_gpu_duration = Duration::from_secs_f64(seconds);
        }
    }

    fn draw_overlay_planes(
        &mut self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        overlay: OverlayRenderFrame<'_>,
        layout: VideoPresentationLayout,
        target: TargetColorState,
    ) -> Result<()> {
        let _ = crate::renderer::metal::inspect_overlay_frame(overlay.frame)?;
        if overlay.frame.subtitle_planes.is_empty()
            && overlay.frame.subtitle_alpha_planes.is_empty()
        {
            return Ok(());
        }

        let pipeline = self.overlay_pipeline_state()?;
        let sampler = self.video_sampler_state()?;
        let viewport_width = overlay.frame.viewport.width;
        let viewport_height = overlay.frame.viewport.height;
        for plane in &overlay.frame.subtitle_planes {
            let texture = self.create_overlay_texture(
                plane.width as usize,
                plane.height as usize,
                &plane.rgba,
            )?;
            let (x, y, width, height) = plane.scaled_rect(viewport_width, viewport_height);
            let uniforms = OverlayUniforms::from_plane(
                x,
                y,
                width,
                height,
                layout,
                target,
                self.output_mode.is_edr(),
            );
            unsafe {
                encoder.setRenderPipelineState(&pipeline);
                encoder.setFragmentTexture_atIndex(Some(&*texture), 0);
                encoder.setFragmentSamplerState_atIndex(Some(&sampler), 0);
                encoder.setVertexBytes_length_atIndex(
                    overlay_uniform_pointer(&uniforms),
                    mem::size_of::<OverlayUniforms>(),
                    0,
                );
                encoder.setFragmentBytes_length_atIndex(
                    overlay_uniform_pointer(&uniforms),
                    mem::size_of::<OverlayUniforms>(),
                    0,
                );
                encoder.drawPrimitives_vertexStart_vertexCount(
                    MTLPrimitiveType::TriangleStrip,
                    0,
                    4,
                );
            }
        }
        if !overlay.frame.subtitle_alpha_planes.is_empty() {
            let atlas = self.prepare_overlay_alpha_atlas(
                &overlay.frame.subtitle_alpha_planes,
                overlay.frame.subtitle_changed,
            )?;
            let texture = atlas.texture.clone();
            let placements = atlas.placements.clone();
            let atlas_width = atlas.width;
            let atlas_height = atlas.height;
            for placement in &placements {
                let bitmap = &overlay.frame.subtitle_alpha_planes[placement.bitmap_index];
                let uniforms = OverlayUniforms::from_alpha_atlas_bitmap(
                    bitmap,
                    placement,
                    atlas_width,
                    atlas_height,
                    layout,
                    target,
                    self.output_mode.is_edr(),
                );
                unsafe {
                    encoder.setRenderPipelineState(&pipeline);
                    encoder.setFragmentTexture_atIndex(Some(&*texture), 0);
                    encoder.setFragmentSamplerState_atIndex(Some(&sampler), 0);
                    encoder.setVertexBytes_length_atIndex(
                        overlay_uniform_pointer(&uniforms),
                        mem::size_of::<OverlayUniforms>(),
                        0,
                    );
                    encoder.setFragmentBytes_length_atIndex(
                        overlay_uniform_pointer(&uniforms),
                        mem::size_of::<OverlayUniforms>(),
                        0,
                    );
                    encoder.drawPrimitives_vertexStart_vertexCount(
                        MTLPrimitiveType::TriangleStrip,
                        0,
                        4,
                    );
                }
            }
        }
        Ok(())
    }

    fn draw_danmaku_plan(
        &mut self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        command_buffer: &Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        plan: &DanmakuRenderPlan,
        layout: VideoPresentationLayout,
        target: TargetColorState,
    ) -> Result<()> {
        if plan.is_empty() {
            return Ok(());
        }
        let Some(atlas) = plan.atlas.as_ref() else {
            return Ok(());
        };
        if !atlas.is_valid() {
            return Err(PlayerError::Renderer(format!(
                "danmaku glyph atlas has fill={} outline={} bytes, expected at least {} for {}x{} stride {}",
                atlas.fill_alpha.len(),
                atlas.outline_alpha.len(),
                atlas.required_len(),
                atlas.width,
                atlas.height,
                atlas.stride
            )));
        }
        let pipeline = self.danmaku_batch_pipeline_state()?;
        let sampler = self.video_sampler_state()?;
        let atlas_started = Instant::now();
        let (fill_texture, outline_texture) = self.prepare_danmaku_alpha_atlas(atlas)?;
        self.stats.last_danmaku_atlas_duration = atlas_started.elapsed();
        self.draw_danmaku_batch(
            encoder,
            command_buffer,
            &pipeline,
            &sampler,
            plan,
            &fill_texture,
            &outline_texture,
            layout,
            target,
        )?;
        Ok(())
    }

    fn prepare_danmaku_alpha_atlas(
        &mut self,
        atlas: &DanmakuGlyphAtlas,
    ) -> Result<(
        Retained<ProtocolObject<dyn MTLTexture>>,
        Retained<ProtocolObject<dyn MTLTexture>>,
    )> {
        if let Some(cache) = &self.danmaku_alpha_atlas_cache {
            if cache.can_reuse_for(atlas) {
                return Ok((cache.fill_texture.clone(), cache.outline_texture.clone()));
            }
        }
        if let Some(cache) = &mut self.danmaku_alpha_atlas_cache {
            if let Some(update) = atlas.incremental_update_from(
                cache.version,
                cache.width,
                cache.height,
                cache.stride,
            ) {
                update_danmaku_alpha_texture(&cache.fill_texture, atlas, &atlas.fill_alpha, update);
                update_danmaku_alpha_texture(
                    &cache.outline_texture,
                    atlas,
                    &atlas.outline_alpha,
                    update,
                );
                cache.version = atlas.version;
                if trace::enabled() {
                    trace::log(format!(
                        "[erika-danmaku-gpu] event=atlas_incremental version={}->{} size={}x{} region={},{}+{}x{}",
                        update.from_version,
                        atlas.version,
                        atlas.width,
                        atlas.height,
                        update.x,
                        update.y,
                        update.width,
                        update.height,
                    ));
                }
                return Ok((cache.fill_texture.clone(), cache.outline_texture.clone()));
            }
        }
        let previous_atlas = self
            .danmaku_alpha_atlas_cache
            .as_ref()
            .map(|cache| (cache.version, cache.width, cache.height));
        let fill_texture = self.create_overlay_alpha_texture(
            atlas.width as usize,
            atlas.height as usize,
            atlas.stride,
            &atlas.fill_alpha,
        )?;
        let outline_texture = self.create_overlay_alpha_texture(
            atlas.width as usize,
            atlas.height as usize,
            atlas.stride,
            &atlas.outline_alpha,
        )?;
        self.danmaku_alpha_atlas_cache = Some(DanmakuAlphaAtlasCache {
            version: atlas.version,
            width: atlas.width,
            height: atlas.height,
            stride: atlas.stride,
            fill_texture: fill_texture.clone(),
            outline_texture: outline_texture.clone(),
        });
        if trace::enabled() {
            trace::log(format!(
                "[erika-danmaku-gpu] event=atlas_recreate previous={} version={} size={}x{} stride={}",
                previous_atlas
                    .map(|(version, width, height)| format!("{version}/{width}x{height}"))
                    .unwrap_or_else(|| "-".to_string()),
                atlas.version,
                atlas.width,
                atlas.height,
                atlas.stride,
            ));
        }
        Ok((fill_texture, outline_texture))
    }

    fn draw_danmaku_batch(
        &mut self,
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        command_buffer: &Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
        sampler: &ProtocolObject<dyn MTLSamplerState>,
        plan: &DanmakuRenderPlan,
        fill_texture: &ProtocolObject<dyn MTLTexture>,
        outline_texture: &ProtocolObject<dyn MTLTexture>,
        layout: VideoPresentationLayout,
        target: TargetColorState,
    ) -> Result<()> {
        let build_started = Instant::now();
        let uniforms = DanmakuBatchUniforms {
            viewport: layout.overlay_viewport(),
            target_transfer: transfer_code(target.transfer),
            _reserved0: 0,
            ui_nits: ui_output_nits(target, self.output_mode.is_edr()),
        };
        unsafe {
            encoder.setRenderPipelineState(pipeline);
            encoder.setFragmentSamplerState_atIndex(Some(sampler), 0);
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(
                    (&uniforms as *const DanmakuBatchUniforms)
                        .cast::<c_void>()
                        .cast_mut(),
                )
                .expect("danmaku batch uniforms pointer is non-null"),
                mem::size_of::<DanmakuBatchUniforms>(),
                1,
            );
            encoder.setFragmentBytes_length_atIndex(
                NonNull::new(
                    (&uniforms as *const DanmakuBatchUniforms)
                        .cast::<c_void>()
                        .cast_mut(),
                )
                .expect("danmaku batch uniforms pointer is non-null"),
                mem::size_of::<DanmakuBatchUniforms>(),
                1,
            );
        }
        let shadow_count = plan
            .items
            .iter()
            .filter(|item| item.shadow_rgba[3] > 0.0)
            .count();
        let outline_count = plan
            .items
            .iter()
            .filter(|item| item.outline_rgba[3] > 0.0)
            .count();
        let effect_count = shadow_count
            .checked_add(outline_count)
            .ok_or_else(|| PlayerError::Renderer("danmaku instance count overflow".to_string()))?;
        let fill_count = plan.items.len();
        let total_instances = effect_count
            .checked_add(fill_count)
            .ok_or_else(|| PlayerError::Renderer("danmaku instance count overflow".to_string()))?;
        let total_bytes = instance_bytes_len(total_instances)?;
        self.stats.last_danmaku_vertex_bytes = total_bytes;
        self.stats.last_danmaku_vertex_count = total_instances;
        let buffer = self.acquire_danmaku_vertex_buffer(total_bytes, command_buffer)?;
        write_danmaku_instances_direct(&buffer, plan, total_instances)?;
        self.stats.last_danmaku_vertex_build_duration = build_started.elapsed();
        self.stats.last_danmaku_vertex_copy_duration = Duration::ZERO;

        let encode_started = Instant::now();
        draw_danmaku_instance_batch(
            encoder,
            fill_texture,
            outline_texture,
            &buffer,
            total_instances,
        )?;
        self.stats.last_danmaku_encode_duration = encode_started.elapsed();
        Ok(())
    }

    fn acquire_danmaku_vertex_buffer(
        &mut self,
        required_len: usize,
        command_buffer: &Retained<ProtocolObject<dyn MTLCommandBuffer>>,
    ) -> Result<Retained<ProtocolObject<dyn MTLBuffer>>> {
        debug_assert!(required_len > 0);

        let slot_count = self.danmaku_vertex_buffers.len();
        let busy_slots = self
            .danmaku_vertex_buffers
            .iter()
            .filter(|slot| !slot.is_reusable())
            .count();
        let reusable_index = (0..slot_count)
            .map(|offset| (self.danmaku_vertex_buffer_cursor + offset) % slot_count)
            .find(|&index| self.danmaku_vertex_buffers[index].is_reusable());
        let grew_pool = reusable_index.is_none();
        let index = match reusable_index {
            Some(index) => index,
            None => {
                self.danmaku_vertex_buffers
                    .push(DanmakuVertexBufferSlot::default());
                self.danmaku_vertex_buffers.len() - 1
            }
        };
        let required_capacity = required_len.next_power_of_two().max(4096);
        let slot = &mut self.danmaku_vertex_buffers[index];
        let previous_status = slot
            .in_flight
            .as_ref()
            .map(|buffer| format!("{:?}", buffer.status()))
            .unwrap_or_else(|| "-".to_string());
        let resized_buffer = slot.capacity < required_len || slot.buffer.is_none();
        if slot.capacity < required_len || slot.buffer.is_none() {
            slot.buffer = Some(
                self.device
                    .newBufferWithLength_options(
                        required_capacity,
                        MTLResourceOptions::StorageModeShared,
                    )
                    .ok_or_else(|| {
                        PlayerError::Renderer("newBufferWithLength returned nil".to_string())
                    })?,
            );
            slot.capacity = required_capacity;
        }
        slot.in_flight = Some(command_buffer.clone());
        let buffer = slot
            .buffer
            .as_ref()
            .expect("danmaku vertex buffer slot is initialized")
            .clone();
        let selected_capacity = slot.capacity;
        self.danmaku_vertex_buffer_cursor = (index + 1) % self.danmaku_vertex_buffers.len();
        self.danmaku_vertex_buffer_acquisitions =
            self.danmaku_vertex_buffer_acquisitions.saturating_add(1);
        if trace::enabled()
            && (grew_pool || resized_buffer || self.danmaku_vertex_buffer_acquisitions % 60 == 0)
        {
            trace::log(format!(
                "[erika-danmaku-gpu] event=vertex_buffer_acquire sequence={} slot={} slots={} busy_before={} previous_status={} required={} capacity={} flags=pool_grew:{} buffer_resized:{}",
                self.danmaku_vertex_buffer_acquisitions,
                index,
                self.danmaku_vertex_buffers.len(),
                busy_slots,
                previous_status,
                required_len,
                selected_capacity,
                grew_pool,
                resized_buffer,
            ));
        }
        Ok(buffer)
    }

    fn prepare_overlay_alpha_atlas(
        &mut self,
        bitmaps: &[SubtitleAlphaBitmap],
        changed: bool,
    ) -> Result<&OverlayAlphaAtlasCache> {
        if !changed {
            if let Some(cache) = &self.overlay_alpha_atlas_cache {
                if cache.can_reuse_for(bitmaps) {
                    self.stats.overlay_alpha_atlas_reuses += 1;
                    return Ok(self
                        .overlay_alpha_atlas_cache
                        .as_ref()
                        .expect("overlay atlas cache exists"));
                }
            }
        }

        let Some(plan) = OverlayAlphaAtlasPlan::pack(bitmaps)? else {
            self.overlay_alpha_atlas_cache = None;
            return Err(PlayerError::Renderer(
                "cannot prepare empty overlay alpha atlas".to_string(),
            ));
        };
        let texture = self.create_overlay_alpha_atlas_texture(&plan)?;
        self.overlay_alpha_atlas_cache = Some(OverlayAlphaAtlasCache::new(texture, plan, bitmaps));
        self.stats.overlay_alpha_atlas_uploads += 1;
        Ok(self
            .overlay_alpha_atlas_cache
            .as_ref()
            .expect("overlay atlas cache exists"))
    }

    fn create_overlay_texture(
        &self,
        width: usize,
        height: usize,
        rgba: &[u8],
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                width,
                height,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setResourceOptions(MTLResourceOptions::StorageModeShared);
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| {
                PlayerError::Renderer("newTextureWithDescriptor returned nil".to_string())
            })?;
        let region = MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: objc2_metal::MTLSize {
                width,
                height,
                depth: 1,
            },
        };
        unsafe {
            texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(rgba.as_ptr().cast::<c_void>().cast_mut())
                    .expect("overlay rgba pointer is non-null"),
                width * 4,
            );
        }
        Ok(texture)
    }

    fn create_overlay_alpha_atlas_texture(
        &self,
        atlas: &OverlayAlphaAtlasPlan,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        self.create_overlay_alpha_texture(atlas.width, atlas.height, atlas.stride, &atlas.pixels)
    }

    fn create_overlay_alpha_texture(
        &self,
        width: usize,
        height: usize,
        stride: usize,
        pixels: &[u8],
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::R8Unorm,
                width,
                height,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setResourceOptions(MTLResourceOptions::StorageModeShared);
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| {
                PlayerError::Renderer("newTextureWithDescriptor returned nil".to_string())
            })?;
        let region = MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: objc2_metal::MTLSize {
                width,
                height,
                depth: 1,
            },
        };
        unsafe {
            texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(pixels.as_ptr().cast::<c_void>().cast_mut())
                    .expect("overlay atlas pointer is non-null"),
                stride,
            );
        }
        Ok(texture)
    }

    pub unsafe fn import_video_frame_textures(
        &mut self,
        source: VideoFrameTextureSource,
    ) -> Result<ImportedVideoFrameResult> {
        if source.raw_pixel_buffer.is_null() {
            return Err(PlayerError::Renderer(
                "cannot import null CVPixelBuffer".to_string(),
            ));
        }

        let pixel_buffer = unsafe { &*(source.raw_pixel_buffer.cast::<CVPixelBuffer>()) };
        let retained_pixel_buffer = unsafe {
            CFRetained::retain(
                NonNull::new(source.raw_pixel_buffer.cast::<CVPixelBuffer>())
                    .expect("checked non-null CVPixelBuffer"),
            )
        };
        let pixel_format = CVPixelBufferGetPixelFormatType(pixel_buffer);
        let mapping =
            PixelBufferMapping::from_core_video_format(pixel_format).ok_or_else(|| {
                PlayerError::Renderer(format!(
                    "unsupported CVPixelBuffer format {}",
                    fourcc_string(pixel_format)
                ))
            })?;
        let plane_count = CVPixelBufferGetPlaneCount(pixel_buffer);
        if plane_count < 2 {
            return Err(PlayerError::Renderer(format!(
                "expected at least 2 planes for {}, got {plane_count}",
                fourcc_string(pixel_format)
            )));
        }

        let cache = self.texture_cache()?;
        let image_buffer = unsafe { &*(source.raw_pixel_buffer.cast::<CVImageBuffer>()) };
        let mut imported_textures = Vec::with_capacity(2);
        let mut planes = Vec::with_capacity(2);

        for plane in mapping.planes {
            let width = CVPixelBufferGetWidthOfPlane(pixel_buffer, plane.index);
            let height = CVPixelBufferGetHeightOfPlane(pixel_buffer, plane.index);
            let texture = create_plane_texture(
                cache,
                image_buffer,
                plane.pixel_format,
                width,
                height,
                plane.index,
            )?;
            let Some(metal_texture) = CVMetalTextureGetTexture(&texture) else {
                return Err(PlayerError::Renderer(format!(
                    "CVMetalTextureGetTexture returned nil for plane {}",
                    plane.index
                )));
            };
            planes.push(ImportedVideoPlaneInfo {
                index: plane.index,
                width: metal_texture.width(),
                height: metal_texture.height(),
                metal_pixel_format: plane.name,
            });
            imported_textures.push(ImportedVideoPlaneTexture {
                cv_texture: Some(texture),
                metal_texture,
            });
        }

        let info = ImportedVideoFrameInfo {
            width: CVPixelBufferGetWidth(pixel_buffer).max(source.width as usize),
            height: CVPixelBufferGetHeight(pixel_buffer).max(source.height as usize),
            pixel_format,
            pixel_format_fourcc: fourcc_string(pixel_format),
            format: mapping.format,
            full_range: mapping.full_range,
            color_range: if mapping.full_range {
                ColorRange::Full
            } else {
                ColorRange::Limited
            },
            planes,
        };

        Ok(ImportedVideoFrameResult {
            info,
            textures: ImportedVideoFrameTextures {
                source_pixel_buffer: Some(retained_pixel_buffer),
                planes: imported_textures,
            },
        })
    }

    pub fn upload_planar_video_frame(
        &self,
        frame: &PlanarFrame,
        full_range: bool,
    ) -> Result<ImportedVideoFrameResult> {
        let width = frame.width as usize;
        let height = frame.height as usize;
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let (
            format,
            luma_format,
            chroma_format,
            luma_stride,
            chroma_stride,
            luma_name,
            chroma_name,
        ) = match frame.format {
            PlanarPixelFormat::Nv12 => (
                ImportedVideoFormat::Nv12,
                MTLPixelFormat::R8Unorm,
                MTLPixelFormat::RG8Unorm,
                width,
                chroma_width * 2,
                "R8Unorm",
                "RG8Unorm",
            ),
            PlanarPixelFormat::P010 => (
                ImportedVideoFormat::P010,
                MTLPixelFormat::R16Unorm,
                MTLPixelFormat::RG16Unorm,
                width * 2,
                chroma_width * 4,
                "R16Unorm",
                "RG16Unorm",
            ),
        };
        let luma =
            self.create_video_plane_texture(width, height, luma_format, luma_stride, &frame.luma)?;
        let chroma = self.create_video_plane_texture(
            chroma_width,
            chroma_height,
            chroma_format,
            chroma_stride,
            &frame.chroma,
        )?;
        Ok(ImportedVideoFrameResult {
            info: ImportedVideoFrameInfo {
                width,
                height,
                pixel_format: 0,
                pixel_format_fourcc: match format {
                    ImportedVideoFormat::Nv12 => "NV12",
                    ImportedVideoFormat::P010 => "P010",
                }
                .to_string(),
                format,
                full_range,
                color_range: if full_range {
                    ColorRange::Full
                } else {
                    ColorRange::Limited
                },
                planes: vec![
                    ImportedVideoPlaneInfo {
                        index: 0,
                        width,
                        height,
                        metal_pixel_format: luma_name,
                    },
                    ImportedVideoPlaneInfo {
                        index: 1,
                        width: chroma_width,
                        height: chroma_height,
                        metal_pixel_format: chroma_name,
                    },
                ],
            },
            textures: ImportedVideoFrameTextures {
                source_pixel_buffer: None,
                planes: vec![
                    ImportedVideoPlaneTexture {
                        cv_texture: None,
                        metal_texture: luma,
                    },
                    ImportedVideoPlaneTexture {
                        cv_texture: None,
                        metal_texture: chroma,
                    },
                ],
            },
        })
    }

    fn create_video_plane_texture(
        &self,
        width: usize,
        height: usize,
        format: MTLPixelFormat,
        stride: usize,
        pixels: &[u8],
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        let expected = stride.checked_mul(height).ok_or_else(|| {
            PlayerError::Renderer("software video plane dimensions overflow".to_string())
        })?;
        if pixels.len() != expected {
            return Err(PlayerError::Renderer(format!(
                "software video plane has {} bytes, expected {expected}",
                pixels.len()
            )));
        }
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                format, width, height, false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setResourceOptions(MTLResourceOptions::StorageModeShared);
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| {
                PlayerError::Renderer("newTextureWithDescriptor returned nil".to_string())
            })?;
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width,
                height,
                depth: 1,
            },
        };
        unsafe {
            texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(pixels.as_ptr().cast::<c_void>().cast_mut())
                    .expect("software video plane pointer is non-null"),
                stride,
            );
        }
        Ok(texture)
    }

    fn texture_cache(&mut self) -> Result<&CVMetalTextureCache> {
        if self.texture_cache.is_none() {
            let mut raw_cache: *mut CVMetalTextureCache = std::ptr::null_mut();
            let status = unsafe {
                CVMetalTextureCache::create(
                    None,
                    None,
                    &self.device,
                    None,
                    NonNull::new(&mut raw_cache as *mut *mut CVMetalTextureCache)
                        .expect("stack cache pointer is non-null"),
                )
            };
            if status != kCVReturnSuccess {
                return Err(PlayerError::Renderer(format!(
                    "CVMetalTextureCacheCreate failed: {status}"
                )));
            }
            let raw_cache = NonNull::new(raw_cache).ok_or_else(|| {
                PlayerError::Renderer("CVMetalTextureCacheCreate returned null cache".to_string())
            })?;
            self.texture_cache = Some(unsafe { CFRetained::from_raw(raw_cache) });
        }
        Ok(self.texture_cache.as_deref().expect("texture cache exists"))
    }

    fn video_pipeline_state(
        &mut self,
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
        if self.video_pipeline.is_none() {
            let library = self
                .device
                .newLibraryWithSource_options_error(&NSString::from_str(VIDEO_SHADER_SOURCE), None)
                .map_err(|error| {
                    PlayerError::Renderer(format!(
                        "Metal video shader compile failed: {}",
                        error.localizedDescription()
                    ))
                })?;
            let vertex = library
                .newFunctionWithName(&NSString::from_str("erika_video_vertex"))
                .ok_or_else(|| {
                    PlayerError::Renderer("Metal shader missing erika_video_vertex".to_string())
                })?;
            let fragment = library
                .newFunctionWithName(&NSString::from_str("erika_video_fragment"))
                .ok_or_else(|| {
                    PlayerError::Renderer("Metal shader missing erika_video_fragment".to_string())
                })?;
            let descriptor = MTLRenderPipelineDescriptor::new();
            descriptor.setLabel(Some(&NSString::from_str("Erika Video Pipeline")));
            descriptor.setVertexFunction(Some(&*vertex));
            descriptor.setFragmentFunction(Some(&*fragment));
            let attachments = descriptor.colorAttachments();
            let attachment = unsafe { attachments.objectAtIndexedSubscript(0) };
            attachment.setPixelFormat(metal_pixel_format(self.drawable_pixel_format));
            let pipeline = self
                .device
                .newRenderPipelineStateWithDescriptor_error(&descriptor)
                .map_err(|error| {
                    PlayerError::Renderer(format!(
                        "Metal video pipeline create failed: {}",
                        error.localizedDescription()
                    ))
                })?;
            self.video_pipeline = Some(pipeline);
        }
        Ok(self
            .video_pipeline
            .as_ref()
            .expect("video pipeline exists")
            .clone())
    }

    fn video_sampler_state(&mut self) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>> {
        if self.video_sampler.is_none() {
            let descriptor = MTLSamplerDescriptor::new();
            descriptor.setMinFilter(MTLSamplerMinMagFilter::Linear);
            descriptor.setMagFilter(MTLSamplerMinMagFilter::Linear);
            let sampler = self
                .device
                .newSamplerStateWithDescriptor(&descriptor)
                .ok_or_else(|| {
                    PlayerError::Renderer("newSamplerStateWithDescriptor returned nil".to_string())
                })?;
            self.video_sampler = Some(sampler);
        }
        Ok(self
            .video_sampler
            .as_ref()
            .expect("video sampler exists")
            .clone())
    }

    fn overlay_pipeline_state(
        &mut self,
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
        if self.overlay_pipeline.is_none() {
            let library = self
                .device
                .newLibraryWithSource_options_error(&NSString::from_str(VIDEO_SHADER_SOURCE), None)
                .map_err(|error| {
                    PlayerError::Renderer(format!(
                        "Metal overlay shader compile failed: {}",
                        error.localizedDescription()
                    ))
                })?;
            let vertex = library
                .newFunctionWithName(&NSString::from_str("erika_overlay_vertex"))
                .ok_or_else(|| {
                    PlayerError::Renderer("Metal shader missing erika_overlay_vertex".to_string())
                })?;
            let fragment = library
                .newFunctionWithName(&NSString::from_str("erika_overlay_fragment"))
                .ok_or_else(|| {
                    PlayerError::Renderer("Metal shader missing erika_overlay_fragment".to_string())
                })?;
            let descriptor = MTLRenderPipelineDescriptor::new();
            descriptor.setLabel(Some(&NSString::from_str("Erika Overlay Pipeline")));
            descriptor.setVertexFunction(Some(&*vertex));
            descriptor.setFragmentFunction(Some(&*fragment));
            let attachments = descriptor.colorAttachments();
            let attachment = unsafe { attachments.objectAtIndexedSubscript(0) };
            attachment.setPixelFormat(metal_pixel_format(self.drawable_pixel_format));
            attachment.setBlendingEnabled(true);
            attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
            attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            attachment.setRgbBlendOperation(MTLBlendOperation::Add);
            attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
            attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
            let pipeline = self
                .device
                .newRenderPipelineStateWithDescriptor_error(&descriptor)
                .map_err(|error| {
                    PlayerError::Renderer(format!(
                        "Metal overlay pipeline create failed: {}",
                        error.localizedDescription()
                    ))
                })?;
            self.overlay_pipeline = Some(pipeline);
        }
        Ok(self
            .overlay_pipeline
            .as_ref()
            .expect("overlay pipeline exists")
            .clone())
    }

    fn danmaku_batch_pipeline_state(
        &mut self,
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
        if self.danmaku_batch_pipeline.is_none() {
            let library = self
                .device
                .newLibraryWithSource_options_error(&NSString::from_str(VIDEO_SHADER_SOURCE), None)
                .map_err(|error| {
                    PlayerError::Renderer(format!(
                        "Metal danmaku batch shader compile failed: {}",
                        error.localizedDescription()
                    ))
                })?;
            let vertex = library
                .newFunctionWithName(&NSString::from_str("erika_danmaku_batch_vertex"))
                .ok_or_else(|| {
                    PlayerError::Renderer(
                        "Metal shader missing erika_danmaku_batch_vertex".to_string(),
                    )
                })?;
            let fragment = library
                .newFunctionWithName(&NSString::from_str("erika_danmaku_batch_fragment"))
                .ok_or_else(|| {
                    PlayerError::Renderer(
                        "Metal shader missing erika_danmaku_batch_fragment".to_string(),
                    )
                })?;
            let descriptor = MTLRenderPipelineDescriptor::new();
            descriptor.setLabel(Some(&NSString::from_str("Erika Danmaku Batch Pipeline")));
            descriptor.setVertexFunction(Some(&*vertex));
            descriptor.setFragmentFunction(Some(&*fragment));
            let attachments = descriptor.colorAttachments();
            let attachment = unsafe { attachments.objectAtIndexedSubscript(0) };
            attachment.setPixelFormat(metal_pixel_format(self.drawable_pixel_format));
            attachment.setBlendingEnabled(true);
            attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
            attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            attachment.setRgbBlendOperation(MTLBlendOperation::Add);
            attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
            attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
            let pipeline = self
                .device
                .newRenderPipelineStateWithDescriptor_error(&descriptor)
                .map_err(|error| {
                    PlayerError::Renderer(format!(
                        "Metal danmaku batch pipeline create failed: {}",
                        error.localizedDescription()
                    ))
                })?;
            self.danmaku_batch_pipeline = Some(pipeline);
        }
        Ok(self
            .danmaku_batch_pipeline
            .as_ref()
            .expect("danmaku batch pipeline exists")
            .clone())
    }
}

fn configure_layer_dynamic_range(layer: &CAMetalLayer, enabled: bool) {
    // On macOS, CAMetalLayer scales `drawableSize` through its Retina backing
    // scale. Setting CALayer.contentsFormat there makes Core Animation treat
    // the drawable like 1x layer contents and clips the right/bottom at 2x.
    // UIKit/tvOS layers need contentsFormat to stay aligned with pixelFormat.
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    {
        let contents_format = unsafe {
            if enabled {
                kCAContentsFormatRGBA16Float
            } else {
                kCAContentsFormatRGBA8Uint
            }
        };
        layer.setContentsFormat(contents_format);
    }
    if layer.respondsToSelector(objc2::sel!(setWantsExtendedDynamicRangeContent:)) {
        layer.setWantsExtendedDynamicRangeContent(enabled);
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VideoUniforms {
    is_p010: u32,
    full_range: u32,
    source_transfer: u32,
    target_transfer: u32,
    tone_map: u32,
    edr_output: u32,
    _reserved0: u32,
    _reserved1: u32,
    rect: [f32; 4],
    viewport: [f32; 4],
    nits: [f32; 4],
    luma_coefficients: [f32; 4],
    gamut_matrix_rows: [[f32; 4]; 3],
    ipt_matrix_rows: [[f32; 4]; 9],
    tone_map_extra: [f32; 4],
    tone_map_coeffs: [f32; 4],
    gamut_lut_enabled: u32,
    gamut_primaries: u32,
    gamut_reserved0: u32,
    gamut_reserved1: u32,
    dovi: DoviUniforms,
}

fn metal_pixel_format(format: MetalDrawablePixelFormat) -> MTLPixelFormat {
    match format {
        MetalDrawablePixelFormat::Bgra8Unorm => MTLPixelFormat::BGRA8Unorm,
        MetalDrawablePixelFormat::Rgba16Float => MTLPixelFormat::RGBA16Float,
    }
}

fn convert_capture_to_rgba8(
    raw: &[u8],
    width: usize,
    height: usize,
    format: MetalDrawablePixelFormat,
) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(width * height * 4);
    match format {
        MetalDrawablePixelFormat::Bgra8Unorm => {
            for pixel in raw.chunks_exact(4).take(width * height) {
                rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        }
        MetalDrawablePixelFormat::Rgba16Float => {
            for pixel in raw.chunks_exact(8).take(width * height) {
                let r = half_to_unorm8(u16::from_le_bytes([pixel[0], pixel[1]]));
                let g = half_to_unorm8(u16::from_le_bytes([pixel[2], pixel[3]]));
                let b = half_to_unorm8(u16::from_le_bytes([pixel[4], pixel[5]]));
                let a = half_to_unorm8(u16::from_le_bytes([pixel[6], pixel[7]]));
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }
    }
    rgba
}

fn half_to_unorm8(bits: u16) -> u8 {
    let value = half_to_f32(bits).clamp(0.0, 1.0);
    (value * 255.0).round() as u8
}

fn half_to_f32(bits: u16) -> f32 {
    let sign = ((bits >> 15) & 0x1) as u32;
    let exp = ((bits >> 10) & 0x1f) as i32;
    let frac = (bits & 0x03ff) as u32;
    let f_bits = if exp == 0 {
        if frac == 0 {
            sign << 31
        } else {
            let mut frac = frac;
            let mut exp = -14;
            while (frac & 0x0400) == 0 {
                frac <<= 1;
                exp -= 1;
            }
            frac &= 0x03ff;
            (sign << 31) | (((exp + 127) as u32) << 23) | (frac << 13)
        }
    } else if exp == 0x1f {
        (sign << 31) | 0x7f80_0000 | (frac << 13)
    } else {
        (sign << 31) | (((exp - 15 + 127) as u32) << 23) | (frac << 13)
    };
    f32::from_bits(f_bits)
}

fn edr_layer_color_space(
    source: Option<crate::renderer::pipeline::SourceColorState>,
) -> (
    Option<&'static objc2_core_foundation::CFString>,
    &'static str,
) {
    match source.map(|source| (source.primaries, source.transfer)) {
        Some((ColorPrimaries::DisplayP3, TransferFunction::Pq)) => {
            (Some(unsafe { kCGColorSpaceDisplayP3_PQ }), "display-p3-pq")
        }
        Some((ColorPrimaries::Bt2020, TransferFunction::Pq))
        | Some((ColorPrimaries::Unknown, TransferFunction::Pq)) => {
            (Some(unsafe { kCGColorSpaceITUR_2100_PQ }), "itur-2100-pq")
        }
        _ => (
            Some(unsafe { kCGColorSpaceExtendedLinearSRGB }),
            "extended-linear-srgb",
        ),
    }
}

fn transfer_code(transfer: crate::core::TransferFunction) -> u32 {
    match transfer {
        crate::core::TransferFunction::Srgb => 1,
        crate::core::TransferFunction::Bt1886 => 2,
        crate::core::TransferFunction::Pq => 3,
        crate::core::TransferFunction::Hlg => 4,
        crate::core::TransferFunction::Unknown => 1,
    }
}

fn ui_reference_white_nits(target: TargetColorState) -> f32 {
    if matches!(target.transfer, TransferFunction::Pq) {
        target.reference_white_nits.max(1.0)
    } else {
        100.0
    }
}

/// `ui_nits.y`: non-zero when the drawable holds linear light (Apple EDR /
/// extended-linear output), in which case the UI color must be linearized
/// before compositing — the video pass writes linear values into the same
/// buffer. PQ targets encode the UI inside the shader and SDR targets
/// composite in the output transfer, so both keep 0 here.
fn ui_linear_reference_white_nits(target: TargetColorState, edr_output: bool) -> f32 {
    if !edr_output || matches!(target.transfer, TransferFunction::Pq) {
        0.0
    } else {
        target.reference_white_nits.max(1.0)
    }
}

fn ui_output_nits(target: TargetColorState, edr_output: bool) -> [f32; 4] {
    [
        ui_reference_white_nits(target),
        ui_linear_reference_white_nits(target, edr_output),
        0.0,
        0.0,
    ]
}

fn luma_coefficients(coeffs: crate::renderer::pipeline::LumaCoefficients) -> [f32; 4] {
    [coeffs.kr, coeffs.kg, coeffs.kb, 0.0]
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct OverlayUniforms {
    rect: [f32; 4],
    tex_rect: [f32; 4],
    viewport: [f32; 2],
    overlay_mode: u32,
    target_transfer: u32,
    color: [f32; 4],
    ui_nits: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DanmakuBatchUniforms {
    viewport: [f32; 2],
    target_transfer: u32,
    _reserved0: u32,
    ui_nits: [f32; 4],
}

const DANMAKU_FILL_ATLAS_TEXTURE: u32 = 0;
const DANMAKU_OUTLINE_ATLAS_TEXTURE: u32 = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DanmakuBatchInstance {
    rect: [f32; 4],
    tex_rect: [f32; 4],
    color: [f32; 4],
    atlas_texture: u32,
    _reserved0: [u32; 3],
}

impl DanmakuBatchInstance {
    fn new(rect: [f32; 4], tex_rect: [f32; 4], color: [f32; 4], atlas_texture: u32) -> Self {
        Self {
            rect,
            tex_rect,
            color,
            atlas_texture,
            _reserved0: [0; 3],
        }
    }
}

impl OverlayUniforms {
    fn from_plane(
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        layout: VideoPresentationLayout,
        target: TargetColorState,
        edr_output: bool,
    ) -> Self {
        Self {
            rect: layout.map_source_rect(x as f32, y as f32, width as f32, height as f32),
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            viewport: layout.overlay_viewport(),
            overlay_mode: 0,
            target_transfer: transfer_code(target.transfer),
            color: [1.0, 1.0, 1.0, 1.0],
            ui_nits: ui_output_nits(target, edr_output),
        }
    }

    fn from_alpha_atlas_bitmap(
        bitmap: &SubtitleAlphaBitmap,
        placement: &OverlayAlphaAtlasPlacement,
        atlas_width: usize,
        atlas_height: usize,
        layout: VideoPresentationLayout,
        target: TargetColorState,
        edr_output: bool,
    ) -> Self {
        let color = AssColor::from_libass_rgba(bitmap.color_rgba);
        let atlas_width = atlas_width.max(1) as f32;
        let atlas_height = atlas_height.max(1) as f32;
        Self {
            rect: layout.map_source_rect(
                bitmap.placement.x as f32,
                bitmap.placement.y as f32,
                bitmap.placement.width as f32,
                bitmap.placement.height as f32,
            ),
            tex_rect: [
                placement.x as f32 / atlas_width,
                placement.y as f32 / atlas_height,
                bitmap.placement.width as f32 / atlas_width,
                bitmap.placement.height as f32 / atlas_height,
            ],
            viewport: layout.overlay_viewport(),
            overlay_mode: 1,
            target_transfer: transfer_code(target.transfer),
            color: [
                color.red as f32 / 255.0,
                color.green as f32 / 255.0,
                color.blue as f32 / 255.0,
                color.alpha as f32 / 255.0,
            ],
            ui_nits: ui_output_nits(target, edr_output),
        }
    }
}

fn overlay_uniform_pointer(uniforms: &OverlayUniforms) -> NonNull<c_void> {
    NonNull::new(
        (uniforms as *const OverlayUniforms)
            .cast::<c_void>()
            .cast_mut(),
    )
    .expect("overlay uniform pointer is non-null")
}

fn instance_bytes_len(instance_count: usize) -> Result<usize> {
    instance_count
        .checked_mul(mem::size_of::<DanmakuBatchInstance>())
        .ok_or_else(|| PlayerError::Renderer("danmaku batch instance buffer overflow".to_string()))
}

fn write_danmaku_instances_direct(
    buffer: &ProtocolObject<dyn MTLBuffer>,
    plan: &DanmakuRenderPlan,
    total_instances: usize,
) -> Result<()> {
    if total_instances == 0 {
        return Ok(());
    }
    let byte_len = instance_bytes_len(total_instances)?;
    let end = byte_len;
    if end > buffer.length() {
        return Err(PlayerError::Renderer(
            "danmaku instance buffer too small".to_string(),
        ));
    }
    unsafe {
        let dst = buffer.contents().as_ptr() as *mut DanmakuBatchInstance;
        let mut write_index = 0usize;
        let written = for_each_ordered_danmaku_instance(&plan.items, |instance| {
            dst.add(write_index).write(instance);
            write_index += 1;
        });
        debug_assert_eq!(written, total_instances);
        debug_assert_eq!(write_index, total_instances);
    }
    Ok(())
}

fn for_each_ordered_danmaku_instance(
    items: &[crate::danmaku::DanmakuGlyphInstance],
    mut emit: impl FnMut(DanmakuBatchInstance),
) -> usize {
    // Scanning for the next different `item_id` only yields whole danmaku
    // while each item's glyphs stay contiguous: one run per distinct item. If
    // that ever stops holding, items silently split into several groups and
    // the shadow/outline/fill layering breaks again with no other symptom.
    debug_assert_eq!(
        items
            .windows(2)
            .filter(|pair| pair[0].item_id != pair[1].item_id)
            .count()
            + usize::from(!items.is_empty()),
        items
            .iter()
            .map(|item| item.item_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "danmaku glyph instances must stay grouped by item_id"
    );

    let mut emitted = 0usize;
    let mut group_start = 0usize;
    while group_start < items.len() {
        let item_id = items[group_start].item_id;
        let group_end = items[group_start..]
            .iter()
            .position(|item| item.item_id != item_id)
            .map_or(items.len(), |offset| group_start + offset);
        let group = &items[group_start..group_end];

        // Preserve danmaku z-order as complete visual units. Drawing every
        // outline on the surface before every fill lets a lower item's fill
        // erase the upper item's edge where they overlap.
        for item in group {
            if item.shadow_rgba[3] > 0.0 {
                let mut rect = item.rect;
                rect[0] += item.shadow_offset[0];
                rect[1] += item.shadow_offset[1];
                emit(DanmakuBatchInstance::new(
                    rect,
                    item.tex_rect,
                    item.shadow_rgba,
                    DANMAKU_OUTLINE_ATLAS_TEXTURE,
                ));
                emitted += 1;
            }
        }
        for item in group {
            if item.outline_rgba[3] > 0.0 {
                emit(DanmakuBatchInstance::new(
                    item.rect,
                    item.tex_rect,
                    item.outline_rgba,
                    DANMAKU_OUTLINE_ATLAS_TEXTURE,
                ));
                emitted += 1;
            }
        }
        for item in group {
            emit(DanmakuBatchInstance::new(
                item.rect,
                item.tex_rect,
                item.color_rgba,
                DANMAKU_FILL_ATLAS_TEXTURE,
            ));
            emitted += 1;
        }
        group_start = group_end;
    }
    emitted
}

fn draw_danmaku_instance_batch(
    encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    fill_texture: &ProtocolObject<dyn MTLTexture>,
    outline_texture: &ProtocolObject<dyn MTLTexture>,
    buffer: &ProtocolObject<dyn MTLBuffer>,
    instance_count: usize,
) -> Result<()> {
    if instance_count == 0 {
        return Ok(());
    }
    unsafe {
        encoder.setFragmentTexture_atIndex(Some(fill_texture), 0);
        encoder.setFragmentTexture_atIndex(Some(outline_texture), 1);
        encoder.setVertexBuffer_offset_atIndex(Some(buffer), 0, 0);
        encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
            MTLPrimitiveType::Triangle,
            0,
            6,
            instance_count,
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayAlphaAtlasPlacement {
    bitmap_index: usize,
    x: usize,
    y: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OverlayAlphaAtlasPlan {
    width: usize,
    height: usize,
    stride: usize,
    pixels: Vec<u8>,
    placements: Vec<OverlayAlphaAtlasPlacement>,
}

struct OverlayAlphaAtlasCache {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    width: usize,
    height: usize,
    placements: Vec<OverlayAlphaAtlasPlacement>,
    signature: OverlayAlphaAtlasSignature,
}

struct DanmakuAlphaAtlasCache {
    version: u64,
    width: u32,
    height: u32,
    stride: usize,
    fill_texture: Retained<ProtocolObject<dyn MTLTexture>>,
    outline_texture: Retained<ProtocolObject<dyn MTLTexture>>,
}

#[derive(Default)]
struct DanmakuVertexBufferSlot {
    buffer: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    capacity: usize,
    in_flight: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
}

impl DanmakuVertexBufferSlot {
    fn is_reusable(&self) -> bool {
        self.in_flight.as_ref().is_none_or(|command_buffer| {
            matches!(
                command_buffer.status(),
                // `NotEnqueued` is deliberately absent: that buffer is still
                // being encoded, and handing its vertex storage to a second
                // draw in the same command buffer would overwrite geometry the
                // first draw already referenced. Metal executes lazily, so the
                // corruption would only appear on the GPU.
                MTLCommandBufferStatus::Completed | MTLCommandBufferStatus::Error
            )
        })
    }
}

impl DanmakuAlphaAtlasCache {
    fn can_reuse_for(&self, atlas: &DanmakuGlyphAtlas) -> bool {
        self.version == atlas.version
            && self.width == atlas.width
            && self.height == atlas.height
            && self.stride == atlas.stride
    }
}

fn update_danmaku_alpha_texture(
    texture: &ProtocolObject<dyn MTLTexture>,
    atlas: &DanmakuGlyphAtlas,
    pixels: &[u8],
    update: &DanmakuAtlasUpdate,
) {
    let offset = update.y as usize * atlas.stride + update.x as usize;
    let region = MTLRegion {
        origin: MTLOrigin {
            x: update.x as usize,
            y: update.y as usize,
            z: 0,
        },
        size: MTLSize {
            width: update.width as usize,
            height: update.height as usize,
            depth: 1,
        },
    };
    unsafe {
        texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
            region,
            0,
            NonNull::new(pixels[offset..].as_ptr().cast::<c_void>().cast_mut())
                .expect("danmaku atlas update pointer is non-null"),
            atlas.stride,
        );
    }
}

impl OverlayAlphaAtlasCache {
    fn new(
        texture: Retained<ProtocolObject<dyn MTLTexture>>,
        plan: OverlayAlphaAtlasPlan,
        bitmaps: &[SubtitleAlphaBitmap],
    ) -> Self {
        Self {
            texture,
            width: plan.width,
            height: plan.height,
            placements: plan.placements,
            signature: OverlayAlphaAtlasSignature::from_bitmaps(bitmaps),
        }
    }

    fn can_reuse_for(&self, bitmaps: &[SubtitleAlphaBitmap]) -> bool {
        self.signature == OverlayAlphaAtlasSignature::from_bitmaps(bitmaps)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OverlayAlphaAtlasSignature {
    bitmaps: Vec<OverlayAlphaBitmapSignature>,
}

impl OverlayAlphaAtlasSignature {
    fn from_bitmaps(bitmaps: &[SubtitleAlphaBitmap]) -> Self {
        Self {
            bitmaps: bitmaps
                .iter()
                .map(OverlayAlphaBitmapSignature::from_bitmap)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayAlphaBitmapSignature {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    stride: usize,
    color_rgba: u32,
    alpha_len: usize,
    alpha_hash: u64,
}

impl OverlayAlphaBitmapSignature {
    fn from_bitmap(bitmap: &SubtitleAlphaBitmap) -> Self {
        Self {
            x: bitmap.placement.x,
            y: bitmap.placement.y,
            width: bitmap.placement.width,
            height: bitmap.placement.height,
            stride: bitmap.stride,
            color_rgba: bitmap.color_rgba,
            alpha_len: bitmap.alpha.len(),
            alpha_hash: hash_alpha_bitmap(bitmap),
        }
    }
}

fn hash_alpha_bitmap(bitmap: &SubtitleAlphaBitmap) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    let width = bitmap.placement.width as usize;
    let height = bitmap.placement.height as usize;
    for row in 0..height {
        let row_start = row.saturating_mul(bitmap.stride);
        let row_end = row_start.saturating_add(width);
        let Some(bytes) = bitmap.alpha.get(row_start..row_end) else {
            break;
        };
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

impl OverlayAlphaAtlasPlan {
    fn pack(bitmaps: &[SubtitleAlphaBitmap]) -> Result<Option<Self>> {
        let total_width = bitmaps.iter().try_fold(0usize, |sum, bitmap| {
            let width = bitmap.placement.width as usize;
            sum.checked_add(width).ok_or_else(|| {
                PlayerError::Renderer("overlay alpha atlas width overflow".to_string())
            })
        })?;
        let max_height = bitmaps
            .iter()
            .map(|bitmap| bitmap.placement.height as usize)
            .max()
            .unwrap_or(0);
        if total_width == 0 || max_height == 0 {
            return Ok(None);
        }

        let width = total_width;
        let height = max_height;
        let len = width.checked_mul(height).ok_or_else(|| {
            PlayerError::Renderer("overlay alpha atlas size overflow".to_string())
        })?;
        let mut pixels = vec![0u8; len];
        let mut placements = Vec::with_capacity(bitmaps.len());
        let mut cursor_x = 0usize;

        for (bitmap_index, bitmap) in bitmaps.iter().enumerate() {
            let bitmap_width = bitmap.placement.width as usize;
            let bitmap_height = bitmap.placement.height as usize;
            if bitmap_width == 0 || bitmap_height == 0 {
                continue;
            }
            if !bitmap.is_valid() {
                return Err(PlayerError::Renderer(format!(
                    "subtitle alpha bitmap has {} bytes, expected at least {} for {}x{} stride {}",
                    bitmap.alpha.len(),
                    bitmap.required_len(),
                    bitmap.placement.width,
                    bitmap.placement.height,
                    bitmap.stride
                )));
            }

            for row in 0..bitmap_height {
                let src_start = row * bitmap.stride;
                let src_end = src_start + bitmap_width;
                let dst_start = row * width + cursor_x;
                let dst_end = dst_start + bitmap_width;
                pixels[dst_start..dst_end].copy_from_slice(&bitmap.alpha[src_start..src_end]);
            }

            placements.push(OverlayAlphaAtlasPlacement {
                bitmap_index,
                x: cursor_x,
                y: 0,
            });
            cursor_x += bitmap_width;
        }

        Ok(Some(Self {
            width,
            height,
            stride: width,
            pixels,
            placements,
        }))
    }
}

const VIDEO_SHADER_SOURCE: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexOut {
    float4 position [[position]];
    float2 tex_coord;
};

struct VideoUniforms {
    uint is_p010;
    uint full_range;
    uint source_transfer;
    uint target_transfer;
    uint tone_map;
    uint edr_output;
    uint video_alpha_mode;
    uint reserved1;
    float4 rect;
    float4 viewport;
    float4 nits;
    float4 luma_coefficients;
    float4 gamut_matrix_rows[3];
    float4 ipt_matrix_rows[9];
    float4 tone_map_extra;
    float4 tone_map_coeffs;
    uint gamut_lut_enabled;
    uint gamut_primaries;
    uint gamut_reserved0;
    uint gamut_reserved1;
    float4 dovi_flags;
    float4 dovi_pivots[6];
    float4 dovi_bounds[3];
    float4 dovi_coefficients[24];
    float4 dovi_mmr[144];
    float4 dovi_nonlinear_matrix[3];
    float4 dovi_nonlinear_offset;
    float4 dovi_lms_matrix[3];
};

float source_peak_nits(constant VideoUniforms& uniforms) {
    return max(uniforms.nits.x, 1.0);
}

float target_peak_nits(constant VideoUniforms& uniforms) {
    return max(uniforms.nits.y, 1.0);
}

float source_reference_white_nits(constant VideoUniforms& uniforms) {
    return max(uniforms.nits.z, 1.0);
}

float target_reference_white_nits(constant VideoUniforms& uniforms) {
    return max(uniforms.nits.w, 1.0);
}

float pq_eotf(float encoded) {
    constexpr float m1 = 0.1593017578125;
    constexpr float m2 = 78.84375;
    constexpr float c1 = 0.8359375;
    constexpr float c2 = 18.8515625;
    constexpr float c3 = 18.6875;
    float p = pow(max(encoded, 0.0), 1.0 / m2);
    float num = max(p - c1, 0.0);
    float den = max(c2 - c3 * p, 0.000001);
    return pow(num / den, 1.0 / m1);
}

float pq_inverse_eotf(float normalized_nits) {
    constexpr float m1 = 0.1593017578125;
    constexpr float m2 = 78.84375;
    constexpr float c1 = 0.8359375;
    constexpr float c2 = 18.8515625;
    constexpr float c3 = 18.6875;
    float p = pow(clamp(normalized_nits, 0.0, 1.0), m1);
    return pow((c1 + c2 * p) / max(1.0 + c3 * p, 0.000001), m2);
}

// BT.2100 HLG inverse OETF: nonlinear signal E' to scene linear light in
// [0, 1]. Mirrors the Rust reference implementation in
// `renderer/pipeline.rs` tests (`hlg_inverse_oetf`).
float hlg_inverse_oetf(float encoded) {
    constexpr float a = 0.17883277;
    constexpr float b = 0.28466892;
    constexpr float c = 0.55991073;
    float e = max(encoded, 0.0);
    if (e <= 0.5) {
        return e * e / 3.0;
    }
    return (exp((e - c) / a) + b) / 12.0;
}

float3 transfer_to_source_reference_linear(float3 rgb, constant VideoUniforms& uniforms) {
    rgb = max(rgb, float3(0.0));
    if (uniforms.source_transfer == 3) {
        constexpr float pq_absolute_peak_nits = 10000.0;
        return float3(pq_eotf(rgb.r), pq_eotf(rgb.g), pq_eotf(rgb.b))
            * (pq_absolute_peak_nits / source_reference_white_nits(uniforms));
    }
    if (uniforms.source_transfer == 4) {
        // HLG: inverse OETF to scene linear, then the BT.2100 OOTF (system
        // gamma 1.2 at the 1000 nit nominal peak) to display linear,
        // normalized to source reference white like the PQ branch above.
        constexpr float hlg_nominal_peak_nits = 1000.0;
        constexpr float hlg_system_gamma = 1.2;
        float3 scene = float3(
            hlg_inverse_oetf(rgb.r),
            hlg_inverse_oetf(rgb.g),
            hlg_inverse_oetf(rgb.b)
        );
        float scene_luma = max(dot(uniforms.luma_coefficients.xyz, scene), 0.000001);
        return scene * (hlg_nominal_peak_nits * pow(scene_luma, hlg_system_gamma - 1.0)
            / source_reference_white_nits(uniforms));
    }
    if (uniforms.source_transfer == 1) {
        return pow(rgb, float3(2.2));
    }
    if (uniforms.source_transfer == 2) {
        return pow(rgb, float3(2.4));
    }
    return rgb;
}

float3 source_reference_to_nits(float3 rgb, constant VideoUniforms& uniforms) {
    return max(rgb, float3(0.0)) * source_reference_white_nits(uniforms);
}

float pq_code(float nits) {
    return pq_inverse_eotf(clamp(nits, 0.0, 10000.0) / 10000.0);
}

float nits_from_pq(float code) {
    return 10000.0 * pq_eotf(clamp(code, 0.0, 1.0));
}

// Simple primaries conversion (HDR10 output path); the tone-mapped path
// converts primaries inside the IPT roundtrip instead.
float3 apply_gamut_map(float3 rgb, constant VideoUniforms& uniforms) {
    return float3(
        dot(uniforms.gamut_matrix_rows[0].xyz, rgb),
        dot(uniforms.gamut_matrix_rows[1].xyz, rgb),
        dot(uniforms.gamut_matrix_rows[2].xyz, rgb)
    );
}

// libplacebo pl_smoothstep with arbitrary edge order (Metal smoothstep has
// undefined results when edge0 >= edge1, and libplacebo's knee tuning term
// deliberately uses reversed edges).
float sstep(float edge0, float edge1, float x) {
    float t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// libplacebo st2094_pick_knee evaluated on absolute PQ codes. The source
// pivot follows the scene average luminance when known and stays within
// [10%, 80%] of the range; the destination pivot rescales it into the output
// range and then adapts towards the 1:1 line (knee_adaptation 0.4).
float2 st2094_pick_knee(float src_min, float src_max, float src_avg, float dst_min, float dst_max) {
    constexpr float knee_adaptation = 0.4;
    constexpr float min_knee = 0.1;
    constexpr float max_knee = 0.8;
    constexpr float def_knee = 0.4;
    float src_knee_min = mix(src_min, src_max, min_knee);
    float src_knee_max = mix(src_min, src_max, max_knee);
    float dst_knee_min = mix(dst_min, dst_max, min_knee);
    float dst_knee_max = mix(dst_min, dst_max, max_knee);
    float fallback = mix(src_min, src_max, def_knee);
    float src_knee = clamp(src_avg > 0.0 ? src_avg : fallback, src_knee_min, src_knee_max);
    float target = (src_knee - src_min) / max(src_max - src_min, 0.000001);
    float adapted = mix(dst_min, dst_max, target);
    float tuning = 1.0 - sstep(max_knee, def_knee, target) * sstep(min_knee, def_knee, target);
    float adaptation = mix(knee_adaptation, 1.0, tuning);
    float dst_knee = clamp(mix(src_knee, adapted, adaptation), dst_knee_min, dst_knee_max);
    return float2(src_knee, dst_knee);
}

// The tone-map curve evaluated on the IPT intensity axis (PQ codes),
// mirroring libplacebo's tone-map functions. `param` is the per-operator
// curve parameter from ToneMapConfig::curve_param (0 = operator default).
float tone_map_curve_pq(float x_in, float param, constant VideoUniforms& uniforms) {
    float src_peak = source_peak_nits(uniforms);
    float dst_peak = target_peak_nits(uniforms);
    float src_avg = uniforms.tone_map_extra.y;
    float dst_black = uniforms.tone_map_extra.z;
    float in_min = 0.0;
    float in_max = max(pq_code(src_peak), 0.000001);
    float out_min = pq_code(dst_black);
    float out_max = max(pq_code(dst_peak), 0.000001);
    float out_range = max(out_max - out_min, 0.000001);
    float x = clamp(x_in, in_min, in_max);
    if (uniforms.tone_map == 0) {
        // Clip: values within the source range pass through untouched.
        return x;
    }
    if (uniforms.tone_map == 1) {
        // Reinhard (output-relative, libplacebo pl_tone_map_reinhard).
        //
        // libplacebo evaluates this curve in the linear PL_HDR_NORM domain
        // and only the PQ-domain operators (BT.2390, spline, ST 2094-10)
        // work on encoded values. Running it on PQ codes bends the curve's
        // meaning: against a 1000->203 nit target mid-tones darken badly, and
        // the Mobius knee lands far below its intended position. Rescale to
        // linear nits here and encode back at the end.
        float in_max_nits = max(src_peak, 0.000001);
        float out_min_nits = dst_black;
        float out_range_nits = max(dst_peak - dst_black, 0.000001);
        float peak = in_max_nits / out_range_nits;
        float contrast = param > 0.0 ? param : 0.5;
        float offset = (1.0 - contrast) / max(contrast, 0.000001);
        float scale = (peak + offset) / peak;
        float t = clamp(nits_from_pq(x), 0.0, in_max_nits) / out_range_nits;
        float mapped = t / (t + offset) * scale;
        return pq_code(mapped * out_range_nits + out_min_nits);
    }
    if (uniforms.tone_map == 2) {
        // Mobius: Mobius transform with a 1:1 linear region below the knee,
        // also evaluated in linear nits (see the Reinhard note above). The
        // knee j is relative to the output range, so the linear region ends
        // at dst_black + j * (dst_peak - dst_black).
        float in_max_nits = max(src_peak, 0.000001);
        float out_min_nits = dst_black;
        float out_range_nits = max(dst_peak - dst_black, 0.000001);
        float peak = in_max_nits / out_range_nits;
        float j = param > 0.0 ? param : 0.3;
        float a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
        float b = (j * j - 2.0 * j * peak + peak) / max(peak - 1.0, 0.000001);
        float scale = (b * b + 2.0 * b * j + j * j) / (b - a);
        float t = clamp(nits_from_pq(x), 0.0, in_max_nits) / out_range_nits;
        float mapped = t > j ? scale * (t + a) / (t + b) : t;
        return pq_code(mapped * out_range_nits + out_min_nits);
    }
    if (uniforms.tone_map == 3) {
        // ITU-R BT.2390 EETF with black-point compensation (the libplacebo
        // version also compensates target black; the earlier port skipped it).
        float knee_offset = param > 0.0 ? param : 1.0;
        float max_lum = clamp(out_max / in_max, 0.0, 1.0);
        float min_lum = out_min / in_max;
        float ks = (1.0 + knee_offset) * max_lum - knee_offset;
        float bp = min(max(1.0 / max(min_lum, 0.000001), 0.0), 4.0);
        float u = x / in_max;
        if (ks < 1.0 && u > ks) {
            float tb = (u - ks) / (1.0 - ks);
            float tb2 = tb * tb;
            float tb3 = tb2 * tb;
            u = (2.0 * tb3 - 3.0 * tb2 + 1.0) * ks
               + (tb3 - 2.0 * tb2 + tb) * (1.0 - ks)
               + (-2.0 * tb3 + 3.0 * tb2) * max_lum;
        }
        if (u < 1.0) {
            u = u + min_lum * pow(1.0 - u, bp);
            float gain = max_lum < 1.0
                ? 1.0 / (1.0 + min_lum / max_lum * pow(1.0 - max_lum, bp))
                : 1.0;
            u = gain * (u - min_lum) + min_lum;
        }
        return u * in_max;
    }
    if (uniforms.tone_map == 4) {
        // Spline: perceptually linear single-pivot polynomial, the default
        // tone map of libplacebo and mpv's gpu-next renderer.
        float contrast = param > 0.0 ? param : 0.3;
        float fallback_avg = clamp(0.4 * src_peak, 100.0, 400.0);
        float effective_src_avg = src_avg > 0.0 ? src_avg : fallback_avg;
        float2 knee = st2094_pick_knee(
            in_min,
            in_max,
            pq_code(effective_src_avg),
            out_min,
            out_max
        );
        float src_pivot = knee.x;
        float dst_pivot = knee.y;
        float slope0 = (dst_pivot - out_min) / max(src_pivot - in_min, 0.000001);
        float ratio = clamp(1.5 * (in_max / out_max - 1.0), 0.2, 1.2);
        float slope = pow(slope0, (1.0 - contrast) * ratio);
        float in_min0 = in_min - src_pivot;
        float in_max0 = in_max - src_pivot;
        float out_min0 = out_min - dst_pivot;
        float out_max0 = out_max - dst_pivot;
        float pa = (out_min0 - slope * in_min0) / (in_min0 * in_min0);
        float qa = (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0 * in_max0);
        float qb = -3.0 * (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0);
        float xr = x - src_pivot;
        float mapped = xr > 0.0
            ? ((qa * xr + qb) * xr + slope) * xr
            : (pa * xr + slope) * xr;
        return mapped + dst_pivot;
    }
    if (uniforms.tone_map == 5) {
        // ITU-R BT.2446 method A: Weber-law log compression from the source
        // peak envelope and a standardized S-curve (mpv's recommended curve
        // for well-mastered content).
        float phdr = 1.0 + 32.0 * pow(src_peak / 10000.0, 1.0 / 2.4);
        float psdr = 1.0 + 32.0 * pow(dst_peak / 10000.0, 1.0 / 2.4);
        float t = pow(nits_from_pq(x) / max(src_peak, 0.000001), 1.0 / 2.4);
        t = log(1.0 + (phdr - 1.0) * t) / log(phdr);
        if (t <= 0.7399) {
            t = 1.0770 * t;
        } else if (t < 0.9909) {
            t = (-1.1510 * t + 2.7811) * t - 0.6302;
        } else {
            t = 0.5 * t + 0.5;
        }
        t = (pow(psdr, t) - 1.0) / (psdr - 1.0);
        // BT.1886 EOTF from the target black point and peak.
        float lb = pow(max(dst_black, 0.0), 1.0 / 2.4);
        float lw = pow(max(dst_peak, 0.0), 1.0 / 2.4);
        return pq_code(pow((lw - lb) * t + lb, 2.4));
    }
    // SMPTE ST 2094-10 (DolbyVision's dynamic-metadata curve): rational
    // Mobius interpolation in absolute nits; coefficients are solved per
    // frame on the CPU from the same scene pivot.
    float c1 = uniforms.tone_map_coeffs.x;
    float c2 = uniforms.tone_map_coeffs.y;
    float c3 = uniforms.tone_map_coeffs.z;
    float x_nits = nits_from_pq(x);
    float y_nits = (c1 + c2 * x_nits) / max(1.0 + c3 * x_nits, 0.000001);
    return pq_code(clamp(y_nits, 0.0, 10000.0));
}

float3 tone_map_nits(
    float3 input_nits,
    texture3d<float, access::sample> gamut_lut,
    sampler video_sampler,
    constant VideoUniforms& uniforms
) {
    if (uniforms.target_transfer == 3) {
        // HDR10 output: convert primaries by the gamut matrix and clamp to
        // the PQ range (no tone mapping; the display does the HDR mapping).
        return clamp(apply_gamut_map(max(input_nits, float3(0.0)) / source_reference_white_nits(uniforms), uniforms)
            * source_reference_white_nits(uniforms), float3(0.0), float3(10000.0));
    }
    // libplacebo color map: RGB in source primaries (absolute nits) to
    // HPE-LMS, PQ-encode, IPT, map the intensity axis and apply the
    // hue-preserving chroma rule, optionally sample the 3D gamut LUT in
    // IPT space, then decode back to RGB in the target primaries (rows 6-8).
    // The primaries conversion and gamut mapping happen in this single IPT pass.
    float3 rgb = max(input_nits, float3(0.0));
    float3 lms = float3(
        dot(uniforms.ipt_matrix_rows[0].xyz, rgb),
        dot(uniforms.ipt_matrix_rows[1].xyz, rgb),
        dot(uniforms.ipt_matrix_rows[2].xyz, rgb)
    );
    float3 lmspq = float3(pq_code(lms.r), pq_code(lms.g), pq_code(lms.b));
    float3 ipt = float3(
        dot(float3(0.4, 0.4, 0.2), lmspq),
        dot(float3(4.455, -4.851, 0.396), lmspq),
        dot(float3(0.8056, 0.3572, -1.1628), lmspq)
    );
    float i_orig = ipt.x;
    ipt.x = tone_map_curve_pq(ipt.x, uniforms.tone_map_extra.x, uniforms);
    // Libplacebo's chroma rule: clamp the saturation boost when brightening
    // and desaturate (by the cubic hull term) when the mapping darkens.
    float2 hull = float2(i_orig, ipt.x);
    float2 hull_c = ((hull - float2(6.0)) * hull + float2(9.0)) * hull;
    float ratio = min(i_orig / max(ipt.x, 0.000001), hull_c.y / max(hull_c.x, 0.000001));
    ipt.yz = ipt.yz * ratio;

    if (uniforms.gamut_lut_enabled != 0) {
        // I axis spans the target's [black, peak] in PQ codes, matching
        // libplacebo's gamut.min_luma/max_luma (tone_map_extra.z is the
        // target black in nits, the same value the LUT was generated for).
        float lut_min = pq_code(uniforms.tone_map_extra.z);
        float lut_max = max(pq_code(target_peak_nits(uniforms)), 0.000001);
        float lut_range = max(lut_max - lut_min, 0.000001);
        float3 pos = float3(
            clamp((ipt.x - lut_min) / lut_range, 0.0, 1.0),
            clamp(2.0 * length(ipt.yz), 0.0, 1.0),
            0.5 + 0.5 * atan2(ipt.z, ipt.y) / 3.14159265
        );
        // libplacebo's texel_scale: the lattice position must be remapped to
        // the texel-center coordinate, otherwise the low end of the chroma
        // axis (whose first texel stores zero chroma) leaks in and crushes
        // saturation.
        float3 idx = float3(
            pos.x * (47.0 / 48.0) + 0.5 / 48.0,
            pos.y * (31.0 / 32.0) + 0.5 / 32.0,
            pos.z * (255.0 / 256.0) + 0.5 / 256.0
        );
        float3 sampled = gamut_lut.sample(video_sampler, idx).xyz;
        ipt = float3(sampled.x, sampled.y - 0.5, sampled.z - 0.5);
    }

    float3 lmspq_out = float3(
        dot(float3(1.0, 0.0975689, 0.205226), ipt),
        dot(float3(1.0, -0.113876, 0.133217), ipt),
        dot(float3(1.0, 0.0326151, -0.676887), ipt)
    );
    float3 lms_out = float3(
        nits_from_pq(lmspq_out.r),
        nits_from_pq(lmspq_out.g),
        nits_from_pq(lmspq_out.b)
    );
    return float3(
        dot(uniforms.ipt_matrix_rows[6].xyz, lms_out),
        dot(uniforms.ipt_matrix_rows[7].xyz, lms_out),
        dot(uniforms.ipt_matrix_rows[8].xyz, lms_out)
    );
}

// Hue-preserving gamut mapping: the linear gamut matrix can push highly
// saturated wide-gamut colors outside the target gamut (negative
// components). Blending those towards luma shifts hue — BT.2020 primary
// red picks up blue and turns pink. Instead blend towards the naive clip
// by an out-of-gamut smoothstep factor: slightly-out colors stay nearly
// intact, strongly-out primaries land on the pure target primary with
// their hue intact, matching mpv's perceptual gamut handling. Mirrors the
// WGSL/HLSL `gamut_compress` and the Rust reference in pipeline.rs tests.
// Brightness overshoot (> 1) is left for the tone map.
float3 gamut_compress(float3 rgb) {
    float lo = min(rgb.r, min(rgb.g, rgb.b));
    float outness = max(-lo, 0.0);
    float k = smoothstep(0.0, 1.0, outness);
    return mix(rgb, clamp(rgb, 0.0, 1.0), k);
}

float3 target_nits_to_reference_linear(float3 nits, constant VideoUniforms& uniforms) {
    // libplacebo's encode maps [target black, target peak] onto [0, 1] where
    // 1.0 is the target reference white, so the tone-map black-point
    // compensation lands back on true black instead of lifting it.
    float black = uniforms.tone_map_extra.z;
    float peak = target_peak_nits(uniforms);
    float range = max(peak - black, 0.0001);
    return max(nits - float3(black), float3(0.0)) / range
        * (range / target_reference_white_nits(uniforms));
}

float3 target_reference_linear_to_output(float3 rgb, constant VideoUniforms& uniforms) {
    if (uniforms.target_transfer == 3) {
        constexpr float pq_absolute_peak_nits = 10000.0;
        float3 nits = max(rgb, float3(0.0)) * target_reference_white_nits(uniforms);
        return float3(
            pq_inverse_eotf(nits.r / pq_absolute_peak_nits),
            pq_inverse_eotf(nits.g / pq_absolute_peak_nits),
            pq_inverse_eotf(nits.b / pq_absolute_peak_nits)
        );
    }
    if (uniforms.edr_output != 0) {
        return max(rgb, float3(0.0));
    }
    if (uniforms.target_transfer == 1) {
        return pow(max(rgb, float3(0.0)), float3(1.0 / 2.2));
    }
    if (uniforms.target_transfer == 2) {
        return pow(max(rgb, float3(0.0)), float3(1.0 / 2.4));
    }
    return rgb;
}

float4 final_output(float3 rgb, float alpha, constant VideoUniforms& uniforms) {
    float3 premultiplied;
    if (uniforms.target_transfer == 3) {
        premultiplied = clamp(rgb, 0.0, 1.0) * alpha;
        return float4(premultiplied, alpha);
    }
    if (uniforms.edr_output != 0) {
        float headroom = max(target_peak_nits(uniforms) / target_reference_white_nits(uniforms), 1.0);
        premultiplied = clamp(rgb, 0.0, headroom) * alpha;
        return float4(premultiplied, alpha);
    }
    premultiplied = clamp(rgb, 0.0, 1.0) * alpha;
    return float4(premultiplied, alpha);
}

// SDR composites the UI in the output transfer. HDR10 (target_transfer == 3)
// PQ-encodes it against ui_nits.x. An Apple EDR / extended-linear drawable
// holds linear light (ui_nits.y != 0), so the sRGB-encoded UI color must be
// linearized and scaled to the same reference white as the video pass.
float3 sdr_ui_color_to_target_output(float3 rgb, uint target_transfer, float4 ui_nits) {
    if (target_transfer == 3) {
        constexpr float pq_absolute_peak_nits = 10000.0;
        float3 linear = pow(max(rgb, float3(0.0)), float3(2.2));
        float3 nits = linear * max(ui_nits.x, 1.0);
        return float3(
            pq_inverse_eotf(nits.r / pq_absolute_peak_nits),
            pq_inverse_eotf(nits.g / pq_absolute_peak_nits),
            pq_inverse_eotf(nits.b / pq_absolute_peak_nits)
        );
    }
    if (ui_nits.y > 0.0) {
        float3 linear_rgb = pow(max(rgb, float3(0.0)), float3(2.2));
        return linear_rgb * (max(ui_nits.x, 1.0) / max(ui_nits.y, 1.0));
    }
    return rgb;
}

struct RangeExpandedYCbCr {
    float y;
    float2 cbcr;
};

RangeExpandedYCbCr expand_ycbcr_range(float y, float2 cbcr, constant VideoUniforms& uniforms) {
    if (uniforms.is_p010 != 0) {
        // P010 stores 10-bit codes as code << 6 in a 16-bit UNORM texture.
        constexpr float p010_scale = 65535.0 / 65472.0;
        y *= p010_scale;
        cbcr *= p010_scale;
    }
    if (uniforms.full_range != 0) {
        return RangeExpandedYCbCr { y, cbcr - float2(0.5) };
    }

    if (uniforms.is_p010 != 0) {
        y = (y - (64.0 / 1023.0)) * (1023.0 / 876.0);
        cbcr = (cbcr - float2(512.0 / 1023.0)) * (1023.0 / 896.0);
        return RangeExpandedYCbCr { y, cbcr };
    }

    y = (y - (16.0 / 255.0)) * (255.0 / 219.0);
    cbcr = (cbcr - float2(128.0 / 255.0)) * (255.0 / 224.0);
    return RangeExpandedYCbCr { y, cbcr };
}

// Dolby Vision RPU reshaping, ported from libplacebo's `pl_shader_dovi_reshape`
// (the renderer behind mpv's Dolby Vision mapping). The base-layer signal is
// reshaped per component through piecewise polynomial/MMR curves selected by
// pivot comparison, where MMR coefficients mix all three raw components.
float3 dovi_reshaped_signal(float3 sig_in, constant VideoUniforms& uniforms) {
    float3 sig = clamp(sig_in, 0.0, 1.0);
    float result[3] = { sig.r, sig.g, sig.b };
    float4 flags = uniforms.dovi_flags;
    for (uint c = 0u; c < 3u; c = c + 1u) {
        uint segments = uint(flags[1u + c]);
        if (segments == 0u) {
            continue;
        }
        float s = result[c];
        uint index = 0u;
        for (uint i = 0u; i < 7u; i = i + 1u) {
            float4 pivot_row = uniforms.dovi_pivots[2u * c + i / 4u];
            float pivot = pivot_row[i % 4u];
            if (s >= pivot) {
                index = index + 1u;
            }
        }
        float4 coeff = uniforms.dovi_coefficients[8u * c + index];
        if (coeff.w < 0.5) {
            s = (coeff.z * s + coeff.y) * s + coeff.x;
        } else {
            uint base = 48u * c + uint(coeff.y);
            uint order = uint(coeff.w);
            float4 sig_x = float4(
                sig.x * sig.y,
                sig.x * sig.z,
                sig.y * sig.z,
                sig.x * sig.y * sig.z
            );
            s = coeff.x;
            s = s + dot(uniforms.dovi_mmr[base].xyz, sig);
            s = s + dot(uniforms.dovi_mmr[base + 1u], sig_x);
            if (order >= 2u) {
                float3 sig2 = sig * sig;
                float4 sig_x2 = sig_x * sig_x;
                s = s + dot(uniforms.dovi_mmr[base + 2u].xyz, sig2);
                s = s + dot(uniforms.dovi_mmr[base + 3u], sig_x2);
                if (order >= 3u) {
                    s = s + dot(uniforms.dovi_mmr[base + 4u].xyz, sig2 * sig);
                    s = s + dot(uniforms.dovi_mmr[base + 5u], sig_x2 * sig_x);
                }
            }
        }
        float4 bounds = uniforms.dovi_bounds[c];
        result[c] = clamp(s, bounds.x, bounds.y);
    }
    return float3(result[0], result[1], result[2]);
}

// Reshaped nonlinear signal to PQ-encoded IPT via the RPU's ycc_to_rgb matrix
// and signal offsets. Applying the RPU offsets keeps integer offset codes
// exactly on sample codes (2^bits/(2^bits-1) folded in on the CPU).
float3 dovi_signal_to_pq_rgb(float3 sig, constant VideoUniforms& uniforms) {
    float3 reshaped = dovi_reshaped_signal(sig, uniforms) - uniforms.dovi_nonlinear_offset.xyz;
    return float3(
        dot(uniforms.dovi_nonlinear_matrix[0].xyz, reshaped),
        dot(uniforms.dovi_nonlinear_matrix[1].xyz, reshaped),
        dot(uniforms.dovi_nonlinear_matrix[2].xyz, reshaped)
    );
}

// Linearized BT.2020-referred HPE LMS back to linear RGB, using the composite
// of the fixed HPE inverse with the RPU's rgb_to_lms matrix (premultiplied on
// the CPU, matching libplacebo's dovi_lms2rgb).
float3 dovi_lms_to_rgb(float3 linear, constant VideoUniforms& uniforms) {
    return float3(
        dot(uniforms.dovi_lms_matrix[0].xyz, linear),
        dot(uniforms.dovi_lms_matrix[1].xyz, linear),
        dot(uniforms.dovi_lms_matrix[2].xyz, linear)
    );
}

vertex VertexOut erika_video_vertex(
    uint vertex_id [[vertex_id]],
    constant VideoUniforms& uniforms [[buffer(0)]]) {
    constexpr float2 unit_positions[4] = {
        float2(0.0, 0.0),
        float2(1.0, 0.0),
        float2(0.0, 1.0),
        float2(1.0, 1.0),
    };
    constexpr float2 tex_coords[4] = {
        float2(0.0, 0.0),
        float2(1.0, 0.0),
        float2(0.0, 1.0),
        float2(1.0, 1.0),
    };
    float2 pixel = uniforms.rect.xy + unit_positions[vertex_id] * uniforms.rect.zw;
    float2 ndc = float2(
        pixel.x / max(uniforms.viewport.x, 1.0) * 2.0 - 1.0,
        1.0 - pixel.y / max(uniforms.viewport.y, 1.0) * 2.0
    );
    VertexOut out;
    out.position = float4(ndc, 0.0, 1.0);
    out.tex_coord = tex_coords[vertex_id];
    return out;
}

fragment float4 erika_video_fragment(
    VertexOut in [[stage_in]],
    texture2d<float, access::sample> luma_texture [[texture(0)]],
    texture2d<float, access::sample> chroma_texture [[texture(1)]],
    texture3d<float, access::sample> gamut_lut [[texture(2)]],
    sampler video_sampler [[sampler(0)]],
    constant VideoUniforms& uniforms [[buffer(0)]]) {
    bool packed_alpha = uniforms.video_alpha_mode == 1;
    float2 color_coord = packed_alpha
        ? float2(in.tex_coord.x * 0.5, in.tex_coord.y)
        : in.tex_coord;
    float2 alpha_coord = float2(0.5 + in.tex_coord.x * 0.5, in.tex_coord.y);
    float y_sample = luma_texture.sample(video_sampler, color_coord).r;
    float2 cbcr_sample = chroma_texture.sample(video_sampler, color_coord).rg;
    bool dovi_enabled = uniforms.dovi_flags.x != 0.0;
    float3 rgb;
    if (dovi_enabled) {
        // The base layer carries the raw 12-bit DV signal (10-bit container,
        // full range); range expansion and the YCbCr matrix are replaced by
        // the RPU reshaping + ycc_to_rgb path.
        float3 sig = float3(y_sample, cbcr_sample.x, cbcr_sample.y);
        if (uniforms.is_p010 != 0) {
            sig *= 65535.0 / 65472.0;
        }
        rgb = dovi_signal_to_pq_rgb(sig, uniforms);
    } else {
        RangeExpandedYCbCr expanded = expand_ycbcr_range(y_sample, cbcr_sample, uniforms);
        float y = expanded.y;
        float2 cbcr = expanded.cbcr;

        float kr = uniforms.luma_coefficients.x;
        float kg = max(uniforms.luma_coefficients.y, 0.000001);
        float kb = uniforms.luma_coefficients.z;
        rgb.r = y + 2.0 * (1.0 - kr) * cbcr.y;
        rgb.b = y + 2.0 * (1.0 - kb) * cbcr.x;
        rgb.g = (y - kr * rgb.r - kb * rgb.b) / kg;
    }
    rgb = transfer_to_source_reference_linear(rgb, uniforms);
    if (dovi_enabled) {
        rgb = dovi_lms_to_rgb(rgb, uniforms);
    }
    rgb = source_reference_to_nits(rgb, uniforms);
    rgb = tone_map_nits(rgb, gamut_lut, video_sampler, uniforms);
    rgb = target_nits_to_reference_linear(rgb, uniforms);
    rgb = gamut_compress(rgb);
    rgb = target_reference_linear_to_output(rgb, uniforms);
    float alpha = 1.0;
    if (packed_alpha) {
        float alpha_sample = luma_texture.sample(video_sampler, alpha_coord).r;
        alpha = clamp(expand_ycbcr_range(alpha_sample, float2(0.5), uniforms).y, 0.0, 1.0);
    }
    return final_output(rgb, alpha, uniforms);
}

struct OverlayUniforms {
    float4 rect;
    float4 tex_rect;
    float2 viewport;
    uint overlay_mode;
    uint target_transfer;
    float4 color;
    float4 ui_nits;
};

vertex VertexOut erika_overlay_vertex(
    uint vertex_id [[vertex_id]],
    constant OverlayUniforms& uniforms [[buffer(0)]]) {
    constexpr float2 unit_positions[4] = {
        float2(0.0, 0.0),
        float2(1.0, 0.0),
        float2(0.0, 1.0),
        float2(1.0, 1.0),
    };
    constexpr float2 tex_coords[4] = {
        float2(0.0, 0.0),
        float2(1.0, 0.0),
        float2(0.0, 1.0),
        float2(1.0, 1.0),
    };

    float2 pixel = uniforms.rect.xy + unit_positions[vertex_id] * uniforms.rect.zw;
    float2 ndc = float2(
        pixel.x / max(uniforms.viewport.x, 1.0) * 2.0 - 1.0,
        1.0 - pixel.y / max(uniforms.viewport.y, 1.0) * 2.0
    );

    VertexOut out;
    out.position = float4(ndc, 0.0, 1.0);
    out.tex_coord = uniforms.tex_rect.xy + tex_coords[vertex_id] * uniforms.tex_rect.zw;
    return out;
}

fragment float4 erika_overlay_fragment(
    VertexOut in [[stage_in]],
    texture2d<float, access::sample> overlay_texture [[texture(0)]],
    sampler overlay_sampler [[sampler(0)]],
    constant OverlayUniforms& uniforms [[buffer(0)]]) {
    float4 sampled = overlay_texture.sample(overlay_sampler, in.tex_coord);
    if (uniforms.overlay_mode == 1) {
        float3 rgb = sdr_ui_color_to_target_output(
            uniforms.color.rgb,
            uniforms.target_transfer,
            uniforms.ui_nits
        );
        return float4(rgb, uniforms.color.a * sampled.r);
    }
    sampled.rgb = sdr_ui_color_to_target_output(
        sampled.rgb,
        uniforms.target_transfer,
        uniforms.ui_nits
    );
    return sampled;
}

struct DanmakuBatchUniforms {
    float2 viewport;
    uint target_transfer;
    uint reserved0;
    float4 ui_nits;
};

struct DanmakuBatchInstance {
    float4 rect;
    float4 tex_rect;
    float4 color;
    uint atlas_texture;
    uint reserved0;
    uint reserved1;
    uint reserved2;
};

struct DanmakuBatchOut {
    float4 position [[position]];
    float2 tex_coord;
    float4 color;
    uint atlas_texture [[flat]];
};

vertex DanmakuBatchOut erika_danmaku_batch_vertex(
    uint vertex_id [[vertex_id]],
    uint instance_id [[instance_id]],
    constant DanmakuBatchInstance* instances [[buffer(0)]],
    constant DanmakuBatchUniforms& uniforms [[buffer(1)]]) {
    constexpr float2 corners[6] = {
        float2(0.0, 0.0),
        float2(1.0, 0.0),
        float2(0.0, 1.0),
        float2(1.0, 0.0),
        float2(1.0, 1.0),
        float2(0.0, 1.0),
    };
    DanmakuBatchInstance glyph = instances[instance_id];
    float2 corner = corners[vertex_id];
    float2 position = glyph.rect.xy + corner * glyph.rect.zw;
    float2 tex_coord = glyph.tex_rect.xy + corner * glyph.tex_rect.zw;
    float2 ndc = float2(
        position.x / max(uniforms.viewport.x, 1.0) * 2.0 - 1.0,
        1.0 - position.y / max(uniforms.viewport.y, 1.0) * 2.0
    );
    DanmakuBatchOut out;
    out.position = float4(ndc, 0.0, 1.0);
    out.tex_coord = tex_coord;
    out.color = glyph.color;
    out.atlas_texture = glyph.atlas_texture;
    return out;
}

fragment float4 erika_danmaku_batch_fragment(
    DanmakuBatchOut in [[stage_in]],
    texture2d<float, access::sample> fill_atlas [[texture(0)]],
    texture2d<float, access::sample> outline_atlas [[texture(1)]],
    sampler atlas_sampler [[sampler(0)]],
    constant DanmakuBatchUniforms& uniforms [[buffer(1)]]) {
    float mask = in.atlas_texture == 0
        ? fill_atlas.sample(atlas_sampler, in.tex_coord).r
        : outline_atlas.sample(atlas_sampler, in.tex_coord).r;
    float3 rgb = sdr_ui_color_to_target_output(
        in.color.rgb,
        uniforms.target_transfer,
        uniforms.ui_nits
    );
    return float4(rgb, in.color.a * mask);
}
"#;

#[derive(Debug, Clone, Copy)]
struct PixelBufferMapping {
    format: ImportedVideoFormat,
    full_range: bool,
    planes: [PlaneMapping; 2],
}

impl PixelBufferMapping {
    fn from_core_video_format(pixel_format: u32) -> Option<Self> {
        match pixel_format {
            CV_PIXEL_FORMAT_420_YP_CB_CR10_BI_PLANAR_VIDEO_RANGE => Some(Self {
                format: ImportedVideoFormat::P010,
                full_range: false,
                planes: [
                    PlaneMapping {
                        index: 0,
                        pixel_format: MTLPixelFormat::R16Unorm,
                        name: "R16Unorm",
                    },
                    PlaneMapping {
                        index: 1,
                        pixel_format: MTLPixelFormat::RG16Unorm,
                        name: "RG16Unorm",
                    },
                ],
            }),
            CV_PIXEL_FORMAT_420_YP_CB_CR10_BI_PLANAR_FULL_RANGE => Some(Self {
                format: ImportedVideoFormat::P010,
                full_range: true,
                planes: [
                    PlaneMapping {
                        index: 0,
                        pixel_format: MTLPixelFormat::R16Unorm,
                        name: "R16Unorm",
                    },
                    PlaneMapping {
                        index: 1,
                        pixel_format: MTLPixelFormat::RG16Unorm,
                        name: "RG16Unorm",
                    },
                ],
            }),
            CV_PIXEL_FORMAT_420_YP_CB_CR8_BI_PLANAR_VIDEO_RANGE => Some(Self {
                format: ImportedVideoFormat::Nv12,
                full_range: false,
                planes: [
                    PlaneMapping {
                        index: 0,
                        pixel_format: MTLPixelFormat::R8Unorm,
                        name: "R8Unorm",
                    },
                    PlaneMapping {
                        index: 1,
                        pixel_format: MTLPixelFormat::RG8Unorm,
                        name: "RG8Unorm",
                    },
                ],
            }),
            CV_PIXEL_FORMAT_420_YP_CB_CR8_BI_PLANAR_FULL_RANGE => Some(Self {
                format: ImportedVideoFormat::Nv12,
                full_range: true,
                planes: [
                    PlaneMapping {
                        index: 0,
                        pixel_format: MTLPixelFormat::R8Unorm,
                        name: "R8Unorm",
                    },
                    PlaneMapping {
                        index: 1,
                        pixel_format: MTLPixelFormat::RG8Unorm,
                        name: "RG8Unorm",
                    },
                ],
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PlaneMapping {
    index: usize,
    pixel_format: MTLPixelFormat,
    name: &'static str,
}

fn create_plane_texture(
    cache: &CVMetalTextureCache,
    image_buffer: &CVImageBuffer,
    pixel_format: MTLPixelFormat,
    width: usize,
    height: usize,
    plane_index: usize,
) -> Result<CFRetained<CVMetalTexture>> {
    let mut raw_texture: *mut CVMetalTexture = std::ptr::null_mut();
    let status = unsafe {
        CVMetalTextureCache::create_texture_from_image(
            None,
            cache,
            image_buffer,
            None,
            pixel_format,
            width,
            height,
            plane_index,
            NonNull::new(&mut raw_texture as *mut *mut CVMetalTexture)
                .expect("stack texture pointer is non-null"),
        )
    };
    if status != kCVReturnSuccess {
        return Err(PlayerError::Renderer(format!(
            "CVMetalTextureCacheCreateTextureFromImage failed for plane {plane_index}: {status}"
        )));
    }
    let raw_texture = NonNull::new(raw_texture).ok_or_else(|| {
        PlayerError::Renderer(format!(
            "CVMetalTextureCacheCreateTextureFromImage returned null for plane {plane_index}"
        ))
    })?;
    Ok(unsafe { CFRetained::from_raw(raw_texture) })
}

#[cfg(test)]
mod tests {
    use super::{
        DANMAKU_FILL_ATLAS_TEXTURE, DANMAKU_OUTLINE_ATLAS_TEXTURE, DanmakuBatchInstance,
        VIDEO_SHADER_SOURCE, for_each_ordered_danmaku_instance, metal_pixel_format, ui_output_nits,
    };
    use crate::core::{ColorPrimaries, TransferFunction};
    use crate::danmaku::DanmakuGlyphInstance;
    use crate::renderer::metal::MetalDrawablePixelFormat;
    use crate::renderer::pipeline::TargetColorState;
    use objc2_metal::MTLPixelFormat;

    #[test]
    fn video_shader_has_bit_depth_aware_range_expansion() {
        assert!(VIDEO_SHADER_SOURCE.contains("uniforms.is_p010"));
        assert!(VIDEO_SHADER_SOURCE.contains("64.0 / 1023.0"));
        assert!(VIDEO_SHADER_SOURCE.contains("512.0 / 1023.0"));
        assert!(VIDEO_SHADER_SOURCE.contains("1023.0 / 876.0"));
        assert!(VIDEO_SHADER_SOURCE.contains("1023.0 / 896.0"));
        assert!(VIDEO_SHADER_SOURCE.contains("16.0 / 255.0"));
        assert!(VIDEO_SHADER_SOURCE.contains("128.0 / 255.0"));
        assert!(VIDEO_SHADER_SOURCE.contains("255.0 / 219.0"));
        assert!(VIDEO_SHADER_SOURCE.contains("255.0 / 224.0"));
    }

    #[test]
    fn video_shader_keeps_source_and_target_transfer_separate() {
        assert!(VIDEO_SHADER_SOURCE.contains("source_transfer"));
        assert!(VIDEO_SHADER_SOURCE.contains("target_transfer"));
    }

    #[test]
    fn video_shader_maps_video_quad_from_presentation_layout() {
        assert!(VIDEO_SHADER_SOURCE.contains("float4 rect"));
        assert!(VIDEO_SHADER_SOURCE.contains("float4 viewport"));
        assert!(VIDEO_SHADER_SOURCE.contains("uniforms.rect.xy"));
        assert!(VIDEO_SHADER_SOURCE.contains("uniforms.rect.zw"));
    }

    #[test]
    fn video_shader_has_edr_output_headroom_clamp() {
        assert!(VIDEO_SHADER_SOURCE.contains("edr_output"));
        assert!(
            VIDEO_SHADER_SOURCE
                .contains("target_peak_nits(uniforms) / target_reference_white_nits(uniforms)")
        );
        assert!(VIDEO_SHADER_SOURCE.contains("return final_output(rgb, alpha, uniforms)"));
    }

    #[test]
    fn overlay_shader_linearizes_the_ui_for_extended_linear_output() {
        // The EDR/extended-linear drawable holds linear light, so the overlay
        // and danmaku passes must not composite the sRGB-encoded UI color
        // directly (ui_nits.y carries the scene-linear reference white).
        assert!(VIDEO_SHADER_SOURCE.contains("if (ui_nits.y > 0.0)"));
        assert!(
            VIDEO_SHADER_SOURCE
                .contains("float3 linear_rgb = pow(max(rgb, float3(0.0)), float3(2.2))")
        );
        assert!(VIDEO_SHADER_SOURCE.contains("max(ui_nits.x, 1.0) / max(ui_nits.y, 1.0)"));
    }

    #[test]
    fn gamut_lut_shader_samples_the_target_black_to_peak_axis() {
        // libplacebo's LUT I axis is [target black, target peak]; sampling
        // `ipt.x / peak` again would diverge from the generated LUT. The
        // lattice position must also be remapped to the texel-center
        // coordinate (libplacebo's `texel_scale`), or the zero-chroma texel at
        // the low end of the C axis crushes saturation.
        assert!(VIDEO_SHADER_SOURCE.contains("float lut_min = pq_code(uniforms.tone_map_extra.z)"));
        assert!(VIDEO_SHADER_SOURCE.contains("clamp((ipt.x - lut_min) / lut_range, 0.0, 1.0)"));
        assert!(VIDEO_SHADER_SOURCE.contains("pos.y * (31.0 / 32.0) + 0.5 / 32.0"));
        assert!(VIDEO_SHADER_SOURCE.contains("pos.x * (47.0 / 48.0) + 0.5 / 48.0"));
    }

    #[test]
    fn ui_output_nits_flags_only_extended_linear_output() {
        let sdr = TargetColorState::sdr(ColorPrimaries::Bt709);
        assert_eq!(ui_output_nits(sdr, false), [100.0, 0.0, 0.0, 0.0]);

        let hdr10 = TargetColorState::hdr10(ColorPrimaries::Bt2020);
        assert_eq!(ui_output_nits(hdr10, false), [203.0, 0.0, 0.0, 0.0]);

        // `metal_target_color` shapes a non-PQ EDR target with a 100-nit
        // reference white; the UI is then linearized against it.
        let edr = TargetColorState {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            peak_nits: 400.0,
            reference_white_nits: 100.0,
            edr_headroom: 4.0,
        };
        assert_eq!(ui_output_nits(edr, true), [100.0, 100.0, 0.0, 0.0]);
        assert_eq!(ui_output_nits(edr, false), [100.0, 0.0, 0.0, 0.0]);

        // An explicit EDR request clamped to headroom 1.0 still renders the
        // linear drawable, so the flag must come from the output mode rather
        // than from the headroom.
        let edr_unit = TargetColorState {
            edr_headroom: 1.0,
            peak_nits: 100.0,
            ..edr
        };
        assert_eq!(ui_output_nits(edr_unit, true), [100.0, 100.0, 0.0, 0.0]);

        // A PQ EDR target encodes the UI inside the shader (branch on
        // target_transfer), so it must not also set the linear flag.
        let edr_pq = TargetColorState {
            primaries: ColorPrimaries::Bt2020,
            transfer: TransferFunction::Pq,
            peak_nits: 10_000.0,
            reference_white_nits: 203.0,
            edr_headroom: 4.0,
        };
        assert_eq!(ui_output_nits(edr_pq, true), [203.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn gamut_lut_key_tracks_only_static_pipeline_state() {
        // Regression guard for the placeholder-LUT black frames: the cache may
        // only be reused when the key equals the current frame's key, so the
        // key must change for anything that changes the LUT (target black and
        // peak, source/target primaries) and must not change for per-frame
        // content brightness.
        use super::{GamutLutKey, quantize_luma_pq};
        use crate::renderer::pipeline::{SourceColorState, ToneMapConfig, VideoRenderPipeline};

        let source = |peak_nits: f32| {
            SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
                .nominal_peak_nits(peak_nits)
        };
        let sdr_target = TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709);
        let key = |source: SourceColorState, target: TargetColorState| {
            GamutLutKey::for_pipeline(&VideoRenderPipeline::new(source, target))
        };

        let base = key(source(1000.0), sdr_target);
        // Dolby Vision L1 / measured brightness move `nominal_peak_nits` every
        // frame; the LUT must not be regenerated for them.
        assert_eq!(
            base,
            key(source(4000.0), sdr_target),
            "a per-frame source peak must not invalidate the LUT"
        );
        // A different target peak (EDR headroom) changes the LUT's I axis.
        let edr_target = TargetColorState {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            peak_nits: 400.0,
            reference_white_nits: 100.0,
            edr_headroom: 4.0,
        };
        assert_ne!(
            base,
            key(source(1000.0), edr_target),
            "a stale cache key must never match the current frame"
        );
        // So does the target black (`contrast_ratio`): 1000:1 vs 10000:1 on the
        // same 203-nit target must not reuse the LUT.
        let contrast_source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let target_black = |contrast_ratio: f32| VideoRenderPipeline {
            tone_map: ToneMapConfig {
                contrast_ratio,
                ..ToneMapConfig::default()
            },
            ..VideoRenderPipeline::new(contrast_source, sdr_target)
        };
        assert_ne!(
            GamutLutKey::for_pipeline(&target_black(0.0)),
            GamutLutKey::for_pipeline(&target_black(10_000.0)),
            "a different target black changes the LUT's I axis"
        );
        // Primaries on either side change the mapping.
        assert_ne!(
            base,
            key(
                source(1000.0),
                TargetColorState::sdr_tone_map_target(ColorPrimaries::DisplayP3)
            )
        );
        assert_ne!(
            base,
            key(
                SourceColorState::new(ColorPrimaries::DisplayP3, TransferFunction::Pq),
                sdr_target
            )
        );
        // The luma quantization is monotonic and stable, and keeps sub-1-nit
        // target blacks distinguishable.
        assert_eq!(quantize_luma_pq(203.0), quantize_luma_pq(203.0));
        assert!(quantize_luma_pq(203.0) < quantize_luma_pq(400.0));
        assert!(quantize_luma_pq(0.1) < quantize_luma_pq(0.203));
        assert_eq!(quantize_luma_pq(10_000.0), 65535);
        assert_eq!(quantize_luma_pq(-1.0), 0);
    }

    #[test]
    fn video_shader_reconstructs_packed_alpha_as_premultiplied_output() {
        assert!(VIDEO_SHADER_SOURCE.contains("uniforms.video_alpha_mode == 1"));
        assert!(VIDEO_SHADER_SOURCE.contains("in.tex_coord.x * 0.5"));
        assert!(VIDEO_SHADER_SOURCE.contains("0.5 + in.tex_coord.x * 0.5"));
        assert!(VIDEO_SHADER_SOURCE.contains("premultiplied = clamp(rgb"));
        assert!(VIDEO_SHADER_SOURCE.contains("float4(premultiplied, alpha)"));
    }

    #[test]
    fn video_shader_uses_absolute_nits_for_tone_mapping() {
        assert!(VIDEO_SHADER_SOURCE.contains("float4 nits"));
        assert!(VIDEO_SHADER_SOURCE.contains("pq_absolute_peak_nits = 10000.0"));
        assert!(VIDEO_SHADER_SOURCE.contains("source_reference_to_nits"));
        assert!(VIDEO_SHADER_SOURCE.contains("tone_map_nits"));
        assert!(VIDEO_SHADER_SOURCE.contains("target_nits_to_reference_linear"));
        let decode = VIDEO_SHADER_SOURCE
            .find("rgb = transfer_to_source_reference_linear")
            .unwrap();
        let source_nits = VIDEO_SHADER_SOURCE
            .find("rgb = source_reference_to_nits")
            .unwrap();
        let tone_map = VIDEO_SHADER_SOURCE.find("rgb = tone_map_nits").unwrap();
        let target_reference = VIDEO_SHADER_SOURCE
            .find("rgb = target_nits_to_reference_linear")
            .unwrap();
        let output = VIDEO_SHADER_SOURCE
            .find("rgb = target_reference_linear_to_output")
            .unwrap();
        assert!(decode < source_nits);
        assert!(source_nits < tone_map);
        assert!(tone_map < target_reference);
        assert!(target_reference < output);
    }

    #[test]
    fn video_shader_runs_the_ipt_tone_map_before_gamut_compression() {
        assert!(VIDEO_SHADER_SOURCE.contains("gamut_matrix_rows"));
        assert!(VIDEO_SHADER_SOURCE.contains("apply_gamut_map"));
        assert!(VIDEO_SHADER_SOURCE.contains("ipt_matrix_rows"));
        assert!(VIDEO_SHADER_SOURCE.contains("st2094_pick_knee"));
        assert!(VIDEO_SHADER_SOURCE.contains("tone_map_curve_pq"));
        // The primaries conversion happens inside the tone map (IPT
        // roundtrip), so the fragment only calls tone_map_nits then the
        // compression pass.
        let tone_map = VIDEO_SHADER_SOURCE.find("rgb = tone_map_nits").unwrap();
        let compress = VIDEO_SHADER_SOURCE.find("rgb = gamut_compress").unwrap();
        assert!(tone_map < compress);
        // The separate matrix call site from the old flow is gone.
        assert!(!VIDEO_SHADER_SOURCE.contains("rgb = apply_gamut_map"));
    }

    #[test]
    fn overlay_shader_supports_libass_alpha_masks() {
        assert!(VIDEO_SHADER_SOURCE.contains("overlay_mode"));
        assert!(VIDEO_SHADER_SOURCE.contains("tex_rect"));
        assert!(VIDEO_SHADER_SOURCE.contains("constant OverlayUniforms& uniforms [[buffer(0)]]"));
        assert!(VIDEO_SHADER_SOURCE.contains(
            "out.tex_coord = uniforms.tex_rect.xy + tex_coords[vertex_id] * uniforms.tex_rect.zw"
        ));
        assert!(VIDEO_SHADER_SOURCE.contains("uniforms.color.a * sampled.r"));
    }

    #[test]
    fn overlay_uniforms_keep_color_aligned() {
        assert_eq!(std::mem::size_of::<super::OverlayUniforms>(), 80);
        assert_eq!(std::mem::offset_of!(super::OverlayUniforms, tex_rect), 16);
        assert_eq!(
            std::mem::offset_of!(super::OverlayUniforms, overlay_mode),
            40
        );
        assert_eq!(
            std::mem::offset_of!(super::OverlayUniforms, target_transfer),
            44
        );
        assert_eq!(std::mem::offset_of!(super::OverlayUniforms, color), 48);
        assert_eq!(std::mem::offset_of!(super::OverlayUniforms, ui_nits), 64);
    }

    #[test]
    fn danmaku_uniforms_keep_ui_output_fields_aligned() {
        assert_eq!(std::mem::size_of::<super::DanmakuBatchUniforms>(), 32);
        assert_eq!(
            std::mem::offset_of!(super::DanmakuBatchUniforms, target_transfer),
            8
        );
        assert_eq!(
            std::mem::offset_of!(super::DanmakuBatchUniforms, ui_nits),
            16
        );
    }

    #[test]
    fn danmaku_batch_instances_keep_each_item_effects_below_its_fill() {
        fn glyph(
            item_id: u64,
            fill_marker: f32,
            outline_marker: f32,
            shadow_marker: f32,
        ) -> DanmakuGlyphInstance {
            DanmakuGlyphInstance {
                item_id,
                rect: [0.0, 0.0, 10.0, 10.0],
                tex_rect: [0.0, 0.0, 1.0, 1.0],
                color_rgba: [fill_marker, 0.0, 0.0, 1.0],
                outline_rgba: [outline_marker, 0.0, 0.0, 1.0],
                shadow_rgba: [shadow_marker, 0.0, 0.0, 1.0],
                shadow_offset: [1.0, 1.0],
            }
        }

        let items = [
            glyph(1, 0.13, 0.12, 0.11),
            glyph(1, 0.23, 0.22, 0.21),
            glyph(2, 0.33, 0.32, 0.31),
        ];
        let mut order = Vec::new();
        let count = for_each_ordered_danmaku_instance(&items, |instance| {
            order.push((instance.atlas_texture, instance.color[0]));
        });

        assert_eq!(count, 9);
        assert_eq!(
            order,
            vec![
                (DANMAKU_OUTLINE_ATLAS_TEXTURE, 0.11),
                (DANMAKU_OUTLINE_ATLAS_TEXTURE, 0.21),
                (DANMAKU_OUTLINE_ATLAS_TEXTURE, 0.12),
                (DANMAKU_OUTLINE_ATLAS_TEXTURE, 0.22),
                (DANMAKU_FILL_ATLAS_TEXTURE, 0.13),
                (DANMAKU_FILL_ATLAS_TEXTURE, 0.23),
                (DANMAKU_OUTLINE_ATLAS_TEXTURE, 0.31),
                (DANMAKU_OUTLINE_ATLAS_TEXTURE, 0.32),
                (DANMAKU_FILL_ATLAS_TEXTURE, 0.33),
            ]
        );
    }

    #[test]
    fn danmaku_batch_instance_and_shader_texture_selector_stay_aligned() {
        assert_eq!(std::mem::size_of::<DanmakuBatchInstance>(), 64);
        assert_eq!(
            std::mem::offset_of!(DanmakuBatchInstance, atlas_texture),
            48
        );
        assert!(VIDEO_SHADER_SOURCE.contains("uint atlas_texture;"));
        assert!(VIDEO_SHADER_SOURCE.contains("fill_atlas [[texture(0)]]"));
        assert!(VIDEO_SHADER_SOURCE.contains("outline_atlas [[texture(1)]]"));
        assert!(VIDEO_SHADER_SOURCE.contains("out.atlas_texture = glyph.atlas_texture;"));
    }

    #[test]
    fn danmaku_batch_shader_compiles_with_both_atlas_textures() {
        // Needs a real Metal device; skip rather than fail where there is none.
        let Ok(mut renderer) =
            super::MetalRendererImpl::new(crate::renderer::metal::MetalRendererConfig::default())
        else {
            eprintln!("skipping: no Metal device available");
            return;
        };
        renderer
            .danmaku_batch_pipeline_state()
            .expect("dual-atlas danmaku pipeline");
    }

    #[test]
    fn reported_display_headroom_drives_output_mode_negotiation() {
        // The renderer negotiates on the render thread, where AppKit must not
        // be touched, so it consumes whatever the host published from the main
        // thread. A missing report negotiates as SDR, exactly like the AppKit
        // fallback it replaced.
        let config = crate::renderer::metal::MetalRendererConfig {
            output_mode: crate::renderer::metal::MetalOutputMode::auto(1.0),
            ..crate::renderer::metal::MetalRendererConfig::default()
        };
        // Needs a real Metal device; skip rather than fail where there is none.
        let Ok(mut renderer) = super::MetalRendererImpl::new(config) else {
            eprintln!("skipping: no Metal device available");
            return;
        };
        let hdr = crate::renderer::pipeline::SourceColorState::new(
            ColorPrimaries::Bt2020,
            TransferFunction::Pq,
        );

        assert_eq!(renderer.display_edr_headroom(), 1.0);
        renderer.select_output_mode_for_source(hdr);
        assert!(
            !renderer.active_output_mode().is_edr(),
            "an unreported display must not promote to EDR"
        );

        renderer.set_output_headroom(4.0, true);
        assert_eq!(renderer.display_edr_headroom(), 4.0);
        renderer.select_output_mode_for_source(hdr);
        assert_eq!(
            renderer.active_output_mode(),
            crate::renderer::metal::MetalOutputMode::apple_edr(4.0)
        );

        // An unreported value clears the cache and returns to SDR.
        renderer.set_output_headroom(4.0, false);
        assert_eq!(renderer.display_edr_headroom(), 1.0);
        renderer.select_output_mode_for_source(hdr);
        assert!(!renderer.active_output_mode().is_edr());
    }

    #[test]
    fn video_shader_compiles() {
        // The video shader is the one carrying the tone-map curves, and it is
        // only compiled at runtime, so a syntax error there would reach users.
        // Needs a real Metal device; skip rather than fail where there is none.
        let Ok(mut renderer) =
            super::MetalRendererImpl::new(crate::renderer::metal::MetalRendererConfig::default())
        else {
            eprintln!("skipping: no Metal device available");
            return;
        };
        renderer.video_pipeline_state().expect("video pipeline");
    }

    #[test]
    fn video_shader_evaluates_reinhard_and_mobius_in_linear_nits() {
        // libplacebo runs these two curves in the linear PL_HDR_NORM domain
        // and only the PQ-domain operators work on encoded values; evaluating
        // them on PQ codes moved the Mobius knee far below its intended
        // position and produced the mid-tone darkening the PR review measured.
        for branch in ["uniforms.tone_map == 1", "uniforms.tone_map == 2"] {
            let start = VIDEO_SHADER_SOURCE
                .find(branch)
                .unwrap_or_else(|| panic!("{branch} branch"));
            let rest = &VIDEO_SHADER_SOURCE[start..];
            let end = rest.find("\n    }").expect("branch end");
            let body = &rest[..end];
            assert!(
                body.contains("float out_range_nits = max(dst_peak - dst_black, 0.000001);"),
                "{branch} must scale in linear nits"
            );
            assert!(
                body.contains("nits_from_pq(x)"),
                "{branch} must decode the PQ code before the curve"
            );
            assert!(
                body.contains("return pq_code("),
                "{branch} must re-encode the result"
            );
        }
    }

    #[test]
    fn overlay_uniforms_decode_libass_color() {
        let bitmap = crate::subtitle::SubtitleAlphaBitmap::new(
            crate::subtitle::SubtitleBitmapPlacement::new(12, 34, 56, 78),
            56,
            0x8040207f,
            vec![255; 56 * 78],
        );
        let placement = super::OverlayAlphaAtlasPlacement {
            bitmap_index: 0,
            x: 10,
            y: 5,
        };
        let layout = super::VideoPresentationLayout::aspect_fit(640, 360, 640, 360);

        let uniforms = super::OverlayUniforms::from_alpha_atlas_bitmap(
            &bitmap,
            &placement,
            200,
            100,
            layout,
            crate::renderer::pipeline::TargetColorState::default(),
            false,
        );

        assert_eq!(uniforms.rect, [12.0, 34.0, 56.0, 78.0]);
        assert_eq!(uniforms.tex_rect, [0.05, 0.05, 0.28, 0.78]);
        assert_eq!(uniforms.overlay_mode, 1);
        assert!((uniforms.color[0] - (128.0 / 255.0)).abs() < 0.0001);
        assert!((uniforms.color[1] - (64.0 / 255.0)).abs() < 0.0001);
        assert!((uniforms.color[2] - (32.0 / 255.0)).abs() < 0.0001);
        assert!((uniforms.color[3] - (128.0 / 255.0)).abs() < 0.0001);
    }

    #[test]
    fn overlay_alpha_atlas_packs_masks_in_one_r8_image() {
        let first = crate::subtitle::SubtitleAlphaBitmap::new(
            crate::subtitle::SubtitleBitmapPlacement::new(0, 0, 2, 2),
            3,
            0xffffff00,
            vec![1, 2, 99, 3, 4],
        );
        let second = crate::subtitle::SubtitleAlphaBitmap::new(
            crate::subtitle::SubtitleBitmapPlacement::new(8, 8, 1, 3),
            1,
            0xff000000,
            vec![5, 6, 7],
        );

        let atlas = super::OverlayAlphaAtlasPlan::pack(&[first, second])
            .unwrap()
            .unwrap();

        assert_eq!(atlas.width, 3);
        assert_eq!(atlas.height, 3);
        assert_eq!(atlas.stride, 3);
        assert_eq!(atlas.pixels, vec![1, 2, 5, 3, 4, 6, 0, 0, 7]);
        assert_eq!(atlas.placements.len(), 2);
        assert_eq!(atlas.placements[0].bitmap_index, 0);
        assert_eq!(atlas.placements[0].x, 0);
        assert_eq!(atlas.placements[1].bitmap_index, 1);
        assert_eq!(atlas.placements[1].x, 2);
    }

    #[test]
    fn overlay_alpha_atlas_signature_tracks_reusable_layout() {
        let first = crate::subtitle::SubtitleAlphaBitmap::new(
            crate::subtitle::SubtitleBitmapPlacement::new(0, 0, 2, 2),
            2,
            0xffffff00,
            vec![1, 2, 3, 4],
        );
        let same_bitmap = crate::subtitle::SubtitleAlphaBitmap::new(
            crate::subtitle::SubtitleBitmapPlacement::new(0, 0, 2, 2),
            2,
            0xffffff00,
            vec![1, 2, 3, 4],
        );
        let moved = crate::subtitle::SubtitleAlphaBitmap::new(
            crate::subtitle::SubtitleBitmapPlacement::new(1, 0, 2, 2),
            2,
            0xffffff00,
            vec![1, 2, 3, 4],
        );

        let signature = super::OverlayAlphaAtlasSignature::from_bitmaps(&[first]);

        assert_eq!(
            signature,
            super::OverlayAlphaAtlasSignature::from_bitmaps(&[same_bitmap])
        );
        assert_ne!(
            signature,
            super::OverlayAlphaAtlasSignature::from_bitmaps(&[moved])
        );
    }

    #[test]
    fn overlay_alpha_atlas_signature_tracks_alpha_content() {
        let first = crate::subtitle::SubtitleAlphaBitmap::new(
            crate::subtitle::SubtitleBitmapPlacement::new(0, 0, 2, 2),
            2,
            0xffffff00,
            vec![1, 2, 3, 4],
        );
        let changed_alpha = crate::subtitle::SubtitleAlphaBitmap::new(
            crate::subtitle::SubtitleBitmapPlacement::new(0, 0, 2, 2),
            2,
            0xffffff00,
            vec![1, 2, 3, 5],
        );

        assert_ne!(
            super::OverlayAlphaAtlasSignature::from_bitmaps(&[first]),
            super::OverlayAlphaAtlasSignature::from_bitmaps(&[changed_alpha])
        );
    }

    #[test]
    fn video_uniforms_keep_float4_fields_aligned() {
        assert_eq!(std::mem::size_of::<super::VideoUniforms>(), 3296);
        assert_eq!(std::mem::offset_of!(super::VideoUniforms, edr_output), 20);
        assert_eq!(std::mem::offset_of!(super::VideoUniforms, rect), 32);
        assert_eq!(std::mem::offset_of!(super::VideoUniforms, viewport), 48);
        assert_eq!(std::mem::offset_of!(super::VideoUniforms, nits), 64);
        assert_eq!(
            std::mem::offset_of!(super::VideoUniforms, luma_coefficients),
            80
        );
        assert_eq!(
            std::mem::offset_of!(super::VideoUniforms, gamut_matrix_rows),
            96
        );
        assert_eq!(
            std::mem::offset_of!(super::VideoUniforms, ipt_matrix_rows),
            144
        );
        assert_eq!(
            std::mem::offset_of!(super::VideoUniforms, tone_map_extra),
            288
        );
        assert_eq!(
            std::mem::offset_of!(super::VideoUniforms, tone_map_coeffs),
            304
        );
        assert_eq!(
            std::mem::offset_of!(super::VideoUniforms, gamut_lut_enabled),
            320
        );
        assert_eq!(std::mem::offset_of!(super::VideoUniforms, dovi), 336);
    }

    #[test]
    fn presentation_layout_preserves_source_aspect_ratio() {
        fn assert_rect_close(actual: [f32; 4], expected: [f32; 4]) {
            for (actual, expected) in actual.into_iter().zip(expected) {
                assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
            }
        }

        assert_rect_close(
            super::VideoPresentationLayout::aspect_fit(1920, 1080, 1000, 1000).target_rect,
            [0.0, 218.75, 1000.0, 562.5],
        );
        assert_rect_close(
            super::VideoPresentationLayout::aspect_fit(1920, 1080, 2000, 1000).target_rect,
            [111.111, 0.0, 1777.778, 1000.0],
        );
    }

    #[test]
    fn overlay_uniforms_map_source_rect_into_presentation_layout() {
        let layout = super::VideoPresentationLayout::aspect_fit(1920, 1080, 1000, 1000);
        let uniforms = super::OverlayUniforms::from_plane(
            960,
            540,
            192,
            108,
            layout,
            crate::renderer::pipeline::TargetColorState::default(),
            false,
        );

        assert_eq!(uniforms.viewport, [1000.0, 1000.0]);
        for (actual, expected) in uniforms.rect.into_iter().zip([500.0, 500.0, 100.0, 56.25]) {
            assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
        }
    }

    #[test]
    fn drawable_pixel_formats_map_to_metal_pipeline_formats() {
        assert_eq!(
            metal_pixel_format(MetalDrawablePixelFormat::Bgra8Unorm),
            MTLPixelFormat::BGRA8Unorm
        );
        assert_eq!(
            metal_pixel_format(MetalDrawablePixelFormat::Rgba16Float),
            MTLPixelFormat::RGBA16Float
        );
    }
}
