use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::env;
use std::ffi::{CStr, CString, c_char};
use std::fs::{OpenOptions, create_dir_all};
use std::io::Write;
use std::panic::{AssertUnwindSafe, Location, catch_unwind};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "android")]
mod android_jni;
mod diagnostics;
pub use diagnostics::ErikaLogLevel;
#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
mod presenter_json;

use crossbeam_channel::Receiver;
#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
use erika::LumaUpscalerBackendStatus;
#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
use erika::audio::AudioRecoveryState;
use erika::danmaku::{
    DanmakuLayoutConfig, DanmakuShadowStyle, DanmakuTimeline, DanmakuTrackInfo, DanmakuTrackSource,
};
use erika::export::{FrameExportFormat, FrameExportOptions, GifExportOptions, GifExportQuality};
pub use erika::{
    ExternalTextureFormat as ErikaExternalTextureFormat,
    ExternalTextureInfo as ErikaExternalTextureInfo,
    ExternalTextureKind as ErikaExternalTextureKind,
};

pub struct ErikaTextureFrame {
    frame: erika::ExternalTextureFrame,
}
#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
use erika::presenter::{
    PresenterConfig, PresenterRuntime, PresenterRuntimeSnapshot, SubtitleMemoryFontFace,
    SubtitleMemoryFontInfo, SubtitleMemoryFontStatus,
};
#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
use erika::renderer::metal::{MetalOutputMode, MetalRendererConfig, VideoAlphaMode};
use erika::renderer::output::{
    ActiveOutputEncoding, OutputFallbackReason, OutputMode, OutputRuntimeStatus,
    OutputSurfaceFormat,
};
#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
use erika::renderer::pipeline::LumaUpscalerMode;
#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
use erika::subtitle::SubtitleStyleConfig;
use erika::{
    FlutterTextureHandle, FlutterTextureKind, MediaRequest, MetalSurfaceHandle, PlatformSurface,
    Player, PlayerConfig, PlayerEvent, PlayerState, RendererRuntimeStats,
    SurfaceOutputCapabilities, TrackInfo, TrackKind, TrackSelection, TrackSource, TransferFunction,
    WgpuSurfaceHandle, WgpuSurfaceKind,
};

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaStatus {
    Ok = 0,
    NullPointer = 1,
    InvalidUtf8 = 2,
    PlayerError = 3,
    Panic = 4,
    NoEvent = 5,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ErikaHttpHeader {
    pub name: *const c_char,
    pub value: *const c_char,
}

/// Extended open parameters mirroring the C header's `ErikaOpenOptions`.
/// `http_read_ahead_bytes` of 0 uses the environment override when set,
/// otherwise the 2 MiB default. `http_back_buffer_bytes` of 0 uses the
/// 16 MiB rewind-budget default.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ErikaOpenOptions {
    pub headers: *const ErikaHttpHeader,
    pub header_count: usize,
    pub http_read_ahead_bytes: u64,
    pub http_back_buffer_bytes: u64,
    pub reserved: [u64; 2],
}

/// Headless GIF export request. All time values are milliseconds. The call is
/// synchronous and must be dispatched to a worker thread by UI integrations.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ErikaGifExportOptions {
    pub input_uri: *const c_char,
    pub output_path: *const c_char,
    pub start_millis: u64,
    pub end_millis: u64,
    pub frames_per_second: u32,
    pub output_width: u32,
    pub output_height: u32,
    /// 0 = normal (bilinear), 1 = high (Lanczos).
    pub quality: i32,
    pub loop_count: i32,
    pub overwrite: bool,
    pub headers: *const ErikaHttpHeader,
    pub header_count: usize,
    pub http_read_ahead_bytes: u64,
    pub reserved: [u64; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ErikaGifExportResult {
    pub width: u32,
    pub height: u32,
    pub frame_count: u64,
    pub file_size: u64,
}

/// Headless single-frame export request. `position_millis` is a media time;
/// the picture shown then is exported. `max_width`/`max_height` bound the
/// output (0 = unbounded); the display aspect ratio is kept and the picture is
/// never larger than the video. The call is synchronous and must be
/// dispatched to a worker thread by UI integrations.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ErikaFrameExportOptions {
    pub input_uri: *const c_char,
    pub headers: *const ErikaHttpHeader,
    pub header_count: usize,
    pub position_millis: u64,
    pub http_read_ahead_bytes: u64,
    pub max_width: u32,
    pub max_height: u32,
    /// 0 = RGBA rows (`width * 4` bytes each), 1 = PNG file bytes.
    pub format: i32,
    pub reserved_flags: u32,
    pub reserved: [u64; 4],
}

/// Result of `erika_export_frame`. `data` is owned by liberika and released
/// with `erika_frame_export_result_free`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ErikaFrameExportResult {
    pub data: *mut u8,
    pub data_len: usize,
    /// Presentation time of the exported picture.
    pub timestamp_micros: u64,
    /// Media duration; 0 when the container reports none.
    pub duration_micros: u64,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub format: i32,
    pub reserved_flags: u32,
    pub reserved: [u64; 2],
}

impl Default for ErikaFrameExportResult {
    fn default() -> Self {
        Self {
            data: std::ptr::null_mut(),
            data_len: 0,
            timestamp_micros: 0,
            duration_micros: 0,
            width: 0,
            height: 0,
            source_width: 0,
            source_height: 0,
            format: 0,
            reserved_flags: 0,
            reserved: [0; 2],
        }
    }
}

/// Bit 0 of `ErikaFrameExportResult::reserved_flags`: the source is HDR (PQ,
/// HLG or Dolby Vision) and the picture was tone mapped to SDR the way Erika
/// renders it on an SDR display.
pub const ERIKA_FRAME_EXPORT_TONE_MAPPED: u32 = 1;

/// Opens a [`ErikaFrameExportSession`]: the media pictures are taken from.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ErikaFrameExportSessionOptions {
    pub input_uri: *const c_char,
    pub headers: *const ErikaHttpHeader,
    pub header_count: usize,
    pub http_read_ahead_bytes: u64,
    pub reserved_flags: u32,
    pub reserved: [u64; 4],
}

/// One picture asked of a frame export session.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ErikaFrameExportRequest {
    pub position_millis: u64,
    pub max_width: u32,
    pub max_height: u32,
    /// 0 = RGBA rows (`width * 4` bytes each), 1 = PNG file bytes.
    pub format: i32,
    pub reserved_flags: u32,
    pub reserved: [u64; 4],
}

/// A media kept open for taking many pictures: one demuxer and one decoder,
/// and requests in ascending time order are decoded on from where the last
/// one stopped instead of seeking again. Calls on one session are serialised.
pub struct ErikaFrameExportSession {
    session: Mutex<erika::export::FrameExportSession>,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<String>> = RefCell::new(None);
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

fn set_last_error(message: impl Into<String>) {
    let message = message.into().replace('\0', "\\0");
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = Some(message);
    });
}

fn ensure_last_error(status: ErikaStatus) {
    LAST_ERROR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(format!("Erika C ABI returned {status:?}"));
        }
    });
}

fn finalize_status(status: ErikaStatus) -> ErikaStatus {
    match status {
        ErikaStatus::Ok => clear_last_error(),
        ErikaStatus::NoEvent => {}
        _ => ensure_last_error(status),
    }
    status
}

fn player_error(message: impl Into<String>) -> ErikaStatus {
    set_last_error(message);
    ErikaStatus::PlayerError
}

fn capi_trace_enabled() -> bool {
    env_flag("ERIKA_CAPI_TRACE") || env_flag("ERIKA_PLAYBACK_TRACE")
}

fn env_flag(name: &str) -> bool {
    match env::var(name).ok().as_deref() {
        Some("0" | "false" | "FALSE" | "off" | "OFF" | "") | None => false,
        Some(_) => true,
    }
}

fn capi_trace_path() -> PathBuf {
    env::var_os("ERIKA_CAPI_TRACE_FILE")
        .or_else(|| env::var_os("ERIKA_PLAYBACK_TRACE_FILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/erika_capi_trace.log"))
}

