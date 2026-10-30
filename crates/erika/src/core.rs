use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded, unbounded};
use thiserror::Error;

use crate::audio::{AudioClockSnapshot, AudioOutputRuntimeStats};
use crate::danmaku::DanmakuRenderPlan;
use crate::ffmpeg::{DecoderBackend, Frame, PcmAudioFrame};
use crate::overlay::OverlayFrame;
use crate::playback::{
    PlaybackClock, PlaybackClockSource, PlaybackDecoderResources, PlaybackRunState,
    PlaybackSessionConfig, VideoPlaybackEngine,
};
use crate::renderer::VideoFramePayload;
use crate::subtitle::{DecodedSubtitleFrame, SubtitleTrackConfig};
use crate::trace;

static NEXT_PLAYER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_EXTERNAL_SUBTITLE_TRACK_ID: AtomicU64 = AtomicU64::new(1);
const EXTERNAL_SUBTITLE_TRACK_ID_BASE: i64 = 1_000_000;
const AUDIO_PREFILL_AFTER_VIDEO_LIMIT: usize = 8;
const AUDIO_PREFILL_PACKET_BUDGET: usize = 6;
const AUDIO_PREFILL_TIME_BUDGET: Duration = Duration::from_millis(5);
// Keep producer prefill aligned with the output queue high-water mark. This
// lets callback-driven backends hold a short rate-change bridge without
// accumulating stale-rate PCM in the presenter channel.
const AUDIO_PREFILL_LOW_WATER: Duration = crate::audio::AUDIO_OUTPUT_QUEUE_HIGH_WATER;
const AUDIO_CLOCK_SNAPSHOT_STALE_AFTER: Duration = Duration::from_millis(500);
const PLAYBACK_STARVATION_GRACE: Duration = Duration::from_millis(500);
const AUDIO_OUTPUT_BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(10);
const POSITION_EVENT_INTERVAL: Duration = Duration::from_millis(100);
const FRAME_OUTPUT_BARRIER_TIMEOUT: Duration = Duration::from_secs(2);
const SUBTITLE_FRAME_HANDOFF_MIN_CAPACITY: usize = 1024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PlayerError {
    #[error("player is closed")]
    Closed,
    #[error("invalid state transition from {from:?} to {to:?}")]
    InvalidStateTransition { from: PlayerState, to: PlayerState },
    #[error("renderer error: {0}")]
    Renderer(String),
    #[error("renderer backpressure: {0}")]
    RendererBackpressure(String),
    #[error("source error: {0}")]
    Source(String),
    #[error("playback error: {0}")]
    Playback(String),
}

