#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
use std::ffi::c_void;
#[cfg(target_os = "windows")]
use std::num::NonZeroIsize;
#[cfg(any(target_os = "android", target_os = "linux", target_env = "ohos"))]
use std::ptr::NonNull;
#[cfg(target_os = "android")]
use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
};
use wgpu::util::DeviceExt;

#[cfg(target_os = "android")]
use crate::android::{AndroidDataSpaceErrorKind, AndroidNativeWindow};
use crate::core::ColorPrimaries;
#[cfg(any(
    target_os = "android",
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "linux",
    target_env = "ohos"
))]
use crate::core::WgpuSurfaceKind;
use crate::core::{
    LumaUpscalerBackendStatus, PlatformSurface, PlayerError, PlayerVideoFrame, RenderFrameContext,
    RendererBackend, RendererRuntimeStats, Result, SurfaceOutputCapabilities, WgpuSurfaceHandle,
};
use crate::danmaku::{
    DanmakuAtlasUpdate, DanmakuGlyphAtlas, DanmakuGlyphInstance, DanmakuRenderPlan,
};
use crate::ffmpeg::{DecoderBackend, PlanarFrame, PlanarPixelFormat};
use crate::overlay::OverlayFrame;
#[cfg(target_os = "android")]
use crate::renderer::android_vulkan::{
    AndroidAhbConversionError, AndroidAhbCrop, AndroidAhbFrameDescription,
    AndroidAhbIntermediateFormat, AndroidVulkanInterop, retire_ahb_conversion_after_submission,
};
use crate::renderer::gamut::{
    GamutLut, GamutLutJob, GamutLutParams, LUT_SIZE_C, LUT_SIZE_H, LUT_SIZE_I, pack_rgba16f,
};
use crate::renderer::metal::{MetalRendererConfig, VideoAlphaMode};
#[cfg(target_env = "ohos")]
use crate::renderer::ohos_vulkan::{
    OhosNativeBufferConversionError, OhosNativeBufferCrop, OhosNativeBufferFrameDescription,
    OhosVulkanInterop, retire_ohb_conversion_after_submission,
};
use crate::renderer::output::{
    ActiveOutputEncoding, OutputDescription, OutputFallbackReason, OutputMode, OutputRuntimeStatus,
    OutputSurfaceFormat,
};
use crate::renderer::pipeline::{LumaUpscalerMode, SourceColorState, VideoRenderPipeline};
use crate::renderer::presentation::{PresentationLayout, PresentationRect};
use crate::renderer::wgpu_artcnn::{
    WgpuArtCnn, WgpuArtCnnInput, WgpuArtCnnInputKind, WgpuArtCnnStatus,
};
use crate::subtitle::AssColor;

pub use crate::renderer::pipeline::VideoUniforms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WgpuRendererStats {
    pub surface_width: u32,
    pub surface_height: u32,
    pub rendered_frames: u64,
    pub offscreen_frames: u64,
    pub software_video_frames: u64,
    pub hardware_video_frames: u64,
    pub zero_copy_video_frames: u64,
    pub direct_zero_copy_video_frames: u64,
    pub shared_handle_video_frames: u64,
    pub cpu_video_frame_fallbacks: u64,
    pub hdr_source_frames: u64,
    pub sdr_tonemap_frames: u64,
    pub danmaku_passes: u64,
    pub danmaku_items: u64,
    pub attached: bool,
}

#[cfg(target_os = "android")]
#[derive(Debug, Clone)]
struct AndroidWgpuDeviceFailure {
    kind: &'static str,
    reason: String,
}

#[cfg(target_os = "android")]
#[derive(Default)]
struct AndroidWgpuDeviceHealth {
    failure: Mutex<Option<AndroidWgpuDeviceFailure>>,
}

#[cfg(target_os = "android")]
impl AndroidWgpuDeviceHealth {
    fn record(&self, kind: &'static str, reason: String, replace_existing: bool) {
        let mut failure = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if replace_existing || failure.is_none() {
            *failure = Some(AndroidWgpuDeviceFailure { kind, reason });
        }
    }

    fn failure(&self) -> Option<AndroidWgpuDeviceFailure> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A clear color in the renderer's working space, components in `[0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WgpuClearColor {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
    pub alpha: f64,
}

impl WgpuClearColor {
    pub fn new(red: f64, green: f64, blue: f64, alpha: f64) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    /// An animated test pattern, matching the Metal renderer's `ClearColor::animated`
    /// so the two backends can be compared frame-for-frame.
    pub fn animated(time_seconds: f64) -> Self {
        Self {
            red: time_seconds.sin() * 0.5 + 0.5,
            green: (time_seconds * 0.73).sin() * 0.5 + 0.5,
            blue: (time_seconds * 1.37).cos() * 0.5 + 0.5,
            alpha: 1.0,
        }
    }

    fn to_wgpu(self) -> wgpu::Color {
        wgpu::Color {
            r: self.red,
            g: self.green,
            b: self.blue,
            a: self.alpha,
        }
    }
}

/// Tightly packed RGBA8 pixels read back from an offscreen render target.
///
/// Used as the headless verification oracle for the wgpu backend: render a pass,
/// copy the target to host memory, and assert the pixels are what we expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WgpuOffscreenReadback {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl WgpuOffscreenReadback {
    /// Returns the RGBA bytes of the pixel at `(x, y)`.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let offset = (y as usize * self.width as usize + x as usize) * 4;
        [
            self.rgba[offset],
            self.rgba[offset + 1],
            self.rgba[offset + 2],
            self.rgba[offset + 3],
        ]
    }
}

fn overlay_has_planes(frame: &OverlayFrame) -> bool {
    !frame.subtitle_planes.is_empty() || !frame.subtitle_alpha_planes.is_empty()
}

/// Overlay quad uniforms, byte-compatible with the Metal `OverlayUniforms`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct OverlayUniforms {
    pub rect: [f32; 4],
    pub tex_rect: [f32; 4],
    pub viewport: [f32; 2],
    pub overlay_mode: u32,
    pub output_encoding: u32,
    pub color: [f32; 4],
}

impl OverlayUniforms {
    /// A straight-RGBA subtitle plane placed at pixel `rect` within the viewport.
    fn rgba_plane(
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        viewport_w: u32,
        viewport_h: u32,
    ) -> Self {
        Self {
            rect: [x as f32, y as f32, width as f32, height as f32],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            viewport: [viewport_w.max(1) as f32, viewport_h.max(1) as f32],
            overlay_mode: 0,
            output_encoding: 0,
            color: [1.0, 1.0, 1.0, 1.0],
        }
    }

    /// A libass alpha coverage bitmap sampled from a horizontal R8 atlas at `atlas_x`,
    /// tinted by `color_rgba` (mode 1). Mirrors the Metal `from_alpha_atlas_bitmap`.
    #[allow(clippy::too_many_arguments)]
    fn alpha_atlas(
        color_rgba: u32,
        place_x: i32,
        place_y: i32,
        place_w: u32,
        place_h: u32,
        atlas_x: u32,
        atlas_w: u32,
        atlas_h: u32,
        viewport_w: u32,
        viewport_h: u32,
    ) -> Self {
        let color = AssColor::from_libass_rgba(color_rgba);
        let aw = atlas_w.max(1) as f32;
        let ah = atlas_h.max(1) as f32;
        Self {
            rect: [
                place_x as f32,
                place_y as f32,
                place_w as f32,
                place_h as f32,
            ],
            tex_rect: [
                atlas_x as f32 / aw,
                0.0,
                place_w as f32 / aw,
                place_h as f32 / ah,
            ],
            viewport: [viewport_w.max(1) as f32, viewport_h.max(1) as f32],
            overlay_mode: 1,
            output_encoding: 0,
            color: [
                f32::from(color.red) / 255.0,
                f32::from(color.green) / 255.0,
                f32::from(color.blue) / 255.0,
                f32::from(color.alpha) / 255.0,
            ],
        }
    }

    fn alpha_atlas_rect(
        color: [f32; 4],
        rect: [f32; 4],
        tex_rect: [f32; 4],
        viewport_w: u32,
        viewport_h: u32,
    ) -> Self {
        Self {
            rect,
            tex_rect,
            viewport: [viewport_w.max(1) as f32, viewport_h.max(1) as f32],
            overlay_mode: 1,
            output_encoding: 0,
            color,
        }
    }

    fn for_output(mut self, output: OutputDescription) -> Self {
        self.output_encoding = u32::from(output.extended_linear);
        self
    }
}

/// Decode the packed (source << 8 | target) primaries codes from
/// `VideoUniforms::_gamut_reserved`, used to key the perceptual LUT cache.
fn gamut_lut_key_of(uniforms: VideoUniforms) -> Option<(ColorPrimaries, ColorPrimaries, u32, u32)> {
    if uniforms.gamut_lut_enabled == 0 {
        return None;
    }
    let packed = uniforms._gamut_primaries;
    let source = match packed >> 8 {
        1 => ColorPrimaries::Bt2020,
        2 => ColorPrimaries::DisplayP3,
        _ => ColorPrimaries::Bt709,
    };
    let target = match packed & 0xff {
        1 => ColorPrimaries::Bt2020,
        2 => ColorPrimaries::DisplayP3,
        _ => ColorPrimaries::Bt709,
    };
    // The LUT's I axis spans [target black, target peak] in PQ codes; any
    // change to either endpoint changes the sampled range, so include both
    // (`tone_map_extra.z` is the target black in nits).
    let target_black_pq = quantize_luma_pq(uniforms.tone_map_extra[2]);
    let target_peak_pq = quantize_luma_pq(uniforms.nits[1]);
    Some((source, target, target_black_pq, target_peak_pq))
}

/// Quantize a luminance (nits) to its PQ code for the LUT cache key. The key
/// only has to change when the LUT's I axis changes, so 16-bit PQ resolution
/// is ample (and keeps the sub-1-nit target blacks of SDR targets distinct).
fn quantize_luma_pq(nits: f32) -> u32 {
    (pq_code_for_lut(nits) * 65535.0) as u32
}

/// Lazily-built GPU objects for the NV12/P010 video pipeline, tied to the color
/// target format the pipeline was compiled for.
struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    format: wgpu::TextureFormat,
}

/// Lazily-built GPU objects for the overlay (subtitle/danmaku) compositing pass,
/// tied to the color target format it was compiled for.
struct OverlayPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    format: wgpu::TextureFormat,
}

/// Per-plane GPU resources for one overlay draw. The texture and uniform buffer are
/// retained so the bind group stays valid for the duration of the render pass.
struct OverlayDraw {
    bind_group: wgpu::BindGroup,
    dynamic_offset: u32,
    _texture: wgpu::Texture,
    _uniform: wgpu::Buffer,
}

#[derive(Clone, Copy)]
enum DanmakuAtlasTexture {
    Fill,
    Outline,
}

struct WgpuDanmakuAtlasCache {
    version: u64,
    width: u32,
    height: u32,
    stride: usize,
    fill_texture: wgpu::Texture,
    outline_texture: wgpu::Texture,
}

impl WgpuDanmakuAtlasCache {
    fn can_reuse_for(&self, atlas: &DanmakuGlyphAtlas) -> bool {
        self.version == atlas.version
            && self.width == atlas.width
            && self.height == atlas.height
            && self.stride == atlas.stride
    }
}

enum UploadedVideoTextures {
    Planar {
        luma: wgpu::Texture,
        chroma: wgpu::Texture,
    },
    Rgb {
        texture: wgpu::Texture,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanarUploadPath {
    Native,
    GpuP010Bytes,
}

struct PreparedPlanarUpload {
    frame: PlanarFrame,
    uniforms: VideoUniforms,
    path: PlanarUploadPath,
}

fn prepare_planar_upload(
    frame: PlanarFrame,
    mut uniforms: VideoUniforms,
    supports_16bit_norm: bool,
) -> PreparedPlanarUpload {
    let path = if frame.format == PlanarPixelFormat::P010 && !supports_16bit_norm {
        PlanarUploadPath::GpuP010Bytes
    } else {
        PlanarUploadPath::Native
    };
    uniforms = uniforms
        .with_p010_representation(frame.format == PlanarPixelFormat::P010)
        .p010_byte_planes(path == PlanarUploadPath::GpuP010Bytes);
    PreparedPlanarUpload {
        frame,
        uniforms,
        path,
    }
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

fn source_color_for_player_frame(frame: &PlayerVideoFrame) -> SourceColorState {
    SourceColorState::new(
        frame.frame.color_primaries(),
        frame.frame.transfer_function(),
    )
    .range(frame.frame.color_range())
    .matrix(frame.frame.matrix_coefficients())
    .hdr_metadata(frame.frame.hdr_metadata())
    .dovi(frame.frame.dovi_metadata())
    .measured_scene_avg_nits(frame.scene_avg_nits)
}

#[cfg(target_os = "android")]
fn android_ahb_intermediate_format(
    source: SourceColorState,
    interop: &AndroidVulkanInterop,
) -> AndroidAhbIntermediateFormat {
    // HDR and wide-gamut SDR retain the floating-point target so gamut conversion and tone
    // mapping can preserve values outside the normalized range. Ordinary SDR uses a packed
    // 10-bit target, halving intermediate-frame storage without reducing 10-bit decode precision.
    if source.is_hdr()
        || matches!(
            source.primaries,
            ColorPrimaries::DisplayP3 | ColorPrimaries::Bt2020
        )
        || !interop.supports_rgb10a2_intermediate()
    {
        AndroidAhbIntermediateFormat::Rgba16Float
    } else {
        AndroidAhbIntermediateFormat::Rgb10a2Unorm
    }
}

/// The currently uploaded video frame: source textures plus the common color
/// uniforms. Retained so the presenter can re-present it across vsync ticks.
struct UploadedVideoFrame {
    textures: UploadedVideoTextures,
    width: u32,
    height: u32,
    uniforms: VideoUniforms,
    source_color: Option<SourceColorState>,
    /// Monotonic renderer-local identity for the uploaded decoded frame.
    /// Presentation ticks reuse this token so an expensive GPU preprocessing
    /// pass is only encoded once per upload, independent of repeated PTS values.
    frame_token: u64,
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    _linux_direct: Option<crate::renderer::linux_vulkan::LinuxDirectFrame>,
}

impl UploadedVideoFrame {
    fn uniforms_for_output(&self, output: OutputDescription) -> VideoUniforms {
        let Some(source) = self.source_color else {
            return self.uniforms;
        };
        let pipeline = VideoRenderPipeline::new(source, output.tone_map_target_for(&source));
        let uniforms = VideoUniforms::from_pipeline(
            &pipeline,
            self.uniforms.is_p010 != 0,
            output.extended_linear,
        )
        .packed_alpha_right(self.uniforms.has_packed_alpha_right())
        .p010_byte_planes(self.uniforms.has_p010_byte_planes());
        match &self.textures {
            UploadedVideoTextures::Planar { .. } => uniforms,
            UploadedVideoTextures::Rgb { .. } => uniforms.rgb_texture_input(),
        }
    }
}

struct AttachedSurface {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    sdr_format: wgpu::TextureFormat,
    output: OutputDescription,
    fallback_reason: OutputFallbackReason,
    data_space_failure: bool,
    native_data_space: i32,
    handle: WgpuSurfaceHandle,
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    formats: Vec<wgpu::TextureFormat>,
    // Declared after `surface` so the wgpu surface is dropped before its native
    // window reference during normal field destruction.
    #[cfg(target_os = "android")]
    _android_window: Option<AndroidNativeWindow>,
}

#[derive(Debug, Clone, Copy)]
struct AttachedOutputState {
    output: OutputDescription,
    fallback_reason: OutputFallbackReason,
    data_space_failure: bool,
    native_data_space: i32,
}

impl AttachedSurface {
    fn output_state(&self) -> AttachedOutputState {
        AttachedOutputState {
            output: self.output,
            fallback_reason: self.fallback_reason,
            data_space_failure: self.data_space_failure,
            native_data_space: self.native_data_space,
        }
    }
}

enum SurfaceFrame {
    Texture {
        texture: wgpu::SurfaceTexture,
        reconfigure_after_present: bool,
    },
    Skipped,
}

pub struct WgpuRenderer {
    flutter_metrics: Option<crate::core::SurfaceMetrics>,
    flutter_frame: Option<crate::core::RendererFrameCapture>,
    _instance: wgpu::Instance,
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    linux_display: Option<(WgpuSurfaceKind, u64)>,
    // Keep one renderer-owned CPU frame so an EGL display can be released at
    // detach and recreated at attach, including while playback is paused.
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    linux_frame: Option<(PlanarFrame, VideoUniforms, Option<SourceColorState>)>,
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    linux_hardware_frame: Option<PlayerVideoFrame>,
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    linux_vulkan:
        Option<std::result::Result<crate::renderer::linux_vulkan::LinuxVulkanInterop, String>>,
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    linux_import_failure: Option<(u64, String)>,
    adapter: wgpu::Adapter,
    #[cfg(target_os = "android")]
    android_vulkan: Option<AndroidVulkanInterop>,
    #[cfg(target_os = "android")]
    android_device_health: Arc<AndroidWgpuDeviceHealth>,
    #[cfg(target_os = "android")]
    android_backend_candidate_index: usize,
    #[cfg(target_env = "ohos")]
    ohos_vulkan: Option<OhosVulkanInterop>,
    #[cfg(target_env = "ohos")]
    ohos_native_buffer_surface: Option<std::sync::Arc<crate::ohos::avcodec::OhosAvCodecSurface>>,
    #[cfg(target_env = "ohos")]
    ohos_gles: Option<crate::ohos::gles::OhosGlesInterop>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: Option<AttachedSurface>,
    video_pipeline: Option<VideoPipeline>,
    overlay_pipeline: Option<OverlayPipeline>,
    /// Perceptual gamut-mapping 3D LUT cache, keyed by the source/target
    /// primaries plus the target black/peak (PQ codes) that parametrize the
    /// LUT's I axis.
    gamut_lut: Option<(
        (ColorPrimaries, ColorPrimaries, u32, u32),
        wgpu::Texture,
        wgpu::TextureView,
    )>,
    /// Background generation for a cache miss; the fast `gamut_compress`
    /// path renders until the LUT lands.
    gamut_lut_job: Option<GamutLutJob>,
    /// 1x1x1 fallback view so binding 4 always has a valid resource.
    dummy_lut: Option<wgpu::TextureView>,
    current_video: Option<UploadedVideoFrame>,
    current_video_visible: bool,
    upload_serial: u64,
    danmaku_atlas_cache: Option<WgpuDanmakuAtlasCache>,
    supports_16bit_norm: bool,
    output_mode: OutputMode,
    video_alpha_mode: VideoAlphaMode,
    output_status: OutputRuntimeStatus,
    output_headroom: OutputHeadroomState,
    upscaler_mode: LumaUpscalerMode,
    upscaler: WgpuArtCnn,
    upscaler_failed_frame_token: Option<u64>,
    upscaler_active_frame_reported: bool,
    cpu_video_frame_fallback_reported: bool,
    p010_byte_planes_reported: bool,
    /// Rotate upload storage so writing the next frame does not overwrite a
    /// texture still being sampled by the previous GPU submission.
    planar_upload_pool: std::collections::VecDeque<(wgpu::Texture, wgpu::Texture)>,
    sdr_hdr_output_reported: bool,
    #[cfg(target_os = "android")]
    android_shared_frame_reported: bool,
    #[cfg(target_os = "android")]
    android_downscaled_frame_reported: bool,
    #[cfg(target_env = "ohos")]
    ohos_shared_frame_reported: bool,
    stats: WgpuRendererStats,
}

/// Offscreen readback targets use a linear `Rgba8Unorm` format so a clear value of
/// `c` reads back as `round(c * 255)` with no transfer-function surprises.
const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
#[cfg(target_os = "android")]
const ANDROID_WGPU_DROP_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, Clone)]
struct WgpuSurfaceOutputSelection {
    format: wgpu::TextureFormat,
    sdr_format: wgpu::TextureFormat,
    output: OutputDescription,
    fallback_reason: OutputFallbackReason,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OutputHeadroomState {
    headroom: f32,
    known: bool,
}

impl OutputHeadroomState {
    fn reported(headroom: f32, known: bool) -> Self {
        if known && headroom.is_finite() && headroom >= 1.0 {
            Self {
                headroom: headroom.min(10_000.0),
                known,
            }
        } else {
            Self::default()
        }
    }
}

impl Default for OutputHeadroomState {
    fn default() -> Self {
        Self {
            headroom: 1.0,
            known: false,
        }
    }
}

fn effective_extended_linear_headroom(
    requested: OutputMode,
    capabilities: SurfaceOutputCapabilities,
    display: OutputHeadroomState,
) -> f32 {
    let mut headroom = requested.headroom().max(1.0);
    if capabilities.desired_headroom.is_finite() && capabilities.desired_headroom >= 1.0 {
        headroom = headroom.min(capabilities.desired_headroom);
    }
    // A ratio of exactly 1 is common before the first HDR layer is visible.
    // Do not clamp the content to SDR at that point or the display may never
    // observe values above reference white and grant additional headroom.
    if display.known && display.headroom > 1.0 {
        headroom = headroom.min(display.headroom);
    }
    headroom.max(1.0)
}

fn preferred_sdr_surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureFormat::Bgra8UnormSrgb,
        wgpu::TextureFormat::Rgba8UnormSrgb,
    ]
    .into_iter()
    .find(|candidate| formats.contains(candidate))
    .or_else(|| {
        formats
            .iter()
            .copied()
            .find(|format| *format != wgpu::TextureFormat::Rgba16Float)
    })
}

fn select_wgpu_surface_output(
    requested: OutputMode,
    capabilities: SurfaceOutputCapabilities,
    backend: wgpu::Backend,
    formats: &[wgpu::TextureFormat],
) -> Option<WgpuSurfaceOutputSelection> {
    let sdr_format = preferred_sdr_surface_format(formats)?;
    let fallback_reason = match requested {
        OutputMode::Sdr | OutputMode::Auto { .. } => OutputFallbackReason::None,
        OutputMode::AppleEdr { .. } => OutputFallbackReason::LegacyAppleEdrUnsupported,
        OutputMode::ExtendedLinear { .. } if !capabilities.extended_linear => {
            if capabilities.fallback_reason == OutputFallbackReason::None {
                OutputFallbackReason::DisplayHdrUnsupported
            } else {
                capabilities.fallback_reason
            }
        }
        OutputMode::ExtendedLinear { .. } if !capabilities.direct_composition => {
            OutputFallbackReason::HybridCompositionRequired
        }
        OutputMode::ExtendedLinear { .. } if backend != wgpu::Backend::Vulkan => {
            OutputFallbackReason::WgpuBackendNotVulkan
        }
        OutputMode::ExtendedLinear { .. }
            if !formats.contains(&wgpu::TextureFormat::Rgba16Float) =>
        {
            OutputFallbackReason::Rgba16FloatSurfaceFormatUnavailable
        }
        OutputMode::ExtendedLinear { headroom } => {
            return Some(WgpuSurfaceOutputSelection {
                format: wgpu::TextureFormat::Rgba16Float,
                sdr_format,
                output: OutputDescription::extended_linear(headroom),
                fallback_reason: OutputFallbackReason::None,
            });
        }
    };

    Some(WgpuSurfaceOutputSelection {
        format: sdr_format,
        sdr_format,
        output: OutputDescription::sdr(),
        fallback_reason,
    })
}

// wgpu's Vulkan backend only exposes Rgba16Float when the WSI advertises
// R16G16B16A16_SFLOAT + EXTENDED_SRGB_LINEAR_EXT together. A 10-bit SDR
// format alone is deliberately insufficient evidence for HDR output.
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
fn select_linux_surface_output(
    requested: OutputMode,
    source_hdr: bool,
    backend: wgpu::Backend,
    formats: &[wgpu::TextureFormat],
) -> Option<WgpuSurfaceOutputSelection> {
    let resolved = match requested {
        OutputMode::Auto { headroom } if source_hdr => {
            OutputMode::extended_linear(if headroom > 1.0 { headroom } else { 12.5 })
        }
        OutputMode::Auto { .. } => OutputMode::Sdr,
        OutputMode::AppleEdr { headroom } => OutputMode::extended_linear(headroom),
        other => other,
    };
    select_wgpu_surface_output(
        resolved,
        SurfaceOutputCapabilities {
            extended_linear: backend == wgpu::Backend::Vulkan
                && formats.contains(&wgpu::TextureFormat::Rgba16Float),
            direct_composition: true,
            desired_headroom: resolved.headroom(),
            ..SurfaceOutputCapabilities::default()
        },
        backend,
        formats,
    )
}

fn requested_device_limits(adapter_limits: wgpu::Limits, backend: wgpu::Backend) -> wgpu::Limits {
    // liberika only needs the portable binding-count and buffer-size baseline, but
    // video and swapchain textures must retain the adapter's real resolution.
    // This also keeps Android software Vulkan implementations usable: SwiftShader
    // exposes the Vulkan minimum 16 KiB uniform-buffer binding limit, while
    // `Limits::default()` asks for 64 KiB and makes `request_device` fail.
    // GLES 3.0 adapters legitimately expose no compute workgroups. Request the
    // WebGL2/GLES baseline there instead of accidentally asking for the GLES 3.1
    // compute minima and rejecting an otherwise valid presentation backend.
    let baseline = if backend == wgpu::Backend::Gl {
        wgpu::Limits::downlevel_webgl2_defaults()
    } else {
        wgpu::Limits::downlevel_defaults()
    };
    baseline
        .using_resolution(adapter_limits.clone())
        .using_alignment(adapter_limits)
}