fn capi_trace(line: impl AsRef<str>) {
    if !capi_trace_enabled() {
        return;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let path = capi_trace_path();
    if let Some(parent) = path.parent() {
        let _ = create_dir_all(parent);
    }
    let line = format!("[erika-capi-trace] ts_ms={now_ms} {}", line.as_ref());
    eprintln!("{line}");
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{line}"));
}

fn redacted_uri(uri: &str) -> String {
    let mut value = uri.to_string();
    for key in ["token=", "api_key=", "AccessToken="] {
        let mut search_from = 0;
        while let Some(relative) = value[search_from..].find(key) {
            let start = search_from + relative + key.len();
            let end = value[start..]
                .find('&')
                .map(|relative_end| start + relative_end)
                .unwrap_or(value.len());
            value.replace_range(start..end, "REDACTED");
            search_from = start + "REDACTED".len();
        }
    }
    value
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaState {
    Idle = 0,
    Opening = 1,
    Ready = 2,
    Playing = 3,
    Paused = 4,
    Stopped = 5,
    Closed = 6,
    Error = 7,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaEventKind {
    None = 0,
    StateChanged = 1,
    DurationChanged = 2,
    PositionChanged = 3,
    TracksChanged = 4,
    BufferingChanged = 5,
    VideoParamsChanged = 6,
    SurfaceAttached = 7,
    SurfaceDetached = 8,
    Error = 9,
    TrackSelectionChanged = 10,
    VideoDecoderChanged = 11,
    AudioOutputChanged = 12,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaTrackKind {
    Video = 0,
    Audio = 1,
    Subtitle = 2,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaTrackSource {
    Embedded = 0,
    External = 1,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErikaTrackSelection {
    pub video: i64,
    pub audio: i64,
    pub subtitle: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErikaTrackInfo {
    pub id: i64,
    pub kind: ErikaTrackKind,
    pub source: ErikaTrackSource,
    pub selected: bool,
    pub can_remove: bool,
    pub title: *mut c_char,
    pub language: *mut c_char,
    pub codec: *mut c_char,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub pixel_format: *mut c_char,
    pub sample_format: *mut c_char,
    pub profile: *mut c_char,
    pub level: i32,
    pub bit_rate: u64,
    pub frame_rate_numerator: u32,
    pub frame_rate_denominator: u32,
}

impl Default for ErikaTrackInfo {
    fn default() -> Self {
        Self {
            id: -1,
            kind: ErikaTrackKind::Video,
            source: ErikaTrackSource::Embedded,
            selected: false,
            can_remove: false,
            title: std::ptr::null_mut(),
            language: std::ptr::null_mut(),
            codec: std::ptr::null_mut(),
            width: 0,
            height: 0,
            sample_rate: 0,
            channels: 0,
            pixel_format: std::ptr::null_mut(),
            sample_format: std::ptr::null_mut(),
            profile: std::ptr::null_mut(),
            level: 0,
            bit_rate: 0,
            frame_rate_numerator: 0,
            frame_rate_denominator: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct ErikaSubtitleMemoryFontStatus {
    pub registered_count: usize,
    pub registered_bytes: usize,
    pub selected_count: usize,
    pub generation: u64,
    pub selected_ids: *mut u64,
}

#[repr(C)]
#[derive(Debug)]
pub struct ErikaSubtitleMemoryFontFace {
    pub index: u32,
    pub families_json: *mut c_char,
    pub post_script_name: *mut c_char,
    pub weight: u16,
    pub italic: bool,
    pub monospaced: bool,
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct ErikaSubtitleMemoryFontInfo {
    pub id: u64,
    pub byte_len: usize,
    pub faces: *mut ErikaSubtitleMemoryFontFace,
    pub face_count: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaWgpuSurfaceKind {
    Unknown = 0,
    MacOsNsView = 1,
    MacOsCaMetalLayer = 2,
    IosUiView = 3,
    WindowsHwnd = 4,
    XlibWindow = 5,
    WaylandSurface = 6,
    AndroidNativeWindow = 7,
    OhosNativeWindow = 8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaFlutterTextureKind {
    Unknown = 0,
    MacOsTextureRegistrar = 1,
    IosTextureRegistrar = 2,
    AndroidSurfaceTexture = 3,
    WindowsTextureRegistrar = 4,
    LinuxTextureRegistrar = 5,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaPresenterOutputMode {
    Sdr = 0,
    AppleEdr = 1,
    ExtendedLinear = 2,
    Auto = 3,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaOutputFallbackReason {
    None = 0,
    DisplayHdrUnsupported = 1,
    HybridCompositionRequired = 2,
    WgpuBackendNotVulkan = 3,
    Rgba16FloatSurfaceFormatUnavailable = 4,
    NativeWindowDataSpaceApiUnavailable = 5,
    ScrgbDataSpaceVerificationFailed = 6,
    SurfaceConfigureFailed = 7,
    LegacyAppleEdrUnsupported = 8,
}

impl ErikaPresenterOutputMode {
    fn from_raw(value: i32) -> Self {
        match value {
            1 => Self::AppleEdr,
            2 => Self::ExtendedLinear,
            3 => Self::Auto,
            _ => Self::Sdr,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaLumaUpscalerMode {
    Off = 0,
    ArtCnnC4F16 = 1,
    ArtCnnC4F32 = 2,
    ArtCnnC4F16Ds = 3,
}

impl ErikaLumaUpscalerMode {
    fn from_raw(value: i32) -> Self {
        match value {
            1 => Self::ArtCnnC4F16,
            2 => Self::ArtCnnC4F32,
            3 => Self::ArtCnnC4F16Ds,
            _ => Self::Off,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErikaUpscalerBackendStatus {
    Off = 0,
    Inactive = 1,
    Building = 2,
    Scalar = 3,
    SimdgroupMatrix = 4,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErikaPresenterConfig {
    pub output_mode: i32,
    pub edr_headroom: f32,
    pub luma_upscaler: i32,
    pub video_alpha_mode: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErikaSubtitleStyle {
    pub font_family: *const c_char,
    pub font_file_path: *const c_char,
    pub primary_color_rgba: u32,
    pub outline_color_rgba: u32,
    pub font_size: f64,
    pub outline_width: f64,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike_out: bool,
    pub spacing: f64,
    pub scale_x_percent: f64,
    pub scale_y_percent: f64,
    pub border_style: i32,
    pub shadow_depth: f64,
    pub blur: f64,
    pub alignment: i32,
    pub margin_left: i32,
    pub margin_right: i32,
    pub margin_vertical: i32,
    pub override_mask: u32,
}

impl Default for ErikaSubtitleStyle {
    fn default() -> Self {
        let style = erika::subtitle::SubtitleStyleConfig::default();
        Self {
            font_family: std::ptr::null(),
            font_file_path: std::ptr::null(),
            primary_color_rgba: style.primary_color_rgba,
            outline_color_rgba: style.outline_color_rgba,
            font_size: style.font_size,
            outline_width: style.outline_width,
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            strike_out: style.strike_out,
            spacing: style.spacing,
            scale_x_percent: style.scale_x_percent,
            scale_y_percent: style.scale_y_percent,
            border_style: style.border_style,
            shadow_depth: style.shadow_depth,
            blur: style.blur,
            alignment: style.alignment,
            margin_left: style.margin_left,
            margin_right: style.margin_right,
            margin_vertical: style.margin_vertical,
            override_mask: style.override_mask,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErikaSurfaceOutputCapabilities {
    pub extended_linear: bool,
    pub direct_composition: bool,
    pub desired_headroom: f32,
    pub fallback_reason: i32,
}

impl Default for ErikaSurfaceOutputCapabilities {
    fn default() -> Self {
        Self {
            extended_linear: false,
            direct_composition: false,
            desired_headroom: 0.0,
            fallback_reason: ErikaOutputFallbackReason::None as i32,
        }
    }
}

impl From<ErikaSurfaceOutputCapabilities> for SurfaceOutputCapabilities {
    fn from(value: ErikaSurfaceOutputCapabilities) -> Self {
        Self {
            extended_linear: value.extended_linear,
            direct_composition: value.direct_composition,
            desired_headroom: if value.desired_headroom.is_finite() && value.desired_headroom >= 0.0
            {
                value.desired_headroom
            } else {
                0.0
            },
            fallback_reason: OutputFallbackReason::from_raw(value.fallback_reason),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErikaUpscalerStatus {
    pub requested_mode: i32,
    pub active_backend: i32,
    pub fallback_count: u64,
    pub upscaled_frames: u64,
    pub last_encode_micros: u64,
    pub last_gpu_micros: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErikaOutputStatus {
    pub requested_mode: i32,
    pub active_encoding: i32,
    pub surface_format: i32,
    pub native_data_space: i32,
    pub requested_headroom: f32,
    pub active_headroom: f32,
    pub active_headroom_known: bool,
    pub extended_linear_active: bool,
    pub fallback_reason: i32,
    pub fallback_count: u64,
    pub data_space_failures: u64,
    pub headroom_updates: u64,
    pub extended_linear_frames: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErikaPresenterResourceStatus {
    pub device_current_allocated_bytes: u64,
    pub device_recommended_working_set_bytes: u64,
    pub drawable_estimated_bytes: u64,
    pub video_frame_bytes: u64,
    pub overlay_atlas_bytes: u64,
    pub danmaku_atlas_bytes: u64,
    pub danmaku_vertex_buffer_bytes: u64,
    pub upscaler_bytes: u64,
    pub renderer_tracked_bytes: u64,
    pub presenter_cpu_danmaku_atlas_bytes: u64,
    pub drawable_count: u32,
    pub output_mode_switches: u64,
}

impl Default for ErikaOutputStatus {
    fn default() -> Self {
        output_status_to_c(OutputRuntimeStatus::default())
    }
}

impl Default for ErikaPresenterConfig {
    fn default() -> Self {
        Self {
            output_mode: ErikaPresenterOutputMode::Sdr as i32,
            edr_headroom: 1.0,
            luma_upscaler: ErikaLumaUpscalerMode::Off as i32,
            video_alpha_mode: VideoAlphaMode::Opaque as i32,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErikaDanmakuConfig {
    pub enabled: bool,
    /// NipaPlay/Flutter logical danmaku font size. liberika uses the NipaPlay
    /// default danmaku font and multiplies by the surface scale for glyph pixels.
    pub font_size: f32,
    pub opacity: f32,
    pub display_area: f32,
    pub scroll_duration_seconds: f32,
    pub scroll_speed_factor: f32,
    pub track_gap_ratio: f32,
    pub outline_width: f32,
    pub shadow_offset_x: f32,
    pub shadow_offset_y: f32,
    pub merge_duplicates: bool,
    pub allow_stacking: bool,
    pub allow_scroll_overwrite: bool,
    pub max_quantity: u32,
    pub max_lines_per_mode: u32,
    pub block_top: bool,
    pub block_bottom: bool,
    pub block_scroll: bool,
    pub shadow_style: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErikaDanmakuTrackInfo {
    pub id: u64,
    pub enabled: bool,
    pub offset_micros: i64,
    pub item_count: usize,
    pub name: *mut c_char,
    pub source: *mut c_char,
}

impl Default for ErikaDanmakuTrackInfo {
    fn default() -> Self {
        Self {
            id: 0,
            enabled: false,
            offset_micros: 0,
            item_count: 0,
            name: std::ptr::null_mut(),
            source: std::ptr::null_mut(),
        }
    }
}

impl Default for ErikaDanmakuConfig {
    fn default() -> Self {
        let config = DanmakuLayoutConfig::default();
        Self {
            enabled: config.enabled,
            font_size: config.font_size,
            opacity: config.opacity,
            display_area: config.display_area,
            scroll_duration_seconds: config.scroll_duration_seconds,
            scroll_speed_factor: config.scroll_speed_factor,
            track_gap_ratio: config.track_gap_ratio,
            outline_width: config.outline_width,
            shadow_offset_x: config.shadow_offset[0],
            shadow_offset_y: config.shadow_offset[1],
            merge_duplicates: config.merge_duplicates,
            allow_stacking: config.allow_stacking,
            allow_scroll_overwrite: config.allow_scroll_overwrite,
            max_quantity: 0,
            max_lines_per_mode: 0,
            block_top: config.block_top,
            block_bottom: config.block_bottom,
            block_scroll: config.block_scroll,
            shadow_style: config.shadow_style.code(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErikaVideoParams {
    pub width: u32,
    pub height: u32,
    pub primaries: u32,
    pub transfer: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErikaTrackCounts {
    pub video: u32,
    pub audio: u32,
    pub subtitle: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErikaEvent {
    pub kind: ErikaEventKind,
    pub status: ErikaStatus,
    pub state: ErikaState,
    pub duration_micros: i64,
    pub position_micros: u64,
    pub buffering: bool,
    pub video: ErikaVideoParams,
    pub tracks: ErikaTrackCounts,
}

impl Default for ErikaEvent {
    fn default() -> Self {
        Self {
            kind: ErikaEventKind::None,
            status: ErikaStatus::Ok,
            state: ErikaState::Idle,
            duration_micros: -1,
            position_micros: 0,
            buffering: false,
            video: ErikaVideoParams::default(),
            tracks: ErikaTrackCounts::default(),
        }
    }
}

pub struct ErikaHandle {
    player: Player,
    events: Receiver<PlayerEvent>,
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
pub struct ErikaPresenterHandle {
    presenter: PresenterRuntime,
    events: Receiver<PlayerEvent>,
    pending_events: VecDeque<PlayerEvent>,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErikaPresenterStats {
    pub decoded_video_frames: u64,
    pub rendered_video_frames: u64,
    pub rendered_test_frames: u64,
    pub pushed_audio_frames: u64,
    pub overlay_frames: u64,
    pub danmaku_frames: u64,
    pub danmaku_items: u64,
    pub import_failures: u64,
    pub render_failures: u64,
    pub audio_failures: u64,
    pub software_video_frames: u64,
    pub hardware_video_frames: u64,
    pub zero_copy_video_frames: u64,
    pub cpu_video_frame_fallbacks: u64,
    pub last_render_micros: u64,
    pub last_render_current_micros: u64,
    pub audio_clock_read_frames: u64,
    pub audio_clock_queued_frames: u64,
    pub audio_clock_underflow_frames: u64,
    pub audio_recovery_state: i32,
    pub audio_last_error_code: i32,
    pub audio_recovery_attempts: u64,
    pub audio_recovery_count: u64,
    pub audio_recovery_failures: u64,
    pub direct_zero_copy_video_frames: u64,
    pub shared_handle_video_frames: u64,
    pub hdr_source_frames: u64,
    pub hdr10_output_frames: u64,
    pub sdr_tonemap_frames: u64,
    pub hdr10_metadata_updates: u64,
    pub hdr10_metadata_failures: u64,
    pub hdr10_output_failures: u64,
    pub hdr10_output_active: bool,
    pub video_frame_backpressure_drops: u64,
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_create() -> *mut ErikaHandle {
    capi_trace(format!(
        "fn=erika_create playback_trace={} playback_trace_file={}",
        env::var("ERIKA_PLAYBACK_TRACE").unwrap_or_else(|_| "<unset>".to_string()),
        env::var("ERIKA_PLAYBACK_TRACE_FILE").unwrap_or_else(|_| "<unset>".to_string()),
    ));
    let player = Player::new(PlayerConfig::default());
    let events = player.subscribe();
    Box::into_raw(Box::new(ErikaHandle { player, events }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_destroy(handle: *mut ErikaHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_last_error_message() -> *mut c_char {
    LAST_ERROR.with(|slot| option_string_to_c(slot.borrow().as_deref()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_string_free(value: *mut c_char) {
    if value.is_null() {
        return;
    }
    unsafe { drop(CString::from_raw(value)) };
}

/// Runs a complete headless GIF export on the calling thread.
///
/// This function owns a separate demux/decode/encode pipeline and does not
/// require either ErikaHandle or ErikaPresenterHandle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_export_gif(
    options: *const ErikaGifExportOptions,
    out_result: *mut ErikaGifExportResult,
) -> ErikaStatus {
    if options.is_null() || out_result.is_null() {
        set_last_error("GIF export options and result pointers must not be null");
        return ErikaStatus::NullPointer;
    }
    match catch_unwind(AssertUnwindSafe(|| {
        let options = unsafe { &*options };
        if options.reserved.iter().any(|field| *field != 0) {
            return player_error("ErikaGifExportOptions.reserved must be zero");
        }
        let input_uri = match c_string(options.input_uri) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let output_path = match c_string(options.output_path) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let headers = match c_http_headers(options.headers, options.header_count) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let quality = match options.quality {
            0 => GifExportQuality::Normal,
            1 => GifExportQuality::High,
            value => return player_error(format!("unsupported GIF export quality: {value}")),
        };
        let mut request = MediaRequest::new(input_uri).with_http_headers(headers);
        request = request.map_http_read_ahead_bytes(Some(options.http_read_ahead_bytes));
        let export_options = GifExportOptions {
            input: request,
            output_path: PathBuf::from(output_path),
            start: Duration::from_millis(options.start_millis),
            end: Duration::from_millis(options.end_millis),
            frames_per_second: options.frames_per_second,
            output_width: options.output_width,
            output_height: options.output_height,
            quality,
            loop_count: options.loop_count,
            overwrite: options.overwrite,
        };
        match erika::export::export_gif(&export_options) {
            Ok(result) => {
                unsafe {
                    *out_result = ErikaGifExportResult {
                        width: result.width,
                        height: result.height,
                        frame_count: result.frame_count,
                        file_size: result.file_size,
                    };
                }
                ErikaStatus::Ok
            }
            Err(error) => player_error(error.to_string()),
        }
    })) {
        Ok(status) => finalize_status(status),
        Err(payload) => {
            set_last_error(report_capi_panic(
                "Erika GIF export C ABI call",
                Location::caller(),
                payload,
            ));
            ErikaStatus::Panic
        }
    }
}

/// Decodes the picture shown at a media time and returns it as RGBA or PNG,
/// on the calling thread.
///
/// Like `erika_export_gif` this owns an independent demux/decode pipeline and
/// needs no player or presenter handle. On success `out_result->data` holds
/// `data_len` bytes until `erika_frame_export_result_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_export_frame(
    options: *const ErikaFrameExportOptions,
    out_result: *mut ErikaFrameExportResult,
) -> ErikaStatus {
    if options.is_null() || out_result.is_null() {
        set_last_error("frame export options and result pointers must not be null");
        return ErikaStatus::NullPointer;
    }
    match catch_unwind(AssertUnwindSafe(|| {
        let options = unsafe { &*options };
        if options.reserved_flags != 0 || options.reserved.iter().any(|field| *field != 0) {
            return player_error("ErikaFrameExportOptions reserved fields must be zero");
        }
        let input_uri = match c_string(options.input_uri) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let headers = match c_http_headers(options.headers, options.header_count) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let format = match frame_export_format(options.format) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let mut request = MediaRequest::new(input_uri).with_http_headers(headers);
        request = request.map_http_read_ahead_bytes(Some(options.http_read_ahead_bytes));
        let export_options = FrameExportOptions {
            input: request,
            position: Duration::from_millis(options.position_millis),
            max_width: options.max_width,
            max_height: options.max_height,
            format,
        };
        match erika::export::export_frame(&export_options) {
            Ok(frame) => {
                unsafe { *out_result = frame_export_result(frame, options.format) };
                ErikaStatus::Ok
            }
            Err(error) => player_error(error.to_string()),
        }
    })) {
        Ok(status) => finalize_status(status),
        Err(payload) => {
            set_last_error(report_capi_panic(
                "Erika frame export C ABI call",
                Location::caller(),
                payload,
            ));
            ErikaStatus::Panic
        }
    }
}

/// Moves an exported picture into the C result; its bytes belong to liberika
/// until `erika_frame_export_result_free`.
fn frame_export_result(frame: erika::export::ExportedFrame, format: i32) -> ErikaFrameExportResult {
    let data_len = frame.data.len();
    let data = Box::into_raw(frame.data.into_boxed_slice());
    ErikaFrameExportResult {
        data: data.cast::<u8>(),
        data_len,
        timestamp_micros: frame.timestamp.as_micros().min(u64::MAX as u128) as u64,
        duration_micros: frame.duration.map_or(0, |duration| {
            duration.as_micros().min(u64::MAX as u128) as u64
        }),
        width: frame.width,
        height: frame.height,
        source_width: frame.source_width,
        source_height: frame.source_height,
        format,
        reserved_flags: if frame.tone_mapped {
            ERIKA_FRAME_EXPORT_TONE_MAPPED
        } else {
            0
        },
        ..ErikaFrameExportResult::default()
    }
}

fn frame_export_format(value: i32) -> Result<FrameExportFormat, ErikaStatus> {
    match value {
        0 => Ok(FrameExportFormat::Rgba),
        1 => Ok(FrameExportFormat::Png),
        value => Err(player_error(format!(
            "unsupported frame export format: {value}"
        ))),
    }
}

/// Opens a media for taking many pictures (see `ErikaFrameExportSession`).
/// Like `erika_export_frame` it owns an independent demux/decode pipeline and
/// needs no player or presenter handle; opening reads the container header on
/// the calling thread. On success `*out_session` is released with
/// `erika_frame_export_session_close`; on failure it is set to NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_frame_export_session_open(
    options: *const ErikaFrameExportSessionOptions,
    out_session: *mut *mut ErikaFrameExportSession,
) -> ErikaStatus {
    if options.is_null() || out_session.is_null() {
        set_last_error("frame export session options and result pointers must not be null");
        return ErikaStatus::NullPointer;
    }
    unsafe { *out_session = std::ptr::null_mut() };
    match catch_unwind(AssertUnwindSafe(|| {
        let options = unsafe { &*options };
        if options.reserved_flags != 0 || options.reserved.iter().any(|field| *field != 0) {
            return player_error("ErikaFrameExportSessionOptions reserved fields must be zero");
        }
        let input_uri = match c_string(options.input_uri) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let headers = match c_http_headers(options.headers, options.header_count) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let request = MediaRequest::new(input_uri)
            .with_http_headers(headers)
            .map_http_read_ahead_bytes(Some(options.http_read_ahead_bytes));
        match erika::export::FrameExportSession::open(&request) {
            Ok(session) => {
                unsafe {
                    *out_session = Box::into_raw(Box::new(ErikaFrameExportSession {
                        session: Mutex::new(session),
                    }));
                }
                ErikaStatus::Ok
            }
            Err(error) => player_error(error.to_string()),
        }
    })) {
        Ok(status) => finalize_status(status),
        Err(payload) => {
            set_last_error(report_capi_panic(
                "Erika frame export session open C ABI call",
                Location::caller(),
                payload,
            ));
            ErikaStatus::Panic
        }
    }
}

/// Decodes the picture shown at `request->position_millis` and returns it as
/// RGBA or PNG, on the calling thread. `out_result->data` is released with
/// `erika_frame_export_result_free`. A failed request leaves the session usable:
/// the next one seeks again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_frame_export_session_frame(
    session: *mut ErikaFrameExportSession,
    request: *const ErikaFrameExportRequest,
    out_result: *mut ErikaFrameExportResult,
) -> ErikaStatus {
    if session.is_null() || request.is_null() || out_result.is_null() {
        set_last_error("frame export session, request and result pointers must not be null");
        return ErikaStatus::NullPointer;
    }
    match catch_unwind(AssertUnwindSafe(|| {
        let request = unsafe { &*request };
        if request.reserved_flags != 0 || request.reserved.iter().any(|field| *field != 0) {
            return player_error("ErikaFrameExportRequest reserved fields must be zero");
        }
        let format = match frame_export_format(request.format) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let session = unsafe { &*session };
        let mut guard = session
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let picture = erika::export::FrameRequest {
            position: Duration::from_millis(request.position_millis),
            max_width: request.max_width,
            max_height: request.max_height,
            format,
        };
        match guard.frame_at(&picture) {
            Ok(frame) => {
                unsafe { *out_result = frame_export_result(frame, request.format) };
                ErikaStatus::Ok
            }
            Err(error) => player_error(error.to_string()),
        }
    })) {
        Ok(status) => finalize_status(status),
        Err(payload) => {
            set_last_error(report_capi_panic(
                "Erika frame export session C ABI call",
                Location::caller(),
                payload,
            ));
            ErikaStatus::Panic
        }
    }
}

/// Closes a frame export session and releases its demuxer and decoder. Safe
/// on NULL. No call on the session may be running or follow.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_frame_export_session_close(session: *mut ErikaFrameExportSession) {
    if !session.is_null() {
        drop(unsafe { Box::from_raw(session) });
    }
}

/// Releases the bytes of an `erika_export_frame` result and clears it. Safe
/// to call on a cleared or zeroed result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_frame_export_result_free(result: *mut ErikaFrameExportResult) {
    if result.is_null() {
        return;
    }
    let result = unsafe { &mut *result };
    if !result.data.is_null() {
        drop(unsafe {
            Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                result.data,
                result.data_len,
            ))
        });
    }
    result.data = std::ptr::null_mut();
    result.data_len = 0;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_open(handle: *mut ErikaHandle, uri: *const c_char) -> ErikaStatus {
    unsafe { erika_open_with_headers(handle, uri, std::ptr::null(), 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_open_with_headers(
    handle: *mut ErikaHandle,
    uri: *const c_char,
    headers: *const ErikaHttpHeader,
    header_count: usize,
) -> ErikaStatus {
    let options = open_options_raw(headers, header_count);
    unsafe { erika_open_with_options(handle, uri, &options) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_open_with_options(
    handle: *mut ErikaHandle,
    uri: *const c_char,
    options: *const ErikaOpenOptions,
) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        let uri = match c_string(uri) {
            Ok(uri) => uri,
            Err(status) => return status,
        };
        capi_trace(format!(
            "fn=erika_open handle={handle:p} uri={}",
            redacted_uri(&uri)
        ));
        let (headers, http_read_ahead_bytes, http_back_buffer_bytes) = match c_open_options(options)
        {
            Ok(parsed) => parsed,
            Err(status) => return status,
        };
        let request = MediaRequest::new(uri)
            .with_http_headers(headers)
            .map_http_read_ahead_bytes(http_read_ahead_bytes)
            .map_http_back_buffer_bytes(http_back_buffer_bytes);
        let status = status_from_player_result(handle.player.open(request));
        capi_trace(format!(
            "fn=erika_open.done handle={handle:p} status={status:?}"
        ));
        status
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_play(handle: *mut ErikaHandle) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        capi_trace(format!("fn=erika_play handle={handle:p}"));
        let status = status_from_player_result(handle.player.play());
        let reason = if status == ErikaStatus::Ok {
            String::new()
        } else {
            LAST_ERROR.with(|slot| slot.borrow().clone().unwrap_or_default())
        };
        capi_trace(format!(
            "fn=erika_play.done handle={handle:p} status={status:?} {reason}"
        ));
        status
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_pause(handle: *mut ErikaHandle) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        capi_trace(format!("fn=erika_pause handle={handle:p}"));
        let status = status_from_player_result(handle.player.pause());
        capi_trace(format!(
            "fn=erika_pause.done handle={handle:p} status={status:?}"
        ));
        status
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_stop(handle: *mut ErikaHandle) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        status_from_player_result(handle.player.stop())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_close(handle: *mut ErikaHandle) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        status_from_player_result(handle.player.close())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_seek(handle: *mut ErikaHandle, position_micros: u64) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        capi_trace(format!(
            "fn=erika_seek handle={handle:p} position_micros={position_micros}"
        ));
        let status =
            status_from_player_result(handle.player.seek(Duration::from_micros(position_micros)));
        capi_trace(format!(
            "fn=erika_seek.done handle={handle:p} status={status:?}"
        ));
        status
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_add_external_subtitle(
    handle: *mut ErikaHandle,
    uri: *const c_char,
    out_track_id: *mut i64,
) -> ErikaStatus {
    if out_track_id.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_handle_mut(handle, |handle| {
        let uri = match c_string(uri) {
            Ok(uri) => uri,
            Err(status) => return status,
        };
        match handle.player.add_external_subtitle(uri) {
            Ok(track) => {
                unsafe { *out_track_id = track.id };
                ErikaStatus::Ok
            }
            Err(error) => player_error(error.to_string()),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_remove_subtitle_track(
    handle: *mut ErikaHandle,
    track_id: i64,
) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        status_from_player_result(handle.player.remove_subtitle_track(track_id))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_select_audio_track(
    handle: *mut ErikaHandle,
    track_id: i64,
) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        status_from_player_result(handle.player.select_audio_track(track_id_option(track_id)))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_select_subtitle_track(
    handle: *mut ErikaHandle,
    track_id: i64,
) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        status_from_player_result(
            handle
                .player
                .select_subtitle_track(track_id_option(track_id)),
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_track_selection(
    handle: *mut ErikaHandle,
    out_selection: *mut ErikaTrackSelection,
) -> ErikaStatus {
    if out_selection.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_handle_mut(handle, |handle| {
        unsafe { *out_selection = track_selection_to_c(handle.player.track_selection()) };
        ErikaStatus::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_tracks(
    handle: *mut ErikaHandle,
    out_tracks: *mut ErikaTrackInfo,
    capacity: usize,
    out_len: *mut usize,
) -> ErikaStatus {
    if out_len.is_null() || (capacity > 0 && out_tracks.is_null()) {
        return ErikaStatus::NullPointer;
    }
    with_handle_mut(handle, |handle| {
        write_tracks_to_c(&handle.player.tracks(), out_tracks, capacity, out_len)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_track_info_free(track: *mut ErikaTrackInfo) {
    if track.is_null() {
        return;
    }
    let track = unsafe { &mut *track };
    free_c_string(&mut track.title);
    free_c_string(&mut track.language);
    free_c_string(&mut track.codec);
    free_c_string(&mut track.pixel_format);
    free_c_string(&mut track.sample_format);
    free_c_string(&mut track.profile);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_danmaku_track_info_free(track: *mut ErikaDanmakuTrackInfo) {
    if track.is_null() {
        return;
    }
    let track = unsafe { &mut *track };
    free_c_string(&mut track.name);
    free_c_string(&mut track.source);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_attach_metal_layer(
    handle: *mut ErikaHandle,
    raw_layer: u64,
    width: u32,
    height: u32,
    scale: f64,
) -> ErikaStatus {
    if raw_layer == 0 {
        set_last_error("metal layer pointer is null");
        return ErikaStatus::NullPointer;
    }
    with_handle_mut(handle, |handle| {
        status_from_player_result(handle.player.attach_surface(PlatformSurface::Metal(
            MetalSurfaceHandle::new(raw_layer, width, height, scale),
        )))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_attach_wgpu_surface(
    handle: *mut ErikaHandle,
    kind: ErikaWgpuSurfaceKind,
    raw_window: u64,
    raw_display: u64,
    width: u32,
    height: u32,
    scale: f64,
) -> ErikaStatus {
    unsafe {
        erika_attach_wgpu_surface_with_output_capabilities(
            handle,
            kind,
            raw_window,
            raw_display,
            width,
            height,
            scale,
            ErikaSurfaceOutputCapabilities::default(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_attach_wgpu_surface_with_output_capabilities(
    handle: *mut ErikaHandle,
    kind: ErikaWgpuSurfaceKind,
    raw_window: u64,
    raw_display: u64,
    width: u32,
    height: u32,
    scale: f64,
    output_capabilities: ErikaSurfaceOutputCapabilities,
) -> ErikaStatus {
    if raw_window == 0 {
        set_last_error("surface window pointer is null");
        return ErikaStatus::NullPointer;
    }
    with_handle_mut(handle, |handle| {
        status_from_player_result(
            handle.player.attach_surface(PlatformSurface::Wgpu(
                wgpu_surface_handle_from_c(kind, raw_window, raw_display, width, height, scale)
                    .with_output_capabilities(output_capabilities.into()),
            )),
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_attach_flutter_texture(
    handle: *mut ErikaHandle,
    kind: ErikaFlutterTextureKind,
    texture_id: i64,
    width: u32,
    height: u32,
    scale: f64,
) -> ErikaStatus {
    if texture_id < 0 {
        return ErikaStatus::NullPointer;
    }
    with_handle_mut(handle, |handle| {
        status_from_player_result(
            handle
                .player
                .attach_surface(PlatformSurface::FlutterTexture(FlutterTextureHandle::new(
                    flutter_texture_kind_from_c(kind),
                    texture_id,
                    width,
                    height,
                    scale,
                ))),
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_detach_surface(handle: *mut ErikaHandle) -> ErikaStatus {
    with_handle_mut(handle, |handle| {
        status_from_player_result(handle.player.detach_surface())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_state(
    handle: *mut ErikaHandle,
    out_state: *mut ErikaState,
) -> ErikaStatus {
    if out_state.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_handle_mut(handle, |handle| {
        unsafe { *out_state = state_to_c(handle.player.state()) };
        ErikaStatus::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_poll_event(
    handle: *mut ErikaHandle,
    out_event: *mut ErikaEvent,
) -> ErikaStatus {
    if out_event.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_handle_mut(handle, |handle| match handle.events.try_recv() {
        Ok(event) => {
            let event = event_to_c(event);
            capi_trace(format!(
                "fn=erika_poll_event event={:?} state={:?} position_micros={} duration_micros={}",
                event.kind, event.state, event.position_micros, event.duration_micros
            ));
            unsafe { *out_event = event };
            ErikaStatus::Ok
        }
        Err(crossbeam_channel::TryRecvError::Empty) => ErikaStatus::NoEvent,
        Err(crossbeam_channel::TryRecvError::Disconnected) => ErikaStatus::PlayerError,
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub extern "C" fn erika_presenter_create() -> *mut ErikaPresenterHandle {
    capi_trace(format!(
        "fn=erika_presenter_create playback_trace={} playback_trace_file={}",
        env::var("ERIKA_PLAYBACK_TRACE").unwrap_or_else(|_| "<unset>".to_string()),
        env::var("ERIKA_PLAYBACK_TRACE_FILE").unwrap_or_else(|_| "<unset>".to_string()),
    ));
    create_presenter_handle(PresenterConfig::default())
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub extern "C" fn erika_presenter_create() -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub extern "C" fn erika_presenter_create_with_config(
    config: ErikaPresenterConfig,
) -> *mut ErikaPresenterHandle {
    create_presenter_handle(presenter_config_from_c(config))
}

/// Creates a presenter using the wgpu backend for native texture export.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub extern "C" fn erika_presenter_create_for_texture_export(
    config: ErikaPresenterConfig,
) -> *mut ErikaPresenterHandle {
    let mut config = presenter_config_from_c(config);
    config.player.renderer = erika::RendererBackendPreference::WgpuFallback;
    create_presenter_handle(config)
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub extern "C" fn erika_presenter_create_with_output_mode(
    output_mode: i32,
    edr_headroom: f32,
) -> *mut ErikaPresenterHandle {
    create_presenter_handle(presenter_config_from_c(ErikaPresenterConfig {
        output_mode,
        edr_headroom,
        ..ErikaPresenterConfig::default()
    }))
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub extern "C" fn erika_presenter_create_with_output_mode_and_alpha(
    output_mode: i32,
    edr_headroom: f32,
    video_alpha_mode: i32,
) -> *mut ErikaPresenterHandle {
    create_presenter_handle(presenter_config_from_c(ErikaPresenterConfig {
        output_mode,
        edr_headroom,
        video_alpha_mode,
        ..ErikaPresenterConfig::default()
    }))
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub extern "C" fn erika_presenter_create_with_config(
    _config: ErikaPresenterConfig,
) -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub extern "C" fn erika_presenter_create_with_output_mode(
    _output_mode: i32,
    _edr_headroom: f32,
) -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_open_with_headers(
    _handle: *mut std::ffi::c_void,
    _uri: *const c_char,
    _headers: *const ErikaHttpHeader,
    _header_count: usize,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_open_with_options(
    _handle: *mut std::ffi::c_void,
    _uri: *const c_char,
    _options: *const ErikaOpenOptions,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn create_presenter_handle(config: PresenterConfig) -> *mut ErikaPresenterHandle {
    let created = catch_unwind(AssertUnwindSafe(|| {
        let presenter = PresenterRuntime::new(config)?;
        let events = presenter.player().subscribe();
        Ok::<_, erika::PlayerError>((presenter, events))
    }));
    match created {
        Ok(Ok((presenter, events))) => {
            clear_last_error();
            let handle = Box::into_raw(Box::new(ErikaPresenterHandle {
                presenter,
                events,
                pending_events: VecDeque::new(),
            }));
            capi_trace(format!("fn=create_presenter_handle.done handle={handle:p}"));
            handle
        }
        Ok(Err(error)) => {
            capi_trace(format!("fn=create_presenter_handle.error error={error}"));
            set_last_error(format!("presenter create failed: {error}"));
            std::ptr::null_mut()
        }
        Err(payload) => {
            let reason = panic_payload_message(payload);
            capi_trace(format!("fn=create_presenter_handle.panic reason={reason}"));
            set_last_error(format!("panic while creating presenter: {reason}"));
            std::ptr::null_mut()
        }
    }
}

fn panic_payload_message(payload: Box<dyn Any + Send>) -> String {
    panic_payload_ref_message(payload.as_ref())
}

fn panic_payload_ref_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_string())
        })
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

fn report_capi_panic(
    scope: &str,
    location: &'static Location<'static>,
    payload: Box<dyn Any + Send>,
) -> String {
    let reason = panic_payload_message(payload).replace(['\r', '\n'], " ");
    let message = format!(
        "panic while handling {scope} at {}:{}:{}: {reason}",
        location.file(),
        location.line(),
        location.column()
    );
    capi_trace(format!(
        "event=capi_panic scope={scope:?} caller_file={:?} caller_line={} caller_column={} reason={reason:?}",
        location.file(),
        location.line(),
        location.column()
    ));
    #[cfg(target_os = "android")]
    android_jni::android_jni_log_error(
        &serde_json::json!({
            "event": "capi_panic",
            "scope": scope,
            "callerFile": location.file(),
            "callerLine": location.line(),
            "callerColumn": location.column(),
            "reason": reason,
        })
        .to_string(),
    );
    message
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn presenter_config_from_c(config: ErikaPresenterConfig) -> PresenterConfig {
    let output_mode = match ErikaPresenterOutputMode::from_raw(config.output_mode) {
        ErikaPresenterOutputMode::AppleEdr => {
            let headroom = if config.edr_headroom.is_finite() {
                config.edr_headroom
            } else {
                1.0
            };
            MetalOutputMode::apple_edr(headroom)
        }
        ErikaPresenterOutputMode::ExtendedLinear => {
            let headroom = if config.edr_headroom.is_finite() {
                config.edr_headroom
            } else {
                1.0
            };
            MetalOutputMode::extended_linear(headroom)
        }
        ErikaPresenterOutputMode::Auto => {
            let headroom = if config.edr_headroom.is_finite() {
                config.edr_headroom
            } else {
                1.0
            };
            MetalOutputMode::auto(headroom)
        }
        ErikaPresenterOutputMode::Sdr => MetalOutputMode::Sdr,
    };

    PresenterConfig {
        renderer: MetalRendererConfig {
            output_mode,
            luma_upscaler: luma_upscaler_mode_from_c(config.luma_upscaler),
            video_alpha_mode: VideoAlphaMode::from_raw(config.video_alpha_mode),
            ..MetalRendererConfig::default()
        },
        ..PresenterConfig::default()
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn subtitle_style_from_c(style: ErikaSubtitleStyle) -> Result<SubtitleStyleConfig, ErikaStatus> {
    Ok(SubtitleStyleConfig {
        font_family: optional_c_string_result(style.font_family)?,
        font_file_path: optional_c_string_result(style.font_file_path)?,
        primary_color_rgba: style.primary_color_rgba,
        outline_color_rgba: style.outline_color_rgba,
        font_size: style.font_size,
        outline_width: style.outline_width,
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        strike_out: style.strike_out,
        spacing: style.spacing,
        scale_x_percent: style.scale_x_percent,
        scale_y_percent: style.scale_y_percent,
        border_style: style.border_style,
        shadow_depth: style.shadow_depth,
        blur: style.blur,
        alignment: style.alignment,
        margin_left: style.margin_left,
        margin_right: style.margin_right,
        margin_vertical: style.margin_vertical,
        override_mask: style.override_mask,
    }
    .normalized())
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn luma_upscaler_mode_from_c(mode: i32) -> LumaUpscalerMode {
    match ErikaLumaUpscalerMode::from_raw(mode) {
        ErikaLumaUpscalerMode::Off => LumaUpscalerMode::Off,
        ErikaLumaUpscalerMode::ArtCnnC4F16 => LumaUpscalerMode::ArtCnnC4F16,
        ErikaLumaUpscalerMode::ArtCnnC4F32 => LumaUpscalerMode::ArtCnnC4F32,
        ErikaLumaUpscalerMode::ArtCnnC4F16Ds => LumaUpscalerMode::ArtCnnC4F16Ds,
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn luma_upscaler_mode_to_c(mode: LumaUpscalerMode) -> i32 {
    match mode {
        LumaUpscalerMode::Off => ErikaLumaUpscalerMode::Off as i32,
        LumaUpscalerMode::ArtCnnC4F16 => ErikaLumaUpscalerMode::ArtCnnC4F16 as i32,
        LumaUpscalerMode::ArtCnnC4F32 => ErikaLumaUpscalerMode::ArtCnnC4F32 as i32,
        LumaUpscalerMode::ArtCnnC4F16Ds => ErikaLumaUpscalerMode::ArtCnnC4F16Ds as i32,
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn upscaler_backend_status_to_c(status: LumaUpscalerBackendStatus) -> i32 {
    match status {
        LumaUpscalerBackendStatus::Off => ErikaUpscalerBackendStatus::Off as i32,
        LumaUpscalerBackendStatus::Inactive => ErikaUpscalerBackendStatus::Inactive as i32,
        LumaUpscalerBackendStatus::Building => ErikaUpscalerBackendStatus::Building as i32,
        LumaUpscalerBackendStatus::Scalar => ErikaUpscalerBackendStatus::Scalar as i32,
        LumaUpscalerBackendStatus::SimdgroupMatrix => {
            ErikaUpscalerBackendStatus::SimdgroupMatrix as i32
        }
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn upscaler_status_to_c(stats: RendererRuntimeStats) -> ErikaUpscalerStatus {
    ErikaUpscalerStatus {
        requested_mode: luma_upscaler_mode_to_c(stats.upscaler_mode),
        active_backend: upscaler_backend_status_to_c(stats.upscaler_backend),
        fallback_count: stats.upscaler_fallbacks,
        upscaled_frames: stats.upscaled_frames,
        last_encode_micros: duration_micros_u64(stats.last_upscaler_encode_duration),
        last_gpu_micros: duration_micros_u64(stats.last_gpu_duration),
    }
}

fn output_status_to_c(status: OutputRuntimeStatus) -> ErikaOutputStatus {
    let requested_mode = match status.requested_mode {
        OutputMode::Sdr => ErikaPresenterOutputMode::Sdr as i32,
        OutputMode::AppleEdr { .. } => ErikaPresenterOutputMode::AppleEdr as i32,
        OutputMode::ExtendedLinear { .. } => ErikaPresenterOutputMode::ExtendedLinear as i32,
        OutputMode::Auto { .. } => ErikaPresenterOutputMode::Auto as i32,
    };
    let active_encoding = match status.active_encoding {
        ActiveOutputEncoding::SdrSrgb => 0,
        ActiveOutputEncoding::AppleEdr => 1,
        ActiveOutputEncoding::AndroidExtendedLinearScRgb => 2,
        ActiveOutputEncoding::Hdr10Pq => 3,
        ActiveOutputEncoding::LinuxExtendedLinearScRgb => 4,
    };
    let surface_format = match status.surface_format {
        OutputSurfaceFormat::EightBitUnorm => 0,
        OutputSurfaceFormat::TenBitUnorm => 1,
        OutputSurfaceFormat::SixteenBitFloat => 2,
    };
    ErikaOutputStatus {
        requested_mode,
        active_encoding,
        surface_format,
        native_data_space: status.native_data_space,
        requested_headroom: status.requested_headroom,
        active_headroom: status.active_headroom,
        active_headroom_known: status.active_headroom_known,
        extended_linear_active: status.extended_linear_active,
        fallback_reason: status.fallback_reason as i32,
        fallback_count: status.fallback_count,
        data_space_failures: status.data_space_failures,
        headroom_updates: status.headroom_updates,
        extended_linear_frames: status.extended_linear_frames,
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn resource_status_to_c(snapshot: PresenterRuntimeSnapshot) -> ErikaPresenterResourceStatus {
    let resources = snapshot.resources;
    ErikaPresenterResourceStatus {
        device_current_allocated_bytes: resources.device_current_allocated_bytes,
        device_recommended_working_set_bytes: resources.device_recommended_working_set_bytes,
        drawable_estimated_bytes: resources.drawable_estimated_bytes,
        video_frame_bytes: resources.video_frame_bytes,
        overlay_atlas_bytes: resources.overlay_atlas_bytes,
        danmaku_atlas_bytes: resources.danmaku_atlas_bytes,
        danmaku_vertex_buffer_bytes: resources.danmaku_vertex_buffer_bytes,
        upscaler_bytes: resources.upscaler_bytes,
        renderer_tracked_bytes: resources.renderer_tracked_bytes,
        presenter_cpu_danmaku_atlas_bytes: snapshot
            .current_danmaku_atlas_bytes
            .min(u64::MAX as usize) as u64,
        drawable_count: resources.drawable_count,
        output_mode_switches: resources.output_mode_switches,
    }
}

fn danmaku_config_from_c(
    config: ErikaDanmakuConfig,
    base: &DanmakuLayoutConfig,
) -> DanmakuLayoutConfig {
    DanmakuLayoutConfig {
        enabled: config.enabled,
        font_size: config.font_size,
        supersampling: base.supersampling,
        opacity: config.opacity,
        display_area: config.display_area,
        scroll_duration_seconds: config.scroll_duration_seconds,
        scroll_speed_factor: config.scroll_speed_factor,
        track_gap_ratio: config.track_gap_ratio,
        outline_width: config.outline_width,
        shadow_offset: [config.shadow_offset_x, config.shadow_offset_y],
        merge_duplicates: config.merge_duplicates,
        allow_stacking: config.allow_stacking,
        allow_scroll_overwrite: config.allow_scroll_overwrite,
        max_quantity: (config.max_quantity > 0).then_some(config.max_quantity),
        max_lines_per_mode: (config.max_lines_per_mode > 0).then_some(config.max_lines_per_mode),
        block_top: config.block_top,
        block_bottom: config.block_bottom,
        block_scroll: config.block_scroll,
        block_words: base.block_words.clone(),
        shadow_style: DanmakuShadowStyle::from_code(config.shadow_style),
        custom_font_family: base.custom_font_family.clone(),
        custom_font_file_path: base.custom_font_file_path.clone(),
        custom_font_face_index: base.custom_font_face_index,
        random_colors: base.random_colors,
    }
}

fn danmaku_config_to_c(config: &DanmakuLayoutConfig) -> ErikaDanmakuConfig {
    ErikaDanmakuConfig {
        enabled: config.enabled,
        font_size: config.font_size,
        opacity: config.opacity,
        display_area: config.display_area,
        scroll_duration_seconds: config.scroll_duration_seconds,
        scroll_speed_factor: config.scroll_speed_factor,
        track_gap_ratio: config.track_gap_ratio,
        outline_width: config.outline_width,
        shadow_offset_x: config.shadow_offset[0],
        shadow_offset_y: config.shadow_offset[1],
        merge_duplicates: config.merge_duplicates,
        allow_stacking: config.allow_stacking,
        allow_scroll_overwrite: config.allow_scroll_overwrite,
        max_quantity: config.max_quantity.unwrap_or(0),
        max_lines_per_mode: config.max_lines_per_mode.unwrap_or(0),
        block_top: config.block_top,
        block_bottom: config.block_bottom,
        block_scroll: config.block_scroll,
        shadow_style: config.shadow_style.code(),
    }
}

fn danmaku_block_words_from_json(json: &str) -> Result<Vec<String>, ErikaStatus> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|_| ErikaStatus::PlayerError)?;
    match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                serde_json::Value::String(value) => Ok(value),
                _ => Err(ErikaStatus::PlayerError),
            })
            .collect(),
        serde_json::Value::String(value) => Ok(vec![value]),
        _ => Err(ErikaStatus::PlayerError),
    }
}

#[cfg(all(
    any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ),
    test
))]
fn metal_output_mode_from_c(config: ErikaPresenterConfig) -> MetalOutputMode {
    presenter_config_from_c(config).renderer.output_mode
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_destroy(handle: *mut ErikaPresenterHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_destroy(_handle: *mut std::ffi::c_void) {}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_open(
    handle: *mut ErikaPresenterHandle,
    uri: *const c_char,
) -> ErikaStatus {
    unsafe { erika_presenter_open_with_headers(handle, uri, std::ptr::null(), 0) }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn retain_presenter_events_from_latest_open(handle: &mut ErikaPresenterHandle) {
    retain_presenter_events_from_latest_state(handle, PlayerState::Opening, "open", true);
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn retain_presenter_events_from_latest_state(
    handle: &mut ErikaPresenterHandle,
    boundary_state: PlayerState,
    boundary_name: &str,
    preserve_surface_lifecycle: bool,
) {
    let mut drained = handle.events.try_iter().collect::<Vec<_>>();
    let Some(boundary) = drained
        .iter()
        .rposition(|event| *event == PlayerEvent::StateChanged(boundary_state))
    else {
        // Argument/state validation can reject Open before Player::open establishes
        // a boundary. Preserve everything in that case.
        handle.pending_events.extend(drained);
        return;
    };
    let mut latest_boundary_events = drained.split_off(boundary);
    let stale_event_count = handle.pending_events.len().saturating_add(drained.len());
    if preserve_surface_lifecycle {
        handle
            .pending_events
            .retain(presenter_event_is_surface_lifecycle);
        handle.pending_events.extend(
            drained
                .into_iter()
                .filter(presenter_event_is_surface_lifecycle),
        );
    } else {
        handle.pending_events.clear();
    }
    let preserved_events = handle.pending_events.len();
    handle
        .pending_events
        .extend(latest_boundary_events.drain(..));
    let discarded_events = stale_event_count.saturating_sub(preserved_events);
    if discarded_events > 0 {
        capi_trace(format!(
            "fn=erika_presenter_{boundary_name}.discard_stale_events handle={handle:p} count={discarded_events}"
        ));
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn presenter_event_is_surface_lifecycle(event: &PlayerEvent) -> bool {
    matches!(
        event,
        PlayerEvent::SurfaceAttached(_) | PlayerEvent::SurfaceDetached
    )
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_open_with_headers(
    handle: *mut ErikaPresenterHandle,
    uri: *const c_char,
    headers: *const ErikaHttpHeader,
    header_count: usize,
) -> ErikaStatus {
    let options = open_options_raw(headers, header_count);
    unsafe { erika_presenter_open_with_options(handle, uri, &options) }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_open_with_options(
    handle: *mut ErikaPresenterHandle,
    uri: *const c_char,
    options: *const ErikaOpenOptions,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let uri = match c_string(uri) {
            Ok(uri) => uri,
            Err(status) => return status,
        };
        capi_trace(format!(
            "fn=erika_presenter_open handle={handle:p} uri={}",
            redacted_uri(&uri)
        ));
        let (headers, http_read_ahead_bytes, http_back_buffer_bytes) = match c_open_options(options)
        {
            Ok(parsed) => parsed,
            Err(status) => return status,
        };
        let request = MediaRequest::new(uri)
            .with_http_headers(headers)
            .map_http_read_ahead_bytes(http_read_ahead_bytes)
            .map_http_back_buffer_bytes(http_back_buffer_bytes);
        let status = status_from_player_result(handle.presenter.open(request));
        retain_presenter_events_from_latest_open(handle);
        capi_trace(format!(
            "fn=erika_presenter_open.done handle={handle:p} status={status:?}"
        ));
        status
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_play(handle: *mut ErikaPresenterHandle) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        capi_trace(format!("fn=erika_presenter_play handle={handle:p}"));
        let status = status_from_player_result(handle.presenter.play());
        capi_trace(format!(
            "fn=erika_presenter_play.done handle={handle:p} status={status:?}"
        ));
        status
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_pause(handle: *mut ErikaPresenterHandle) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        capi_trace(format!("fn=erika_presenter_pause handle={handle:p}"));
        let status = status_from_player_result(handle.presenter.pause());
        capi_trace(format!(
            "fn=erika_presenter_pause.done handle={handle:p} status={status:?}"
        ));
        status
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_stop(handle: *mut ErikaPresenterHandle) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        status_from_player_result(handle.presenter.stop())
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_close(handle: *mut ErikaPresenterHandle) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let status = status_from_player_result(handle.presenter.close());
        retain_presenter_events_from_latest_state(handle, PlayerState::Closed, "close", false);
        status
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_seek(
    handle: *mut ErikaPresenterHandle,
    position_micros: u64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        capi_trace(format!(
            "fn=erika_presenter_seek handle={handle:p} position_micros={position_micros}"
        ));
        let status = status_from_player_result(
            handle
                .presenter
                .seek(Duration::from_micros(position_micros)),
        );
        capi_trace(format!(
            "fn=erika_presenter_seek.done handle={handle:p} status={status:?}"
        ));
        status
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_playback_rate(
    handle: *mut ErikaPresenterHandle,
    rate: f64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        status_from_player_result(handle.presenter.set_playback_rate(rate))
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_volume(
    handle: *mut ErikaPresenterHandle,
    volume: f64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        handle.presenter.set_volume(volume);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_upscaler(
    handle: *mut ErikaPresenterHandle,
    mode: i32,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        handle
            .presenter
            .set_luma_upscaler(luma_upscaler_mode_from_c(mode));
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_subtitle_scale(
    handle: *mut ErikaPresenterHandle,
    scale: f64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        handle.presenter.set_subtitle_scale(scale);
        ErikaStatus::Ok
    })
}

/// Sets the fallback subtitle font. A null or empty `family`/`file_path`
/// clears that half of the selection and restores the platform default.
#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_subtitle_font(
    handle: *mut ErikaPresenterHandle,
    family: *const c_char,
    file_path: *const c_char,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let family = optional_c_string(family).unwrap_or_default();
        let file_path = optional_c_string(file_path).unwrap_or_default();
        handle.presenter.set_subtitle_font(family, file_path);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_subtitle_style(
    handle: *mut ErikaPresenterHandle,
    style: ErikaSubtitleStyle,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let style = match subtitle_style_from_c(style) {
            Ok(style) => style,
            Err(status) => return status,
        };
        handle.presenter.set_subtitle_style(style);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_register_subtitle_memory_font(
    handle: *mut ErikaPresenterHandle,
    data: *const u8,
    data_len: usize,
    out_font_id: *mut u64,
) -> ErikaStatus {
    if out_font_id.is_null() || (data_len > 0 && data.is_null()) {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let bytes = if data_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(data, data_len) }
        };
        match handle.presenter.register_subtitle_font_bytes(bytes) {
            Ok(id) => {
                unsafe { *out_font_id = id };
                ErikaStatus::Ok
            }
            Err(error) => player_error(error.to_string()),
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_select_subtitle_memory_fonts(
    handle: *mut ErikaPresenterHandle,
    font_ids: *const u64,
    font_id_count: usize,
) -> ErikaStatus {
    if font_id_count > 0 && font_ids.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let ids = if font_id_count == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(font_ids, font_id_count) }
        };
        status_from_player_result(handle.presenter.select_subtitle_memory_fonts(ids))
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_clear_subtitle_memory_fonts(
    handle: *mut ErikaPresenterHandle,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        handle.presenter.clear_subtitle_memory_fonts();
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_subtitle_memory_font_status(
    handle: *mut ErikaPresenterHandle,
    out_status: *mut ErikaSubtitleMemoryFontStatus,
) -> ErikaStatus {
    if out_status.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        unsafe {
            *out_status =
                subtitle_memory_font_status_to_c(handle.presenter.subtitle_memory_font_status())
        };
        ErikaStatus::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_subtitle_memory_font_status_free(
    status: *mut ErikaSubtitleMemoryFontStatus,
) {
    if status.is_null() {
        return;
    }
    let status = unsafe { &mut *status };
    if !status.selected_ids.is_null() {
        let slice = std::ptr::slice_from_raw_parts_mut(status.selected_ids, status.selected_count);
        unsafe { drop(Box::from_raw(slice)) };
    }
    *status = ErikaSubtitleMemoryFontStatus::default();
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_subtitle_memory_font_info(
    handle: *mut ErikaPresenterHandle,
    font_id: u64,
    out_info: *mut ErikaSubtitleMemoryFontInfo,
) -> ErikaStatus {
    if out_info.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let Some(info) = handle.presenter.subtitle_memory_font_info(font_id) else {
            return player_error(format!(
                "subtitle memory font ID {font_id} is not registered"
            ));
        };
        unsafe { *out_info = subtitle_memory_font_info_to_c(info) };
        ErikaStatus::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_subtitle_memory_font_info_free(
    info: *mut ErikaSubtitleMemoryFontInfo,
) {
    if info.is_null() {
        return;
    }
    let info = unsafe { &mut *info };
    if !info.faces.is_null() {
        let slice = std::ptr::slice_from_raw_parts_mut(info.faces, info.face_count);
        let mut faces = unsafe { Box::from_raw(slice) };
        for face in &mut faces {
            free_c_string(&mut face.families_json);
            free_c_string(&mut face.post_script_name);
        }
    }
    *info = ErikaSubtitleMemoryFontInfo::default();
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_output_headroom(
    handle: *mut ErikaPresenterHandle,
    headroom: f32,
    known: bool,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let headroom = if headroom.is_finite() {
            headroom.clamp(1.0, 10_000.0)
        } else {
            1.0
        };
        handle.presenter.set_output_headroom(headroom, known);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_upscaler_status(
    handle: *mut ErikaPresenterHandle,
    out_status: *mut ErikaUpscalerStatus,
) -> ErikaStatus {
    if out_status.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let status = upscaler_status_to_c(handle.presenter.runtime_snapshot().renderer);
        unsafe { *out_status = status };
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_output_status(
    handle: *mut ErikaPresenterHandle,
    out_status: *mut ErikaOutputStatus,
) -> ErikaStatus {
    if out_status.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let status = output_status_to_c(handle.presenter.runtime_snapshot().output);
        unsafe { *out_status = status };
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_resource_status(
    handle: *mut ErikaPresenterHandle,
    out_status: *mut ErikaPresenterResourceStatus,
) -> ErikaStatus {
    if out_status.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let status = resource_status_to_c(handle.presenter.runtime_snapshot());
        unsafe { *out_status = status };
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_add_external_subtitle(
    handle: *mut ErikaPresenterHandle,
    uri: *const c_char,
    out_track_id: *mut i64,
) -> ErikaStatus {
    if out_track_id.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let uri = match c_string(uri) {
            Ok(uri) => uri,
            Err(status) => return status,
        };
        match handle.presenter.add_external_subtitle(uri) {
            Ok(track) => {
                unsafe { *out_track_id = track.id };
                ErikaStatus::Ok
            }
            Err(error) => player_error(error.to_string()),
        }
    })
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_add_external_subtitle(
    _handle: *mut std::ffi::c_void,
    _uri: *const c_char,
    _out_track_id: *mut i64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_resource_status(
    _handle: *mut std::ffi::c_void,
    out_status: *mut ErikaPresenterResourceStatus,
) -> ErikaStatus {
    if out_status.is_null() {
        return ErikaStatus::NullPointer;
    }
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_output_headroom(
    _handle: *mut std::ffi::c_void,
    _headroom: f32,
    _known: bool,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_remove_subtitle_track(
    handle: *mut ErikaPresenterHandle,
    track_id: i64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        status_from_player_result(handle.presenter.remove_subtitle_track(track_id))
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_select_audio_track(
    handle: *mut ErikaPresenterHandle,
    track_id: i64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        status_from_player_result(
            handle
                .presenter
                .select_audio_track(track_id_option(track_id)),
        )
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_select_subtitle_track(
    handle: *mut ErikaPresenterHandle,
    track_id: i64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        status_from_player_result(
            handle
                .presenter
                .select_subtitle_track(track_id_option(track_id)),
        )
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_load_danmaku_file(
    handle: *mut ErikaPresenterHandle,
    uri: *const c_char,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let uri = match c_string(uri) {
            Ok(uri) => uri,
            Err(status) => return status,
        };
        match danmaku_timeline_from_uri(&uri) {
            Ok(timeline) => {
                handle.presenter.set_danmaku_timeline(timeline);
                ErikaStatus::Ok
            }
            Err(error) => {
                set_last_error(error);
                ErikaStatus::PlayerError
            }
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_load_danmaku_json(
    handle: *mut ErikaPresenterHandle,
    json: *const c_char,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let json = match c_string(json) {
            Ok(json) => json,
            Err(status) => return status,
        };
        match DanmakuTimeline::parse_auto(&json) {
            Ok(timeline) => {
                handle.presenter.set_danmaku_timeline(timeline);
                ErikaStatus::Ok
            }
            Err(error) => {
                set_last_error(format!("failed to parse danmaku JSON: {error}"));
                ErikaStatus::PlayerError
            }
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_add_danmaku_track_file(
    handle: *mut ErikaPresenterHandle,
    uri: *const c_char,
    name: *const c_char,
    offset_micros: i64,
    out_track_id: *mut u64,
) -> ErikaStatus {
    if out_track_id.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let uri = match c_string(uri) {
            Ok(uri) => uri,
            Err(status) => return status,
        };
        let name = optional_c_string(name).unwrap_or_else(|| danmaku_track_name_from_uri(&uri));
        match danmaku_timeline_from_uri(&uri) {
            Ok(timeline) => {
                let track_id = handle.presenter.add_danmaku_track(
                    timeline,
                    name,
                    DanmakuTrackSource::File(std::path::PathBuf::from(uri)),
                    offset_micros,
                );
                unsafe { *out_track_id = track_id };
                ErikaStatus::Ok
            }
            Err(error) => {
                set_last_error(error);
                ErikaStatus::PlayerError
            }
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_add_danmaku_track_json(
    handle: *mut ErikaPresenterHandle,
    json: *const c_char,
    name: *const c_char,
    offset_micros: i64,
    out_track_id: *mut u64,
) -> ErikaStatus {
    if out_track_id.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let json = match c_string(json) {
            Ok(json) => json,
            Err(status) => return status,
        };
        let name = optional_c_string(name).unwrap_or_else(|| "danmaku".to_string());
        match DanmakuTimeline::parse_auto(&json) {
            Ok(timeline) => {
                let track_id = handle.presenter.add_danmaku_track(
                    timeline,
                    name,
                    DanmakuTrackSource::Json,
                    offset_micros,
                );
                unsafe { *out_track_id = track_id };
                ErikaStatus::Ok
            }
            Err(_) => ErikaStatus::PlayerError,
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_remove_danmaku_track(
    handle: *mut ErikaPresenterHandle,
    track_id: u64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        if handle.presenter.remove_danmaku_track(track_id) {
            ErikaStatus::Ok
        } else {
            ErikaStatus::PlayerError
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_track_enabled(
    handle: *mut ErikaPresenterHandle,
    track_id: u64,
    enabled: bool,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        if handle
            .presenter
            .set_danmaku_track_enabled(track_id, enabled)
        {
            ErikaStatus::Ok
        } else {
            ErikaStatus::PlayerError
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_track_offset(
    handle: *mut ErikaPresenterHandle,
    track_id: u64,
    offset_micros: i64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        if handle
            .presenter
            .set_danmaku_track_offset(track_id, offset_micros)
        {
            ErikaStatus::Ok
        } else {
            ErikaStatus::PlayerError
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_global_offset(
    handle: *mut ErikaPresenterHandle,
    offset_micros: i64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        handle.presenter.set_danmaku_global_offset(offset_micros);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_danmaku_tracks(
    handle: *mut ErikaPresenterHandle,
    out_tracks: *mut ErikaDanmakuTrackInfo,
    capacity: usize,
    out_len: *mut usize,
) -> ErikaStatus {
    if out_len.is_null() || (capacity > 0 && out_tracks.is_null()) {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        write_danmaku_tracks_to_c(
            &handle.presenter.danmaku_tracks(),
            out_tracks,
            capacity,
            out_len,
        )
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_clear_danmaku(
    handle: *mut ErikaPresenterHandle,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        handle.presenter.clear_danmaku();
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_enabled(
    handle: *mut ErikaPresenterHandle,
    enabled: bool,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        handle.presenter.set_danmaku_enabled(enabled);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_debug_hud_enabled(
    handle: *mut ErikaPresenterHandle,
    enabled: bool,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        handle.presenter.set_debug_hud_enabled(enabled);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_config(
    handle: *mut ErikaPresenterHandle,
    config: ErikaDanmakuConfig,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let base = handle
            .presenter
            .danmaku_config()
            .cloned()
            .unwrap_or_default();
        handle
            .presenter
            .set_danmaku_config(danmaku_config_from_c(config, &base));
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_config_ptr(
    handle: *mut ErikaPresenterHandle,
    config: *const ErikaDanmakuConfig,
) -> ErikaStatus {
    if config.is_null() {
        return ErikaStatus::NullPointer;
    }
    unsafe { erika_presenter_set_danmaku_config(handle, *config) }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_danmaku_config(
    handle: *mut ErikaPresenterHandle,
    out_config: *mut ErikaDanmakuConfig,
) -> ErikaStatus {
    if out_config.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let config = handle
            .presenter
            .danmaku_config()
            .map(danmaku_config_to_c)
            .unwrap_or_default();
        unsafe {
            *out_config = config;
        }
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_font(
    handle: *mut ErikaPresenterHandle,
    family: *const c_char,
    file_path: *const c_char,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let family = optional_c_string(family).unwrap_or_default();
        let file_path = optional_c_string(file_path).unwrap_or_default();
        handle.presenter.set_danmaku_font(family, file_path);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_block_words_json(
    handle: *mut ErikaPresenterHandle,
    json: *const c_char,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let json = match c_string(json) {
            Ok(json) => json,
            Err(status) => return status,
        };
        let block_words = match danmaku_block_words_from_json(&json) {
            Ok(words) => words,
            Err(status) => return status,
        };
        let mut config = handle
            .presenter
            .danmaku_config()
            .cloned()
            .unwrap_or_default();
        config.block_words = block_words;
        handle.presenter.set_danmaku_config(config);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_random_colors(
    handle: *mut ErikaPresenterHandle,
    enabled: bool,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        let mut config = handle
            .presenter
            .danmaku_config()
            .cloned()
            .unwrap_or_default();
        config.random_colors = enabled;
        handle.presenter.set_danmaku_config(config);
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_track_selection(
    handle: *mut ErikaPresenterHandle,
    out_selection: *mut ErikaTrackSelection,
) -> ErikaStatus {
    if out_selection.is_null() {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        unsafe { *out_selection = track_selection_to_c(handle.presenter.track_selection()) };
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_tracks(
    handle: *mut ErikaPresenterHandle,
    out_tracks: *mut ErikaTrackInfo,
    capacity: usize,
    out_len: *mut usize,
) -> ErikaStatus {
    if out_len.is_null() || (capacity > 0 && out_tracks.is_null()) {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        write_tracks_to_c(&handle.presenter.tracks(), out_tracks, capacity, out_len)
    })
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_remove_subtitle_track(
    _handle: *mut std::ffi::c_void,
    _track_id: i64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_select_audio_track(
    _handle: *mut std::ffi::c_void,
    _track_id: i64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_select_subtitle_track(
    _handle: *mut std::ffi::c_void,
    _track_id: i64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_track_selection(
    _handle: *mut std::ffi::c_void,
    _out_selection: *mut ErikaTrackSelection,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_tracks(
    _handle: *mut std::ffi::c_void,
    _out_tracks: *mut ErikaTrackInfo,
    _capacity: usize,
    _out_len: *mut usize,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_playback_rate(
    _handle: *mut std::ffi::c_void,
    _rate: f64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_volume(
    _handle: *mut std::ffi::c_void,
    _volume: f64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_upscaler(
    _handle: *mut std::ffi::c_void,
    _mode: i32,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_subtitle_scale(
    _handle: *mut std::ffi::c_void,
    _scale: f64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_subtitle_font(
    _handle: *mut std::ffi::c_void,
    _family: *const c_char,
    _file_path: *const c_char,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_subtitle_style(
    _handle: *mut std::ffi::c_void,
    _style: ErikaSubtitleStyle,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_upscaler_status(
    _handle: *mut std::ffi::c_void,
    out_status: *mut ErikaUpscalerStatus,
) -> ErikaStatus {
    if out_status.is_null() {
        return ErikaStatus::NullPointer;
    }
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_output_status(
    _handle: *mut std::ffi::c_void,
    out_status: *mut ErikaOutputStatus,
) -> ErikaStatus {
    if out_status.is_null() {
        return ErikaStatus::NullPointer;
    }
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_load_danmaku_file(
    _handle: *mut std::ffi::c_void,
    _uri: *const c_char,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_load_danmaku_json(
    _handle: *mut std::ffi::c_void,
    _json: *const c_char,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_add_danmaku_track_file(
    _handle: *mut std::ffi::c_void,
    uri: *const c_char,
    _name: *const c_char,
    _offset_micros: i64,
    out_track_id: *mut u64,
) -> ErikaStatus {
    if uri.is_null() || out_track_id.is_null() {
        return ErikaStatus::NullPointer;
    }
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_add_danmaku_track_json(
    _handle: *mut std::ffi::c_void,
    json: *const c_char,
    _name: *const c_char,
    _offset_micros: i64,
    out_track_id: *mut u64,
) -> ErikaStatus {
    if json.is_null() || out_track_id.is_null() {
        return ErikaStatus::NullPointer;
    }
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_remove_danmaku_track(
    _handle: *mut std::ffi::c_void,
    _track_id: u64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_track_enabled(
    _handle: *mut std::ffi::c_void,
    _track_id: u64,
    _enabled: bool,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_track_offset(
    _handle: *mut std::ffi::c_void,
    _track_id: u64,
    _offset_micros: i64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_global_offset(
    _handle: *mut std::ffi::c_void,
    _offset_micros: i64,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_danmaku_tracks(
    _handle: *mut std::ffi::c_void,
    out_tracks: *mut ErikaDanmakuTrackInfo,
    capacity: usize,
    out_len: *mut usize,
) -> ErikaStatus {
    if out_len.is_null() || (capacity > 0 && out_tracks.is_null()) {
        return ErikaStatus::NullPointer;
    }
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_clear_danmaku(
    _handle: *mut std::ffi::c_void,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_enabled(
    _handle: *mut std::ffi::c_void,
    _enabled: bool,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_config(
    _handle: *mut std::ffi::c_void,
    _config: ErikaDanmakuConfig,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_config_ptr(
    _handle: *mut std::ffi::c_void,
    config: *const ErikaDanmakuConfig,
) -> ErikaStatus {
    if config.is_null() {
        return ErikaStatus::NullPointer;
    }
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_danmaku_config(
    _handle: *mut std::ffi::c_void,
    out_config: *mut ErikaDanmakuConfig,
) -> ErikaStatus {
    if out_config.is_null() {
        return ErikaStatus::NullPointer;
    }
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_font(
    _handle: *mut std::ffi::c_void,
    _family: *const c_char,
    _file_path: *const c_char,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_block_words_json(
    _handle: *mut std::ffi::c_void,
    json: *const c_char,
) -> ErikaStatus {
    if json.is_null() {
        return ErikaStatus::NullPointer;
    }
    ErikaStatus::PlayerError
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_danmaku_random_colors(
    _handle: *mut std::ffi::c_void,
    _enabled: bool,
) -> ErikaStatus {
    ErikaStatus::PlayerError
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_attach_metal_layer(
    handle: *mut ErikaPresenterHandle,
    raw_layer: u64,
    width: u32,
    height: u32,
    scale: f64,
) -> ErikaStatus {
    if raw_layer == 0 {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        status_from_player_result(handle.presenter.attach_surface(PlatformSurface::Metal(
            MetalSurfaceHandle::new(raw_layer, width, height, scale),
        )))
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_attach_flutter_texture(
    handle: *mut ErikaPresenterHandle,
    kind: ErikaFlutterTextureKind,
    texture_id: i64,
    width: u32,
    height: u32,
    scale: f64,
) -> ErikaStatus {
    if texture_id < 0 || width == 0 || height == 0 {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        status_from_player_result(
            handle
                .presenter
                .attach_surface(PlatformSurface::FlutterTexture(FlutterTextureHandle::new(
                    flutter_texture_kind_from_c(kind),
                    texture_id,
                    width,
                    height,
                    scale,
                ))),
        )
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_set_flutter_texture_buffer(
    handle: *mut ErikaPresenterHandle,
    raw_texture: u64,
    width: u32,
    height: u32,
) -> ErikaStatus {
    if raw_texture == 0 || width == 0 || height == 0 {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        status_from_player_result(handle.presenter.set_flutter_texture_buffer(
            raw_texture,
            width,
            height,
        ))
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_attach_wgpu_surface(
    handle: *mut ErikaPresenterHandle,
    kind: ErikaWgpuSurfaceKind,
    raw_window: u64,
    raw_display: u64,
    width: u32,
    height: u32,
    scale: f64,
) -> ErikaStatus {
    unsafe {
        erika_presenter_attach_wgpu_surface_with_output_capabilities(
            handle,
            kind,
            raw_window,
            raw_display,
            width,
            height,
            scale,
            ErikaSurfaceOutputCapabilities::default(),
        )
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_attach_wgpu_surface_with_output_capabilities(
    handle: *mut ErikaPresenterHandle,
    kind: ErikaWgpuSurfaceKind,
    raw_window: u64,
    raw_display: u64,
    width: u32,
    height: u32,
    scale: f64,
    output_capabilities: ErikaSurfaceOutputCapabilities,
) -> ErikaStatus {
    if raw_window == 0 {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        status_from_player_result(
            handle.presenter.attach_surface(PlatformSurface::Wgpu(
                wgpu_surface_handle_from_c(kind, raw_window, raw_display, width, height, scale)
                    .with_output_capabilities(output_capabilities.into()),
            )),
        )
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_attach_windows_hwnd(
    handle: *mut ErikaPresenterHandle,
    hwnd: u64,
    hinstance: u64,
    width: u32,
    height: u32,
    scale: f64,
) -> ErikaStatus {
    unsafe {
        erika_presenter_attach_wgpu_surface(
            handle,
            ErikaWgpuSurfaceKind::WindowsHwnd,
            hwnd,
            hinstance,
            width,
            height,
            scale,
        )
    }
}

/// Returns an AddRef'd IUnknown for the presenter's DirectComposition swap
/// chain. The caller owns the returned COM reference and must Release it.
#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_windows_composition_swapchain_iunknown(
    handle: *mut ErikaPresenterHandle,
    out_swapchain: *mut *mut std::ffi::c_void,
) -> ErikaStatus {
    if out_swapchain.is_null() {
        set_last_error("DirectComposition swap chain output pointer is null");
        return ErikaStatus::NullPointer;
    }
    unsafe { *out_swapchain = std::ptr::null_mut() };
    with_presenter_mut(handle, |handle| {
        let Some(swapchain) = handle.presenter.composition_swapchain_iunknown() else {
            set_last_error("presenter does not own a DirectComposition swap chain");
            return ErikaStatus::PlayerError;
        };
        unsafe { *out_swapchain = swapchain };
        ErikaStatus::Ok
    })
}

/// Returns an AddRef'd IUnknown for the latest completed, immutable Windows
/// Flutter SDR frame. The caller owns the COM reference and must Release it.
#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_windows_flutter_texture_iunknown(
    handle: *mut ErikaPresenterHandle,
    out_texture: *mut *mut std::ffi::c_void,
) -> ErikaStatus {
    if out_texture.is_null() {
        set_last_error("Windows Flutter texture output pointer is null");
        return ErikaStatus::NullPointer;
    }
    unsafe { *out_texture = std::ptr::null_mut() };
    with_presenter_mut(handle, |handle| {
        let Some(texture) = handle.presenter.windows_flutter_texture_iunknown() else {
            set_last_error("presenter does not own a Windows Flutter texture");
            return ErikaStatus::PlayerError;
        };
        unsafe { *out_texture = texture };
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_resize_surface(
    handle: *mut ErikaPresenterHandle,
    width: u32,
    height: u32,
    scale: f64,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        status_from_player_result(handle.presenter.resize_surface(width, height, scale))
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_detach_surface(
    handle: *mut ErikaPresenterHandle,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| {
        status_from_player_result(handle.presenter.detach_surface())
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_render_tick(
    handle: *mut ErikaPresenterHandle,
    time_seconds: f64,
    out_stats: *mut ErikaPresenterStats,
) -> ErikaStatus {
    unsafe {
        erika_presenter_render_tick_with_timing(handle, time_seconds, std::ptr::null(), out_stats)
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
// The legacy entry point calls this symbol even without a target. Keep that
// reference through LTO so Apple static linking retains the optional dlsym API.
#[inline(never)]
pub unsafe extern "C" fn erika_presenter_render_tick_with_timing(
    handle: *mut ErikaPresenterHandle,
    time_seconds: f64,
    presentation_delay_seconds: *const f64,
    out_stats: *mut ErikaPresenterStats,
) -> ErikaStatus {
    // Translate the caller's relative monotonic target before taking any
    // presenter lock or pumping audio/video, so work duration cannot move it.
    let presentation_time = if presentation_delay_seconds.is_null() {
        None
    } else {
        let delay = unsafe { *presentation_delay_seconds };
        if !delay.is_finite() || delay.abs() > 0.25 {
            return player_error("presentation delay must be finite and within +/-0.25 seconds");
        }
        let now = std::time::Instant::now();
        let delta = std::time::Duration::from_secs_f64(delay.abs());
        if delay >= 0.0 {
            now.checked_add(delta)
        } else {
            now.checked_sub(delta)
        }
    };
    with_presenter_mut(handle, |handle| {
        let result = match presentation_time {
            Some(target) => handle.presenter.render_tick_at(time_seconds, target),
            None => handle.presenter.render_tick(time_seconds),
        };
        match result {
            Ok(_stats) => {
                if !out_stats.is_null() {
                    let snapshot = handle.presenter.runtime_snapshot();
                    unsafe { *out_stats = presenter_stats_to_c(snapshot) };
                }
                clear_last_error();
                ErikaStatus::Ok
            }
            Err(error) => player_error(format!("render_tick failed: {error}")),
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_audio_only_tick(
    handle: *mut ErikaPresenterHandle,
    out_stats: *mut ErikaPresenterStats,
) -> ErikaStatus {
    with_presenter_mut(handle, |handle| match handle.presenter.audio_only_tick() {
        Ok(_stats) => {
            if !out_stats.is_null() {
                let snapshot = handle.presenter.runtime_snapshot();
                unsafe { *out_stats = presenter_stats_to_c(snapshot) };
            }
            clear_last_error();
            ErikaStatus::Ok
        }
        Err(error) => player_error(format!("audio_only_tick failed: {error}")),
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_get_stats(
    handle: *mut ErikaPresenterHandle,
    out_stats: *mut ErikaPresenterStats,
) -> ErikaStatus {
    if out_stats.is_null() {
        set_last_error("presenter stats output pointer is null");
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        let snapshot = handle.presenter.runtime_snapshot();
        unsafe { *out_stats = presenter_stats_to_c(snapshot) };
        ErikaStatus::Ok
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_capture_frame_rgba(
    handle: *mut ErikaPresenterHandle,
    width: u32,
    height: u32,
    out_rgba: *mut u8,
    out_capacity: usize,
) -> ErikaStatus {
    unsafe {
        capture_presenter_frame_rgba_into(handle, width, height, out_rgba, out_capacity, false)
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
/// Captures the current video, subtitles and enabled danmaku as SDR RGBA8.
///
/// # Safety
/// `handle` must be null or a live presenter accessed exclusively for this
/// call. `out_rgba` must be null or point to a writable allocation of at least
/// `out_capacity` bytes, which must not overlap the presenter's storage.
pub unsafe extern "C" fn erika_presenter_capture_frame_rgba_with_danmaku(
    handle: *mut ErikaPresenterHandle,
    width: u32,
    height: u32,
    out_rgba: *mut u8,
    out_capacity: usize,
) -> ErikaStatus {
    unsafe {
        capture_presenter_frame_rgba_into(handle, width, height, out_rgba, out_capacity, true)
    }
}

/// Renders an immutable native GPU frame, including enabled danmaku.
///
/// # Safety
/// `handle` must be null or an exclusively accessed live presenter. `out_frame`
/// must be null or point to writable storage. Release successful frames only
/// after all consumer GPU work and imported aliases are retired.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_export_frame_texture(
    handle: *mut ErikaPresenterHandle,
    width: u32,
    height: u32,
    content_scale: f64,
    out_frame: *mut *mut ErikaTextureFrame,
) -> ErikaStatus {
    if out_frame.is_null() {
        set_last_error("native texture frame output pointer is null");
        return ErikaStatus::NullPointer;
    }
    unsafe { *out_frame = std::ptr::null_mut() };
    with_presenter_mut(handle, |handle| {
        match handle
            .presenter
            .export_frame_texture(width, height, content_scale)
        {
            Ok(Some(frame)) => {
                unsafe { *out_frame = Box::into_raw(Box::new(ErikaTextureFrame { frame })) };
                ErikaStatus::Ok
            }
            Ok(None) => ErikaStatus::NoEvent,
            Err(error) => player_error(error.to_string()),
        }
    })
}

/// Reads borrowed native handles and their import metadata.
///
/// # Safety
/// `frame` must be null or a live frame and `out_info` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_texture_frame_get_info(
    frame: *const ErikaTextureFrame,
    out_info: *mut ErikaExternalTextureInfo,
) -> ErikaStatus {
    if frame.is_null() || out_info.is_null() {
        set_last_error("native texture frame or info pointer is null");
        return ErikaStatus::NullPointer;
    }
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        *out_info = *(*frame).frame.info()
    })) {
        Ok(()) => {
            clear_last_error();
            ErikaStatus::Ok
        }
        Err(_) => {
            set_last_error("panic while reading native texture frame info");
            ErikaStatus::Panic
        }
    }
}

/// Releases a frame and its native texture. Null is accepted.
///
/// # Safety
/// Release each successful frame exactly once, after all consuming GPU work
/// and imported aliases have been retired. No concurrent frame access.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_texture_frame_release(frame: *mut ErikaTextureFrame) {
    if !frame.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { drop(Box::from_raw(frame)) }));
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
unsafe fn capture_presenter_frame_rgba_into(
    handle: *mut ErikaPresenterHandle,
    width: u32,
    height: u32,
    out_rgba: *mut u8,
    out_capacity: usize,
    include_danmaku: bool,
) -> ErikaStatus {
    let Some(expected_len) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return ErikaStatus::PlayerError;
    };
    if expected_len == 0 || out_rgba.is_null() || out_capacity < expected_len {
        return ErikaStatus::NullPointer;
    }
    with_presenter_mut(handle, |handle| {
        match capture_presenter_frame_rgba(handle, width, height, include_danmaku) {
            Ok(Some(rgba)) => {
                unsafe {
                    std::ptr::copy_nonoverlapping(rgba.as_ptr(), out_rgba, expected_len);
                }
                ErikaStatus::Ok
            }
            Ok(None) => player_error(format!(
                "capture_frame_rgba has no current video frame for {width}x{height} capture"
            )),
            Err(error) => player_error(error),
        }
    })
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn capture_presenter_frame_rgba(
    handle: &mut ErikaPresenterHandle,
    width: u32,
    height: u32,
    include_danmaku: bool,
) -> Result<Option<Vec<u8>>, String> {
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            format!("capture_frame_rgba dimensions overflow for {width}x{height} RGBA")
        })?;
    let capture = if include_danmaku {
        handle
            .presenter
            .capture_frame_rgba_with_danmaku(width, height)
    } else {
        handle.presenter.capture_frame_rgba(width, height)
    };
    match capture {
        Ok(Some(rgba)) if rgba.len() == expected_len => Ok(Some(rgba)),
        Ok(Some(rgba)) => Err(format!(
            "capture_frame_rgba returned {} bytes, expected {expected_len} for {width}x{height} RGBA",
            rgba.len()
        )),
        Ok(None) => Ok(None),
        Err(error) => Err(format!("capture_frame_rgba failed: {error}")),
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erika_presenter_poll_event(
    handle: *mut ErikaPresenterHandle,
    out_event: *mut ErikaEvent,
) -> ErikaStatus {
    if out_event.is_null() {
        set_last_error("event output pointer is null");
        return ErikaStatus::NullPointer;
    }
    if handle.is_null() {
        set_last_error("Erika presenter handle pointer is null");
        return ErikaStatus::NullPointer;
    }
    match catch_unwind(AssertUnwindSafe(|| {
        let handle = unsafe { &mut *handle };
        let next_event = handle
            .pending_events
            .pop_front()
            .map(Ok)
            .unwrap_or_else(|| handle.events.try_recv());
        match next_event {
            Ok(event) => {
                let event = event_to_c(event);
                let preserves_message = matches!(
                    event.kind,
                    ErikaEventKind::Error
                        | ErikaEventKind::VideoDecoderChanged
                        | ErikaEventKind::AudioOutputChanged
                );
                capi_trace(format!(
                    "fn=erika_presenter_poll_event event={:?} state={:?} position_micros={} duration_micros={} video={}x{} tracks={}/{}/{}",
                    event.kind,
                    event.state,
                    event.position_micros,
                    event.duration_micros,
                    event.video.width,
                    event.video.height,
                    event.tracks.video,
                    event.tracks.audio,
                    event.tracks.subtitle,
                ));
                unsafe { *out_event = event };
                if !preserves_message {
                    clear_last_error();
                }
                ErikaStatus::Ok
            }
            Err(crossbeam_channel::TryRecvError::Empty) => ErikaStatus::NoEvent,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                player_error("presenter event channel disconnected")
            }
        }
    })) {
        Ok(status) => status,
        Err(_) => {
            set_last_error("panic while polling Erika presenter event");
            ErikaStatus::Panic
        }
    }
}

#[track_caller]
fn with_handle_mut(
    handle: *mut ErikaHandle,
    f: impl FnOnce(&mut ErikaHandle) -> ErikaStatus,
) -> ErikaStatus {
    if handle.is_null() {
        set_last_error("Erika handle pointer is null");
        return ErikaStatus::NullPointer;
    }
    match catch_unwind(AssertUnwindSafe(|| f(unsafe { &mut *handle }))) {
        Ok(status) => finalize_status(status),
        Err(payload) => {
            set_last_error(report_capi_panic(
                "Erika C ABI call",
                Location::caller(),
                payload,
            ));
            ErikaStatus::Panic
        }
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
#[track_caller]
fn with_presenter_mut(
    handle: *mut ErikaPresenterHandle,
    f: impl FnOnce(&mut ErikaPresenterHandle) -> ErikaStatus,
) -> ErikaStatus {
    if handle.is_null() {
        set_last_error("Erika presenter handle pointer is null");
        return ErikaStatus::NullPointer;
    }
    match catch_unwind(AssertUnwindSafe(|| f(unsafe { &mut *handle }))) {
        Ok(status) => finalize_status(status),
        Err(payload) => {
            set_last_error(report_capi_panic(
                "Erika presenter C ABI call",
                Location::caller(),
                payload,
            ));
            ErikaStatus::Panic
        }
    }
}

fn c_string(ptr: *const c_char) -> Result<String, ErikaStatus> {
    if ptr.is_null() {
        set_last_error("required C string pointer is null");
        return Err(ErikaStatus::NullPointer);
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(str::to_string)
        .map_err(|_| {
            set_last_error("required C string is not valid UTF-8");
            ErikaStatus::InvalidUtf8
        })
}

fn optional_c_string_result(ptr: *const c_char) -> Result<String, ErikaStatus> {
    if ptr.is_null() {
        return Ok(String::new());
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(|value| value.trim().to_string())
        .map_err(|_| {
            set_last_error("optional C string is not valid UTF-8");
            ErikaStatus::InvalidUtf8
        })
}

fn c_http_headers(
    headers: *const ErikaHttpHeader,
    header_count: usize,
) -> Result<Vec<(String, String)>, ErikaStatus> {
    if header_count == 0 {
        return Ok(Vec::new());
    }
    if headers.is_null() {
        set_last_error("HTTP headers pointer is null while header count is non-zero");
        return Err(ErikaStatus::NullPointer);
    }
    let headers = unsafe { std::slice::from_raw_parts(headers, header_count) };
    headers
        .iter()
        .map(|header| {
            let name = c_string(header.name)?;
            let value = c_string(header.value)?;
            if let Some(error) = http_header_error(&name, &value) {
                set_last_error(error);
                return Err(ErikaStatus::PlayerError);
            }
            Ok((name, value))
        })
        .collect()
}

/// Packs the legacy (headers, header_count) pair into an `ErikaOpenOptions`
/// so `erika_open_with_headers` can delegate to `erika_open_with_options`.
/// Returned by value: the struct must live on the caller's stack for as long
/// as the delegated call reads it.
fn open_options_raw(headers: *const ErikaHttpHeader, header_count: usize) -> ErikaOpenOptions {
    ErikaOpenOptions {
        headers,
        header_count,
        http_read_ahead_bytes: 0,
        http_back_buffer_bytes: 0,
        reserved: [0; 2],
    }
}

/// Validates an `ErikaOpenOptions` at the ABI boundary. A NULL options pointer
/// means "defaults" (no headers, default read-ahead). `http_read_ahead_bytes`
/// and `http_back_buffer_bytes` of 0 also mean default; reserved fields must
/// stay zero for forward compatibility.
fn c_open_options(
    options: *const ErikaOpenOptions,
) -> Result<(Vec<(String, String)>, Option<u64>, Option<u64>), ErikaStatus> {
    if options.is_null() {
        return Ok((Vec::new(), None, None));
    }
    let options = unsafe { &*options };
    for field in options.reserved {
        if field != 0 {
            set_last_error("ErikaOpenOptions.reserved must be zero");
            return Err(ErikaStatus::PlayerError);
        }
    }
    let headers = c_http_headers(options.headers, options.header_count)?;
    let read_ahead = (options.http_read_ahead_bytes > 0).then_some(options.http_read_ahead_bytes);
    let back_buffer =
        (options.http_back_buffer_bytes > 0).then_some(options.http_back_buffer_bytes);
    Ok((headers, read_ahead, back_buffer))
}

/// Headers liberika derives itself for every request. Accepting a caller override
/// would append a second copy (ureq appends rather than replaces), which makes
/// servers answer requests liberika cannot interpret — a duplicated `Range` in
/// particular can yield a `200` full-entity response.
const RESERVED_HTTP_HEADERS: [&str; 5] = [
    "range",
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
];

/// Validates a caller-supplied header at the ABI boundary so a malformed name
/// or value fails at `open` instead of inside every later range request.
fn http_header_error(name: &str, value: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("HTTP header name is empty".to_string());
    }
    if !name.bytes().all(is_http_token_byte) {
        return Some(format!(
            "HTTP header name `{name}` contains characters that are not allowed in a header name"
        ));
    }
    if RESERVED_HTTP_HEADERS
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Some(format!(
            "HTTP header `{name}` is managed by Erika and cannot be overridden"
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
    {
        return Some(format!(
            "HTTP header `{name}` has a value containing characters that are not allowed in a header value"
        ));
    }
    None
}

/// RFC 9110 `token` characters, the only bytes valid in a header field name.
fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn optional_c_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()?
        .trim()
        .to_string();
    (!value.is_empty()).then_some(value)
}

fn status_from_player_result(result: erika::Result<()>) -> ErikaStatus {
    match result {
        Ok(()) => {
            clear_last_error();
            ErikaStatus::Ok
        }
        Err(error) => player_error(error.to_string()),
    }
}

fn track_id_option(track_id: i64) -> Option<i64> {
    (track_id >= 0).then_some(track_id)
}

fn write_tracks_to_c(
    tracks: &[TrackInfo],
    out_tracks: *mut ErikaTrackInfo,
    capacity: usize,
    out_len: *mut usize,
) -> ErikaStatus {
    unsafe { *out_len = tracks.len() };
    if capacity == 0 {
        return ErikaStatus::Ok;
    }
    let count = tracks.len().min(capacity);
    for (index, track) in tracks.iter().take(count).enumerate() {
        unsafe { *out_tracks.add(index) = track_info_to_c(track) };
    }
    ErikaStatus::Ok
}

fn write_danmaku_tracks_to_c(
    tracks: &[DanmakuTrackInfo],
    out_tracks: *mut ErikaDanmakuTrackInfo,
    capacity: usize,
    out_len: *mut usize,
) -> ErikaStatus {
    unsafe { *out_len = tracks.len() };
    if capacity == 0 {
        return ErikaStatus::Ok;
    }
    let count = tracks.len().min(capacity);
    for (index, track) in tracks.iter().take(count).enumerate() {
        unsafe { *out_tracks.add(index) = danmaku_track_info_to_c(track) };
    }
    ErikaStatus::Ok
}

fn danmaku_track_info_to_c(track: &DanmakuTrackInfo) -> ErikaDanmakuTrackInfo {
    ErikaDanmakuTrackInfo {
        id: track.id,
        enabled: track.enabled,
        offset_micros: track.offset_micros,
        item_count: track.item_count,
        name: option_string_to_c(Some(&track.name)),
        source: option_string_to_c(Some(&track.source)),
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn subtitle_memory_font_status_to_c(
    status: SubtitleMemoryFontStatus,
) -> ErikaSubtitleMemoryFontStatus {
    let mut selected_ids = status.selected_ids.into_boxed_slice();
    let selected_ids_ptr = selected_ids.as_mut_ptr();
    std::mem::forget(selected_ids);
    ErikaSubtitleMemoryFontStatus {
        registered_count: status.registered_count,
        registered_bytes: status.registered_bytes,
        selected_count: status.selected_count,
        generation: status.generation,
        selected_ids: selected_ids_ptr,
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn subtitle_memory_font_info_to_c(info: SubtitleMemoryFontInfo) -> ErikaSubtitleMemoryFontInfo {
    let mut faces = info
        .faces
        .into_iter()
        .map(subtitle_memory_font_face_to_c)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let face_count = faces.len();
    let faces_ptr = faces.as_mut_ptr();
    std::mem::forget(faces);
    ErikaSubtitleMemoryFontInfo {
        id: info.id,
        byte_len: info.byte_len,
        faces: faces_ptr,
        face_count,
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn subtitle_memory_font_face_to_c(face: SubtitleMemoryFontFace) -> ErikaSubtitleMemoryFontFace {
    ErikaSubtitleMemoryFontFace {
        index: face.index,
        families_json: option_string_to_c(Some(
            &serde_json::to_string(&face.families).unwrap_or_else(|_| "[]".to_string()),
        )),
        post_script_name: option_string_to_c(Some(&face.post_script_name)),
        weight: face.weight,
        italic: face.italic,
        monospaced: face.monospaced,
    }
}

fn danmaku_timeline_from_uri(uri: &str) -> std::result::Result<DanmakuTimeline, String> {
    let bytes = erika::source::read_uri_to_end(uri)
        .map_err(|error| format!("failed to read danmaku source {uri}: {error}"))?;
    let input = String::from_utf8(bytes)
        .map_err(|error| format!("danmaku source {uri} is not valid UTF-8: {error}"))?;
    DanmakuTimeline::parse_auto(input.strip_prefix('\u{feff}').unwrap_or(&input))
        .map_err(|error| format!("failed to parse danmaku source {uri}: {error}"))
}

fn danmaku_track_name_from_uri(uri: &str) -> String {
    std::path::Path::new(uri)
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("danmaku")
        .to_string()
}

fn track_info_to_c(track: &TrackInfo) -> ErikaTrackInfo {
    ErikaTrackInfo {
        id: track.id,
        kind: track_kind_to_c(track.kind),
        source: track_source_to_c(track.source),
        selected: track.selected,
        can_remove: track.can_remove,
        title: option_string_to_c(track.title.as_deref()),
        language: option_string_to_c(track.language.as_deref()),
        codec: option_string_to_c(track.codec.as_deref()),
        width: track.width,
        height: track.height,
        sample_rate: track.sample_rate,
        channels: track.channels,
        pixel_format: option_string_to_c(track.pixel_format.as_deref()),
        sample_format: option_string_to_c(track.sample_format.as_deref()),
        profile: option_string_to_c(track.profile.as_deref()),
        level: track.level.unwrap_or(0),
        bit_rate: track.bit_rate.unwrap_or(0),
        frame_rate_numerator: track
            .frame_rate
            .as_ref()
            .map(|rate| rate.numerator)
            .unwrap_or(0),
        frame_rate_denominator: track
            .frame_rate
            .as_ref()
            .map(|rate| rate.denominator)
            .unwrap_or(0),
    }
}

fn track_kind_to_c(kind: TrackKind) -> ErikaTrackKind {
    match kind {
        TrackKind::Video => ErikaTrackKind::Video,
        TrackKind::Audio => ErikaTrackKind::Audio,
        TrackKind::Subtitle => ErikaTrackKind::Subtitle,
    }
}

fn track_source_to_c(source: TrackSource) -> ErikaTrackSource {
    match source {
        TrackSource::Embedded => ErikaTrackSource::Embedded,
        TrackSource::External => ErikaTrackSource::External,
    }
}

fn track_selection_to_c(selection: TrackSelection) -> ErikaTrackSelection {
    ErikaTrackSelection {
        video: selection.video.unwrap_or(-1),
        audio: selection.audio.unwrap_or(-1),
        subtitle: selection.subtitle.unwrap_or(-1),
    }
}

fn option_string_to_c(value: Option<&str>) -> *mut c_char {
    let Some(value) = value else {
        return std::ptr::null_mut();
    };
    match CString::new(value) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn free_c_string(ptr: &mut *mut c_char) {
    if ptr.is_null() {
        return;
    }
    let raw = *ptr;
    *ptr = std::ptr::null_mut();
    unsafe { drop(CString::from_raw(raw)) };
}

fn event_to_c(event: PlayerEvent) -> ErikaEvent {
    match event {
        PlayerEvent::StateChanged(state) => ErikaEvent {
            kind: ErikaEventKind::StateChanged,
            state: state_to_c(state),
            ..ErikaEvent::default()
        },
        PlayerEvent::DurationChanged(duration) => ErikaEvent {
            kind: ErikaEventKind::DurationChanged,
            duration_micros: duration.map(duration_micros_i64).unwrap_or(-1),
            ..ErikaEvent::default()
        },
        PlayerEvent::PositionChanged(position) => ErikaEvent {
            kind: ErikaEventKind::PositionChanged,
            position_micros: duration_micros_u64(position),
            ..ErikaEvent::default()
        },
        PlayerEvent::TracksChanged(tracks) => {
            let mut counts = ErikaTrackCounts::default();
            for track in tracks {
                match track.kind {
                    TrackKind::Video => counts.video = counts.video.saturating_add(1),
                    TrackKind::Audio => counts.audio = counts.audio.saturating_add(1),
                    TrackKind::Subtitle => counts.subtitle = counts.subtitle.saturating_add(1),
                }
            }
            ErikaEvent {
                kind: ErikaEventKind::TracksChanged,
                tracks: counts,
                ..ErikaEvent::default()
            }
        }
        PlayerEvent::TrackSelectionChanged(_) => ErikaEvent {
            kind: ErikaEventKind::TrackSelectionChanged,
            ..ErikaEvent::default()
        },
        PlayerEvent::BufferingChanged(buffering) => ErikaEvent {
            kind: ErikaEventKind::BufferingChanged,
            buffering,
            ..ErikaEvent::default()
        },
        PlayerEvent::VideoParamsChanged(params) => ErikaEvent {
            kind: ErikaEventKind::VideoParamsChanged,
            video: ErikaVideoParams {
                width: params.width,
                height: params.height,
                primaries: params.primaries as u32,
                transfer: transfer_to_c(params.transfer),
            },
            ..ErikaEvent::default()
        },
        PlayerEvent::VideoDecoderChanged(event) => {
            set_last_error(event.structured_message());
            ErikaEvent {
                kind: ErikaEventKind::VideoDecoderChanged,
                ..ErikaEvent::default()
            }
        }
        PlayerEvent::AudioOutputChanged(event) => {
            set_last_error(event.structured_message());
            ErikaEvent {
                kind: ErikaEventKind::AudioOutputChanged,
                ..ErikaEvent::default()
            }
        }
        PlayerEvent::SurfaceAttached(_) => ErikaEvent {
            kind: ErikaEventKind::SurfaceAttached,
            ..ErikaEvent::default()
        },
        PlayerEvent::SurfaceDetached => ErikaEvent {
            kind: ErikaEventKind::SurfaceDetached,
            ..ErikaEvent::default()
        },
        PlayerEvent::Error(error) => {
            set_last_error(error.to_string());
            ErikaEvent {
                kind: ErikaEventKind::Error,
                status: ErikaStatus::PlayerError,
                ..ErikaEvent::default()
            }
        }
    }
}

fn state_to_c(state: PlayerState) -> ErikaState {
    match state {
        PlayerState::Idle => ErikaState::Idle,
        PlayerState::Opening => ErikaState::Opening,
        PlayerState::Ready => ErikaState::Ready,
        PlayerState::Playing => ErikaState::Playing,
        PlayerState::Paused => ErikaState::Paused,
        PlayerState::Stopped => ErikaState::Stopped,
        PlayerState::Closed => ErikaState::Closed,
        PlayerState::Error => ErikaState::Error,
    }
}

fn transfer_to_c(transfer: TransferFunction) -> u32 {
    match transfer {
        TransferFunction::Unknown => 0,
        TransferFunction::Srgb => 1,
        TransferFunction::Bt1886 => 2,
        TransferFunction::Pq => 3,
        TransferFunction::Hlg => 4,
    }
}

fn wgpu_surface_kind_from_c(kind: ErikaWgpuSurfaceKind) -> WgpuSurfaceKind {
    match kind {
        ErikaWgpuSurfaceKind::Unknown => WgpuSurfaceKind::Unknown,
        ErikaWgpuSurfaceKind::MacOsNsView => WgpuSurfaceKind::MacOsNsView,
        ErikaWgpuSurfaceKind::MacOsCaMetalLayer => WgpuSurfaceKind::MacOsCaMetalLayer,
        ErikaWgpuSurfaceKind::IosUiView => WgpuSurfaceKind::IosUiView,
        ErikaWgpuSurfaceKind::WindowsHwnd => WgpuSurfaceKind::WindowsHwnd,
        ErikaWgpuSurfaceKind::XlibWindow => WgpuSurfaceKind::XlibWindow,
        ErikaWgpuSurfaceKind::WaylandSurface => WgpuSurfaceKind::WaylandSurface,
        ErikaWgpuSurfaceKind::AndroidNativeWindow => WgpuSurfaceKind::AndroidNativeWindow,
        ErikaWgpuSurfaceKind::OhosNativeWindow => WgpuSurfaceKind::OhosNativeWindow,
    }
}

fn wgpu_surface_handle_from_c(
    kind: ErikaWgpuSurfaceKind,
    raw_window: u64,
    raw_display: u64,
    width: u32,
    height: u32,
    scale: f64,
) -> WgpuSurfaceHandle {
    let kind = wgpu_surface_kind_from_c(kind);
    // The public C ABI defines width/height as exact physical pixels. `scale`
    // is retained independently for logical UI content such as danmaku.
    WgpuSurfaceHandle::new(kind, raw_window, raw_display, width, height, scale)
}

fn flutter_texture_kind_from_c(kind: ErikaFlutterTextureKind) -> FlutterTextureKind {
    match kind {
        ErikaFlutterTextureKind::Unknown => FlutterTextureKind::Unknown,
        ErikaFlutterTextureKind::MacOsTextureRegistrar => FlutterTextureKind::MacOsTextureRegistrar,
        ErikaFlutterTextureKind::IosTextureRegistrar => FlutterTextureKind::IosTextureRegistrar,
        ErikaFlutterTextureKind::AndroidSurfaceTexture => FlutterTextureKind::AndroidSurfaceTexture,
        ErikaFlutterTextureKind::WindowsTextureRegistrar => {
            FlutterTextureKind::WindowsTextureRegistrar
        }
        ErikaFlutterTextureKind::LinuxTextureRegistrar => FlutterTextureKind::LinuxTextureRegistrar,
    }
}

fn duration_micros_i64(duration: Duration) -> i64 {
    duration.as_micros().min(i64::MAX as u128) as i64
}

fn duration_micros_u64(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn presenter_stats_to_c(snapshot: PresenterRuntimeSnapshot) -> ErikaPresenterStats {
    let stats = snapshot.stats;
    let renderer = snapshot.renderer;
    ErikaPresenterStats {
        decoded_video_frames: stats.decoded_video_frames,
        rendered_video_frames: stats.rendered_video_frames,
        rendered_test_frames: stats.rendered_test_frames,
        pushed_audio_frames: stats.pushed_audio_frames,
        overlay_frames: stats.overlay_frames,
        danmaku_frames: stats.danmaku_frames,
        danmaku_items: stats.danmaku_items,
        import_failures: stats.import_failures,
        render_failures: stats.render_failures,
        audio_failures: stats.audio_failures,
        software_video_frames: renderer.software_video_frames,
        hardware_video_frames: renderer.hardware_video_frames,
        zero_copy_video_frames: renderer.zero_copy_video_frames,
        cpu_video_frame_fallbacks: renderer.cpu_video_frame_fallbacks,
        last_render_micros: duration_micros_u64(snapshot.last_render_duration),
        last_render_current_micros: duration_micros_u64(snapshot.last_render_current_duration),
        audio_clock_read_frames: snapshot.audio_output_read_frames,
        audio_clock_queued_frames: snapshot.audio_output_queued_frames.min(u64::MAX as usize)
            as u64,
        audio_clock_underflow_frames: snapshot.audio_output_underflow_frames,
        audio_recovery_state: audio_recovery_state_to_c(
            snapshot.audio_output_runtime_stats.recovery_state,
        ),
        audio_last_error_code: snapshot.audio_output_runtime_stats.last_error_code,
        audio_recovery_attempts: snapshot.audio_output_runtime_stats.recovery_attempts,
        audio_recovery_count: snapshot.audio_output_runtime_stats.recovery_count,
        audio_recovery_failures: snapshot.audio_output_runtime_stats.recovery_failures,
        direct_zero_copy_video_frames: renderer.direct_zero_copy_video_frames,
        shared_handle_video_frames: renderer.shared_handle_video_frames,
        hdr_source_frames: renderer.hdr_source_frames,
        hdr10_output_frames: renderer.hdr10_output_frames,
        sdr_tonemap_frames: renderer.sdr_tonemap_frames,
        hdr10_metadata_updates: renderer.hdr10_metadata_updates,
        hdr10_metadata_failures: renderer.hdr10_metadata_failures,
        hdr10_output_failures: renderer.hdr10_output_failures,
        hdr10_output_active: renderer.hdr10_output_active,
        video_frame_backpressure_drops: stats.video_frame_backpressure_drops,
    }
}

#[cfg(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
))]
fn audio_recovery_state_to_c(state: AudioRecoveryState) -> i32 {
    match state {
        AudioRecoveryState::Stable => 0,
        AudioRecoveryState::Disconnected => 1,
        AudioRecoveryState::Recovering => 2,
        AudioRecoveryState::Failed => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_event_counts_tracks() {
        let event = event_to_c(PlayerEvent::TracksChanged(vec![
            erika::TrackInfo::embedded(0, TrackKind::Video),
            erika::TrackInfo::embedded(1, TrackKind::Audio),
        ]));

        assert_eq!(event.kind, ErikaEventKind::TracksChanged);
        assert_eq!(event.tracks.video, 1);
        assert_eq!(event.tracks.audio, 1);
        assert_eq!(event.tracks.subtitle, 0);
    }

    #[test]
    fn c_event_reports_track_selection_changed() {
        let event = event_to_c(PlayerEvent::TrackSelectionChanged(erika::TrackSelection {
            video: Some(0),
            audio: Some(1),
            subtitle: None,
        }));

        assert_eq!(event.kind, ErikaEventKind::TrackSelectionChanged);
    }

    #[test]
    fn c_event_exposes_video_decoder_transition_message() {
        let event = event_to_c(PlayerEvent::VideoDecoderChanged(erika::VideoDecoderEvent {
            stage: "renderer_import".to_string(),
            requested_backend: erika::ffmpeg::DecoderBackend::MediaCodec,
            previous_backend: Some(erika::ffmpeg::DecoderBackend::MediaCodec),
            active_backend: erika::ffmpeg::DecoderBackend::Software,
            fallback_count: 1,
            codec: Some("h264".to_string()),
            pixel_format: Some("nv12".to_string()),
            line_sizes: Some([1920, 1920, 0, 0]),
            reason: Some("test import failure".to_string()),
        }));

        assert_eq!(event.kind, ErikaEventKind::VideoDecoderChanged);
        let raw_message = erika_last_error_message();
        assert!(!raw_message.is_null());
        let message = unsafe { CStr::from_ptr(raw_message) }
            .to_string_lossy()
            .into_owned();
        assert!(message.contains("renderer_import"));
        assert!(message.contains("software"));
        unsafe { erika_string_free(raw_message) };
    }

    #[test]
    fn c_event_exposes_audio_recovery_transition_message() {
        assert_eq!(ErikaEventKind::AudioOutputChanged as i32, 12);
        let event = event_to_c(PlayerEvent::AudioOutputChanged(erika::AudioOutputEvent {
            stats: erika::audio::AudioOutputRuntimeStats {
                recovery_state: erika::audio::AudioRecoveryState::Stable,
                last_error_code: -899,
                recovery_attempts: 1,
                recovery_count: 1,
                recovery_failures: 0,
                transition_sequence: 3,
            },
        }));

        assert_eq!(event.kind, ErikaEventKind::AudioOutputChanged);
        let raw_message = erika_last_error_message();
        assert!(!raw_message.is_null());
        let message = unsafe { CStr::from_ptr(raw_message) }
            .to_string_lossy()
            .into_owned();
        assert!(message.contains("audio_output_changed"));
        assert!(message.contains("-899"));
        assert!(message.contains("stable"));
        unsafe { erika_string_free(raw_message) };
    }

    #[test]
    fn c_track_info_maps_source_selection_and_strings() {
        let mut track = erika::TrackInfo::external(1_000_001, TrackKind::Subtitle);
        track.selected = true;
        track.title = Some("Signs".to_string());
        track.language = Some("jpn".to_string());
        track.codec = Some("ass".to_string());
        track.bit_rate = Some(8_000_000);
        track.frame_rate = erika::FrameRate::new(30_000, 1_001);

        let mut c_track = track_info_to_c(&track);

        assert_eq!(c_track.id, 1_000_001);
        assert_eq!(c_track.kind, ErikaTrackKind::Subtitle);
        assert_eq!(c_track.source, ErikaTrackSource::External);
        assert!(c_track.selected);
        assert!(c_track.can_remove);
        assert_eq!(
            unsafe { CStr::from_ptr(c_track.title).to_str().unwrap() },
            "Signs"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(c_track.language).to_str().unwrap() },
            "jpn"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(c_track.codec).to_str().unwrap() },
            "ass"
        );
        assert_eq!(c_track.bit_rate, 8_000_000);
        assert_eq!(c_track.frame_rate_numerator, 30_000);
        assert_eq!(c_track.frame_rate_denominator, 1_001);

        unsafe { erika_track_info_free(&mut c_track) };
        assert!(c_track.title.is_null());
        assert!(c_track.language.is_null());
        assert!(c_track.codec.is_null());
    }

    #[test]
    fn c_track_selection_uses_negative_one_for_disabled_tracks() {
        let selection = track_selection_to_c(erika::TrackSelection {
            video: Some(0),
            audio: None,
            subtitle: Some(2),
        });

        assert_eq!(selection.video, 0);
        assert_eq!(selection.audio, -1);
        assert_eq!(selection.subtitle, 2);
    }

    #[test]
    fn null_handle_is_rejected() {
        assert_eq!(
            unsafe { erika_play(std::ptr::null_mut()) },
            ErikaStatus::NullPointer
        );
    }

    #[test]
    fn c_external_subtitle_api_rejects_null_pointers() {
        let subtitle_uri = std::ffi::CString::new("/tmp/subs.srt").unwrap();
        let handle = erika_create();
        assert!(!handle.is_null());

        let status = unsafe {
            erika_add_external_subtitle(handle, subtitle_uri.as_ptr(), std::ptr::null_mut())
        };
        assert_eq!(status, ErikaStatus::NullPointer);

        let mut track_id = 0;
        let status = unsafe {
            erika_add_external_subtitle(std::ptr::null_mut(), subtitle_uri.as_ptr(), &mut track_id)
        };
        assert_eq!(status, ErikaStatus::NullPointer);

        let status = unsafe { erika_remove_subtitle_track(std::ptr::null_mut(), 1_000_001) };
        assert_eq!(status, ErikaStatus::NullPointer);

        unsafe { erika_destroy(handle) };
    }

    #[test]
    fn c_http_headers_validate_parameters_and_preserve_order() {
        assert_eq!(c_http_headers(std::ptr::null(), 0), Ok(Vec::new()));
        assert_eq!(
            c_http_headers(std::ptr::null(), 1),
            Err(ErikaStatus::NullPointer)
        );

        let first_name = CString::new("Accept").unwrap();
        let first_value = CString::new("video/mp4").unwrap();
        let second_name = CString::new("X-Test").unwrap();
        let second_value = CString::new("two").unwrap();
        let headers = [
            ErikaHttpHeader {
                name: first_name.as_ptr(),
                value: first_value.as_ptr(),
            },
            ErikaHttpHeader {
                name: second_name.as_ptr(),
                value: second_value.as_ptr(),
            },
        ];
        assert_eq!(
            c_http_headers(headers.as_ptr(), headers.len()).unwrap(),
            vec![
                ("Accept".to_string(), "video/mp4".to_string()),
                ("X-Test".to_string(), "two".to_string()),
            ]
        );

        let null_name = [ErikaHttpHeader {
            name: std::ptr::null(),
            value: first_value.as_ptr(),
        }];
        assert_eq!(
            c_http_headers(null_name.as_ptr(), 1),
            Err(ErikaStatus::NullPointer)
        );

        let null_value = [ErikaHttpHeader {
            name: first_name.as_ptr(),
            value: std::ptr::null(),
        }];
        assert_eq!(
            c_http_headers(null_value.as_ptr(), 1),
            Err(ErikaStatus::NullPointer)
        );

        let invalid_utf8 = [0xff_u8, 0];
        let invalid_name = [ErikaHttpHeader {
            name: invalid_utf8.as_ptr().cast(),
            value: first_value.as_ptr(),
        }];
        assert_eq!(
            c_http_headers(invalid_name.as_ptr(), 1),
            Err(ErikaStatus::InvalidUtf8)
        );

        let empty_name = CString::new("").unwrap();
        let empty_header = [ErikaHttpHeader {
            name: empty_name.as_ptr(),
            value: first_value.as_ptr(),
        }];
        assert_eq!(
            c_http_headers(empty_header.as_ptr(), 1),
            Err(ErikaStatus::PlayerError)
        );

        let empty_value = CString::new("").unwrap();
        let empty_value_header = [ErikaHttpHeader {
            name: first_name.as_ptr(),
            value: empty_value.as_ptr(),
        }];
        assert_eq!(
            c_http_headers(empty_value_header.as_ptr(), 1),
            Ok(vec![("Accept".to_string(), String::new())])
        );
    }

    #[test]
    fn c_http_headers_reject_reserved_and_malformed_headers() {
        let value = CString::new("value").unwrap();
        for name in [
            "Range",
            "range",
            "HOST",
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
        ] {
            let name = CString::new(name).unwrap();
            let header = [ErikaHttpHeader {
                name: name.as_ptr(),
                value: value.as_ptr(),
            }];
            assert_eq!(
                c_http_headers(header.as_ptr(), 1),
                Err(ErikaStatus::PlayerError),
                "reserved header {name:?} must be rejected"
            );
            assert!(
                LAST_ERROR
                    .with(|slot| slot.borrow().clone())
                    .unwrap_or_default()
                    .contains("managed by Erika")
            );
        }

        for name in ["X Test", "X-Test:", "X\u{00e9}-Test", " Accept"] {
            let name = CString::new(name).unwrap();
            let header = [ErikaHttpHeader {
                name: name.as_ptr(),
                value: value.as_ptr(),
            }];
            assert_eq!(
                c_http_headers(header.as_ptr(), 1),
                Err(ErikaStatus::PlayerError),
                "malformed header name {name:?} must be rejected"
            );
        }

        let name = CString::new("X-Test").unwrap();
        for raw_value in ["line\rbreak", "line\nbreak", "bell\u{0007}"] {
            let raw_value = CString::new(raw_value).unwrap();
            let header = [ErikaHttpHeader {
                name: name.as_ptr(),
                value: raw_value.as_ptr(),
            }];
            assert_eq!(
                c_http_headers(header.as_ptr(), 1),
                Err(ErikaStatus::PlayerError),
                "malformed header value {raw_value:?} must be rejected"
            );
        }

        let allowed_value = CString::new("Bearer a+b/c== \tpadded").unwrap();
        let header = [ErikaHttpHeader {
            name: name.as_ptr(),
            value: allowed_value.as_ptr(),
        }];
        assert_eq!(
            c_http_headers(header.as_ptr(), 1),
            Ok(vec![(
                "X-Test".to_string(),
                "Bearer a+b/c== \tpadded".to_string()
            )])
        );
    }

    #[test]
    fn c_open_options_accepts_null_and_defaults() {
        assert_eq!(
            c_open_options(std::ptr::null()),
            Ok((Vec::new(), None, None))
        );
    }

    #[test]
    fn c_open_options_parses_headers_and_read_ahead() {
        let name = CString::new("Accept").unwrap();
        let value = CString::new("video/mp4").unwrap();
        let headers = [ErikaHttpHeader {
            name: name.as_ptr(),
            value: value.as_ptr(),
        }];
        let options = ErikaOpenOptions {
            headers: headers.as_ptr(),
            header_count: headers.len(),
            http_read_ahead_bytes: 16 * 1024 * 1024,
            http_back_buffer_bytes: 0,
            reserved: [0; 2],
        };
        assert_eq!(
            c_open_options(&options),
            Ok((
                vec![("Accept".to_string(), "video/mp4".to_string())],
                Some(16 * 1024 * 1024),
                None
            ))
        );

        let default_read_ahead = ErikaOpenOptions {
            headers: std::ptr::null(),
            header_count: 0,
            http_read_ahead_bytes: 0,
            http_back_buffer_bytes: 0,
            reserved: [0; 2],
        };
        assert_eq!(
            c_open_options(&default_read_ahead),
            Ok((Vec::new(), None, None))
        );
    }

    #[test]
    fn c_open_options_rejects_nonzero_reserved() {
        let options = ErikaOpenOptions {
            headers: std::ptr::null(),
            header_count: 0,
            http_read_ahead_bytes: 0,
            http_back_buffer_bytes: 0,
            reserved: [1, 0],
        };
        assert_eq!(c_open_options(&options), Err(ErikaStatus::PlayerError));
        assert!(
            LAST_ERROR
                .with(|slot| slot.borrow().clone())
                .unwrap_or_default()
                .contains("reserved")
        );
    }

    #[test]
    fn gif_export_c_api_validates_boundary_before_opening_input() {
        let mut result = ErikaGifExportResult::default();
        assert_eq!(
            unsafe { erika_export_gif(std::ptr::null(), &mut result) },
            ErikaStatus::NullPointer
        );

        let input = CString::new("/tmp/erika-gif-missing-input.mp4").unwrap();
        let output = CString::new("/tmp/erika-gif-output.gif").unwrap();
        let options = ErikaGifExportOptions {
            input_uri: input.as_ptr(),
            output_path: output.as_ptr(),
            start_millis: 0,
            end_millis: 0,
            frames_per_second: 10,
            output_width: 640,
            output_height: 360,
            quality: 0,
            loop_count: 0,
            overwrite: false,
            headers: std::ptr::null(),
            header_count: 0,
            http_read_ahead_bytes: 0,
            reserved: [0; 3],
        };
        assert_eq!(
            unsafe { erika_export_gif(&options, &mut result) },
            ErikaStatus::PlayerError
        );
        assert!(
            LAST_ERROR
                .with(|slot| slot.borrow().clone())
                .unwrap_or_default()
                .contains("end must be greater than start")
        );

        let unsupported_quality = ErikaGifExportOptions {
            end_millis: 1_000,
            quality: 2,
            ..options
        };
        assert_eq!(
            unsafe { erika_export_gif(&unsupported_quality, &mut result) },
            ErikaStatus::PlayerError
        );
        assert!(
            LAST_ERROR
                .with(|slot| slot.borrow().clone())
                .unwrap_or_default()
                .contains("unsupported GIF export quality")
        );

        let reserved = ErikaGifExportOptions {
            end_millis: 1_000,
            reserved: [1, 0, 0],
            ..options
        };
        assert_eq!(
            unsafe { erika_export_gif(&reserved, &mut result) },
            ErikaStatus::PlayerError
        );
        assert!(
            LAST_ERROR
                .with(|slot| slot.borrow().clone())
                .unwrap_or_default()
                .contains("reserved")
        );
    }

    #[test]
    fn frame_export_c_api_returns_owned_pixels_and_validates_its_boundary() {
        let mut result = ErikaFrameExportResult::default();
        assert_eq!(
            unsafe { erika_export_frame(std::ptr::null(), &mut result) },
            ErikaStatus::NullPointer
        );
        let fixture = CString::new(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../erika/testdata/playback/playback-fixture.mkv")
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap();
        let options = ErikaFrameExportOptions {
            input_uri: fixture.as_ptr(),
            headers: std::ptr::null(),
            header_count: 0,
            position_millis: 1_000,
            http_read_ahead_bytes: 0,
            max_width: 0,
            max_height: 45,
            format: 0,
            reserved_flags: 0,
            reserved: [0; 4],
        };
        assert_eq!(
            unsafe { erika_export_frame(&options, &mut result) },
            ErikaStatus::Ok
        );
        assert_eq!((result.width, result.height), (80, 45));
        assert_eq!((result.source_width, result.source_height), (160, 90));
        assert_eq!(result.data_len, 80 * 45 * 4);
        assert!(!result.data.is_null());
        assert!(result.timestamp_micros <= 1_000_000);
        assert_eq!(result.duration_micros, 8_000_000);
        unsafe { erika_frame_export_result_free(&mut result) };
        assert!(result.data.is_null());
        assert_eq!(result.data_len, 0);
        // A cleared result may be released again.
        unsafe { erika_frame_export_result_free(&mut result) };

        let png = ErikaFrameExportOptions {
            format: 1,
            ..options
        };
        assert_eq!(
            unsafe { erika_export_frame(&png, &mut result) },
            ErikaStatus::Ok
        );
        let bytes = unsafe { std::slice::from_raw_parts(result.data, result.data_len) };
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        unsafe { erika_frame_export_result_free(&mut result) };

        for (options, message) in [
            (
                ErikaFrameExportOptions {
                    format: 2,
                    ..options
                },
                "unsupported frame export format",
            ),
            (
                ErikaFrameExportOptions {
                    reserved: [0, 1, 0, 0],
                    ..options
                },
                "reserved",
            ),
            (
                ErikaFrameExportOptions {
                    position_millis: 9_000,
                    ..options
                },
                "not before the end",
            ),
        ] {
            assert_eq!(
                unsafe { erika_export_frame(&options, &mut result) },
                ErikaStatus::PlayerError
            );
            assert!(result.data.is_null());
            let error = LAST_ERROR
                .with(|slot| slot.borrow().clone())
                .unwrap_or_default();
            assert!(error.contains(message), "{error}");
        }
    }

    #[test]
    fn frame_export_session_c_api_takes_many_pictures_from_one_open_media() {
        let fixture = CString::new(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../erika/testdata/playback/playback-fixture.mkv")
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap();
        let mut session: *mut ErikaFrameExportSession = std::ptr::null_mut();
        assert_eq!(
            unsafe { erika_frame_export_session_open(std::ptr::null(), &mut session) },
            ErikaStatus::NullPointer
        );
        let options = ErikaFrameExportSessionOptions {
            input_uri: fixture.as_ptr(),
            headers: std::ptr::null(),
            header_count: 0,
            http_read_ahead_bytes: 0,
            reserved_flags: 0,
            reserved: [0; 4],
        };
        let missing = CString::new("/nonexistent/erika-missing.mkv").unwrap();
        assert_eq!(
            unsafe {
                erika_frame_export_session_open(
                    &ErikaFrameExportSessionOptions {
                        input_uri: missing.as_ptr(),
                        ..options
                    },
                    &mut session,
                )
            },
            ErikaStatus::PlayerError
        );
        assert!(session.is_null());
        assert_eq!(
            unsafe {
                erika_frame_export_session_open(
                    &ErikaFrameExportSessionOptions {
                        reserved: [1, 0, 0, 0],
                        ..options
                    },
                    &mut session,
                )
            },
            ErikaStatus::PlayerError
        );
        assert_eq!(
            unsafe { erika_frame_export_session_open(&options, &mut session) },
            ErikaStatus::Ok
        );
        assert!(!session.is_null());

        let request = ErikaFrameExportRequest {
            position_millis: 100,
            max_width: 0,
            max_height: 45,
            format: 0,
            reserved_flags: 0,
            reserved: [0; 4],
        };
        let mut result = ErikaFrameExportResult::default();
        let mut previous = Vec::new();
        for millis in [100, 200, 300, 2_500] {
            assert_eq!(
                unsafe {
                    erika_frame_export_session_frame(
                        session,
                        &ErikaFrameExportRequest {
                            position_millis: millis,
                            ..request
                        },
                        &mut result,
                    )
                },
                ErikaStatus::Ok,
                "{millis} ms"
            );
            assert_eq!((result.width, result.height), (80, 45));
            assert_eq!(result.duration_micros, 8_000_000);
            assert_eq!(result.reserved_flags, 0);
            let pixels =
                unsafe { std::slice::from_raw_parts(result.data, result.data_len) }.to_vec();
            assert_eq!(pixels.len(), 80 * 45 * 4);
            unsafe { erika_frame_export_result_free(&mut result) };
            previous.push(pixels);
        }
        assert_ne!(previous[0], previous[3], "pictures differ between times");

        // A failed request leaves the session usable.
        assert_eq!(
            unsafe {
                erika_frame_export_session_frame(
                    session,
                    &ErikaFrameExportRequest {
                        position_millis: 9_000,
                        ..request
                    },
                    &mut result,
                )
            },
            ErikaStatus::PlayerError
        );
        assert!(result.data.is_null());
        for (bad, message) in [
            (
                ErikaFrameExportRequest {
                    format: 2,
                    ..request
                },
                "unsupported frame export format",
            ),
            (
                ErikaFrameExportRequest {
                    reserved: [0, 0, 1, 0],
                    ..request
                },
                "reserved",
            ),
        ] {
            assert_eq!(
                unsafe { erika_frame_export_session_frame(session, &bad, &mut result) },
                ErikaStatus::PlayerError
            );
            let error = LAST_ERROR
                .with(|slot| slot.borrow().clone())
                .unwrap_or_default();
            assert!(error.contains(message), "{error}");
        }
        let png = ErikaFrameExportRequest {
            position_millis: 100,
            format: 1,
            ..request
        };
        assert_eq!(
            unsafe { erika_frame_export_session_frame(session, &png, &mut result) },
            ErikaStatus::Ok
        );
        let bytes = unsafe { std::slice::from_raw_parts(result.data, result.data_len) };
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        unsafe { erika_frame_export_result_free(&mut result) };
        assert_eq!(
            unsafe { erika_frame_export_session_frame(std::ptr::null_mut(), &png, &mut result) },
            ErikaStatus::NullPointer
        );
        unsafe { erika_frame_export_session_close(session) };
        unsafe { erika_frame_export_session_close(std::ptr::null_mut()) };
    }

    #[test]
    fn open_options_raw_matches_legacy_pair() {
        let empty = open_options_raw(std::ptr::null(), 0);
        assert_eq!(empty.header_count, 0);

        let name = CString::new("Accept").unwrap();
        let value = CString::new("video/mp4").unwrap();
        let headers = [ErikaHttpHeader {
            name: name.as_ptr(),
            value: value.as_ptr(),
        }];
        let options = open_options_raw(headers.as_ptr(), headers.len());
        assert_eq!(options.headers, headers.as_ptr());
        assert_eq!(options.header_count, headers.len());
        assert_eq!(options.http_read_ahead_bytes, 0);
        assert_eq!(options.http_back_buffer_bytes, 0);
        assert_eq!(options.reserved, [0; 2]);
        assert_eq!(
            c_open_options(&options),
            Ok((
                vec![("Accept".to_string(), "video/mp4".to_string())],
                None,
                None
            ))
        );
    }

    #[test]
    fn erika_open_with_options_rejects_null_pointer_arguments() {
        let handle = erika_create();
        assert!(!handle.is_null());
        let uri = CString::new("/tmp/erika-missing.mp4").unwrap();

        assert_eq!(
            unsafe {
                erika_open_with_options(std::ptr::null_mut(), uri.as_ptr(), std::ptr::null())
            },
            ErikaStatus::NullPointer
        );
        assert_eq!(
            unsafe { erika_open_with_options(handle, std::ptr::null(), std::ptr::null()) },
            ErikaStatus::NullPointer
        );

        unsafe { erika_destroy(handle) };
    }

    #[test]
    fn c_surface_attach_emits_events() {
        let handle = erika_create();
        assert!(!handle.is_null());

        let status = unsafe { erika_attach_metal_layer(handle, 42, 1920, 1080, 2.0) };
        assert_eq!(status, ErikaStatus::Ok);

        let mut event = ErikaEvent::default();
        let status = unsafe { erika_poll_event(handle, &mut event) };
        assert_eq!(status, ErikaStatus::Ok);
        assert_eq!(event.kind, ErikaEventKind::SurfaceAttached);

        let status = unsafe { erika_detach_surface(handle) };
        assert_eq!(status, ErikaStatus::Ok);
        let status = unsafe { erika_poll_event(handle, &mut event) };
        assert_eq!(status, ErikaStatus::Ok);
        assert_eq!(event.kind, ErikaEventKind::SurfaceDetached);

        unsafe { erika_destroy(handle) };
    }

    #[test]
    fn c_wgpu_surface_attach_emits_event() {
        let handle = erika_create();
        assert!(!handle.is_null());

        let status = unsafe {
            erika_attach_wgpu_surface(
                handle,
                ErikaWgpuSurfaceKind::MacOsCaMetalLayer,
                42,
                0,
                1920,
                1080,
                2.0,
            )
        };
        assert_eq!(status, ErikaStatus::Ok);

        let mut event = ErikaEvent::default();
        let status = unsafe { erika_poll_event(handle, &mut event) };
        assert_eq!(status, ErikaStatus::Ok);
        assert_eq!(event.kind, ErikaEventKind::SurfaceAttached);

        unsafe { erika_destroy(handle) };
    }

    #[test]
    fn c_wgpu_surface_keeps_physical_extent_separate_from_scale() {
        let handle = wgpu_surface_handle_from_c(
            ErikaWgpuSurfaceKind::AndroidNativeWindow,
            42,
            0,
            1081,
            607,
            2.625,
        );

        assert_eq!(handle.metrics().physical_size(), (1081, 607));
        assert_eq!(handle.metrics().content_scale, 2.625);
    }

    #[test]
    fn c_flutter_texture_attach_emits_event() {
        let handle = erika_create();
        assert!(!handle.is_null());

        let status = unsafe {
            erika_attach_flutter_texture(
                handle,
                ErikaFlutterTextureKind::MacOsTextureRegistrar,
                7,
                1280,
                720,
                2.0,
            )
        };
        assert_eq!(status, ErikaStatus::Ok);

        let mut event = ErikaEvent::default();
        let status = unsafe { erika_poll_event(handle, &mut event) };
        assert_eq!(status, ErikaStatus::Ok);
        assert_eq!(event.kind, ErikaEventKind::SurfaceAttached);

        unsafe { erika_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_lifecycle_rejects_null_and_can_be_destroyed() {
        assert_eq!(
            unsafe { erika_presenter_play(std::ptr::null_mut()) },
            ErikaStatus::NullPointer
        );
        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn presenter_open_discards_events_from_the_previous_content_boundary() {
        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        let first = CString::new("/tmp/erika-capi-missing-first.mp4").unwrap();
        let second = CString::new("/tmp/erika-capi-missing-second.mp4").unwrap();

        assert_eq!(
            unsafe { erika_presenter_open(handle, first.as_ptr()) },
            ErikaStatus::PlayerError
        );
        assert_eq!(
            unsafe { erika_presenter_open(handle, second.as_ptr()) },
            ErikaStatus::PlayerError
        );

        let mut event = ErikaEvent::default();
        let mut delivered = 0usize;
        let mut saw_buffering_reset = false;
        while unsafe { erika_presenter_poll_event(handle, &mut event) } == ErikaStatus::Ok {
            delivered += 1;
            saw_buffering_reset |=
                event.kind == ErikaEventKind::BufferingChanged && !event.buffering;
        }
        assert_eq!(
            delivered, 4,
            "only the second failed Open may remain queued"
        );
        assert!(
            saw_buffering_reset,
            "Open must reset the previous content's buffering state",
        );

        assert_eq!(
            unsafe { erika_presenter_open(handle, first.as_ptr()) },
            ErikaStatus::PlayerError
        );
        assert_eq!(unsafe { erika_presenter_close(handle) }, ErikaStatus::Ok);
        let mut closed_events = Vec::new();
        while unsafe { erika_presenter_poll_event(handle, &mut event) } == ErikaStatus::Ok {
            closed_events.push((event.kind, event.state, event.buffering));
        }
        assert_eq!(
            closed_events,
            vec![
                (ErikaEventKind::StateChanged, ErikaState::Closed, false,),
                (ErikaEventKind::BufferingChanged, ErikaState::Idle, false,),
            ],
            "Close must retire failed Open events and reset buffering",
        );

        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn presenter_open_preserves_surface_lifecycle_events() {
        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        let missing = CString::new("/tmp/erika-capi-missing-surface-boundary.mp4").unwrap();

        unsafe { &mut *handle }
            .presenter
            .player()
            .attach_surface(PlatformSurface::Metal(MetalSurfaceHandle::new(
                42, 1920, 1080, 2.0,
            )))
            .unwrap();
        assert_eq!(
            unsafe { erika_presenter_open(handle, missing.as_ptr()) },
            ErikaStatus::PlayerError
        );

        let mut event = ErikaEvent::default();
        assert_eq!(
            unsafe { erika_presenter_poll_event(handle, &mut event) },
            ErikaStatus::Ok
        );
        assert_eq!(event.kind, ErikaEventKind::SurfaceAttached);
        while unsafe { erika_presenter_poll_event(handle, &mut event) } == ErikaStatus::Ok {}

        unsafe { &mut *handle }
            .presenter
            .player()
            .detach_surface()
            .unwrap();
        assert_eq!(
            unsafe { erika_presenter_open(handle, missing.as_ptr()) },
            ErikaStatus::PlayerError
        );
        assert_eq!(
            unsafe { erika_presenter_poll_event(handle, &mut event) },
            ErikaStatus::Ok
        );
        assert_eq!(event.kind, ErikaEventKind::SurfaceDetached);

        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn presenter_capture_distinguishes_absent_frame_from_capture_failure() {
        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        let handle_ref = unsafe { &mut *handle };

        assert_eq!(
            capture_presenter_frame_rgba(handle_ref, 2, 2, false),
            Ok(None)
        );
        let failure = capture_presenter_frame_rgba(handle_ref, 0, 2, false)
            .expect_err("zero-width capture must remain a real error");
        assert!(failure.contains("capture size must be non-zero"));
        assert_eq!(
            capture_presenter_frame_rgba(handle_ref, 2, 2, true),
            Ok(None)
        );
        let failure = capture_presenter_frame_rgba(handle_ref, 0, 2, true)
            .expect_err("zero-width capture with danmaku must remain a real error");
        assert!(failure.contains("capture size must be non-zero"));

        for capture in [
            erika_presenter_capture_frame_rgba,
            erika_presenter_capture_frame_rgba_with_danmaku,
        ] {
            let mut rgba = [0xabu8; 16];
            assert_eq!(
                unsafe { capture(handle, 2, 2, rgba.as_mut_ptr(), 15) },
                ErikaStatus::NullPointer
            );
            assert_eq!(
                unsafe { capture(handle, 0, 2, rgba.as_mut_ptr(), 16) },
                ErikaStatus::NullPointer
            );
            assert_eq!(
                unsafe { capture(handle, 2, 2, std::ptr::null_mut(), 16) },
                ErikaStatus::NullPointer
            );
            assert_eq!(
                unsafe { capture(std::ptr::null_mut(), 2, 2, rgba.as_mut_ptr(), 16) },
                ErikaStatus::NullPointer
            );
            assert_eq!(
                unsafe { capture(handle, u32::MAX, u32::MAX, rgba.as_mut_ptr(), 16) },
                ErikaStatus::PlayerError
            );
            assert_eq!(
                unsafe { capture(handle, 2, 2, rgba.as_mut_ptr(), 16) },
                ErikaStatus::PlayerError
            );
            assert_eq!(
                rgba, [0xab; 16],
                "failed capture must leave the output untouched"
            );
        }

        unsafe { erika_presenter_destroy(handle) };
    }

    #[test]
    #[cfg(all(
        feature = "wgpu",
        any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            all(target_os = "linux", not(target_env = "ohos"))
        )
    ))]
    fn native_texture_c_api_handles_absence_errors_and_frame_ownership() {
        let handle = erika_presenter_create_for_texture_export(ErikaPresenterConfig::default());
        assert!(!handle.is_null());
        let mut frame = std::ptr::dangling_mut::<ErikaTextureFrame>();
        assert_eq!(
            unsafe {
                erika_presenter_export_frame_texture(
                    std::ptr::null_mut(),
                    320,
                    180,
                    1.0,
                    &mut frame,
                )
            },
            ErikaStatus::NullPointer
        );
        assert!(frame.is_null());
        assert_eq!(
            unsafe {
                erika_presenter_export_frame_texture(handle, 320, 180, 1.0, std::ptr::null_mut())
            },
            ErikaStatus::NullPointer
        );
        assert_eq!(
            unsafe { erika_presenter_export_frame_texture(handle, 0, 180, 1.0, &mut frame) },
            ErikaStatus::PlayerError
        );
        assert!(frame.is_null());
        assert_eq!(
            unsafe { erika_presenter_export_frame_texture(handle, 320, 180, 1.0, &mut frame) },
            ErikaStatus::NoEvent
        );
        assert!(frame.is_null());
        assert_eq!(
            unsafe { erika_texture_frame_get_info(frame, std::ptr::null_mut()) },
            ErikaStatus::NullPointer
        );
        unsafe { erika_texture_frame_release(std::ptr::null_mut()) };

        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../erika/testdata/playback/playback-fixture.mkv");
        let uri = CString::new(fixture.to_str().unwrap()).unwrap();
        assert_eq!(
            unsafe { erika_presenter_open(handle, uri.as_ptr()) },
            ErikaStatus::Ok
        );
        assert_eq!(
            unsafe { erika_presenter_set_volume(handle, 0.0) },
            ErikaStatus::Ok
        );
        assert_eq!(unsafe { erika_presenter_play(handle) }, ErikaStatus::Ok);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status =
                unsafe { erika_presenter_export_frame_texture(handle, 320, 180, 1.0, &mut frame) };
            if status == ErikaStatus::Ok {
                break;
            }
            assert_eq!(status, ErikaStatus::NoEvent);
            assert!(
                std::time::Instant::now() < deadline,
                "first native texture was not delivered"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!frame.is_null());
        unsafe { erika_presenter_destroy(handle) };
        let mut info = std::mem::MaybeUninit::<ErikaExternalTextureInfo>::uninit();
        assert_eq!(
            unsafe { erika_texture_frame_get_info(frame, info.as_mut_ptr()) },
            ErikaStatus::Ok
        );
        let info = unsafe { info.assume_init() };
        assert_eq!((info.width, info.height), (320, 180));
        assert_eq!(info.format, ErikaExternalTextureFormat::Rgba8Unorm);
        assert_eq!(
            unsafe { erika_texture_frame_get_info(frame, std::ptr::null_mut()) },
            ErikaStatus::NullPointer
        );
        unsafe { erika_texture_frame_release(frame) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_set_volume_accepts_valid_handle() {
        assert_eq!(
            unsafe { erika_presenter_set_volume(std::ptr::null_mut(), 0.5) },
            ErikaStatus::NullPointer
        );

        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        assert_eq!(
            unsafe { erika_presenter_set_volume(handle, 0.5) },
            ErikaStatus::Ok
        );
        assert_eq!(
            unsafe { erika_presenter_set_volume(handle, f64::NAN) },
            ErikaStatus::Ok
        );
        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_toggles_debug_hud() {
        assert_eq!(
            unsafe { erika_presenter_set_debug_hud_enabled(std::ptr::null_mut(), true) },
            ErikaStatus::NullPointer
        );

        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        assert_eq!(
            unsafe { erika_presenter_set_debug_hud_enabled(handle, true) },
            ErikaStatus::Ok
        );
        assert!(unsafe { &*handle }.presenter.debug_hud_enabled());
        assert_eq!(
            unsafe { erika_presenter_set_debug_hud_enabled(handle, false) },
            ErikaStatus::Ok
        );
        assert!(!unsafe { &*handle }.presenter.debug_hud_enabled());
        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_set_upscaler_accepts_valid_handle() {
        assert_eq!(
            unsafe {
                erika_presenter_set_upscaler(
                    std::ptr::null_mut(),
                    ErikaLumaUpscalerMode::ArtCnnC4F16 as i32,
                )
            },
            ErikaStatus::NullPointer
        );

        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        assert_eq!(
            unsafe {
                erika_presenter_set_upscaler(handle, ErikaLumaUpscalerMode::ArtCnnC4F16 as i32)
            },
            ErikaStatus::Ok
        );
        assert_eq!(
            unsafe { erika_presenter_set_upscaler(handle, ErikaLumaUpscalerMode::Off as i32) },
            ErikaStatus::Ok
        );
        assert_eq!(
            unsafe { erika_presenter_set_upscaler(handle, 999) },
            ErikaStatus::Ok
        );
        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_reports_upscaler_status() {
        assert_eq!(
            unsafe {
                erika_presenter_get_upscaler_status(std::ptr::null_mut(), std::ptr::null_mut())
            },
            ErikaStatus::NullPointer
        );

        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        let mut status = ErikaUpscalerStatus::default();
        assert_eq!(
            unsafe { erika_presenter_get_upscaler_status(handle, &mut status) },
            ErikaStatus::Ok
        );
        assert_eq!(status.requested_mode, ErikaLumaUpscalerMode::Off as i32);
        assert_eq!(
            status.active_backend,
            ErikaUpscalerBackendStatus::Off as i32
        );

        assert_eq!(
            unsafe {
                erika_presenter_set_upscaler(handle, ErikaLumaUpscalerMode::ArtCnnC4F16 as i32)
            },
            ErikaStatus::Ok
        );
        assert_eq!(
            unsafe { erika_presenter_get_upscaler_status(handle, &mut status) },
            ErikaStatus::Ok
        );
        assert_eq!(
            status.requested_mode,
            ErikaLumaUpscalerMode::ArtCnnC4F16 as i32
        );
        let expected_backend = if cfg!(all(
            any(
                target_os = "android",
                all(target_os = "linux", not(target_env = "ohos"))
            ),
            feature = "wgpu"
        )) {
            ErikaUpscalerBackendStatus::Scalar
        } else if cfg!(target_os = "windows") {
            // D3D11 initializes the requested GPU upscaler after attaching a
            // device; before attachment the existing backend reports Building.
            ErikaUpscalerBackendStatus::Building
        } else {
            ErikaUpscalerBackendStatus::Inactive
        };
        if cfg!(all(target_os = "linux", feature = "wgpu"))
            && status.active_backend == ErikaUpscalerBackendStatus::Inactive as i32
        {
            // GLES adapters can render video without the storage buffers and
            // compute limits ArtCNN needs. Capability fallback is observable.
            assert!(status.fallback_count > 0);
        } else {
            assert_eq!(status.active_backend, expected_backend as i32);
            assert_eq!(status.fallback_count, 0);
        }
        assert_eq!(status.upscaled_frames, 0);
        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_reports_resource_status_without_changing_stats_abi() {
        assert_eq!(
            unsafe {
                erika_presenter_get_resource_status(std::ptr::null_mut(), std::ptr::null_mut())
            },
            ErikaStatus::NullPointer
        );

        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        let mut status = ErikaPresenterResourceStatus::default();
        assert_eq!(
            unsafe { erika_presenter_get_resource_status(handle, &mut status) },
            ErikaStatus::Ok
        );
        assert_eq!(status.drawable_estimated_bytes, 0);
        assert_eq!(status.video_frame_bytes, 0);
        assert_eq!(status.renderer_tracked_bytes, 0);
        assert_eq!(status.drawable_count, 0);
        assert_eq!(status.output_mode_switches, 0);
        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_can_be_created_with_edr_config() {
        let handle = erika_presenter_create_with_config(ErikaPresenterConfig {
            output_mode: ErikaPresenterOutputMode::AppleEdr as i32,
            edr_headroom: 4.0,
            ..ErikaPresenterConfig::default()
        });
        assert!(!handle.is_null());
        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_danmaku_api_loads_configures_and_clears() {
        let handle = erika_presenter_create();
        assert!(!handle.is_null());

        let json = CString::new(
            r##"{"comments":[{"time":1.0,"content":"c api danmaku","type":"scroll","color":"#ffffff"}]}"##,
        )
        .unwrap();
        assert_eq!(
            unsafe { erika_presenter_load_danmaku_json(handle, json.as_ptr()) },
            ErikaStatus::Ok
        );
        assert_eq!(
            unsafe { erika_presenter_set_danmaku_enabled(handle, true) },
            ErikaStatus::Ok
        );
        assert_eq!(
            unsafe { erika_presenter_set_danmaku_config(handle, ErikaDanmakuConfig::default()) },
            ErikaStatus::Ok
        );
        assert_eq!(
            unsafe { erika_presenter_clear_danmaku(handle) },
            ErikaStatus::Ok
        );

        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_config_maps_output_modes() {
        assert_eq!(
            metal_output_mode_from_c(ErikaPresenterConfig::default()),
            MetalOutputMode::Sdr
        );
        assert_eq!(
            metal_output_mode_from_c(ErikaPresenterConfig {
                output_mode: ErikaPresenterOutputMode::AppleEdr as i32,
                edr_headroom: 4.0,
                ..ErikaPresenterConfig::default()
            }),
            MetalOutputMode::apple_edr(4.0)
        );
        assert_eq!(
            metal_output_mode_from_c(ErikaPresenterConfig {
                output_mode: ErikaPresenterOutputMode::ExtendedLinear as i32,
                edr_headroom: 3.0,
                ..ErikaPresenterConfig::default()
            }),
            MetalOutputMode::extended_linear(3.0)
        );
        assert_eq!(
            metal_output_mode_from_c(ErikaPresenterConfig {
                output_mode: ErikaPresenterOutputMode::Auto as i32,
                edr_headroom: 2.5,
                ..ErikaPresenterConfig::default()
            }),
            MetalOutputMode::auto(2.5)
        );
        assert_eq!(
            metal_output_mode_from_c(ErikaPresenterConfig {
                output_mode: 999,
                edr_headroom: 4.0,
                ..ErikaPresenterConfig::default()
            }),
            MetalOutputMode::Sdr
        );
        assert_eq!(
            metal_output_mode_from_c(ErikaPresenterConfig {
                output_mode: ErikaPresenterOutputMode::AppleEdr as i32,
                edr_headroom: f32::NAN,
                ..ErikaPresenterConfig::default()
            }),
            MetalOutputMode::apple_edr(1.0)
        );
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_subtitle_style_maps_all_fields_and_normalizes_override_mask() {
        let family = CString::new(" Test Family ").unwrap();
        let file_path = CString::new(" /tmp/subtitle.ttf ").unwrap();
        let style = subtitle_style_from_c(ErikaSubtitleStyle {
            font_family: family.as_ptr(),
            font_file_path: file_path.as_ptr(),
            primary_color_rgba: 0x1122_33ff,
            outline_color_rgba: 0x4455_667f,
            font_size: 52.0,
            outline_width: 4.0,
            bold: true,
            italic: true,
            underline: true,
            strike_out: true,
            spacing: 3.5,
            scale_x_percent: 125.0,
            scale_y_percent: 90.0,
            border_style: 3,
            shadow_depth: 2.0,
            blur: 1.5,
            alignment: 8,
            margin_left: 12,
            margin_right: 13,
            margin_vertical: 14,
            override_mask: u32::MAX,
        })
        .unwrap();

        assert_eq!(style.font_family, "Test Family");
        assert_eq!(style.font_file_path, "/tmp/subtitle.ttf");
        assert_eq!(style.primary_color_rgba, 0x1122_33ff);
        assert_eq!(style.outline_color_rgba, 0x4455_667f);
        assert_eq!(style.font_size, 52.0);
        assert_eq!(style.outline_width, 4.0);
        assert!(style.bold && style.italic && style.underline && style.strike_out);
        assert_eq!(style.spacing, 3.5);
        assert_eq!(style.scale_x_percent, 125.0);
        assert_eq!(style.scale_y_percent, 90.0);
        assert_eq!(style.border_style, 3);
        assert_eq!(style.shadow_depth, 2.0);
        assert_eq!(style.blur, 1.5);
        assert_eq!(style.alignment, 8);
        assert_eq!(style.margin_left, 12);
        assert_eq!(style.margin_right, 13);
        assert_eq!(style.margin_vertical, 14);
        assert_eq!(style.override_mask, erika::subtitle::SUBTITLE_OVERRIDE_ALL);
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_memory_font_round_trip_exposes_status_and_info() {
        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        let font = include_bytes!("../../erika/assets/subfont.ttf");
        let mut font_id = 0;
        assert_eq!(
            unsafe {
                erika_presenter_register_subtitle_memory_font(
                    handle,
                    font.as_ptr(),
                    font.len(),
                    &mut font_id,
                )
            },
            ErikaStatus::Ok
        );
        assert!(font_id > 0);
        assert_eq!(
            unsafe { erika_presenter_select_subtitle_memory_fonts(handle, &font_id, 1) },
            ErikaStatus::Ok
        );

        let mut status = ErikaSubtitleMemoryFontStatus::default();
        assert_eq!(
            unsafe { erika_presenter_get_subtitle_memory_font_status(handle, &mut status) },
            ErikaStatus::Ok
        );
        assert_eq!(status.registered_count, 1);
        assert_eq!(status.registered_bytes, font.len());
        assert_eq!(status.selected_count, 1);
        assert_eq!(unsafe { *status.selected_ids }, font_id);
        unsafe { erika_subtitle_memory_font_status_free(&mut status) };
        assert!(status.selected_ids.is_null());

        let mut info = ErikaSubtitleMemoryFontInfo::default();
        assert_eq!(
            unsafe { erika_presenter_get_subtitle_memory_font_info(handle, font_id, &mut info) },
            ErikaStatus::Ok
        );
        assert_eq!(info.id, font_id);
        assert_eq!(info.byte_len, font.len());
        assert!(info.face_count > 0);
        let face = unsafe { &*info.faces };
        assert!(!face.families_json.is_null());
        assert!(!face.post_script_name.is_null());
        unsafe { erika_subtitle_memory_font_info_free(&mut info) };
        assert!(info.faces.is_null());

        assert_eq!(
            unsafe { erika_presenter_clear_subtitle_memory_fonts(handle) },
            ErikaStatus::Ok
        );
        unsafe { erika_presenter_destroy(handle) };
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_memory_font_rejects_invalid_pointers_and_ids() {
        assert_eq!(
            unsafe {
                erika_presenter_register_subtitle_memory_font(
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                )
            },
            ErikaStatus::NullPointer
        );
        let handle = erika_presenter_create();
        assert!(!handle.is_null());
        assert_eq!(
            unsafe { erika_presenter_select_subtitle_memory_fonts(handle, std::ptr::null(), 1) },
            ErikaStatus::NullPointer
        );
        assert_eq!(
            unsafe { erika_presenter_select_subtitle_memory_fonts(handle, &99, 1) },
            ErikaStatus::PlayerError
        );
        let mut info = ErikaSubtitleMemoryFontInfo::default();
        assert_eq!(
            unsafe { erika_presenter_get_subtitle_memory_font_info(handle, 99, &mut info) },
            ErikaStatus::PlayerError
        );
        unsafe { erika_presenter_destroy(handle) };
    }

    #[test]
    fn c_surface_output_capabilities_preserve_auto_headroom() {
        let capabilities: SurfaceOutputCapabilities = ErikaSurfaceOutputCapabilities {
            extended_linear: true,
            direct_composition: true,
            desired_headroom: 0.0,
            fallback_reason: ErikaOutputFallbackReason::DisplayHdrUnsupported as i32,
        }
        .into();

        assert!(capabilities.extended_linear);
        assert!(capabilities.direct_composition);
        assert_eq!(capabilities.desired_headroom, 0.0);
        assert_eq!(
            capabilities.fallback_reason,
            OutputFallbackReason::DisplayHdrUnsupported
        );
    }

    #[cfg(any(
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_env = "ohos"
    ))]
    #[test]
    fn c_presenter_config_maps_upscaler_modes() {
        assert_eq!(
            presenter_config_from_c(ErikaPresenterConfig::default())
                .renderer
                .luma_upscaler,
            LumaUpscalerMode::Off
        );
        assert_eq!(
            presenter_config_from_c(ErikaPresenterConfig {
                luma_upscaler: ErikaLumaUpscalerMode::ArtCnnC4F16 as i32,
                ..ErikaPresenterConfig::default()
            })
            .renderer
            .luma_upscaler,
            LumaUpscalerMode::ArtCnnC4F16
        );
        assert_eq!(
            presenter_config_from_c(ErikaPresenterConfig {
                luma_upscaler: ErikaLumaUpscalerMode::ArtCnnC4F16Ds as i32,
                ..ErikaPresenterConfig::default()
            })
            .renderer
            .luma_upscaler,
            LumaUpscalerMode::ArtCnnC4F16Ds
        );
        assert_eq!(
            presenter_config_from_c(ErikaPresenterConfig {
                luma_upscaler: ErikaLumaUpscalerMode::ArtCnnC4F32 as i32,
                ..ErikaPresenterConfig::default()
            })
            .renderer
            .luma_upscaler,
            LumaUpscalerMode::ArtCnnC4F32
        );
        assert_eq!(
            presenter_config_from_c(ErikaPresenterConfig {
                luma_upscaler: 999,
                ..ErikaPresenterConfig::default()
            })
            .renderer
            .luma_upscaler,
            LumaUpscalerMode::Off
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn c_presenter_config_keeps_windows_native_decode_path() {
        let config = presenter_config_from_c(ErikaPresenterConfig::default());
        assert_eq!(
            config.player.renderer,
            erika::RendererBackendPreference::Auto
        );
        assert_eq!(
            config.player.playback.video_decode,
            erika::playback::VideoDecodePreference::D3d11va
        );
    }
}