pub type Result<T> = std::result::Result<T, PlayerError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerConfig {
    pub name: String,
    pub event_channel_capacity: usize,
    pub playback: PlaybackSessionConfig,
    pub renderer: RendererBackendPreference,
    pub video_frame_queue_capacity: usize,
    pub audio_frame_queue_capacity: usize,
    pub subtitle_frame_queue_capacity: usize,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            name: "Erika".to_string(),
            event_channel_capacity: 1024,
            playback: PlaybackSessionConfig::default(),
            renderer: RendererBackendPreference::default(),
            video_frame_queue_capacity: 3,
            audio_frame_queue_capacity: 192,
            subtitle_frame_queue_capacity: 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererBackendPreference {
    PlatformNative,
    FlutterTexture,
    WgpuFallback,
    Auto,
}

impl Default for RendererBackendPreference {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlayerId(NonZeroU64);

impl PlayerId {
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

impl Default for PlayerId {
    fn default() -> Self {
        let id = NEXT_PLAYER_ID.fetch_add(1, Ordering::Relaxed).max(1);
        Self(NonZeroU64::new(id).expect("player id is non-zero"))
    }
}

fn next_external_subtitle_track_id() -> i64 {
    let offset = NEXT_EXTERNAL_SUBTITLE_TRACK_ID.fetch_add(1, Ordering::Relaxed);
    EXTERNAL_SUBTITLE_TRACK_ID_BASE.saturating_add(offset.min(i64::MAX as u64) as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Idle,
    Opening,
    Ready,
    Playing,
    Paused,
    Stopped,
    Closed,
    Error,
}

/// A single-lock view of the shared playback clock.
///
/// Carries the clock rather than a reading of it, so a consumer evaluates it
/// once at its own tick instead of inheriting the playback worker's polling
/// granularity. See [`Player::playback_snapshot`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackSnapshot {
    pub clock: PlaybackClock,
    pub generation: u64,
    pub state: PlayerState,
}

impl PlaybackSnapshot {
    pub fn media_time(&self) -> Duration {
        self.clock.media_time_at(Instant::now())
    }

    pub fn media_time_at(&self, now: Instant) -> Duration {
        self.clock.media_time_at(now)
    }

    pub fn is_playing(&self) -> bool {
        self.state == PlayerState::Playing
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSourceHint {
    Auto,
    LocalFile,
    Http,
}

#[derive(Clone, PartialEq, Eq)]
pub struct MediaRequest {
    pub uri: String,
    pub source_hint: MediaSourceHint,
    pub http_headers: Vec<(String, String)>,
    /// HTTP read-ahead window in bytes for HTTP(S) playback. `None` honors the
    /// `ERIKA_HTTP_READAHEAD_BYTES` env override, then uses the 2 MiB engine
    /// default (see `HttpRangeSource::DEFAULT_READ_AHEAD_BYTES`).
    pub http_read_ahead_bytes: Option<u64>,
    /// HTTP rewind budget in bytes: how much already-played data the source
    /// retains behind the reader. `None` uses the 16 MiB engine default
    /// (`HTTP_CACHE_RETAIN_BYTES`).
    pub http_back_buffer_bytes: Option<u64>,
    /// Where the media opens: the engine seeks there before its worker starts,
    /// so the first frame presented is the one at this position, not the
    /// media's first. `None` (or zero, or past the end) opens at the start.
    pub start_position: Option<Duration>,
}

/// Hand-written so that credentials carried by custom headers (`Authorization`,
/// session cookies, …) never reach a log line through a derived `Debug`.
impl std::fmt::Debug for MediaRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaRequest")
            .field("uri", &self.uri)
            .field("source_hint", &self.source_hint)
            .field(
                "http_headers",
                &self
                    .http_headers
                    .iter()
                    .map(|(name, _)| (name.as_str(), "REDACTED"))
                    .collect::<Vec<_>>(),
            )
            .field("http_read_ahead_bytes", &self.http_read_ahead_bytes)
            .field("http_back_buffer_bytes", &self.http_back_buffer_bytes)
            .finish()
    }
}

impl MediaRequest {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            source_hint: MediaSourceHint::Auto,
            http_headers: Vec::new(),
            http_read_ahead_bytes: None,
            http_back_buffer_bytes: None,
            start_position: None,
        }
    }

    /// Opens at [position] instead of the start (see [`Self::start_position`]).
    /// Zero is treated as `None`.
    pub fn with_start_position(mut self, position: Option<Duration>) -> Self {
        self.start_position = position.filter(|position| !position.is_zero());
        self
    }

    pub fn with_http_headers(mut self, http_headers: Vec<(String, String)>) -> Self {
        self.http_headers = http_headers;
        self
    }

    /// Overrides the HTTP read-ahead window in bytes. Only meaningful for
    /// HTTP(S) sources; other source kinds ignore it. `0` is treated as `None`.
    pub fn with_http_read_ahead_bytes(self, read_ahead_bytes: u64) -> Self {
        self.map_http_read_ahead_bytes(Some(read_ahead_bytes))
    }

    /// Same as [`Self::with_http_read_ahead_bytes`] but `None` keeps the
    /// default resolution; `Some(0)` is normalized to `None`.
    pub fn map_http_read_ahead_bytes(mut self, read_ahead_bytes: Option<u64>) -> Self {
        self.http_read_ahead_bytes = read_ahead_bytes.filter(|bytes| *bytes > 0);
        self
    }

    /// Overrides the HTTP rewind budget in bytes. Only meaningful for HTTP(S)
    /// sources; other source kinds ignore it. `0` is treated as `None`.
    pub fn with_http_back_buffer_bytes(self, back_buffer_bytes: u64) -> Self {
        self.map_http_back_buffer_bytes(Some(back_buffer_bytes))
    }

    /// Same as [`Self::with_http_back_buffer_bytes`] but `None` keeps the
    /// default budget; `Some(0)` is normalized to `None`.
    pub fn map_http_back_buffer_bytes(mut self, back_buffer_bytes: Option<u64>) -> Self {
        self.http_back_buffer_bytes = back_buffer_bytes.filter(|bytes| *bytes > 0);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSource {
    Embedded,
    External,
}

impl Default for TrackSource {
    fn default() -> Self {
        Self::Embedded
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRate {
    pub numerator: u32,
    pub denominator: u32,
}

impl FrameRate {
    pub fn new(numerator: u32, denominator: u32) -> Option<Self> {
        if numerator == 0 || denominator == 0 {
            return None;
        }
        let divisor = greatest_common_divisor(numerator, denominator);
        Some(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    pub fn frames_per_second(&self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }
}

fn greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    pub id: i64,
    pub kind: TrackKind,
    pub title: Option<String>,
    pub language: Option<String>,
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub pixel_format: Option<String>,
    pub sample_format: Option<String>,
    pub profile: Option<String>,
    pub level: Option<i32>,
    pub bit_rate: Option<u64>,
    pub frame_rate: Option<FrameRate>,
    pub selected: bool,
    pub source: TrackSource,
    pub can_remove: bool,
}

impl TrackInfo {
    pub fn embedded(id: i64, kind: TrackKind) -> Self {
        Self {
            id,
            kind,
            title: None,
            language: None,
            codec: None,
            width: 0,
            height: 0,
            sample_rate: 0,
            channels: 0,
            pixel_format: None,
            sample_format: None,
            profile: None,
            level: None,
            bit_rate: None,
            frame_rate: None,
            selected: false,
            source: TrackSource::Embedded,
            can_remove: false,
        }
    }

    pub fn external(id: i64, kind: TrackKind) -> Self {
        Self {
            source: TrackSource::External,
            can_remove: true,
            ..Self::embedded(id, kind)
        }
    }
}

/// A container chapter on the same zero-based timeline as playback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterInfo {
    pub title: String,
    pub start: Duration,
    pub end: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrackSelection {
    pub video: Option<i64>,
    pub audio: Option<i64>,
    pub subtitle: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPrimaries {
    Unknown,
    Bt709,
    DisplayP3,
    Bt2020,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFunction {
    Unknown,
    Srgb,
    Bt1886,
    Pq,
    Hlg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub primaries: ColorPrimaries,
    pub transfer: TransferFunction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrameImportFailure {
    pub decode_backend: DecoderBackend,
    pub mediacodec_surface: bool,
    pub codec: Option<String>,
    pub pixel_format: Option<String>,
    pub line_sizes: [i32; 4],
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    pub reason: String,
}

impl VideoFrameImportFailure {
    pub fn structured_message(&self) -> String {
        serde_json::json!({
            "event": "video_frame_import_failure",
            "backend": self.decode_backend.as_str(),
            "mediaCodecSurface": self.mediacodec_surface,
            "codec": self.codec.as_deref(),
            "pixelFormat": self.pixel_format.as_deref(),
            "lineSizes": self.line_sizes,
            "width": self.width,
            "height": self.height,
            "generation": self.generation,
            "reason": self.reason.as_str(),
        })
        .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoDecoderEvent {
    pub stage: String,
    pub requested_backend: DecoderBackend,
    pub previous_backend: Option<DecoderBackend>,
    pub active_backend: DecoderBackend,
    pub fallback_count: u64,
    pub codec: Option<String>,
    pub pixel_format: Option<String>,
    pub line_sizes: Option<[i32; 4]>,
    pub reason: Option<String>,
}

impl VideoDecoderEvent {
    pub fn structured_message(&self) -> String {
        serde_json::json!({
            "event": "video_decoder_changed",
            "stage": self.stage.as_str(),
            "requestedBackend": self.requested_backend.as_str(),
            "previousBackend": self.previous_backend.map(DecoderBackend::as_str),
            "activeBackend": self.active_backend.as_str(),
            "fallbackCount": self.fallback_count,
            "codec": self.codec.as_deref(),
            "pixelFormat": self.pixel_format.as_deref(),
            "lineSizes": self.line_sizes,
            "reason": self.reason.as_deref(),
        })
        .to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioOutputEvent {
    pub stats: AudioOutputRuntimeStats,
}

impl AudioOutputEvent {
    pub fn structured_message(&self) -> String {
        serde_json::json!({
            "event": "audio_output_changed",
            "recoveryState": self.stats.recovery_state.as_str(),
            "lastErrorCode": self.stats.last_error_code,
            "recoveryAttempts": self.stats.recovery_attempts,
            "recoveryCount": self.stats.recovery_count,
            "recoveryFailures": self.stats.recovery_failures,
            "transitionSequence": self.stats.transition_sequence,
        })
        .to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    StateChanged(PlayerState),
    DurationChanged(Option<Duration>),
    PositionChanged(Duration),
    TracksChanged(Vec<TrackInfo>),
    TrackSelectionChanged(TrackSelection),
    BufferingChanged(bool),
    VideoParamsChanged(VideoParams),
    VideoDecoderChanged(VideoDecoderEvent),
    AudioOutputChanged(AudioOutputEvent),
    SurfaceAttached(PlatformSurface),
    SurfaceDetached,
    Error(PlayerError),
    /// The media played to its end on its own (not a stop asked for). Sent
    /// right after the `Stopped` state that the end brings.
    Ended,
}

pub struct PlayerVideoFrame {
    pub frame: VideoFramePayload,
    pub decode_backend: DecoderBackend,
    pub pts: Option<Duration>,
    pub media_time: Duration,
    pub late_by: Option<Duration>,
    pub generation: u64,
    /// Measured scene-average luminance (nits) attached by the presenter for
    /// HDR10 software frames without Dolby Vision L1 metadata. `None` for
    /// every other source; renderers fold it into the tone-map pivot.
    pub scene_avg_nits: Option<f32>,
}

impl PlayerVideoFrame {
    fn from_decoded(
        frame: Frame,
        decode_backend: DecoderBackend,
        pts: Option<Duration>,
        media_time: Duration,
        late_by: Option<Duration>,
        generation: u64,
    ) -> crate::ffmpeg::Result<Self> {
        Ok(Self {
            frame: VideoFramePayload::from_decoded(frame)?,
            decode_backend,
            pts,
            media_time,
            late_by,
            generation,
            scene_avg_nits: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerAudioFrame {
    pub frame: PcmAudioFrame,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSubtitleFrame {
    pub frame: DecodedSubtitleFrame,
    pub pts: Option<Duration>,
    pub media_time: Duration,
    pub late_by: Option<Duration>,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct RenderFrameContext<'a> {
    pub media_time: Duration,
    pub generation: u64,
    pub overlay: Option<&'a OverlayFrame>,
    pub danmaku: Option<&'a DanmakuRenderPlan>,
    pub output_width: u32,
    pub output_height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererFrameCapture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

const EXPORT_UNSUPPORTED: &str = "native texture export is unsupported by this renderer; select WgpuFallback on Linux/Vulkan or Apple/Metal";

/// Native resource type carried by an [`ExternalTextureFrame`].
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalTextureKind {
    Metal = 1,
    VulkanOpaqueFd = 2,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalTextureFormat {
    Rgba8Unorm = 1,
}

/// Version-independent native texture metadata. Pixels are SDR sRGB-encoded
/// values in RGBA8Unorm storage, matching `capture_frame_rgba`.
///
/// Handles are borrowed from the frame. Vulkan import consumes an FD, so import
/// a `dup` of `native_handle`, never the original. Vulkan images use dedicated
/// allocations, one mip/layer/sample, and the exact format/usage/tiling below.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalTextureInfo {
    pub kind: ExternalTextureKind,
    pub format: ExternalTextureFormat,
    pub width: u32,
    pub height: u32,
    /// `id<MTLTexture>` pointer or borrowed OPAQUE_FD (zero is a valid FD).
    pub native_handle: u64,
    /// Metal MTLDevice registry ID; zero for Vulkan.
    pub device_id: u64,
    pub allocation_size: u64,
    pub memory_type_index: u32,
    pub vulkan_format: u32,
    pub vulkan_usage: u32,
    pub vulkan_tiling: u32,
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
}

/// Owns an immutable, GPU-complete texture without reading pixels to the CPU.
///
/// Keep the frame alive until all consumer GPU work and imported aliases have
/// been retired. Frames can outlive their presenter and renderer. The consumer
/// must use the same GPU. Vulkan consumers must acquire queue-family ownership
/// from EXTERNAL with old layout GENERAL before reading; do not modify pixels.
/// Metal consumers may instead retain the native MTLTexture independently and
/// then drop this guard; that native reference must cover all consumer GPU work.
pub struct ExternalTextureFrame {
    info: ExternalTextureInfo,
    _resource: Box<dyn Send + Sync>,
}

impl ExternalTextureFrame {
    pub fn info(&self) -> &ExternalTextureInfo {
        &self.info
    }

    #[cfg(all(
        feature = "wgpu",
        any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            all(target_os = "linux", not(target_env = "ohos"))
        )
    ))]
    pub(crate) fn new(info: ExternalTextureInfo, resource: impl Send + Sync + 'static) -> Self {
        Self {
            info,
            _resource: Box::new(resource),
        }
    }
}

impl<'a> RenderFrameContext<'a> {
    pub fn new(media_time: Duration, generation: u64) -> Self {
        Self {
            media_time,
            generation,
            overlay: None,
            danmaku: None,
            output_width: 0,
            output_height: 0,
        }
    }

    pub fn overlay(mut self, overlay: Option<&'a OverlayFrame>) -> Self {
        self.overlay = overlay;
        self
    }

    pub fn danmaku(mut self, danmaku: Option<&'a DanmakuRenderPlan>) -> Self {
        self.danmaku = danmaku;
        self
    }

    pub fn output_size(mut self, width: u32, height: u32) -> Self {
        self.output_width = width;
        self.output_height = height;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlatformSurface {
    Metal(MetalSurfaceHandle),
    Wgpu(WgpuSurfaceHandle),
    FlutterTexture(FlutterTextureHandle),
}

impl PlatformSurface {
    pub fn metrics(self) -> SurfaceMetrics {
        match self {
            Self::Metal(handle) => handle.metrics,
            Self::Wgpu(handle) => handle.metrics(),
            Self::FlutterTexture(handle) => handle.metrics,
        }
    }
}

/// Exact native drawable/swapchain extent. Surface width and height in liberika's
/// public API are physical pixels and are never derived from the content scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfacePhysicalExtent {
    pub width: u32,
    pub height: u32,
}

impl SurfacePhysicalExtent {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
        }
    }
}

/// Shared surface sizing contract. `physical_extent` configures the GPU
/// target; `content_scale` converts Flutter/NipaPlay logical UI units to
/// physical pixels without changing that target extent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceMetrics {
    pub physical_extent: SurfacePhysicalExtent,
    pub content_scale: f64,
}

impl SurfaceMetrics {
    pub fn new(width: u32, height: u32, content_scale: f64) -> Self {
        Self {
            physical_extent: SurfacePhysicalExtent::new(width, height),
            content_scale: normalize_surface_scale(content_scale),
        }
    }

    pub fn resized(self, width: u32, height: u32, content_scale: f64) -> Self {
        Self::new(width, height, content_scale)
    }

    pub fn physical_size(self) -> (u32, u32) {
        (self.physical_extent.width, self.physical_extent.height)
    }
}

fn normalize_surface_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetalSurfaceHandle {
    pub raw_layer: u64,
    pub metrics: SurfaceMetrics,
}

impl MetalSurfaceHandle {
    pub fn new(raw_layer: u64, width: u32, height: u32, scale: f64) -> Self {
        Self {
            raw_layer,
            metrics: SurfaceMetrics::new(width, height, scale),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WgpuSurfaceKind {
    Unknown,
    MacOsNsView,
    MacOsCaMetalLayer,
    IosUiView,
    WindowsHwnd,
    XlibWindow,
    WaylandSurface,
    AndroidNativeWindow,
    OhosNativeWindow,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WgpuSurfaceHandle {
    pub kind: WgpuSurfaceKind,
    pub raw_window: u64,
    pub raw_display: u64,
    pub metrics: SurfaceMetrics,
    pub output_capabilities: SurfaceOutputCapabilities,
}

impl WgpuSurfaceHandle {
    pub fn new(
        kind: WgpuSurfaceKind,
        raw_window: u64,
        raw_display: u64,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Self {
        Self {
            kind,
            raw_window,
            raw_display,
            metrics: SurfaceMetrics::new(width, height, scale),
            output_capabilities: SurfaceOutputCapabilities::default(),
        }
    }

    pub fn metrics(self) -> SurfaceMetrics {
        self.metrics
    }

    pub fn resize(&mut self, metrics: SurfaceMetrics) {
        self.metrics = metrics;
    }

    pub fn with_output_capabilities(
        mut self,
        output_capabilities: SurfaceOutputCapabilities,
    ) -> Self {
        self.output_capabilities = output_capabilities;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceOutputCapabilities {
    /// The display/window host is eligible for an extended-linear signal.
    pub extended_linear: bool,
    /// The native surface bypasses Flutter texture-layer composition.
    ///
    /// Android: a SurfaceView hosted with Hybrid Composition (extended-linear HDR).
    /// Windows D3D11: create an `IDXGISwapChain1` with
    /// `CreateSwapChainForComposition` instead of `CreateSwapChainForHwnd`.
    /// Default `false` leaves the HWND swap-chain path unchanged.
    pub direct_composition: bool,
    /// Requested display headroom ratio relative to SDR reference white.
    pub desired_headroom: f32,
    /// Host-side reason why extended-linear eligibility is unavailable. This
    /// keeps display/API failures queryable after the renderer falls back.
    pub fallback_reason: crate::renderer::output::OutputFallbackReason,
}

impl Default for SurfaceOutputCapabilities {
    fn default() -> Self {
        Self {
            extended_linear: false,
            direct_composition: false,
            desired_headroom: 0.0,
            fallback_reason: crate::renderer::output::OutputFallbackReason::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlutterTextureKind {
    Unknown,
    MacOsTextureRegistrar,
    IosTextureRegistrar,
    AndroidSurfaceTexture,
    WindowsTextureRegistrar,
    LinuxTextureRegistrar,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlutterTextureHandle {
    pub kind: FlutterTextureKind,
    pub texture_id: i64,
    pub metrics: SurfaceMetrics,
    pub hdr_capable: bool,
}

impl FlutterTextureHandle {
    pub fn new(
        kind: FlutterTextureKind,
        texture_id: i64,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Self {
        Self {
            kind,
            texture_id,
            metrics: SurfaceMetrics::new(width, height, scale),
            hdr_capable: false,
        }
    }
}

pub trait RendererBackend {
    fn attach_surface(&mut self, surface: PlatformSurface) -> Result<()>;
    fn detach_surface(&mut self) -> Result<()>;
    fn resize_surface(&mut self, metrics: SurfaceMetrics) -> Result<()>;
    fn render_test_frame(&mut self, time_seconds: f64) -> Result<()>;

    /// Import/upload a freshly decoded video frame and retain it as the current
    /// frame to display. The backend owns the imported representation (Metal
    /// textures, wgpu textures, ...) so the presenter stays backend-agnostic.
    fn upload_player_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()>;

    /// Drop any retained video frame and clear the attached output surface when
    /// the backend can do so.
    fn clear_current_frame(&mut self) -> Result<()> {
        Ok(())
    }

    /// Release any decoder-owned recovery payload before a decoder transition
    /// while preserving a renderer-owned GPU snapshot when the backend can do
    /// so safely. Backends without a detached snapshot fall back to a full
    /// clear.
    fn preserve_current_frame_for_transition(&mut self) -> Result<()> {
        self.clear_current_frame()
    }

    /// Prepare the retained video frame for an audio/subtitle track switch.
    /// Native renderers keep their existing imported frame, matching the
    /// historical track-switch behavior. Recovery renderers may override this
    /// to release decoder-owned recovery payloads while retaining a detached
    /// renderer-owned snapshot.
    fn preserve_current_frame_for_track_transition(&mut self) -> Result<()> {
        Ok(())
    }

    /// Render the current frame (optionally compositing `overlay`) to the attached
    /// surface. Returns `false` if there is no current frame to draw, letting the
    /// caller fall back to a test frame.
    fn render_current_frame(&mut self, context: RenderFrameContext<'_>) -> Result<bool>;

    /// Selects the native GPU texture that receives the next Flutter texture
    /// frame. Apple embedders pass an `id<MTLTexture>` pointer as `raw_texture`.
    /// Backends without an external Flutter texture path reject the request.
    fn set_flutter_texture_buffer(
        &mut self,
        _raw_texture: u64,
        _width: u32,
        _height: u32,
    ) -> Result<()> {
        Err(PlayerError::Renderer(
            "external Flutter texture buffers are not supported by this renderer".to_string(),
        ))
    }

    /// Render the retained current frame into an offscreen RGBA buffer.
    fn capture_current_frame(
        &mut self,
        _context: RenderFrameContext<'_>,
        _width: u32,
        _height: u32,
    ) -> Result<Option<RendererFrameCapture>> {
        Ok(None)
    }

    /// Checks that `export_current_frame_texture` can produce a texture of this
    /// size, before the presenter does any per-frame work for it.
    fn validate_export_texture_size(&mut self, _width: u32, _height: u32) -> Result<()> {
        Err(PlayerError::Renderer(EXPORT_UNSUPPORTED.into()))
    }

    /// Render video, subtitles and danmaku directly into an immutable native
    /// texture. Unsupported backends return an error, never a CPU fallback.
    fn export_current_frame_texture(
        &mut self,
        _context: RenderFrameContext<'_>,
        _width: u32,
        _height: u32,
    ) -> Result<Option<ExternalTextureFrame>> {
        Err(PlayerError::Renderer(EXPORT_UNSUPPORTED.into()))
    }

    fn runtime_stats(&self) -> RendererRuntimeStats {
        RendererRuntimeStats::default()
    }

    /// Consume the latest composited Linux Flutter pixel buffer, rendered on
    /// the GPU by render_current_frame. The caller owns the returned bytes.
    fn take_flutter_frame(&mut self) -> Option<RendererFrameCapture> {
        None
    }

    fn resource_stats(&self) -> RendererResourceStats {
        RendererResourceStats::default()
    }

    fn output_status(&self) -> crate::renderer::output::OutputRuntimeStatus {
        crate::renderer::output::OutputRuntimeStatus::default()
    }

    /// Whether this renderer can consume MediaCodec Surface/AHardwareBuffer
    /// frames without a CPU readback. Android uses this before opening the
    /// decoder so unsupported Vulkan implementations start in ByteBuffer mode.
    fn supports_mediacodec_surface_frames(&self) -> bool {
        false
    }

    #[cfg(target_env = "ohos")]
    fn ohos_avcodec_surface(&self) -> Option<Arc<crate::ohos::avcodec::OhosAvCodecSurface>> {
        None
    }

    /// Switches the neural luma upscaler at runtime. Backends without an
    /// upscaler implementation ignore the request.
    fn set_luma_upscaler(&mut self, _mode: crate::renderer::pipeline::LumaUpscalerMode) {}

    /// Publishes the display's current HDR/SDR headroom ratio. Android uses
    /// this for queryable runtime status; renderers that do not expose dynamic
    /// display headroom may ignore the update.
    fn set_output_headroom(&mut self, _headroom: f32, _known: bool) {}

    /// Borrowed COM identity of a DirectComposition swap chain. No `AddRef`.
    ///
    /// Compare this pointer across frames. Only call
    /// [`Self::composition_swapchain_iunknown`] when transferring a reference
    /// into `IDCompositionVisual::SetContent`. `None` unless this renderer
    /// currently owns a composition swap chain.
    fn composition_swapchain_ptr(&self) -> Option<*mut std::ffi::c_void> {
        None
    }

    /// AddRef'd `IUnknown` for a DirectComposition `SetContent` swap chain.
    ///
    /// Null-free raw pointer; the caller owns the reference and must `Release`
    /// it (for example by wrapping it in a host `DcompContent`). `None` unless
    /// this renderer currently owns a composition swap chain.
    fn composition_swapchain_iunknown(&self) -> Option<*mut std::ffi::c_void> {
        None
    }

    /// AddRef'd `IUnknown` for the latest completed Windows Flutter SDR frame.
    ///
    /// The caller owns the reference and must `Release` it. This lets the
    /// Windows embedder expose a shareable D3D11 snapshot to Flutter. Published
    /// resources are immutable, even after the renderer produces another frame.
    fn windows_flutter_texture_iunknown(&self) -> Option<*mut std::ffi::c_void> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LumaUpscalerBackendStatus {
    #[default]
    Off,
    Inactive,
    Building,
    Scalar,
    SimdgroupMatrix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RendererRuntimeStats {
    pub surface_width: u32,
    pub surface_height: u32,
    pub rendered_frames: u64,
    pub offscreen_frames: u64,
    pub prepared_overlay_frames: u64,
    pub prepared_overlay_subtitle_planes: u64,
    pub danmaku_passes: u64,
    pub danmaku_draw_items: u64,
    pub overlay_alpha_atlas_uploads: u64,
    pub overlay_alpha_atlas_reuses: u64,
    pub last_danmaku_atlas_duration: Duration,
    pub last_danmaku_vertex_build_duration: Duration,
    pub last_danmaku_vertex_copy_duration: Duration,
    pub last_danmaku_encode_duration: Duration,
    pub last_danmaku_vertex_bytes: usize,
    pub last_danmaku_vertex_count: usize,
    pub upscaler_mode: crate::renderer::pipeline::LumaUpscalerMode,
    pub upscaler_backend: LumaUpscalerBackendStatus,
    pub upscaler_fallbacks: u64,
    pub upscaled_frames: u64,
    pub last_upscaler_encode_duration: Duration,
    pub last_gpu_duration: Duration,
    pub attached: bool,
    pub software_video_frames: u64,
    pub hardware_video_frames: u64,
    pub zero_copy_video_frames: u64,
    pub direct_zero_copy_video_frames: u64,
    pub shared_handle_video_frames: u64,
    pub cpu_video_frame_fallbacks: u64,
    pub hdr_source_frames: u64,
    pub hdr10_output_frames: u64,
    pub sdr_tonemap_frames: u64,
    pub hdr10_metadata_updates: u64,
    pub hdr10_metadata_failures: u64,
    pub hdr10_output_failures: u64,
    pub hdr10_output_active: bool,
}

/// Snapshot of memory visible from the renderer. Individual fields describe
/// resources tracked by liberika; `device_current_allocated_bytes` is Metal's
/// process-wide device counter and can therefore include driver allocations
/// that do not belong to a single presenter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RendererResourceStats {
    pub device_current_allocated_bytes: u64,
    pub device_recommended_working_set_bytes: u64,
    pub drawable_estimated_bytes: u64,
    pub video_frame_bytes: u64,
    pub overlay_atlas_bytes: u64,
    pub danmaku_atlas_bytes: u64,
    pub danmaku_vertex_buffer_bytes: u64,
    pub upscaler_bytes: u64,
    pub renderer_tracked_bytes: u64,
    pub drawable_count: u32,
    pub output_mode_switches: u64,
}

struct PlayerInner {
    state: PlayerState,
    ended: bool,
    media: Option<MediaRequest>,
    duration: Option<Duration>,
    playback_clock: PlaybackClock,
    playback_generation: u64,
    playback_command_sequence: u64,
    pending_play_sequence: Option<u64>,
    audio_output_epoch: u64,
    surface: Option<PlatformSurface>,
    tracks: Vec<TrackInfo>,
    chapters: Vec<ChapterInfo>,
    track_selection: TrackSelection,
    subscribers: Vec<Sender<PlayerEvent>>,
    video_frame_sender: Option<Sender<PlayerVideoFrame>>,
    audio_frame_sender: Option<Sender<PlayerAudioFrame>>,
    subtitle_frame_sender: Option<Sender<PlayerSubtitleFrame>>,
    subtitle_frame_backpressure_drops: u64,
}

struct PlayerLifecycle {
    epoch: u64,
    playback: Option<PlaybackRuntime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioClockContext {
    generation: u64,
    command_sequence: u64,
    output_epoch: u64,
}

impl AudioClockContext {
    fn from_inner(inner: &PlayerInner) -> Self {
        Self {
            generation: inner.playback_generation,
            command_sequence: inner.playback_command_sequence,
            output_epoch: inner.audio_output_epoch,
        }
    }
}

/// An audio measurement captured together with its playback/output identity.
/// Construct with [`Player::capture_audio_clock`] before queuing feedback.
#[derive(Debug, Clone, Copy)]
pub struct AudioClockObservation {
    player_id: PlayerId,
    snapshot: AudioClockSnapshot,
    captured_at: Instant,
    context: AudioClockContext,
}

impl AudioClockObservation {
    pub fn snapshot(&self) -> AudioClockSnapshot {
        self.snapshot
    }

    fn is_current(
        &self,
        inner: &PlayerInner,
        worker_generation: u64,
        worker_command_sequence: u64,
        now: Instant,
    ) -> bool {
        inner.state == PlayerState::Playing
            && self.context == AudioClockContext::from_inner(inner)
            && self.context.generation == worker_generation
            && self.context.command_sequence == worker_command_sequence
            && now
                .checked_duration_since(self.captured_at)
                .is_some_and(|age| age < AUDIO_CLOCK_SNAPSHOT_STALE_AFTER)
    }
}

enum PlaybackCommand {
    Play {
        sequence: u64,
        generation: u64,
    },
    Pause {
        sequence: u64,
        activate_before_pause: bool,
    },
    Seek {
        position: Duration,
        sequence: u64,
        generation: u64,
        resume_after_seek: bool,
    },
    SetPlaybackRate(f64),
    Stop {
        sequence: u64,
        generation: u64,
    },
    AudioClock(AudioClockObservation),
    VideoFrameImportFailed(VideoFrameImportFailure),
    AddExternalSubtitle {
        config: SubtitleTrackConfig,
        reply: Sender<std::result::Result<SubtitleTrackConfig, String>>,
    },
    RemoveSubtitleTrack(i64),
    SelectAudioTrack(Option<i64>),
    SelectSubtitleTrack(Option<i64>),
    SetFrameOutputQuiesced {
        quiesced: bool,
        reply: Sender<()>,
    },
    SetVideoDecodeSuspended {
        suspended: bool,
        reply: Sender<std::result::Result<(), String>>,
    },
    Shutdown,
}

struct PlaybackRuntime {
    commands: Sender<PlaybackCommand>,
    worker: Option<JoinHandle<()>>,
}

impl PlaybackRuntime {
    fn spawn(
        mut engine: VideoPlaybackEngine,
        inner: Arc<Mutex<PlayerInner>>,
        capacity: usize,
        initial_generation: u64,
    ) -> Self {
        let (commands, receiver) = bounded(capacity.max(1));
        let worker = thread::Builder::new()
            .name("erika-playback".to_string())
            .spawn(move || run_playback_worker(&mut engine, inner, receiver, initial_generation))
            .expect("spawn playback worker");
        Self {
            commands,
            worker: Some(worker),
        }
    }

    fn shutdown(&mut self) {
        if self.commands.send(PlaybackCommand::Shutdown).is_err() {
            trace::diagnostic(
                serde_json::json!({
                    "event": "playback_worker_shutdown",
                    "stage": "command_disconnected",
                    "reason": "the playback worker stopped before accepting Shutdown",
                })
                .to_string(),
            );
        }
        if let Some(worker) = self.worker.take() {
            if let Err(payload) = worker.join() {
                let reason = payload
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic payload".to_string());
                trace::diagnostic(
                    serde_json::json!({
                        "event": "playback_worker_shutdown",
                        "stage": "join_panicked",
                        "reason": reason,
                    })
                    .to_string(),
                );
            }
        }
    }
}

impl Drop for PlaybackRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
pub struct Player {
    id: PlayerId,
    config: PlayerConfig,
    decoder_resources: PlaybackDecoderResources,
    inner: Arc<Mutex<PlayerInner>>,
    lifecycle: Arc<Mutex<PlayerLifecycle>>,
    timing_counters: Arc<crate::diagnostics::PlaybackTimingCounters>,
}

impl Player {
    pub fn new(config: PlayerConfig) -> Self {
        Self {
            id: PlayerId::default(),
            config,
            decoder_resources: PlaybackDecoderResources::default(),
            inner: Arc::new(Mutex::new(PlayerInner {
                state: PlayerState::Idle,
                ended: false,
                media: None,
                duration: None,
                playback_clock: PlaybackClock::paused_at(Duration::ZERO),
                playback_generation: 1,
                playback_command_sequence: 0,
                pending_play_sequence: None,
                audio_output_epoch: 1,
                surface: None,
                tracks: Vec::new(),
                chapters: Vec::new(),
                track_selection: TrackSelection::default(),
                subscribers: Vec::new(),
                video_frame_sender: None,
                audio_frame_sender: None,
                subtitle_frame_sender: None,
                subtitle_frame_backpressure_drops: 0,
            })),
            lifecycle: Arc::new(Mutex::new(PlayerLifecycle {
                epoch: 1,
                playback: None,
            })),
            timing_counters: Arc::new(crate::diagnostics::PlaybackTimingCounters::new()),
        }
    }

    /// Scheduler, clock-discipline, buffering and queue counters of the
    /// current media. Lock-free to read from any thread.
    pub fn timing_counters(&self) -> &crate::diagnostics::PlaybackTimingCounters {
        &self.timing_counters
    }

    #[cfg(target_env = "ohos")]
    pub(crate) fn new_with_ohos_avcodec_surface(
        config: PlayerConfig,
        surface: Option<Arc<crate::ohos::avcodec::OhosAvCodecSurface>>,
    ) -> Self {
        let mut player = Self::new(config);
        player.decoder_resources = PlaybackDecoderResources::with_ohos_avcodec_surface(surface);
        player
    }

    pub fn id(&self) -> PlayerId {
        self.id
    }

    pub fn config(&self) -> &PlayerConfig {
        &self.config
    }

    pub fn state(&self) -> PlayerState {
        self.inner.lock().expect("player mutex poisoned").state
    }

    /// Playback stopped because the media ended, as opposed to `stop()`.
    pub fn is_stopped_at_end(&self) -> bool {
        let inner = self.inner.lock().expect("player mutex poisoned");
        inner.state == PlayerState::Stopped && inner.ended
    }

    #[cfg(test)]
    fn is_ended(&self) -> bool {
        self.inner.lock().expect("player mutex poisoned").ended
    }

    pub fn current_media_time(&self) -> Duration {
        self.inner
            .lock()
            .expect("player mutex poisoned")
            .playback_clock
            .media_time_at(Instant::now())
    }

    /// Reads media time, timeline generation and play state under one lock.
    ///
    /// Consumers that mix these three values must not sample them separately:
    /// the playback worker publishes a new clock sample and `Player::pause`
    /// publishes the paused state from different critical sections, so
    /// independent reads can observe a torn triple (for example a paused state
    /// carrying the still-advancing media time of the previous frame).
    pub fn playback_snapshot(&self) -> PlaybackSnapshot {
        let inner = self.inner.lock().expect("player mutex poisoned");
        PlaybackSnapshot {
            clock: inner.playback_clock.clone(),
            generation: inner.playback_generation.max(1),
            state: inner.state,
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        self.inner.lock().expect("player mutex poisoned").duration
    }

    pub fn playback_generation(&self) -> u64 {
        self.inner
            .lock()
            .expect("player mutex poisoned")
            .playback_generation
            .max(1)
    }

    /// State intended by the latest accepted playback command, including a
    /// Play command that has been queued but not yet committed by the worker.
    pub fn playback_intent_state(&self) -> PlayerState {
        let inner = self.inner.lock().expect("player mutex poisoned");
        if matches!(
            inner.state,
            PlayerState::Ready | PlayerState::Paused | PlayerState::Stopped
        ) && inner.pending_play_sequence == Some(inner.playback_command_sequence)
        {
            PlayerState::Playing
        } else {
            inner.state
        }
    }

    pub fn subscribe(&self) -> Receiver<PlayerEvent> {
        let (sender, receiver) = unbounded();
        self.inner
            .lock()
            .expect("player mutex poisoned")
            .subscribers
            .push(sender);
        receiver
    }

    pub fn subscribe_video_frames(&self) -> Receiver<PlayerVideoFrame> {
        let capacity = self.config.video_frame_queue_capacity.max(1);
        let (sender, receiver) = bounded(capacity);
        self.inner
            .lock()
            .expect("player mutex poisoned")
            .video_frame_sender = Some(sender);
        receiver
    }

    pub fn subscribe_audio_frames(&self) -> Receiver<PlayerAudioFrame> {
        let capacity = self.config.audio_frame_queue_capacity.max(1);
        let (sender, receiver) = bounded(capacity);
        self.inner
            .lock()
            .expect("player mutex poisoned")
            .audio_frame_sender = Some(sender);
        receiver
    }

    pub fn subscribe_subtitle_frames(&self) -> Receiver<PlayerSubtitleFrame> {
        // Subtitle frames are event data: dropping one can permanently remove an
        // ASS chunk (style animation, sign, or overlapping line). Keep a large
        // handoff for temporary host stalls, but cap it so a subscriber that stops
        // draining cannot grow memory without bound.
        let capacity = self
            .config
            .subtitle_frame_queue_capacity
            .max(SUBTITLE_FRAME_HANDOFF_MIN_CAPACITY);
        let (sender, receiver) = bounded(capacity);
        let mut inner = self.inner.lock().expect("player mutex poisoned");
        inner.subtitle_frame_sender = Some(sender);
        inner.subtitle_frame_backpressure_drops = 0;
        receiver
    }

    pub fn open(&self, media: MediaRequest) -> Result<()> {
        let epoch = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .expect("player lifecycle mutex poisoned");
            self.ensure_not_closed()?;
            lifecycle.epoch = lifecycle.epoch.saturating_add(1).max(1);
            let epoch = lifecycle.epoch;
            let previous = lifecycle.playback.take();
            // Join the old producer before publishing Opening. The lifecycle
            // lock serializes open/close while the worker is retired, and the
            // worker never needs this lock to finish.
            drop(previous);
            self.transition_to_opening();
            // Content boundaries publish an authoritative buffering reset so
            // consumers never retain the previous media's incremental state.
            self.emit(PlayerEvent::BufferingChanged(false));
            epoch
        };
        let mut engine = match VideoPlaybackEngine::open_with_decoder_resources(
            &media,
            self.config.playback,
            self.decoder_resources.clone(),
        ) {
            Ok(engine) => engine,
            Err(error) => {
                let error = PlayerError::Playback(error.to_string());
                let lifecycle = self
                    .lifecycle
                    .lock()
                    .expect("player lifecycle mutex poisoned");
                if lifecycle.epoch != epoch {
                    return Err(self.superseded_open_error());
                }
                self.transition(PlayerState::Error)?;
                self.emit(PlayerEvent::Error(error.clone()));
                return Err(error);
            }
        };
        let info = engine.info().clone();
        // Seek before the worker starts, so the first frame it presents is
        // the one at the start position.
        let start = media
            .start_position
            .filter(|start| info.duration.is_none_or(|duration| *start < duration));
        if let Some(start) = start {
            if let Err(error) = engine.seek(start) {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "open_start_position",
                        "stage": "seek_failed",
                        "positionMicros": start.as_micros() as u64,
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
            }
        }
        let start = start.unwrap_or(Duration::ZERO);
        let decoder_events = engine.take_video_decoder_events();
        let mut lifecycle = self
            .lifecycle
            .lock()
            .expect("player lifecycle mutex poisoned");
        if lifecycle.epoch != epoch {
            return Err(self.superseded_open_error());
        }
        let generation = {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            if inner.state == PlayerState::Closed {
                return Err(PlayerError::Closed);
            }
            let generation = inner.playback_generation.saturating_add(1).max(1);
            inner.media = Some(media);
            inner.duration = info.duration;
            inner.playback_clock = PlaybackClock::paused_at(start);
            inner.playback_generation = generation;
            inner.playback_command_sequence = 0;
            inner.pending_play_sequence = None;
            inner.ended = false;
            inner.tracks = info.tracks.clone();
            inner.chapters = info.chapters.clone();
            inner.track_selection = info.track_selection();
            generation
        };
        engine.attach_timing_counters(Arc::clone(&self.timing_counters));
        lifecycle.playback = Some(PlaybackRuntime::spawn(
            engine,
            Arc::clone(&self.inner),
            self.config.event_channel_capacity,
            generation,
        ));
        self.emit(PlayerEvent::DurationChanged(info.duration));
        self.emit(PlayerEvent::TracksChanged(info.tracks.clone()));
        self.emit(PlayerEvent::TrackSelectionChanged(info.track_selection()));
        if let Some(params) = info.video_params {
            self.emit(PlayerEvent::VideoParamsChanged(params));
        }
        for event in decoder_events {
            self.emit(PlayerEvent::VideoDecoderChanged(event));
        }
        if !start.is_zero() {
            self.emit(PlayerEvent::PositionChanged(start));
        }
        self.transition(PlayerState::Ready)?;
        Ok(())
    }

    pub fn play(&self) -> Result<()> {
        self.ensure_not_closed()?;
        let commands = self.playback_commands()?;
        let (sequence, generation, previous_pending_play_sequence) = {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            match inner.state {
                PlayerState::Ready | PlayerState::Paused | PlayerState::Stopped => {}
                from => {
                    return Err(PlayerError::InvalidStateTransition {
                        from,
                        to: PlayerState::Playing,
                    });
                }
            }
            inner.playback_command_sequence =
                inner.playback_command_sequence.saturating_add(1).max(1);
            let previous_pending_play_sequence = inner.pending_play_sequence;
            inner.pending_play_sequence = Some(inner.playback_command_sequence);
            (
                inner.playback_command_sequence,
                inner.playback_generation.max(1),
                previous_pending_play_sequence,
            )
        };
        if commands
            .send(PlaybackCommand::Play {
                sequence,
                generation,
            })
            .is_err()
        {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            if inner.playback_command_sequence == sequence {
                inner.pending_play_sequence = previous_pending_play_sequence;
            }
            return Err(PlayerError::Playback(
                "playback worker is not running".to_string(),
            ));
        }
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        self.ensure_not_closed()?;
        let commands = self.playback_commands()?;
        let (sequence, activate_before_pause) = {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            let pending_play_is_current =
                inner.pending_play_sequence == Some(inner.playback_command_sequence);
            if inner.state != PlayerState::Playing && !pending_play_is_current {
                return Err(PlayerError::InvalidStateTransition {
                    from: inner.state,
                    to: PlayerState::Paused,
                });
            }
            inner.playback_command_sequence =
                inner.playback_command_sequence.saturating_add(1).max(1);
            inner.pending_play_sequence = None;
            (inner.playback_command_sequence, pending_play_is_current)
        };
        commands
            .send(PlaybackCommand::Pause {
                sequence,
                activate_before_pause,
            })
            .map_err(|_| PlayerError::Playback("playback worker is not running".to_string()))?;
        // Publish the paused intent immediately so a sequential pause/play pair
        // cannot race the worker and observe a stale Playing state. The worker
        // still replaces the parked clock with its authoritative media time.
        let _ = commit_playback_command_intent(
            &self.inner,
            sequence,
            None,
            None,
            Some(PlayerState::Paused),
        );
        Ok(())
    }

    pub fn seek(&self, position: Duration) -> Result<()> {
        self.ensure_not_closed()?;
        let commands = self.playback_commands()?;
        let (
            previous_clock,
            previous_generation,
            previous_ended,
            previous_pending_play_sequence,
            sequence,
            generation,
            resume_after_seek,
        ) = {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            let previous_clock = inner.playback_clock.clone();
            let previous_generation = inner.playback_generation;
            let previous_ended = inner.ended;
            let previous_pending_play_sequence = inner.pending_play_sequence;
            let resume_after_seek = inner.state == PlayerState::Playing
                || inner.pending_play_sequence == Some(inner.playback_command_sequence);
            inner.playback_command_sequence =
                inner.playback_command_sequence.saturating_add(1).max(1);
            // A Seek supersedes the queued Play command, but it also carries
            // that command's resume intent. Keep the new Seek sequence visible
            // as Playing until the worker either commits or fails it.
            inner.pending_play_sequence =
                resume_after_seek.then_some(inner.playback_command_sequence);
            inner.playback_clock = PlaybackClock::paused_at(position);
            inner.playback_generation = inner.playback_generation.saturating_add(1).max(1);
            inner.ended = false;
            (
                previous_clock,
                previous_generation,
                previous_ended,
                previous_pending_play_sequence,
                inner.playback_command_sequence,
                inner.playback_generation,
                resume_after_seek,
            )
        };
        if commands
            .send(PlaybackCommand::Seek {
                position,
                sequence,
                generation,
                resume_after_seek,
            })
            .is_err()
        {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            if inner.playback_command_sequence == sequence {
                inner.playback_clock = previous_clock;
                inner.playback_generation = previous_generation;
                inner.ended = previous_ended;
                inner.pending_play_sequence = previous_pending_play_sequence;
            }
            return Err(PlayerError::Playback(
                "playback worker is not running".to_string(),
            ));
        }
        let _ = commit_playback_command_intent(&self.inner, sequence, None, Some(position), None);
        Ok(())
    }

    pub fn set_playback_rate(&self, rate: f64) -> Result<()> {
        self.ensure_not_closed()?;
        self.send_playback_command(PlaybackCommand::SetPlaybackRate(rate))
    }

    pub fn add_external_subtitle(&self, uri: impl Into<String>) -> Result<SubtitleTrackConfig> {
        self.ensure_not_closed()?;
        let uri = uri.into();
        let id = next_external_subtitle_track_id();
        let config = SubtitleTrackConfig::external(id, uri);
        let (reply, response) = bounded(1);
        self.send_playback_command(PlaybackCommand::AddExternalSubtitle { config, reply })?;
        response
            .recv()
            .map_err(|_| {
                PlayerError::Playback(
                    "playback worker stopped before opening the external subtitle".to_string(),
                )
            })?
            .map_err(PlayerError::Playback)
    }

    pub fn remove_subtitle_track(&self, track_id: i64) -> Result<()> {
        self.ensure_not_closed()?;
        self.send_playback_command(PlaybackCommand::RemoveSubtitleTrack(track_id))
    }

    pub fn select_audio_track(&self, track_id: Option<i64>) -> Result<()> {
        self.ensure_not_closed()?;
        self.send_playback_command(PlaybackCommand::SelectAudioTrack(track_id))
    }

    /// Selects a subtitle track, or none. Asking for none while no media is
    /// open succeeds and changes nothing: no subtitle is shown then.
    pub fn select_subtitle_track(&self, track_id: Option<i64>) -> Result<()> {
        self.ensure_not_closed()?;
        if track_id.is_none() && self.optional_playback_commands().is_none() {
            return Ok(());
        }
        self.send_playback_command(PlaybackCommand::SelectSubtitleTrack(track_id))
    }

    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.inner
            .lock()
            .expect("player mutex poisoned")
            .tracks
            .clone()
    }

    pub fn track_selection(&self) -> TrackSelection {
        self.inner
            .lock()
            .expect("player mutex poisoned")
            .track_selection
    }

    /// Chapters read while opening the current media; empty when absent.
    pub fn chapters(&self) -> Vec<ChapterInfo> {
        self.inner
            .lock()
            .expect("player mutex poisoned")
            .chapters
            .clone()
    }

    pub fn stop(&self) -> Result<()> {
        self.ensure_not_closed()?;
        let commands = self.playback_commands()?;
        let (
            previous_clock,
            previous_generation,
            previous_ended,
            previous_pending_play_sequence,
            sequence,
            generation,
        ) = {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            let previous_clock = inner.playback_clock.clone();
            let previous_generation = inner.playback_generation;
            let previous_ended = inner.ended;
            let previous_pending_play_sequence = inner.pending_play_sequence;
            inner.playback_command_sequence =
                inner.playback_command_sequence.saturating_add(1).max(1);
            inner.pending_play_sequence = None;
            inner.playback_clock = PlaybackClock::paused_at(Duration::ZERO);
            inner.playback_generation = inner.playback_generation.saturating_add(1).max(1);
            inner.ended = false;
            (
                previous_clock,
                previous_generation,
                previous_ended,
                previous_pending_play_sequence,
                inner.playback_command_sequence,
                inner.playback_generation,
            )
        };
        if commands
            .send(PlaybackCommand::Stop {
                sequence,
                generation,
            })
            .is_err()
        {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            if inner.playback_command_sequence == sequence {
                inner.playback_clock = previous_clock;
                inner.playback_generation = previous_generation;
                inner.ended = previous_ended;
                inner.pending_play_sequence = previous_pending_play_sequence;
            }
            return Err(PlayerError::Playback(
                "playback worker is not running".to_string(),
            ));
        }
        let _ = commit_playback_command_intent(
            &self.inner,
            sequence,
            None,
            Some(Duration::ZERO),
            Some(PlayerState::Stopped),
        );
        Ok(())
    }

    /// Submit a snapshot read synchronously on the current playback timeline.
    /// For delayed or concurrent measurements, use [`Self::capture_audio_clock`]
    /// and [`Self::update_audio_clock_observation`] to retain capture identity.
    pub fn update_audio_clock(&self, snapshot: AudioClockSnapshot) -> Result<()> {
        let context =
            AudioClockContext::from_inner(&self.inner.lock().expect("player mutex poisoned"));
        self.update_audio_clock_observation(AudioClockObservation {
            player_id: self.id,
            snapshot,
            captured_at: Instant::now(),
            context,
        })
    }

    /// Capture identity and time before reading the backend, without holding a
    /// player lock across backend work. A concurrent seek/reset invalidates the
    /// resulting observation instead of relabelling old PCM as the new timeline.
    /// Read the backend synchronously here; do not return a previously cached
    /// snapshot. The output owner must serialize reads with output reconfiguration.
    pub fn capture_audio_clock(
        &self,
        read: impl FnOnce() -> Option<AudioClockSnapshot>,
    ) -> Option<AudioClockObservation> {
        let context =
            AudioClockContext::from_inner(&self.inner.lock().expect("player mutex poisoned"));
        let captured_at = Instant::now();
        Some(AudioClockObservation {
            player_id: self.id,
            snapshot: read()?,
            captured_at,
            context,
        })
    }

    pub(crate) fn invalidate_audio_clock(&self) {
        let mut inner = self.inner.lock().expect("player mutex poisoned");
        inner.audio_output_epoch = inner.audio_output_epoch.saturating_add(1);
    }

    pub fn update_audio_clock_observation(&self, observation: AudioClockObservation) -> Result<()> {
        self.ensure_not_closed()?;
        if observation.player_id != self.id {
            return Err(PlayerError::Playback(
                "audio observation belongs to another player".into(),
            ));
        }
        let commands = self.playback_commands()?;
        match commands.try_send(PlaybackCommand::AudioClock(observation)) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err(PlayerError::Playback(
                "playback worker is not running".to_string(),
            )),
        }
    }

    pub fn report_video_frame_import_failure(
        &self,
        failure: VideoFrameImportFailure,
    ) -> Result<()> {
        self.ensure_not_closed()?;
        self.send_playback_command(PlaybackCommand::VideoFrameImportFailed(failure))
    }

    pub(crate) fn set_frame_output_quiesced(&self, quiesced: bool) -> Result<bool> {
        self.set_frame_output_quiesced_with_timeout(quiesced, FRAME_OUTPUT_BARRIER_TIMEOUT)
    }

    pub(crate) fn set_video_decode_suspended(&self, suspended: bool) -> Result<bool> {
        self.set_video_decode_suspended_with_timeout(suspended, FRAME_OUTPUT_BARRIER_TIMEOUT)
    }

    fn set_video_decode_suspended_with_timeout(
        &self,
        suspended: bool,
        timeout: Duration,
    ) -> Result<bool> {
        let Some(commands) = self.optional_playback_commands() else {
            return Ok(false);
        };
        let (reply, response) = bounded(1);
        let requested_at = Instant::now();
        match commands.send_timeout(
            PlaybackCommand::SetVideoDecodeSuspended { suspended, reply },
            timeout,
        ) {
            Ok(()) => {}
            Err(crossbeam_channel::SendTimeoutError::Timeout(_)) => {
                return Err(PlayerError::Playback(format!(
                    "timed out after {} ms while sending video decode mode to the playback worker",
                    timeout.as_millis(),
                )));
            }
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                return Err(PlayerError::Playback(
                    "playback worker is not running".to_string(),
                ));
            }
        }
        let remaining = timeout.saturating_sub(requested_at.elapsed());
        match response.recv_timeout(remaining) {
            Ok(result) => result.map_err(PlayerError::Playback)?,
            Err(RecvTimeoutError::Timeout) => {
                if suspended {
                    enqueue_video_decode_resume_after_timeout(&commands);
                }
                return Err(PlayerError::Playback(format!(
                    "playback worker did not acknowledge video decode mode within {} ms",
                    timeout.as_millis(),
                )));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(PlayerError::Playback(
                    "playback worker stopped before acknowledging video decode mode".to_string(),
                ));
            }
        }
        Ok(true)
    }

    fn set_frame_output_quiesced_with_timeout(
        &self,
        quiesced: bool,
        timeout: Duration,
    ) -> Result<bool> {
        let Some(commands) = self.optional_playback_commands() else {
            return Ok(false);
        };
        let (reply, response) = bounded(1);
        let requested_at = Instant::now();
        let action = if quiesced { "quiesce" } else { "resume" };
        trace::diagnostic(
            serde_json::json!({
                "event": "player_frame_output",
                "stage": "request",
                "action": action,
                "timeoutMs": timeout.as_millis(),
                "generation": self.playback_generation(),
            })
            .to_string(),
        );
        match commands.send_timeout(
            PlaybackCommand::SetFrameOutputQuiesced { quiesced, reply },
            timeout,
        ) {
            Ok(()) => {}
            Err(crossbeam_channel::SendTimeoutError::Timeout(_)) => {
                let message = format!(
                    "timed out after {} ms while sending frame output {action} to the playback worker",
                    timeout.as_millis(),
                );
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_frame_output",
                        "stage": "command_timeout",
                        "action": action,
                        "timeoutMs": timeout.as_millis(),
                        "elapsedMs": requested_at.elapsed().as_millis(),
                        "reason": message.as_str(),
                    })
                    .to_string(),
                );
                return Err(PlayerError::Playback(message));
            }
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                return Err(PlayerError::Playback(
                    "playback worker is not running".to_string(),
                ));
            }
        }
        let remaining = timeout.saturating_sub(requested_at.elapsed());
        match response.recv_timeout(remaining) {
            Ok(()) => {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_frame_output",
                        "stage": "acknowledged",
                        "action": action,
                        "elapsedMs": requested_at.elapsed().as_millis(),
                        "generation": self.playback_generation(),
                    })
                    .to_string(),
                );
            }
            Err(RecvTimeoutError::Timeout) => {
                let message = format!(
                    "playback worker did not acknowledge frame output {action} within {} ms",
                    timeout.as_millis(),
                );
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_frame_output",
                        "stage": "ack_timeout",
                        "action": action,
                        "timeoutMs": timeout.as_millis(),
                        "elapsedMs": requested_at.elapsed().as_millis(),
                        "reason": message.as_str(),
                    })
                    .to_string(),
                );
                if quiesced {
                    enqueue_frame_output_resume_after_timeout(&commands);
                }
                return Err(PlayerError::Playback(message));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(PlayerError::Playback(format!(
                    "playback worker stopped before acknowledging frame output {action}",
                )));
            }
        }
        Ok(true)
    }

    pub(crate) fn report_audio_output_event(&self, event: AudioOutputEvent) {
        self.emit(PlayerEvent::AudioOutputChanged(event));
    }

    pub fn close(&self) -> Result<()> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .expect("player lifecycle mutex poisoned");
        lifecycle.epoch = lifecycle.epoch.saturating_add(1).max(1);
        let previous = lifecycle.playback.take();
        // Stop and join before publishing Closed so no worker event can appear
        // after the terminal lifecycle event.
        drop(previous);
        {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            inner.media = None;
            inner.tracks.clear();
            inner.chapters.clear();
            inner.track_selection = TrackSelection::default();
            inner.duration = None;
            inner.playback_clock = PlaybackClock::paused_at(Duration::ZERO);
            inner.playback_generation = inner.playback_generation.saturating_add(1).max(1);
            inner.ended = false;
            inner.pending_play_sequence = None;
        }
        self.transition(PlayerState::Closed)?;
        self.emit(PlayerEvent::BufferingChanged(false));
        Ok(())
    }

    pub fn attach_surface(&self, surface: PlatformSurface) -> Result<()> {
        self.ensure_not_closed()?;
        {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            inner.surface = Some(surface);
        }
        self.emit(PlayerEvent::SurfaceAttached(surface));
        Ok(())
    }

    pub fn detach_surface(&self) -> Result<()> {
        {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            inner.surface = None;
        }
        self.emit(PlayerEvent::SurfaceDetached);
        Ok(())
    }

    fn ensure_not_closed(&self) -> Result<()> {
        if self.state() == PlayerState::Closed {
            Err(PlayerError::Closed)
        } else {
            Ok(())
        }
    }

    fn send_playback_command(&self, command: PlaybackCommand) -> Result<()> {
        let commands = self.playback_commands()?;
        commands
            .send(command)
            .map_err(|_| PlayerError::Playback("playback worker is not running".to_string()))
    }

    fn playback_commands(&self) -> Result<Sender<PlaybackCommand>> {
        let lifecycle = self
            .lifecycle
            .lock()
            .expect("player lifecycle mutex poisoned");
        Ok(lifecycle
            .playback
            .as_ref()
            .ok_or_else(|| PlayerError::Playback("no media is open".to_string()))?
            .commands
            .clone())
    }

    /// Whether a media is open (opening counts): a playback worker exists.
    pub fn has_open_media(&self) -> bool {
        self.optional_playback_commands().is_some()
    }

    fn optional_playback_commands(&self) -> Option<Sender<PlaybackCommand>> {
        self.lifecycle
            .lock()
            .expect("player lifecycle mutex poisoned")
            .playback
            .as_ref()
            .map(|runtime| runtime.commands.clone())
    }

    fn superseded_open_error(&self) -> PlayerError {
        if self.state() == PlayerState::Closed {
            PlayerError::Closed
        } else {
            PlayerError::Playback("media open was superseded by a newer lifecycle operation".into())
        }
    }

    fn transition(&self, next: PlayerState) -> Result<()> {
        let previous = {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            let previous = inner.state;
            inner.state = next;
            previous
        };
        if previous != next {
            self.emit(PlayerEvent::StateChanged(next));
        }
        Ok(())
    }

    fn transition_to_opening(&self) {
        let previous = {
            let mut inner = self.inner.lock().expect("player mutex poisoned");
            inner.chapters.clear();
            // A racing Play may have cloned the old worker sender before this
            // Open took the lifecycle lock, then queued behind Shutdown. The
            // old worker is joined above, so invalidate that orphaned intent
            // in the same critical section that publishes Opening.
            inner.pending_play_sequence = None;
            let previous = inner.state;
            inner.state = PlayerState::Opening;
            previous
        };
        if previous != PlayerState::Opening {
            self.emit(PlayerEvent::StateChanged(PlayerState::Opening));
        }
    }

    fn emit(&self, event: PlayerEvent) {
        let mut inner = self.inner.lock().expect("player mutex poisoned");
        inner
            .subscribers
            .retain(|sender| sender.send(event.clone()).is_ok());
    }
}

fn run_playback_worker(
    engine: &mut VideoPlaybackEngine,
    inner: Arc<Mutex<PlayerInner>>,
    commands: Receiver<PlaybackCommand>,
    initial_generation: u64,
) {
    let worker_started = std::time::Instant::now();
    let mut last_position_event = None;
    let mut last_position_emit = Instant::now()
        .checked_sub(POSITION_EVENT_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut playback_generation = initial_generation.max(1);
    let mut frame_output_quiesced = false;
    let mut last_executed_playback_command_sequence = 0u64;
    let mut last_worker_clock = None;
    let mut last_audio_observation = None;
    let mut audio_output_backpressure = AudioOutputBackpressureState::default();
    let mut buffering = PlaybackBufferingTracker::default();
    let mut eof_published = false;
    let mut loop_count = 0u64;
    trace::log(format!(
        "[erika-playback-trace] stage=worker_start state={:?} generation={} poll_ms={} uptime_ms={:.3}",
        engine.state(),
        playback_generation,
        playback_poll_interval(engine.state(), engine.has_pending_paused_seek_frame()).as_millis(),
        worker_started.elapsed().as_secs_f64() * 1000.0,
    ));
    loop {
        loop_count = loop_count.saturating_add(1);
        let loop_started = std::time::Instant::now();
        let mut command_count = 0usize;
        match commands.recv_timeout(playback_poll_interval(
            engine.state(),
            // A quiesced output cannot present the pending preview frame, so
            // fast polling for it would never terminate.
            engine.has_pending_paused_seek_frame() && !frame_output_quiesced,
        )) {
            Ok(command) => {
                command_count += 1;
                observe_audio_pump_command(engine.state(), &mut last_audio_observation, &command);
                if !handle_playback_command(
                    engine,
                    &inner,
                    command,
                    &mut playback_generation,
                    &mut last_executed_playback_command_sequence,
                    &mut frame_output_quiesced,
                ) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        while let Ok(command) = commands.try_recv() {
            command_count += 1;
            observe_audio_pump_command(engine.state(), &mut last_audio_observation, &command);
            if !handle_playback_command(
                engine,
                &inner,
                command,
                &mut playback_generation,
                &mut last_executed_playback_command_sequence,
                &mut frame_output_quiesced,
            ) {
                return;
            }
        }
        if engine.state() != PlaybackRunState::Ended {
            eof_published = false;
        }
        let after_commands = std::time::Instant::now();

        let audio_observation = last_audio_observation.filter(|observation| {
            observation.is_current(
                &inner.lock().expect("player mutex poisoned"),
                playback_generation,
                last_executed_playback_command_sequence,
                after_commands,
            )
        });
        let last_audio_snapshot = audio_observation.map(|observation| observation.snapshot);
        let last_audio_snapshot_at = audio_observation.map(|observation| observation.captured_at);

        engine.set_audio_output_active(audio_frame_output_is_active(&inner));
        if engine.state() != PlaybackRunState::Playing || frame_output_quiesced {
            audio_output_backpressure.reset();
        }

        sync_playback_clock_from_worker(
            engine,
            &inner,
            playback_generation,
            "before_video_tick",
            last_executed_playback_command_sequence,
            &mut last_worker_clock,
        );
        let after_clock_sync = std::time::Instant::now();

        let mut produced_video_frame = false;
        if !frame_output_quiesced {
            match engine.tick() {
                Ok(Some(frame)) => {
                    produced_video_frame = true;
                    let position = frame.pts.unwrap_or(frame.media_time);
                    last_position_event = Some((playback_generation, position));
                    trace::log(format!(
                        "[erika-playback-trace] stage=video_frame gen={} pts={} media={} late={} loop={} commands={} state={:?}",
                        playback_generation,
                        trace::duration_label(frame.pts),
                        trace::duration_label(Some(frame.media_time)),
                        frame
                            .late_by
                            .map(|duration| format!("{:.3}", duration.as_secs_f64()))
                            .unwrap_or_else(|| "-".to_string()),
                        loop_count,
                        command_count,
                        engine.state(),
                    ));
                    match PlayerVideoFrame::from_decoded(
                        frame.frame,
                        frame.decode_backend,
                        frame.pts,
                        frame.media_time,
                        frame.late_by,
                        playback_generation,
                    ) {
                        Ok(frame) => emit_video_frame_from_worker(&inner, frame),
                        Err(error) => fail_playback_from_worker(
                            engine,
                            &inner,
                            "video_frame_handoff",
                            error.to_string(),
                        ),
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    trace::log(format!(
                        "[erika-playback-trace] stage=video_tick_error gen={} loop={} commands={} error={}",
                        playback_generation, loop_count, command_count, error,
                    ));
                    fail_playback_from_worker(engine, &inner, "video_tick", error.to_string());
                }
            }
        }

        sync_playback_clock_from_worker(
            engine,
            &inner,
            playback_generation,
            "after_video_tick",
            last_executed_playback_command_sequence,
            &mut last_worker_clock,
        );
        let after_video = std::time::Instant::now();

        if !frame_output_quiesced && engine.state() == PlaybackRunState::Playing {
            if should_prefill_audio_from_worker(engine, last_audio_snapshot, last_audio_snapshot_at)
            {
                pump_audio_from_worker(
                    engine,
                    &inner,
                    playback_generation,
                    "after_video_tick",
                    AUDIO_PREFILL_AFTER_VIDEO_LIMIT,
                    &mut audio_output_backpressure,
                );
            }
        }

        // Subtitles are delivered while paused too: a seek (or a subtitle
        // chosen) on a paused player presents a frame, and the cue that is
        // on at that time has to arrive with it, not after the next play.
        if !frame_output_quiesced
            && matches!(
                engine.state(),
                PlaybackRunState::Playing | PlaybackRunState::Paused
            )
        {
            match engine.tick_subtitle() {
                Ok(Some(frame)) => emit_subtitle_frame_from_worker(
                    &inner,
                    PlayerSubtitleFrame {
                        frame: frame.frame,
                        pts: frame.pts,
                        media_time: frame.media_time,
                        late_by: frame.late_by,
                        generation: playback_generation,
                    },
                ),
                Ok(None) => {}
                Err(error) => {
                    fail_playback_from_worker(engine, &inner, "subtitle_tick", error.to_string());
                }
            }
        }

        let buffering_event = update_buffering_from_worker(
            engine,
            &mut buffering,
            last_audio_snapshot,
            last_audio_snapshot_at,
            produced_video_frame,
        );

        sync_playback_clock_from_worker(
            engine,
            &inner,
            playback_generation,
            "after_av_tick",
            last_executed_playback_command_sequence,
            &mut last_worker_clock,
        );
        let after_av = std::time::Instant::now();

        if let Some(buffering) = buffering_event {
            engine.timing_counters().record_buffering(buffering);
            emit_from_worker(&inner, PlayerEvent::BufferingChanged(buffering));
        }

        emit_video_decoder_events_from_worker(engine, &inner);
        engine.publish_queue_depths();

        if last_position_event.is_some_and(|(generation, _)| generation != playback_generation) {
            last_position_event = None;
        }
        if publish_natural_eof_from_worker(engine, &inner, playback_generation, &mut eof_published)
        {
            last_position_event = None;
        } else if last_position_event.is_some()
            && (last_position_emit.elapsed() >= POSITION_EVENT_INTERVAL
                || engine.state() != PlaybackRunState::Playing)
        {
            let (generation, position) =
                last_position_event.take().expect("position event present");
            if !emit_from_worker_for_generation(
                &inner,
                generation,
                PlayerEvent::PositionChanged(position),
            ) {
                trace::log(format!(
                    "[erika-clock-trace] stage=worker_position_skip_stale position={} gen={} shared_gen={}",
                    trace::duration_label(Some(position)),
                    generation,
                    shared_playback_generation(&inner),
                ));
            }
            last_position_emit = Instant::now();
        }

        let loop_duration = loop_started.elapsed();
        if trace::enabled() && (loop_duration > Duration::from_millis(4) || command_count > 0) {
            trace::log(format!(
                "[erika-playback-trace] stage=worker_loop gen={} loop={} commands={} total_ms={:.3} commands_ms={:.3} clock_ms={:.3} video_ms={:.3} av_ms={:.3} state={:?}",
                playback_generation,
                loop_count,
                command_count,
                loop_duration.as_secs_f64() * 1000.0,
                after_commands.duration_since(loop_started).as_secs_f64() * 1000.0,
                after_clock_sync
                    .duration_since(after_commands)
                    .as_secs_f64()
                    * 1000.0,
                after_video.duration_since(after_clock_sync).as_secs_f64() * 1000.0,
                after_av.duration_since(after_video).as_secs_f64() * 1000.0,
                engine.state(),
            ));
        }
    }
}

#[derive(Default)]
struct PlaybackBufferingTracker {
    active: bool,
    starvation_started_at: Option<Instant>,
    last_video_frame_at: Option<Instant>,
    last_audio_underflow_frames: Option<u64>,
}

fn audio_snapshot_is_starved(
    snapshot: AudioClockSnapshot,
    previous_underflow_frames: Option<u64>,
    starvation_pending: bool,
) -> bool {
    let underflow_advanced =
        previous_underflow_frames.is_some_and(|previous| snapshot.underflow_frames > previous);
    snapshot.queued_frames == 0 && (underflow_advanced || starvation_pending)
}

fn audio_snapshot_recovery_reference(
    snapshot: AudioClockSnapshot,
    recovery_audio: Duration,
) -> Option<Duration> {
    let queued = snapshot.queued_duration?;
    (queued >= recovery_audio)
        .then_some(snapshot.media_time)
        .flatten()
}

fn update_buffering_from_worker(
    engine: &mut VideoPlaybackEngine,
    tracker: &mut PlaybackBufferingTracker,
    audio_snapshot: Option<AudioClockSnapshot>,
    audio_snapshot_at: Option<Instant>,
    produced_video_frame: bool,
) -> Option<bool> {
    let now = Instant::now();
    if produced_video_frame {
        tracker.last_video_frame_at = Some(now);
    }

    if engine.state() != PlaybackRunState::Playing {
        let ended_buffering = tracker.active;
        *tracker = PlaybackBufferingTracker::default();
        return ended_buffering.then_some(false);
    }

    if tracker.active && !engine.is_buffering() {
        tracker.active = false;
        tracker.starvation_started_at = None;
        tracker.last_video_frame_at = produced_video_frame.then_some(now);
        tracker.last_audio_underflow_frames =
            audio_snapshot.map(|snapshot| snapshot.underflow_frames);
        return Some(false);
    }

    let fresh_audio = audio_snapshot.filter(|_| {
        audio_snapshot_at.is_some_and(|observed| {
            now.saturating_duration_since(observed) < AUDIO_CLOCK_SNAPSHOT_STALE_AFTER
        })
    });

    if engine.is_buffering() {
        let video_ready = !engine.has_video_output() || engine.buffering_video_pts().is_some();
        let recovery_audio = engine.buffer_recovery_audio();
        let audio_reference = fresh_audio
            .and_then(|snapshot| audio_snapshot_recovery_reference(snapshot, recovery_audio));
        // A stream may legitimately end its audio track before the final
        // video frames. Once the decoder reaches EOF or the bounded recovery
        // scan establishes that audio is unavailable for now, resume from the
        // parked video timestamp instead of waiting forever for an audio queue
        // threshold that can no longer be reached.
        let audio_ready = !engine.has_audio_output()
            || audio_reference.is_some()
            || engine.buffering_audio_is_exhausted();
        if video_ready && audio_ready {
            let reference = audio_reference
                .or_else(|| engine.buffering_video_pts())
                .unwrap_or_else(|| engine.media_time());
            let source = if audio_reference.is_some() {
                PlaybackClockSource::Audio
            } else {
                PlaybackClockSource::Wall
            };
            if engine.resume_buffering_at(reference, source, now) {
                tracker.active = false;
                tracker.starvation_started_at = None;
                tracker.last_video_frame_at = Some(now);
                tracker.last_audio_underflow_frames =
                    audio_snapshot.map(|snapshot| snapshot.underflow_frames);
                return Some(false);
            }
        }
        tracker.last_audio_underflow_frames =
            audio_snapshot.map(|snapshot| snapshot.underflow_frames);
        return None;
    }

    let audio_starved = if engine.has_audio_output() {
        fresh_audio.is_some_and(|snapshot| {
            audio_snapshot_is_starved(
                snapshot,
                tracker.last_audio_underflow_frames,
                tracker.starvation_started_at.is_some(),
            )
        })
    } else {
        false
    };
    let video_starved = engine.has_video_output()
        && !engine.has_audio_output()
        && tracker
            .last_video_frame_at
            .is_some_and(|last| now.saturating_duration_since(last) >= PLAYBACK_STARVATION_GRACE);
    let waiting_for_first_frame = engine.is_waiting_for_first_frame();
    let starved = audio_starved || video_starved || waiting_for_first_frame;

    if starved {
        let started = *tracker.starvation_started_at.get_or_insert(now);
        if now.saturating_duration_since(started) >= PLAYBACK_STARVATION_GRACE
            && engine.begin_buffering_at(now)
        {
            tracker.active = true;
            tracker.last_audio_underflow_frames =
                audio_snapshot.map(|snapshot| snapshot.underflow_frames);
            return Some(true);
        }
    } else {
        tracker.starvation_started_at = None;
        if tracker.last_video_frame_at.is_none() {
            tracker.last_video_frame_at = Some(now);
        }
    }
    tracker.last_audio_underflow_frames = audio_snapshot.map(|snapshot| snapshot.underflow_frames);
    None
}

fn emit_video_decoder_events_from_worker(
    engine: &mut VideoPlaybackEngine,
    inner: &Arc<Mutex<PlayerInner>>,
) {
    for event in engine.take_video_decoder_events() {
        emit_from_worker(inner, PlayerEvent::VideoDecoderChanged(event));
    }
}

#[derive(Default)]
struct AudioOutputBackpressureState {
    started_at: Option<Instant>,
    polls: u64,
    pending_logged: bool,
}

impl AudioOutputBackpressureState {
    fn observe(&mut self, inner: &Arc<Mutex<PlayerInner>>) -> Option<String> {
        let now = Instant::now();
        let started_at = *self.started_at.get_or_insert(now);
        self.polls = self.polls.saturating_add(1);
        let (queued_frames, capacity) = audio_frame_channel_metrics(inner);
        if !self.pending_logged {
            self.pending_logged = true;
            trace::diagnostic(
                serde_json::json!({
                    "event": "playback_audio_output",
                    "stage": "backpressure_pending",
                    "polls": self.polls,
                    "queuedFrames": queued_frames,
                    "capacityFrames": capacity,
                    "timeoutSeconds": AUDIO_OUTPUT_BACKPRESSURE_TIMEOUT.as_secs_f64(),
                    "retryOwner": "playback_audio_pump",
                })
                .to_string(),
            );
        }
        let stalled_for = now.saturating_duration_since(started_at);
        if stalled_for < AUDIO_OUTPUT_BACKPRESSURE_TIMEOUT {
            return None;
        }
        let reason = format!(
            "audio frame consumer remained full for {:.3}s after {} polls (queued_frames={}, capacity={})",
            stalled_for.as_secs_f64(),
            self.polls,
            queued_frames,
            capacity,
        );
        trace::diagnostic(
            serde_json::json!({
                "event": "playback_audio_output",
                "stage": "backpressure_timeout",
                "polls": self.polls,
                "stalledSeconds": stalled_for.as_secs_f64(),
                "queuedFrames": queued_frames,
                "capacityFrames": capacity,
                "reason": reason.as_str(),
            })
            .to_string(),
        );
        Some(reason)
    }

    fn reset(&mut self) {
        self.started_at = None;
        self.polls = 0;
        self.pending_logged = false;
    }
}

fn pump_audio_from_worker(
    engine: &mut VideoPlaybackEngine,
    inner: &Arc<Mutex<PlayerInner>>,
    playback_generation: u64,
    stage: &'static str,
    burst_limit: usize,
    backpressure: &mut AudioOutputBackpressureState,
) {
    let started = Instant::now();
    let mut emitted = 0usize;
    let mut budget_exhausted = false;
    for _ in 0..burst_limit {
        let elapsed = started.elapsed();
        if elapsed >= AUDIO_PREFILL_TIME_BUDGET {
            budget_exhausted = true;
            break;
        }
        match engine.tick_audio_bounded(
            AUDIO_PREFILL_PACKET_BUDGET,
            AUDIO_PREFILL_TIME_BUDGET.saturating_sub(elapsed),
        ) {
            Ok(Some(frame)) => {
                let output_end = frame.pts.map(|pts| {
                    pts.saturating_add(Duration::from_secs_f64(
                        frame.frame.frames as f64 / f64::from(frame.frame.format.sample_rate),
                    ))
                });
                match try_emit_audio_frame_from_worker(
                    inner,
                    PlayerAudioFrame {
                        frame: frame.frame,
                        generation: playback_generation,
                    },
                ) {
                    AudioFrameEmitResult::Sent => {
                        engine.record_audio_output_end(output_end);
                        backpressure.reset();
                        emitted += 1;
                    }
                    AudioFrameEmitResult::Full(frame) => {
                        engine.restore_pending_audio_frame(frame);
                        if let Some(reason) = backpressure.observe(inner) {
                            fail_playback_from_worker(
                                engine,
                                inner,
                                "audio_output_backpressure",
                                reason,
                            );
                        }
                        break;
                    }
                    AudioFrameEmitResult::Disconnected(_frame) => {
                        backpressure.reset();
                        engine.set_audio_output_active(false);
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(error) => {
                fail_playback_from_worker(engine, inner, "audio_tick", error.to_string());
                break;
            }
        }
    }
    if emitted > 0 || budget_exhausted {
        trace::log(format!(
            "[erika-clock-trace] stage=worker_audio_prefill:{stage} emitted={} frame_limit={} packet_budget={} budget_exhausted={} elapsed_ms={:.3}",
            emitted,
            burst_limit,
            AUDIO_PREFILL_PACKET_BUDGET,
            budget_exhausted,
            started.elapsed().as_secs_f64() * 1000.0,
        ));
    }
}

fn should_prefill_audio_from_worker(
    engine: &VideoPlaybackEngine,
    snapshot: Option<AudioClockSnapshot>,
    snapshot_at: Option<Instant>,
) -> bool {
    if !engine.should_prefill_audio() {
        return false;
    }
    if snapshot_at.is_none_or(|observed| observed.elapsed() >= AUDIO_CLOCK_SNAPSHOT_STALE_AFTER) {
        return true;
    }
    snapshot
        .and_then(|snapshot| snapshot.queued_duration)
        .is_none_or(|queued| queued < AUDIO_PREFILL_LOW_WATER)
}

fn observe_audio_pump_command(
    engine_state: PlaybackRunState,
    last_audio_observation: &mut Option<AudioClockObservation>,
    command: &PlaybackCommand,
) {
    match command {
        PlaybackCommand::AudioClock(observation) => {
            if last_audio_observation.is_none_or(|last| observation.captured_at > last.captured_at)
            {
                *last_audio_observation = Some(*observation);
            }
        }
        PlaybackCommand::Play { .. }
            if matches!(
                engine_state,
                PlaybackRunState::Stopped | PlaybackRunState::Ended
            ) =>
        {
            *last_audio_observation = None;
        }
        PlaybackCommand::Seek { .. }
        | PlaybackCommand::Stop { .. }
        | PlaybackCommand::SetPlaybackRate(_)
        | PlaybackCommand::SelectSubtitleTrack(_)
        | PlaybackCommand::SelectAudioTrack(_) => {
            *last_audio_observation = None;
        }
        _ => {}
    }
}

fn handle_playback_command(
    engine: &mut VideoPlaybackEngine,
    inner: &Arc<Mutex<PlayerInner>>,
    command: PlaybackCommand,
    playback_generation: &mut u64,
    last_executed_playback_command_sequence: &mut u64,
    frame_output_quiesced: &mut bool,
) -> bool {
    match command {
        PlaybackCommand::Play {
            sequence,
            generation,
        } => {
            if !begin_playback_command_execution(sequence, last_executed_playback_command_sequence)
                || !playback_command_sequence_is_current(inner, sequence)
            {
                return true;
            }
            let state_before = engine.state();
            let restarting = matches!(
                state_before,
                PlaybackRunState::Stopped | PlaybackRunState::Ended
            );
            match engine.play_checked() {
                Ok(restarted) => {
                    *playback_generation = (*playback_generation).max(generation).max(1);
                    if restarted {
                        *playback_generation = playback_generation.saturating_add(1).max(1);
                    }
                    if !playback_command_sequence_is_current(inner, sequence) {
                        return true;
                    }
                    let _ = commit_playback_command_intent(
                        inner,
                        sequence,
                        Some(*playback_generation),
                        restarted.then_some(Duration::ZERO),
                        Some(PlayerState::Playing),
                    );
                }
                Err(error) => {
                    if !playback_command_sequence_is_current(inner, sequence) {
                        return true;
                    }
                    let message = if restarting {
                        format!("failed to restart playback from {state_before:?} at zero: {error}")
                    } else {
                        format!("failed to start playback from {state_before:?}: {error}")
                    };
                    fail_playback_command_from_worker(
                        engine,
                        inner,
                        sequence,
                        if restarting { "play_restart" } else { "play" },
                        message,
                    );
                }
            }
        }
        PlaybackCommand::Pause {
            sequence,
            activate_before_pause,
        } => {
            if !begin_playback_command_execution(sequence, last_executed_playback_command_sequence)
                || !playback_command_sequence_is_current(inner, sequence)
            {
                return true;
            }
            let mut restarted = false;
            if activate_before_pause && engine.state() != PlaybackRunState::Playing {
                match engine.play_checked() {
                    Ok(did_restart) => {
                        restarted = did_restart;
                        if did_restart {
                            *playback_generation = playback_generation.saturating_add(1).max(1);
                        }
                    }
                    Err(error) => {
                        fail_playback_command_from_worker(
                            engine,
                            inner,
                            sequence,
                            "pause_after_queued_play",
                            format!("failed to activate engine before pause: {error}"),
                        );
                        return true;
                    }
                }
            }
            engine.pause();
            let position = if restarted {
                Duration::ZERO
            } else {
                engine.media_time()
            };
            let _ = commit_playback_command_intent(
                inner,
                sequence,
                restarted.then_some(*playback_generation),
                Some(position),
                Some(PlayerState::Paused),
            );
        }
        PlaybackCommand::Seek {
            position,
            sequence,
            generation,
            resume_after_seek,
        } => {
            if !begin_playback_command_execution(sequence, last_executed_playback_command_sequence)
            {
                trace::log(format!(
                    "[erika-clock-trace] stage=worker_command_seek_skip_out_of_order target={} sequence={} last_executed_sequence={}",
                    trace::duration_label(Some(position)),
                    sequence,
                    *last_executed_playback_command_sequence,
                ));
                return true;
            }
            trace::log(format!(
                "[erika-clock-trace] stage=worker_command_seek target={} gen_before={} sequence={} resume_after_seek={}",
                trace::duration_label(Some(position)),
                *playback_generation,
                sequence,
                resume_after_seek,
            ));
            let result = engine.seek_with_playback_intent(position, resume_after_seek);
            if result.is_ok() {
                *playback_generation = (*playback_generation).max(generation).max(1);
            }
            if !playback_command_sequence_is_current(inner, sequence) {
                trace::log(format!(
                    "[erika-clock-trace] stage=worker_command_seek_superseded_after_execute target={} sequence={}",
                    trace::duration_label(Some(position)),
                    sequence,
                ));
                return true;
            }
            match result {
                Err(error) => fail_playback_command_from_worker(
                    engine,
                    inner,
                    sequence,
                    "seek",
                    error.to_string(),
                ),
                Ok(()) => {
                    let _ = commit_playback_command_intent(
                        inner,
                        sequence,
                        Some(*playback_generation),
                        None,
                        resume_after_seek.then_some(PlayerState::Playing),
                    );
                    trace::log(format!(
                        "[erika-clock-trace] stage=worker_command_seek_done target={} gen_after={} sequence={} resume_after_seek={}",
                        trace::duration_label(Some(position)),
                        *playback_generation,
                        sequence,
                        resume_after_seek,
                    ));
                }
            }
        }
        PlaybackCommand::SetPlaybackRate(rate) => {
            engine.set_playback_rate(rate);
        }
        PlaybackCommand::Stop {
            sequence,
            generation,
        } => {
            if !begin_playback_command_execution(sequence, last_executed_playback_command_sequence)
            {
                return true;
            }
            let result = engine.stop_checked();
            if !playback_command_sequence_is_current(inner, sequence) {
                trace::log(format!(
                    "[erika-clock-trace] stage=worker_command_stop_superseded_after_execute sequence={sequence}",
                ));
                return true;
            }
            match result {
                Ok(()) => {
                    *playback_generation = (*playback_generation).max(generation).max(1);
                    let _ = commit_playback_command_intent(
                        inner,
                        sequence,
                        Some(*playback_generation),
                        Some(Duration::ZERO),
                        Some(PlayerState::Stopped),
                    );
                }
                Err(error) => {
                    fail_playback_from_worker(engine, inner, "stop", error.to_string());
                }
            }
        }
        PlaybackCommand::AudioClock(observation) => {
            let shared = inner.lock().expect("player mutex poisoned");
            if !observation.is_current(
                &shared,
                *playback_generation,
                *last_executed_playback_command_sequence,
                Instant::now(),
            ) {
                trace::log(format!(
                    "[erika-clock-trace] stage=worker_audio_clock_skip observed={:?} current={:?} worker_gen={} worker_sequence={} age_ms={} state={:?}",
                    observation.context,
                    AudioClockContext::from_inner(&shared),
                    *playback_generation,
                    *last_executed_playback_command_sequence,
                    observation.captured_at.elapsed().as_millis(),
                    shared.state,
                ));
                return true;
            }
            drop(shared);
            let snapshot = observation.snapshot;
            trace::log(format!(
                "[erika-clock-trace] stage=worker_command_audio_clock media={} queued={} queued_frames={} read={} written={} underflow={} gen={}",
                trace::duration_label(snapshot.media_time),
                trace::duration_label(snapshot.queued_duration),
                snapshot.queued_frames,
                snapshot.read_frames,
                snapshot.written_frames,
                snapshot.underflow_frames,
                *playback_generation,
            ));
            let _ = engine.sync_to_audio_clock_observed(
                snapshot,
                observation.captured_at,
                observation.context.output_epoch,
            );
        }
        PlaybackCommand::VideoFrameImportFailed(failure) => {
            trace::diagnostic(failure.structured_message());
            let shared_generation = shared_playback_generation(inner);
            if !video_import_feedback_is_current(
                failure.generation,
                *playback_generation,
                shared_generation,
            ) {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "video_frame_import_failure",
                        "stage": "stale_feedback_ignored",
                        "failureGeneration": failure.generation,
                        "workerGeneration": *playback_generation,
                        "sharedGeneration": shared_generation,
                        "backend": failure.decode_backend.as_str(),
                        "mediaCodecSurface": failure.mediacodec_surface,
                        "reason": failure.reason.as_str(),
                    })
                    .to_string(),
                );
                return true;
            }
            match failure.decode_backend {
                DecoderBackend::MediaCodec
                | DecoderBackend::VideoToolbox
                | DecoderBackend::D3d11va
                | DecoderBackend::Cuda
                | DecoderBackend::Vaapi
                | DecoderBackend::AvCodec => {
                    if let Err(error) = engine.handle_video_frame_import_failure(&failure) {
                        fail_video_import_from_worker(
                            engine,
                            inner,
                            format!(
                                "{}; software decoder recovery failed: {error}",
                                failure.structured_message()
                            ),
                        );
                    }
                }
                DecoderBackend::Software => {
                    fail_video_import_from_worker(engine, inner, failure.structured_message());
                }
            }
        }
        PlaybackCommand::AddExternalSubtitle { config, reply } => {
            match engine.add_external_subtitle(config) {
                Ok((track, clear_frame)) => {
                    *playback_generation = playback_generation.saturating_add(1).max(1);
                    publish_playback_generation_from_worker(inner, *playback_generation);
                    if let Some(frame) = clear_frame {
                        emit_subtitle_frame_from_worker(
                            inner,
                            PlayerSubtitleFrame {
                                frame: frame.frame,
                                pts: frame.pts,
                                media_time: frame.media_time,
                                late_by: frame.late_by,
                                generation: *playback_generation,
                            },
                        );
                    }
                    sync_track_state_from_worker(inner, engine);
                    let _ = reply.send(Ok(track));
                }
                Err(error) => {
                    let message = error.to_string();
                    emit_from_worker(
                        inner,
                        PlayerEvent::Error(PlayerError::Playback(message.clone())),
                    );
                    let _ = reply.send(Err(message));
                }
            }
        }
        PlaybackCommand::RemoveSubtitleTrack(track_id) => {
            match engine.remove_subtitle_track(track_id) {
                Ok(Some(frame)) => {
                    *playback_generation = playback_generation.saturating_add(1).max(1);
                    publish_playback_generation_from_worker(inner, *playback_generation);
                    emit_subtitle_frame_from_worker(
                        inner,
                        PlayerSubtitleFrame {
                            frame: frame.frame,
                            pts: frame.pts,
                            media_time: frame.media_time,
                            late_by: frame.late_by,
                            generation: *playback_generation,
                        },
                    );
                    sync_track_state_from_worker(inner, engine);
                }
                Ok(None) => {}
                Err(error) => emit_from_worker(
                    inner,
                    PlayerEvent::Error(PlayerError::Playback(error.to_string())),
                ),
            }
        }
        PlaybackCommand::SelectAudioTrack(track_id) => match engine.select_audio_track(track_id) {
            Ok(()) => {
                *playback_generation = playback_generation.saturating_add(1).max(1);
                publish_playback_generation_from_worker(inner, *playback_generation);
                sync_track_state_from_worker(inner, engine)
            }
            Err(error) => emit_from_worker(
                inner,
                PlayerEvent::Error(PlayerError::Playback(error.to_string())),
            ),
        },
        PlaybackCommand::SelectSubtitleTrack(track_id) => {
            match engine.select_subtitle_track(track_id) {
                Ok(Some(frame)) => {
                    *playback_generation = playback_generation.saturating_add(1).max(1);
                    publish_playback_generation_from_worker(inner, *playback_generation);
                    emit_subtitle_frame_from_worker(
                        inner,
                        PlayerSubtitleFrame {
                            frame: frame.frame,
                            pts: frame.pts,
                            media_time: frame.media_time,
                            late_by: frame.late_by,
                            generation: *playback_generation,
                        },
                    );
                    sync_track_state_from_worker(inner, engine);
                }
                Ok(None) => {
                    *playback_generation = playback_generation.saturating_add(1).max(1);
                    publish_playback_generation_from_worker(inner, *playback_generation);
                    sync_track_state_from_worker(inner, engine)
                }
                Err(error) => emit_from_worker(
                    inner,
                    PlayerEvent::Error(PlayerError::Playback(error.to_string())),
                ),
            }
        }
        PlaybackCommand::SetFrameOutputQuiesced { quiesced, reply } => {
            *frame_output_quiesced = quiesced;
            if !quiesced {
                engine.rebase_progress_watchdogs();
            }
            trace::diagnostic(
                serde_json::json!({
                    "event": "player_frame_output",
                    "stage": if quiesced { "quiesced" } else { "resumed" },
                    "generation": *playback_generation,
                })
                .to_string(),
            );
            let _ = reply.send(());
        }
        PlaybackCommand::SetVideoDecodeSuspended { suspended, reply } => {
            let result = engine
                .set_video_decode_suspended(suspended)
                .map_err(|error| error.to_string());
            trace::diagnostic(
                serde_json::json!({
                    "event": "player_video_decode",
                    "stage": if suspended { "suspended" } else { "resumed_at_keyframe" },
                    "generation": *playback_generation,
                })
                .to_string(),
            );
            let _ = reply.send(result);
        }
        PlaybackCommand::Shutdown => return false,
    }
    true
}

fn fail_playback_from_worker(
    engine: &mut VideoPlaybackEngine,
    inner: &Arc<Mutex<PlayerInner>>,
    stage: &'static str,
    message: String,
) {
    // A decoder/demux/audio failure is terminal for the active session. Pause
    // the engine before publishing the error so the worker cannot retry the
    // same failing packet every poll interval and flood both native and Dart
    // event queues. The Player remains alive for explicit close/dispose.
    engine.pause();
    trace::diagnostic(
        serde_json::json!({
            "event": "playback_fatal",
            "stage": stage,
            "reason": message.as_str(),
        })
        .to_string(),
    );
    emit_from_worker(inner, PlayerEvent::Error(PlayerError::Playback(message)));
    set_state_from_worker(inner, PlayerState::Error);
}

fn fail_playback_command_from_worker(
    engine: &mut VideoPlaybackEngine,
    inner: &Arc<Mutex<PlayerInner>>,
    sequence: u64,
    stage: &'static str,
    message: String,
) {
    engine.pause();
    trace::diagnostic(
        serde_json::json!({
            "event": "playback_fatal",
            "stage": stage,
            "reason": message.as_str(),
            "commandSequence": sequence,
        })
        .to_string(),
    );
    let _ = commit_playback_command_failure(inner, sequence, message);
}

fn commit_playback_command_failure(
    inner: &Arc<Mutex<PlayerInner>>,
    sequence: u64,
    message: String,
) -> bool {
    let mut inner = inner.lock().expect("player mutex poisoned");
    if inner.playback_command_sequence != sequence {
        return false;
    }
    inner.pending_play_sequence = None;
    inner.subscribers.retain(|sender| {
        sender
            .send(PlayerEvent::Error(PlayerError::Playback(message.clone())))
            .is_ok()
    });
    let previous = inner.state;
    inner.state = PlayerState::Error;
    if previous != PlayerState::Error {
        inner.subscribers.retain(|sender| {
            sender
                .send(PlayerEvent::StateChanged(PlayerState::Error))
                .is_ok()
        });
    }
    true
}

fn fail_video_import_from_worker(
    engine: &mut VideoPlaybackEngine,
    inner: &Arc<Mutex<PlayerInner>>,
    message: String,
) {
    engine.pause();
    trace::diagnostic(
        serde_json::json!({
            "event": "video_frame_import_fatal",
            "reason": message.as_str(),
        })
        .to_string(),
    );
    emit_from_worker(inner, PlayerEvent::Error(PlayerError::Renderer(message)));
    set_state_from_worker(inner, PlayerState::Error);
}

fn sync_track_state_from_worker(inner: &Arc<Mutex<PlayerInner>>, engine: &VideoPlaybackEngine) {
    let tracks = engine.info().tracks.clone();
    let selection = engine.track_selection();
    let mut events = Vec::new();
    {
        let mut inner = inner.lock().expect("player mutex poisoned");
        if inner.tracks != tracks {
            inner.tracks = tracks.clone();
            events.push(PlayerEvent::TracksChanged(tracks));
        }
        if inner.track_selection != selection {
            inner.track_selection = selection;
            events.push(PlayerEvent::TrackSelectionChanged(selection));
        }
    }
    for event in events {
        emit_from_worker(inner, event);
    }
}

fn publish_natural_eof_from_worker(
    engine: &VideoPlaybackEngine,
    inner: &Arc<Mutex<PlayerInner>>,
    playback_generation: u64,
    eof_published: &mut bool,
) -> bool {
    if engine.state() != PlaybackRunState::Ended {
        *eof_published = false;
        return false;
    }
    let final_position = engine
        .info()
        .duration
        .unwrap_or_else(|| engine.media_time());
    publish_natural_eof_events_from_worker(
        inner,
        playback_generation,
        final_position,
        eof_published,
    );
    true
}

fn publish_natural_eof_events_from_worker(
    inner: &Arc<Mutex<PlayerInner>>,
    playback_generation: u64,
    final_position: Duration,
    eof_published: &mut bool,
) {
    if *eof_published {
        return;
    }
    let mut inner = inner.lock().expect("player mutex poisoned");
    let generation = playback_generation.max(1);
    if worker_generation_is_stale(generation, inner.playback_generation) {
        trace::log(format!(
            "[erika-clock-trace] stage=worker_eof_skip_stale media={} worker_gen={} shared_gen={}",
            trace::duration_label(Some(final_position)),
            generation,
            inner.playback_generation,
        ));
        return;
    }
    inner.playback_clock = PlaybackClock::paused_at(final_position);
    inner.ended = true;
    inner.subscribers.retain(|sender| {
        sender
            .send(PlayerEvent::PositionChanged(final_position))
            .is_ok()
    });
    let previous = inner.state;
    inner.state = PlayerState::Stopped;
    if previous != PlayerState::Stopped {
        inner.subscribers.retain(|sender| {
            sender
                .send(PlayerEvent::StateChanged(PlayerState::Stopped))
                .is_ok()
        });
    }
    // A stop asked for is `Stopped` too; this says the media ended.
    inner
        .subscribers
        .retain(|sender| sender.send(PlayerEvent::Ended).is_ok());
    *eof_published = true;
}

fn sync_playback_clock_from_worker(
    engine: &VideoPlaybackEngine,
    inner: &Arc<Mutex<PlayerInner>>,
    playback_generation: u64,
    stage: &'static str,
    last_executed_sequence: u64,
    last_worker_clock: &mut Option<(Duration, u64)>,
) {
    let clock = engine.clock();
    let now = Instant::now();
    let media_time = clock.media_time_at(now);
    let mut inner = inner.lock().expect("player mutex poisoned");
    let shared_before = inner.playback_clock.media_time_at(now);
    let generation = playback_generation.max(1);
    let shared_generation = inner.playback_generation.max(1);
    if worker_clock_generation_is_stale(shared_generation, generation) {
        trace::log(format!(
            "[erika-clock-trace] stage=worker_sync:{stage} action=skip_stale_worker media={} shared_before={} worker_gen={} shared_gen={}",
            trace::duration_label(Some(media_time)),
            trace::duration_label(Some(shared_before)),
            generation,
            shared_generation,
        ));
        return;
    }
    if inner.state == PlayerState::Paused
        && inner.playback_command_sequence > last_executed_sequence
    {
        // A pause intent bumped the command sequence and parked the shared
        // clock, but the worker has not executed the Pause command yet: the
        // engine clock is still running, and publishing it would overwrite
        // the parked clock and show a Paused player whose position advances
        // until the worker catches up. Seek and stop intents bump the
        // generation too, so this sequence check only gates pause.
        trace::log(format!(
            "[erika-clock-trace] stage=worker_sync:{stage} action=skip_pause_intent media={} shared_before={} worker_seq={} shared_seq={}",
            trace::duration_label(Some(media_time)),
            trace::duration_label(Some(shared_before)),
            last_executed_sequence,
            inner.playback_command_sequence,
        ));
        return;
    }
    let worker_back = last_worker_clock.is_some_and(|(last_time, last_generation)| {
        last_generation == generation && trace::duration_regressed(media_time, last_time)
    });
    let shared_back = trace::duration_regressed(media_time, shared_before);
    let generation_changed =
        last_worker_clock.is_some_and(|(_, last_generation)| last_generation != generation);
    if worker_back || shared_back || generation_changed {
        trace::log(format!(
            "[erika-clock-trace] stage=worker_sync:{stage} media={} shared_before={} gen={} last_worker={} last_worker_gen={} flags=worker_back:{} shared_back:{} gen_change:{}",
            trace::duration_label(Some(media_time)),
            trace::duration_label(Some(shared_before)),
            generation,
            trace::duration_label(last_worker_clock.map(|(time, _)| time)),
            last_worker_clock
                .map(|(_, generation)| generation)
                .unwrap_or(0),
            worker_back,
            shared_back,
            generation_changed,
        ));
    }
    // Publish the clock, not a reading of it: consumers evaluate it at their
    // own tick so a slow worker loop cannot quantize playback position.
    inner.playback_clock = clock;
    inner.playback_generation = generation;
    *last_worker_clock = Some((media_time, generation));
}

fn worker_clock_generation_is_stale(shared_generation: u64, worker_generation: u64) -> bool {
    shared_generation.max(1) > worker_generation.max(1)
}

fn playback_poll_interval(state: PlaybackRunState, paused_seek_frame_pending: bool) -> Duration {
    if state == PlaybackRunState::Playing
        || (state == PlaybackRunState::Paused && paused_seek_frame_pending)
    {
        Duration::from_millis(2)
    } else {
        Duration::from_millis(50)
    }
}

fn playback_command_sequence_is_current(inner: &Arc<Mutex<PlayerInner>>, sequence: u64) -> bool {
    inner
        .lock()
        .expect("player mutex poisoned")
        .playback_command_sequence
        == sequence
}

fn begin_playback_command_execution(sequence: u64, last_executed_sequence: &mut u64) -> bool {
    if sequence <= *last_executed_sequence {
        return false;
    }
    *last_executed_sequence = sequence;
    true
}

fn commit_playback_command_intent(
    inner: &Arc<Mutex<PlayerInner>>,
    sequence: u64,
    playback_generation: Option<u64>,
    position: Option<Duration>,
    state: Option<PlayerState>,
) -> bool {
    let mut inner = inner.lock().expect("player mutex poisoned");
    if inner.playback_command_sequence != sequence {
        return false;
    }
    // Position-only commits are published synchronously when Seek is queued.
    // They must not clear a resume intent that is still owned by the worker.
    if state.is_some() {
        inner.pending_play_sequence = None;
    }
    if let Some(playback_generation) = playback_generation {
        inner.playback_generation = inner.playback_generation.max(playback_generation.max(1));
    }
    inner.ended = false;
    if let Some(position) = position {
        inner.playback_clock = PlaybackClock::paused_at(position);
        inner
            .subscribers
            .retain(|sender| sender.send(PlayerEvent::PositionChanged(position)).is_ok());
    }
    if let Some(state) = state {
        if state == PlayerState::Paused {
            // Publish the paused state and its parked clock under the same
            // lock so snapshots cannot observe a paused player that still
            // advances until the worker's next polling turn.
            inner.playback_clock.pause(Instant::now());
        }
        let previous = inner.state;
        inner.state = state;
        if previous != state {
            inner
                .subscribers
                .retain(|sender| sender.send(PlayerEvent::StateChanged(state)).is_ok());
        }
    }
    true
}

fn worker_generation_is_stale(worker_generation: u64, shared_generation: u64) -> bool {
    worker_generation.max(1) < shared_generation.max(1)
}

fn video_import_feedback_is_current(
    failure_generation: u64,
    worker_generation: u64,
    shared_generation: u64,
) -> bool {
    failure_generation == worker_generation && failure_generation == shared_generation
}

fn shared_playback_generation(inner: &Arc<Mutex<PlayerInner>>) -> u64 {
    inner
        .lock()
        .expect("player mutex poisoned")
        .playback_generation
        .max(1)
}

fn publish_playback_generation_from_worker(
    inner: &Arc<Mutex<PlayerInner>>,
    playback_generation: u64,
) {
    let mut inner = inner.lock().expect("player mutex poisoned");
    inner.playback_generation = inner.playback_generation.max(playback_generation.max(1));
}

#[cfg(test)]
fn set_state_from_worker_for_generation(
    inner: &Arc<Mutex<PlayerInner>>,
    playback_generation: u64,
    next: PlayerState,
) -> bool {
    let mut inner = inner.lock().expect("player mutex poisoned");
    if worker_generation_is_stale(playback_generation, inner.playback_generation) {
        return false;
    }
    let previous = inner.state;
    inner.state = next;
    if previous != next {
        inner
            .subscribers
            .retain(|sender| sender.send(PlayerEvent::StateChanged(next)).is_ok());
    }
    true
}

fn enqueue_frame_output_resume_after_timeout(commands: &Sender<PlaybackCommand>) {
    let (reply, response) = bounded(1);
    drop(response);
    match commands.try_send(PlaybackCommand::SetFrameOutputQuiesced {
        quiesced: false,
        reply,
    }) {
        Ok(()) => trace::diagnostic(
            serde_json::json!({
                "event": "player_frame_output",
                "stage": "timeout_resume_enqueued",
                "action": "resume",
                "reason": "a timed-out quiesce may still be pending in the playback command FIFO",
            })
            .to_string(),
        ),
        Err(TrySendError::Full(_)) => trace::diagnostic(
            serde_json::json!({
                "event": "player_frame_output",
                "stage": "timeout_resume_enqueue_failed",
                "action": "resume",
                "reason": "playback command queue is full",
            })
            .to_string(),
        ),
        Err(TrySendError::Disconnected(_)) => trace::diagnostic(
            serde_json::json!({
                "event": "player_frame_output",
                "stage": "timeout_resume_enqueue_failed",
                "action": "resume",
                "reason": "playback command queue is disconnected",
            })
            .to_string(),
        ),
    }
}

fn enqueue_video_decode_resume_after_timeout(commands: &Sender<PlaybackCommand>) {
    let (reply, response) = bounded(1);
    drop(response);
    let _ = commands.try_send(PlaybackCommand::SetVideoDecodeSuspended {
        suspended: false,
        reply,
    });
}

fn set_state_from_worker(inner: &Arc<Mutex<PlayerInner>>, next: PlayerState) {
    let previous = {
        let mut inner = inner.lock().expect("player mutex poisoned");
        let previous = inner.state;
        inner.state = next;
        previous
    };
    if previous != next {
        emit_from_worker(inner, PlayerEvent::StateChanged(next));
    }
}

fn emit_from_worker(inner: &Arc<Mutex<PlayerInner>>, event: PlayerEvent) {
    let mut inner = inner.lock().expect("player mutex poisoned");
    inner
        .subscribers
        .retain(|sender| sender.send(event.clone()).is_ok());
}

fn emit_from_worker_for_generation(
    inner: &Arc<Mutex<PlayerInner>>,
    playback_generation: u64,
    event: PlayerEvent,
) -> bool {
    let mut inner = inner.lock().expect("player mutex poisoned");
    if worker_generation_is_stale(playback_generation, inner.playback_generation) {
        return false;
    }
    inner
        .subscribers
        .retain(|sender| sender.send(event.clone()).is_ok());
    true
}

fn emit_video_frame_from_worker(inner: &Arc<Mutex<PlayerInner>>, frame: PlayerVideoFrame) {
    let mut inner = inner.lock().expect("player mutex poisoned");
    let Some(sender) = inner.video_frame_sender.as_ref() else {
        return;
    };
    match sender.try_send(frame) {
        Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => inner.video_frame_sender = None,
    }
}

fn audio_frame_output_is_active(inner: &Arc<Mutex<PlayerInner>>) -> bool {
    inner
        .lock()
        .expect("player mutex poisoned")
        .audio_frame_sender
        .is_some()
}

fn audio_frame_channel_metrics(inner: &Arc<Mutex<PlayerInner>>) -> (usize, usize) {
    let inner = inner.lock().expect("player mutex poisoned");
    let Some(sender) = inner.audio_frame_sender.as_ref() else {
        return (0, 0);
    };
    (sender.len(), sender.capacity().unwrap_or(0))
}

enum AudioFrameEmitResult {
    Sent,
    Full(PcmAudioFrame),
    Disconnected(PcmAudioFrame),
}

fn try_emit_audio_frame_from_worker(
    inner: &Arc<Mutex<PlayerInner>>,
    frame: PlayerAudioFrame,
) -> AudioFrameEmitResult {
    let mut inner = inner.lock().expect("player mutex poisoned");
    let result = {
        let Some(sender) = inner.audio_frame_sender.as_ref() else {
            return AudioFrameEmitResult::Disconnected(frame.frame);
        };
        sender.try_send(frame)
    };
    match result {
        Ok(()) => AudioFrameEmitResult::Sent,
        Err(TrySendError::Full(frame)) => AudioFrameEmitResult::Full(frame.frame),
        Err(TrySendError::Disconnected(frame)) => {
            inner.audio_frame_sender = None;
            AudioFrameEmitResult::Disconnected(frame.frame)
        }
    }
}

#[cfg(test)]
fn emit_audio_frame_from_worker(inner: &Arc<Mutex<PlayerInner>>, frame: PlayerAudioFrame) -> bool {
    matches!(
        try_emit_audio_frame_from_worker(inner, frame),
        AudioFrameEmitResult::Sent
    )
}

fn emit_subtitle_frame_from_worker(inner: &Arc<Mutex<PlayerInner>>, frame: PlayerSubtitleFrame) {
    let mut inner = inner.lock().expect("player mutex poisoned");
    let (result, capacity) = {
        let Some(sender) = inner.subtitle_frame_sender.as_ref() else {
            return;
        };
        (
            sender.try_send(frame),
            sender
                .capacity()
                .unwrap_or(SUBTITLE_FRAME_HANDOFF_MIN_CAPACITY),
        )
    };
    match result {
        Ok(()) => {}
        Err(TrySendError::Full(frame)) => {
            inner.subtitle_frame_backpressure_drops =
                inner.subtitle_frame_backpressure_drops.saturating_add(1);
            let dropped = inner.subtitle_frame_backpressure_drops;
            if dropped == 1 || dropped.is_power_of_two() {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "subtitle_frame_handoff",
                        "stage": "backlog_limit",
                        "capacity": capacity,
                        "droppedFrames": dropped,
                        "trackId": frame.frame.track_id,
                        "generation": frame.generation,
                        "reason": "subtitle subscriber is not draining; dropping newest frame to bound memory",
                    })
                    .to_string(),
                );
            }
        }
        Err(TrySendError::Disconnected(_)) => inner.subtitle_frame_sender = None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_request_normalizes_http_read_ahead_defaults() {
        assert_eq!(
            MediaRequest::new("https://example.invalid/video.mp4")
                .with_http_read_ahead_bytes(8 * 1024 * 1024)
                .http_read_ahead_bytes,
            Some(8 * 1024 * 1024)
        );
        assert_eq!(
            MediaRequest::new("https://example.invalid/video.mp4")
                .with_http_read_ahead_bytes(0)
                .http_read_ahead_bytes,
            None
        );
    }

    #[test]
    fn surface_metrics_keep_exact_physical_extent() {
        let metrics = SurfaceMetrics::new(1081, 607, 2.625);

        assert_eq!(metrics.physical_size(), (1081, 607));
        assert_eq!(metrics.content_scale, 2.625);
    }

    #[test]
    fn surface_metrics_preserve_low_density_and_sanitize_invalid_scale() {
        assert_eq!(SurfaceMetrics::new(320, 240, 0.75).content_scale, 0.75);
        assert_eq!(SurfaceMetrics::new(320, 240, f64::NAN).content_scale, 1.0);
        assert_eq!(SurfaceMetrics::new(320, 240, 0.0).content_scale, 1.0);
        assert_eq!(SurfaceMetrics::new(320, 240, -2.0).content_scale, 1.0);
    }

    #[test]
    fn surface_metrics_resize_never_derives_extent_from_scale() {
        let metrics = SurfaceMetrics::new(1080, 2400, 2.625);

        assert_eq!(metrics.physical_size(), (1080, 2400));
        assert_eq!(
            metrics.resized(1440, 3120, 3.5).physical_size(),
            (1440, 3120)
        );
    }

    #[derive(Default)]
    struct TransitionFrameProbe {
        clears: u32,
    }

    impl RendererBackend for TransitionFrameProbe {
        fn attach_surface(&mut self, _surface: PlatformSurface) -> Result<()> {
            Ok(())
        }

        fn detach_surface(&mut self) -> Result<()> {
            Ok(())
        }

        fn resize_surface(&mut self, _metrics: SurfaceMetrics) -> Result<()> {
            Ok(())
        }

        fn render_test_frame(&mut self, _time_seconds: f64) -> Result<()> {
            Ok(())
        }

        fn upload_player_frame(&mut self, _frame: &PlayerVideoFrame) -> Result<()> {
            Ok(())
        }

        fn clear_current_frame(&mut self) -> Result<()> {
            self.clears += 1;
            Ok(())
        }

        fn render_current_frame(&mut self, _context: RenderFrameContext<'_>) -> Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn native_track_transition_keeps_frame_while_decoder_transition_clears() {
        let mut renderer = TransitionFrameProbe::default();

        RendererBackend::preserve_current_frame_for_track_transition(&mut renderer).unwrap();
        assert_eq!(renderer.clears, 0);

        RendererBackend::preserve_current_frame_for_transition(&mut renderer).unwrap();
        assert_eq!(renderer.clears, 1);
    }

    fn install_test_runtime(player: &Player, capacity: usize) -> Receiver<PlaybackCommand> {
        let (commands, receiver) = bounded(capacity.max(1));
        player
            .lifecycle
            .lock()
            .expect("player lifecycle mutex poisoned")
            .playback = Some(PlaybackRuntime {
            commands,
            worker: None,
        });
        receiver
    }

    fn play_with_test_commit(
        player: &Player,
        commands: &Receiver<PlaybackCommand>,
        restarted_from_eof: bool,
    ) -> (u64, u64) {
        player.play().expect("queue play command");
        let (sequence, generation) = match commands
            .recv_timeout(Duration::from_secs(1))
            .expect("play command")
        {
            PlaybackCommand::Play {
                sequence,
                generation,
            } => (sequence, generation),
            _ => panic!("expected play command"),
        };
        let worker_generation = if restarted_from_eof {
            generation.saturating_add(1).max(1)
        } else {
            generation.max(1)
        };
        assert!(commit_playback_command_intent(
            &player.inner,
            sequence,
            Some(worker_generation),
            restarted_from_eof.then_some(Duration::ZERO),
            Some(PlayerState::Playing),
        ));
        (sequence, generation)
    }

    #[test]
    fn worker_sync_respects_a_pause_intent_the_worker_has_not_executed() {
        let player = Player::new(PlayerConfig::default());
        let worker_generation = 3;
        let commands = install_fake_playback(
            &player,
            PlayerState::Playing,
            Duration::ZERO,
            worker_generation,
            false,
        );

        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/playback/playback-fixture.mkv");
        let mut engine = VideoPlaybackEngine::open(
            &MediaRequest::new(path.to_string_lossy().as_ref()),
            PlaybackSessionConfig::default(),
        )
        .unwrap();
        engine
            .seek_with_playback_intent(Duration::from_millis(5_125), false)
            .unwrap();
        engine.play().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if engine.tick().unwrap().is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the asynchronous seek preview"
            );
            std::thread::yield_now();
        }
        let engine_clock = engine.clock();
        assert!(
            engine_clock.is_running(),
            "precondition: worker clock is running"
        );

        player.pause().expect("queue pause command");
        let pause_sequence = match commands
            .recv_timeout(Duration::from_secs(1))
            .expect("pause command")
        {
            PlaybackCommand::Pause { sequence, .. } => sequence,
            _ => panic!("expected pause command"),
        };
        let parked_clock = player.inner.lock().unwrap().playback_clock.clone();
        assert!(!parked_clock.is_running());
        // The intent bumped the command sequence past everything the worker
        // has executed, parked the shared clock, and published Paused.
        assert_eq!(pause_sequence, 1);
        assert_eq!(player.state(), PlayerState::Paused);
        assert_ne!(
            engine_clock,
            player.inner.lock().unwrap().playback_clock,
            "precondition: the engine clock must differ from the intent-parked clock"
        );

        // The worker has executed nothing when it syncs: the pause intent
        // owns the shared clock, and publishing the still-running engine
        // clock would show a Paused player whose position advances.
        let mut last_worker_clock = None;
        sync_playback_clock_from_worker(
            &engine,
            &player.inner,
            worker_generation,
            "test",
            0,
            &mut last_worker_clock,
        );
        assert_ne!(
            player.inner.lock().unwrap().playback_clock,
            engine_clock,
            "a pending pause intent must not be overwritten by the worker clock"
        );

        assert_eq!(player.inner.lock().unwrap().playback_clock, parked_clock);
        assert!(last_worker_clock.is_none());

        // Executing the pause command releases the sync: the engine clock is
        // authoritative again.
        engine.pause();
        let engine_clock = engine.clock();
        assert!(!engine_clock.is_running());
        sync_playback_clock_from_worker(
            &engine,
            &player.inner,
            worker_generation,
            "test",
            pause_sequence,
            &mut last_worker_clock,
        );
        assert_eq!(
            player.inner.lock().unwrap().playback_clock,
            engine_clock,
            "after the pause command executes, the worker clock publishes again"
        );
    }

    fn install_fake_playback(
        player: &Player,
        state: PlayerState,
        media_time: Duration,
        generation: u64,
        ended: bool,
    ) -> Receiver<PlaybackCommand> {
        let receiver = install_test_runtime(player, 16);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = state;
            inner.playback_clock = PlaybackClock::paused_at(media_time);
            inner.playback_generation = generation;
            inner.ended = ended;
        }
        receiver
    }

    fn install_disconnected_playback(
        player: &Player,
        state: PlayerState,
        media_time: Duration,
        generation: u64,
        ended: bool,
    ) {
        let receiver = install_test_runtime(player, 1);
        drop(receiver);
        let mut inner = player.inner.lock().expect("player mutex poisoned");
        inner.state = state;
        inner.playback_clock = PlaybackClock::paused_at(media_time);
        inner.playback_generation = generation;
        inner.ended = ended;
    }

    fn test_audio_frame(generation: u64) -> PlayerAudioFrame {
        PlayerAudioFrame {
            frame: PcmAudioFrame {
                format: Default::default(),
                pts: Some(Duration::from_millis(generation)),
                frames: 1,
                samples: vec![0.0, 0.0],
            },
            generation,
        }
    }

    #[test]
    fn frame_output_barrier_and_runtime_owner_do_not_form_an_inner_cycle() {
        let player = Player::new(PlayerConfig::default());
        let inner_weak = Arc::downgrade(&player.inner);
        let lifecycle_weak = Arc::downgrade(&player.lifecycle);
        let worker_inner = Arc::clone(&player.inner);
        let (commands, receiver) = bounded(4);
        let worker = std::thread::spawn(move || {
            while let Ok(command) = receiver.recv() {
                match command {
                    PlaybackCommand::SetFrameOutputQuiesced { reply, .. } => {
                        let _ = reply.send(());
                    }
                    PlaybackCommand::Shutdown => break,
                    _ => {}
                }
            }
            drop(worker_inner);
        });
        player
            .lifecycle
            .lock()
            .expect("player lifecycle mutex poisoned")
            .playback = Some(PlaybackRuntime {
            commands,
            worker: Some(worker),
        });

        assert!(player.set_frame_output_quiesced(true).unwrap());
        assert!(player.set_frame_output_quiesced(false).unwrap());

        drop(player);
        assert!(lifecycle_weak.upgrade().is_none());
        assert!(inner_weak.upgrade().is_none());
    }

    #[test]
    fn frame_output_barrier_is_a_noop_without_open_media() {
        let player = Player::new(PlayerConfig::default());
        assert!(!player.set_frame_output_quiesced(true).unwrap());
    }

    #[test]
    fn selecting_no_subtitle_without_open_media_succeeds_and_a_track_does_not() {
        let player = Player::new(PlayerConfig::default());
        assert!(!player.has_open_media());
        player.select_subtitle_track(None).unwrap();
        let error = player.select_subtitle_track(Some(3)).unwrap_err();
        assert!(error.to_string().contains("no media is open"), "{error}");
        let receiver = install_test_runtime(&player, 2);
        assert!(player.has_open_media());
        player.select_subtitle_track(None).unwrap();
        assert!(matches!(
            receiver.recv().unwrap(),
            PlaybackCommand::SelectSubtitleTrack(None)
        ));
    }

    #[test]
    fn video_decode_mode_is_a_noop_without_open_media() {
        let player = Player::new(PlayerConfig::default());
        assert!(!player.set_video_decode_suspended(true).unwrap());
    }

    #[test]
    fn video_decode_mode_uses_an_acknowledged_worker_command() {
        let player = Player::new(PlayerConfig::default());
        let receiver = install_test_runtime(&player, 2);
        let worker = thread::spawn(move || match receiver.recv().unwrap() {
            PlaybackCommand::SetVideoDecodeSuspended {
                suspended: true,
                reply,
            } => reply.send(Ok(())).unwrap(),
            _ => panic!("unexpected playback command"),
        });

        assert!(player.set_video_decode_suspended(true).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn video_decode_mode_reports_an_unresponsive_worker() {
        let player = Player::new(PlayerConfig::default());
        let receiver = install_test_runtime(&player, 2);
        let started = Instant::now();

        let error = player
            .set_video_decode_suspended_with_timeout(true, Duration::from_millis(25))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("did not acknowledge video decode mode")
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            PlaybackCommand::SetVideoDecodeSuspended {
                suspended: true,
                ..
            }
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            PlaybackCommand::SetVideoDecodeSuspended {
                suspended: false,
                ..
            }
        ));
        drop(receiver);
    }

    #[test]
    fn frame_output_barrier_reports_an_unresponsive_worker() {
        let player = Player::new(PlayerConfig::default());
        let receiver = install_test_runtime(&player, 2);
        let started = Instant::now();

        let error = player
            .set_frame_output_quiesced_with_timeout(true, Duration::from_millis(25))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("did not acknowledge frame output quiesce")
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            PlaybackCommand::SetFrameOutputQuiesced { quiesced: true, .. }
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            PlaybackCommand::SetFrameOutputQuiesced {
                quiesced: false,
                ..
            }
        ));
        drop(receiver);
    }

    #[test]
    fn stale_worker_generation_cannot_publish_position_or_stopped_state() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Playing;
            inner.playback_clock = PlaybackClock::paused_at(Duration::from_secs(5));
            inner.playback_generation = 4;
        }

        assert!(worker_generation_is_stale(3, 4));
        assert!(!emit_from_worker_for_generation(
            &player.inner,
            3,
            PlayerEvent::PositionChanged(Duration::from_millis(3_400))
        ));
        assert!(!set_state_from_worker_for_generation(
            &player.inner,
            3,
            PlayerState::Stopped
        ));

        assert_eq!(player.state(), PlayerState::Playing);
        assert_eq!(player.current_media_time(), Duration::from_secs(5));
        assert!(events.try_recv().is_err());
        assert!(!worker_generation_is_stale(4, 4));
    }

    #[test]
    fn stale_video_import_feedback_cannot_downgrade_a_new_decoder_generation() {
        assert!(video_import_feedback_is_current(7, 7, 7));
        assert!(!video_import_feedback_is_current(6, 7, 7));
        assert!(!video_import_feedback_is_current(7, 7, 8));
        assert!(!video_import_feedback_is_current(8, 7, 7));
    }

    #[test]
    fn newer_command_sequence_rejects_older_state_and_position_commit() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Stopped;
            inner.playback_clock = PlaybackClock::paused_at(Duration::from_secs(9));
            inner.playback_command_sequence = 7;
        }

        assert!(!commit_playback_command_intent(
            &player.inner,
            6,
            None,
            Some(Duration::ZERO),
            Some(PlayerState::Playing),
        ));
        assert_eq!(player.state(), PlayerState::Stopped);
        assert_eq!(player.current_media_time(), Duration::from_secs(9));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn fifo_commands_execute_even_when_a_newer_sequence_is_already_reserved() {
        let player = Player::new(PlayerConfig::default());
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.playback_command_sequence = 2;
        }
        let mut last_executed = 0;

        assert!(begin_playback_command_execution(1, &mut last_executed));
        assert_eq!(last_executed, 1);
        assert!(!commit_playback_command_intent(
            &player.inner,
            1,
            None,
            Some(Duration::from_secs(3)),
            Some(PlayerState::Playing),
        ));
        assert!(begin_playback_command_execution(2, &mut last_executed));
        assert_eq!(last_executed, 2);
    }

    #[test]
    fn lower_sequence_arriving_after_newer_execution_is_skipped() {
        let mut last_executed = 0;

        assert!(begin_playback_command_execution(2, &mut last_executed));
        assert!(!begin_playback_command_execution(1, &mut last_executed));
        assert_eq!(last_executed, 2);
    }

    #[test]
    fn seek_while_playing_carries_resume_intent_past_stale_eof() {
        let player = Player::new(PlayerConfig::default());
        let receiver = install_test_runtime(&player, 2);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Playing;
        }
        let stale_generation = player.playback_generation();

        player.seek(Duration::from_secs(4)).unwrap();

        let (sequence, generation, resume_after_seek) = match receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("seek command")
        {
            PlaybackCommand::Seek {
                position,
                sequence,
                generation,
                resume_after_seek,
            } => {
                assert_eq!(position, Duration::from_secs(4));
                (sequence, generation, resume_after_seek)
            }
            _ => panic!("expected seek command"),
        };
        assert!(resume_after_seek);
        assert!(generation > stale_generation);
        assert!(playback_command_sequence_is_current(
            &player.inner,
            sequence
        ));
        assert!(!set_state_from_worker_for_generation(
            &player.inner,
            stale_generation,
            PlayerState::Stopped,
        ));
        assert_eq!(player.state(), PlayerState::Playing);
    }

    #[test]
    fn stop_sequence_supersedes_a_pending_restart_commit() {
        let player = Player::new(PlayerConfig::default());
        let receiver = install_test_runtime(&player, 3);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Stopped;
            inner.playback_clock = PlaybackClock::paused_at(Duration::from_secs(11));
        }
        player.play().unwrap();
        let play_sequence = match receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("play command")
        {
            PlaybackCommand::Play { sequence, .. } => sequence,
            _ => panic!("expected play command"),
        };

        player.stop().unwrap();
        let stop_sequence = match receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("stop command")
        {
            PlaybackCommand::Stop { sequence, .. } => sequence,
            _ => panic!("expected stop command"),
        };
        assert!(stop_sequence > play_sequence);
        assert!(!commit_playback_command_intent(
            &player.inner,
            play_sequence,
            None,
            Some(Duration::ZERO),
            Some(PlayerState::Playing),
        ));
        assert_eq!(player.state(), PlayerState::Stopped);
        assert_eq!(player.current_media_time(), Duration::ZERO);
    }

    #[test]
    fn player_play_returns_after_queueing_without_false_playing_state() {
        let player = Player::new(PlayerConfig::default());
        let receiver = install_test_runtime(&player, 2);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Stopped;
        }
        player.play().unwrap();

        let sequence = match receiver.recv_timeout(Duration::from_secs(1)).unwrap() {
            PlaybackCommand::Play { sequence, .. } => sequence,
            _ => panic!("expected play command"),
        };
        assert_eq!(player.state(), PlayerState::Stopped);

        assert!(commit_playback_command_intent(
            &player.inner,
            sequence,
            Some(1),
            None,
            Some(PlayerState::Playing),
        ));
        assert_eq!(player.state(), PlayerState::Playing);
    }

    #[test]
    fn player_stop_after_error_then_failed_restart_does_not_false_publish_playing() {
        let player = Player::new(PlayerConfig::default());
        let receiver = install_test_runtime(&player, 2);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Error;
            inner.playback_clock = PlaybackClock::paused_at(Duration::from_secs(7));
        }

        player.stop().unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            PlaybackCommand::Stop { .. }
        ));
        assert_eq!(player.state(), PlayerState::Stopped);
        assert_eq!(player.current_media_time(), Duration::ZERO);

        player.play().unwrap();

        match receiver.recv_timeout(Duration::from_secs(1)).unwrap() {
            PlaybackCommand::Play { .. } => {}
            _ => panic!("expected play command"),
        }
        let message = "video decoder unavailable: all fallback routes failed".to_string();
        set_state_from_worker(&player.inner, PlayerState::Error);
        player.emit(PlayerEvent::Error(PlayerError::Playback(message)));

        assert_eq!(player.state(), PlayerState::Error);
        assert_ne!(player.state(), PlayerState::Playing);
        assert_eq!(player.current_media_time(), Duration::ZERO);
    }

    #[test]
    fn player_play_pause_emits_state_events_after_ready() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();

        player.transition(PlayerState::Ready).unwrap();
        assert!(matches!(
            player.play().unwrap_err(),
            PlayerError::Playback(_)
        ));

        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::StateChanged(PlayerState::Ready)
        );
    }

    #[test]
    fn pause_publishes_intent_before_worker_position() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let commands = install_test_runtime(&player, 1);
        let anchor = Instant::now();
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Playing;
            inner.playback_clock = PlaybackClock::running_at(Duration::from_secs(10), anchor);
        }

        player.pause().unwrap();

        let sequence = match commands.recv_timeout(Duration::from_secs(1)).unwrap() {
            PlaybackCommand::Pause { sequence, .. } => sequence,
            _ => panic!("expected pause command"),
        };
        assert_eq!(player.state(), PlayerState::Paused);
        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::StateChanged(PlayerState::Paused)
        );

        let position = Duration::from_millis(9_250);
        assert!(commit_playback_command_intent(
            &player.inner,
            sequence,
            None,
            Some(position),
            Some(PlayerState::Paused),
        ));
        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::PositionChanged(position)
        );
        assert!(events.try_recv().is_err());
        assert_eq!(player.current_media_time(), position);
        assert_eq!(player.state(), PlayerState::Paused);
        let snapshot = player.playback_snapshot();
        assert_eq!(snapshot.state, PlayerState::Paused);
        assert!(!snapshot.clock.is_running());
        assert_eq!(
            snapshot.media_time_at(anchor + Duration::from_secs(60)),
            snapshot.media_time()
        );
    }

    #[test]
    fn failed_pause_leaves_the_running_clock_unchanged() {
        let player = Player::new(PlayerConfig::default());
        let commands = install_test_runtime(&player, 1);
        drop(commands);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Playing;
            inner.playback_clock =
                PlaybackClock::running_at(Duration::from_secs(10), Instant::now());
        }

        assert!(matches!(player.pause(), Err(PlayerError::Playback(_))));

        let snapshot = player.playback_snapshot();
        assert_eq!(snapshot.state, PlayerState::Playing);
        assert!(snapshot.clock.is_running());
    }

    #[test]
    fn closed_player_rejects_commands() {
        let player = Player::new(PlayerConfig::default());
        player.close().unwrap();
        assert_eq!(player.play().unwrap_err(), PlayerError::Closed);
    }

    #[test]
    fn closed_player_allows_idempotent_surface_detach() {
        let player = Player::new(PlayerConfig::default());
        player
            .attach_surface(PlatformSurface::Metal(MetalSurfaceHandle::new(
                42, 1920, 1080, 2.0,
            )))
            .unwrap();
        player.close().unwrap();

        player.detach_surface().unwrap();
        player.detach_surface().unwrap();

        assert_eq!(player.state(), PlayerState::Closed);
        assert!(
            player
                .inner
                .lock()
                .expect("player mutex poisoned")
                .surface
                .is_none()
        );
    }

    #[test]
    fn attach_surface_emits_event() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let surface = PlatformSurface::Metal(MetalSurfaceHandle::new(42, 1920, 1080, 2.0));

        player.attach_surface(surface).unwrap();

        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::SurfaceAttached(surface)
        );
    }

    #[test]
    fn attach_wgpu_surface_emits_event() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let surface = PlatformSurface::Wgpu(WgpuSurfaceHandle::new(
            WgpuSurfaceKind::MacOsCaMetalLayer,
            42,
            0,
            1920,
            1080,
            2.0,
        ));

        player.attach_surface(surface).unwrap();

        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::SurfaceAttached(surface)
        );
    }

    #[test]
    fn attach_flutter_texture_surface_emits_event() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let surface = PlatformSurface::FlutterTexture(FlutterTextureHandle::new(
            FlutterTextureKind::MacOsTextureRegistrar,
            7,
            1280,
            720,
            2.0,
        ));

        player.attach_surface(surface).unwrap();

        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::SurfaceAttached(surface)
        );
    }

    #[test]
    fn subscribe_subtitle_frames_replaces_previous_sender() {
        let player = Player::new(PlayerConfig::default());
        let first = player.subscribe_subtitle_frames();
        let second = player.subscribe_subtitle_frames();
        let frame = PlayerSubtitleFrame {
            frame: crate::subtitle::DecodedSubtitleFrame::new(2, Some(Duration::ZERO), None),
            pts: Some(Duration::ZERO),
            media_time: Duration::ZERO,
            late_by: None,
            generation: 1,
        };

        emit_subtitle_frame_from_worker(&player.inner, frame);

        assert!(first.try_recv().is_err());
        assert!(second.try_recv().is_ok());
    }

    #[test]
    fn full_audio_frame_queue_cannot_block_quiesce_command_ack() {
        let player = Player::new(PlayerConfig {
            audio_frame_queue_capacity: 1,
            ..PlayerConfig::default()
        });
        let frames = player.subscribe_audio_frames();
        assert!(emit_audio_frame_from_worker(
            &player.inner,
            test_audio_frame(1),
        ));

        let worker_inner = Arc::clone(&player.inner);
        let (commands, receiver) = bounded(1);
        let (emit_result_sender, emit_result_receiver) = bounded(1);
        let worker = std::thread::spawn(move || {
            let emitted = emit_audio_frame_from_worker(&worker_inner, test_audio_frame(2));
            emit_result_sender.send(emitted).unwrap();
            match receiver.recv().unwrap() {
                PlaybackCommand::SetFrameOutputQuiesced { reply, .. } => reply.send(()).unwrap(),
                _ => panic!("expected frame-output quiesce command"),
            }
        });

        let (reply, response) = bounded(1);
        commands
            .send(PlaybackCommand::SetFrameOutputQuiesced {
                quiesced: true,
                reply,
            })
            .unwrap();
        let acknowledged = response.recv_timeout(Duration::from_secs(1));
        if acknowledged.is_err() {
            // Let a blocking implementation unwind so the regression test itself
            // never strands its worker thread after reporting the timeout.
            let _ = frames.recv_timeout(Duration::from_secs(1));
            let _ = response.recv_timeout(Duration::from_secs(1));
        }
        let emitted = emit_result_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        worker.join().unwrap();

        assert!(acknowledged.is_ok(), "full audio queue blocked quiesce ACK");
        assert!(!emitted, "a full audio queue must apply backpressure");
    }

    #[test]
    fn disconnected_audio_frame_queue_is_removed_without_blocking() {
        let player = Player::new(PlayerConfig::default());
        let frames = player.subscribe_audio_frames();
        drop(frames);

        assert!(!emit_audio_frame_from_worker(
            &player.inner,
            test_audio_frame(1),
        ));
        assert!(
            player
                .inner
                .lock()
                .expect("player mutex poisoned")
                .audio_frame_sender
                .is_none()
        );
    }

    #[test]
    fn disconnected_full_audio_queue_is_detected_without_a_preflight_full_check() {
        let player = Player::new(PlayerConfig {
            audio_frame_queue_capacity: 1,
            ..PlayerConfig::default()
        });
        let frames = player.subscribe_audio_frames();
        assert!(emit_audio_frame_from_worker(
            &player.inner,
            test_audio_frame(1),
        ));
        drop(frames);

        assert!(matches!(
            try_emit_audio_frame_from_worker(&player.inner, test_audio_frame(2)),
            AudioFrameEmitResult::Disconnected(_)
        ));
        assert!(
            player
                .inner
                .lock()
                .expect("player mutex poisoned")
                .audio_frame_sender
                .is_none()
        );
    }

    #[test]
    fn subtitle_event_handoff_does_not_drop_when_configured_capacity_is_exceeded() {
        let player = Player::new(PlayerConfig {
            subtitle_frame_queue_capacity: 1,
            ..PlayerConfig::default()
        });
        let frames = player.subscribe_subtitle_frames();

        for index in 0..64 {
            emit_subtitle_frame_from_worker(
                &player.inner,
                PlayerSubtitleFrame {
                    frame: crate::subtitle::DecodedSubtitleFrame::new(
                        2,
                        Some(Duration::from_millis(index)),
                        Some(Duration::from_millis(index + 1)),
                    ),
                    pts: Some(Duration::from_millis(index)),
                    media_time: Duration::from_millis(index),
                    late_by: None,
                    generation: 1,
                },
            );
        }

        assert_eq!(frames.try_iter().count(), 64);
    }

    #[test]
    fn subtitle_event_handoff_bounds_a_stalled_subscriber() {
        let player = Player::new(PlayerConfig {
            subtitle_frame_queue_capacity: 1,
            ..PlayerConfig::default()
        });
        let frames = player.subscribe_subtitle_frames();

        for index in 0..=SUBTITLE_FRAME_HANDOFF_MIN_CAPACITY {
            emit_subtitle_frame_from_worker(
                &player.inner,
                PlayerSubtitleFrame {
                    frame: crate::subtitle::DecodedSubtitleFrame::new(
                        2,
                        Some(Duration::from_millis(index as u64)),
                        Some(Duration::from_millis(index as u64 + 1)),
                    ),
                    pts: Some(Duration::from_millis(index as u64)),
                    media_time: Duration::from_millis(index as u64),
                    late_by: None,
                    generation: 1,
                },
            );
        }

        assert_eq!(
            frames.try_iter().count(),
            SUBTITLE_FRAME_HANDOFF_MIN_CAPACITY
        );
        assert_eq!(
            player
                .inner
                .lock()
                .expect("player mutex poisoned")
                .subtitle_frame_backpressure_drops,
            1,
        );
    }

    #[test]
    fn player_track_cache_defaults_to_empty_selection() {
        let player = Player::new(PlayerConfig::default());

        assert!(player.tracks().is_empty());
        assert_eq!(player.track_selection(), TrackSelection::default());
    }

    #[test]
    fn queued_play_intent_is_visible_and_can_be_superseded_by_pause() {
        for initial_state in [
            PlayerState::Ready,
            PlayerState::Stopped,
            PlayerState::Paused,
        ] {
            let player = Player::new(PlayerConfig::default());
            let commands = install_fake_playback(
                &player,
                initial_state,
                Duration::ZERO,
                7,
                initial_state == PlayerState::Stopped,
            );

            player.play().unwrap();
            assert_eq!(player.state(), initial_state);
            assert_eq!(player.playback_intent_state(), PlayerState::Playing);

            player.pause().unwrap();
            assert_eq!(player.state(), PlayerState::Paused);
            assert_eq!(player.playback_intent_state(), PlayerState::Paused);

            let play_sequence = match commands.try_recv().expect("play command") {
                PlaybackCommand::Play { sequence, .. } => sequence,
                _ => panic!("expected play command"),
            };
            let pause_sequence = match commands.try_recv().expect("pause command") {
                PlaybackCommand::Pause {
                    sequence,
                    activate_before_pause,
                } => {
                    assert!(activate_before_pause);
                    sequence
                }
                _ => panic!("expected pause command"),
            };
            assert!(pause_sequence > play_sequence);
        }
    }

    #[test]
    fn media_request_start_position_treats_zero_as_the_start() {
        assert_eq!(
            MediaRequest::new("a.mkv")
                .with_start_position(Some(Duration::ZERO))
                .start_position,
            None
        );
        assert_eq!(
            MediaRequest::new("a.mkv")
                .with_start_position(Some(Duration::from_secs(5)))
                .start_position,
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn player_opens_at_the_start_position_when_env_is_set() {
        let Some(path) = std::env::var_os("ERIKA_TEST_SAMPLE") else {
            return;
        };
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        player
            .open(
                MediaRequest::new(path.to_string_lossy().into_owned())
                    .with_start_position(Some(Duration::from_secs(3))),
            )
            .unwrap();
        let mut said = None;
        loop {
            match events.recv_timeout(Duration::from_secs(5)).unwrap() {
                PlayerEvent::PositionChanged(position) => said = Some(position),
                PlayerEvent::StateChanged(PlayerState::Ready) => break,
                _ => {}
            }
        }
        assert_eq!(said, Some(Duration::from_secs(3)));
        assert_eq!(player.current_media_time(), Duration::from_secs(3));
        player.close().unwrap();
    }

    fn ended_playback_fixture_engine() -> Option<VideoPlaybackEngine> {
        let Some(path) = std::env::var_os("ERIKA_PLAYBACK_FIXTURE") else {
            return None;
        };
        let request = MediaRequest {
            uri: path.to_string_lossy().into_owned(),
            source_hint: MediaSourceHint::LocalFile,
            http_headers: Vec::new(),
            http_read_ahead_bytes: None,
            http_back_buffer_bytes: None,
            start_position: None,
        };
        let mut engine = VideoPlaybackEngine::open(
            &request,
            PlaybackSessionConfig {
                video_decode: crate::playback::VideoDecodePreference::Software,
                ..PlaybackSessionConfig::default()
            },
        )
        .expect("open playback fixture");
        engine
            .seek(Duration::from_secs(9))
            .expect("seek fixture past EOF");
        engine.play().expect("play fixture at EOF");
        let eof_deadline = Instant::now() + Duration::from_secs(2);
        while engine.state() != PlaybackRunState::Ended && Instant::now() < eof_deadline {
            let _ = engine.tick().expect("pump EOF video");
            let _ = engine.tick_audio().expect("pump EOF audio");
            thread::yield_now();
        }
        assert_eq!(engine.state(), PlaybackRunState::Ended);
        Some(engine)
    }

    #[test]
    fn superseded_play_restart_publishes_generation_before_pause() {
        let Some(mut engine) = ended_playback_fixture_engine() else {
            return;
        };

        let player = Player::new(PlayerConfig::default());
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Paused;
            inner.ended = true;
            inner.playback_generation = 7;
            inner.playback_command_sequence = 2;
            inner.playback_clock = PlaybackClock::paused_at(Duration::from_secs(8));
        }
        let mut worker_generation = 7;
        let mut last_sequence = 0;
        let mut frame_output_quiesced = false;

        assert!(handle_playback_command(
            &mut engine,
            &player.inner,
            PlaybackCommand::Pause {
                sequence: 2,
                activate_before_pause: true,
            },
            &mut worker_generation,
            &mut last_sequence,
            &mut frame_output_quiesced,
        ));

        assert_eq!(worker_generation, 8);
        assert_eq!(player.playback_generation(), 8);
        assert_eq!(player.state(), PlayerState::Paused);
        assert_eq!(player.current_media_time(), Duration::ZERO);
    }

    #[test]
    fn stale_pause_cannot_consume_the_latest_eof_play_restart() {
        let Some(mut engine) = ended_playback_fixture_engine() else {
            return;
        };
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Paused;
            inner.ended = true;
            inner.playback_generation = 7;
            inner.playback_command_sequence = 3;
            inner.pending_play_sequence = Some(3);
            inner.playback_clock = PlaybackClock::paused_at(Duration::from_secs(8));
        }
        let mut worker_generation = 7;
        let mut last_sequence = 0;
        let mut frame_output_quiesced = false;

        assert!(handle_playback_command(
            &mut engine,
            &player.inner,
            PlaybackCommand::Pause {
                sequence: 2,
                activate_before_pause: true,
            },
            &mut worker_generation,
            &mut last_sequence,
            &mut frame_output_quiesced,
        ));
        assert_eq!(engine.state(), PlaybackRunState::Ended);
        assert_eq!(worker_generation, 7);
        assert!(events.try_recv().is_err());

        assert!(handle_playback_command(
            &mut engine,
            &player.inner,
            PlaybackCommand::Play {
                sequence: 3,
                generation: 7,
            },
            &mut worker_generation,
            &mut last_sequence,
            &mut frame_output_quiesced,
        ));

        assert_eq!(worker_generation, 8);
        assert_eq!(player.playback_generation(), 8);
        assert_eq!(player.state(), PlayerState::Playing);
        assert_eq!(player.current_media_time(), Duration::ZERO);
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![
                PlayerEvent::PositionChanged(Duration::ZERO),
                PlayerEvent::StateChanged(PlayerState::Playing),
            ],
        );
    }

    #[test]
    fn queued_play_followed_by_seek_keeps_playing_intent_until_seek_commits() {
        for initial_state in [PlayerState::Paused, PlayerState::Stopped] {
            let player = Player::new(PlayerConfig::default());
            let commands = install_fake_playback(
                &player,
                initial_state,
                Duration::from_secs(8),
                7,
                initial_state == PlayerState::Stopped,
            );

            player.play().unwrap();
            let play_sequence = match commands.try_recv().expect("play command") {
                PlaybackCommand::Play { sequence, .. } => sequence,
                _ => panic!("expected play command"),
            };

            let target = Duration::from_secs(3);
            player.seek(target).unwrap();
            let (seek_sequence, generation) = match commands.try_recv().expect("seek command") {
                PlaybackCommand::Seek {
                    position,
                    sequence,
                    generation,
                    resume_after_seek,
                } => {
                    assert_eq!(position, target);
                    assert!(resume_after_seek);
                    (sequence, generation)
                }
                _ => panic!("expected seek command"),
            };

            assert!(seek_sequence > play_sequence);
            assert_eq!(player.state(), initial_state);
            assert_eq!(player.current_media_time(), target);
            assert_eq!(player.playback_intent_state(), PlayerState::Playing);
            assert!(commit_playback_command_intent(
                &player.inner,
                seek_sequence,
                Some(generation),
                None,
                Some(PlayerState::Playing),
            ));
            assert_eq!(player.state(), PlayerState::Playing);
            assert_eq!(player.playback_intent_state(), PlayerState::Playing);
        }
    }

    #[test]
    fn failed_resume_seek_clears_pending_playing_intent() {
        let player = Player::new(PlayerConfig::default());
        let commands = install_fake_playback(
            &player,
            PlayerState::Paused,
            Duration::from_secs(8),
            7,
            false,
        );

        player.play().unwrap();
        let _ = commands.try_recv().expect("play command");
        player.seek(Duration::from_secs(3)).unwrap();
        let sequence = match commands.try_recv().expect("seek command") {
            PlaybackCommand::Seek { sequence, .. } => sequence,
            _ => panic!("expected seek command"),
        };
        assert_eq!(player.playback_intent_state(), PlayerState::Playing);

        assert!(commit_playback_command_failure(
            &player.inner,
            sequence,
            "synthetic seek failure".to_string(),
        ));
        assert_eq!(player.state(), PlayerState::Error);
        assert_eq!(player.playback_intent_state(), PlayerState::Error);
    }

    #[test]
    fn current_play_failure_clears_pending_intent_and_publishes_error() {
        let player = Player::new(PlayerConfig::default());
        let commands = install_fake_playback(&player, PlayerState::Ready, Duration::ZERO, 7, false);
        let events = player.subscribe();

        player.play().unwrap();
        let sequence = match commands.try_recv().expect("play command") {
            PlaybackCommand::Play { sequence, .. } => sequence,
            _ => panic!("expected play command"),
        };
        assert!(commit_playback_command_failure(
            &player.inner,
            sequence,
            "synthetic play failure".to_string(),
        ));

        assert_eq!(player.state(), PlayerState::Error);
        assert_eq!(player.playback_intent_state(), PlayerState::Error);
        assert!(matches!(
            events.try_recv().expect("error event"),
            PlayerEvent::Error(_)
        ));
        assert_eq!(
            events.try_recv().expect("state event"),
            PlayerEvent::StateChanged(PlayerState::Error),
        );
    }

    #[test]
    fn terminal_and_opening_states_override_a_stale_pending_play_intent() {
        let player = Player::new(PlayerConfig::default());
        for state in [
            PlayerState::Opening,
            PlayerState::Error,
            PlayerState::Closed,
        ] {
            {
                let mut inner = player.inner.lock().expect("player mutex poisoned");
                inner.state = state;
                inner.playback_command_sequence = 9;
                inner.pending_play_sequence = Some(9);
            }
            assert_eq!(player.playback_intent_state(), state);
        }
    }

    #[test]
    fn failed_reopen_clears_the_previous_workers_pending_play_intent() {
        let player = Player::new(PlayerConfig::default());
        let _commands = install_fake_playback(
            &player,
            PlayerState::Ready,
            Duration::from_secs(3),
            7,
            false,
        );
        player.play().expect("queue play on old runtime");
        assert_eq!(player.playback_intent_state(), PlayerState::Playing);

        let error = player
            .open(MediaRequest::new(
                "/tmp/erika-definitely-missing-reopen.mp4",
            ))
            .expect_err("replacement media is deliberately missing");

        assert!(matches!(error, PlayerError::Playback(_)));
        assert_eq!(player.state(), PlayerState::Error);
        assert_eq!(player.playback_intent_state(), PlayerState::Error);
        assert_eq!(
            player
                .inner
                .lock()
                .expect("player mutex poisoned")
                .pending_play_sequence,
            None,
        );
    }

    #[test]
    fn stopped_play_from_eof_publishes_rewind_before_playing() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let commands = install_fake_playback(
            &player,
            PlayerState::Stopped,
            Duration::from_secs(8),
            7,
            true,
        );

        let (_, command_generation) = play_with_test_commit(&player, &commands, true);

        assert_eq!(command_generation, 7);
        assert_eq!(player.current_media_time(), Duration::ZERO);
        assert_eq!(player.playback_generation(), 8);
        assert_eq!(player.state(), PlayerState::Playing);
        assert!(!player.is_ended());
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![
                PlayerEvent::PositionChanged(Duration::ZERO),
                PlayerEvent::StateChanged(PlayerState::Playing),
            ]
        );
    }

    #[test]
    fn stopped_play_at_zero_does_not_publish_another_generation() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let commands =
            install_fake_playback(&player, PlayerState::Stopped, Duration::ZERO, 7, false);

        let (_, command_generation) = play_with_test_commit(&player, &commands, false);

        assert_eq!(command_generation, 7);
        assert_eq!(player.current_media_time(), Duration::ZERO);
        assert_eq!(player.playback_generation(), 7);
        assert!(!player.is_ended());
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![PlayerEvent::StateChanged(PlayerState::Playing)]
        );
    }

    #[test]
    fn failed_eof_play_rolls_back_prepublished_clock_and_generation() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        install_disconnected_playback(
            &player,
            PlayerState::Stopped,
            Duration::from_secs(8),
            7,
            true,
        );

        assert!(matches!(player.play(), Err(PlayerError::Playback(_))));

        assert_eq!(player.current_media_time(), Duration::from_secs(8));
        assert_eq!(player.playback_generation(), 7);
        assert_eq!(player.state(), PlayerState::Stopped);
        assert!(player.is_ended());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn seek_from_eof_then_play_preserves_nonzero_target_and_single_generation() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let commands = install_fake_playback(
            &player,
            PlayerState::Stopped,
            Duration::from_secs(8),
            7,
            true,
        );
        let target = Duration::from_millis(3_250);

        player.seek(target).unwrap();

        assert!(matches!(
            commands.try_recv(),
            Ok(PlaybackCommand::Seek { position, .. }) if position == target
        ));
        let (_, command_generation) = play_with_test_commit(&player, &commands, false);
        assert_eq!(command_generation, 8);
        assert_eq!(player.current_media_time(), target);
        assert_eq!(player.playback_generation(), 8);
        assert_eq!(player.state(), PlayerState::Playing);
        assert!(!player.is_ended());
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![
                PlayerEvent::PositionChanged(target),
                PlayerEvent::StateChanged(PlayerState::Playing),
            ]
        );
    }

    #[test]
    fn failed_seek_from_eof_restores_ended_marker() {
        let player = Player::new(PlayerConfig::default());
        install_disconnected_playback(
            &player,
            PlayerState::Stopped,
            Duration::from_secs(8),
            7,
            true,
        );

        assert!(matches!(
            player.seek(Duration::from_millis(3_250)),
            Err(PlayerError::Playback(_))
        ));

        assert_eq!(player.current_media_time(), Duration::from_secs(8));
        assert_eq!(player.playback_generation(), 7);
        assert_eq!(player.state(), PlayerState::Stopped);
        assert!(player.is_ended());
    }

    #[test]
    fn stop_publishes_zero_before_stopped() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let commands = install_fake_playback(
            &player,
            PlayerState::Playing,
            Duration::from_secs(3),
            11,
            false,
        );

        player.stop().unwrap();

        assert!(matches!(
            commands.try_recv(),
            Ok(PlaybackCommand::Stop { .. })
        ));
        assert_eq!(player.current_media_time(), Duration::ZERO);
        assert_eq!(player.playback_generation(), 12);
        assert_eq!(player.state(), PlayerState::Stopped);
        assert!(!player.is_ended());
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![
                PlayerEvent::PositionChanged(Duration::ZERO),
                PlayerEvent::StateChanged(PlayerState::Stopped),
            ]
        );
    }

    #[test]
    fn failed_stop_rolls_back_prepublished_clock_and_generation() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        install_disconnected_playback(
            &player,
            PlayerState::Playing,
            Duration::from_secs(3),
            11,
            false,
        );

        assert!(matches!(player.stop(), Err(PlayerError::Playback(_))));

        assert_eq!(player.current_media_time(), Duration::from_secs(3));
        assert_eq!(player.playback_generation(), 11);
        assert_eq!(player.state(), PlayerState::Playing);
        assert!(!player.is_ended());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn natural_eof_worker_events_are_ordered_once_without_generation_change() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Playing;
            inner.playback_clock = PlaybackClock::paused_at(Duration::from_millis(7_967));
            inner.playback_generation = 13;
        }
        let mut eof_published = false;

        publish_natural_eof_events_from_worker(
            &player.inner,
            13,
            Duration::from_secs(8),
            &mut eof_published,
        );
        publish_natural_eof_events_from_worker(
            &player.inner,
            13,
            Duration::from_secs(8),
            &mut eof_published,
        );

        assert!(eof_published);
        assert_eq!(player.current_media_time(), Duration::from_secs(8));
        assert_eq!(player.playback_generation(), 13);
        assert_eq!(player.state(), PlayerState::Stopped);
        assert!(player.is_ended());
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![
                PlayerEvent::PositionChanged(Duration::from_secs(8)),
                PlayerEvent::StateChanged(PlayerState::Stopped),
                PlayerEvent::Ended,
            ]
        );
    }

    #[test]
    fn ended_play_clears_stale_audio_snapshot_before_restart() {
        let player = Player::new(PlayerConfig::default());
        let mut observation = player.capture_audio_clock(|| {
            Some(AudioClockSnapshot {
                media_time: Some(Duration::from_secs(8)),
                queued_duration: Some(Duration::from_millis(100)),
                queued_frames: 4_800,
                read_frames: 48_000,
                written_frames: 52_800,
                underflow_frames: 0,
            })
        });
        let command = PlaybackCommand::Play {
            sequence: 1,
            generation: 1,
        };

        observe_audio_pump_command(PlaybackRunState::Ended, &mut observation, &command);

        assert!(observation.is_none());
    }

    fn audio_observation_test_sample() -> AudioClockSnapshot {
        AudioClockSnapshot {
            media_time: Some(Duration::from_millis(200)),
            queued_duration: Some(Duration::from_millis(100)),
            queued_frames: 4_800,
            read_frames: 9_600,
            written_frames: 14_400,
            underflow_frames: 0,
        }
    }

    #[test]
    fn audio_observation_keeps_capture_generation_across_seek() {
        let player = Player::new(PlayerConfig::default());
        player.inner.lock().unwrap().state = PlayerState::Playing;
        let observation = player
            .capture_audio_clock(|| Some(audio_observation_test_sample()))
            .unwrap();
        let mut inner = player.inner.lock().unwrap();
        assert!(observation.is_current(
            &inner,
            inner.playback_generation,
            inner.playback_command_sequence,
            Instant::now()
        ));
        // Seek publishes the new timeline before the worker commits its clock.
        inner.playback_generation += 1;
        inner.playback_command_sequence += 1;
        inner.playback_clock = PlaybackClock::paused_at(Duration::from_secs(3));
        assert!(!observation.is_current(
            &inner,
            inner.playback_generation,
            inner.playback_command_sequence,
            Instant::now()
        ));
    }

    #[test]
    fn audio_observation_ages_from_capture_not_delivery() {
        let player = Player::new(PlayerConfig::default());
        player.inner.lock().unwrap().state = PlayerState::Playing;
        let observation = player
            .capture_audio_clock(|| Some(audio_observation_test_sample()))
            .unwrap();
        let inner = player.inner.lock().unwrap();
        assert!(!observation.is_current(
            &inner,
            inner.playback_generation,
            inner.playback_command_sequence,
            observation.captured_at + AUDIO_CLOCK_SNAPSHOT_STALE_AFTER
        ));
        assert!(!observation.is_current(
            &inner,
            inner.playback_generation,
            inner.playback_command_sequence,
            observation.captured_at - Duration::from_millis(1)
        ));
        let mut cached = None;
        observe_audio_pump_command(
            PlaybackRunState::Playing,
            &mut cached,
            &PlaybackCommand::AudioClock(observation),
        );
        assert_eq!(cached.unwrap().captured_at, observation.captured_at);
    }

    #[test]
    fn audio_capture_does_not_relabel_a_reset_during_backend_read() {
        let player = Player::new(PlayerConfig::default());
        player.inner.lock().unwrap().state = PlayerState::Playing;
        let observation = player
            .capture_audio_clock(|| {
                // Also proves no player lock is held across a backend read.
                player.invalidate_audio_clock();
                Some(audio_observation_test_sample())
            })
            .unwrap();
        let inner = player.inner.lock().unwrap();
        assert!(!observation.is_current(
            &inner,
            inner.playback_generation,
            inner.playback_command_sequence,
            Instant::now()
        ));
    }

    #[test]
    fn audio_observation_rejects_old_pause_resume_intent() {
        let player = Player::new(PlayerConfig::default());
        player.inner.lock().unwrap().state = PlayerState::Playing;
        let observation = player
            .capture_audio_clock(|| Some(audio_observation_test_sample()))
            .unwrap();
        let mut inner = player.inner.lock().unwrap();
        inner.state = PlayerState::Paused;
        assert!(!observation.is_current(
            &inner,
            inner.playback_generation,
            inner.playback_command_sequence,
            Instant::now()
        ));
        inner.playback_command_sequence += 2;
        inner.state = PlayerState::Playing;
        assert!(!observation.is_current(
            &inner,
            inner.playback_generation,
            inner.playback_command_sequence,
            Instant::now()
        ));
    }

    #[test]
    fn audio_observation_waits_for_worker_to_execute_capture_intent() {
        let player = Player::new(PlayerConfig::default());
        {
            let mut inner = player.inner.lock().unwrap();
            inner.state = PlayerState::Playing;
            inner.playback_command_sequence = 2;
        }
        let observation = player
            .capture_audio_clock(|| Some(audio_observation_test_sample()))
            .unwrap();
        let inner = player.inner.lock().unwrap();
        assert!(!observation.is_current(&inner, inner.playback_generation, 1, Instant::now()));
        assert!(observation.is_current(&inner, inner.playback_generation, 2, Instant::now()));
    }

    #[test]
    fn audio_observation_cannot_be_submitted_to_another_player() {
        let first = Player::new(PlayerConfig::default());
        let second = Player::new(PlayerConfig::default());
        let observation = first
            .capture_audio_clock(|| Some(audio_observation_test_sample()))
            .unwrap();
        let error = second
            .update_audio_clock_observation(observation)
            .unwrap_err();
        assert!(error.to_string().contains("another player"));
    }

    #[test]
    fn buffering_audio_starvation_requires_empty_queue_and_underflow_progress() {
        let healthy = AudioClockSnapshot {
            media_time: Some(Duration::from_secs(5)),
            queued_duration: Some(Duration::from_millis(100)),
            queued_frames: 4_800,
            read_frames: 48_000,
            written_frames: 52_800,
            underflow_frames: 10,
        };
        assert!(!audio_snapshot_is_starved(healthy, Some(9), false));

        let empty = AudioClockSnapshot {
            queued_duration: Some(Duration::ZERO),
            queued_frames: 0,
            underflow_frames: 11,
            ..healthy
        };
        assert!(audio_snapshot_is_starved(empty, Some(10), false));
        assert!(!audio_snapshot_is_starved(empty, Some(11), false));
        assert!(audio_snapshot_is_starved(empty, Some(11), true));
    }

    #[test]
    fn buffering_audio_recovery_requires_low_water_mark() {
        let snapshot = |queued_duration, queued_frames| AudioClockSnapshot {
            media_time: Some(Duration::from_secs(7)),
            queued_duration: Some(queued_duration),
            queued_frames,
            read_frames: 48_000,
            written_frames: 60_000,
            underflow_frames: 0,
        };

        assert_eq!(
            audio_snapshot_recovery_reference(
                snapshot(Duration::from_millis(99), 4_752),
                Duration::from_millis(100),
            ),
            None
        );
        assert_eq!(
            audio_snapshot_recovery_reference(
                snapshot(Duration::from_millis(100), 4_800),
                Duration::from_millis(100),
            ),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            audio_snapshot_recovery_reference(
                snapshot(Duration::from_millis(249), 11_952),
                Duration::from_millis(250),
            ),
            None
        );
        assert_eq!(
            audio_snapshot_recovery_reference(
                snapshot(Duration::from_millis(250), 12_000),
                Duration::from_millis(250),
            ),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn worker_clock_waits_for_prepublished_generation() {
        assert!(worker_clock_generation_is_stale(8, 7));
        assert!(!worker_clock_generation_is_stale(8, 8));
        assert!(!worker_clock_generation_is_stale(7, 8));
    }

    #[test]
    fn player_rapid_seeks_enqueue_fifo_and_publish_latest_target() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let receiver = install_test_runtime(&player, 3);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Playing;
        }
        let before_generation = player.playback_generation();
        let targets = [
            Duration::from_secs(1),
            Duration::from_secs(4),
            Duration::from_secs(2),
        ];

        for target in targets {
            player.seek(target).unwrap();
        }

        let queued_targets = receiver
            .try_iter()
            .map(|command| match command {
                PlaybackCommand::Seek { position, .. } => position,
                _ => panic!("unexpected playback command"),
            })
            .collect::<Vec<_>>();
        let published_targets = events
            .try_iter()
            .map(|event| match event {
                PlayerEvent::PositionChanged(position) => position,
                _ => panic!("unexpected player event"),
            })
            .collect::<Vec<_>>();

        assert_eq!(queued_targets, targets);
        assert_eq!(published_targets, targets);
        assert_eq!(player.current_media_time(), targets[2]);
        assert_eq!(
            player.playback_generation(),
            before_generation + targets.len() as u64
        );
        assert_eq!(player.state(), PlayerState::Playing);
    }

    #[test]
    fn player_seek_updates_shared_media_time_and_generation_immediately_when_ready() {
        let player = Player::new(PlayerConfig::default());
        let receiver = install_test_runtime(&player, 1);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Ready;
        }
        let before_generation = player.playback_generation();

        player.seek(Duration::from_secs(12)).unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(PlaybackCommand::Seek {
                position,
                resume_after_seek: false,
                ..
            }) if position == Duration::from_secs(12)
        ));
        assert_eq!(player.current_media_time(), Duration::from_secs(12));
        assert_eq!(player.playback_generation(), before_generation + 1);
        assert_eq!(player.state(), PlayerState::Ready);
    }

    #[test]
    fn player_seek_while_paused_preserves_paused_state() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let receiver = install_test_runtime(&player, 1);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Paused;
            inner.playback_clock = PlaybackClock::paused_at(Duration::from_secs(8));
        }
        let target = Duration::from_secs(23);

        player.seek(target).unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(PlaybackCommand::Seek {
                position,
                resume_after_seek: false,
                ..
            }) if position == target
        ));
        assert_eq!(player.current_media_time(), target);
        assert_eq!(player.state(), PlayerState::Paused);
        assert_eq!(events.try_recv(), Ok(PlayerEvent::PositionChanged(target)));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn failed_seek_does_not_leave_clock_generation_half_updated() {
        let player = Player::new(PlayerConfig::default());
        let receiver = install_test_runtime(&player, 1);
        drop(receiver);
        {
            let mut inner = player.inner.lock().expect("player mutex poisoned");
            inner.state = PlayerState::Ready;
        }
        let before_generation = player.playback_generation();

        assert!(player.seek(Duration::from_secs(12)).is_err());

        assert_eq!(player.current_media_time(), Duration::ZERO);
        assert_eq!(player.playback_generation(), before_generation);
    }

    #[test]
    fn player_open_missing_media_reports_playback_error() {
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();

        let error = player
            .open(MediaRequest::new("/tmp/erika-definitely-missing.mp4"))
            .unwrap_err();

        assert!(matches!(error, PlayerError::Playback(_)));
        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::StateChanged(PlayerState::Opening)
        );
        assert_eq!(events.recv().unwrap(), PlayerEvent::BufferingChanged(false));
        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::StateChanged(PlayerState::Error)
        );
        assert!(matches!(
            events.recv().unwrap(),
            PlayerEvent::Error(PlayerError::Playback(_))
        ));
    }

    #[test]
    fn player_open_sample_emits_probe_events_when_env_is_set() {
        let Ok(sample) = std::env::var("ERIKA_TEST_SAMPLE") else {
            return;
        };
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();

        player.open(MediaRequest::new(sample)).unwrap();

        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::StateChanged(PlayerState::Opening)
        );
        assert_eq!(events.recv().unwrap(), PlayerEvent::BufferingChanged(false));
        assert!(matches!(
            events.recv().unwrap(),
            PlayerEvent::DurationChanged(Some(_))
        ));
        assert!(
            matches!(events.recv().unwrap(), PlayerEvent::TracksChanged(tracks) if !tracks.is_empty())
        );
        assert!(matches!(
            events.recv().unwrap(),
            PlayerEvent::TrackSelectionChanged(selection) if selection.video.is_some()
        ));
        assert!(
            matches!(events.recv().unwrap(), PlayerEvent::VideoParamsChanged(params) if params.width > 0 && params.height > 0)
        );
        assert!(matches!(
            events.recv().unwrap(),
            PlayerEvent::VideoDecoderChanged(event)
                if event.stage.starts_with("open") && event.codec.is_some()
        ));
        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::StateChanged(PlayerState::Ready)
        );
    }

    #[test]
    fn container_chapters_reach_player_and_clear_on_reopen_or_failure() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let chapter_file = root.join("chapters/chapters.mkv");
        let player = Player::new(PlayerConfig::default());
        player
            .open(MediaRequest::new(chapter_file.to_string_lossy()))
            .unwrap();
        let chapters = player.chapters();
        assert_eq!(chapters.len(), 6);
        assert_eq!(
            chapters[1],
            ChapterInfo {
                title: "OP".into(),
                start: Duration::from_secs(10),
                end: Some(Duration::from_secs(20)),
            }
        );
        assert_eq!(chapters[4].title, "ED");
        assert_eq!(chapters[4].end, Some(Duration::from_secs(50)));
        player
            .open(MediaRequest::new(
                root.join("playback/playback-fixture.mkv").to_string_lossy(),
            ))
            .unwrap();
        assert!(player.chapters().is_empty());
        player
            .open(MediaRequest::new(chapter_file.to_string_lossy()))
            .unwrap();
        assert!(
            player
                .open(MediaRequest::new(
                    root.join("missing.mkv").to_string_lossy()
                ))
                .is_err()
        );
        assert!(player.chapters().is_empty());
        player.close().unwrap();
        assert!(player.chapters().is_empty());
    }

    #[test]
    fn playback_generation_remains_monotonic_across_reopen_when_env_is_set() {
        let Ok(sample) = std::env::var("ERIKA_TEST_SAMPLE") else {
            return;
        };
        let player = Player::new(PlayerConfig::default());

        player.open(MediaRequest::new(sample.clone())).unwrap();
        let first_generation = player.playback_generation();
        player.open(MediaRequest::new(sample)).unwrap();

        assert!(player.playback_generation() > first_generation);
    }

    #[test]
    fn player_play_sample_emits_position_events_when_env_is_set() {
        let Ok(sample) = std::env::var("ERIKA_TEST_SAMPLE") else {
            return;
        };
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();

        player.open(MediaRequest::new(sample)).unwrap();
        while events.recv().unwrap() != PlayerEvent::StateChanged(PlayerState::Ready) {}
        player.play().unwrap();

        assert_eq!(
            events.recv().unwrap(),
            PlayerEvent::StateChanged(PlayerState::Playing)
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::PositionChanged(position)) if position > Duration::ZERO => break,
                Ok(_) => {}
                Err(_) if std::time::Instant::now() < deadline => {}
                Err(error) => panic!("expected playback position event: {error}"),
            }
        }
        player.close().unwrap();
    }

    #[test]
    fn player_restarts_from_zero_after_sample_eof_when_env_is_set() {
        let Ok(sample) = std::env::var("ERIKA_TEST_SAMPLE") else {
            return;
        };
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();

        player.open(MediaRequest::new(sample)).unwrap();
        while events.recv().unwrap() != PlayerEvent::StateChanged(PlayerState::Ready) {}
        let duration = player.duration().expect("sample duration");
        player
            .seek(duration.saturating_add(Duration::from_secs(1)))
            .unwrap();
        player.play().unwrap();

        let eof_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::StateChanged(PlayerState::Stopped)) => break,
                Ok(PlayerEvent::Error(error)) => panic!("playback failed before EOF: {error}"),
                Ok(_) => {}
                Err(_) if Instant::now() < eof_deadline => {}
                Err(error) => panic!("expected EOF stopped state: {error}"),
            }
        }
        while events.try_recv().is_ok() {}
        let generation_at_eof = player.playback_generation();

        player.play().unwrap();

        // Play is intentionally asynchronous: the accepted intent is visible
        // immediately, while state/generation move only after the worker has
        // completed the EOF rewind.
        assert_eq!(player.playback_intent_state(), PlayerState::Playing);
        assert_eq!(
            events.recv_timeout(Duration::from_secs(1)).unwrap(),
            PlayerEvent::PositionChanged(Duration::ZERO)
        );
        assert_eq!(
            events.recv_timeout(Duration::from_secs(1)).unwrap(),
            PlayerEvent::StateChanged(PlayerState::Playing)
        );
        assert_eq!(player.state(), PlayerState::Playing);
        assert!(player.playback_generation() > generation_at_eof);
        assert!(player.current_media_time() < Duration::from_secs(1));

        let frame_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(PlayerEvent::PositionChanged(position)) if position > Duration::ZERO => break,
                Ok(PlayerEvent::Error(error)) => panic!("restart failed: {error}"),
                Ok(_) => {}
                Err(_) if Instant::now() < frame_deadline => {}
                Err(error) => panic!("expected position after EOF restart: {error}"),
            }
        }
        player.close().unwrap();
    }
}