fn wgpu_instance_flags() -> wgpu::InstanceFlags {
    let flags = wgpu::InstanceFlags::from_build_config().with_env();
    #[cfg(target_os = "android")]
    {
        // Android Emulator's ranchu Vulkan driver can dereference a null
        // debug-utils object while assigning HAL labels. Keep wgpu validation
        // enabled, but do not forward object labels into that vendor path.
        return flags | wgpu::InstanceFlags::DISCARD_HAL_LABELS;
    }
    #[cfg(not(target_os = "android"))]
    {
        flags
    }
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
fn linux_adapter_is_hardware(device_type: wgpu::DeviceType, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    device_type != wgpu::DeviceType::Cpu
        && ![
            "llvmpipe",
            "softpipe",
            "lavapipe",
            "swiftshader",
            "microsoft basic render",
        ]
        .iter()
        .any(|software| name.contains(software))
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
fn retain_wsl_d3d_runtime() {
    // WSL's D3D libraries register thread-local destructors used after EGL or
    // Dozen/Vulkan teardown. Unloading them before a renderer thread exits can jump into
    // unmapped code. Keep the runtime loaded for the process lifetime, just as
    // the Windows driver does; no environment or system files are changed.
    static RETAIN: std::sync::Once = std::sync::Once::new();
    if std::path::Path::new("/dev/dxg").exists() {
        RETAIN.call_once(|| {
            for library in [
                c"/usr/lib/wsl/lib/libd3d12core.so",
                c"/usr/lib/wsl/lib/libdxcore.so",
            ] {
                // SAFETY: these are the WSL system runtime libraries. The
                // retained loader references intentionally live until exit.
                unsafe {
                    libc::dlopen(
                        library.as_ptr(),
                        libc::RTLD_LAZY | libc::RTLD_LOCAL | libc::RTLD_NODELETE,
                    );
                }
            }
        });
    }
}

#[derive(Clone, Copy)]
struct WgpuBackendCandidate {
    label: &'static str,
    backends: wgpu::Backends,
    force_fallback_adapter: bool,
    #[cfg(target_os = "android")]
    android_ahb_interop: bool,
    #[cfg(target_os = "android")]
    allow_cpu_adapter: bool,
    #[cfg(target_env = "ohos")]
    ohos_native_buffer_interop: bool,
}

struct WgpuDeviceContext {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    supports_16bit_norm: bool,
    #[cfg(target_os = "android")]
    android_vulkan: Option<AndroidVulkanInterop>,
    #[cfg(target_os = "android")]
    android_device_health: Arc<AndroidWgpuDeviceHealth>,
    #[cfg(target_os = "android")]
    android_backend_candidate_index: usize,
    #[cfg(target_env = "ohos")]
    ohos_vulkan: Option<OhosVulkanInterop>,
}

#[cfg(target_env = "ohos")]
fn ohos_vulkan_video_enabled() -> bool {
    !option_env!("ERIKA_OHOS_VULKAN_VIDEO").is_some_and(|value| {
        value == "0" || value.eq_ignore_ascii_case("false") || value.eq_ignore_ascii_case("off")
    })
}

#[cfg_attr(not(any(target_os = "android", target_env = "ohos")), allow(dead_code))]
fn fit_extent_without_upscale(
    source_width: u32,
    source_height: u32,
    max_width: u32,
    max_height: u32,
) -> (u32, u32) {
    if source_width == 0 || source_height == 0 {
        return (source_width, source_height);
    }
    let max_width = max_width.max(1);
    let max_height = max_height.max(1);
    if source_width <= max_width && source_height <= max_height {
        return (source_width, source_height);
    }
    if u64::from(source_width) * u64::from(max_height)
        > u64::from(source_height) * u64::from(max_width)
    {
        let height = (u64::from(source_height) * u64::from(max_width)
            + u64::from(source_width).saturating_sub(1))
            / u64::from(source_width);
        (
            max_width,
            u32::try_from(height)
                .unwrap_or(max_height)
                .clamp(1, max_height),
        )
    } else {
        let width = (u64::from(source_width) * u64::from(max_height)
            + u64::from(source_height).saturating_sub(1))
            / u64::from(source_height);
        (
            u32::try_from(width)
                .unwrap_or(max_width)
                .clamp(1, max_width),
            max_height,
        )
    }
}

fn wgpu_backend_candidates() -> Vec<WgpuBackendCandidate> {
    #[cfg(target_os = "android")]
    {
        // Keep the instances separate. A driver may support ordinary Vulkan
        // rendering while lacking one of the AHardwareBuffer interop extensions;
        // retain that Vulkan CPU-upload route before falling back to EGL/GLES.
        vec![
            WgpuBackendCandidate {
                label: "vulkan-ahb",
                backends: wgpu::Backends::VULKAN,
                force_fallback_adapter: false,
                android_ahb_interop: true,
                allow_cpu_adapter: false,
            },
            WgpuBackendCandidate {
                label: "vulkan",
                backends: wgpu::Backends::VULKAN,
                force_fallback_adapter: false,
                android_ahb_interop: false,
                allow_cpu_adapter: false,
            },
            WgpuBackendCandidate {
                label: "gles",
                backends: wgpu::Backends::GL,
                force_fallback_adapter: false,
                android_ahb_interop: false,
                allow_cpu_adapter: false,
            },
            WgpuBackendCandidate {
                label: "vulkan-software",
                backends: wgpu::Backends::VULKAN,
                force_fallback_adapter: true,
                android_ahb_interop: false,
                allow_cpu_adapter: true,
            },
        ]
    }
    #[cfg(target_env = "ohos")]
    {
        let mut candidates = Vec::new();
        if ohos_vulkan_video_enabled() {
            candidates.push(WgpuBackendCandidate {
                label: "ohos-vulkan-native-buffer",
                backends: wgpu::Backends::VULKAN,
                force_fallback_adapter: false,
                ohos_native_buffer_interop: true,
            });
        }
        candidates.extend([
            WgpuBackendCandidate {
                label: "ohos-vulkan",
                backends: wgpu::Backends::VULKAN,
                force_fallback_adapter: false,
                ohos_native_buffer_interop: false,
            },
            WgpuBackendCandidate {
                label: "ohos-gles",
                backends: wgpu::Backends::GL,
                force_fallback_adapter: false,
                ohos_native_buffer_interop: false,
            },
        ]);
        candidates
    }
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        vec![WgpuBackendCandidate {
            label: "linux",
            backends: wgpu::Backends::all().with_env(),
            force_fallback_adapter: false,
        }]
    }
    #[cfg(not(any(target_os = "android", target_os = "linux", target_env = "ohos")))]
    {
        vec![WgpuBackendCandidate {
            label: "platform-default",
            backends: wgpu::Backends::all(),
            force_fallback_adapter: false,
        }]
    }
}

fn backend_candidate_order(
    candidate_count: usize,
    start_index: usize,
    excluded: &[usize],
) -> Vec<usize> {
    if candidate_count == 0 {
        return Vec::new();
    }
    let start_index = start_index % candidate_count;
    (0..candidate_count)
        .map(|offset| (start_index + offset) % candidate_count)
        .filter(|candidate_index| !excluded.contains(candidate_index))
        .collect()
}

fn attempted_candidate_prefix(
    candidate_order: &[usize],
    selected_candidate: Option<usize>,
) -> Vec<usize> {
    let attempted_count = selected_candidate
        .and_then(|selected_candidate| {
            candidate_order
                .iter()
                .position(|candidate_index| *candidate_index == selected_candidate)
        })
        .map_or(candidate_order.len(), |position| position + 1);
    candidate_order
        .iter()
        .copied()
        .take(attempted_count)
        .collect()
}

#[cfg(target_os = "android")]
fn install_device_diagnostics(
    device: &wgpu::Device,
    candidate: &'static str,
    adapter_info: &wgpu::AdapterInfo,
) -> Arc<AndroidWgpuDeviceHealth> {
    let health = Arc::new(AndroidWgpuDeviceHealth::default());
    let backend = format!("{:?}", adapter_info.backend);
    let name = adapter_info.name.clone();
    let uncaptured_backend = backend.clone();
    let uncaptured_name = name.clone();
    let uncaptured_health = Arc::clone(&health);
    device.on_uncaptured_error(Arc::new(move |error| {
        let error_kind = match &error {
            wgpu::Error::OutOfMemory { .. } => "out_of_memory",
            wgpu::Error::Validation { .. } => "validation",
            wgpu::Error::Internal { .. } => "internal",
        };
        uncaptured_health.record(error_kind, error.to_string(), false);
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "wgpu_renderer",
                "stage": "uncaptured_error",
                "backendCandidate": candidate,
                "backend": uncaptured_backend.as_str(),
                "name": uncaptured_name.as_str(),
                "errorKind": error_kind,
                "message": error.to_string(),
            })
            .to_string(),
        );
    }));

    let lost_health = Arc::clone(&health);
    device.set_device_lost_callback(move |reason, message| {
        let failure_reason = if message.is_empty() {
            format!("{reason:?}")
        } else {
            format!("{reason:?}: {message}")
        };
        lost_health.record("device_lost", failure_reason, true);
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "wgpu_renderer",
                "stage": "device_lost",
                "backendCandidate": candidate,
                "backend": backend.as_str(),
                "name": name.as_str(),
                "reason": format!("{reason:?}"),
                "message": message,
            })
            .to_string(),
        );
    });
    health
}

fn request_wgpu_device(
    candidate: WgpuBackendCandidate,
    backend_candidate_index: usize,
    attempt_index: usize,
    attempt_count: usize,
    linux_surface: Option<WgpuSurfaceHandle>,
) -> std::result::Result<WgpuDeviceContext, String> {
    #[cfg(not(all(target_os = "linux", not(target_env = "ohos"))))]
    let _ = linux_surface;
    #[cfg(not(target_os = "android"))]
    let _ = (backend_candidate_index, attempt_index, attempt_count);

    #[cfg(target_os = "android")]
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "wgpu_renderer",
            "stage": "backend_attempt_started",
            "backendCandidate": candidate.label,
            "backendCandidateIndex": backend_candidate_index,
            "attempt": attempt_index + 1,
            "attemptCount": attempt_count,
            "fallback": attempt_index > 0,
            "forceFallbackAdapter": candidate.force_fallback_adapter,
            "adapterSelectionPolicy": if candidate.force_fallback_adapter {
                "forced_fallback_only"
            } else {
                "default"
            },
        })
        .to_string(),
    );

    #[cfg(target_os = "android")]
    if candidate.android_ahb_interop {
        let context = crate::renderer::android_vulkan::create_device().map_err(|message| {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "wgpu_renderer",
                    "stage": "android_vulkan_interop_unavailable",
                    "backendCandidate": candidate.label,
                    "attempt": attempt_index + 1,
                    "attemptCount": attempt_count,
                    "reason": message.as_str(),
                    "fallback": "vulkan_cpu_upload",
                })
                .to_string(),
            );
            message
        })?;
        let adapter_info = context.adapter.get_info();
        let adapter_limits = context.adapter.limits();
        let android_device_health =
            install_device_diagnostics(&context.device, candidate.label, &adapter_info);
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "wgpu_renderer",
                "stage": "device_created",
                "backendCandidate": candidate.label,
                "backendCandidateIndex": backend_candidate_index,
                "attempt": attempt_index + 1,
                "attemptCount": attempt_count,
                "fallback": false,
                "forceFallbackAdapter": candidate.force_fallback_adapter,
                "backend": format!("{:?}", adapter_info.backend),
                "deviceType": format!("{:?}", adapter_info.device_type),
                "name": adapter_info.name.as_str(),
                "driver": adapter_info.driver.as_str(),
                "driverInfo": adapter_info.driver_info.as_str(),
                "supports16BitNorm": context.supports_16bit_norm,
                "adapterMaxTextureDimension2D": adapter_limits.max_texture_dimension_2d,
                "adapterMaxUniformBufferBindingSize": adapter_limits.max_uniform_buffer_binding_size,
                "androidHardwareBufferInteropCapable": true,
            })
            .to_string(),
        );
        return Ok(WgpuDeviceContext {
            instance: context.instance,
            adapter: context.adapter,
            device: context.device,
            queue: context.queue,
            supports_16bit_norm: context.supports_16bit_norm,
            android_vulkan: Some(context.interop),
            android_device_health,
            android_backend_candidate_index: backend_candidate_index,
        });
    }

    #[cfg(target_env = "ohos")]
    if candidate.ohos_native_buffer_interop {
        let context = crate::renderer::ohos_vulkan::create_device().map_err(|message| {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "wgpu_renderer",
                    "stage": "ohos_vulkan_native_buffer_interop_unavailable",
                    "backendCandidate": candidate.label,
                    "attempt": attempt_index + 1,
                    "attemptCount": attempt_count,
                    "reason": message.as_str(),
                    "fallback": "ohos-vulkan",
                })
                .to_string(),
            );
            message
        })?;
        let adapter_info = context.adapter.get_info();
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "wgpu_renderer",
                "stage": "device_created",
                "backendCandidate": candidate.label,
                "backendCandidateIndex": backend_candidate_index,
                "attempt": attempt_index + 1,
                "attemptCount": attempt_count,
                "backend": format!("{:?}", adapter_info.backend),
                "deviceType": format!("{:?}", adapter_info.device_type),
                "name": adapter_info.name.as_str(),
                "driver": adapter_info.driver.as_str(),
                "driverInfo": adapter_info.driver_info.as_str(),
                "supports16BitNorm": context.supports_16bit_norm,
                "ohosNativeBufferInteropCapable": true,
            })
            .to_string(),
        );
        return Ok(WgpuDeviceContext {
            instance: context.instance,
            adapter: context.adapter,
            device: context.device,
            queue: context.queue,
            supports_16bit_norm: context.supports_16bit_norm,
            ohos_vulkan: Some(context.interop),
        });
    }

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    retain_wsl_d3d_runtime();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: candidate.backends,
        flags: wgpu_instance_flags(),
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        display: linux_surface.map(|handle| Box::new(LinuxDisplayHandle(handle)) as _),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    let compatible_surface = linux_surface
        .map(|handle| {
            let target = linux_surface_target(handle).map_err(|error| error.to_string())?;
            // SAFETY: the host retains the native handles until detach/destroy.
            unsafe { instance.create_surface_unsafe(target) }.map_err(|error| error.to_string())
        })
        .transpose()?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: candidate.force_fallback_adapter,
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        compatible_surface: compatible_surface.as_ref(),
        #[cfg(not(all(target_os = "linux", not(target_env = "ohos"))))]
        compatible_surface: None,
    }))
    .map_err(|error| {
        let message = format!("adapter request failed: {error}");
        #[cfg(target_os = "android")]
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "wgpu_renderer",
                "stage": "adapter_request_failed",
                "backendCandidate": candidate.label,
                "attempt": attempt_index + 1,
                "attemptCount": attempt_count,
                "forceFallbackAdapter": candidate.force_fallback_adapter,
                "reason": message.as_str(),
            })
            .to_string(),
        );
        message
    })?;
    let adapter_info = adapter.get_info();
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        let hardware = linux_adapter_is_hardware(adapter_info.device_type, &adapter_info.name);
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "wgpu_renderer",
                "stage": "adapter_selected",
                "backend": format!("{:?}", adapter_info.backend),
                "deviceType": format!("{:?}", adapter_info.device_type),
                "name": adapter_info.name,
                "driver": adapter_info.driver,
                "driverInfo": adapter_info.driver_info,
                "hardware": hardware,
                "displayBound": linux_surface.is_some(),
            })
            .to_string(),
        );
        if !hardware && std::env::var("ERIKA_REQUIRE_HARDWARE_GPU").as_deref() == Ok("1") {
            return Err(format!(
                "hardware GPU required, but selected CPU adapter {}",
                adapter_info.name
            ));
        }
    }
    #[cfg(target_os = "android")]
    if candidate.backends == wgpu::Backends::VULKAN
        && adapter_info.device_type == wgpu::DeviceType::Cpu
        && !candidate.allow_cpu_adapter
    {
        let message = format!(
            "software Vulkan adapter {} is deferred until after GLES",
            adapter_info.name
        );
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "wgpu_renderer",
                "stage": "software_vulkan_deferred",
                "backendCandidate": candidate.label,
                "attempt": attempt_index + 1,
                "attemptCount": attempt_count,
                "backend": format!("{:?}", adapter_info.backend),
                "deviceType": format!("{:?}", adapter_info.device_type),
                "name": adapter_info.name.as_str(),
                "fallback": "gles_cpu_upload",
                "reason": message.as_str(),
            })
            .to_string(),
        );
        return Err(message);
    }
    let adapter_limits = adapter.limits();

    // 16-bit normalized textures (R16Unorm/Rg16Unorm) are needed for P010/10-bit
    // upload. They are not in the WebGPU baseline, so request the feature only when
    // the adapter advertises it (true on Metal/Vulkan/DX12 native backends).
    let supports_16bit_norm = adapter
        .features()
        .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM);
    let required_features = if supports_16bit_norm {
        wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
    } else {
        wgpu::Features::empty()
    };
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    let required_features = required_features
        | if adapter_info.backend == wgpu::Backend::Vulkan {
            adapter.features() & wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
        } else {
            wgpu::Features::empty()
        };
    let required_limits = requested_device_limits(adapter_limits.clone(), adapter_info.backend);

    #[cfg(target_os = "android")]
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "wgpu_renderer",
            "stage": "adapter_selected",
            "backendCandidate": candidate.label,
            "attempt": attempt_index + 1,
            "attemptCount": attempt_count,
            "fallback": attempt_index > 0,
            "forceFallbackAdapter": candidate.force_fallback_adapter,
            "adapterSelectionPolicy": if candidate.force_fallback_adapter {
                "forced_fallback_only"
            } else {
                "default"
            },
            "backend": format!("{:?}", adapter_info.backend),
            "deviceType": format!("{:?}", adapter_info.device_type),
            "name": adapter_info.name.as_str(),
            "driver": adapter_info.driver.as_str(),
            "driverInfo": adapter_info.driver_info.as_str(),
            "androidHardwareBufferInteropCapable": false,
            "supports16BitNorm": supports_16bit_norm,
            "adapterMaxTextureDimension2D": adapter_limits.max_texture_dimension_2d,
            "adapterMaxUniformBufferBindingSize": adapter_limits.max_uniform_buffer_binding_size,
            "requestedMaxTextureDimension2D": required_limits.max_texture_dimension_2d,
            "requestedMaxUniformBufferBindingSize": required_limits.max_uniform_buffer_binding_size,
        })
        .to_string(),
    );

    let descriptor = wgpu::DeviceDescriptor {
        label: Some("erika-wgpu-device"),
        required_features,
        required_limits,
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        trace: wgpu::Trace::Off,
    };
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    let interop_device = crate::renderer::linux_vulkan::request_device(&adapter, &descriptor);
    #[cfg(not(all(target_os = "linux", not(target_env = "ohos"))))]
    let interop_device: Option<std::result::Result<(wgpu::Device, wgpu::Queue), String>> = None;
    let device_result = match interop_device {
        Some(Ok(device)) => Ok(device),
        unavailable => {
            if let Some(Err(reason)) = unavailable {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "wgpu_renderer", "stage": "interop_device_unavailable",
                        "reason": reason,
                    })
                    .to_string(),
                );
            }
            pollster::block_on(adapter.request_device(&descriptor)).map_err(|e| e.to_string())
        }
    };
    let (device, queue) = device_result.map_err(|error| {
        let message = format!("device request failed: {error}");
        #[cfg(target_os = "android")]
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "wgpu_renderer",
                "stage": "device_request_failed",
                "backendCandidate": candidate.label,
                "attempt": attempt_index + 1,
                "attemptCount": attempt_count,
                "backend": format!("{:?}", adapter_info.backend),
                "name": adapter_info.name.as_str(),
                "reason": message.as_str(),
            })
            .to_string(),
        );
        message
    })?;

    #[cfg(target_os = "android")]
    let android_device_health = install_device_diagnostics(&device, candidate.label, &adapter_info);

    #[cfg(target_os = "android")]
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "wgpu_renderer",
            "stage": "device_created",
            "backendCandidate": candidate.label,
            "backendCandidateIndex": backend_candidate_index,
            "attempt": attempt_index + 1,
            "attemptCount": attempt_count,
            "fallback": attempt_index > 0,
            "forceFallbackAdapter": candidate.force_fallback_adapter,
            "adapterSelectionPolicy": if candidate.force_fallback_adapter {
                "forced_fallback_only"
            } else {
                "default"
            },
            "backend": format!("{:?}", adapter_info.backend),
            "name": adapter_info.name.as_str(),
            "androidHardwareBufferInteropCapable": false,
        })
        .to_string(),
    );

    Ok(WgpuDeviceContext {
        instance,
        adapter,
        device,
        queue,
        supports_16bit_norm,
        #[cfg(target_os = "android")]
        android_vulkan: None,
        #[cfg(target_os = "android")]
        android_device_health,
        #[cfg(target_os = "android")]
        android_backend_candidate_index: backend_candidate_index,
        #[cfg(target_env = "ohos")]
        ohos_vulkan: None,
    })
}

impl WgpuRenderer {
    pub fn new() -> Result<Self> {
        Self::new_with_config(MetalRendererConfig::default())
    }

    pub fn new_with_output_mode(output_mode: OutputMode) -> Result<Self> {
        Self::new_with_config(MetalRendererConfig {
            output_mode,
            ..MetalRendererConfig::default()
        })
    }

    pub fn new_with_config(config: MetalRendererConfig) -> Result<Self> {
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        let config = {
            let mut config = config;
            match std::env::var("ERIKA_LINUX_HDR").as_deref() {
                Ok("auto") => config.output_mode = OutputMode::auto(12.5),
                Ok("1" | "on") => config.output_mode = OutputMode::extended_linear(12.5),
                Ok("0" | "off") => config.output_mode = OutputMode::Sdr,
                _ => {}
            }
            config
        };
        let candidate_count = wgpu_backend_candidates().len();
        Self::new_with_candidate_order(
            backend_candidate_order(candidate_count, 0, &[]),
            config.output_mode,
            config.video_alpha_mode,
            None,
        )
    }

    #[cfg(target_os = "android")]
    fn new_after_runtime_failure(
        current_candidate: usize,
        excluded: &[usize],
        output_mode: OutputMode,
        video_alpha_mode: VideoAlphaMode,
    ) -> (Result<Self>, Vec<usize>) {
        let candidate_count = wgpu_backend_candidates().len();
        let candidate_order = backend_candidate_order(
            candidate_count,
            current_candidate.saturating_add(1),
            excluded,
        );
        let result = Self::new_with_candidate_order(
            candidate_order.clone(),
            output_mode,
            video_alpha_mode,
            None,
        );
        let selected_candidate = result
            .as_ref()
            .ok()
            .map(|renderer| renderer.android_backend_candidate_index);
        (
            result,
            attempted_candidate_prefix(&candidate_order, selected_candidate),
        )
    }

    fn new_with_candidate_order(
        candidate_order: Vec<usize>,
        output_mode: OutputMode,
        video_alpha_mode: VideoAlphaMode,
        linux_surface: Option<WgpuSurfaceHandle>,
    ) -> Result<Self> {
        let candidates = wgpu_backend_candidates();
        if candidate_order.is_empty() {
            return Err(PlayerError::Renderer(
                "wgpu initialization has no untried backend candidates".to_string(),
            ));
        }
        let mut failures = Vec::with_capacity(candidate_order.len());
        let mut selected = None;
        for (attempt_index, candidate_index) in candidate_order.iter().copied().enumerate() {
            let candidate = candidates[candidate_index];
            match request_wgpu_device(
                candidate,
                candidate_index,
                attempt_index,
                candidate_order.len(),
                linux_surface,
            ) {
                Ok(context) => {
                    selected = Some(context);
                    break;
                }
                Err(error) => failures.push(format!("{}: {error}", candidate.label)),
            }
        }
        let Some(context) = selected else {
            let message = format!(
                "wgpu initialization failed after all backend candidates: {}",
                failures.join("; ")
            );
            #[cfg(target_os = "android")]
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "wgpu_renderer",
                    "stage": "initialization_failed",
                    "backendCandidates": candidate_order
                        .iter()
                        .map(|candidate_index| candidates[*candidate_index].label)
                        .collect::<Vec<_>>(),
                    "failures": failures,
                    "reason": message.as_str(),
                })
                .to_string(),
            );
            return Err(PlayerError::Renderer(message));
        };

        let upscaler = WgpuArtCnn::new(&context.adapter, &context.device);
        #[cfg(target_env = "ohos")]
        let ohos_native_buffer_surface = {
            let enabled = ohos_vulkan_video_enabled();
            if enabled && context.ohos_vulkan.is_some() {
                match crate::ohos::avcodec::OhosAvCodecSurface::new_native_buffer() {
                    Ok(surface) => {
                        crate::trace::diagnostic(
                            serde_json::json!({
                                "event": "ohos_avcodec_surface",
                                "stage": "vulkan_native_buffer_ready",
                                "zeroCopySource": true,
                                "experimental": true,
                            })
                            .to_string(),
                        );
                        Some(surface)
                    }
                    Err(error) => {
                        crate::trace::diagnostic(
                            serde_json::json!({
                                "event": "ohos_avcodec_surface",
                                "stage": "vulkan_native_buffer_unavailable",
                                "zeroCopySource": false,
                                "reason": error,
                            })
                            .to_string(),
                        );
                        None
                    }
                }
            } else {
                None
            }
        };
        #[cfg(target_env = "ohos")]
        let ohos_gles = {
            let enabled = option_env!("ERIKA_OHOS_SURFACE_VIDEO")
                .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
            if enabled {
                match crate::ohos::gles::OhosGlesInterop::new(&context.device) {
                    Ok(interop) => {
                        crate::trace::diagnostic(
                            serde_json::json!({
                                "event": "ohos_avcodec_surface",
                                "stage": "native_image_external_texture_ready",
                                "zeroCopySource": true,
                                "experimental": true,
                            })
                            .to_string(),
                        );
                        Some(interop)
                    }
                    Err(error) => {
                        crate::trace::diagnostic(
                            serde_json::json!({
                                "event": "ohos_avcodec_surface",
                                "stage": "native_image_external_texture_unavailable",
                                "zeroCopySource": false,
                                "reason": error,
                            })
                            .to_string(),
                        );
                        None
                    }
                }
            } else {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "ohos_avcodec_surface",
                        "stage": "experimental_surface_video_disabled",
                        "zeroCopySource": false,
                        "reason": "set ERIKA_OHOS_SURFACE_VIDEO=1 at compile time to test NativeImage import",
                    })
                    .to_string(),
                );
                None
            }
        };
        Ok(Self {
            _instance: context.instance,
            #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
            linux_display: linux_surface.map(|handle| (handle.kind, handle.raw_display)),
            #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
            linux_frame: None,
            #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
            linux_hardware_frame: None,
            #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
            linux_vulkan: None,
            #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
            linux_import_failure: None,
            flutter_metrics: None,
            flutter_frame: None,
            adapter: context.adapter,
            device: context.device,
            queue: context.queue,
            #[cfg(target_os = "android")]
            android_vulkan: context.android_vulkan,
            #[cfg(target_os = "android")]
            android_device_health: context.android_device_health,
            #[cfg(target_os = "android")]
            android_backend_candidate_index: context.android_backend_candidate_index,
            #[cfg(target_env = "ohos")]
            ohos_vulkan: context.ohos_vulkan,
            #[cfg(target_env = "ohos")]
            ohos_native_buffer_surface,
            #[cfg(target_env = "ohos")]
            ohos_gles,
            surface: None,
            video_pipeline: None,
            overlay_pipeline: None,
            current_video: None,
            current_video_visible: false,
            upload_serial: 0,
            danmaku_atlas_cache: None,
            supports_16bit_norm: context.supports_16bit_norm,
            output_mode,
            video_alpha_mode,
            output_status: OutputRuntimeStatus::requested(output_mode),
            output_headroom: OutputHeadroomState::default(),
            upscaler_mode: LumaUpscalerMode::Off,
            gamut_lut: None,
            gamut_lut_job: None,
            dummy_lut: None,
            upscaler,
            upscaler_failed_frame_token: None,
            upscaler_active_frame_reported: false,
            cpu_video_frame_fallback_reported: false,
            p010_byte_planes_reported: false,
            planar_upload_pool: std::collections::VecDeque::new(),
            sdr_hdr_output_reported: false,
            #[cfg(target_os = "android")]
            android_shared_frame_reported: false,
            #[cfg(target_os = "android")]
            android_downscaled_frame_reported: false,
            #[cfg(target_env = "ohos")]
            ohos_shared_frame_reported: false,
            stats: WgpuRendererStats::default(),
        })
    }

    pub fn surface(&self) -> Option<WgpuSurfaceHandle> {
        self.surface.as_ref().map(|attached| attached.handle)
    }

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    fn for_linux_display(&mut self, surface: Option<WgpuSurfaceHandle>) -> Result<Self> {
        let mut replacement = Self::new_with_candidate_order(
            backend_candidate_order(wgpu_backend_candidates().len(), 0, &[]),
            self.output_mode,
            self.video_alpha_mode,
            surface,
        )?;
        replacement.set_luma_upscaler(self.upscaler_mode);
        replacement.upload_serial = self.upload_serial;
        if let Some((frame, uniforms, color)) = self.linux_frame.as_ref() {
            replacement.upload_planar_with_context(frame.clone(), *uniforms, *color)?;
            replacement.current_video_visible = self.current_video_visible;
        } else if let Some(frame) = self.linux_hardware_frame.as_ref() {
            // Return shared decoder memory before a second VkDevice imports
            // it. Retaining the AVFrame keeps the paused picture available.
            if self
                .current_video
                .as_ref()
                .is_some_and(|video| video._linux_direct.is_some())
            {
                self.current_video = None;
            }
            replacement.upload_player_frame(frame)?;
            replacement.current_video_visible = self.current_video_visible;
        }
        replacement.stats = self.stats;
        Ok(replacement)
    }

    /// Whether the adapter supports 16-bit normalized textures (needed for P010).
    pub fn supports_16bit_norm(&self) -> bool {
        self.supports_16bit_norm
    }

    pub fn stats(&self) -> WgpuRendererStats {
        self.stats
    }

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    fn update_linux_output_for_source(&mut self, source_hdr: bool) -> Result<()> {
        let strict = std::env::var("ERIKA_REQUIRE_HDR").as_deref() == Ok("1");
        let Some(surface) = self.surface.as_mut() else {
            if source_hdr && strict && self.flutter_metrics.is_some() {
                return Err(PlayerError::Renderer("HDR requires a native Linux video surface; Flutter's RGBA8 texture compositor is SDR".into()));
            }
            return Ok(());
        };
        let selection = select_linux_surface_output(
            self.output_mode,
            source_hdr,
            self.adapter.get_info().backend,
            &surface.formats,
        )
        .ok_or_else(|| PlayerError::Renderer("Linux surface has no usable output format".into()))?;
        if source_hdr && strict && !selection.output.extended_linear {
            return Err(PlayerError::Renderer(format!(
                "HDR output unavailable: {}; the compositor must advertise an FP16 scRGB Vulkan surface",
                selection.fallback_reason.label()
            )));
        }
        let changed = surface.output != selection.output
            || surface.fallback_reason != selection.fallback_reason;
        if !changed {
            return Ok(());
        }
        if surface.config.format != selection.format {
            surface.config.format = selection.format;
            surface.surface.configure(&self.device, &surface.config);
        }
        surface.output = selection.output;
        surface.fallback_reason = selection.fallback_reason;
        let state = surface.output_state();
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "video_output_mode", "stage": "linux_surface_negotiated",
                "sourceHdr": source_hdr, "colorSpace": selection.output.color_space.label(),
                "surfaceFormat": format!("{:?}", selection.format),
                "hdrActive": selection.output.extended_linear,
                "cpuReadback": false, "reason": selection.fallback_reason.label(),
            })
            .to_string(),
        );
        self.observe_attached_output(state, true);
        Ok(())
    }

    fn observe_attached_output(&mut self, attached: AttachedOutputState, count_fallback: bool) {
        self.output_status.active_encoding = if attached.output.extended_linear {
            if cfg!(all(target_os = "linux", not(target_env = "ohos"))) {
                ActiveOutputEncoding::LinuxExtendedLinearScRgb
            } else {
                ActiveOutputEncoding::AndroidExtendedLinearScRgb
            }
        } else {
            ActiveOutputEncoding::SdrSrgb
        };
        self.output_status.surface_format = attached.output.surface_format;
        self.output_status.native_data_space = attached.native_data_space;
        self.output_status.active_headroom = if attached.output.extended_linear {
            if self.output_headroom.known {
                self.output_headroom.headroom
            } else {
                attached.output.target.edr_headroom.max(1.0)
            }
        } else {
            1.0
        };
        self.output_status.active_headroom_known = if attached.output.extended_linear {
            self.output_headroom.known
        } else {
            true
        };
        self.output_status.extended_linear_active = attached.output.extended_linear;
        self.output_status.fallback_reason = attached.fallback_reason;
        if count_fallback && attached.fallback_reason != OutputFallbackReason::None {
            self.output_status.fallback_count = self.output_status.fallback_count.saturating_add(1);
        }
        if count_fallback && attached.data_space_failure {
            self.output_status.data_space_failures =
                self.output_status.data_space_failures.saturating_add(1);
        }
    }

    fn update_output_headroom_state(&mut self, state: OutputHeadroomState, count_update: bool) {
        let unchanged = self.output_headroom.known == state.known
            && (self.output_headroom.headroom - state.headroom).abs() < 0.001;
        if unchanged {
            return;
        }
        self.output_headroom = state;

        let mut effective_content_headroom = None;
        let mut extended_linear_active = false;
        if let Some(attached) = self.surface.as_mut()
            && attached.output.extended_linear
        {
            let effective = effective_extended_linear_headroom(
                self.output_mode,
                attached.handle.output_capabilities,
                state,
            );
            attached.output = OutputDescription::extended_linear(effective);
            effective_content_headroom = Some(effective);
            extended_linear_active = true;
        }

        if extended_linear_active {
            self.output_status.active_headroom = if state.known {
                state.headroom
            } else {
                effective_content_headroom.unwrap_or_else(|| self.output_mode.headroom())
            };
            self.output_status.active_headroom_known = state.known;
        }
        if count_update && self.output_mode.is_android_extended_linear() {
            self.output_status.headroom_updates =
                self.output_status.headroom_updates.saturating_add(1);
        }

        #[cfg(target_os = "android")]
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "video_output_mode",
                "stage": "headroom_updated",
                "requested": "extended_linear",
                "reportedHeadroom": state.headroom,
                "reportedHeadroomKnown": state.known,
                "effectiveContentHeadroom": effective_content_headroom,
                "extendedLinearActive": extended_linear_active,
                "headroomUpdates": self.output_status.headroom_updates,
            })
            .to_string(),
        );
    }

    fn observe_detached_output(&mut self) {
        self.output_status.active_encoding = ActiveOutputEncoding::SdrSrgb;
        self.output_status.surface_format = OutputSurfaceFormat::EightBitUnorm;
        self.output_status.native_data_space = -1;
        self.output_status.active_headroom = 1.0;
        self.output_status.active_headroom_known = false;
        self.output_status.extended_linear_active = false;
    }

    pub fn adapter_info(&self) -> wgpu::AdapterInfo {
        self.adapter.get_info()
    }

    fn next_upload_serial(&mut self) -> u64 {
        self.upload_serial = self.upload_serial.saturating_add(1).max(1);
        self.upload_serial
    }

    #[cfg(target_os = "android")]
    fn android_backend_candidate_label(&self) -> &'static str {
        wgpu_backend_candidates()
            .get(self.android_backend_candidate_index)
            .map_or("unknown", |candidate| candidate.label)
    }

    #[cfg(target_os = "android")]
    fn android_poll_device_health(&self, operation: &'static str) -> Result<()> {
        if let Some(failure) = self.android_device_health.failure() {
            return Err(PlayerError::Renderer(format!(
                "Android wgpu device is unhealthy before {operation}: kind={} backend={} reason={}",
                failure.kind,
                self.android_backend_candidate_label(),
                failure.reason
            )));
        }
        if let Err(error) = self.device.poll(wgpu::PollType::Poll) {
            self.android_device_health
                .record("poll_error", error.to_string(), true);
        }
        if let Some(failure) = self.android_device_health.failure() {
            return Err(PlayerError::Renderer(format!(
                "Android wgpu device became unhealthy during {operation}: kind={} backend={} reason={}",
                failure.kind,
                self.android_backend_candidate_label(),
                failure.reason
            )));
        }
        Ok(())
    }

    /// Render a single clear pass into an offscreen `width`x`height` target and read
    /// the result back to host memory. This is the backend's headless test path: it
    /// needs no window or platform surface, so it runs under plain `cargo test`.
    pub fn clear_offscreen(
        &mut self,
        width: u32,
        height: u32,
        color: WgpuClearColor,
    ) -> Result<WgpuOffscreenReadback> {
        if width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "offscreen target must have non-zero dimensions".to_string(),
            ));
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("erika-wgpu-offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("erika-wgpu-offscreen-encoder"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("erika-wgpu-offscreen-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(color.to_wgpu()),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }

        self.queue.submit(Some(encoder.finish()));

        let rgba = self.read_back_rgba8(&texture, width, height)?;
        self.stats.offscreen_frames += 1;
        Ok(WgpuOffscreenReadback {
            width,
            height,
            rgba,
        })
    }

    /// Copy an RGBA8 texture into host memory, stripping the row padding that
    /// `copy_texture_to_buffer` requires (rows aligned to COPY_BYTES_PER_ROW_ALIGNMENT).
    fn read_back_rgba8(&self, texture: &wgpu::Texture, width: u32, height: u32) -> Result<Vec<u8>> {
        let unpadded_bytes_per_row = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
        let buffer_size = (padded_bytes_per_row * height) as wgpu::BufferAddress;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("erika-wgpu-readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("erika-wgpu-readback-encoder"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| PlayerError::Renderer(format!("wgpu device poll failed: {error}")))?;
        receiver
            .recv()
            .map_err(|_| PlayerError::Renderer("wgpu readback channel dropped".to_string()))?
            .map_err(|error| PlayerError::Renderer(format!("wgpu buffer map failed: {error}")))?;

        let mapped = slice.get_mapped_range();
        let mut rgba = Vec::with_capacity((unpadded_bytes_per_row * height) as usize);
        for row in 0..height {
            let start = (row * padded_bytes_per_row) as usize;
            let end = start + unpadded_bytes_per_row as usize;
            rgba.extend_from_slice(&mapped[start..end]);
        }
        drop(mapped);
        readback.unmap();
        Ok(rgba)
    }

    /// Render a software-decoded NV12 frame through the WGSL video pipeline into an
    /// offscreen RGBA8 target and read it back. Mirrors the Metal `render_video_frame`
    /// path so results can be compared against the native backend.
    ///
    /// `luma` is `width * height` bytes (Y plane). `chroma` is the interleaved
    /// Cb/Cr plane at half resolution:
    /// `ceil(width / 2) * ceil(height / 2) * 2` bytes.
    pub fn render_nv12_offscreen(
        &mut self,
        width: u32,
        height: u32,
        luma: &[u8],
        chroma: &[u8],
        uniforms: VideoUniforms,
    ) -> Result<WgpuOffscreenReadback> {
        self.upload_nv12(width, height, luma, chroma, uniforms)?;
        self.render_current_offscreen(None)?
            .ok_or_else(|| PlayerError::Renderer("no current frame after upload".to_string()))
    }

    /// Upload tightly packed NV12 planes as the current video frame. `luma` is
    /// `width * height` bytes; `chroma` is the interleaved Cb/Cr plane at half
    /// resolution (`ceil(width / 2) * ceil(height / 2) * 2` bytes).
    pub fn upload_nv12(
        &mut self,
        width: u32,
        height: u32,
        luma: &[u8],
        chroma: &[u8],
        uniforms: VideoUniforms,
    ) -> Result<()> {
        self.upload_planar(
            PlanarFrame {
                format: PlanarPixelFormat::Nv12,
                width,
                height,
                luma: luma.to_vec(),
                chroma: chroma.to_vec(),
            },
            uniforms,
        )
    }

    /// Upload a repacked planar frame (8-bit NV12 or 10-bit P010) as the current
    /// video frame. When the adapter lacks `TEXTURE_FORMAT_16BIT_NORM`, P010 is
    /// uploaded unchanged as byte pairs and reconstructed by the GPU. This
    /// preserves all 10 bits without a per-pixel CPU conversion.
    pub fn upload_planar(&mut self, frame: PlanarFrame, uniforms: VideoUniforms) -> Result<()> {
        self.upload_planar_with_context(frame, uniforms, None)
    }

    fn upload_planar_with_context(
        &mut self,
        frame: PlanarFrame,
        uniforms: VideoUniforms,
        source_color: Option<SourceColorState>,
    ) -> Result<()> {
        let prepared = prepare_planar_upload(frame, uniforms, self.supports_16bit_norm);
        if prepared.path == PlanarUploadPath::GpuP010Bytes && !self.p010_byte_planes_reported {
            self.p010_byte_planes_reported = true;
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "video_frame_import",
                    "stage": "gpu_p010_byte_planes",
                    "renderer": "wgpu",
                    "width": prepared.frame.width,
                    "height": prepared.frame.height,
                    "sourcePixelFormat": "P010",
                    "uploadPixelFormat": "P010_LE_BYTES",
                    "sourceBitDepth": 10,
                    "uploadBitDepth": 10,
                    "adapterSupports16BitNorm": self.supports_16bit_norm,
                    "colorPipelinePreserved": true,
                    "hdrDescriptionPreserved": true,
                    "fullRange": prepared.uniforms.full_range != 0,
                    "sourceTransferCode": prepared.uniforms.source_transfer,
                    "sourcePeakNits": prepared.uniforms.nits[0],
                    "toneMapCode": prepared.uniforms.tone_map,
                    "reason": "adapter lacks TEXTURE_FORMAT_16BIT_NORM; sample unchanged P010 bytes through RG8/RGBA8 textures and reconstruct 16-bit UNORM on the GPU",
                })
                .to_string(),
            );
        }
        let frame = prepared.frame;
        let uniforms = prepared.uniforms;
        let width = frame.width;
        let height = frame.height;
        if width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "planar frame dimensions must be non-zero".to_string(),
            ));
        }
        let (luma_format, chroma_format, bytes_per_sample) = match frame.format {
            PlanarPixelFormat::Nv12 => (
                wgpu::TextureFormat::R8Unorm,
                wgpu::TextureFormat::Rg8Unorm,
                1u32,
            ),
            PlanarPixelFormat::P010 if uniforms.has_p010_byte_planes() => (
                wgpu::TextureFormat::Rg8Unorm,
                wgpu::TextureFormat::Rgba8Unorm,
                2u32,
            ),
            PlanarPixelFormat::P010 => (
                wgpu::TextureFormat::R16Unorm,
                wgpu::TextureFormat::Rg16Unorm,
                2u32,
            ),
        };
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let expected_luma = (width * height * bytes_per_sample) as usize;
        let expected_chroma = (chroma_width * chroma_height * 2 * bytes_per_sample) as usize;
        if frame.luma.len() != expected_luma {
            return Err(PlayerError::Renderer(format!(
                "{:?} luma plane is {} bytes, expected {expected_luma}",
                frame.format,
                frame.luma.len()
            )));
        }
        if frame.chroma.len() != expected_chroma {
            return Err(PlayerError::Renderer(format!(
                "{:?} chroma plane is {} bytes, expected {expected_chroma}",
                frame.format,
                frame.chroma.len()
            )));
        }

        // The pool contains CPU upload storage only, never decoder aliases.
        // Rotate three slots to avoid serializing upload with the last draw.
        if self
            .planar_upload_pool
            .front()
            .is_some_and(|(luma, chroma)| {
                luma.width() != width
                    || luma.height() != height
                    || luma.format() != luma_format
                    || chroma.width() != chroma_width
                    || chroma.height() != chroma_height
                    || chroma.format() != chroma_format
            })
        {
            self.planar_upload_pool.clear();
        }
        let reusable = if self.planar_upload_pool.len() == 3 {
            self.planar_upload_pool.pop_front()
        } else {
            None
        };
        let (luma_texture, chroma_texture) = if let Some((luma, chroma)) = reusable {
            // Queue ordering still protects reuse if the GPU falls behind.
            self.write_plane_texture(&luma, &frame.luma, width * bytes_per_sample);
            self.write_plane_texture(&chroma, &frame.chroma, chroma_width * 2 * bytes_per_sample);
            (luma, chroma)
        } else {
            let luma_texture = self.create_plane_texture(
                "erika-wgpu-luma",
                width,
                height,
                luma_format,
                &frame.luma,
                width * bytes_per_sample,
            );
            let chroma_texture = self.create_plane_texture(
                "erika-wgpu-chroma",
                chroma_width,
                chroma_height,
                chroma_format,
                &frame.chroma,
                chroma_width * 2 * bytes_per_sample,
            );
            (luma_texture, chroma_texture)
        };
        self.planar_upload_pool
            .push_back((luma_texture.clone(), chroma_texture.clone()));
        let frame_token = self.next_upload_serial();
        self.current_video = Some(UploadedVideoFrame {
            textures: UploadedVideoTextures::Planar {
                luma: luma_texture,
                chroma: chroma_texture,
            },
            width,
            height,
            uniforms,
            source_color,
            frame_token,
            #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
            _linux_direct: None,
        });
        self.current_video_visible = true;
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        {
            self.linux_frame = Some((frame, uniforms, source_color));
            self.linux_hardware_frame = None;
        }
        Ok(())
    }

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    fn upload_linux_hardware_frame(&mut self, frame: &PlayerVideoFrame) -> Result<bool> {
        let require_direct = std::env::var("ERIKA_REQUIRE_ZERO_COPY").as_deref() == Ok("1");
        let wsl_copy = std::env::var("ERIKA_WSL_D3D12").as_deref() == Ok("1");
        let strict =
            require_direct || std::env::var("ERIKA_REQUIRE_GPU_FRAMES").as_deref() == Ok("1");
        if let Some((generation, reason)) = &self.linux_import_failure {
            if *generation == frame.generation {
                return if strict {
                    Err(PlayerError::Renderer(reason.clone()))
                } else {
                    Ok(false)
                };
            }
        }
        let interop = self.linux_vulkan.get_or_insert_with(|| {
            crate::renderer::linux_vulkan::LinuxVulkanInterop::new(&self.adapter, &self.device)
        });
        let result = match interop {
            Ok(interop) => frame
                .frame
                .decoded_frame()
                .ok_or_else(|| "Expected an FFmpeg hardware frame".to_owned())
                .and_then(|decoded| {
                    if frame.decode_backend == DecoderBackend::Vaapi {
                        let imported = if wsl_copy {
                            if require_direct {
                                return Err("WSL D3D12 uses a GPU plane copy; direct zero-copy was required".into());
                            }
                            interop.import_wsl_copy(decoded)
                        } else {
                            interop.import_direct(decoded)
                        };
                        imported.map(|direct| {
                            let [luma, chroma] = direct.planes.clone();
                            let is_p010 = luma.format() == wgpu::TextureFormat::R16Unorm;
                            (luma, chroma, is_p010, Some(direct))
                        })
                    } else if require_direct {
                        Err("Direct zero-copy was required, but CUDA/Vulkan uses a GPU copy".into())
                    } else {
                        interop
                            .copy_planes(&self.queue, decoded)
                            .map(|(luma, chroma, p010)| (luma, chroma, p010, None))
                    }
                }),
            Err(reason) => Err(reason.clone()),
        };
        let (luma, chroma, is_p010, direct) = match result {
            Ok(textures) => textures,
            Err(reason) => {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "video_frame_import", "stage": "linux_gpu_import_unavailable",
                        "decodeBackend": frame.decode_backend.as_str(), "reason": reason,
                        "fallback": if strict { "rejected" } else { "cpu_planes" },
                    })
                    .to_string(),
                );
                self.linux_import_failure = Some((frame.generation, reason.clone()));
                return if strict {
                    Err(PlayerError::Renderer(reason))
                } else {
                    Ok(false)
                };
            }
        };
        let retained = PlayerVideoFrame {
            frame: frame
                .frame
                .try_clone_ref()
                .map_err(|e| PlayerError::Renderer(e.to_string()))?,
            decode_backend: frame.decode_backend,
            pts: frame.pts,
            media_time: frame.media_time,
            late_by: frame.late_by,
            generation: frame.generation,
            scene_avg_nits: frame.scene_avg_nits,
        };
        let uniforms = self.video_uniforms_for_frame(frame, is_p010);
        let frame_token = self.next_upload_serial();
        let direct_zero_copy = direct.is_some() && !wsl_copy;
        self.current_video = Some(UploadedVideoFrame {
            textures: UploadedVideoTextures::Planar { luma, chroma },
            width: frame.frame.width(),
            height: frame.frame.height(),
            uniforms,
            source_color: Some(source_color_for_player_frame(frame)),
            frame_token,
            _linux_direct: direct,
        });
        self.current_video_visible = true;
        self.linux_frame = None;
        self.linux_hardware_frame = Some(retained);
        self.stats.hardware_video_frames += 1;
        self.stats.shared_handle_video_frames += 1;
        if direct_zero_copy {
            self.stats.zero_copy_video_frames += 1;
            self.stats.direct_zero_copy_video_frames += 1;
        }
        if self.stats.shared_handle_video_frames == 1 {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "video_frame_import", "stage": "linux_vulkan_gpu_planes",
                    "decodeBackend": frame.decode_backend.as_str(), "hostPixelCopies": 0,
                    "directZeroCopy": direct_zero_copy, "p010": is_p010,
                    "gpuPlaneCopies": if direct_zero_copy { 0 } else { 1 },
                })
                .to_string(),
            );
        }
        Ok(true)
    }

    fn video_uniforms_for_frame(
        &mut self,
        frame: &PlayerVideoFrame,
        is_p010: bool,
    ) -> VideoUniforms {
        let source = source_color_for_player_frame(frame);
        let output = self
            .surface
            .as_ref()
            .map_or_else(OutputDescription::sdr, |surface| surface.output);
        let pipeline = VideoRenderPipeline::new(source, output.tone_map_target_for(&source));
        if source.is_hdr() {
            self.stats.hdr_source_frames += 1;
            if !output.extended_linear && pipeline.requires_tone_mapping() {
                self.stats.sdr_tonemap_frames += 1;
            }
            if !output.extended_linear && !self.sdr_hdr_output_reported {
                self.sdr_hdr_output_reported = true;
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "video_output_mode",
                        "stage": "sdr_tonemap",
                        "renderer": "wgpu",
                        "sourcePrimaries": format!("{:?}", source.primaries),
                        "sourceTransfer": format!("{:?}", source.transfer),
                        "sourcePeakNits": source.nominal_peak_nits,
                        "targetPrimaries": "Bt709",
                        "targetTransfer": "Srgb",
                        "reason": "the active wgpu surface is SDR; HDR source is tone-mapped instead of being silently clipped",
                    })
                    .to_string(),
                );
            }
        }
        VideoUniforms::from_pipeline(&pipeline, is_p010, output.extended_linear)
            .packed_alpha_right(self.video_alpha_mode.has_alpha())
    }

    #[cfg(target_os = "android")]
    fn upload_android_mediacodec_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        if self.android_vulkan.is_none() {
            return Err(PlayerError::Renderer(
                "stage=android_mediacodec_import reason=vulkan_ahardwarebuffer_interop_unavailable"
                    .to_string(),
            ));
        }
        let image = frame.frame.prepared_mediacodec_image().map_err(|error| {
            PlayerError::Renderer(format!(
                "stage=android_mediacodec_prepared_image reason={error}"
            ))
        })?;
        let description = image.description();
        let crop = image.crop();
        let timestamp_ns = image.timestamp_ns();
        let owner: std::sync::Arc<dyn std::any::Any + Send + Sync> = image.clone();
        let hardware_buffer = image.hardware_buffer().cast::<ash::vk::AHardwareBuffer>();
        let crop = AndroidAhbCrop {
            left: crop.left as u32,
            top: crop.top as u32,
            right: crop.right as u32,
            bottom: crop.bottom as u32,
        };
        let visible_width = crop.right.saturating_sub(crop.left);
        let visible_height = crop.bottom.saturating_sub(crop.top);
        let (output_width, output_height) =
            self.android_ahb_conversion_extent(visible_width, visible_height);
        let source_color = source_color_for_player_frame(frame);
        let output_format = android_ahb_intermediate_format(
            source_color,
            self.android_vulkan
                .as_ref()
                .expect("Android Vulkan interop checked"),
        );
        let frame_description = AndroidAhbFrameDescription {
            hardware_buffer,
            buffer_width: description.width,
            buffer_height: description.height,
            crop,
            output_width,
            output_height,
            color_range: frame.frame.color_range(),
            matrix_coefficients: frame.frame.matrix_coefficients(),
            output_format,
            owner,
        };
        let mut state_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("erika-android-ahb-state-encoder"),
                });
        let mut conversion_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("erika-android-ahb-conversion-encoder"),
                });
        let conversion = unsafe {
            self.android_vulkan
                .as_ref()
                .expect("Android Vulkan interop checked")
                .convert_ahardware_buffer(
                    &self.device,
                    &mut state_encoder,
                    &mut conversion_encoder,
                    frame_description,
                )
        }
        .map_err(|error| match error {
            AndroidAhbConversionError::Backpressure { .. } => PlayerError::RendererBackpressure(
                format!("stage=android_ahardwarebuffer_conversion reason={error}"),
            ),
            AndroidAhbConversionError::Interop(_) => PlayerError::Renderer(format!(
                "stage=android_ahardwarebuffer_conversion reason={error}"
            )),
        })?;
        let uniforms = self.video_uniforms_for_frame(frame, false);
        // Preserve this order: the wgpu command buffer establishes the tracked
        // COLOR_ATTACHMENT state, then the raw Vulkan command buffer overwrites
        // the texture while leaving the real image in that same state.
        let submission = self
            .queue
            .submit([state_encoder.finish(), conversion_encoder.finish()]);
        let _ = submission;
        let conversion_output_format = conversion.output_format;
        retire_ahb_conversion_after_submission(&self.queue, conversion.pending);
        self.upload_converted_rgb_texture(
            conversion.texture,
            conversion.width,
            conversion.height,
            uniforms,
            Some(source_color),
        )?;
        self.stats.hardware_video_frames += 1;
        self.stats.zero_copy_video_frames += 1;
        self.stats.shared_handle_video_frames += 1;
        let conversion_downscaled =
            conversion.width < visible_width || conversion.height < visible_height;
        if !self.android_shared_frame_reported
            || (conversion_downscaled && !self.android_downscaled_frame_reported)
        {
            self.android_shared_frame_reported = true;
            self.android_downscaled_frame_reported |= conversion_downscaled;
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "video_frame_import",
                    "stage": "android_ahardwarebuffer_shared_handle_active",
                    "decodeBackend": frame.decode_backend.as_str(),
                    "timestampNs": timestamp_ns,
                    "bufferWidth": description.width,
                    "bufferHeight": description.height,
                    "visibleWidth": conversion.width,
                    "visibleHeight": conversion.height,
                    "decodedVisibleWidth": visible_width,
                    "decodedVisibleHeight": visible_height,
                    "conversionDownscaled": conversion_downscaled,
                    "crop": {
                        "left": crop.left,
                        "top": crop.top,
                        "right": crop.right,
                        "bottom": crop.bottom,
                    },
                    "directPlaneSampling": false,
                    "conversionTarget": conversion_output_format.diagnostic_name(),
                    "conversionTargetBytesPerPixel": conversion_output_format.bytes_per_pixel(),
                })
                .to_string(),
            );
        }
        Ok(())
    }

    #[cfg(target_os = "android")]
    fn android_ahb_conversion_extent(&self, source_width: u32, source_height: u32) -> (u32, u32) {
        let Some(surface) = self.surface.as_ref() else {
            return (source_width, source_height);
        };
        fit_extent_without_upscale(
            source_width,
            source_height,
            surface.config.width,
            surface.config.height,
        )
    }

    #[cfg(any(target_os = "android", target_env = "ohos"))]
    fn upload_converted_rgb_texture(
        &mut self,
        texture: wgpu::Texture,
        width: u32,
        height: u32,
        uniforms: VideoUniforms,
        source_color: Option<SourceColorState>,
    ) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "converted native frame dimensions must be non-zero".to_string(),
            ));
        }
        let frame_token = self.next_upload_serial();
        self.current_video = Some(UploadedVideoFrame {
            textures: UploadedVideoTextures::Rgb { texture },
            width,
            height,
            uniforms: uniforms.rgb_texture_input(),
            source_color,
            frame_token,
        });
        self.current_video_visible = true;
        Ok(())
    }

    #[cfg(target_env = "ohos")]
    fn upload_ohos_avcodec_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        let image = frame.frame.prepared_ohos_native_buffer().map_err(|error| {
            PlayerError::Renderer(format!("stage=ohos_native_buffer_prepare reason={error}"))
        })?;
        if let Some(interop) = self.ohos_vulkan.as_ref() {
            let (native_buffer, config) = image.native_buffer().map_err(|error| {
                PlayerError::Renderer(format!("stage=ohos_native_buffer_acquire reason={error}"))
            })?;
            let buffer_width = u32::try_from(config.width).map_err(|_| {
                PlayerError::Renderer(format!(
                    "stage=ohos_native_buffer_config reason=invalid_width width={}",
                    config.width
                ))
            })?;
            let buffer_height = u32::try_from(config.height).map_err(|_| {
                PlayerError::Renderer(format!(
                    "stage=ohos_native_buffer_config reason=invalid_height height={}",
                    config.height
                ))
            })?;
            let visible_width = frame.frame.width().min(buffer_width);
            let visible_height = frame.frame.height().min(buffer_height);
            let (output_width, output_height) =
                self.ohos_native_buffer_conversion_extent(visible_width, visible_height);
            let owner: std::sync::Arc<dyn std::any::Any + Send + Sync> = image.clone();
            let description = OhosNativeBufferFrameDescription {
                native_buffer,
                buffer_width,
                buffer_height,
                usage: config.usage as u32 as u64,
                crop: OhosNativeBufferCrop {
                    left: 0,
                    top: 0,
                    right: visible_width,
                    bottom: visible_height,
                },
                output_width,
                output_height,
                color_range: frame.frame.color_range(),
                matrix_coefficients: frame.frame.matrix_coefficients(),
                owner,
            };
            let mut state_encoder =
                self.device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("erika-ohos-native-buffer-state-encoder"),
                    });
            let mut conversion_encoder =
                self.device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("erika-ohos-native-buffer-conversion-encoder"),
                    });
            let conversion = unsafe {
                interop.convert_native_buffer(
                    &self.device,
                    &mut state_encoder,
                    &mut conversion_encoder,
                    description,
                )
            }
            .map_err(|error| match error {
                OhosNativeBufferConversionError::Backpressure { .. } => {
                    PlayerError::RendererBackpressure(format!(
                        "stage=ohos_native_buffer_conversion reason={error}"
                    ))
                }
                OhosNativeBufferConversionError::Interop(_) => PlayerError::Renderer(format!(
                    "stage=ohos_native_buffer_conversion reason={error}"
                )),
            })?;
            let source_color = source_color_for_player_frame(frame);
            let uniforms = self.video_uniforms_for_frame(frame, false);
            self.queue
                .submit([state_encoder.finish(), conversion_encoder.finish()]);
            retire_ohb_conversion_after_submission(&self.queue, conversion.pending);
            self.upload_converted_rgb_texture(
                conversion.texture,
                conversion.width,
                conversion.height,
                uniforms,
                Some(source_color),
            )?;
            self.stats.hardware_video_frames += 1;
            self.stats.zero_copy_video_frames += 1;
            self.stats.shared_handle_video_frames += 1;
            if !self.ohos_shared_frame_reported {
                self.ohos_shared_frame_reported = true;
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "video_frame_import",
                        "stage": "ohos_vulkan_native_buffer_active",
                        "decodeBackend": frame.decode_backend.as_str(),
                        "bufferWidth": buffer_width,
                        "bufferHeight": buffer_height,
                        "visibleWidth": conversion.width,
                        "visibleHeight": conversion.height,
                        "decodedWidth": visible_width,
                        "decodedHeight": visible_height,
                        "nativeFormat": config.format,
                        "nativeStride": config.stride,
                        "nativeUsage": config.usage,
                        "conversionTarget": "rgba16float",
                        "cpuReadback": false,
                        "synchronization": "acquire_fence_then_wgpu_submission_retirement",
                    })
                    .to_string(),
                );
            }
            return Ok(());
        }
        let width = frame.frame.width();
        let height = frame.frame.height();
        let source_color = source_color_for_player_frame(frame);
        let texture = self
            .ohos_gles
            .as_ref()
            .ok_or_else(|| {
                PlayerError::Renderer(
                    "stage=ohos_native_image_interop reason=not_initialized".to_string(),
                )
            })?
            .convert(&self.queue, &image, width, height)
            .map_err(|error| {
                PlayerError::Renderer(format!("stage=ohos_native_image_conversion reason={error}"))
            })?;
        let Some(texture) = texture else {
            // The NativeImage could not produce a texture this tick. Keeping
            // the previous frame on screen is correct, but the drop must stay
            // observable instead of silently mimicking a successful import.
            crate::trace::log(
                "[erika-wgpu-trace] stage=ohos_native_image_conversion result=texture_not_ready",
            );
            return Ok(());
        };
        let uniforms = self
            .video_uniforms_for_frame(frame, false)
            .rgb_texture_input();
        let frame_token = self.next_upload_serial();
        self.current_video = Some(UploadedVideoFrame {
            textures: UploadedVideoTextures::Rgb { texture },
            width,
            height,
            uniforms,
            source_color: Some(source_color),
            frame_token,
        });
        self.current_video_visible = true;
        self.stats.hardware_video_frames += 1;
        self.stats.zero_copy_video_frames += 1;
        self.stats.shared_handle_video_frames += 1;
        if !self.ohos_shared_frame_reported {
            self.ohos_shared_frame_reported = true;
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "video_frame_import",
                    "stage": "ohos_native_buffer_external_oes_active",
                    "decodeBackend": frame.decode_backend.as_str(),
                    "width": width,
                    "height": height,
                    "conversionTarget": "rgba8",
                    "cpuReadback": false,
                    "synchronization": "gl_finish",
                })
                .to_string(),
            );
        }
        Ok(())
    }

    #[cfg(target_env = "ohos")]
    fn ohos_native_buffer_conversion_extent(
        &self,
        source_width: u32,
        source_height: u32,
    ) -> (u32, u32) {
        let Some(surface) = self.surface.as_ref() else {
            return (source_width, source_height);
        };
        fit_extent_without_upscale(
            source_width,
            source_height,
            surface.config.width,
            surface.config.height,
        )
    }

    /// Render the current video frame (optionally compositing `overlay`) into an
    /// offscreen RGBA8 target and read it back. Returns `None` if no frame has been
    /// uploaded.
    pub fn render_current_offscreen(
        &mut self,
        overlay: Option<&OverlayFrame>,
    ) -> Result<Option<WgpuOffscreenReadback>> {
        let Some((width, height)) = self
            .current_video
            .as_ref()
            .map(|video| (video.width, video.height))
        else {
            return Ok(None);
        };
        self.render_current_offscreen_sized(width, height, overlay, None)
    }

    fn render_current_offscreen_sized(
        &mut self,
        width: u32,
        height: u32,
        overlay: Option<&OverlayFrame>,
        danmaku: Option<&DanmakuRenderPlan>,
    ) -> Result<Option<WgpuOffscreenReadback>> {
        if self.current_video.is_none() {
            return Ok(None);
        }
        if width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "offscreen target must have non-zero dimensions".to_string(),
            ));
        }
        self.ensure_video_pipeline(OFFSCREEN_FORMAT);
        if overlay.is_some_and(overlay_has_planes) || danmaku.is_some_and(|plan| !plan.is_empty()) {
            self.ensure_overlay_pipeline(OFFSCREEN_FORMAT);
        }
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("erika-wgpu-video-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let danmaku_draws = self.draw_current_video(
            &target_view,
            width,
            height,
            overlay,
            danmaku,
            OutputDescription::sdr(),
        )?;
        let rgba = self.read_back_rgba8(&target, width, height)?;
        self.stats.rendered_frames += 1;
        self.stats.offscreen_frames += 1;
        if danmaku_draws > 0 {
            self.stats.danmaku_passes += 1;
            self.stats.danmaku_items += danmaku_draws as u64;
        }
        Ok(Some(WgpuOffscreenReadback {
            width,
            height,
            rgba,
        }))
    }

    /// Lazily create (and cache) the perceptual gamut LUT texture described
    /// by the current uniforms. Returns `None` while the fast path is active
    /// or the background generation has not finished yet; callers must then
    /// mask `gamut_lut_enabled` off so the shader keeps the fast path.
    fn gamut_lut_view(&mut self, uniforms: VideoUniforms) -> Option<wgpu::TextureView> {
        let (source, target, target_black_pq, target_peak_pq) = gamut_lut_key_of(uniforms)?;
        let cached = self.gamut_lut.as_ref().is_some_and(|(key, _, _)| {
            key.0 == source
                && key.1 == target
                && key.2 == target_black_pq
                && key.3 == target_peak_pq
        });
        if !cached {
            let peak_nits = uniforms.nits[1].max(1.0);
            let params = GamutLutParams {
                source,
                target,
                // Same target black the shader derives from tone_map_extra.z.
                min_luma: pq_code_for_lut(uniforms.tone_map_extra[2]),
                max_luma: pq_code_for_lut(peak_nits),
            };
            let job_params = self
                .gamut_lut_job
                .as_ref()
                .map(GamutLutJob::params)
                .filter(|job_params| *job_params == params);
            match job_params {
                // A matching job is running: take its result once it lands.
                Some(_) => {
                    let lut = self.gamut_lut_job.as_ref().and_then(GamutLutJob::poll)?;
                    self.gamut_lut_job = None;
                    self.upload_gamut_lut(lut, source, target, target_black_pq, target_peak_pq);
                }
                None => {
                    // First request (or the key changed): spawn generation
                    // and keep the fast path for this frame.
                    self.gamut_lut_job = Some(GamutLutJob::spawn(params));
                    return None;
                }
            }
        }
        Some(
            self.gamut_lut
                .as_ref()
                .expect("gamut lut after upload")
                .2
                .clone(),
        )
    }

    fn upload_gamut_lut(
        &mut self,
        lut: GamutLut,
        source: ColorPrimaries,
        target: ColorPrimaries,
        target_black_pq: u32,
        target_peak_pq: u32,
    ) {
        let size = wgpu::Extent3d {
            width: LUT_SIZE_I as u32,
            height: LUT_SIZE_C as u32,
            depth_or_array_layers: LUT_SIZE_H as u32,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("erika-wgpu-gamut-lut"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // The texels are packed RGB (I, P+0.5, T+0.5); pad to RGBA16F.
        let rgba16 = pack_rgba16f(&lut.texels, 1.0);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba16,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((LUT_SIZE_I * 8) as u32),
                // One "row" is a texel; one image (slice) is the C axis.
                rows_per_image: Some(LUT_SIZE_C as u32),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.gamut_lut = Some((
            (source, target, target_black_pq, target_peak_pq),
            texture,
            view,
        ));
    }

    /// A tiny 1x1x1 view for binding 4 when no LUT is in use.
    fn dummy_lut_view(&mut self) -> Option<wgpu::TextureView> {
        if self.dummy_lut.is_none() {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("erika-wgpu-gamut-lut-dummy"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            self.dummy_lut = Some(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        }
        self.dummy_lut.clone()
    }

    /// Encode and submit a render pass drawing the current video frame into
    /// `target_view`. The caller must have uploaded a frame and the video pipeline
    /// must be initialized.
    fn draw_current_video(
        &mut self,
        target_view: &wgpu::TextureView,
        target_width: u32,
        target_height: u32,
        overlay: Option<&OverlayFrame>,
        danmaku: Option<&DanmakuRenderPlan>,
        output: OutputDescription,
    ) -> Result<usize> {
        let overlay_draws = match overlay {
            Some(frame) if overlay_has_planes(frame) => {
                self.prepare_overlay_draws(frame, output)?
            }
            _ => Vec::new(),
        };
        let danmaku_draws = match danmaku {
            Some(plan) if !plan.is_empty() => self.prepare_danmaku_draws(plan, output)?,
            _ => Vec::new(),
        };
        let (
            native_luma_view,
            native_chroma_view,
            source_is_rgb,
            video_width,
            video_height,
            frame_token,
            mut video_uniforms,
        ) = {
            let video = self
                .current_video
                .as_ref()
                .ok_or_else(|| PlayerError::Renderer("no current video frame".to_string()))?;
            let (luma_view, chroma_view, source_is_rgb) = match &video.textures {
                UploadedVideoTextures::Planar { luma, chroma } => (
                    luma.create_view(&wgpu::TextureViewDescriptor::default()),
                    chroma.create_view(&wgpu::TextureViewDescriptor::default()),
                    false,
                ),
                UploadedVideoTextures::Rgb { texture } => (
                    texture.create_view(&wgpu::TextureViewDescriptor::default()),
                    texture.create_view(&wgpu::TextureViewDescriptor::default()),
                    true,
                ),
            };
            (
                luma_view,
                chroma_view,
                source_is_rgb,
                video.width,
                video.height,
                video.frame_token,
                video.uniforms_for_output(output),
            )
        };
        let logical_video_width = self.video_alpha_mode.logical_width(video_width);
        let viewport = aspect_fit_viewport(
            logical_video_width,
            video_height,
            target_width,
            target_height,
        );
        let upscale_requested = !self.video_alpha_mode.has_alpha()
            && self.upscaler_mode.is_enabled()
            && self.upscaler.status() == WgpuArtCnnStatus::Scalar
            && viewport.width > video_width as f32
            && self.upscaler_failed_frame_token != Some(frame_token);
        let mut upscaled_output = None;
        if upscale_requested {
            let input = if source_is_rgb {
                WgpuArtCnnInput::NonlinearRgb {
                    view: &native_luma_view,
                    luma_coefficients: [
                        video_uniforms.luma_coefficients[0],
                        video_uniforms.luma_coefficients[1],
                        video_uniforms.luma_coefficients[2],
                    ],
                }
            } else {
                WgpuArtCnnInput::PlanarLuma {
                    view: &native_luma_view,
                    p010_le_bytes: video_uniforms.has_p010_byte_planes(),
                }
            };
            let input_kind = input.kind();
            let out_of_memory_scope = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
            let internal_scope = self.device.push_error_scope(wgpu::ErrorFilter::Internal);
            let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let mut compute_encoder =
                self.device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("erika-wgpu-artcnn-encoder"),
                    });
            let encode_result = self.upscaler.encode(
                &self.device,
                &self.queue,
                &mut compute_encoder,
                input,
                video_width,
                video_height,
                Some(frame_token),
            );
            let compute_commands = compute_encoder.finish();
            let submit_compute = matches!(&encode_result, Ok(Some(output)) if !output.cache_hit);
            if submit_compute {
                // Keep all scopes active through queue submission. Some backends
                // defer resource-usage validation until this point, and an ArtCNN
                // failure must remain local to the optional upscaler rather than
                // becoming an uncaptured renderer/device failure.
                self.queue.submit(Some(compute_commands));
            } else {
                drop(compute_commands);
            }
            let validation = pollster::block_on(validation_scope.pop());
            let internal = pollster::block_on(internal_scope.pop());
            let out_of_memory = pollster::block_on(out_of_memory_scope.pop());
            if let Some(error) = validation.or(internal).or(out_of_memory) {
                let failure = self
                    .upscaler
                    .handle_deferred_encode_failure(input_kind, error.to_string());
                self.upscaler_failed_frame_token = Some(frame_token);
                crate::trace::diagnostic(failure.diagnostic_json().to_string());
            } else {
                match encode_result {
                    Ok(Some(output)) => {
                        if let Err(commit_failure) = self.upscaler.commit_encoded_output(&output) {
                            let failure = self.upscaler.handle_deferred_encode_failure(
                                input_kind,
                                commit_failure.to_string(),
                            );
                            self.upscaler_failed_frame_token = Some(frame_token);
                            crate::trace::diagnostic(failure.diagnostic_json().to_string());
                        } else {
                            video_uniforms = match output.input_kind {
                                WgpuArtCnnInputKind::PlanarLuma => {
                                    video_uniforms.packed_d2s_luma_input()
                                }
                                WgpuArtCnnInputKind::NonlinearRgb => {
                                    video_uniforms.packed_d2s_rgb_detail_input()
                                }
                            };
                            self.upscaler_failed_frame_token = None;
                            if !self.upscaler_active_frame_reported {
                                self.upscaler_active_frame_reported = true;
                                let stats = self.upscaler.stats();
                                crate::trace::diagnostic(
                                    serde_json::json!({
                                        "event": "luma_upscaler",
                                        "stage": "frame_encoded",
                                        "renderer": "wgpu",
                                        "requestedMode": format!("{:?}", self.upscaler_mode),
                                        "activeBackend": "scalar_compute",
                                        "inputKind": format!("{:?}", output.input_kind),
                                        "width": video_width,
                                        "height": video_height,
                                        "frameToken": frame_token,
                                        "cacheHit": output.cache_hit,
                                        "encodedTiles": stats.encoded_tiles,
                                        "computeDispatches": stats.compute_dispatches,
                                        "fallbackCount": stats.fallback_count,
                                    })
                                    .to_string(),
                                );
                            }
                            upscaled_output = Some(output);
                        }
                    }
                    Ok(None) => {}
                    Err(failure) => {
                        self.upscaler_failed_frame_token = Some(frame_token);
                        crate::trace::diagnostic(failure.diagnostic_json().to_string());
                    }
                }
            }
        }
        let luma_view = upscaled_output
            .as_ref()
            .map_or(&native_luma_view, |output| &output.view);
        let chroma_view = &native_chroma_view;
        // Create/cache the gamut LUT first: it needs `&mut self`, while the
        // pipeline borrow below is immutable and would otherwise conflict.
        // While the background generation is pending the uniform keeps the
        // fast gamut_compress path and the dummy LUT holds binding 4.
        let gamut_lut_view = self.gamut_lut_view(video_uniforms);
        if gamut_lut_view.is_none() {
            video_uniforms.gamut_lut_enabled = 0;
        }
        let dummy_lut_view = self.dummy_lut_view();
        let gamut_binding = match &gamut_lut_view {
            Some(view) => wgpu::BindingResource::TextureView(view),
            None => wgpu::BindingResource::TextureView(&dummy_lut_view.expect("dummy LUT view")),
        };
        let pipeline = self
            .video_pipeline
            .as_ref()
            .ok_or_else(|| PlayerError::Renderer("video pipeline not initialized".to_string()))?;
        let uniform_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("erika-wgpu-video-uniforms"),
                contents: bytemuck::bytes_of(&video_uniforms),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("erika-wgpu-video-bind-group"),
            layout: &pipeline.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(luma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(chroma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: gamut_binding,
                },
            ],
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("erika-wgpu-video-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("erika-wgpu-video-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(if self.video_alpha_mode.has_alpha() {
                            wgpu::Color::TRANSPARENT
                        } else {
                            wgpu::Color::BLACK
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.set_viewport(
                viewport.x,
                viewport.y,
                viewport.width,
                viewport.height,
                0.0,
                1.0,
            );
            pass.draw(0..3, 0..1);
        }

        if !overlay_draws.is_empty() || !danmaku_draws.is_empty() {
            let overlay_pipeline = self.overlay_pipeline.as_ref().ok_or_else(|| {
                PlayerError::Renderer("overlay pipeline not initialized".to_string())
            })?;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("erika-wgpu-overlay-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Load to preserve the video plane, then alpha-blend overlays.
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&overlay_pipeline.pipeline);
            for draw in &overlay_draws {
                pass.set_bind_group(0, &draw.bind_group, &[draw.dynamic_offset]);
                pass.draw(0..4, 0..1);
            }
            for draw in &danmaku_draws {
                pass.set_bind_group(0, &draw.bind_group, &[draw.dynamic_offset]);
                pass.draw(0..4, 0..1);
            }
        }

        self.queue.submit(Some(encoder.finish()));
        Ok(danmaku_draws.len())
    }

    /// Build per-quad GPU resources for the overlay: straight-RGBA subtitle planes
    /// (mode 0) plus libass alpha coverage bitmaps packed into one R8 atlas (mode 1).
    fn prepare_overlay_draws(
        &self,
        frame: &OverlayFrame,
        output: OutputDescription,
    ) -> Result<Vec<OverlayDraw>> {
        if self.overlay_pipeline.is_none() {
            return Err(PlayerError::Renderer(
                "overlay pipeline not initialized".to_string(),
            ));
        }
        let viewport_w = frame.viewport.width;
        let viewport_h = frame.viewport.height;
        let mut draws = Vec::new();

        for plane in &frame.subtitle_planes {
            if plane.width == 0 || plane.height == 0 {
                continue;
            }
            let expected = plane.width as usize * plane.height as usize * 4;
            if plane.rgba.len() != expected {
                return Err(PlayerError::Renderer(format!(
                    "overlay subtitle plane has {} bytes, expected {expected} for {}x{} RGBA",
                    plane.rgba.len(),
                    plane.width,
                    plane.height
                )));
            }
            let texture = self.create_plane_texture(
                "erika-wgpu-overlay-plane",
                plane.width,
                plane.height,
                wgpu::TextureFormat::Rgba8Unorm,
                &plane.rgba,
                plane.width * 4,
            );
            let (x, y, width, height) = plane.scaled_rect(viewport_w, viewport_h);
            let uniforms = OverlayUniforms::rgba_plane(x, y, width, height, viewport_w, viewport_h)
                .for_output(output);
            draws.push(self.make_overlay_draw(&texture, uniforms));
        }

        self.append_alpha_atlas_draws(frame, viewport_w, viewport_h, output, &mut draws)?;
        Ok(draws)
    }

    fn prepare_danmaku_draws(
        &mut self,
        plan: &DanmakuRenderPlan,
        output: OutputDescription,
    ) -> Result<Vec<OverlayDraw>> {
        if self.overlay_pipeline.is_none() {
            return Err(PlayerError::Renderer(
                "overlay pipeline not initialized".to_string(),
            ));
        }
        let Some(atlas) = plan.atlas.as_ref() else {
            return Ok(Vec::new());
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
        let viewport_w = plan.viewport.width;
        let viewport_h = plan.viewport.height;
        let mut uniforms = Vec::with_capacity(plan.items.len() * 3);
        let (fill_texture, outline_texture) = self.prepare_danmaku_atlas_textures(atlas);
        for item in &plan.items {
            self.append_danmaku_glyph_uniforms(item, viewport_w, viewport_h, output, &mut uniforms);
        }
        self.make_batched_danmaku_draws(&fill_texture, &outline_texture, &uniforms)
    }

    fn prepare_danmaku_atlas_textures(
        &mut self,
        atlas: &DanmakuGlyphAtlas,
    ) -> (wgpu::Texture, wgpu::Texture) {
        if let Some(cache) = &self.danmaku_atlas_cache {
            if cache.can_reuse_for(atlas) {
                return (cache.fill_texture.clone(), cache.outline_texture.clone());
            }
        }
        let incremental = self.danmaku_atlas_cache.as_ref().and_then(|cache| {
            atlas
                .incremental_update_from(cache.version, cache.width, cache.height, cache.stride)
                .map(|update| {
                    (
                        cache.fill_texture.clone(),
                        cache.outline_texture.clone(),
                        update.clone(),
                    )
                })
        });
        if let Some((fill_texture, outline_texture, update)) = incremental {
            self.update_danmaku_atlas_texture(&fill_texture, atlas, &atlas.fill_alpha, &update);
            self.update_danmaku_atlas_texture(
                &outline_texture,
                atlas,
                &atlas.outline_alpha,
                &update,
            );
            if let Some(cache) = &mut self.danmaku_atlas_cache {
                cache.version = atlas.version;
            }
            return (fill_texture, outline_texture);
        }
        let fill_texture = self.create_plane_texture(
            "erika-wgpu-danmaku-fill-atlas",
            atlas.width,
            atlas.height,
            wgpu::TextureFormat::R8Unorm,
            &atlas.fill_alpha,
            atlas.stride as u32,
        );
        let outline_texture = self.create_plane_texture(
            "erika-wgpu-danmaku-outline-atlas",
            atlas.width,
            atlas.height,
            wgpu::TextureFormat::R8Unorm,
            &atlas.outline_alpha,
            atlas.stride as u32,
        );
        self.danmaku_atlas_cache = Some(WgpuDanmakuAtlasCache {
            version: atlas.version,
            width: atlas.width,
            height: atlas.height,
            stride: atlas.stride,
            fill_texture: fill_texture.clone(),
            outline_texture: outline_texture.clone(),
        });
        (fill_texture, outline_texture)
    }

    fn update_danmaku_atlas_texture(
        &self,
        texture: &wgpu::Texture,
        atlas: &DanmakuGlyphAtlas,
        pixels: &[u8],
        update: &DanmakuAtlasUpdate,
    ) {
        let offset = update.y as usize * atlas.stride + update.x as usize;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: update.x,
                    y: update.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &pixels[offset..],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas.stride as u32),
                rows_per_image: Some(update.height),
            },
            wgpu::Extent3d {
                width: update.width,
                height: update.height,
                depth_or_array_layers: 1,
            },
        );
    }

    fn append_danmaku_glyph_uniforms(
        &self,
        item: &DanmakuGlyphInstance,
        viewport_w: u32,
        viewport_h: u32,
        output: OutputDescription,
        draws: &mut Vec<(DanmakuAtlasTexture, OverlayUniforms)>,
    ) {
        if item.shadow_rgba[3] > 0.0 {
            let mut rect = item.rect;
            rect[0] += item.shadow_offset[0];
            rect[1] += item.shadow_offset[1];
            let uniform = OverlayUniforms::alpha_atlas_rect(
                item.shadow_rgba,
                rect,
                item.tex_rect,
                viewport_w,
                viewport_h,
            )
            .for_output(output);
            draws.push((DanmakuAtlasTexture::Outline, uniform));
        }
        if item.outline_rgba[3] > 0.0 {
            let uniform = OverlayUniforms::alpha_atlas_rect(
                item.outline_rgba,
                item.rect,
                item.tex_rect,
                viewport_w,
                viewport_h,
            )
            .for_output(output);
            draws.push((DanmakuAtlasTexture::Outline, uniform));
        }
        let uniform = OverlayUniforms::alpha_atlas_rect(
            item.color_rgba,
            item.rect,
            item.tex_rect,
            viewport_w,
            viewport_h,
        )
        .for_output(output);
        draws.push((DanmakuAtlasTexture::Fill, uniform));
    }

    fn make_batched_danmaku_draws(
        &self,
        fill_texture: &wgpu::Texture,
        outline_texture: &wgpu::Texture,
        uniforms: &[(DanmakuAtlasTexture, OverlayUniforms)],
    ) -> Result<Vec<OverlayDraw>> {
        if uniforms.is_empty() {
            return Ok(Vec::new());
        }
        let uniform_size = std::mem::size_of::<OverlayUniforms>();
        let alignment = self
            .device
            .limits()
            .min_uniform_buffer_offset_alignment
            .max(1) as usize;
        let stride = uniform_size.div_ceil(alignment) * alignment;
        let buffer_size = stride.checked_mul(uniforms.len()).ok_or_else(|| {
            PlayerError::Renderer("danmaku dynamic uniform buffer size overflow".to_string())
        })?;
        let mut bytes = vec![0u8; buffer_size];
        for (index, (_, uniform)) in uniforms.iter().enumerate() {
            let start = index * stride;
            bytes[start..start + uniform_size].copy_from_slice(bytemuck::bytes_of(uniform));
        }
        // TODO(perf): retain a growable per-frame uniform buffer and update it via
        // `Queue::write_buffer` (or a small ring) instead of allocating a new GPU
        // buffer for every danmaku frame. Keep `stride` device-aligned when reusing it.
        let uniform_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("erika-wgpu-danmaku-dynamic-uniforms"),
                contents: &bytes,
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let make_bind_group = |label: &'static str, texture: &wgpu::Texture| {
            let pipeline = self
                .overlay_pipeline
                .as_ref()
                .expect("overlay pipeline initialized");
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &pipeline.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &uniform_buffer,
                            offset: 0,
                            size: std::num::NonZeroU64::new(uniform_size as u64),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
                    },
                ],
            })
        };
        let fill_bind_group = make_bind_group("erika-wgpu-danmaku-fill-bind-group", fill_texture);
        let outline_bind_group =
            make_bind_group("erika-wgpu-danmaku-outline-bind-group", outline_texture);
        uniforms
            .iter()
            .enumerate()
            .map(|(index, (texture, _))| {
                let offset = index.checked_mul(stride).ok_or_else(|| {
                    PlayerError::Renderer("danmaku dynamic uniform offset overflow".to_string())
                })?;
                let dynamic_offset = u32::try_from(offset).map_err(|_| {
                    PlayerError::Renderer(format!(
                        "danmaku dynamic uniform offset exceeds u32: {offset}"
                    ))
                })?;
                let (bind_group, texture) = match texture {
                    DanmakuAtlasTexture::Fill => (fill_bind_group.clone(), fill_texture.clone()),
                    DanmakuAtlasTexture::Outline => {
                        (outline_bind_group.clone(), outline_texture.clone())
                    }
                };
                Ok(OverlayDraw {
                    bind_group,
                    dynamic_offset,
                    _texture: texture,
                    _uniform: uniform_buffer.clone(),
                })
            })
            .collect()
    }

    /// Pack libass alpha coverage bitmaps horizontally into one R8 atlas and add a
    /// mode-1 (coverage tinted by the bitmap's color) draw per placement. Mirrors the
    /// Metal `prepare_overlay_alpha_atlas` packing.
    fn append_alpha_atlas_draws(
        &self,
        frame: &OverlayFrame,
        viewport_w: u32,
        viewport_h: u32,
        output: OutputDescription,
        draws: &mut Vec<OverlayDraw>,
    ) -> Result<()> {
        let bitmaps = &frame.subtitle_alpha_planes;
        let mut atlas_width = 0usize;
        let mut atlas_height = 0usize;
        for bitmap in bitmaps {
            if bitmap.placement.width == 0 || bitmap.placement.height == 0 {
                continue;
            }
            atlas_width += bitmap.placement.width as usize;
            atlas_height = atlas_height.max(bitmap.placement.height as usize);
        }
        if atlas_width == 0 || atlas_height == 0 {
            return Ok(());
        }

        let mut pixels = vec![0u8; atlas_width * atlas_height];
        let mut cursor_x = 0usize;
        let mut placements: Vec<(usize, usize)> = Vec::new();
        for (index, bitmap) in bitmaps.iter().enumerate() {
            let bw = bitmap.placement.width as usize;
            let bh = bitmap.placement.height as usize;
            if bw == 0 || bh == 0 {
                continue;
            }
            if !bitmap.is_valid() {
                return Err(PlayerError::Renderer(format!(
                    "overlay alpha bitmap has {} bytes, expected at least {} for {}x{} stride {}",
                    bitmap.alpha.len(),
                    bitmap.required_len(),
                    bitmap.placement.width,
                    bitmap.placement.height,
                    bitmap.stride
                )));
            }
            for row in 0..bh {
                let src = row * bitmap.stride;
                let dst = row * atlas_width + cursor_x;
                pixels[dst..dst + bw].copy_from_slice(&bitmap.alpha[src..src + bw]);
            }
            placements.push((index, cursor_x));
            cursor_x += bw;
        }

        let atlas = self.create_plane_texture(
            "erika-wgpu-overlay-atlas",
            atlas_width as u32,
            atlas_height as u32,
            wgpu::TextureFormat::R8Unorm,
            &pixels,
            atlas_width as u32,
        );
        for (index, atlas_x) in placements {
            let bitmap = &bitmaps[index];
            let uniforms = OverlayUniforms::alpha_atlas(
                bitmap.color_rgba,
                bitmap.placement.x,
                bitmap.placement.y,
                bitmap.placement.width,
                bitmap.placement.height,
                atlas_x as u32,
                atlas_width as u32,
                atlas_height as u32,
                viewport_w,
                viewport_h,
            )
            .for_output(output);
            draws.push(self.make_overlay_draw(&atlas, uniforms));
        }
        Ok(())
    }

    /// Create the bind group (uniform + texture + sampler) for one overlay quad,
    /// retaining the texture and uniform buffer alongside it. The overlay pipeline
    /// must be initialized.
    fn make_overlay_draw(&self, texture: &wgpu::Texture, uniforms: OverlayUniforms) -> OverlayDraw {
        let pipeline = self
            .overlay_pipeline
            .as_ref()
            .expect("overlay pipeline initialized");
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("erika-wgpu-overlay-uniforms"),
                contents: bytemuck::bytes_of(&uniforms),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("erika-wgpu-overlay-bind-group"),
            layout: &pipeline.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &uniform,
                        offset: 0,
                        size: std::num::NonZeroU64::new(
                            std::mem::size_of::<OverlayUniforms>() as u64
                        ),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
                },
            ],
        });
        OverlayDraw {
            bind_group,
            dynamic_offset: 0,
            _texture: texture.clone(),
            _uniform: uniform,
        }
    }

    fn create_plane_texture(
        &self,
        label: &str,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        data: &[u8],
        bytes_per_row: u32,
    ) -> wgpu::Texture {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.write_plane_texture(&texture, data, bytes_per_row);
        texture
    }

    fn write_plane_texture(&self, texture: &wgpu::Texture, data: &[u8], bytes_per_row: u32) {
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(texture.height()),
            },
            wgpu::Extent3d {
                width: texture.width(),
                height: texture.height(),
                depth_or_array_layers: 1,
            },
        );
    }

    fn ensure_video_pipeline(&mut self, format: wgpu::TextureFormat) {
        // The render pipeline's color target format must match the render pass
        // attachment, so rebuild if the target format changed (offscreen Rgba8Unorm
        // vs the surface's format).
        if self
            .video_pipeline
            .as_ref()
            .is_some_and(|video| video.format == format)
        {
            return;
        }
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("erika-wgpu-video-shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("wgpu_video.wgsl").into()),
            });
        let texture_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bind_group_layout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("erika-wgpu-video-bgl"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        texture_entry(1),
                        texture_entry(2),
                        wgpu::BindGroupLayoutEntry {
                            binding: 3,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 4,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D3,
                                multisampled: false,
                            },
                            count: None,
                        },
                    ],
                });
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("erika-wgpu-video-layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("erika-wgpu-video-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("erika_video_vertex"),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("erika_video_fragment"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                multiview_mask: None,
                cache: None,
            });
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("erika-wgpu-video-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        self.video_pipeline = Some(VideoPipeline {
            pipeline,
            bind_group_layout,
            sampler,
            format,
        });
    }

    fn ensure_overlay_pipeline(&mut self, format: wgpu::TextureFormat) {
        if self
            .overlay_pipeline
            .as_ref()
            .is_some_and(|overlay| overlay.format == format)
        {
            return;
        }
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("erika-wgpu-overlay-shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("wgpu_overlay.wgsl").into()),
            });
        let bind_group_layout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("erika-wgpu-overlay-bgl"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: true,
                                min_binding_size: std::num::NonZeroU64::new(std::mem::size_of::<
                                    OverlayUniforms,
                                >(
                                )
                                    as u64),
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                });
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("erika-wgpu-overlay-layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("erika-wgpu-overlay-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("erika_overlay_vertex"),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("erika_overlay_fragment"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        // Straight-alpha blending, matching the Metal overlay pipeline.
                        blend: Some(wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::SrcAlpha,
                                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                operation: wgpu::BlendOperation::Add,
                            },
                            alpha: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                operation: wgpu::BlendOperation::Add,
                            },
                        }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                multiview_mask: None,
                cache: None,
            });
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("erika-wgpu-overlay-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        self.overlay_pipeline = Some(OverlayPipeline {
            pipeline,
            bind_group_layout,
            sampler,
            format,
        });
    }

    fn render_surface_clear(&mut self, color: WgpuClearColor) -> Result<()> {
        if self.surface.is_none() {
            return Err(PlayerError::Renderer(
                "no wgpu surface attached".to_string(),
            ));
        }
        let SurfaceFrame::Texture {
            texture: frame,
            reconfigure_after_present,
        } = self.acquire_surface_frame()?
        else {
            return Ok(());
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("erika-wgpu-surface-encoder"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("erika-wgpu-surface-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(color.to_wgpu()),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        if reconfigure_after_present {
            self.reconfigure_surface();
        }
        self.stats.rendered_frames += 1;
        Ok(())
    }

    fn create_attached_surface(&self, handle: WgpuSurfaceHandle) -> Result<AttachedSurface> {
        #[cfg(not(any(
            target_os = "android",
            target_os = "macos",
            any(target_os = "ios", target_os = "tvos"),
            target_os = "windows",
            target_os = "linux",
            target_env = "ohos"
        )))]
        {
            return Err(PlayerError::Renderer(format!(
                "wgpu surface kind {:?} is not wired on this platform",
                handle.kind
            )));
        }

        #[cfg(any(
            target_os = "android",
            target_os = "macos",
            any(target_os = "ios", target_os = "tvos"),
            target_os = "windows",
            target_os = "linux",
            target_env = "ohos"
        ))]
        {
            #[cfg(target_os = "android")]
            let android_window;

            // SAFETY: the embedder owns the platform handle for the attachment. On
            // Android we additionally acquire an ANativeWindow reference retained by
            // `AttachedSurface`, so the raw handle outlives the wgpu surface.
            let target = match handle.kind {
                #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
                WgpuSurfaceKind::XlibWindow | WgpuSurfaceKind::WaylandSurface => {
                    linux_surface_target(handle)?
                }
                #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
                WgpuSurfaceKind::MacOsCaMetalLayer => {
                    wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(handle.raw_window as *mut c_void)
                }
                #[cfg(target_os = "windows")]
                WgpuSurfaceKind::WindowsHwnd => {
                    let hwnd = NonZeroIsize::new(handle.raw_window as isize).ok_or_else(|| {
                        PlayerError::Renderer(
                            "wgpu Windows HWND surface handle is null".to_string(),
                        )
                    })?;
                    let mut window = wgpu::rwh::Win32WindowHandle::new(hwnd);
                    window.hinstance = NonZeroIsize::new(handle.raw_display as isize);
                    wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: Some(wgpu::rwh::RawDisplayHandle::Windows(
                            wgpu::rwh::WindowsDisplayHandle::new(),
                        )),
                        raw_window_handle: wgpu::rwh::RawWindowHandle::Win32(window),
                    }
                }
                #[cfg(target_os = "android")]
                WgpuSurfaceKind::AndroidNativeWindow => {
                    let raw_window =
                        NonNull::new(handle.raw_window as *mut c_void).ok_or_else(|| {
                            PlayerError::Renderer(
                                "wgpu Android ANativeWindow surface handle is null".to_string(),
                            )
                        })?;
                    // SAFETY: attach_surface's contract requires a live ANativeWindow.
                    let owned_window = unsafe { AndroidNativeWindow::acquire(raw_window) };
                    let window_handle =
                        wgpu::rwh::AndroidNdkWindowHandle::new(owned_window.as_non_null());
                    android_window = Some(owned_window);
                    wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: Some(wgpu::rwh::RawDisplayHandle::Android(
                            wgpu::rwh::AndroidDisplayHandle::new(),
                        )),
                        raw_window_handle: wgpu::rwh::RawWindowHandle::AndroidNdk(window_handle),
                    }
                }
                #[cfg(target_env = "ohos")]
                WgpuSurfaceKind::OhosNativeWindow => {
                    let raw_window =
                        NonNull::new(handle.raw_window as *mut c_void).ok_or_else(|| {
                            PlayerError::Renderer(
                                "wgpu OpenHarmony OHNativeWindow surface handle is null"
                                    .to_string(),
                            )
                        })?;
                    let window_handle = wgpu::rwh::OhosNdkWindowHandle::new(raw_window);
                    wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: Some(wgpu::rwh::RawDisplayHandle::Ohos(
                            wgpu::rwh::OhosDisplayHandle::new(),
                        )),
                        raw_window_handle: wgpu::rwh::RawWindowHandle::OhosNdk(window_handle),
                    }
                }
                other => {
                    return Err(PlayerError::Renderer(format!(
                        "wgpu surface kind {other:?} is not wired yet"
                    )));
                }
            };
            let surface =
                unsafe { self._instance.create_surface_unsafe(target) }.map_err(|error| {
                    PlayerError::Renderer(format!("wgpu surface creation failed: {error}"))
                })?;
            let caps = surface.get_capabilities(&self.adapter);
            let adapter_backend = self.adapter.get_info().backend;
            #[cfg(not(all(target_os = "linux", not(target_env = "ohos"))))]
            let selection = select_wgpu_surface_output(
                self.output_mode,
                handle.output_capabilities,
                adapter_backend,
                &caps.formats,
            );
            #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
            let selection = select_linux_surface_output(
                self.output_mode,
                self.current_video
                    .as_ref()
                    .and_then(|v| v.source_color)
                    .is_some_and(|s| s.is_hdr()),
                adapter_backend,
                &caps.formats,
            );
            let selection = selection.ok_or_else(|| {
                PlayerError::Renderer(
                    "wgpu surface exposes no usable SDR presentation format".to_string(),
                )
            })?;
            // The GTK platform thread also dispatches Wayland parent commits.
            // EGL FIFO can wait forever for an occluded subsurface callback on
            // that same thread. GTK's tick timer already paces this GL path.
            let preferred_present_mode = if cfg!(all(target_os = "linux", not(target_env = "ohos")))
            {
                if adapter_backend == wgpu::Backend::Vulkan
                    && caps.present_modes.contains(&wgpu::PresentMode::Mailbox)
                {
                    wgpu::PresentMode::Mailbox
                } else {
                    wgpu::PresentMode::Immediate
                }
            } else {
                wgpu::PresentMode::Fifo
            };
            let present_mode = caps
                .present_modes
                .iter()
                .copied()
                .find(|mode| *mode == preferred_present_mode)
                .or_else(|| caps.present_modes.first().copied())
                .ok_or_else(|| {
                    PlayerError::Renderer("wgpu surface exposes no present modes".to_string())
                })?;
            let preferred_alpha_mode = if self.video_alpha_mode.has_alpha() {
                wgpu::CompositeAlphaMode::PreMultiplied
            } else {
                wgpu::CompositeAlphaMode::Opaque
            };
            let alpha_mode = caps
                .alpha_modes
                .iter()
                .copied()
                .find(|mode| *mode == preferred_alpha_mode)
                .or_else(|| caps.alpha_modes.first().copied())
                .ok_or_else(|| {
                    PlayerError::Renderer("wgpu surface exposes no alpha modes".to_string())
                })?;
            let (width, height) = handle.metrics().physical_size();
            let mut config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: selection.format,
                width,
                height,
                present_mode,
                desired_maximum_frame_latency: 2,
                alpha_mode,
                view_formats: vec![],
            };
            surface.configure(&self.device, &config);
            let mut output = selection.output;
            if output.extended_linear && !cfg!(all(target_os = "linux", not(target_env = "ohos"))) {
                output = OutputDescription::extended_linear(effective_extended_linear_headroom(
                    self.output_mode,
                    handle.output_capabilities,
                    self.output_headroom,
                ));
            }
            let mut fallback_reason = selection.fallback_reason;
            #[cfg(target_os = "android")]
            let mut data_space_verification = None;
            #[cfg(target_os = "android")]
            let mut data_space_failure = false;
            #[cfg(target_os = "android")]
            if output.extended_linear {
                let verification = android_window
                    .as_ref()
                    .expect("Android surface retains an ANativeWindow")
                    .ensure_scrgb_linear_data_space();
                match verification {
                    Ok(verification) => data_space_verification = Some(verification),
                    Err(error) => {
                        data_space_failure = true;
                        fallback_reason = match error.kind {
                            AndroidDataSpaceErrorKind::ApiUnavailable => {
                                OutputFallbackReason::NativeWindowDataSpaceApiUnavailable
                            }
                            AndroidDataSpaceErrorKind::VerificationFailed => {
                                OutputFallbackReason::ScrgbDataSpaceVerificationFailed
                            }
                        };
                        crate::trace::diagnostic(
                            serde_json::json!({
                                "event": "video_output_mode",
                                "stage": "native_dataspace_verification_failed",
                                "renderer": "wgpu",
                                "requested": "extended_linear",
                                "backend": format!("{adapter_backend:?}"),
                                "surfaceFormat": format!("{:?}", config.format),
                                "expectedDataSpace": "SCRGB_LINEAR",
                                "reason": error.to_string(),
                                "fallback": "sdr",
                            })
                            .to_string(),
                        );
                        config.format = selection.sdr_format;
                        surface.configure(&self.device, &config);
                        output = OutputDescription::sdr();
                    }
                }
            }
            #[cfg(target_os = "android")]
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "video_output_mode",
                    "stage": if output.extended_linear { "surface_active" } else { "surface_fallback" },
                    "renderer": "wgpu",
                    "requestedMode": format!("{:?}", self.output_mode),
                    "activeEncoding": if output.extended_linear { "android_extended_linear_scrgb" } else { "sdr_srgb" },
                    "backend": format!("{adapter_backend:?}"),
                    "surfaceFormat": format!("{:?}", config.format),
                    "colorSpace": output.color_space.label(),
                    "surfaceFormatClass": output.surface_format.label(),
                    "requestedHeadroom": self.output_mode.headroom(),
                    "surfaceDesiredHeadroom": handle.output_capabilities.desired_headroom,
                    "reportedHeadroom": self.output_headroom.headroom,
                    "reportedHeadroomKnown": self.output_headroom.known,
                    "effectiveContentHeadroom": output.target.edr_headroom,
                    "surfaceExtendedLinearCapable": handle.output_capabilities.extended_linear,
                    "surfaceDirectComposition": handle.output_capabilities.direct_composition,
                    "availableFormats": caps.formats.iter().map(|format| format!("{format:?}")).collect::<Vec<_>>(),
                    "dataSpaceBefore": data_space_verification.map(|value| value.before),
                    "dataSpaceAfter": data_space_verification.map(|value| value.after),
                    "dataSpaceCorrected": data_space_verification.is_some_and(|value| value.corrected),
                    "fallback": fallback_reason != OutputFallbackReason::None,
                    "reasonCode": fallback_reason as i32,
                    "reason": fallback_reason.label(),
                })
                .to_string(),
            );
            Ok(AttachedSurface {
                surface,
                config,
                sdr_format: selection.sdr_format,
                output,
                fallback_reason,
                #[cfg(target_os = "android")]
                data_space_failure,
                #[cfg(not(target_os = "android"))]
                data_space_failure: false,
                #[cfg(target_os = "android")]
                native_data_space: data_space_verification.map_or(-1, |value| value.after),
                #[cfg(not(target_os = "android"))]
                native_data_space: -1,
                handle,
                #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
                formats: caps.formats,
                #[cfg(target_os = "android")]
                _android_window: android_window,
            })
        }
    }

    fn acquire_surface_frame(&mut self) -> Result<SurfaceFrame> {
        for recovery_attempt in 0..2 {
            let status = self
                .surface
                .as_ref()
                .ok_or_else(|| PlayerError::Renderer("no wgpu surface attached".to_string()))?
                .surface
                .get_current_texture();
            match status {
                wgpu::CurrentSurfaceTexture::Success(texture) => {
                    return Ok(SurfaceFrame::Texture {
                        texture,
                        reconfigure_after_present: false,
                    });
                }
                wgpu::CurrentSurfaceTexture::Suboptimal(texture) => {
                    return Ok(SurfaceFrame::Texture {
                        texture,
                        reconfigure_after_present: true,
                    });
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return Ok(SurfaceFrame::Skipped);
                }
                wgpu::CurrentSurfaceTexture::Outdated => {
                    #[cfg(target_os = "android")]
                    crate::trace::diagnostic(
                        serde_json::json!({
                            "event": "android_gpu_recovery",
                            "stage": "surface_outdated",
                            "backendCandidate": self.android_backend_candidate_label(),
                            "surfaceRecoveryAttempt": recovery_attempt + 1,
                            "surfaceRecoveryAttemptLimit": 2,
                            "action": "reconfigure_surface_on_current_device",
                        })
                        .to_string(),
                    );
                    self.reconfigure_surface();
                }
                wgpu::CurrentSurfaceTexture::Lost => {
                    #[cfg(target_os = "android")]
                    crate::trace::diagnostic(
                        serde_json::json!({
                            "event": "android_gpu_recovery",
                            "stage": "surface_lost",
                            "backendCandidate": self.android_backend_candidate_label(),
                            "surfaceRecoveryAttempt": recovery_attempt + 1,
                            "surfaceRecoveryAttemptLimit": 2,
                            "action": "recreate_surface_on_current_device",
                        })
                        .to_string(),
                    );
                    self.recreate_surface()?;
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    #[cfg(target_os = "android")]
                    crate::trace::diagnostic(
                        serde_json::json!({
                            "event": "android_gpu_recovery",
                            "stage": "surface_validation_failed",
                            "backendCandidate": self.android_backend_candidate_label(),
                            "surfaceRecoveryAttempt": recovery_attempt + 1,
                            "surfaceRecoveryAttemptLimit": 2,
                            "action": "escalate_to_renderer_rebuild",
                        })
                        .to_string(),
                    );
                    return Err(PlayerError::Renderer(
                        "wgpu surface acquisition failed validation".to_string(),
                    ));
                }
            }
        }
        #[cfg(target_os = "android")]
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "android_gpu_recovery",
                "stage": "surface_recovery_exhausted_on_current_device",
                "backendCandidate": self.android_backend_candidate_label(),
                "surfaceRecoveryAttemptLimit": 2,
                "action": "escalate_to_renderer_rebuild",
            })
            .to_string(),
        );
        Err(PlayerError::Renderer(
            "wgpu surface remained outdated or lost after recovery".to_string(),
        ))
    }

    fn recreate_surface(&mut self) -> Result<()> {
        let handle = self
            .surface
            .as_ref()
            .ok_or_else(|| PlayerError::Renderer("no wgpu surface attached".to_string()))?
            .handle;
        let replacement = self.create_attached_surface(handle)?;
        self.stats.surface_width = replacement.config.width;
        self.stats.surface_height = replacement.config.height;
        self.observe_attached_output(replacement.output_state(), true);
        self.surface = Some(replacement);
        Ok(())
    }

    fn reconfigure_surface(&mut self) {
        let output_state = {
            let Some(attached) = self.surface.as_mut() else {
                return;
            };
            let new_fallback =
                configure_attached_surface(&self.device, attached, "reconfigure_surface");
            (attached.output_state(), new_fallback)
        };
        self.observe_attached_output(output_state.0, output_state.1);
    }

    fn configure_surface(&mut self, width: u32, height: u32) {
        let (surface_width, surface_height, output_state, new_fallback) = {
            let Some(attached) = self.surface.as_mut() else {
                return;
            };
            attached.config.width = width.max(1);
            attached.config.height = height.max(1);
            let new_fallback = configure_attached_surface(&self.device, attached, "resize_surface");
            (
                attached.config.width,
                attached.config.height,
                attached.output_state(),
                new_fallback,
            )
        };
        self.stats.surface_width = surface_width;
        self.stats.surface_height = surface_height;
        self.observe_attached_output(output_state, new_fallback);
    }
}

// The host retains the connection until successful detach or destruction.
// WgpuRenderer drops every display-bound EGL object before detach returns.
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
#[derive(Debug)]
struct LinuxDisplayHandle(WgpuSurfaceHandle);

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
impl wgpu::rwh::HasDisplayHandle for LinuxDisplayHandle {
    fn display_handle(
        &self,
    ) -> std::result::Result<wgpu::rwh::DisplayHandle<'_>, wgpu::rwh::HandleError> {
        match linux_surface_target(self.0).map_err(|_| wgpu::rwh::HandleError::Unavailable)? {
            wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(raw),
                ..
            } => {
                // SAFETY: attach's native handle contract covers this instance.
                Ok(unsafe { wgpu::rwh::DisplayHandle::borrow_raw(raw) })
            }
            _ => Err(wgpu::rwh::HandleError::Unavailable),
        }
    }
}

/// The host retains both native handles until the surface is detached.
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
fn linux_surface_target(handle: WgpuSurfaceHandle) -> Result<wgpu::SurfaceTargetUnsafe> {
    let display = NonNull::new(handle.raw_display as *mut c_void)
        .ok_or_else(|| PlayerError::Renderer("Linux display handle is null".to_string()))?;
    if handle.raw_window == 0 {
        return Err(PlayerError::Renderer(
            "Linux window handle is null".to_string(),
        ));
    }
    let (raw_display_handle, raw_window_handle) = match handle.kind {
        WgpuSurfaceKind::XlibWindow => (
            wgpu::rwh::RawDisplayHandle::Xlib(wgpu::rwh::XlibDisplayHandle::new(Some(display), 0)),
            wgpu::rwh::RawWindowHandle::Xlib(wgpu::rwh::XlibWindowHandle::new(
                handle.raw_window as _,
            )),
        ),
        WgpuSurfaceKind::WaylandSurface => (
            wgpu::rwh::RawDisplayHandle::Wayland(wgpu::rwh::WaylandDisplayHandle::new(display)),
            wgpu::rwh::RawWindowHandle::Wayland(wgpu::rwh::WaylandWindowHandle::new(
                NonNull::new(handle.raw_window as *mut c_void).expect("validated non-null window"),
            )),
        ),
        _ => {
            return Err(PlayerError::Renderer(
                "expected an Xlib or Wayland surface".to_string(),
            ));
        }
    };
    Ok(wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(raw_display_handle),
        raw_window_handle,
    })
}

fn configure_attached_surface(
    device: &wgpu::Device,
    attached: &mut AttachedSurface,
    operation: &'static str,
) -> bool {
    attached.surface.configure(device, &attached.config);
    #[cfg(target_os = "android")]
    if attached.output.extended_linear {
        match attached
            ._android_window
            .as_ref()
            .expect("Android surface retains an ANativeWindow")
            .ensure_scrgb_linear_data_space()
        {
            Ok(verification) => attached.native_data_space = verification.after,
            Err(error) => {
                let fallback_reason = match error.kind {
                    AndroidDataSpaceErrorKind::ApiUnavailable => {
                        OutputFallbackReason::NativeWindowDataSpaceApiUnavailable
                    }
                    AndroidDataSpaceErrorKind::VerificationFailed => {
                        OutputFallbackReason::ScrgbDataSpaceVerificationFailed
                    }
                };
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "video_output_mode",
                        "stage": "native_dataspace_revalidation_failed",
                        "renderer": "wgpu",
                        "operation": operation,
                        "surfaceFormat": format!("{:?}", attached.config.format),
                        "expectedDataSpace": "SCRGB_LINEAR",
                        "reason": error.to_string(),
                        "reasonCode": fallback_reason as i32,
                        "reasonLabel": fallback_reason.label(),
                        "fallback": "sdr",
                    })
                    .to_string(),
                );
                attached.config.format = attached.sdr_format;
                attached.output = OutputDescription::sdr();
                attached.fallback_reason = fallback_reason;
                attached.data_space_failure = true;
                attached.native_data_space = -1;
                attached.surface.configure(device, &attached.config);
                return true;
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = operation;
    false
}

type PresentationViewport = PresentationRect;

fn aspect_fit_viewport(
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> PresentationViewport {
    PresentationLayout::aspect_fit(source_width, source_height, target_width, target_height)
        .presentation_rect()
}

#[cfg(target_os = "android")]
fn android_wgpu_drop_poll_type() -> wgpu::PollType {
    wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(ANDROID_WGPU_DROP_POLL_TIMEOUT),
    }
}

#[cfg(target_os = "android")]
impl Drop for WgpuRenderer {
    fn drop(&mut self) {
        if let Err(error) = self.device.poll(android_wgpu_drop_poll_type()) {
            let timeout = matches!(&error, wgpu::PollError::Timeout);
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "wgpu_renderer",
                    "stage": if timeout { "drop_poll_timeout" } else { "drop_poll_error" },
                    "errorKind": if timeout { "timeout" } else { "poll_error" },
                    "timeoutMs": ANDROID_WGPU_DROP_POLL_TIMEOUT.as_millis() as u64,
                    "message": error.to_string(),
                    "reason": "Android renderer teardown uses a bounded GPU wait so lifecycle destruction cannot block indefinitely",
                })
                .to_string(),
            );
        }
    }
}

impl RendererBackend for WgpuRenderer {
    fn attach_surface(&mut self, surface: PlatformSurface) -> Result<()> {
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        if let PlatformSurface::FlutterTexture(handle) = surface {
            if handle.kind != crate::core::FlutterTextureKind::LinuxTextureRegistrar {
                return Err(PlayerError::Renderer(
                    "expected Linux Flutter texture".into(),
                ));
            }
            self.detach_surface()?;
            self.flutter_metrics = Some(handle.metrics);
            (self.stats.surface_width, self.stats.surface_height) = handle.metrics.physical_size();
            self.stats.attached = true;
            return Ok(());
        }
        self.flutter_metrics = None;
        self.flutter_frame = None;
        let PlatformSurface::Wgpu(handle) = surface else {
            return Err(PlayerError::Renderer(
                "non-wgpu surface cannot be attached to WgpuRenderer".to_string(),
            ));
        };

        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        {
            linux_surface_target(handle)?;
            if self.linux_display != Some((handle.kind, handle.raw_display)) {
                // EGL must be created against the actual X11/Wayland display,
                // and adapter selection must consider presentation support.
                let mut replacement = self.for_linux_display(Some(handle))?;
                replacement.attach_surface(surface)?;
                *self = replacement;
                return Ok(());
            }
        }
        let attached = self.create_attached_surface(handle)?;
        self.stats.surface_width = attached.config.width;
        self.stats.surface_height = attached.config.height;
        self.stats.attached = true;
        self.observe_attached_output(attached.output_state(), true);
        self.surface = Some(attached);
        Ok(())
    }

    fn detach_surface(&mut self) -> Result<()> {
        self.flutter_metrics = None;
        self.flutter_frame = None;
        // Release native window references even when headless device recovery
        // fails; hosts are allowed to destroy their surface after detaching.
        self.surface = None;
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        if self.linux_display.is_some() {
            // A headless replacement retains the paused image but releases
            // the borrowed display, so the host may immediately close it.
            *self = self.for_linux_display(None)?;
        }
        self.surface = None;
        self.stats.attached = false;
        self.observe_detached_output();
        Ok(())
    }

    fn resize_surface(&mut self, metrics: crate::core::SurfaceMetrics) -> Result<()> {
        if self.flutter_metrics.is_some() {
            self.flutter_metrics = Some(metrics);
            self.flutter_frame = None;
            (self.stats.surface_width, self.stats.surface_height) = metrics.physical_size();
            return Ok(());
        }
        let current_size = self
            .surface
            .as_ref()
            .ok_or_else(|| PlayerError::Renderer("no wgpu surface attached".to_string()))?
            .handle
            .metrics()
            .physical_size();
        let (surface_width, surface_height) = metrics.physical_size();
        if current_size != (surface_width, surface_height) {
            self.configure_surface(surface_width, surface_height);
        }
        if let Some(attached) = self.surface.as_mut() {
            attached.handle.resize(metrics);
        }
        Ok(())
    }

    fn render_test_frame(&mut self, time_seconds: f64) -> Result<()> {
        let color = WgpuClearColor::animated(time_seconds);
        if self.surface.is_some() {
            self.render_surface_clear(color)
        } else {
            // No surface: exercise the GPU path headlessly and count it as a frame.
            self.clear_offscreen(16, 16, color)?;
            self.stats.rendered_frames += 1;
            Ok(())
        }
    }

    fn upload_player_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        self.update_linux_output_for_source(source_color_for_player_frame(frame).is_hdr())?;
        #[cfg(target_os = "android")]
        if frame.decode_backend == DecoderBackend::MediaCodec && frame.frame.is_mediacodec() {
            return self.upload_android_mediacodec_frame(frame);
        }
        #[cfg(target_env = "ohos")]
        if frame.decode_backend == DecoderBackend::AvCodec && frame.frame.is_ohos_avcodec_surface()
        {
            return self.upload_ohos_avcodec_frame(frame);
        }
        let hardware_frame = frame.frame.has_hw_frames_context();
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        if hardware_frame
            && matches!(
                frame.decode_backend,
                DecoderBackend::Cuda | DecoderBackend::Vaapi
            )
            && self.upload_linux_hardware_frame(frame)?
        {
            return Ok(());
        }
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        if std::env::var("ERIKA_REQUIRE_GPU_FRAMES").as_deref() == Ok("1")
            || std::env::var("ERIKA_REQUIRE_ZERO_COPY").as_deref() == Ok("1")
        {
            return Err(PlayerError::Renderer(
                "GPU-only decoded frames / direct zero-copy were required, but this frame needs a CPU upload".into(),
            ));
        }
        // NVDEC/VA-API still perform decode on the GPU. Transfer the decoded
        // planes for portable wgpu upload until native interop is available.
        // Keep this distinct from software decode and zero-copy in the stats.
        let downloaded = if hardware_frame
            && matches!(
                frame.decode_backend,
                DecoderBackend::Cuda | DecoderBackend::Vaapi
            ) {
            Some(
                frame
                    .frame
                    .decoded_frame()
                    .ok_or_else(|| PlayerError::Renderer("expected FFmpeg hardware frame".into()))?
                    .transfer_to_system_memory()
                    .map_err(|error| PlayerError::Renderer(error.to_string()))?,
            )
        } else {
            None
        };
        let planes = downloaded
            .as_ref()
            .and_then(|value| value.to_planar_frame())
            .or_else(|| frame.frame.to_planar_frame());
        let planar = if let Some(planar) = planes {
            match frame.decode_backend {
                DecoderBackend::Software => self.stats.software_video_frames += 1,
                DecoderBackend::VideoToolbox
                | DecoderBackend::D3d11va
                | DecoderBackend::Cuda
                | DecoderBackend::Vaapi
                | DecoderBackend::MediaCodec
                | DecoderBackend::AvCodec => {
                    self.stats.hardware_video_frames += 1;
                    self.stats.cpu_video_frame_fallbacks += 1;
                    if !self.cpu_video_frame_fallback_reported {
                        self.cpu_video_frame_fallback_reported = true;
                        crate::trace::diagnostic(
                            serde_json::json!({
                                "event": "video_frame_import",
                                "stage": "cpu_upload_fallback",
                                "decodeBackend": frame.decode_backend.as_str(),
                                "pixelFormat": frame.frame.pixel_format(),
                                "lineSizes": frame.frame.line_sizes(),
                                "fallbackCount": self.stats.cpu_video_frame_fallbacks,
                                "reason": "hardware decoder produced CPU-readable planes; wgpu uploaded those planes because native zero-copy interop was unavailable",
                            })
                            .to_string(),
                        );
                    }
                }
            }
            planar
        } else if hardware_frame {
            self.stats.hardware_video_frames += 1;
            return Err(PlayerError::Renderer(
                "wgpu: hardware video frames require zero-copy native interop; use software decode or a native hardware renderer".to_string(),
            ));
        } else {
            return Err(PlayerError::Renderer(
                "wgpu: frame is not software 4:2:0 8-bit/10-bit".to_string(),
            ));
        };
        let is_p010 = matches!(planar.format, PlanarPixelFormat::P010);
        let source_color = source_color_for_player_frame(frame);
        let uniforms = self.video_uniforms_for_frame(frame, is_p010);
        self.upload_planar_with_context(planar, uniforms, Some(source_color))
    }

    fn clear_current_frame(&mut self) -> Result<()> {
        self.flutter_frame = None;
        self.current_video_visible = false;
        self.current_video = None;
        self.planar_upload_pool.clear();
        #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
        {
            self.linux_frame = None;
            self.linux_hardware_frame = None;
            self.linux_import_failure = None;
        }
        if self.surface.is_some() {
            self.render_surface_clear(WgpuClearColor::new(0.0, 0.0, 0.0, 1.0))?;
        }
        Ok(())
    }

    fn preserve_current_frame_for_transition(&mut self) -> Result<()> {
        // Every wgpu upload lives in renderer-owned textures. Android
        // MediaCodec/AHardwareBuffer ownership has already been retired after
        // the conversion submission, so this detached snapshot can remain
        // visible and capturable while the decoder is reopened.
        Ok(())
    }

    fn preserve_current_frame_for_track_transition(&mut self) -> Result<()> {
        // Track switches use the same renderer-owned snapshot, without making
        // native Metal/D3D backends clear their historical current frame.
        Ok(())
    }

    fn render_current_frame(&mut self, context: RenderFrameContext<'_>) -> Result<bool> {
        if let Some(metrics) = self.flutter_metrics {
            let (width, height) = metrics.physical_size();
            let danmaku = context.danmaku.filter(|plan| {
                plan.generation == context.generation
                    && plan.viewport.width == width
                    && plan.viewport.height == height
            });
            if let Some(readback) =
                self.render_current_offscreen_sized(width, height, context.overlay, danmaku)?
            {
                self.flutter_frame = Some(crate::core::RendererFrameCapture {
                    width,
                    height,
                    rgba: readback.rgba,
                });
                return Ok(true);
            }
            return Ok(false);
        }
        #[cfg(target_env = "ohos")]
        if let Some(interop) = &self.ohos_gles {
            interop.drain_discarded_frames().map_err(|error| {
                PlayerError::Renderer(format!(
                    "stage=ohos_native_image_drain_discarded reason={error}"
                ))
            })?;
        }
        if !self.current_video_visible || self.current_video.is_none() {
            return Ok(false);
        }
        if self.surface.is_none() {
            // No surface to present to (e.g. ticked before attach); the presenter
            // falls back to a test frame.
            return Ok(false);
        }
        let danmaku = context.danmaku.filter(|plan| {
            plan.generation == context.generation
                && (context.output_width == 0 || plan.viewport.width == context.output_width)
                && (context.output_height == 0 || plan.viewport.height == context.output_height)
        });
        let SurfaceFrame::Texture {
            texture: frame,
            reconfigure_after_present,
        } = self.acquire_surface_frame()?
        else {
            // A timeout or occlusion skips this tick but the current video frame
            // remains valid; report it handled so the presenter does not clear it.
            return Ok(true);
        };
        let (format, target_width, target_height, output) = {
            let attached = self.surface.as_ref().expect("surface present");
            (
                attached.config.format,
                attached.config.width,
                attached.config.height,
                attached.output,
            )
        };
        self.ensure_video_pipeline(format);
        if context.overlay.is_some_and(overlay_has_planes)
            || danmaku.is_some_and(|plan| !plan.is_empty())
        {
            self.ensure_overlay_pipeline(format);
        }
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let danmaku_draws = self.draw_current_video(
            &view,
            target_width,
            target_height,
            context.overlay,
            danmaku,
            output,
        )?;
        frame.present();
        if reconfigure_after_present {
            self.reconfigure_surface();
        }
        self.stats.rendered_frames += 1;
        if output.extended_linear {
            self.output_status.extended_linear_frames =
                self.output_status.extended_linear_frames.saturating_add(1);
        }
        if danmaku_draws > 0 {
            self.stats.danmaku_passes += 1;
            self.stats.danmaku_items += danmaku_draws as u64;
        }
        Ok(true)
    }

    fn take_flutter_frame(&mut self) -> Option<crate::core::RendererFrameCapture> {
        self.flutter_frame.take()
    }

    fn capture_current_frame(
        &mut self,
        context: RenderFrameContext<'_>,
        width: u32,
        height: u32,
    ) -> Result<Option<crate::core::RendererFrameCapture>> {
        if !self.current_video_visible || self.current_video.is_none() {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "frame_capture",
                    "stage": "no_current_frame",
                    "renderer": "wgpu",
                    "requestedWidth": width,
                    "requestedHeight": height,
                    "currentVideoVisible": self.current_video_visible,
                    "hasCurrentVideo": self.current_video.is_some(),
                })
                .to_string(),
            );
            return Ok(None);
        }
        if width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "capture size must be non-zero".to_string(),
            ));
        }
        let danmaku = context.danmaku.filter(|plan| {
            plan.generation == context.generation
                && plan.viewport.width == width
                && plan.viewport.height == height
        });
        let Some(readback) =
            self.render_current_offscreen_sized(width, height, context.overlay, danmaku)?
        else {
            return Ok(None);
        };
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "frame_capture",
                "stage": "completed",
                "renderer": "wgpu",
                "requestedWidth": width,
                "requestedHeight": height,
                "actualWidth": readback.width,
                "actualHeight": readback.height,
                "rgbaBytes": readback.rgba.len(),
            })
            .to_string(),
        );
        Ok(Some(crate::core::RendererFrameCapture {
            width: readback.width,
            height: readback.height,
            rgba: readback.rgba,
        }))
    }

    fn runtime_stats(&self) -> RendererRuntimeStats {
        let stats = self.stats();
        let upscaler = self.upscaler.stats();
        RendererRuntimeStats {
            surface_width: stats.surface_width,
            surface_height: stats.surface_height,
            rendered_frames: stats.rendered_frames,
            offscreen_frames: stats.offscreen_frames,
            prepared_overlay_frames: 0,
            prepared_overlay_subtitle_planes: 0,
            danmaku_passes: stats.danmaku_passes,
            danmaku_draw_items: stats.danmaku_items,
            overlay_alpha_atlas_uploads: 0,
            overlay_alpha_atlas_reuses: 0,
            last_danmaku_atlas_duration: Default::default(),
            last_danmaku_vertex_build_duration: Default::default(),
            last_danmaku_vertex_copy_duration: Default::default(),
            last_danmaku_encode_duration: Default::default(),
            last_danmaku_vertex_bytes: 0,
            last_danmaku_vertex_count: 0,
            upscaler_mode: self.upscaler_mode,
            upscaler_backend: match self.upscaler.status() {
                WgpuArtCnnStatus::Off => LumaUpscalerBackendStatus::Off,
                WgpuArtCnnStatus::Building => LumaUpscalerBackendStatus::Building,
                WgpuArtCnnStatus::Inactive => LumaUpscalerBackendStatus::Inactive,
                WgpuArtCnnStatus::Scalar => LumaUpscalerBackendStatus::Scalar,
            },
            upscaler_fallbacks: upscaler.fallback_count,
            upscaled_frames: upscaler.upscaled_frames,
            last_upscaler_encode_duration: upscaler.last_encode_duration,
            last_gpu_duration: Default::default(),
            attached: stats.attached,
            software_video_frames: stats.software_video_frames,
            hardware_video_frames: stats.hardware_video_frames,
            zero_copy_video_frames: stats.zero_copy_video_frames,
            direct_zero_copy_video_frames: stats.direct_zero_copy_video_frames,
            shared_handle_video_frames: stats.shared_handle_video_frames,
            cpu_video_frame_fallbacks: stats.cpu_video_frame_fallbacks,
            hdr_source_frames: stats.hdr_source_frames,
            hdr10_output_frames: 0,
            sdr_tonemap_frames: stats.sdr_tonemap_frames,
            hdr10_metadata_updates: 0,
            hdr10_metadata_failures: 0,
            hdr10_output_failures: 0,
            hdr10_output_active: false,
        }
    }

    fn output_status(&self) -> OutputRuntimeStatus {
        self.output_status
    }

    fn set_luma_upscaler(&mut self, mode: LumaUpscalerMode) {
        if mode == self.upscaler_mode
            && self.upscaler.mode() == mode
            && ((mode == LumaUpscalerMode::Off && self.upscaler.status() == WgpuArtCnnStatus::Off)
                || (mode.is_enabled() && self.upscaler.status() == WgpuArtCnnStatus::Scalar))
        {
            return;
        }
        self.upscaler_mode = mode;
        self.upscaler_failed_frame_token = None;
        self.upscaler_active_frame_reported = false;
        if mode.is_enabled() {
            crate::trace::diagnostic(self.upscaler.capability().diagnostic_json(mode).to_string());
        }
        match self.upscaler.set_mode(&self.device, mode) {
            Ok(()) => {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "luma_upscaler",
                        "stage": if mode.is_enabled() { "active" } else { "disabled" },
                        "renderer": "wgpu",
                        "requestedMode": format!("{mode:?}"),
                        "activeBackend": format!("{:?}", self.upscaler.status()),
                        "fallback": serde_json::Value::Null,
                    })
                    .to_string(),
                );
            }
            Err(failure) => {
                crate::trace::diagnostic(failure.diagnostic_json().to_string());
            }
        }
    }

    fn set_output_headroom(&mut self, headroom: f32, known: bool) {
        self.update_output_headroom_state(OutputHeadroomState::reported(headroom, known), true);
    }

    fn supports_mediacodec_surface_frames(&self) -> bool {
        #[cfg(target_os = "android")]
        {
            return self.android_vulkan.is_some();
        }
        #[cfg(target_env = "ohos")]
        {
            return (self.ohos_vulkan.is_some() && self.ohos_native_buffer_surface.is_some())
                || self.ohos_gles.is_some();
        }
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        {
            false
        }
    }

    #[cfg(target_env = "ohos")]
    fn ohos_avcodec_surface(
        &self,
    ) -> Option<std::sync::Arc<crate::ohos::avcodec::OhosAvCodecSurface>> {
        self.ohos_native_buffer_surface.clone().or_else(|| {
            self.ohos_gles
                .as_ref()
                .map(crate::ohos::gles::OhosGlesInterop::avcodec_surface)
        })
    }
}

#[cfg(target_os = "android")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AndroidWgpuRecoveryPolicy {
    AnyRendererFailure,
    DeviceFailureOnly,
}

#[cfg(target_os = "android")]
struct AndroidWgpuOperationFailure {
    error: PlayerError,
    recoverable: bool,
}

#[cfg(target_os = "android")]
struct AndroidWgpuRebuildFailure {
    error: PlayerError,
    can_try_another_backend: bool,
}

/// Android wgpu presentation with bounded runtime backend recovery.
///
/// `WgpuRenderer` already repairs an outdated/lost swapchain on the active
/// device. This layer handles the next failure boundary: configure/present
/// validation, an unhealthy/lost device, or a surface that remains unusable.
/// Each backend candidate is selected at most once per recovery sequence. A
/// replacement renderer reattaches the retained `ANativeWindow` and reimports
/// the last decoded frame when that frame remains importable.
#[cfg(target_os = "android")]
pub struct AndroidRecoveringWgpuRenderer {
    active: WgpuRenderer,
    // Bridges the short interval between dropping a failed renderer's
    // `AttachedSurface` and acquiring the same raw window in its replacement.
    // The JNI bridge also owns the window, but the renderer recovery layer must
    // be correct for direct C ABI embedders that satisfy only the attach call's
    // lifetime contract.
    recovery_window: Option<AndroidNativeWindow>,
    surface: Option<WgpuSurfaceHandle>,
    current_frame: Option<PlayerVideoFrame>,
    output_mode: OutputMode,
    video_alpha_mode: VideoAlphaMode,
    output_headroom: OutputHeadroomState,
    upscaler_mode: LumaUpscalerMode,
    retired_stats: RendererRuntimeStats,
    retired_output_status: OutputRuntimeStatus,
    terminal_failure: Option<String>,
    recovery_sequence: u64,
}

#[cfg(target_os = "android")]
impl AndroidRecoveringWgpuRenderer {
    pub fn new() -> Result<Self> {
        Self::new_with_config(MetalRendererConfig::default())
    }

    pub fn new_with_output_mode(output_mode: OutputMode) -> Result<Self> {
        Self::new_with_config(MetalRendererConfig {
            output_mode,
            ..MetalRendererConfig::default()
        })
    }

    pub fn new_with_config(config: MetalRendererConfig) -> Result<Self> {
        Ok(Self {
            active: WgpuRenderer::new_with_config(config)?,
            recovery_window: None,
            surface: None,
            current_frame: None,
            output_mode: config.output_mode,
            video_alpha_mode: config.video_alpha_mode,
            output_headroom: OutputHeadroomState::default(),
            upscaler_mode: LumaUpscalerMode::Off,
            retired_stats: RendererRuntimeStats::default(),
            retired_output_status: OutputRuntimeStatus::requested(config.output_mode),
            terminal_failure: None,
            recovery_sequence: 0,
        })
    }

    fn reset_terminal_failure(&mut self, operation: &'static str) {
        if let Some(previous) = self.terminal_failure.take() {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "android_gpu_recovery",
                    "stage": "terminal_failure_reset",
                    "operation": operation,
                    "previousFailure": previous,
                    "reason": "a new Android surface attachment starts a fresh bounded GPU recovery sequence",
                })
                .to_string(),
            );
        }
    }

    fn invoke_active<T>(
        &mut self,
        operation: &'static str,
        policy: AndroidWgpuRecoveryPolicy,
        call: &mut impl FnMut(&mut WgpuRenderer) -> Result<T>,
    ) -> std::result::Result<T, AndroidWgpuOperationFailure> {
        if let Err(error) = self.active.android_poll_device_health(operation) {
            return Err(AndroidWgpuOperationFailure {
                error,
                recoverable: true,
            });
        }

        let call_result = catch_unwind(AssertUnwindSafe(|| call(&mut self.active)));
        let result = match call_result {
            Ok(result) => result,
            Err(payload) => {
                return Err(AndroidWgpuOperationFailure {
                    error: PlayerError::Renderer(format!(
                        "panic during Android wgpu {operation}: {}",
                        panic_payload_message(payload.as_ref())
                    )),
                    recoverable: true,
                });
            }
        };

        if let Err(health_error) = self.active.android_poll_device_health(operation) {
            let reason = match result {
                Ok(_) => health_error.to_string(),
                Err(operation_error) => format!(
                    "{}; operation also returned: {}",
                    health_error, operation_error
                ),
            };
            return Err(AndroidWgpuOperationFailure {
                error: PlayerError::Renderer(reason),
                recoverable: true,
            });
        }

        result.map_err(|error| {
            let recoverable = matches!(error, PlayerError::Renderer(_))
                && policy == AndroidWgpuRecoveryPolicy::AnyRendererFailure;
            AndroidWgpuOperationFailure { error, recoverable }
        })
    }

    fn execute_with_recovery<T>(
        &mut self,
        operation: &'static str,
        policy: AndroidWgpuRecoveryPolicy,
        mut call: impl FnMut(&mut WgpuRenderer) -> Result<T>,
    ) -> Result<T> {
        if let Some(reason) = self.terminal_failure.as_ref() {
            return Err(PlayerError::Renderer(format!(
                "Android wgpu recovery is exhausted; {operation} requires a new surface attachment: {reason}"
            )));
        }

        let candidate_count = wgpu_backend_candidates().len();
        let mut attempted_candidates = vec![self.active.android_backend_candidate_index];
        let mut failures = Vec::new();

        loop {
            match self.invoke_active(operation, policy, &mut call) {
                Ok(value) => {
                    if !failures.is_empty() {
                        crate::trace::diagnostic(
                            serde_json::json!({
                                "event": "android_gpu_recovery",
                                "stage": "operation_recovered",
                                "sequence": self.recovery_sequence,
                                "operation": operation,
                                "activeBackendCandidate": self.active.android_backend_candidate_label(),
                                "attemptedBackendCandidates": candidate_labels(&attempted_candidates),
                                "failureCount": failures.len(),
                                "failures": failures,
                                "surfaceRestored": self.surface.is_some(),
                                "currentFrameCached": self.current_frame.is_some(),
                            })
                            .to_string(),
                        );
                    }
                    return Ok(value);
                }
                Err(failure) if !failure.recoverable => return Err(failure.error),
                Err(failure) => {
                    failures.push(format!(
                        "{}: {}",
                        self.active.android_backend_candidate_label(),
                        failure.error
                    ));
                    self.recovery_sequence = self.recovery_sequence.saturating_add(1).max(1);
                    crate::trace::diagnostic(
                        serde_json::json!({
                            "event": "android_gpu_recovery",
                            "stage": "operation_failed",
                            "sequence": self.recovery_sequence,
                            "operation": operation,
                            "activeBackendCandidate": self.active.android_backend_candidate_label(),
                            "attemptedBackendCandidates": candidate_labels(&attempted_candidates),
                            "failureCount": failures.len(),
                            "reason": failure.error.to_string(),
                            "action": "rebuild_renderer_rotate_backend_restore_surface_and_frame",
                        })
                        .to_string(),
                    );
                }
            }

            loop {
                if attempted_candidates.len() >= candidate_count {
                    return self.exhaust_recovery(operation, attempted_candidates, failures);
                }
                match self.rebuild_active_renderer(operation, &mut attempted_candidates) {
                    Ok(()) => break,
                    Err(failure) => {
                        failures.push(format!(
                            "{}: {}",
                            self.active.android_backend_candidate_label(),
                            failure.error
                        ));
                        if !failure.can_try_another_backend {
                            return self.exhaust_recovery(
                                operation,
                                attempted_candidates,
                                failures,
                            );
                        }
                    }
                }
            }
        }
    }

    fn rebuild_active_renderer(
        &mut self,
        operation: &'static str,
        attempted_candidates: &mut Vec<usize>,
    ) -> std::result::Result<(), AndroidWgpuRebuildFailure> {
        let previous_candidate = self.active.android_backend_candidate_index;
        let previous_label = self.active.android_backend_candidate_label();
        let (replacement, construction_attempts) = catch_unwind(AssertUnwindSafe(|| {
            WgpuRenderer::new_after_runtime_failure(
                previous_candidate,
                attempted_candidates,
                self.output_mode,
                self.video_alpha_mode,
            )
        }))
        .map_err(|payload| AndroidWgpuRebuildFailure {
            error: PlayerError::Renderer(format!(
                "panic while rebuilding Android wgpu renderer after {operation}: {}",
                panic_payload_message(payload.as_ref())
            )),
            can_try_another_backend: false,
        })?;
        for candidate_index in construction_attempts {
            if !attempted_candidates.contains(&candidate_index) {
                attempted_candidates.push(candidate_index);
            }
        }
        let replacement = replacement.map_err(|error| AndroidWgpuRebuildFailure {
            error,
            can_try_another_backend: false,
        })?;

        let replacement_candidate = replacement.android_backend_candidate_index;
        let replacement_label = replacement.android_backend_candidate_label();
        accumulate_retired_renderer_stats(&mut self.retired_stats, self.active.runtime_stats());
        accumulate_retired_output_status(
            &mut self.retired_output_status,
            self.active.output_status(),
        );
        self.active = replacement;
        self.active.set_luma_upscaler(self.upscaler_mode);
        self.active
            .update_output_headroom_state(self.output_headroom, false);

        let surface_restored = if let Some(surface) = self.surface {
            match catch_unwind(AssertUnwindSafe(|| {
                self.active.attach_surface(PlatformSurface::Wgpu(surface))
            })) {
                Ok(Ok(())) => {
                    if let Err(error) = self.active.android_poll_device_health("restore_surface") {
                        return Err(AndroidWgpuRebuildFailure {
                            error,
                            can_try_another_backend: attempted_candidates.len()
                                < wgpu_backend_candidates().len(),
                        });
                    }
                    true
                }
                Ok(Err(error)) => {
                    return Err(AndroidWgpuRebuildFailure {
                        error,
                        can_try_another_backend: attempted_candidates.len()
                            < wgpu_backend_candidates().len(),
                    });
                }
                Err(payload) => {
                    return Err(AndroidWgpuRebuildFailure {
                        error: PlayerError::Renderer(format!(
                            "panic while restoring Android wgpu surface: {}",
                            panic_payload_message(payload.as_ref())
                        )),
                        can_try_another_backend: attempted_candidates.len()
                            < wgpu_backend_candidates().len(),
                    });
                }
            }
        } else {
            false
        };

        let mut frame_restored = false;
        if let Some(frame) = self.current_frame.as_ref() {
            let restore_result =
                catch_unwind(AssertUnwindSafe(|| self.active.upload_player_frame(frame)));
            match restore_result {
                Ok(Ok(())) => match self.active.android_poll_device_health("restore_frame") {
                    Ok(()) => frame_restored = true,
                    Err(error) => {
                        return Err(AndroidWgpuRebuildFailure {
                            error,
                            can_try_another_backend: attempted_candidates.len()
                                < wgpu_backend_candidates().len(),
                        });
                    }
                },
                Ok(Err(error)) => {
                    if let Err(health_error) =
                        self.active.android_poll_device_health("restore_frame")
                    {
                        return Err(AndroidWgpuRebuildFailure {
                            error: PlayerError::Renderer(format!(
                                "{health_error}; cached frame restore also returned: {error}"
                            )),
                            can_try_another_backend: attempted_candidates.len()
                                < wgpu_backend_candidates().len(),
                        });
                    }
                    crate::trace::diagnostic(
                        serde_json::json!({
                            "event": "android_gpu_recovery",
                            "stage": "current_frame_restore_failed",
                            "sequence": self.recovery_sequence,
                            "operation": operation,
                            "backendCandidate": replacement_label,
                            "decodeBackend": frame.decode_backend.as_str(),
                            "generation": frame.generation,
                            "reason": error.to_string(),
                            "action": "keep_renderer_and_surface_wait_for_next_decoded_frame",
                        })
                        .to_string(),
                    );
                }
                Err(payload) => {
                    return Err(AndroidWgpuRebuildFailure {
                        error: PlayerError::Renderer(format!(
                            "panic while restoring cached Android video frame: {}",
                            panic_payload_message(payload.as_ref())
                        )),
                        can_try_another_backend: attempted_candidates.len()
                            < wgpu_backend_candidates().len(),
                    });
                }
            }
        }

        crate::trace::diagnostic(
            serde_json::json!({
                "event": "android_gpu_recovery",
                "stage": "renderer_rebuilt",
                "sequence": self.recovery_sequence,
                "operation": operation,
                "previousBackendCandidate": previous_label,
                "activeBackendCandidate": replacement_label,
                "backendChanged": previous_candidate != replacement_candidate,
                "attemptedBackendCandidates": candidate_labels(attempted_candidates),
                "surfaceRestored": surface_restored,
                "currentFrameCached": self.current_frame.is_some(),
                "currentFrameRestored": frame_restored,
                "mediaCodecSurfaceFramesSupported": self.active.supports_mediacodec_surface_frames(),
            })
            .to_string(),
        );
        Ok(())
    }

    fn exhaust_recovery<T>(
        &mut self,
        operation: &'static str,
        attempted_candidates: Vec<usize>,
        failures: Vec<String>,
    ) -> Result<T> {
        let message = format!(
            "Android wgpu runtime recovery exhausted during {operation} after {} backend candidate(s): {}",
            attempted_candidates.len(),
            failures.join("; ")
        );
        self.terminal_failure = Some(message.clone());
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "android_gpu_recovery",
                "stage": "recovery_exhausted",
                "sequence": self.recovery_sequence,
                "operation": operation,
                "attemptedBackendCandidates": candidate_labels(&attempted_candidates),
                "attemptedBackendCount": attempted_candidates.len(),
                "candidateCount": wgpu_backend_candidates().len(),
                "failures": failures,
                "reason": message.as_str(),
                "requiredAction": "detach_and_attach_a_live_android_surface_to_start_a_new_bounded_recovery_sequence",
            })
            .to_string(),
        );
        Err(PlayerError::Renderer(message))
    }
}

#[cfg(target_os = "android")]
impl RendererBackend for AndroidRecoveringWgpuRenderer {
    fn attach_surface(&mut self, surface: PlatformSurface) -> Result<()> {
        let PlatformSurface::Wgpu(handle) = surface else {
            return Err(PlayerError::Renderer(
                "non-wgpu surface cannot be attached to AndroidRecoveringWgpuRenderer".to_string(),
            ));
        };
        if handle.kind != WgpuSurfaceKind::AndroidNativeWindow {
            return Err(PlayerError::Renderer(format!(
                "wgpu surface kind {:?} cannot be attached to AndroidRecoveringWgpuRenderer",
                handle.kind
            )));
        }
        let raw_window = NonNull::new(handle.raw_window as *mut c_void).ok_or_else(|| {
            PlayerError::Renderer("wgpu Android ANativeWindow surface handle is null".to_string())
        })?;
        // SAFETY: RendererBackend::attach_surface requires a live native handle.
        // This extra owned reference remains across every active renderer swap.
        let recovery_window = unsafe { AndroidNativeWindow::acquire(raw_window) };
        self.reset_terminal_failure("attach_surface");
        self.execute_with_recovery(
            "attach_surface",
            AndroidWgpuRecoveryPolicy::AnyRendererFailure,
            |renderer| renderer.attach_surface(surface),
        )?;
        self.recovery_window = Some(recovery_window);
        self.surface = Some(handle);
        Ok(())
    }

    fn detach_surface(&mut self) -> Result<()> {
        self.active.detach_surface()?;
        self.surface = None;
        self.recovery_window = None;
        self.terminal_failure = None;
        Ok(())
    }

    fn resize_surface(&mut self, metrics: crate::core::SurfaceMetrics) -> Result<()> {
        self.execute_with_recovery(
            "resize_surface",
            AndroidWgpuRecoveryPolicy::AnyRendererFailure,
            |renderer| renderer.resize_surface(metrics),
        )?;
        if let Some(surface) = self.surface.as_mut() {
            surface.resize(metrics);
        }
        Ok(())
    }

    fn render_test_frame(&mut self, time_seconds: f64) -> Result<()> {
        self.execute_with_recovery(
            "render_test_frame",
            AndroidWgpuRecoveryPolicy::AnyRendererFailure,
            |renderer| renderer.render_test_frame(time_seconds),
        )
    }

    fn upload_player_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        self.execute_with_recovery(
            "upload_player_frame",
            AndroidWgpuRecoveryPolicy::DeviceFailureOnly,
            |renderer| renderer.upload_player_frame(frame),
        )?;
        self.current_frame = match retain_player_video_frame(frame) {
            Ok(frame) => Some(frame),
            Err(error) => {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "android_gpu_recovery",
                        "stage": "current_frame_cache_failed",
                        "decodeBackend": frame.decode_backend.as_str(),
                        "generation": frame.generation,
                        "pixelFormat": frame.frame.pixel_format(),
                        "reason": error.to_string(),
                        "action": "continue_rendering_without_cross_device_frame_restore",
                    })
                    .to_string(),
                );
                None
            }
        };
        Ok(())
    }

    fn clear_current_frame(&mut self) -> Result<()> {
        // Retained cross-device recovery data must be released even when the
        // active GPU is already lost and clearing the surface fails. Taking it
        // first also prevents recovery from restoring a frame that the
        // presenter is explicitly retiring before a decoder transition.
        self.current_frame = None;
        self.execute_with_recovery(
            "clear_current_frame",
            AndroidWgpuRecoveryPolicy::AnyRendererFailure,
            WgpuRenderer::clear_current_frame,
        )
    }

    fn preserve_current_frame_for_transition(&mut self) -> Result<()> {
        // The recovery cache may retain the original PlayerVideoFrame and its
        // MediaCodec release callback. Drop that decoder-owned payload, but
        // keep the active renderer's detached GPU texture as a transition
        // snapshot.
        self.current_frame = None;
        self.active.preserve_current_frame_for_transition()
    }

    fn preserve_current_frame_for_track_transition(&mut self) -> Result<()> {
        // Track selection also seeks/reopens MediaCodec on Android. Retire the
        // decoder-owned recovery payload while preserving the active wgpu
        // renderer's detached GPU snapshot.
        self.current_frame = None;
        self.active.preserve_current_frame_for_track_transition()
    }

    fn render_current_frame(&mut self, context: RenderFrameContext<'_>) -> Result<bool> {
        self.execute_with_recovery(
            "render_current_frame",
            AndroidWgpuRecoveryPolicy::AnyRendererFailure,
            |renderer| renderer.render_current_frame(context),
        )
    }

    fn capture_current_frame(
        &mut self,
        context: RenderFrameContext<'_>,
        width: u32,
        height: u32,
    ) -> Result<Option<crate::core::RendererFrameCapture>> {
        self.execute_with_recovery(
            "capture_current_frame",
            AndroidWgpuRecoveryPolicy::AnyRendererFailure,
            |renderer| renderer.capture_current_frame(context, width, height),
        )
    }

    fn runtime_stats(&self) -> RendererRuntimeStats {
        let mut stats = self.active.runtime_stats();
        add_retired_renderer_stats(&mut stats, self.retired_stats);
        stats
    }

    fn output_status(&self) -> OutputRuntimeStatus {
        let mut status = self.active.output_status();
        add_retired_output_counters(&mut status, self.retired_output_status);
        status
    }

    fn supports_mediacodec_surface_frames(&self) -> bool {
        self.active.supports_mediacodec_surface_frames()
    }

    fn set_luma_upscaler(&mut self, mode: LumaUpscalerMode) {
        self.upscaler_mode = mode;
        self.active.set_luma_upscaler(mode);
    }

    fn set_output_headroom(&mut self, headroom: f32, known: bool) {
        self.output_headroom = OutputHeadroomState::reported(headroom, known);
        self.active
            .update_output_headroom_state(self.output_headroom, true);
    }
}

#[cfg(target_os = "android")]
fn retain_player_video_frame(frame: &PlayerVideoFrame) -> Result<PlayerVideoFrame> {
    Ok(PlayerVideoFrame {
        frame: frame.frame.try_clone_ref().map_err(|error| {
            PlayerError::Renderer(format!(
                "failed to retain current frame for Android GPU recovery: {error}"
            ))
        })?,
        decode_backend: frame.decode_backend,
        pts: frame.pts,
        media_time: frame.media_time,
        late_by: frame.late_by,
        generation: frame.generation,
        scene_avg_nits: frame.scene_avg_nits,
    })
}

#[cfg(target_os = "android")]
fn candidate_labels(candidate_indices: &[usize]) -> Vec<&'static str> {
    let candidates = wgpu_backend_candidates();
    candidate_indices
        .iter()
        .map(|candidate_index| {
            candidates
                .get(*candidate_index)
                .map_or("unknown", |candidate| candidate.label)
        })
        .collect()
}

#[cfg(target_os = "android")]
fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

#[cfg(target_os = "android")]
fn accumulate_retired_renderer_stats(
    retired: &mut RendererRuntimeStats,
    current: RendererRuntimeStats,
) {
    add_retired_renderer_stats(retired, current);
}

#[cfg(target_os = "android")]
fn accumulate_retired_output_status(
    retired: &mut OutputRuntimeStatus,
    current: OutputRuntimeStatus,
) {
    add_retired_output_counters(retired, current);
}

#[cfg(target_os = "android")]
fn add_retired_output_counters(target: &mut OutputRuntimeStatus, retired: OutputRuntimeStatus) {
    target.fallback_count = target.fallback_count.saturating_add(retired.fallback_count);
    target.data_space_failures = target
        .data_space_failures
        .saturating_add(retired.data_space_failures);
    target.headroom_updates = target
        .headroom_updates
        .saturating_add(retired.headroom_updates);
    target.extended_linear_frames = target
        .extended_linear_frames
        .saturating_add(retired.extended_linear_frames);
}

#[cfg(target_os = "android")]
fn add_retired_renderer_stats(target: &mut RendererRuntimeStats, retired: RendererRuntimeStats) {
    target.rendered_frames = target
        .rendered_frames
        .saturating_add(retired.rendered_frames);
    target.offscreen_frames = target
        .offscreen_frames
        .saturating_add(retired.offscreen_frames);
    target.prepared_overlay_frames = target
        .prepared_overlay_frames
        .saturating_add(retired.prepared_overlay_frames);
    target.prepared_overlay_subtitle_planes = target
        .prepared_overlay_subtitle_planes
        .saturating_add(retired.prepared_overlay_subtitle_planes);
    target.danmaku_passes = target.danmaku_passes.saturating_add(retired.danmaku_passes);
    target.danmaku_draw_items = target
        .danmaku_draw_items
        .saturating_add(retired.danmaku_draw_items);
    target.overlay_alpha_atlas_uploads = target
        .overlay_alpha_atlas_uploads
        .saturating_add(retired.overlay_alpha_atlas_uploads);
    target.overlay_alpha_atlas_reuses = target
        .overlay_alpha_atlas_reuses
        .saturating_add(retired.overlay_alpha_atlas_reuses);
    target.upscaler_fallbacks = target
        .upscaler_fallbacks
        .saturating_add(retired.upscaler_fallbacks);
    target.upscaled_frames = target
        .upscaled_frames
        .saturating_add(retired.upscaled_frames);
    target.software_video_frames = target
        .software_video_frames
        .saturating_add(retired.software_video_frames);
    target.hardware_video_frames = target
        .hardware_video_frames
        .saturating_add(retired.hardware_video_frames);
    target.zero_copy_video_frames = target
        .zero_copy_video_frames
        .saturating_add(retired.zero_copy_video_frames);
    target.direct_zero_copy_video_frames = target
        .direct_zero_copy_video_frames
        .saturating_add(retired.direct_zero_copy_video_frames);
    target.shared_handle_video_frames = target
        .shared_handle_video_frames
        .saturating_add(retired.shared_handle_video_frames);
    target.cpu_video_frame_fallbacks = target
        .cpu_video_frame_fallbacks
        .saturating_add(retired.cpu_video_frame_fallbacks);
    target.hdr_source_frames = target
        .hdr_source_frames
        .saturating_add(retired.hdr_source_frames);
    target.hdr10_output_frames = target
        .hdr10_output_frames
        .saturating_add(retired.hdr10_output_frames);
    target.sdr_tonemap_frames = target
        .sdr_tonemap_frames
        .saturating_add(retired.sdr_tonemap_frames);
    target.hdr10_metadata_updates = target
        .hdr10_metadata_updates
        .saturating_add(retired.hdr10_metadata_updates);
    target.hdr10_metadata_failures = target
        .hdr10_metadata_failures
        .saturating_add(retired.hdr10_metadata_failures);
    target.hdr10_output_failures = target
        .hdr10_output_failures
        .saturating_add(retired.hdr10_output_failures);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    #[test]
    fn linux_hdr_requires_negotiated_scrgb_not_just_ten_bit_storage() {
        let formats = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgb10a2Unorm,
        ];
        let output = select_linux_surface_output(
            OutputMode::auto(12.5),
            true,
            wgpu::Backend::Vulkan,
            &formats,
        )
        .unwrap();
        assert!(!output.output.extended_linear);
        assert_eq!(
            output.fallback_reason,
            OutputFallbackReason::DisplayHdrUnsupported
        );
    }

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    #[test]
    fn linux_hdr_auto_switches_back_to_sdr_for_sdr_media() {
        let formats = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba16Float,
        ];
        let hdr = select_linux_surface_output(
            OutputMode::auto(1.0),
            true,
            wgpu::Backend::Vulkan,
            &formats,
        )
        .unwrap();
        assert!(hdr.output.extended_linear);
        assert_eq!(hdr.output.target.reference_white_nits, 80.0);
        assert_eq!(hdr.output.target.edr_headroom, 12.5);
        let sdr = select_linux_surface_output(
            OutputMode::auto(1.0),
            false,
            wgpu::Backend::Vulkan,
            &formats,
        )
        .unwrap();
        assert!(!sdr.output.extended_linear);
        let gl =
            select_linux_surface_output(OutputMode::auto(12.5), true, wgpu::Backend::Gl, &formats)
                .unwrap();
        assert!(!gl.output.extended_linear);
    }

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    #[test]
    fn linux_hardware_requirement_rejects_cpu_and_warp_adapters() {
        assert!(linux_adapter_is_hardware(
            wgpu::DeviceType::Other,
            "D3D12 (NVIDIA GeForce RTX 5070)"
        ));
        assert!(!linux_adapter_is_hardware(
            wgpu::DeviceType::Cpu,
            "Software Device"
        ));
        assert!(!linux_adapter_is_hardware(
            wgpu::DeviceType::Other,
            "D3D12 (Microsoft Basic Render Driver)"
        ));
        assert!(!linux_adapter_is_hardware(
            wgpu::DeviceType::Other,
            "llvmpipe (LLVM 21)"
        ));
    }

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    #[test]
    fn linux_surfaces_require_both_handles_and_keep_their_native_types() {
        for kind in [WgpuSurfaceKind::XlibWindow, WgpuSurfaceKind::WaylandSurface] {
            for (window, display) in [(0, 1), (1, 0), (0, 0)] {
                assert!(
                    linux_surface_target(WgpuSurfaceHandle::new(
                        kind, window, display, 100, 50, 1.0
                    ))
                    .is_err()
                );
            }
            // These are opaque handles; conversion must not dereference them.
            let target =
                linux_surface_target(WgpuSurfaceHandle::new(kind, 1, 2, 100, 50, 1.0)).unwrap();
            assert!(matches!(
                (kind, target),
                (
                    WgpuSurfaceKind::XlibWindow,
                    wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_window_handle: wgpu::rwh::RawWindowHandle::Xlib(_),
                        raw_display_handle: Some(wgpu::rwh::RawDisplayHandle::Xlib(_)),
                    }
                ) | (
                    WgpuSurfaceKind::WaylandSurface,
                    wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_window_handle: wgpu::rwh::RawWindowHandle::Wayland(_),
                        raw_display_handle: Some(wgpu::rwh::RawDisplayHandle::Wayland(_)),
                    }
                )
            ));
        }
    }

    #[test]
    fn conversion_extent_caps_4k_to_the_display_without_upscaling_smaller_video() {
        assert_eq!(
            fit_extent_without_upscale(3840, 2160, 2800, 1264),
            (2248, 1264)
        );
        assert_eq!(
            fit_extent_without_upscale(1920, 1080, 2800, 1264),
            (1920, 1080)
        );
        assert_eq!(
            fit_extent_without_upscale(2160, 3840, 1264, 2800),
            (1264, 2248)
        );
    }

    #[test]
    fn backend_candidate_recovery_order_rotates_and_excludes_attempted_backends() {
        assert_eq!(backend_candidate_order(4, 2, &[]), vec![2, 3, 0, 1]);
        assert_eq!(backend_candidate_order(4, 2, &[2, 0]), vec![3, 1]);
        assert_eq!(backend_candidate_order(4, 7, &[3]), vec![0, 1, 2]);
        assert!(backend_candidate_order(0, 0, &[]).is_empty());
    }

    #[test]
    fn runtime_recovery_records_initialization_failures_before_selected_candidate() {
        let order = vec![1, 2, 3, 0];
        assert_eq!(attempted_candidate_prefix(&order, Some(3)), vec![1, 2, 3]);
        assert_eq!(attempted_candidate_prefix(&order, Some(1)), vec![1]);
        assert_eq!(attempted_candidate_prefix(&order, None), order);
    }

    #[cfg(target_os = "android")]
    #[test]
    fn android_backend_candidates_keep_plain_vulkan_before_gles() {
        let candidates = wgpu_backend_candidates();
        let labels = candidates
            .iter()
            .map(|candidate| candidate.label)
            .collect::<Vec<_>>();

        assert_eq!(
            labels,
            vec!["vulkan-ahb", "vulkan", "gles", "vulkan-software"]
        );
        assert!(candidates[0].android_ahb_interop);
        assert!(!candidates[1].android_ahb_interop);
        assert!(!candidates[2].android_ahb_interop);
        assert!(!candidates[1].allow_cpu_adapter);
        assert!(!candidates[2].allow_cpu_adapter);
        assert!(candidates[3].allow_cpu_adapter);
        assert!(
            candidates[..3]
                .iter()
                .all(|candidate| !candidate.force_fallback_adapter)
        );
        assert!(candidates[3].force_fallback_adapter);
    }

    #[cfg(target_os = "android")]
    #[test]
    fn android_device_health_promotes_device_lost_over_prior_validation() {
        let health = AndroidWgpuDeviceHealth::default();
        health.record("validation", "bad surface configuration".to_string(), false);
        health.record("internal", "later internal error".to_string(), false);
        assert_eq!(health.failure().unwrap().kind, "validation");

        health.record("device_lost", "unknown: reset".to_string(), true);
        let failure = health.failure().unwrap();
        assert_eq!(failure.kind, "device_lost");
        assert_eq!(failure.reason, "unknown: reset");
    }

    #[cfg(target_os = "android")]
    #[test]
    fn retired_renderer_stats_preserve_current_surface_and_accumulate_counters() {
        let mut current = RendererRuntimeStats {
            surface_width: 1920,
            surface_height: 1080,
            rendered_frames: 4,
            hardware_video_frames: 5,
            attached: true,
            ..RendererRuntimeStats::default()
        };
        let retired = RendererRuntimeStats {
            surface_width: 1280,
            surface_height: 720,
            rendered_frames: 7,
            hardware_video_frames: 9,
            attached: false,
            ..RendererRuntimeStats::default()
        };

        add_retired_renderer_stats(&mut current, retired);

        assert_eq!(current.surface_width, 1920);
        assert_eq!(current.surface_height, 1080);
        assert!(current.attached);
        assert_eq!(current.rendered_frames, 11);
        assert_eq!(current.hardware_video_frames, 14);
    }

    #[cfg(target_os = "android")]
    #[test]
    fn android_renderer_drop_poll_is_bounded() {
        match android_wgpu_drop_poll_type() {
            wgpu::PollType::Wait {
                submission_index,
                timeout,
            } => {
                assert!(submission_index.is_none());
                assert_eq!(timeout, Some(ANDROID_WGPU_DROP_POLL_TIMEOUT));
            }
            wgpu::PollType::Poll => panic!("renderer drop must perform one bounded wait"),
        }
    }
    use crate::core::MetalSurfaceHandle;
    use crate::danmaku::{
        DanmakuFrameStats, DanmakuGlyphAtlas, DanmakuGlyphInstance, DanmakuRenderPlan,
        DanmakuViewport,
    };
    use std::time::Duration;

    fn to_u8(component: f64) -> u8 {
        (component * 255.0).round() as u8
    }

    #[test]
    fn wgpu_surface_extent_is_not_multiplied_by_content_scale() {
        let handle =
            WgpuSurfaceHandle::new(WgpuSurfaceKind::AndroidNativeWindow, 1, 0, 1081, 607, 2.625);

        assert_eq!(handle.metrics().physical_size(), (1081, 607));
        assert_eq!(handle.metrics().content_scale, 2.625);
    }

    #[test]
    fn requested_limits_accept_minimum_uniform_binding_without_losing_resolution() {
        let adapter_limits = wgpu::Limits {
            max_texture_dimension_2d: 16_384,
            max_uniform_buffer_binding_size: 16 * 1024,
            ..wgpu::Limits::downlevel_defaults()
        };

        let requested = requested_device_limits(adapter_limits, wgpu::Backend::Vulkan);

        assert_eq!(requested.max_texture_dimension_2d, 16_384);
        assert_eq!(requested.max_uniform_buffer_binding_size, 16 * 1024);
        assert!(std::mem::size_of::<VideoUniforms>() <= 16 * 1024);
    }

    #[test]
    fn requested_limits_accept_gles_without_compute_support() {
        let adapter_limits = wgpu::Limits {
            max_texture_dimension_2d: 8192,
            max_uniform_buffer_binding_size: 16 * 1024,
            max_compute_workgroup_storage_size: 0,
            max_compute_invocations_per_workgroup: 0,
            max_compute_workgroup_size_x: 0,
            max_compute_workgroup_size_y: 0,
            max_compute_workgroup_size_z: 0,
            max_compute_workgroups_per_dimension: 0,
            ..wgpu::Limits::downlevel_webgl2_defaults()
        };

        let requested = requested_device_limits(adapter_limits, wgpu::Backend::Gl);

        assert_eq!(requested.max_texture_dimension_2d, 8192);
        assert_eq!(requested.max_uniform_buffer_binding_size, 16 * 1024);
        assert_eq!(requested.max_compute_workgroups_per_dimension, 0);
    }

    #[test]
    fn android_extended_linear_surface_requires_vulkan_fp16_and_direct_composition() {
        let capabilities = SurfaceOutputCapabilities {
            extended_linear: true,
            direct_composition: true,
            desired_headroom: 4.0,
            fallback_reason: OutputFallbackReason::None,
        };
        let formats = [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Rgba16Float,
        ];

        let active = select_wgpu_surface_output(
            OutputMode::extended_linear(4.0),
            capabilities,
            wgpu::Backend::Vulkan,
            &formats,
        )
        .unwrap();
        assert_eq!(active.format, wgpu::TextureFormat::Rgba16Float);
        assert!(active.output.extended_linear);
        assert_eq!(active.fallback_reason, OutputFallbackReason::None);
        assert_eq!(active.output.target.reference_white_nits, 80.0);

        let gles = select_wgpu_surface_output(
            OutputMode::extended_linear(4.0),
            capabilities,
            wgpu::Backend::Gl,
            &formats,
        )
        .unwrap();
        assert_eq!(gles.format, wgpu::TextureFormat::Rgba8Unorm);
        assert!(!gles.output.extended_linear);
        assert_eq!(
            gles.fallback_reason,
            OutputFallbackReason::WgpuBackendNotVulkan
        );

        let texture_composited = select_wgpu_surface_output(
            OutputMode::extended_linear(4.0),
            SurfaceOutputCapabilities {
                direct_composition: false,
                ..capabilities
            },
            wgpu::Backend::Vulkan,
            &formats,
        )
        .unwrap();
        assert!(!texture_composited.output.extended_linear);
        assert_eq!(
            texture_composited.fallback_reason,
            OutputFallbackReason::HybridCompositionRequired
        );

        let display_unsupported = select_wgpu_surface_output(
            OutputMode::extended_linear(4.0),
            SurfaceOutputCapabilities {
                extended_linear: false,
                direct_composition: true,
                desired_headroom: 0.0,
                fallback_reason: OutputFallbackReason::DisplayHdrUnsupported,
            },
            wgpu::Backend::Vulkan,
            &formats,
        )
        .unwrap();
        assert_eq!(
            display_unsupported.fallback_reason,
            OutputFallbackReason::DisplayHdrUnsupported
        );

        let api_unavailable = select_wgpu_surface_output(
            OutputMode::extended_linear(4.0),
            SurfaceOutputCapabilities {
                extended_linear: false,
                direct_composition: true,
                desired_headroom: 0.0,
                fallback_reason: OutputFallbackReason::NativeWindowDataSpaceApiUnavailable,
            },
            wgpu::Backend::Vulkan,
            &formats,
        )
        .unwrap();
        assert_eq!(
            api_unavailable.fallback_reason,
            OutputFallbackReason::NativeWindowDataSpaceApiUnavailable
        );
    }

    #[test]
    fn extended_linear_headroom_respects_explicit_and_reported_limits() {
        let requested = OutputMode::extended_linear(4.0);
        let auto = SurfaceOutputCapabilities {
            extended_linear: true,
            direct_composition: true,
            desired_headroom: 0.0,
            fallback_reason: OutputFallbackReason::None,
        };
        assert_eq!(
            effective_extended_linear_headroom(requested, auto, OutputHeadroomState::default(),),
            4.0
        );
        // A pre-content ratio of 1 must not suppress the first HDR signal.
        assert_eq!(
            effective_extended_linear_headroom(
                requested,
                auto,
                OutputHeadroomState::reported(1.0, true),
            ),
            4.0
        );
        assert_eq!(
            effective_extended_linear_headroom(
                requested,
                auto,
                OutputHeadroomState::reported(2.5, true),
            ),
            2.5
        );

        let explicit = SurfaceOutputCapabilities {
            desired_headroom: 3.0,
            ..auto
        };
        assert_eq!(
            effective_extended_linear_headroom(
                requested,
                explicit,
                OutputHeadroomState::reported(3.5, true),
            ),
            3.0
        );
        assert_eq!(
            effective_extended_linear_headroom(
                requested,
                explicit,
                OutputHeadroomState::reported(2.0, true),
            ),
            2.0
        );
    }

    #[test]
    fn legacy_apple_edr_request_does_not_activate_android_scrgb() {
        let selection = select_wgpu_surface_output(
            OutputMode::apple_edr(4.0),
            SurfaceOutputCapabilities {
                extended_linear: true,
                direct_composition: true,
                desired_headroom: 4.0,
                fallback_reason: OutputFallbackReason::None,
            },
            wgpu::Backend::Vulkan,
            &[
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureFormat::Rgba16Float,
            ],
        )
        .unwrap();

        assert_eq!(selection.format, wgpu::TextureFormat::Rgba8Unorm);
        assert!(!selection.output.extended_linear);
        assert_eq!(
            selection.fallback_reason,
            OutputFallbackReason::LegacyAppleEdrUnsupported
        );
    }

    #[test]
    fn planar_upload_preserves_p010_bytes_when_16bit_norm_is_missing() {
        let pack = |codes: &[u16]| {
            codes
                .iter()
                .flat_map(|code| (*code << 6).to_le_bytes())
                .collect::<Vec<_>>()
        };
        let p010 = PlanarFrame {
            format: PlanarPixelFormat::P010,
            width: 2,
            height: 2,
            luma: pack(&[64, 940, 512, 1023]),
            chroma: pack(&[512, 960]),
        };
        let mut uniforms =
            VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);
        uniforms.source_transfer = 77;
        uniforms.nits = [1_000.0, 100.0, 203.0, 100.0];

        let native = prepare_planar_upload(p010.clone(), uniforms, true);
        let mut expected_native_uniforms = uniforms;
        expected_native_uniforms.is_p010 = 1;
        assert_eq!(native.path, PlanarUploadPath::Native);
        assert_eq!(native.frame, p010);
        assert_eq!(native.uniforms, expected_native_uniforms);

        let original_luma = p010.luma.as_ptr();
        let fallback = prepare_planar_upload(p010.clone(), uniforms, false);
        let expected_fallback_uniforms = expected_native_uniforms.p010_byte_planes(true);
        assert_eq!(fallback.path, PlanarUploadPath::GpuP010Bytes);
        assert_eq!(fallback.frame, p010);
        assert_eq!(fallback.uniforms, expected_fallback_uniforms);
        let moved = prepare_planar_upload(p010, uniforms, false);
        assert_eq!(moved.frame.luma.as_ptr(), original_luma);

        let nv12 = fallback.frame.downconvert_p010_to_nv12().unwrap();
        let native_nv12 = prepare_planar_upload(nv12.clone(), fallback.uniforms, false);
        assert_eq!(native_nv12.path, PlanarUploadPath::Native);
        assert_eq!(native_nv12.frame, nv12);
        assert_eq!(native_nv12.uniforms.is_p010, 0);
        assert!(!native_nv12.uniforms.has_p010_byte_planes());
    }

    #[test]
    fn aspect_fit_viewport_letterboxes_and_pillarboxes() {
        assert_eq!(
            aspect_fit_viewport(1920, 1080, 1000, 1000),
            PresentationViewport {
                x: 0.0,
                y: 218.75,
                width: 1000.0,
                height: 562.5,
            }
        );
        assert_eq!(
            aspect_fit_viewport(1080, 1920, 1000, 1000),
            PresentationViewport {
                x: 218.75,
                y: 0.0,
                width: 562.5,
                height: 1000.0,
            }
        );
    }

    #[test]
    fn wgpu_renderer_clears_offscreen_target_to_expected_color() {
        let mut renderer = WgpuRenderer::new().unwrap();
        let color = WgpuClearColor::new(0.25, 0.5, 0.75, 1.0);

        let readback = renderer.clear_offscreen(4, 3, color).unwrap();

        assert_eq!(readback.width, 4);
        assert_eq!(readback.height, 3);
        assert_eq!(readback.rgba.len(), 4 * 3 * 4);
        let expected = [
            to_u8(color.red),
            to_u8(color.green),
            to_u8(color.blue),
            to_u8(color.alpha),
        ];
        for y in 0..readback.height {
            for x in 0..readback.width {
                let pixel = readback.pixel(x, y);
                // Allow a tolerance of 1 LSB for rounding differences across drivers.
                for channel in 0..4 {
                    let delta = (pixel[channel] as i16 - expected[channel] as i16).unsigned_abs();
                    assert!(
                        delta <= 1,
                        "pixel ({x},{y}) channel {channel} = {} expected ~{}",
                        pixel[channel],
                        expected[channel]
                    );
                }
            }
        }
        assert_eq!(renderer.stats().offscreen_frames, 1);
    }

    #[test]
    fn wgpu_renderer_render_test_frame_without_surface_uses_offscreen_path() {
        let mut renderer = WgpuRenderer::new().unwrap();

        renderer.render_test_frame(0.0).unwrap();

        let stats = renderer.stats();
        assert_eq!(stats.rendered_frames, 1);
        assert_eq!(stats.offscreen_frames, 1);
        assert!(!stats.attached);
    }

    #[test]
    fn renderer_backend_capture_uses_requested_size_and_aspect_fit() {
        let mut renderer = WgpuRenderer::new().unwrap();
        let uniforms =
            VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);
        renderer
            .upload_nv12(4, 2, &[235; 8], &[128; 4], uniforms)
            .unwrap();

        let capture = RendererBackend::capture_current_frame(
            &mut renderer,
            RenderFrameContext::new(Duration::ZERO, 1).output_size(4, 4),
            4,
            4,
        )
        .unwrap()
        .expect("current wgpu frame captured");

        assert_eq!((capture.width, capture.height), (4, 4));
        assert_eq!(capture.rgba.len(), 4 * 4 * 4);
        assert_eq!(&capture.rgba[0..4], &[0, 0, 0, 255]);
        assert!(capture.rgba[4 * 4 + 0] > 200);
        assert_eq!(renderer.stats().offscreen_frames, 1);
    }

    #[test]
    fn transition_snapshot_remains_available_for_capture() {
        let mut renderer = WgpuRenderer::new().unwrap();
        let uniforms =
            VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);
        renderer
            .upload_nv12(4, 2, &[235; 8], &[128; 4], uniforms)
            .unwrap();

        RendererBackend::preserve_current_frame_for_track_transition(&mut renderer).unwrap();
        RendererBackend::preserve_current_frame_for_transition(&mut renderer).unwrap();

        let capture = RendererBackend::capture_current_frame(
            &mut renderer,
            RenderFrameContext::new(Duration::from_secs(15), 2).output_size(4, 4),
            4,
            4,
        )
        .unwrap()
        .expect("transition snapshot remains capturable");

        assert_eq!((capture.width, capture.height), (4, 4));
        assert_eq!(capture.rgba.len(), 4 * 4 * 4);
        assert_eq!(&capture.rgba[0..4], &[0, 0, 0, 255]);
        assert!(capture.rgba[4 * 4 + 0] > 200);
    }

    #[test]
    fn full_clear_removes_current_frame_from_capture() {
        let mut renderer = WgpuRenderer::new().unwrap();
        let uniforms =
            VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);
        renderer
            .upload_nv12(4, 2, &[235; 8], &[128; 4], uniforms)
            .unwrap();

        RendererBackend::clear_current_frame(&mut renderer).unwrap();

        let capture = RendererBackend::capture_current_frame(
            &mut renderer,
            RenderFrameContext::new(Duration::ZERO, 1).output_size(4, 4),
            4,
            4,
        )
        .unwrap();
        assert!(capture.is_none());
    }

    #[test]
    fn wgpu_renderer_composites_rgba_overlay_pixels() {
        let mut renderer = WgpuRenderer::new().unwrap();
        let uniforms =
            VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);
        renderer
            .upload_nv12(4, 4, &[16; 16], &[128; 8], uniforms)
            .unwrap();

        let overlay = OverlayFrame {
            pts: Duration::ZERO,
            viewport: crate::overlay::OverlayViewport::new(4, 4),
            subtitle_planes: vec![crate::subtitle::SubtitleBitmapPlane::new(
                1,
                1,
                2,
                2,
                vec![255, 0, 0, 255].repeat(4),
            )],
            subtitle_alpha_planes: Vec::new(),
            subtitle_changed: true,
        };
        let capture = renderer
            .render_current_offscreen(Some(&overlay))
            .unwrap()
            .expect("current wgpu frame captured with overlay");

        assert_eq!(capture.pixel(0, 0), [0, 0, 0, 255]);
        assert_eq!(capture.pixel(1, 1), [255, 0, 0, 255]);
        assert_eq!(capture.pixel(2, 2), [255, 0, 0, 255]);
        assert_eq!(capture.pixel(3, 3), [0, 0, 0, 255]);
    }

    #[test]
    fn wgpu_renderer_prepares_danmaku_glyph_atlas_draws_and_reuses_cache() {
        let mut renderer = WgpuRenderer::new().unwrap();
        renderer.ensure_overlay_pipeline(OFFSCREEN_FORMAT);
        let atlas = DanmakuGlyphAtlas {
            width: 4,
            height: 4,
            stride: 4,
            fill_alpha: vec![255; 16],
            outline_alpha: vec![64; 16],
            version: 42,
            update: None,
        };
        let plan = DanmakuRenderPlan {
            media_time: Duration::from_millis(10),
            generation: 7,
            viewport: DanmakuViewport::new(32, 18),
            atlas: Some(std::sync::Arc::new(atlas.clone())),
            items: vec![DanmakuGlyphInstance {
                item_id: 1,
                rect: [1.0, 2.0, 4.0, 4.0],
                tex_rect: [0.0, 0.0, 1.0, 1.0],
                color_rgba: [1.0, 1.0, 1.0, 1.0],
                outline_rgba: [0.0, 0.0, 0.0, 0.75],
                shadow_rgba: [0.0, 0.0, 0.0, 0.0],
                shadow_offset: [1.0, 1.0],
            }],
            frame_stats: DanmakuFrameStats::default(),
        };

        let draws = renderer
            .prepare_danmaku_draws(&plan, OutputDescription::sdr())
            .unwrap();
        assert_eq!(draws.len(), 2);
        assert!(
            renderer
                .danmaku_atlas_cache
                .as_ref()
                .is_some_and(|cache| cache.can_reuse_for(&atlas))
        );

        let cached_draws = renderer
            .prepare_danmaku_draws(&plan, OutputDescription::sdr())
            .unwrap();
        assert_eq!(cached_draws.len(), 2);
        assert!(
            renderer
                .danmaku_atlas_cache
                .as_ref()
                .is_some_and(|cache| cache.can_reuse_for(&atlas))
        );
        let cached_fill_texture = renderer
            .danmaku_atlas_cache
            .as_ref()
            .expect("danmaku atlas cache")
            .fill_texture
            .clone();

        let mut updated_atlas = atlas.clone();
        updated_atlas.version = 43;
        updated_atlas.fill_alpha[5] = 128;
        updated_atlas.outline_alpha[5] = 192;
        updated_atlas.update = Some(DanmakuAtlasUpdate {
            from_version: 42,
            x: 1,
            y: 1,
            width: 1,
            height: 1,
        });
        let updated_plan = DanmakuRenderPlan {
            atlas: Some(std::sync::Arc::new(updated_atlas.clone())),
            ..plan.clone()
        };
        let updated_draws = renderer
            .prepare_danmaku_draws(&updated_plan, OutputDescription::sdr())
            .unwrap();
        assert_eq!(updated_draws.len(), 2);
        let updated_cache = renderer
            .danmaku_atlas_cache
            .as_ref()
            .expect("updated danmaku atlas cache");
        assert!(updated_cache.can_reuse_for(&updated_atlas));
        assert_eq!(updated_cache.fill_texture, cached_fill_texture);
    }

    #[test]
    fn wgsl_gamut_lut_samples_the_target_black_to_peak_axis() {
        // libplacebo's LUT I axis is [target black, target peak]; sampling
        // `ipt.x / peak` again would diverge from the generated LUT. The
        // lattice position must also be remapped to the texel-center
        // coordinate (libplacebo's `texel_scale`), or the zero-chroma texel at
        // the low end of the C axis crushes saturation.
        let source = include_str!("wgpu_video.wgsl");
        assert!(source.contains("let lut_min = pq_code(uniforms.tone_map_extra.z);"));
        assert!(source.contains("clamp((ipt.x - lut_min) / lut_range, 0.0, 1.0)"));
        assert!(source.contains("pos.y * (31.0 / 32.0) + 0.5 / 32.0"));
        assert!(source.contains("pos.x * (47.0 / 48.0) + 0.5 / 48.0"));
    }

    #[test]
    fn wgpu_renderer_rejects_metal_surface() {
        let mut renderer = WgpuRenderer::new().unwrap();
        let result = renderer.attach_surface(PlatformSurface::Metal(MetalSurfaceHandle::new(
            42, 640, 360, 2.0,
        )));

        assert!(matches!(result, Err(PlayerError::Renderer(_))));
    }

    // --- Video pipeline parity oracle ---------------------------------------
    //
    // `reference_pixel` is a CPU port of the WGSL `erika_video_fragment` (which is
    // itself a port of the Metal `VIDEO_SHADER_SOURCE`). Asserting the GPU output
    // matches this reference proves the wgpu backend computes the same color math
    // as the native Metal renderer for the same uniforms.

    fn ref_pq_eotf(encoded: f32) -> f32 {
        let m1 = 0.1593017578125;
        let m2 = 78.84375;
        let c1 = 0.8359375;
        let c2 = 18.8515625;
        let c3 = 18.6875;
        let p = encoded.max(0.0).powf(1.0 / m2);
        let num = (p - c1).max(0.0);
        let den = (c2 - c3 * p).max(0.000001);
        (num / den).powf(1.0 / m1)
    }

    fn ref_pq_inverse_eotf(normalized_nits: f32) -> f32 {
        let m1 = 0.1593017578125;
        let m2 = 78.84375;
        let c1 = 0.8359375;
        let c2 = 18.8515625;
        let c3 = 18.6875;
        let p = normalized_nits.clamp(0.0, 1.0).powf(m1);
        ((c1 + c2 * p) / (1.0 + c3 * p).max(0.000001)).powf(m2)
    }

    fn ref_transfer_to_source_linear(rgb: [f32; 3], u: &VideoUniforms) -> [f32; 3] {
        let rgb = rgb.map(|c| c.max(0.0));
        match u.source_transfer {
            3 => {
                let peak = u.nits[2].max(1.0);
                rgb.map(|c| ref_pq_eotf(c) * (10000.0 / peak))
            }
            1 => rgb.map(|c| c.powf(2.2)),
            2 => rgb.map(|c| c.powf(2.4)),
            _ => rgb,
        }
    }

    fn ref_gamut(rgb: [f32; 3], u: &VideoUniforms) -> [f32; 3] {
        let m = u.gamut_matrix_rows;
        [
            m[0][0] * rgb[0] + m[0][1] * rgb[1] + m[0][2] * rgb[2],
            m[1][0] * rgb[0] + m[1][1] * rgb[1] + m[1][2] * rgb[2],
            m[2][0] * rgb[0] + m[2][1] * rgb[1] + m[2][2] * rgb[2],
        ]
    }

    fn ref_tone_map(nits: [f32; 3], u: &VideoUniforms) -> [f32; 3] {
        // Modeled for the configurations this suite renders: Clip (code 0) and
        // BT.2390 (code 3), which is the identity once the source peak already
        // fits the target. Reinhard (1) and Mobius (2) are deliberately not
        // approximated here — the shader evaluates them in IPT space with the
        // curve in linear light, so a nits-domain stand-in would silently
        // disagree with it. `wgpu_reinhard_and_mobius_run_in_linear_luminance`
        // covers those against an independent reference instead.
        assert!(
            matches!(u.tone_map, 0 | 3),
            "ref_tone_map models only clip and BT.2390, not code {}",
            u.tone_map
        );
        let target_peak = u.nits[1].max(1.0);
        let x = nits.map(|n| n.max(0.0) / target_peak);
        x.map(|xi| target_peak * xi.clamp(0.0, 1.0))
    }

    fn ref_output(rgb: [f32; 3], u: &VideoUniforms) -> [f32; 3] {
        if u.target_transfer == 3 {
            let pq_absolute_peak_nits = 10000.0;
            let target_white = u.nits[3].max(1.0);
            return rgb
                .map(|c| ref_pq_inverse_eotf(c.max(0.0) * target_white / pq_absolute_peak_nits));
        }
        if u.edr_output != 0 {
            return rgb.map(|c| c.max(0.0));
        }
        match u.target_transfer {
            1 => rgb.map(|c| c.max(0.0).powf(1.0 / 2.2)),
            2 => rgb.map(|c| c.max(0.0).powf(1.0 / 2.4)),
            _ => rgb,
        }
    }

    fn ref_final(rgb: [f32; 3], u: &VideoUniforms) -> [f32; 3] {
        if u.target_transfer == 3 {
            return rgb.map(|c| c.clamp(0.0, 1.0));
        }
        if u.edr_output != 0 {
            let headroom = (u.nits[1].max(1.0) / u.nits[3].max(1.0)).max(1.0);
            rgb.map(|c| c.clamp(0.0, headroom))
        } else {
            rgb.map(|c| c.clamp(0.0, 1.0))
        }
    }

    fn reference_pixel(y: f32, cb: f32, cr: f32, u: &VideoUniforms) -> [f32; 3] {
        let (yy, cbcr) = if u.full_range != 0 {
            (y, [cb - 0.5, cr - 0.5])
        } else if u.is_p010 != 0 {
            (
                (y - 64.0 / 1023.0) * (1023.0 / 876.0),
                [
                    (cb - 512.0 / 1023.0) * (1023.0 / 896.0),
                    (cr - 512.0 / 1023.0) * (1023.0 / 896.0),
                ],
            )
        } else {
            (
                (y - 16.0 / 255.0) * (255.0 / 219.0),
                [
                    (cb - 128.0 / 255.0) * (255.0 / 224.0),
                    (cr - 128.0 / 255.0) * (255.0 / 224.0),
                ],
            )
        };
        let kr = u.luma_coefficients[0];
        let kg = u.luma_coefficients[1].max(0.000001);
        let kb = u.luma_coefficients[2];
        let r = yy + 2.0 * (1.0 - kr) * cbcr[1];
        let b = yy + 2.0 * (1.0 - kb) * cbcr[0];
        let g = (yy - kr * r - kb * b) / kg;
        let mut rgb = [r, g, b];
        rgb = ref_transfer_to_source_linear(rgb, u);
        rgb = ref_gamut(rgb, u);
        let srw = u.nits[2].max(1.0);
        rgb = rgb.map(|c| c.max(0.0) * srw);
        rgb = ref_tone_map(rgb, u);
        let trw = u.nits[3].max(1.0);
        rgb = rgb.map(|c| c.max(0.0) / trw);
        rgb = ref_output(rgb, u);
        ref_final(rgb, u)
    }

    fn build_solid_nv12(width: u32, height: u32, y: u8, cb: u8, cr: u8) -> (Vec<u8>, Vec<u8>) {
        let luma = vec![y; (width * height) as usize];
        let chroma_pixels = width.div_ceil(2) as usize * height.div_ceil(2) as usize;
        let mut chroma = Vec::with_capacity(chroma_pixels * 2);
        for _ in 0..chroma_pixels {
            chroma.push(cb);
            chroma.push(cr);
        }
        (luma, chroma)
    }

    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    #[test]
    #[ignore = "requires the optional WSL D3D12 bridge, patched Mesa and a hardware video fixture"]
    fn linux_wsl_gpu_copy_matches_decoded_pixels() {
        use crate::ffmpeg::{Decoder, DecoderConfig, DecoderOutputFrame, Demuxer};
        let fixture = std::env::var("ERIKA_WSL_TEST_VIDEO")
            .expect("set ERIKA_WSL_TEST_VIDEO (video stream 0)");
        let mut demuxer = Demuxer::open_path(fixture).unwrap();
        let mut decoder =
            Decoder::open_with_config(demuxer.codec_parameters(0).unwrap(), DecoderConfig::vaapi())
                .unwrap();
        let mut renderer = WgpuRenderer::new().unwrap();
        assert_eq!(renderer.adapter_info().backend, wgpu::Backend::Vulkan);
        let mut interop = crate::renderer::linux_vulkan::LinuxVulkanInterop::new(
            &renderer.adapter,
            &renderer.device,
        )
        .unwrap();
        let mut checked = 0;
        'packets: while let Some(packet) = demuxer.read_packet().unwrap() {
            if packet.stream_index() != 0 {
                continue;
            }
            decoder.send_packet(&packet).unwrap();
            while let DecoderOutputFrame::Frame(frame) = decoder.receive_frame().unwrap() {
                // Explicit validation readback only; production import never
                // calls transfer_to_system_memory.
                let cpu = frame
                    .transfer_to_system_memory()
                    .unwrap()
                    .to_planar_frame()
                    .unwrap();
                let uniforms = VideoUniforms::from_pipeline(
                    &VideoRenderPipeline::sdr_default(),
                    cpu.format == PlanarPixelFormat::P010,
                    false,
                );
                renderer.upload_planar(cpu, uniforms).unwrap();
                let expected = renderer
                    .render_current_offscreen_sized(256, 144, None, None)
                    .unwrap()
                    .unwrap();
                let imported = interop.import_wsl_copy(&frame).unwrap();
                let [luma, chroma] = imported.planes.clone();
                renderer.current_video = Some(UploadedVideoFrame {
                    textures: UploadedVideoTextures::Planar { luma, chroma },
                    width: frame.width(),
                    height: frame.height(),
                    uniforms,
                    source_color: None,
                    frame_token: renderer.next_upload_serial(),
                    _linux_direct: Some(imported),
                });
                for _ in 0..2 {
                    let actual = renderer
                        .render_current_offscreen_sized(256, 144, None, None)
                        .unwrap()
                        .unwrap();
                    for (i, (a, b)) in actual.rgba.iter().zip(&expected.rgba).enumerate() {
                        assert!(
                            a.abs_diff(*b) <= 2,
                            "frame={checked} byte={i}: GPU={a}, reference={b}"
                        );
                    }
                }
                checked += 1;
                if checked == 48 {
                    break 'packets;
                }
            }
        }
        assert_eq!(checked, 48, "fixture must contain at least 48 frames");
        // Drop with the last imported frame still displayed.
        drop(renderer);
        drop(interop);
    }

    #[cfg(all(target_os = "linux", not(target_env = "ohos"), erika_test_linux_vulkan))]
    #[test]
    fn linux_direct_vulkan_sampling_preserves_pixels_and_releases_frames() {
        // Explicitly enabled by check_linux_zero_copy.sh: never silently skip
        // missing Vulkan/interop support and count that as hardware coverage.
        let mut renderer = WgpuRenderer::new().unwrap();
        assert_eq!(renderer.adapter_info().backend, wgpu::Backend::Vulkan);
        let interop = crate::renderer::linux_vulkan::LinuxVulkanInterop::new(
            &renderer.adapter,
            &renderer.device,
        )
        .unwrap();
        for p010 in [false, true, false, true] {
            let bytes = |v: u8| if p010 { vec![0, v] } else { vec![v] };
            let luma: Vec<u8> = (0..8)
                .flat_map(|_| (0..16).flat_map(|x| bytes(32 + x * 12)))
                .collect();
            let chroma: Vec<u8> = (0..4)
                .flat_map(|_| (0..16).flat_map(|x| bytes(if x % 2 == 0 { 90 } else { 170 })))
                .collect();
            let uniforms =
                VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), p010, false);
            renderer
                .upload_planar(
                    PlanarFrame {
                        format: if p010 {
                            PlanarPixelFormat::P010
                        } else {
                            PlanarPixelFormat::Nv12
                        },
                        width: 16,
                        height: 8,
                        luma,
                        chroma,
                    },
                    uniforms,
                )
                .unwrap();
            let expected = renderer.render_current_offscreen(None).unwrap().unwrap();
            let (direct, producer) = interop.direct_fixture(p010);
            let [luma, chroma] = direct.planes.clone();
            renderer.current_video = Some(UploadedVideoFrame {
                textures: UploadedVideoTextures::Planar { luma, chroma },
                width: 16,
                height: 8,
                uniforms,
                source_color: None,
                frame_token: renderer.next_upload_serial(),
                _linux_direct: Some(direct),
            });
            // Redraw the same held decoder images; also exercises screenshot
            // readback while the original producer reference is retained.
            for _ in 0..3 {
                let actual = renderer.render_current_offscreen(None).unwrap().unwrap();
                for y in 0..8 {
                    for x in 0..16 {
                        assert_eq!(
                            actual.pixel(x, y),
                            expected.pixel(x, y),
                            "p010={p010}, ({x},{y})"
                        );
                    }
                }
            }
            renderer.current_video = None;
            producer.check_released();
        }
        // Teardown with a live imported picture, without an explicit clear.
        let (direct, producer) = interop.direct_fixture(false);
        let [luma, chroma] = direct.planes.clone();
        renderer.current_video = Some(UploadedVideoFrame {
            textures: UploadedVideoTextures::Planar { luma, chroma },
            width: 16,
            height: 8,
            uniforms: VideoUniforms::from_pipeline(
                &VideoRenderPipeline::sdr_default(),
                false,
                false,
            ),
            source_color: None,
            frame_token: renderer.next_upload_serial(),
            _linux_direct: Some(direct),
        });
        let target = renderer.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("direct-frame-in-flight-teardown"),
            size: wgpu::Extent3d {
                width: 16,
                height: 8,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        renderer
            .draw_current_video(
                &target.create_view(&Default::default()),
                16,
                8,
                None,
                None,
                OutputDescription::sdr(),
            )
            .unwrap();
        drop(renderer);
        producer.check_released();
    }

    /// Renders a solid grey NV12 sample and returns the readback's red
    /// channel. The uniforms below keep the shader output in
    /// target-reference-linear space, so the channel value is
    /// `255 * nits / target_reference_white`.
    fn render_grey_sample(
        renderer: &mut WgpuRenderer,
        uniforms: &VideoUniforms,
        tone_map: u32,
        y: u8,
    ) -> u8 {
        let mut uniforms = *uniforms;
        uniforms.tone_map = tone_map;
        let (luma, chroma) = build_solid_nv12(4, 4, y, 128, 128);
        let out = renderer
            .render_nv12_offscreen(4, 4, &luma, &chroma, uniforms)
            .unwrap();
        out.pixel(1, 1)[0]
    }

    #[test]
    fn wgpu_reinhard_and_mobius_run_in_linear_luminance() {
        // The PR review's readback case: a 1000-nit source peak mapped onto a
        // 203-nit target, so the Mobius knee sits at 0.3 * 203 = 60.9 nits.
        // libplacebo evaluates Reinhard and Mobius in the linear PL_HDR_NORM
        // domain; running them on PQ codes instead moved the knee far lower
        // and darkened a 50-nit patch from ~50 to ~31 nits.
        let mut renderer = WgpuRenderer::new().unwrap();
        let sdr = VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);

        let mut uniforms = sdr;
        uniforms.full_range = 1;
        uniforms.source_transfer = 0;
        uniforms.target_transfer = 0;
        uniforms.scene_linear = 1;
        uniforms.nits = [1000.0, 203.0, 203.0, 203.0];
        uniforms.tone_map_extra = [0.0, 0.0, 0.0, 0.0];
        uniforms.gamut_lut_enabled = 0;
        uniforms.luma_coefficients = [0.2126, 0.7152, 0.0722, 0.0];
        uniforms.gamut_matrix_rows = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ];

        // Y = 63 is 50.15 nits in this configuration, below the knee.
        let below_knee_y = 63u8;
        let clip = render_grey_sample(&mut renderer, &uniforms, 0, below_knee_y);
        let mobius = render_grey_sample(&mut renderer, &uniforms, 2, below_knee_y);
        let reinhard = render_grey_sample(&mut renderer, &uniforms, 1, below_knee_y);

        assert!(
            (clip as i16 - 63).abs() <= 2,
            "clip should reproduce the 50.15-nit input, got {clip}"
        );
        // Mobius' 1:1 region covers everything below the knee, so this sample
        // must come back untouched. A PQ-domain curve would return ~39 here.
        assert!(
            (mobius as i16 - clip as i16).abs() <= 1,
            "mobius below the knee = {mobius}, clip = {clip}"
        );
        // Reinhard compresses the whole range, so this sample darkens a
        // little, but only a little.
        assert!(
            (clip as i16 - reinhard as i16) >= 1 && (clip as i16 - reinhard as i16) <= 3,
            "reinhard below the knee = {reinhard}, clip = {clip}"
        );

        // Independent reference for the readback. libplacebo evaluates both
        // curves in linear light, so the sample's nits go through the curve
        // and come back as `255 * mapped_nits / target_reference_white`; the
        // CPU tests in `renderer::pipeline` pin the same formulas. The
        // PQ-domain port this replaces would miss these by tens of LSBs.
        let expected = |nits: f32, tone_map: u32| -> i16 {
            let (src_peak, dst_peak, dst_black) = (1000.0_f32, 203.0_f32, 0.0_f32);
            let out_range = dst_peak - dst_black;
            let peak = src_peak / out_range;
            let t = nits / out_range;
            let mapped = match tone_map {
                1 => {
                    // Reinhard, contrast 0.5 -> offset 1.
                    let offset = 1.0;
                    let scale = (peak + offset) / peak;
                    t / (t + offset) * scale
                }
                2 => {
                    let j = 0.3;
                    let a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
                    let b = (j * j - 2.0 * j * peak + peak) / (peak - 1.0);
                    let scale = (b * b + 2.0 * b * j + j * j) / (b - a);
                    if t > j { scale * (t + a) / (t + b) } else { t }
                }
                _ => t,
            };
            (255.0 * mapped).round() as i16
        };

        for (y, tone_map) in [(below_knee_y, 1u32), (below_knee_y, 2), (200, 1), (200, 2)] {
            let nits = f32::from(y) / 255.0 * 203.0;
            let want = expected(nits, tone_map);
            let got = i16::from(render_grey_sample(&mut renderer, &uniforms, tone_map, y));
            assert!(
                (got - want).abs() <= 2,
                "y={y} tone_map={tone_map} ({nits} nits) = {got}, expected {want}"
            );
        }
    }

    #[test]
    fn wgpu_video_nv12_matches_cpu_reference() {
        let mut renderer = WgpuRenderer::new().unwrap();

        let sdr = VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);
        assert_eq!(sdr.source_transfer, 1);
        assert_eq!(sdr.nits[2], 100.0);

        // A full-range BT.709 identity configuration: linear in/out, clip tone map,
        // matched nits, identity gamut. Output should be the plain clamped YCbCr->RGB.
        let mut identity = sdr;
        identity.full_range = 1;
        identity.source_transfer = 0;
        identity.target_transfer = 0;
        identity.tone_map = 0;
        identity.nits = [100.0, 100.0, 100.0, 100.0];
        identity.luma_coefficients = [0.2126, 0.7152, 0.0722, 0.0];
        identity.gamut_matrix_rows = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ];

        let mut pq_target = identity;
        pq_target.target_transfer = 3;
        pq_target.nits = [1000.0, 10000.0, 203.0, 203.0];

        let samples = [
            (16u8, 128u8, 128u8),
            (128, 128, 128),
            (200, 90, 160),
            (80, 200, 64),
            (235, 128, 128),
        ];

        for uniforms in [sdr, identity, pq_target] {
            for (y, cb, cr) in samples {
                let (luma, chroma) = build_solid_nv12(4, 4, y, cb, cr);
                let out = renderer
                    .render_nv12_offscreen(4, 4, &luma, &chroma, uniforms)
                    .unwrap();

                let expect = reference_pixel(
                    f32::from(y) / 255.0,
                    f32::from(cb) / 255.0,
                    f32::from(cr) / 255.0,
                    &uniforms,
                );
                let expected = [
                    to_u8(f64::from(expect[0])),
                    to_u8(f64::from(expect[1])),
                    to_u8(f64::from(expect[2])),
                    255,
                ];

                for py in 0..out.height {
                    for px in 0..out.width {
                        let pixel = out.pixel(px, py);
                        for channel in 0..4 {
                            let delta =
                                (pixel[channel] as i16 - expected[channel] as i16).unsigned_abs();
                            assert!(
                                delta <= 2,
                                "ycbcr ({y},{cb},{cr}) full_range={} pixel ch{channel} = {} expected ~{}",
                                uniforms.full_range,
                                pixel[channel],
                                expected[channel]
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn wgpu_renderer_is_usable_as_dyn_backend_and_reports_no_current_frame() {
        let mut renderer = WgpuRenderer::new().unwrap();
        // The presenter holds the backend as `Box<dyn RendererBackend>`; confirm the
        // wgpu renderer is object-safe through the trait and reports no current frame
        // so the presenter falls back to a test frame.
        let backend: &mut dyn RendererBackend = &mut renderer;
        assert!(
            !backend
                .render_current_frame(RenderFrameContext::new(Duration::ZERO, 1))
                .unwrap()
        );
    }

    #[test]
    fn wgpu_uploads_and_renders_p010_frame() {
        let mut renderer = WgpuRenderer::new().unwrap();

        // 4x4 P010 frame: bright luma, neutral chroma. Samples are 10-bit values
        // MSB-aligned in 16-bit LE (code << 6), matching `Frame::to_planar_frame`.
        let luma_sample: u16 = 700 << 6;
        let chroma_sample: u16 = 512 << 6;
        let luma: Vec<u8> = std::iter::repeat(luma_sample)
            .take(4 * 4)
            .flat_map(u16::to_le_bytes)
            .collect();
        let chroma: Vec<u8> = std::iter::repeat(chroma_sample)
            .take(2 * 2 * 2)
            .flat_map(u16::to_le_bytes)
            .collect();

        let uniforms =
            VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), true, false);
        renderer
            .upload_planar(
                PlanarFrame {
                    format: PlanarPixelFormat::P010,
                    width: 4,
                    height: 4,
                    luma,
                    chroma,
                },
                uniforms,
            )
            .unwrap();

        let readback = renderer
            .render_current_offscreen(None)
            .unwrap()
            .expect("p010 frame rendered");
        assert_eq!(readback.width, 4);
        assert_eq!(readback.height, 4);
        // A bright luma frame must not render fully black.
        assert!(readback.rgba.iter().any(|&byte| byte > 0));
    }

    #[test]
    fn p010_byte_planes_preserve_low_bits_and_reuse_upload_storage() {
        let mut renderer = WgpuRenderer::new().unwrap();
        // Exercise the portable representation even on adapters with R16 UNORM.
        renderer.supports_16bit_norm = false;
        let pack = |codes: &[u16]| {
            codes
                .iter()
                .flat_map(|code| (code << 6).to_le_bytes())
                .collect::<Vec<_>>()
        };
        let mut uniforms =
            VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), true, false);
        uniforms.source_transfer = 0;
        uniforms.target_transfer = 0;
        uniforms.tone_map = 0;
        let codes = [
            64, 67, 68, 71, 253, 254, 255, 256, 509, 510, 511, 512, 937, 938, 939, 940,
        ];
        let frame = PlanarFrame {
            format: PlanarPixelFormat::P010,
            width: 4,
            height: 4,
            luma: pack(&codes),
            chroma: pack(&[512; 8]),
        };
        renderer.upload_planar(frame.clone(), uniforms).unwrap();
        let video = renderer.current_video.as_ref().unwrap();
        let UploadedVideoTextures::Planar { luma, chroma } = &video.textures else {
            panic!()
        };
        let original_textures = (luma.clone(), chroma.clone());
        assert_eq!(luma.format(), wgpu::TextureFormat::Rg8Unorm);
        assert_eq!(chroma.format(), wgpu::TextureFormat::Rgba8Unorm);
        let readback = renderer.render_current_offscreen(None).unwrap().unwrap();
        for (i, code) in codes.iter().enumerate() {
            let expected = ((*code as f32 - 64.0) / 876.0 * 255.0).round() as u8;
            let pixel = readback.pixel(i as u32 % 4, i as u32 / 4);
            for value in &pixel[..3] {
                assert!(
                    value.abs_diff(expected) <= 1,
                    "code={code}, pixel={pixel:?}, expected={expected}"
                );
            }
        }
        // These codes share the high byte; dropping the two low signal bits
        // would make the pixels identical.
        assert!(readback.pixel(1, 0)[0] > readback.pixel(0, 0)[0]);
        let mut next = frame;
        next.luma = pack(&[940; 16]);
        for _ in 0..3 {
            renderer.upload_planar(next.clone(), uniforms).unwrap();
            let readback = renderer.render_current_offscreen(None).unwrap().unwrap();
            assert!(readback.pixel(0, 0)[0] >= 254);
        }
        let video = renderer.current_video.as_ref().unwrap();
        let UploadedVideoTextures::Planar { luma, chroma } = &video.textures else {
            panic!()
        };
        assert_eq!(luma, &original_textures.0);
        assert_eq!(chroma, &original_textures.1);
        let next_readback = renderer.render_current_offscreen(None).unwrap().unwrap();
        assert!(next_readback.pixel(0, 0)[0] >= 254);
        assert_eq!(readback.pixel(0, 0)[0], 0);

        // Check scaled samples against code values, independently of the native
        // R16 path: both representations must not share an interpolation bug.
        renderer
            .upload_planar(
                PlanarFrame {
                    format: PlanarPixelFormat::P010,
                    width: 4,
                    height: 4,
                    luma: pack(&codes),
                    chroma: pack(&[512; 8]),
                },
                uniforms,
            )
            .unwrap();
        let scaled = renderer
            .render_current_offscreen_sized(12, 12, None, None)
            .unwrap()
            .unwrap();
        for py in 0..12 {
            for px in 0..12 {
                let x = (f64::from(px) + 0.5) / 3.0 - 0.5;
                let y = (f64::from(py) + 0.5) / 3.0 - 0.5;
                let code = |dx: i32, dy: i32| {
                    let ix = (x.floor() as i32 + dx).clamp(0, 3) as usize;
                    let iy = (y.floor() as i32 + dy).clamp(0, 3) as usize;
                    f64::from(codes[iy * 4 + ix])
                };
                let fx = x - x.floor();
                let fy = y - y.floor();
                let top = code(0, 0) * (1.0 - fx) + code(1, 0) * fx;
                let bottom = code(0, 1) * (1.0 - fx) + code(1, 1) * fx;
                let signal = top * (1.0 - fy) + bottom * fy;
                let expected = ((signal - 64.0) / 876.0 * 255.0).clamp(0.0, 255.0).round() as u8;
                let pixel = scaled.pixel(px, py);
                for value in &pixel[..3] {
                    assert!(
                        value.abs_diff(expected) <= 1,
                        "scaled ({px},{py}), signal={signal}, pixel={pixel:?}, expected={expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn p010_byte_planes_match_native_sampling_when_scaled() {
        use crate::TransferFunction;
        let mut renderer = WgpuRenderer::new().unwrap();
        if !renderer.supports_16bit_norm {
            return;
        }
        let pack = |codes: &[u16]| {
            codes
                .iter()
                .flat_map(|code| (code << 6).to_le_bytes())
                .collect::<Vec<_>>()
        };
        let frame = PlanarFrame {
            format: PlanarPixelFormat::P010,
            width: 4,
            height: 4,
            luma: pack(&[
                65, 511, 256, 937, 102, 723, 413, 799, 133, 467, 331, 871, 83, 512, 523, 930,
            ]),
            chroma: pack(&[400, 602, 703, 511, 501, 689, 543, 376]),
        };
        // Includes tone mapping, output-uniform reconstruction and packed alpha.
        for source in [
            SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Bt1886),
            SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq),
        ] {
            for alpha in [false, true] {
                let pipeline = VideoRenderPipeline::new(
                    source,
                    OutputDescription::sdr().tone_map_target_for(&source),
                );
                let uniforms =
                    VideoUniforms::from_pipeline(&pipeline, true, false).packed_alpha_right(alpha);
                renderer.supports_16bit_norm = true;
                renderer
                    .upload_planar_with_context(frame.clone(), uniforms, Some(source))
                    .unwrap();
                let expected = renderer
                    .render_current_offscreen_sized(12, 12, None, None)
                    .unwrap()
                    .unwrap();
                renderer.supports_16bit_norm = false;
                renderer
                    .upload_planar_with_context(frame.clone(), uniforms, Some(source))
                    .unwrap();
                let actual = renderer
                    .render_current_offscreen_sized(12, 12, None, None)
                    .unwrap()
                    .unwrap();
                for (i, (a, b)) in actual.rgba.iter().zip(&expected.rgba).enumerate() {
                    assert!(
                        a.abs_diff(*b) <= 2,
                        "source={source:?}, alpha={alpha}, byte={i}, actual={a}, expected={b}"
                    );
                }
            }
        }
    }

    #[test]
    fn wgpu_video_rejects_wrong_plane_sizes() {
        let mut renderer = WgpuRenderer::new().unwrap();
        let uniforms =
            VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);

        // Luma too short for a 4x4 frame.
        let result = renderer.render_nv12_offscreen(4, 4, &[0u8; 8], &[0u8; 8], uniforms);
        assert!(matches!(result, Err(PlayerError::Renderer(_))));

        // Odd dimensions are rejected.
        let result = renderer.render_nv12_offscreen(3, 4, &[0u8; 12], &[0u8; 4], uniforms);
        assert!(matches!(result, Err(PlayerError::Renderer(_))));
    }
}
