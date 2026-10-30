use std::{
    collections::{HashMap, HashSet},
    env,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};

#[cfg(target_os = "android")]
use crate::android::aaudio::{AAudioOutput, AAudioOutputConfig};
#[cfg(target_os = "macos")]
use crate::apple::coreaudio::{CoreAudioOutput, CoreAudioOutputConfig};
#[cfg(any(target_os = "ios", target_os = "tvos"))]
use crate::apple::iosaudio::{IosAudioQueueOutput, IosAudioQueueOutputConfig};
#[cfg(not(any(
    target_os = "android",
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "linux",
    target_env = "ohos"
)))]
use crate::audio::BufferedAudioOutput;
use crate::audio::{
    AudioClockSnapshot, AudioOutputBackend, AudioOutputRuntimeStats, AudioRingBufferConfig,
};
use crate::core::{
    AudioOutputEvent, MediaRequest, PlatformSurface, PlaybackSnapshot, Player, PlayerAudioFrame,
    PlayerConfig, PlayerSubtitleFrame, PlayerVideoFrame, RenderFrameContext, RendererBackend,
    RendererBackendPreference, RendererRuntimeStats, SurfaceMetrics, TrackInfo, TrackSelection,
    VideoDecoderEvent, VideoFrameImportFailure,
};
use crate::danmaku::{
    DANMAKU_DEBUG_BUCKETS, DanmakuConfigChange, DanmakuDebugBucket, DanmakuFontSelection,
    DanmakuLayoutConfig, DanmakuMode, DanmakuPreparedStats, DanmakuRenderPlan, DanmakuSession,
    DanmakuTextRasterizer, DanmakuTimeline, DanmakuTrackInfo, DanmakuTrackSource, DanmakuViewport,
    DfmLayoutEngine, DfmPreparedLayout, scroll_duration_for_viewport,
};
use crate::debug_hud::{DebugHud, DebugHudSnapshot};
use crate::ffmpeg::DecoderBackend;
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
use crate::linux::PulseAudioOutput;
use crate::luma_stats::LumaSmoother;
#[cfg(target_env = "ohos")]
use crate::ohos::ohaudio::{OHAudioOutput, OHAudioOutputConfig};
use crate::overlay::{OverlayFrame, OverlayTimeline, OverlayViewport};
#[cfg(any(target_os = "windows", target_os = "android"))]
use crate::playback::VideoDecodePreference;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
use crate::renderer::metal::MetalRenderer;
use crate::renderer::metal::MetalRendererConfig;
#[cfg(feature = "libass")]
use crate::subtitle::{
    AssTrackResources, LibassRenderConfig, LibassSubtitleRenderer, SubtitleBitmapSet,
    SubtitleError, SubtitleRenderOutput, SubtitleRenderRequest, SubtitleRenderer,
    decoded_subtitle_frames_to_ass_script_with_style,
};
use crate::subtitle::{
    DecodedSubtitleFrame, MAX_MEMORY_SUBTITLE_FONT_BYTES, MAX_MEMORY_SUBTITLE_FONT_TOTAL_BYTES,
    SubtitleAssStyle, SubtitleFontAttachment, SubtitleRendererCore, SubtitleStyleConfig,
    SubtitleTrackConfig, SubtitleViewport, decoded_subtitle_frames_to_timeline,
};
use crate::trace;
#[cfg(target_os = "windows")]
use crate::windows::wasapi::{WasapiAudioOutput, WasapiAudioOutputConfig};
use crate::{PlayerError, Result};

const AUDIO_START_BUFFER: Duration = Duration::from_millis(250);
const AUDIO_PUMP_FRAME_LIMIT: usize = 16;
const AUDIO_PUMP_TIME_BUDGET: Duration = Duration::from_millis(4);
// The audio transform currently runs on the display-driven presenter path.
// Keep a rate-change refill bounded to one normal audio-pump slice so it
// cannot consume an entire render frame while SoundTouch is warming up.
const AUDIO_FAST_RATE_PUMP_FRAME_LIMIT: usize = 24;
const AUDIO_FAST_RATE_PUMP_TIME_BUDGET: Duration = Duration::from_millis(4);
const PLAYBACK_RATE_EPSILON: f64 = 0.001;
const VIDEO_PUMP_FRAME_LIMIT: usize = 8;
const VIDEO_PUMP_TIME_BUDGET: Duration = Duration::from_millis(4);
// A foreground resume that keeps failing must not retry once per display
// frame: every attempt crosses the playback worker channel and can block the
// render queue on the frame-output barrier timeout.
const MAX_VIDEO_DECODE_RESUME_ATTEMPTS: u32 = 5;
const DANMAKU_PLAN_REQUEST_QUANTUM: Duration = Duration::from_millis(250);
const DANMAKU_PREPARE_REFRESH_MARGIN: Duration = Duration::from_secs(4);
// Geometry previews are immediate. Rasterize the final value after idle,
// with a bounded wait so a long drag still refreshes its timeline window.
const DANMAKU_FONT_SIZE_SETTLE: Duration = Duration::from_millis(120);
const DANMAKU_FONT_SIZE_MAX_WAIT: Duration = Duration::from_millis(500);
const DANMAKU_RESIZE_SETTLE: Duration = Duration::from_millis(100);
const DANMAKU_PLAN_LOOKAHEAD: Duration = Duration::from_secs(8);
const DANMAKU_PLAN_LOOKBACK_PADDING: Duration = Duration::from_secs(2);
const DANMAKU_MOTION_TRACE_INTERVAL: Duration = Duration::from_millis(500);
const DANMAKU_MOTION_BACKSTEP_EPSILON: f32 = 0.5;
const DEFAULT_SUBTITLE_FONT_SCALE: f64 = 1.0;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubtitleMemoryFontStatus {
    pub registered_count: usize,
    pub registered_bytes: usize,
    pub selected_count: usize,
    pub generation: u64,
    pub selected_ids: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleMemoryFontFace {
    pub index: u32,
    pub families: Vec<String>,
    pub post_script_name: String,
    pub weight: u16,
    pub italic: bool,
    pub monospaced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleMemoryFontInfo {
    pub id: u64,
    pub byte_len: usize,
    pub faces: Vec<SubtitleMemoryFontFace>,
}

#[derive(Debug, Clone)]
struct SubtitleMemoryFontEntry {
    attachment: SubtitleFontAttachment,
    faces: Vec<SubtitleMemoryFontFace>,
}

#[derive(Debug, Default)]
struct SubtitleMemoryFonts {
    next_id: u64,
    registered: HashMap<u64, SubtitleMemoryFontEntry>,
    selected_ids: Vec<u64>,
    generation: u64,
    selection_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionFramePolicy {
    Clear,
    PreserveRendererSnapshot,
    PreserveTrackSwitchFrame,
}

#[derive(Debug, Clone)]
pub struct PresenterConfig {
    pub player: PlayerConfig,
    pub audio: PresenterAudioConfig,
    pub renderer: MetalRendererConfig,
    pub overlay: OverlayTimeline,
    pub danmaku: Option<DanmakuTimeline>,
    pub danmaku_config: DanmakuLayoutConfig,
    pub render_test_pattern_when_idle: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresenterAudioConfig {
    pub ring_buffer: AudioRingBufferConfig,
}

impl Default for PresenterAudioConfig {
    fn default() -> Self {
        #[cfg(target_os = "macos")]
        {
            let config = CoreAudioOutputConfig::default();
            Self {
                ring_buffer: config.ring_buffer,
            }
        }
        #[cfg(any(target_os = "ios", target_os = "tvos"))]
        {
            let config = IosAudioQueueOutputConfig::default();
            Self {
                ring_buffer: config.ring_buffer,
            }
        }
        #[cfg(target_os = "windows")]
        {
            let config = WasapiAudioOutputConfig::default();
            Self {
                ring_buffer: config.ring_buffer,
            }
        }
        #[cfg(target_os = "android")]
        {
            let config = AAudioOutputConfig::default();
            Self {
                ring_buffer: config.ring_buffer,
            }
        }
        #[cfg(target_env = "ohos")]
        {
            let config = OHAudioOutputConfig::default();
            Self {
                ring_buffer: config.ring_buffer,
            }
        }
        #[cfg(not(any(
            target_os = "android",
            target_os = "macos",
            any(target_os = "ios", target_os = "tvos"),
            target_os = "windows",
            target_env = "ohos"
        )))]
        {
            Self {
                ring_buffer: AudioRingBufferConfig {
                    capacity_frames: 192_000,
                    drop_oldest_on_overflow: true,
                },
            }
        }
    }
}

impl Default for PresenterConfig {
    fn default() -> Self {
        Self {
            player: PlayerConfig::default(),
            audio: PresenterAudioConfig::default(),
            renderer: MetalRendererConfig::default(),
            overlay: OverlayTimeline::default(),
            danmaku: None,
            danmaku_config: DanmakuLayoutConfig::default(),
            render_test_pattern_when_idle: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PresenterStats {
    pub decoded_video_frames: u64,
    pub rendered_video_frames: u64,
    pub rendered_test_frames: u64,
    pub pushed_audio_frames: u64,
    pub decoded_subtitle_frames: u64,
    pub overlay_frames: u64,
    pub danmaku_frames: u64,
    pub danmaku_items: u64,
    pub import_failures: u64,
    pub video_frame_backpressure_drops: u64,
    pub render_failures: u64,
    pub audio_failures: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PresenterRuntimeSnapshot {
    pub stats: PresenterStats,
    pub renderer: RendererRuntimeStats,
    pub resources: crate::core::RendererResourceStats,
    pub output: crate::renderer::output::OutputRuntimeStatus,
    pub audio_output_queued_duration: Option<Duration>,
    pub audio_output_queued_frames: usize,
    pub audio_output_read_frames: u64,
    pub audio_output_written_frames: u64,
    pub audio_output_underflow_frames: u64,
    pub audio_output_runtime_stats: AudioOutputRuntimeStats,
    pub media_time: Duration,
    pub generation: u64,
    pub playing: bool,
    pub current_danmaku_items: usize,
    pub current_danmaku_atlas_version: u64,
    pub current_danmaku_atlas_bytes: usize,
    pub current_danmaku_viewport_width: u32,
    pub current_danmaku_viewport_height: u32,
    pub current_danmaku_placed_items: usize,
    pub current_danmaku_scroll_items: usize,
    pub current_danmaku_top_items: usize,
    pub current_danmaku_bottom_items: usize,
    pub current_danmaku_scroll_rows: usize,
    pub current_danmaku_scroll_track_min: usize,
    pub current_danmaku_scroll_track_max: usize,
    pub current_danmaku_scroll_min_y: f32,
    pub current_danmaku_scroll_max_y: f32,
    pub current_danmaku_scroll_bucket_count: usize,
    pub current_danmaku_scroll_buckets: [DanmakuDebugBucket; DANMAKU_DEBUG_BUCKETS],
    pub current_danmaku_prepared: DanmakuPreparedStats,
    pub last_tick_duration: Duration,
    pub last_pump_duration: Duration,
    pub last_audio_pump_duration: Duration,
    pub last_subtitle_pump_duration: Duration,
    pub last_video_pump_duration: Duration,
    pub last_clock_sync_duration: Duration,
    pub last_danmaku_plan_duration: Duration,
    pub last_render_duration: Duration,
    pub last_render_current_duration: Duration,
    pub last_render_test_duration: Duration,
}

mod subtitle_resolution;
use subtitle_resolution::SubtitleResolution;

pub struct PresenterRuntime {
    player: Player,
    renderer: Box<dyn RendererBackend>,
    video_frames: Receiver<PlayerVideoFrame>,
    audio_frames: Receiver<PlayerAudioFrame>,
    subtitle_frames: Receiver<PlayerSubtitleFrame>,
    player_events: Receiver<crate::core::PlayerEvent>,
    audio_output: Box<dyn AudioOutputBackend>,
    audio_configured: bool,
    audio_started: bool,
    last_audio_clock_report: Option<AudioClockReportState>,
    last_audio_runtime_stats: AudioOutputRuntimeStats,
    playback_rate: f64,
    pending_playback_rate: Option<PendingPlaybackRate>,
    audio_only_tick_active: bool,
    resume_pending: bool,
    video_decode_resume_attempts: u32,
    latest_video_decoder: Option<VideoDecoderEvent>,
    current_overlay: Option<OverlayFrame>,
    debug_hud: DebugHud,
    current_danmaku: Option<DanmakuRenderPlan>,
    current_danmaku_prepared: Option<CurrentDanmakuPrepared>,
    danmaku_plan_replacement_pending: bool,
    danmaku_resize_settle_at: Option<Instant>,
    danmaku_font_size_update: Option<DanmakuFontSizeUpdate>,
    rejected_video_import_route: Option<RejectedVideoImportRoute>,
    /// Rolling scene-average luminance (HDR10 without DoVi L1), injected
    /// into each software frame's `scene_avg_nits` to drive the tone-map
    /// pivot like Dolby Vision L1 would.
    scene_luma_smoother: LumaSmoother,
    last_scene_luma_generation: u64,
    current_media_time: Duration,
    current_generation: u64,
    current_surface_metrics: Option<SurfaceMetrics>,
    current_danmaku_viewport: Option<DanmakuViewport>,
    subtitle_font_scale: f64,
    subtitle_resolution: SubtitleResolution,
    subtitle_style: SubtitleStyleConfig,
    subtitle_memory_fonts: SubtitleMemoryFonts,
    subtitles: SubtitleFrameState,
    overlay: OverlayTimeline,
    render_test_pattern_when_idle: bool,
    danmaku_session: DanmakuSession,
    danmaku: DfmLayoutEngine,
    danmaku_planner: AsyncDanmakuPlanner,
    danmaku_generation: u64,
    danmaku_trace: DanmakuTimeTrace,
    stats: PresenterStats,
    last_tick_duration: Duration,
    last_pump_duration: Duration,
    last_audio_pump_duration: Duration,
    last_subtitle_pump_duration: Duration,
    last_video_pump_duration: Duration,
    last_clock_sync_duration: Duration,
    last_danmaku_plan_duration: Duration,
    last_render_duration: Duration,
    last_render_current_duration: Duration,
    last_render_test_duration: Duration,
    presentation: crate::diagnostics::PresentationStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RejectedVideoImportRoute {
    backend: DecoderBackend,
    mediacodec_surface: bool,
    generation: u64,
}

impl RejectedVideoImportRoute {
    fn new(backend: DecoderBackend, mediacodec_surface: bool, generation: u64) -> Self {
        Self {
            backend,
            mediacodec_surface: backend == DecoderBackend::MediaCodec && mediacodec_surface,
            generation,
        }
    }
}

fn should_reject_video_import(
    rejected: Option<RejectedVideoImportRoute>,
    candidate: RejectedVideoImportRoute,
) -> bool {
    rejected == Some(candidate)
}

/// Measure the scene-average luminance of an HDR10 (PQ, no Dolby Vision L1)
/// software-decoded frame and fold it into `smoother`, returning the smoothed
/// average in nits. Hardware frames (no CPU plane) and non-HDR10 sources keep
/// the previous estimate (`None` when nothing is known yet). The luma plane is
/// sampled in place with its native stride — no frame repack happens here.
fn measure_frame_scene_avg(frame: &PlayerVideoFrame, smoother: &mut LumaSmoother) -> Option<f32> {
    use crate::core::TransferFunction;
    use crate::luma_stats::{measure_luma_plane_view, pq_code_to_nits};
    use crate::renderer::pipeline::ColorRange;
    // Hardware payloads have no CPU plane to sample.
    let decoded = frame.frame.decoded_frame()?;
    // Only PQ (HDR10) sources. When the RPU already carries L1 brightness,
    // prefer that over measuring the base layer.
    if decoded.transfer_function() != TransferFunction::Pq {
        return None;
    }
    if decoded.dovi_metadata().and_then(|dovi| dovi.l1).is_some() {
        return None;
    }
    let full_range = matches!(decoded.color_range(), ColorRange::Full);
    let plane = decoded.luma_plane_view()?;
    let measured = measure_luma_plane_view(&plane, full_range)?;
    let (avg_pq, _max_pq) = smoother.push(measured);
    let nits = pq_code_to_nits(avg_pq);
    (nits.is_finite() && nits > 0.0).then_some(nits)
}

fn should_report_video_frame_backpressure(drop_count: u64) -> bool {
    drop_count == 1 || drop_count.is_power_of_two()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioClockReportState {
    media_time: Duration,
    queued_frames: usize,
    read_frames: u64,
    underflow_frames: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingPlaybackRate {
    rate: f64,
    commit_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DanmakuPlanKey {
    media_time: Duration,
    viewport: DanmakuViewport,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct AsyncDanmakuPlanRequest {
    key: DanmakuPlanKey,
}

#[derive(Debug)]
struct AsyncDanmakuPlanResult {
    request: AsyncDanmakuPlanRequest,
    prepared: DfmPreparedLayout,
    rasterizer: DanmakuTextRasterizer,
    window_start: Duration,
    window_end: Duration,
    elapsed: Duration,
}

#[derive(Debug)]
struct AsyncDanmakuPlannerState {
    revision: u64,
    config_revision: u64,
    timeline: DanmakuTimeline,
    config: DanmakuLayoutConfig,
    rasterizer: DanmakuTextRasterizer,
    latest_request: Option<AsyncDanmakuPlanRequest>,
    invalidate_placement_history: bool,
    shutdown: bool,
}

struct AsyncDanmakuPlanner {
    shared: Arc<(Mutex<AsyncDanmakuPlannerState>, Condvar)>,
    results: Receiver<AsyncDanmakuPlanResult>,
    last_requested: Option<DanmakuPlanKey>,
    #[cfg(target_os = "windows")]
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
struct CurrentDanmakuPrepared {
    request: AsyncDanmakuPlanRequest,
    prepared: DfmPreparedLayout,
    rasterizer: DanmakuTextRasterizer,
    window_start: Duration,
    window_end: Duration,
}

#[derive(Debug, Clone, Copy)]
struct DanmakuFontSizeUpdate {
    settle_at: Instant,
    deadline: Instant,
}

impl DanmakuFontSizeUpdate {
    fn retarget(pending: &mut Option<Self>, now: Instant) {
        let deadline = pending.map_or(now + DANMAKU_FONT_SIZE_MAX_WAIT, |p| p.deadline);
        *pending = Some(Self {
            settle_at: now + DANMAKU_FONT_SIZE_SETTLE,
            deadline,
        });
    }

    fn ready(self, now: Instant) -> bool {
        now >= self.settle_at || now >= self.deadline
    }
}

impl AsyncDanmakuPlanner {
    fn new(
        engine: DfmLayoutEngine,
        timeline: DanmakuTimeline,
        config: DanmakuLayoutConfig,
    ) -> Self {
        let rasterizer = engine.rasterizer_clone();
        let state = AsyncDanmakuPlannerState {
            revision: 0,
            config_revision: 1,
            timeline,
            config,
            rasterizer,
            latest_request: None,
            invalidate_placement_history: false,
            shutdown: false,
        };
        let shared = Arc::new((Mutex::new(state), Condvar::new()));
        let (result_tx, results) = crossbeam_channel::unbounded();
        let worker_shared = Arc::clone(&shared);
        let _worker = thread::Builder::new()
            .name("erika-danmaku".to_string())
            .spawn(move || run_async_danmaku_planner(worker_shared, result_tx, engine))
            .expect("spawn erika danmaku planner");
        Self {
            shared,
            results,
            last_requested: None,
            #[cfg(target_os = "windows")]
            worker: Some(_worker),
        }
    }

    fn set_timeline(&mut self, timeline: DanmakuTimeline) {
        self.update_configuration(Some(timeline), None);
    }

    fn clear_timeline(&mut self) {
        self.update_configuration(Some(DanmakuTimeline::default()), None);
    }

    fn set_config(&mut self, config: DanmakuLayoutConfig, rasterizer: DanmakuTextRasterizer) {
        self.last_requested = None;
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.config = config;
        state.rasterizer = rasterizer;
        state.latest_request = None;
        state.config_revision = state.config_revision.saturating_add(1);
        state.revision = state.revision.saturating_add(1);
        cvar.notify_one();
    }

    fn set_paint_config(&mut self, config: DanmakuLayoutConfig) {
        let (lock, _) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.config.opacity = config.opacity;
        state.config.shadow_offset = config.shadow_offset;
        state.config.shadow_style = config.shadow_style;
        state.config.enabled = config.enabled;
        // Do not leak a pending font-size preview into background preparation.
        // Do not wake or invalidate an in-flight layout for a renderer-only
        // change. The worker will adopt this revision before its next request.
        state.config_revision = state.config_revision.saturating_add(1);
    }

    fn set_font_selection(&mut self, font_selection: DanmakuFontSelection) {
        self.last_requested = None;
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.rasterizer =
            DanmakuTextRasterizer::for_config_and_selection(&state.config, &font_selection);
        state.latest_request = None;
        state.config_revision = state.config_revision.saturating_add(1);
        state.revision = state.revision.saturating_add(1);
        cvar.notify_one();
    }

    fn invalidate_requests(&mut self) {
        self.last_requested = None;
        let (lock, _) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.latest_request = None;
    }

    fn request_plan(&mut self, key: DanmakuPlanKey) {
        if self.last_requested == Some(key) {
            return;
        }
        self.last_requested = Some(key);
        let request = AsyncDanmakuPlanRequest { key };
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.latest_request = Some(request);
        state.revision = state.revision.saturating_add(1);
        cvar.notify_one();
    }

    fn try_recv(&self) -> Option<AsyncDanmakuPlanResult> {
        self.results.try_recv().ok()
    }

    fn update_configuration(
        &mut self,
        timeline: Option<DanmakuTimeline>,
        config: Option<DanmakuLayoutConfig>,
    ) {
        self.last_requested = None;
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(timeline) = timeline {
            if state.timeline != timeline {
                state.timeline = timeline;
                state.invalidate_placement_history = true;
            }
        }
        if let Some(config) = config {
            state.config = config;
        }
        state.latest_request = None;
        state.config_revision = state.config_revision.saturating_add(1);
        state.revision = state.revision.saturating_add(1);
        cvar.notify_one();
    }
}

impl Drop for AsyncDanmakuPlanner {
    fn drop(&mut self) {
        {
            let (lock, cvar) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.shutdown = true;
            state.revision = state.revision.saturating_add(1);
            cvar.notify_one();
        }
        // The Windows plugin unloads the native DLL after the last presenter.
        // A shutdown notification alone leaves the worker executing (or even
        // starting) in unmapped code. Release the state lock before joining.
        #[cfg(target_os = "windows")]
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Debug, Clone)]
struct DanmakuTimeTrace {
    enabled: bool,
    log_path: Option<PathBuf>,
    samples: u64,
    last_event_time: Option<Duration>,
    last_event_generation: u64,
    last_player_time: Option<Duration>,
    last_player_generation: u64,
    last_video_time: Option<Duration>,
    last_video_generation: u64,
    last_plan_time: Option<Duration>,
    last_plan_generation: u64,
    last_motion_log_at: Option<Instant>,
    motion_samples: HashMap<u64, DanmakuMotionSample>,
    motion_backsteps: u64,
    last_motion_atlas_version: u64,
    last_motion_prepared_key: Option<DanmakuPlanKey>,
    last_motion_viewport: Option<DanmakuViewport>,
    last_motion_had_plan: bool,
    surface_resize_calls: u64,
    surface_resize_redundant: u64,
    last_surface_resize_log_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct DanmakuMotionSample {
    x: f32,
    plan_time: Duration,
    generation: u64,
    mode: DanmakuMode,
    viewport: DanmakuViewport,
}

impl DanmakuTimeTrace {
    fn from_env() -> Self {
        let env_value = env::var("ERIKA_DANMAKU_TRACE").ok();
        let enabled = trace::env_flag("ERIKA_DANMAKU_TRACE");
        let log_path = enabled.then(|| {
            env::var_os("ERIKA_DANMAKU_TRACE_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp/erika_danmaku_trace.log"))
        });
        if let Some(path) = &log_path {
            let _ = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)
                .and_then(|mut file| {
                    writeln!(
                        file,
                        "erika danmaku trace start pid={} debug_assertions={} env={}",
                        std::process::id(),
                        cfg!(debug_assertions),
                        env_value.as_deref().unwrap_or("<unset>"),
                    )
                });
        }
        Self {
            enabled,
            log_path,
            samples: 0,
            last_event_time: None,
            last_event_generation: 0,
            last_player_time: None,
            last_player_generation: 0,
            last_video_time: None,
            last_video_generation: 0,
            last_plan_time: None,
            last_plan_generation: 0,
            last_motion_log_at: None,
            motion_samples: HashMap::new(),
            motion_backsteps: 0,
            last_motion_atlas_version: 0,
            last_motion_prepared_key: None,
            last_motion_viewport: None,
            last_motion_had_plan: false,
            surface_resize_calls: 0,
            surface_resize_redundant: 0,
            last_surface_resize_log_at: None,
        }
    }

    fn write_line(&self, line: &str) {
        eprintln!("{line}");
        if let Some(path) = &self.log_path {
            let _ = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| writeln!(file, "{line}"));
        }
    }
}

impl PresenterRuntime {
    pub fn new(mut config: PresenterConfig) -> Result<Self> {
        let renderer_preference = config.player.renderer;
        let renderer = build_renderer(renderer_preference, config.renderer)?;
        let supports_mediacodec_surface = renderer.supports_mediacodec_surface_frames();
        #[cfg(target_env = "ohos")]
        let ohos_avcodec_surface = renderer.ohos_avcodec_surface();
        resolve_presenter_player_config(
            &mut config.player,
            renderer_preference,
            supports_mediacodec_surface,
        );
        #[cfg(target_env = "ohos")]
        let player = Player::new_with_ohos_avcodec_surface(config.player, ohos_avcodec_surface);
        #[cfg(not(target_env = "ohos"))]
        let player = Player::new(config.player);
        let video_frames = player.subscribe_video_frames();
        let audio_frames = player.subscribe_audio_frames();
        let subtitle_frames = player.subscribe_subtitle_frames();
        let player_events = player.subscribe();
        let mut danmaku_session = config
            .danmaku
            .map(DanmakuSession::from_timeline)
            .unwrap_or_default();
        let danmaku_timeline = danmaku_session.active_timeline_clone();
        let danmaku_config = config.danmaku_config;
        let danmaku = DfmLayoutEngine::new(danmaku_timeline.clone(), danmaku_config.clone());
        let danmaku_planner =
            AsyncDanmakuPlanner::new(danmaku.clone(), danmaku_timeline, danmaku_config);
        Ok(Self {
            player,
            renderer,
            video_frames,
            audio_frames,
            subtitle_frames,
            player_events,
            audio_output: build_audio_output(config.audio),
            audio_configured: false,
            audio_started: false,
            last_audio_clock_report: None,
            last_audio_runtime_stats: AudioOutputRuntimeStats::default(),
            playback_rate: 1.0,
            pending_playback_rate: None,
            audio_only_tick_active: false,
            resume_pending: false,
            video_decode_resume_attempts: 0,
            latest_video_decoder: None,
            current_overlay: None,
            debug_hud: DebugHud::new(),
            current_danmaku: None,
            current_danmaku_prepared: None,
            danmaku_plan_replacement_pending: false,
            danmaku_resize_settle_at: None,
            danmaku_font_size_update: None,
            rejected_video_import_route: None,
            scene_luma_smoother: LumaSmoother::new(),
            last_scene_luma_generation: 0,
            current_media_time: Duration::ZERO,
            current_generation: 1,
            current_surface_metrics: None,
            current_danmaku_viewport: None,
            subtitle_font_scale: DEFAULT_SUBTITLE_FONT_SCALE,
            subtitle_resolution: SubtitleResolution::default(),
            subtitle_style: SubtitleStyleConfig::default(),
            subtitle_memory_fonts: SubtitleMemoryFonts::default(),
            subtitles: SubtitleFrameState::default(),
            overlay: config.overlay,
            render_test_pattern_when_idle: config.render_test_pattern_when_idle,
            danmaku_session,
            danmaku,
            danmaku_planner,
            danmaku_generation: 1,
            danmaku_trace: DanmakuTimeTrace::from_env(),
            stats: PresenterStats::default(),
            last_tick_duration: Duration::ZERO,
            last_pump_duration: Duration::ZERO,
            last_audio_pump_duration: Duration::ZERO,
            last_subtitle_pump_duration: Duration::ZERO,
            last_video_pump_duration: Duration::ZERO,
            last_clock_sync_duration: Duration::ZERO,
            last_danmaku_plan_duration: Duration::ZERO,
            last_render_duration: Duration::ZERO,
            last_render_current_duration: Duration::ZERO,
            last_render_test_duration: Duration::ZERO,
            presentation: Default::default(),
        })
    }

    pub fn player(&self) -> &Player {
        &self.player
    }

    pub fn attach_surface(&mut self, surface: PlatformSurface) -> Result<()> {
        let metrics = surface.metrics();
        self.renderer.attach_surface(surface)?;
        if let Err(error) = self.player.attach_surface(surface) {
            let _ = self.renderer.detach_surface();
            return Err(error);
        }
        self.current_surface_metrics = Some(metrics);
        self.clear_current_danmaku_state();
        Ok(())
    }

    pub fn detach_surface(&mut self) -> Result<()> {
        self.current_surface_metrics = None;
        self.clear_current_danmaku_state();
        self.player.detach_surface()?;
        self.renderer.detach_surface()
    }

    /// Borrowed COM identity of a DirectComposition swap chain. No `AddRef`.
    ///
    /// Compare this pointer across frames. Only call
    /// [`Self::composition_swapchain_iunknown`] when transferring a reference
    /// into `IDCompositionVisual::SetContent`.
    pub fn composition_swapchain_ptr(&self) -> Option<*mut std::ffi::c_void> {
        self.renderer.composition_swapchain_ptr()
    }

    /// AddRef'd `IUnknown` for a DirectComposition `SetContent` swap chain.
    ///
    /// The caller owns the reference and must `Release` it (for example by
    /// wrapping it in a host `DcompContent`).
    pub fn composition_swapchain_iunknown(&self) -> Option<*mut std::ffi::c_void> {
        self.renderer.composition_swapchain_iunknown()
    }

    /// AddRef'd `IUnknown` for the renderer-owned Windows Flutter texture.
    /// The caller owns the returned reference.
    pub fn windows_flutter_texture_iunknown(&self) -> Option<*mut std::ffi::c_void> {
        self.renderer.windows_flutter_texture_iunknown()
    }

    pub fn resize_surface(&mut self, width: u32, height: u32, scale: f64) -> Result<()> {
        let metrics = SurfaceMetrics::new(width, height, scale);
        let previous_metrics = self.current_surface_metrics;
        let redundant = previous_metrics == Some(metrics);
        self.danmaku_trace.surface_resize_calls =
            self.danmaku_trace.surface_resize_calls.saturating_add(1);
        if redundant {
            self.danmaku_trace.surface_resize_redundant = self
                .danmaku_trace
                .surface_resize_redundant
                .saturating_add(1);
        }
        self.renderer.resize_surface(metrics)?;
        self.current_surface_metrics = Some(metrics);
        let next_viewport = surface_metrics_to_viewport(metrics);
        let requires_relayout = self
            .current_danmaku_viewport
            .is_none_or(|current| danmaku_viewport_requires_relayout(current, next_viewport));
        if requires_relayout {
            // The menu animates this surface every display tick. Clearing the
            // prepared layout here blanks comments until the async worker can
            // catch up. Keep it moving, remap to the new surface, and prepare
            // only after the animation settles (also when playback is paused).
            self.danmaku_planner.invalidate_requests();
            self.danmaku_plan_replacement_pending =
                self.current_danmaku_prepared.is_some() || self.current_danmaku.is_some();
            self.danmaku_resize_settle_at = self
                .danmaku_plan_replacement_pending
                .then(|| Instant::now() + DANMAKU_RESIZE_SETTLE);
        }
        self.current_danmaku_viewport = Some(next_viewport);
        if let Some(plan) = self.current_danmaku.as_mut() {
            plan.retarget(next_viewport);
        }
        self.request_current_danmaku_plan_for_current_time();
        if self.danmaku_trace.enabled {
            let now = Instant::now();
            let periodic_sample_due = self
                .danmaku_trace
                .last_surface_resize_log_at
                .is_none_or(|last| last.elapsed() >= DANMAKU_MOTION_TRACE_INTERVAL);
            if !redundant || requires_relayout || periodic_sample_due {
                let line = format!(
                    "[erika-danmaku-surface] previous={} next={} viewport={} calls={} redundant={} flags=same:{} relayout:{}",
                    previous_metrics
                        .map(surface_metrics_label)
                        .unwrap_or_else(|| "-".to_string()),
                    surface_metrics_label(metrics),
                    viewport_label(next_viewport),
                    self.danmaku_trace.surface_resize_calls,
                    self.danmaku_trace.surface_resize_redundant,
                    redundant,
                    requires_relayout,
                );
                self.danmaku_trace.write_line(&line);
                self.danmaku_trace.last_surface_resize_log_at = Some(now);
            }
        }
        self.last_audio_clock_report = None;
        Ok(())
    }

    pub fn open(&mut self, media: MediaRequest) -> Result<()> {
        self.subtitle_resolution.reset_budget();
        self.quiesce_frame_output("open")?;
        self.reset_video_decode_resume_state();
        self.reset_audio_output_with_committed_rate();
        self.clear_playback_visual_state(Duration::ZERO, TransitionFramePolicy::Clear);
        self.drain_pending_player_frames();
        self.current_generation = self.current_generation.saturating_add(1).max(1);
        self.latest_video_decoder = None;
        self.presentation.reset();
        let result = self.player.open(media);
        let rate_result = if result.is_ok() && !playback_rate_matches(self.playback_rate, 1.0) {
            self.player.set_playback_rate(self.playback_rate)
        } else {
            Ok(())
        };
        // Player::open joins the previous producer before returning, so this
        // second drain deterministically removes anything it emitted between
        // the first drain and shutdown. The new engine is still paused.
        self.drain_pending_player_frames();
        result.and(rate_result)
    }

    pub fn play(&mut self) -> Result<()> {
        if self.player.is_stopped_at_end() {
            self.cancel_pending_video_decode_resume();
            self.reset_audio_output_with_committed_rate();
            self.drain_pending_player_frames();
            self.bump_danmaku_generation();
            self.clear_playback_visual_state(Duration::ZERO, TransitionFramePolicy::Clear);
        }
        self.player.play()
    }

    pub fn pause(&mut self) -> Result<()> {
        let result = self.player.pause();
        if self.pending_playback_rate.is_some() {
            // A pending transition is measured against wall time while the
            // output is running. Pausing that output would otherwise leave the
            // old-rate bridge queued but allow the stale deadline to commit on
            // the next play. Flush the bridge and commit the requested rate
            // while the clock is parked.
            self.reset_audio_output();
            let rate_result = self.commit_pending_playback_rate_now();
            return result.and(rate_result);
        }
        if let Err(error) = self.audio_output.pause() {
            self.stats.audio_failures += 1;
            trace::diagnostic_at(
                trace::LogLevel::Warn,
                format!("Erika presenter audio pause failed: {error}"),
            );
        }
        self.audio_started = false;
        self.last_audio_clock_report = None;
        result
    }

    pub fn is_playing(&self) -> bool {
        self.player.state() == crate::core::PlayerState::Playing
    }

    pub fn media_time(&self) -> Duration {
        self.player.current_media_time()
    }

    pub fn duration(&self) -> Option<Duration> {
        self.player.duration()
    }

    pub fn stop(&mut self) -> Result<()> {
        let quiesced = self.quiesce_frame_output("stop")?;
        let result = self.player.stop();
        self.cancel_pending_video_decode_resume();
        self.reset_audio_output_with_committed_rate();
        self.bump_danmaku_generation();
        self.clear_playback_visual_state(Duration::ZERO, TransitionFramePolicy::Clear);
        let transition = self.finish_frame_output_transition("stop", quiesced, true);
        result.and(transition)
    }

    pub fn close(&mut self) -> Result<()> {
        self.quiesce_frame_output("close")?;
        self.reset_video_decode_resume_state();
        self.reset_audio_output_with_committed_rate();
        self.bump_danmaku_generation();
        self.clear_playback_visual_state(Duration::ZERO, TransitionFramePolicy::Clear);
        self.drain_pending_player_frames();
        self.latest_video_decoder = None;
        let result = self.player.close();
        // Shutdown is joined at this point; no producer can refill a receiver.
        self.drain_pending_player_frames();
        result
    }

    pub fn seek(&mut self, position: Duration) -> Result<()> {
        let quiesced = self.quiesce_frame_output("seek")?;
        let result = self.player.seek(position);
        let rate_result = self.commit_pending_playback_rate_now();
        self.reset_audio_output();
        self.bump_danmaku_generation();
        self.clear_playback_visual_state(position, TransitionFramePolicy::PreserveRendererSnapshot);
        let transition = self.finish_frame_output_transition("seek", quiesced, true);
        result.and(rate_result).and(transition)
    }

    pub fn set_playback_rate(&mut self, rate: f64) -> Result<()> {
        let next_rate = normalize_playback_rate(rate);
        if playback_rate_request_is_idempotent(
            self.playback_rate,
            self.pending_playback_rate,
            next_rate,
        ) {
            return Ok(());
        }
        self.player.invalidate_audio_clock();
        self.audio_output.set_playback_rate(next_rate);
        self.last_audio_clock_report = None;

        let bridge = audio_transition_bridge(
            self.audio_output.clock_snapshot(),
            self.audio_output.queued_output_duration(),
        );
        if self.is_playing()
            && let Some(bridge) = bridge
        {
            self.pending_playback_rate = Some(PendingPlaybackRate {
                rate: next_rate,
                commit_at: Instant::now() + bridge,
            });
            return Ok(());
        }

        // A paused output keeps its queued PCM. Discard it before committing a
        // new rate so a later resume cannot play old-rate samples while the
        // player clock is already running at the new rate.
        if !self.is_playing() && bridge.is_some() {
            self.reset_audio_output();
        }
        if let Err(error) = self.player.set_playback_rate(next_rate) {
            self.audio_output.set_playback_rate(self.playback_rate);
            return Err(error);
        }
        self.playback_rate = next_rate;
        self.pending_playback_rate = None;
        Ok(())
    }

    pub fn set_volume(&mut self, volume: f64) {
        self.audio_output.set_volume(volume as f32);
    }

    pub fn volume(&self) -> f64 {
        self.audio_output.volume() as f64
    }

    pub fn set_debug_hud_enabled(&mut self, enabled: bool) {
        self.debug_hud.set_enabled(enabled);
    }

    pub fn debug_hud_enabled(&self) -> bool {
        self.debug_hud.enabled()
    }

    pub fn set_subtitle_scale(&mut self, scale: f64) {
        let scale = normalize_subtitle_font_scale(scale);
        if (self.subtitle_font_scale - scale).abs() < 0.001 {
            return;
        }
        self.subtitle_font_scale = scale;
        self.refresh_current_overlay();
    }

    /// Rasterize text subtitles at 2x precision without resizing the video or
    /// the displayed text. Enabled by default. Sustained subtitle CPU cost
    /// falls back to native resolution; large canvases also stay at native resolution.
    pub fn set_subtitle_supersampling(&mut self, enabled: bool) {
        self.subtitle_resolution.set_enabled(enabled);
        self.refresh_current_overlay();
    }

    /// Sets the fallback subtitle font. An empty family or path clears that
    /// half of the selection and restores the platform default.
    pub fn set_subtitle_font(&mut self, family: String, file_path: String) {
        let style = SubtitleStyleConfig {
            font_family: family,
            font_file_path: file_path,
            ..self.subtitle_style.clone()
        }
        .normalized();
        self.apply_subtitle_style(style);
    }

    pub fn set_subtitle_style(&mut self, style: SubtitleStyleConfig) {
        self.apply_subtitle_style(style);
    }

    pub fn subtitle_style(&self) -> &SubtitleStyleConfig {
        &self.subtitle_style
    }

    pub fn register_subtitle_font_bytes(&mut self, data: &[u8]) -> Result<u64> {
        if data.is_empty() || data.len() > MAX_MEMORY_SUBTITLE_FONT_BYTES {
            return Err(PlayerError::Playback(
                "subtitle memory font size is invalid".to_string(),
            ));
        }
        let registered_bytes = self
            .subtitle_memory_fonts
            .registered
            .values()
            .try_fold(0usize, |total, font| {
                total.checked_add(font.attachment.byte_len())
            });
        if registered_bytes
            .and_then(|total| total.checked_add(data.len()))
            .is_none_or(|total| total > MAX_MEMORY_SUBTITLE_FONT_TOTAL_BYTES)
        {
            return Err(PlayerError::Playback(
                "subtitle memory font total byte limit exceeded".to_string(),
            ));
        }
        let mut database = fontdb::Database::new();
        database.load_font_data(data.to_vec());
        let faces = database
            .faces()
            .map(|face| SubtitleMemoryFontFace {
                index: face.index,
                families: face
                    .families
                    .iter()
                    .map(|(family, _)| family.clone())
                    .collect(),
                post_script_name: face.post_script_name.clone(),
                weight: face.weight.0,
                italic: face.style == fontdb::Style::Italic,
                monospaced: face.monospaced,
            })
            .collect::<Vec<_>>();
        let mut families = faces
            .iter()
            .flat_map(|face| face.families.iter().cloned())
            .collect::<Vec<_>>();
        families.sort();
        families.dedup();
        if families.is_empty() {
            return Err(PlayerError::Playback(
                "subtitle memory font contains no faces".to_string(),
            ));
        }
        self.subtitle_memory_fonts.next_id = self.subtitle_memory_fonts.next_id.saturating_add(1);
        let id = self.subtitle_memory_fonts.next_id.max(1);
        self.subtitle_memory_fonts.registered.insert(
            id,
            SubtitleMemoryFontEntry {
                attachment: SubtitleFontAttachment::new(
                    format!("memory-subtitle-font-{id}"),
                    None,
                    families,
                    Arc::<[u8]>::from(data),
                ),
                faces,
            },
        );
        self.bump_subtitle_memory_font_registry_generation();
        Ok(id)
    }

    pub fn select_subtitle_memory_fonts(&mut self, ids: &[u64]) -> Result<()> {
        let mut seen = HashSet::with_capacity(ids.len());
        if ids.iter().any(|id| {
            *id == 0 || !self.subtitle_memory_fonts.registered.contains_key(id) || !seen.insert(*id)
        }) {
            return Err(PlayerError::Playback(
                "subtitle memory font selection contains an invalid id".to_string(),
            ));
        }
        if self.subtitle_memory_fonts.selected_ids == ids {
            return Ok(());
        }
        self.subtitle_memory_fonts.selected_ids.clear();
        self.subtitle_memory_fonts
            .selected_ids
            .extend_from_slice(ids);
        self.bump_subtitle_memory_font_selection_generation();
        Ok(())
    }

    pub fn clear_subtitle_memory_fonts(&mut self) {
        if self.subtitle_memory_fonts.registered.is_empty() {
            return;
        }
        self.subtitle_memory_fonts.registered.clear();
        self.subtitle_memory_fonts.selected_ids.clear();
        self.bump_subtitle_memory_font_selection_generation();
    }

    pub fn subtitle_memory_font_status(&self) -> SubtitleMemoryFontStatus {
        SubtitleMemoryFontStatus {
            registered_count: self.subtitle_memory_fonts.registered.len(),
            registered_bytes: self
                .subtitle_memory_fonts
                .registered
                .values()
                .map(|font| font.attachment.byte_len())
                .sum(),
            selected_count: self.subtitle_memory_fonts.selected_ids.len(),
            generation: self.subtitle_memory_fonts.generation,
            selected_ids: self.subtitle_memory_fonts.selected_ids.clone(),
        }
    }

    pub fn subtitle_memory_font_info(&self, id: u64) -> Option<SubtitleMemoryFontInfo> {
        let font = self.subtitle_memory_fonts.registered.get(&id)?;
        Some(SubtitleMemoryFontInfo {
            id,
            byte_len: font.attachment.byte_len(),
            faces: font.faces.clone(),
        })
    }

    fn bump_subtitle_memory_font_registry_generation(&mut self) {
        self.subtitle_memory_fonts.generation =
            self.subtitle_memory_fonts.generation.saturating_add(1);
    }

    fn bump_subtitle_memory_font_selection_generation(&mut self) {
        self.bump_subtitle_memory_font_registry_generation();
        self.subtitle_memory_fonts.selection_generation = self
            .subtitle_memory_fonts
            .selection_generation
            .saturating_add(1);
        let selection = self.danmaku_font_selection();
        self.danmaku.set_font_selection(selection.clone());
        self.danmaku_planner.set_font_selection(selection);
        self.clear_current_danmaku_state();
        self.bump_danmaku_generation();
        self.refresh_current_overlay();
    }

    fn danmaku_font_selection(&self) -> DanmakuFontSelection {
        let fonts = self
            .subtitle_memory_fonts
            .selected_ids
            .iter()
            .filter_map(|id| {
                self.subtitle_memory_fonts
                    .registered
                    .get(id)
                    .map(|font| font.attachment.clone())
            })
            .collect::<Vec<_>>();
        DanmakuFontSelection::new(
            self.subtitle_memory_fonts.selection_generation,
            Arc::from(fonts),
        )
    }

    fn apply_subtitle_style(&mut self, style: SubtitleStyleConfig) {
        let style = style.normalized();
        if self.subtitle_style == style {
            return;
        }
        self.subtitle_style = style;
        self.refresh_current_overlay();
    }

    pub fn set_danmaku_timeline(&mut self, timeline: DanmakuTimeline) {
        self.danmaku_session.replace_default_track(
            timeline,
            "default",
            DanmakuTrackSource::Unknown,
        );
        self.sync_danmaku_engine_timeline();
        self.bump_danmaku_generation();
    }

    pub fn add_danmaku_track(
        &mut self,
        timeline: DanmakuTimeline,
        name: impl Into<String>,
        source: DanmakuTrackSource,
        offset_micros: i64,
    ) -> u64 {
        let track_id =
            self.danmaku_session
                .add_track_with_offset(timeline, name, source, offset_micros);
        self.sync_danmaku_engine_timeline();
        self.bump_danmaku_generation();
        track_id
    }

    pub fn remove_danmaku_track(&mut self, track_id: u64) -> bool {
        let removed = self.danmaku_session.remove_track(track_id);
        if removed {
            self.sync_danmaku_engine_timeline();
            self.bump_danmaku_generation();
        }
        removed
    }

    pub fn set_danmaku_track_enabled(&mut self, track_id: u64, enabled: bool) -> bool {
        let updated = self.danmaku_session.set_track_enabled(track_id, enabled);
        if updated {
            self.sync_danmaku_engine_timeline();
            self.bump_danmaku_generation();
        }
        updated
    }

    pub fn set_danmaku_track_offset(&mut self, track_id: u64, offset_micros: i64) -> bool {
        let updated = self
            .danmaku_session
            .set_track_offset(track_id, offset_micros);
        if updated {
            self.sync_danmaku_engine_timeline();
            self.bump_danmaku_generation();
        }
        updated
    }

    pub fn set_danmaku_global_offset(&mut self, offset_micros: i64) {
        if self.danmaku_session.global_offset() == offset_micros {
            return;
        }
        self.danmaku_session.set_global_offset(offset_micros);
        self.sync_danmaku_engine_timeline();
        self.bump_danmaku_generation();
    }

    pub fn danmaku_tracks(&self) -> Vec<DanmakuTrackInfo> {
        self.danmaku_session.track_infos()
    }

    pub fn clear_danmaku(&mut self) {
        self.danmaku_session.clear();
        self.danmaku.clear_timeline();
        self.danmaku_planner.clear_timeline();
        self.clear_current_danmaku_state();
        self.bump_danmaku_generation();
    }

    pub fn set_danmaku_enabled(&mut self, enabled: bool) {
        let mut config = self.danmaku.config().clone();
        config.enabled = enabled;
        self.set_danmaku_config(config);
    }

    pub fn set_danmaku_font(&mut self, family: impl Into<String>, file_path: impl Into<String>) {
        let mut config = self.danmaku.config().clone();
        config.custom_font_family = family.into();
        config.custom_font_file_path = file_path.into();
        self.set_danmaku_config(config);
    }

    pub fn set_danmaku_config(&mut self, config: DanmakuLayoutConfig) {
        let config = config.sanitized();
        let mut previous_with_size = self.danmaku.config().clone();
        previous_with_size.font_size = config.font_size;
        let size_only = previous_with_size == config && self.current_danmaku_prepared.is_some();
        match self.danmaku.apply_config(config.clone()) {
            DanmakuConfigChange::Unchanged => {}
            DanmakuConfigChange::PaintOnly => {
                self.danmaku_planner.set_paint_config(config);
                if let Some(prepared) = &mut self.current_danmaku_prepared {
                    prepared.prepared.apply_paint_config(self.danmaku.config());
                }
                if self.danmaku.config().enabled {
                    self.refresh_current_danmaku_plan_from_prepared();
                } else {
                    // A layout that was already being prepared may still
                    // complete after this call. Stop requesting more work and
                    // make every subsequent refresh keep the surface empty.
                    self.danmaku_planner.invalidate_requests();
                    self.current_danmaku = None;
                    self.danmaku_plan_replacement_pending = false;
                }
            }
            DanmakuConfigChange::Layout => {
                if let Some(prepared) = &mut self.current_danmaku_prepared {
                    prepared.prepared.apply_paint_config(&config);
                }
                if size_only {
                    // Size-only results remain usable while dragging: scale them
                    // to the latest desired size. Keeping this generation also
                    // prevents continuous input from starving a long-running plan.
                    DanmakuFontSizeUpdate::retarget(
                        &mut self.danmaku_font_size_update,
                        Instant::now(),
                    );
                    self.refresh_current_danmaku_plan_from_prepared();
                    self.request_current_danmaku_plan_for_current_time();
                    return;
                }
                self.danmaku_font_size_update = None;
                self.danmaku_planner
                    .set_config(config, self.danmaku.rasterizer_clone());
                self.bump_danmaku_generation_for_config_change();
                self.refresh_current_danmaku_plan_from_prepared();
                // A paused player may not produce another video frame. Keep the
                // existing surface viewport and enqueue the replacement plan now so
                // display ticks can redraw the retained frame with the new settings.
                self.request_current_danmaku_plan_for_current_time();
            }
        }
    }

    pub fn danmaku_config(&self) -> Option<&DanmakuLayoutConfig> {
        Some(self.danmaku.config())
    }

    /// Switches the neural luma upscaler at runtime. Backends without an
    /// upscaler implementation ignore the request.
    pub fn set_luma_upscaler(&mut self, mode: crate::renderer::pipeline::LumaUpscalerMode) {
        self.renderer.set_luma_upscaler(mode);
    }

    pub fn set_output_headroom(&mut self, headroom: f32, known: bool) {
        self.renderer.set_output_headroom(headroom, known);
    }

    pub fn add_external_subtitle(&self, uri: impl Into<String>) -> Result<SubtitleTrackConfig> {
        self.player.add_external_subtitle(uri)
    }

    pub fn remove_subtitle_track(&self, track_id: i64) -> Result<()> {
        self.player.remove_subtitle_track(track_id)
    }

    pub fn select_audio_track(&mut self, track_id: Option<i64>) -> Result<()> {
        let quiesced = self.quiesce_frame_output("select_audio_track")?;
        let result = self.player.select_audio_track(track_id);
        let rate_result = self.commit_pending_playback_rate_now();
        self.reset_audio_output();
        self.bump_danmaku_generation();
        self.clear_playback_visual_state(
            self.current_media_time,
            TransitionFramePolicy::PreserveTrackSwitchFrame,
        );
        let transition = self.finish_frame_output_transition("select_audio_track", quiesced, true);
        result.and(rate_result).and(transition)
    }

    pub fn select_subtitle_track(&mut self, track_id: Option<i64>) -> Result<()> {
        if !self.player.tracks().is_empty() && self.player.track_selection().subtitle == track_id {
            return Ok(());
        }
        if track_id.is_none() && !self.player.has_open_media() {
            // Nothing is open, so no subtitle is shown: nothing to quiesce.
            return self.player.select_subtitle_track(None);
        }
        let quiesced = self.quiesce_frame_output("select_subtitle_track")?;
        let result = self.player.select_subtitle_track(track_id);
        let rate_result = self.commit_pending_playback_rate_now();
        self.reset_audio_output();
        self.bump_danmaku_generation();
        self.clear_playback_visual_state(
            self.current_media_time,
            TransitionFramePolicy::PreserveTrackSwitchFrame,
        );
        let transition =
            // Selecting a track can emit its current cue while output is
            // quiesced. Keep that cue; stale audio/subtitle generations are
            // filtered by their normal pumps after the barrier.
            self.finish_frame_output_transition("select_subtitle_track", quiesced, false);
        result.and(rate_result).and(transition)
    }

    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.player.tracks()
    }

    pub fn track_selection(&self) -> TrackSelection {
        self.player.track_selection()
    }

    pub fn render_tick(&mut self, time_seconds: f64) -> Result<PresenterStats> {
        self.render_tick_with_presentation_time(time_seconds, None)
    }

    /// Render using the display's target instant, independent of queue/pump
    /// latency. This only samples the shared playback clock; it never advances
    /// or re-anchors that clock, including while paused or after a seek.
    pub fn render_tick_at(
        &mut self,
        time_seconds: f64,
        presentation_time: Instant,
    ) -> Result<PresenterStats> {
        self.render_tick_with_presentation_time(time_seconds, Some(presentation_time))
    }

    fn render_tick_with_presentation_time(
        &mut self,
        time_seconds: f64,
        presentation_time: Option<Instant>,
    ) -> Result<PresenterStats> {
        if self.audio_only_tick_active {
            if self.resume_pending {
                self.try_resume_video_decode();
            } else if self.video_decode_resume_attempts < MAX_VIDEO_DECODE_RESUME_ATTEMPTS {
                // Returning to the foreground can race the layer's first
                // drawable. Arm the resume on this tick and let a later tick
                // perform the decoder seek/flush after the surface is ready.
                self.discard_pending_video_frames();
                self.resume_pending = true;
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_video_decode",
                        "stage": "resume_pending",
                        "reason": "foreground_render_tick",
                    })
                    .to_string(),
                );
            }
            // Once the attempt budget is spent the resume stays disarmed until
            // the next background tick opens a new foreground window, so a
            // wedged worker cannot be re-probed on every display frame.
        } else {
            self.resume_pending = false;
            self.video_decode_resume_attempts = 0;
        }
        let tick_started = Instant::now();
        self.presentation
            .on_tick(presentation_time.unwrap_or(tick_started));
        self.commit_pending_playback_rate()?;
        let pump_started = Instant::now();
        self.refresh_video_decoder_status();

        let subtitle_started = Instant::now();
        self.pump_subtitles();
        self.last_subtitle_pump_duration = subtitle_started.elapsed();

        let video_started = Instant::now();
        self.pump_video();
        self.last_video_pump_duration = video_started.elapsed();

        let audio_started = Instant::now();
        // Publish a callback-observed disconnection before clock/push recovery
        // advances the backend to recovering or stable during this tick.
        self.report_audio_output_runtime_stats();
        self.pump_audio();
        self.report_audio_output_runtime_stats();
        self.last_audio_pump_duration = audio_started.elapsed();

        let sync_started = Instant::now();
        let _snapshot = self.sync_media_time_from_player_at(presentation_time);
        self.last_clock_sync_duration = sync_started.elapsed();

        let plan_started = Instant::now();
        self.refresh_stale_danmaku_plan();
        self.trace_current_danmaku_motion(time_seconds);
        self.last_danmaku_plan_duration = plan_started.elapsed();
        self.last_pump_duration = pump_started.elapsed();
        let plan_time = self.current_danmaku.as_ref().map(|plan| plan.media_time);
        let plan_generation = self.current_danmaku.as_ref().map(|plan| plan.generation);
        let plan_items = self
            .current_danmaku
            .as_ref()
            .map_or(0, |plan| plan.items.len());
        self.trace_danmaku_time(
            "render_context",
            self.current_media_time,
            self.current_generation,
            Some(self.player.current_media_time()),
            None,
            plan_time,
            plan_generation,
            plan_items,
        );

        let hud_overlay = if self.debug_hud.enabled() {
            let hud_snapshot = self.debug_hud_snapshot();
            let hud_viewport = self
                .current_overlay
                .as_ref()
                .map(|overlay| overlay.viewport)
                .or_else(|| {
                    self.current_surface_metrics.map(|metrics| {
                        OverlayViewport::new(
                            metrics.physical_extent.width,
                            metrics.physical_extent.height,
                        )
                    })
                });
            let hud_plane = hud_viewport.and_then(|viewport| {
                self.debug_hud
                    .update(
                        Instant::now(),
                        viewport.width,
                        viewport.height,
                        hud_snapshot,
                    )
                    .cloned()
            });
            hud_plane.map(|plane| {
                let viewport = hud_viewport.expect("HUD requires a viewport");
                let mut overlay = self
                    .current_overlay
                    .clone()
                    .unwrap_or_else(|| OverlayFrame {
                        pts: self.current_media_time,
                        viewport,
                        subtitle_planes: Vec::new(),
                        subtitle_alpha_planes: Vec::new(),
                        subtitle_changed: false,
                    });
                overlay.subtitle_planes.push(plane);
                overlay
            })
        } else {
            None
        };
        let render_overlay = hud_overlay.as_ref().or(self.current_overlay.as_ref());
        let context = RenderFrameContext::new(self.current_media_time, self.current_generation)
            .overlay(render_overlay)
            .danmaku(self.current_danmaku.as_ref())
            .output_size(
                self.current_surface_metrics
                    .map_or(0, |metrics| metrics.physical_extent.width),
                self.current_surface_metrics
                    .map_or(0, |metrics| metrics.physical_extent.height),
            );
        if self.resume_pending && !self.surface_is_ready() {
            self.last_render_duration = Duration::ZERO;
            self.last_render_current_duration = Duration::ZERO;
            self.last_render_test_duration = Duration::ZERO;
            self.last_tick_duration = tick_started.elapsed();
            return Ok(self.stats);
        }
        let render_started = Instant::now();
        let render_result = self.renderer.render_current_frame(context);
        self.last_render_current_duration = render_started.elapsed();
        self.last_render_duration = self.last_render_current_duration;
        self.last_render_test_duration = Duration::ZERO;
        match render_result {
            Ok(true) => {
                self.stats.rendered_video_frames += 1;
                self.presentation.on_render(
                    presentation_time.unwrap_or_else(Instant::now),
                    true,
                    self.is_playing(),
                    self.playback_rate,
                );
            }
            Ok(false) => {
                if self.render_test_pattern_when_idle {
                    let render_started = Instant::now();
                    self.renderer.render_test_frame(time_seconds)?;
                    self.last_render_test_duration = render_started.elapsed();
                    self.last_render_duration = self.last_render_test_duration;
                    self.stats.rendered_test_frames += 1;
                }
            }
            Err(error) => {
                self.stats.render_failures += 1;
                return Err(error);
            }
        }

        self.last_tick_duration = tick_started.elapsed();
        if trace::enabled() {
            let renderer = self.renderer.runtime_stats();
            trace::log(format!(
                "[erika-presenter-trace] stage=render_tick media={} player={} gen={} playing={} tick_ms={:.3} pump_ms={:.3} audio_ms={:.3} subtitle_ms={:.3} video_ms={:.3} clock_ms={:.3} plan_ms={:.3} render_ms={:.3} render_current_ms={:.3} render_test_ms={:.3} stats_video={} stats_audio={} stats_subtitle={} stats_overlay={} renderer_rendered={} renderer_offscreen={} renderer_gpu_ms={:.3} audio_queued={} audio_queued_ms={} audio_underflow={} output={}x{} danmaku_items={}",
                duration_label(Some(self.current_media_time)),
                duration_label(Some(self.player.current_media_time())),
                self.current_generation,
                self.is_playing(),
                self.last_tick_duration.as_secs_f64() * 1000.0,
                self.last_pump_duration.as_secs_f64() * 1000.0,
                self.last_audio_pump_duration.as_secs_f64() * 1000.0,
                self.last_subtitle_pump_duration.as_secs_f64() * 1000.0,
                self.last_video_pump_duration.as_secs_f64() * 1000.0,
                self.last_clock_sync_duration.as_secs_f64() * 1000.0,
                self.last_danmaku_plan_duration.as_secs_f64() * 1000.0,
                self.last_render_duration.as_secs_f64() * 1000.0,
                self.last_render_current_duration.as_secs_f64() * 1000.0,
                self.last_render_test_duration.as_secs_f64() * 1000.0,
                self.stats.decoded_video_frames,
                self.stats.pushed_audio_frames,
                self.stats.decoded_subtitle_frames,
                self.stats.overlay_frames,
                renderer.rendered_frames,
                renderer.offscreen_frames,
                renderer.last_gpu_duration.as_secs_f64() * 1000.0,
                self.audio_output
                    .clock_snapshot()
                    .map(|snapshot| snapshot.queued_frames)
                    .unwrap_or(0),
                self.audio_output
                    .clock_snapshot()
                    .and_then(|snapshot| snapshot.queued_duration)
                    .map(|duration| duration.as_secs_f64() * 1000.0)
                    .unwrap_or(0.0),
                self.audio_output
                    .clock_snapshot()
                    .map(|snapshot| snapshot.underflow_frames)
                    .unwrap_or(0),
                self.current_surface_metrics
                    .map_or(0, |metrics| metrics.physical_extent.width),
                self.current_surface_metrics
                    .map_or(0, |metrics| metrics.physical_extent.height),
                self.current_danmaku
                    .as_ref()
                    .map_or(0, |plan| plan.items.len()),
            ));
        }
        Ok(self.stats)
    }

    pub fn audio_only_tick(&mut self) -> Result<PresenterStats> {
        let tick_started = Instant::now();
        self.commit_pending_playback_rate()?;
        // A background tick starts a new foreground-resume window. If a
        // previous foreground attempt failed, do not carry its pending flag or
        // its spent attempt budget into the next foreground frame.
        self.resume_pending = false;
        self.video_decode_resume_attempts = 0;
        if !self.audio_only_tick_active {
            self.player.set_video_decode_suspended(true)?;
            self.discard_pending_video_frames();
            self.audio_only_tick_active = true;
        }
        let pump_started = Instant::now();
        self.last_subtitle_pump_duration = Duration::ZERO;
        self.last_video_pump_duration = Duration::ZERO;
        self.report_audio_output_runtime_stats();
        let audio_started = Instant::now();
        self.pump_audio();
        self.report_audio_output_runtime_stats();
        self.last_audio_pump_duration = audio_started.elapsed();
        let sync_started = Instant::now();
        self.sync_media_time_from_player();
        self.last_clock_sync_duration = sync_started.elapsed();
        self.last_danmaku_plan_duration = Duration::ZERO;
        self.last_render_duration = Duration::ZERO;
        self.last_render_current_duration = Duration::ZERO;
        self.last_render_test_duration = Duration::ZERO;
        self.last_pump_duration = pump_started.elapsed();
        self.last_tick_duration = tick_started.elapsed();
        Ok(self.stats)
    }

    fn discard_pending_video_frames(&self) {
        while self.video_frames.try_recv().is_ok() {}
    }

    /// Clears the resume bookkeeping across a boundary that replaces or
    /// retires the playback engine.
    ///
    /// `open` builds a fresh engine and `close` retires the worker, and a new
    /// engine starts with video decode running, so the presenter owes it no
    /// resume.
    fn reset_video_decode_resume_state(&mut self) {
        self.audio_only_tick_active = false;
        self.resume_pending = false;
        self.video_decode_resume_attempts = 0;
    }

    /// Drops an in-flight resume attempt without forgetting that the worker
    /// still has video decode suspended.
    ///
    /// `stop` and a stopped-at-end replay reuse the current engine, and
    /// neither clears its `video_decode_suspended` flag. Clearing
    /// `audio_only_tick_active` here would desynchronize the presenter from
    /// the worker: no later foreground tick would owe a resume, so playback
    /// would stay audio-only with a frozen frame until the app is backgrounded
    /// again. Resetting the budget lets the next foreground window retry from
    /// scratch.
    fn cancel_pending_video_decode_resume(&mut self) {
        self.resume_pending = false;
        self.video_decode_resume_attempts = 0;
    }

    fn surface_is_ready(&self) -> bool {
        let Some(metrics) = self.current_surface_metrics else {
            return false;
        };
        let renderer = self.renderer.runtime_stats();
        metrics.physical_extent.width > 0
            && metrics.physical_extent.height > 0
            && renderer.attached
            && renderer.surface_width > 0
            && renderer.surface_height > 0
    }

    fn try_resume_video_decode(&mut self) {
        if !self.surface_is_ready() {
            return;
        }

        self.discard_pending_video_frames();
        match self.player.set_video_decode_suspended(false) {
            Ok(_) => {
                self.audio_only_tick_active = false;
                self.resume_pending = false;
                self.video_decode_resume_attempts = 0;
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_video_decode",
                        "stage": "resumed_at_keyframe",
                    })
                    .to_string(),
                );
            }
            Err(error) => self.note_video_decode_resume_failure(&error.to_string()),
        }
    }

    /// Accounts for one failed foreground resume and decides whether the next
    /// display tick may try again.
    ///
    /// A worker timeout or transient lifecycle race must not turn the render
    /// loop into a terminal C ABI error, so the attempt is retried instead of
    /// propagated. The budget bounds that retry: a disconnected or hung
    /// playback worker never recovers, and probing it on every display frame
    /// would send one IPC command and one seek/flush per frame while each
    /// attempt can stall the render queue for the frame-output barrier
    /// timeout.
    fn note_video_decode_resume_failure(&mut self, reason: &str) {
        self.video_decode_resume_attempts = self.video_decode_resume_attempts.saturating_add(1);
        let exhausted = self.video_decode_resume_attempts >= MAX_VIDEO_DECODE_RESUME_ATTEMPTS;
        if exhausted {
            // Leave the decoder suspended and keep rendering the last frame
            // with audio; a later background/foreground cycle reopens the
            // window with a fresh budget.
            self.resume_pending = false;
        }
        trace::diagnostic(
            serde_json::json!({
                "event": "player_video_decode",
                "stage": if exhausted {
                    "resume_abandoned"
                } else {
                    "resume_retry_pending"
                },
                "reason": reason,
                "attempt": self.video_decode_resume_attempts,
                "maxAttempts": MAX_VIDEO_DECODE_RESUME_ATTEMPTS,
            })
            .to_string(),
        );
    }

    fn debug_hud_snapshot(&self) -> DebugHudSnapshot {
        let selection = self.player.track_selection();
        let tracks = self.player.tracks();
        let selected = selection
            .video
            .and_then(|selected| tracks.iter().find(|track| track.id == selected));
        let selected_audio = selection
            .audio
            .and_then(|selected| tracks.iter().find(|track| track.id == selected));
        let renderer = self.renderer.runtime_stats();
        let clock = self.audio_output.clock_snapshot();
        let output = self.renderer.output_status();
        let audio_runtime = self.audio_output.runtime_stats();
        let surface = self.current_surface_metrics;
        let decoder = self.latest_video_decoder.as_ref();
        DebugHudSnapshot {
            codec: selected.as_ref().and_then(|track| track.codec.clone()),
            width: selected.as_ref().map_or(0, |track| track.width),
            height: selected.as_ref().map_or(0, |track| track.height),
            bit_rate: selected.as_ref().and_then(|track| track.bit_rate),
            nominal_fps: selected
                .as_ref()
                .and_then(|track| track.frame_rate.as_ref())
                .map(|rate| rate.frames_per_second()),
            pixel_format: selected
                .as_ref()
                .and_then(|track| track.pixel_format.clone()),
            profile: selected.as_ref().and_then(|track| track.profile.clone()),
            decoder_requested_backend: decoder
                .map(|event| event.requested_backend.as_str().to_string()),
            decoder_previous_backend: decoder
                .and_then(|event| event.previous_backend)
                .map(|backend| backend.as_str().to_string()),
            decoder_active_backend: decoder.map(|event| event.active_backend.as_str().to_string()),
            decoder_codec: decoder.and_then(|event| event.codec.clone()),
            decoder_pixel_format: decoder.and_then(|event| event.pixel_format.clone()),
            decoder_line_sizes: decoder.and_then(|event| event.line_sizes),
            decoder_fallback_count: decoder.map_or(0, |event| event.fallback_count),
            decoder_stage: decoder.map(|event| event.stage.clone()),
            decoder_reason: decoder.and_then(|event| event.reason.clone()),
            player_state: format!("{:?}", self.player.state()).to_lowercase(),
            media_time: self.current_media_time,
            duration: self.player.duration(),
            playback_rate: self.playback_rate,
            surface_width: surface.map_or(0, |metrics| metrics.physical_extent.width),
            surface_height: surface.map_or(0, |metrics| metrics.physical_extent.height),
            decoded_video_frames: self.stats.decoded_video_frames,
            rendered_video_frames: self.stats.rendered_video_frames,
            dropped_video_frames: self.stats.video_frame_backpressure_drops,
            hardware_video_frames: renderer.hardware_video_frames,
            software_video_frames: renderer.software_video_frames,
            zero_copy_video_frames: renderer.zero_copy_video_frames,
            direct_zero_copy_video_frames: renderer.direct_zero_copy_video_frames,
            shared_handle_video_frames: renderer.shared_handle_video_frames,
            cpu_video_frame_fallbacks: renderer.cpu_video_frame_fallbacks,
            import_failures: self.stats.import_failures,
            render_failures: self.stats.render_failures,
            render_duration: self.last_render_duration,
            gpu_duration: renderer.last_gpu_duration,
            audio_queued_frames: clock.map_or(0, |snapshot| snapshot.queued_frames),
            audio_queued_duration: clock.and_then(|snapshot| snapshot.queued_duration),
            audio_underflow_frames: clock.map_or(0, |snapshot| snapshot.underflow_frames),
            audio_codec: selected_audio
                .as_ref()
                .and_then(|track| track.codec.clone()),
            audio_sample_rate: selected_audio.as_ref().map_or(0, |track| track.sample_rate),
            audio_channels: selected_audio.as_ref().map_or(0, |track| track.channels),
            audio_recovery_state: audio_runtime.recovery_state.as_str().to_string(),
            hdr_output_active: renderer.hdr10_output_active,
            output_encoding: output.active_encoding.label().to_string(),
            output_format: output.surface_format.label().to_string(),
            output_headroom: output.active_headroom,
            output_fallback: output.fallback_reason.label().to_string(),
            danmaku_items: self
                .current_danmaku
                .as_ref()
                .map_or(0, |plan| plan.items.len()),
        }
    }

    fn refresh_video_decoder_status(&mut self) {
        while let Ok(event) = self.player_events.try_recv() {
            if let crate::core::PlayerEvent::VideoDecoderChanged(event) = event {
                self.latest_video_decoder = Some(event);
            }
        }
    }

    pub fn capture_frame_rgba(&mut self, width: u32, height: u32) -> Result<Option<Vec<u8>>> {
        if width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "capture size must be non-zero".to_string(),
            ));
        }

        self.pump_subtitles();
        self.pump_video();
        self.sync_media_time_from_player();

        // Capture is a real render target with its own pixel viewport. Reusing
        // the surface-sized overlay here would scale a stale subtitle atlas, so
        // rebuild the regular subtitle overlay for the requested target.
        //
        // Danmaku is deliberately not attached to this offscreen context:
        // screenshots are used as watch-history thumbnails and must represent
        // the video rather than transient on-screen comments. The presentation
        // context in `render_current` still attaches `current_danmaku`.
        let capture_overlay = self.capture_overlay(width, height);

        let context = RenderFrameContext::new(self.current_media_time, self.current_generation)
            .overlay(Some(&capture_overlay))
            .output_size(width, height);
        self.renderer
            .capture_current_frame(context, width, height)
            .map(|capture| capture.map(|capture| capture.rgba))
    }

    pub fn take_flutter_frame(&mut self) -> Option<crate::core::RendererFrameCapture> {
        self.renderer.take_flutter_frame()
    }

    /// Selects the external Flutter texture written by the next render tick.
    ///
    /// The platform plugin owns the texture and keeps it alive for the entire
    /// call. The renderer retains the native object until another buffer is
    /// selected or the surface is detached.
    pub fn set_flutter_texture_buffer(
        &mut self,
        raw_texture: u64,
        width: u32,
        height: u32,
    ) -> Result<()> {
        if raw_texture == 0 || width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "Flutter texture buffer and dimensions must be non-zero".to_string(),
            ));
        }
        self.renderer
            .set_flutter_texture_buffer(raw_texture, width, height)
    }

    fn capture_overlay(&mut self, width: u32, height: u32) -> OverlayFrame {
        let capture_overlay_viewport = OverlayViewport::new(width, height);
        let mut capture_overlay = self
            .overlay
            .render(self.current_media_time, capture_overlay_viewport);
        let subtitle_style = self.subtitle_ass_style(capture_overlay.viewport);
        self.subtitles.append_to_overlay(
            self.current_media_time,
            &mut capture_overlay,
            &subtitle_style,
        );
        capture_overlay
    }

    pub fn stats(&self) -> PresenterStats {
        self.stats
    }

    pub fn runtime_snapshot(&self) -> PresenterRuntimeSnapshot {
        let renderer = self.renderer.runtime_stats();
        let resources = self.renderer.resource_stats();
        let output = self.renderer.output_status();
        let (current_danmaku_items, current_danmaku_atlas_version, current_danmaku_atlas_bytes) =
            self.current_danmaku.as_ref().map_or((0, 0, 0), |plan| {
                let atlas = plan.atlas.as_ref();
                (
                    plan.items.len(),
                    atlas.map_or(0, |atlas| atlas.version),
                    atlas.map_or(0, |atlas| atlas.required_len().saturating_mul(2)),
                )
            });
        let frame_stats = self
            .current_danmaku
            .as_ref()
            .map_or(Default::default(), |plan| plan.frame_stats);
        let audio_snapshot = self.audio_output.clock_snapshot();
        PresenterRuntimeSnapshot {
            stats: self.stats,
            renderer,
            resources,
            output,
            audio_output_queued_duration: audio_snapshot
                .and_then(|snapshot| snapshot.queued_duration),
            audio_output_queued_frames: audio_snapshot.map_or(0, |snapshot| snapshot.queued_frames),
            audio_output_read_frames: audio_snapshot.map_or(0, |snapshot| snapshot.read_frames),
            audio_output_written_frames: audio_snapshot
                .map_or(0, |snapshot| snapshot.written_frames),
            audio_output_underflow_frames: audio_snapshot
                .map_or(0, |snapshot| snapshot.underflow_frames),
            audio_output_runtime_stats: self.audio_output.runtime_stats(),
            media_time: self.current_media_time,
            generation: self.current_generation,
            playing: self.is_playing(),
            current_danmaku_items,
            current_danmaku_atlas_version,
            current_danmaku_atlas_bytes,
            current_danmaku_viewport_width: self
                .current_danmaku_viewport
                .map_or(0, |viewport| viewport.width),
            current_danmaku_viewport_height: self
                .current_danmaku_viewport
                .map_or(0, |viewport| viewport.height),
            current_danmaku_placed_items: frame_stats.placed_items,
            current_danmaku_scroll_items: frame_stats.scroll_items,
            current_danmaku_top_items: frame_stats.top_items,
            current_danmaku_bottom_items: frame_stats.bottom_items,
            current_danmaku_scroll_rows: frame_stats.scroll_rows,
            current_danmaku_scroll_track_min: frame_stats.scroll_track_min,
            current_danmaku_scroll_track_max: frame_stats.scroll_track_max,
            current_danmaku_scroll_min_y: frame_stats.scroll_min_y,
            current_danmaku_scroll_max_y: frame_stats.scroll_max_y,
            current_danmaku_scroll_bucket_count: frame_stats.scroll_bucket_count,
            current_danmaku_scroll_buckets: frame_stats.scroll_buckets,
            current_danmaku_prepared: frame_stats.prepared,
            last_tick_duration: self.last_tick_duration,
            last_pump_duration: self.last_pump_duration,
            last_audio_pump_duration: self.last_audio_pump_duration,
            last_subtitle_pump_duration: self.last_subtitle_pump_duration,
            last_video_pump_duration: self.last_video_pump_duration,
            last_clock_sync_duration: self.last_clock_sync_duration,
            last_danmaku_plan_duration: self.last_danmaku_plan_duration,
            last_render_duration: self.last_render_duration,
            last_render_current_duration: self.last_render_current_duration,
            last_render_test_duration: self.last_render_test_duration,
        }
    }

    /// One-call diagnostics document for hosts and agents, schema
    /// [`crate::diagnostics::DIAGNOSTICS_SCHEMA`].
    ///
    /// It reads presenter fields, renderer/audio counters and the player's
    /// lock-free timing counters; it never decodes, renders or waits on the
    /// playback worker. Unknown values are `null`.
    pub fn diagnostics_snapshot(&mut self) -> serde_json::Value {
        use crate::diagnostics as diag;
        use serde_json::json;

        self.refresh_video_decoder_status();
        let now = Instant::now();
        let snapshot = self.runtime_snapshot();
        let renderer = &snapshot.renderer;
        let tracks = self.player.tracks();
        let selection = self.player.track_selection();
        let find_track =
            |id: Option<i64>| id.and_then(|id| tracks.iter().find(|track| track.id == id));
        let video_track = find_track(selection.video).map(|track| {
            json!({
                "trackId": track.id,
                "codec": track.codec,
                "profile": track.profile,
                "level": track.level,
                "width": track.width,
                "height": track.height,
                "pixelFormat": track.pixel_format,
                "frameRate": track.frame_rate.as_ref().map(|rate| rate.frames_per_second()),
                "bitRate": track.bit_rate,
            })
        });
        let audio_track = find_track(selection.audio).map(|track| {
            json!({
                "trackId": track.id,
                "codec": track.codec,
                "profile": track.profile,
                "sampleRate": track.sample_rate,
                "channels": track.channels,
                "sampleFormat": track.sample_format,
                "bitRate": track.bit_rate,
            })
        });
        let subtitle_track = find_track(selection.subtitle).map(|track| {
            json!({ "trackId": track.id, "codec": track.codec, "language": track.language })
        });
        let timing = self.player.timing_counters();
        let audio_ring = self.audio_output.stats();
        let audio_runtime = snapshot.audio_output_runtime_stats;
        let queues = timing.queues_json();
        let channel = |queued: usize, capacity: Option<usize>| json!({ "queued": queued, "capacity": capacity });
        let surface = self.current_surface_metrics;
        json!({
            "schema": diag::DIAGNOSTICS_SCHEMA,
            "capturedAtUnixMs": diag::unix_ms_now(),
            "playback": {
                "state": format!("{:?}", self.player.state()).to_lowercase(),
                "playing": snapshot.playing,
                "mediaTimeSeconds": snapshot.media_time.as_secs_f64(),
                "durationSeconds": self.player.duration().map(|duration| duration.as_secs_f64()),
                "playbackRate": self.playback_rate,
                "generation": snapshot.generation,
                "audioOnly": self.audio_only_tick_active,
                "buffering": timing.buffering_json(),
            },
            "media": {
                "video": video_track,
                "audio": audio_track,
                "subtitle": subtitle_track,
            },
            "decoder": diag::decoder_json(self.latest_video_decoder.as_ref()),
            "video": {
                "decodedFrames": snapshot.stats.decoded_video_frames,
                "renderedFrames": snapshot.stats.rendered_video_frames,
                "hardwareFrames": renderer.hardware_video_frames,
                "softwareFrames": renderer.software_video_frames,
                "zeroCopyFrames": renderer.zero_copy_video_frames,
                "directZeroCopyFrames": renderer.direct_zero_copy_video_frames,
                "sharedHandleFrames": renderer.shared_handle_video_frames,
                "cpuFallbackFrames": renderer.cpu_video_frame_fallbacks,
                "importFailures": snapshot.stats.import_failures,
                "renderFailures": snapshot.stats.render_failures,
                "backpressureDrops": snapshot.stats.video_frame_backpressure_drops,
                "rejectedImportRoute": self.rejected_video_import_route.map(|route| json!({
                    "backend": route.backend.as_str(),
                    "mediaCodecSurface": route.mediacodec_surface,
                    "generation": route.generation,
                })),
            },
            "presenter": {
                "surfaceAttached": renderer.attached,
                "surfaceWidth": surface.map(|metrics| metrics.physical_extent.width),
                "surfaceHeight": surface.map(|metrics| metrics.physical_extent.height),
                "lastTickMs": diag::duration_ms_value(snapshot.last_tick_duration),
                "lastPumpMs": diag::duration_ms_value(snapshot.last_pump_duration),
                "lastAudioPumpMs": diag::duration_ms_value(snapshot.last_audio_pump_duration),
                "lastSubtitlePumpMs": diag::duration_ms_value(snapshot.last_subtitle_pump_duration),
                "lastVideoPumpMs": diag::duration_ms_value(snapshot.last_video_pump_duration),
                "lastClockSyncMs": diag::duration_ms_value(snapshot.last_clock_sync_duration),
                "lastDanmakuPlanMs": diag::duration_ms_value(snapshot.last_danmaku_plan_duration),
                "lastRenderMs": diag::duration_ms_value(snapshot.last_render_duration),
                "lastGpuMs": diag::duration_ms_value(renderer.last_gpu_duration),
                "renderedTestFrames": snapshot.stats.rendered_test_frames,
                "overlayFrames": snapshot.stats.overlay_frames,
                "decodedSubtitleFrames": snapshot.stats.decoded_subtitle_frames,
                "danmakuFrames": snapshot.stats.danmaku_frames,
                "danmakuItems": snapshot.stats.danmaku_items,
                "currentDanmakuItems": snapshot.current_danmaku_items,
            },
            "output": diag::output_json(&snapshot.output),
            "hdr": diag::hdr_json(renderer),
            "resources": diag::resources_json(&snapshot.resources, snapshot.current_danmaku_atlas_bytes),
            "upscaler": diag::upscaler_json(renderer),
            "audio": {
                "outputState": format!("{:?}", self.audio_output.state()).to_lowercase(),
                "configured": self.audio_configured,
                "started": self.audio_started,
                "volume": self.audio_output.volume(),
                "pushedFrames": snapshot.stats.pushed_audio_frames,
                "failures": snapshot.stats.audio_failures,
                "queuedFrames": snapshot.audio_output_queued_frames,
                "queuedMs": diag::optional_duration_ms(snapshot.audio_output_queued_duration),
                "readFrames": snapshot.audio_output_read_frames,
                "writtenFrames": snapshot.audio_output_written_frames,
                "underflowFrames": snapshot.audio_output_underflow_frames,
                "ringDroppedFrames": audio_ring.dropped_frames,
                "recovery": {
                    "state": audio_runtime.recovery_state.as_str(),
                    "lastErrorCode": audio_runtime.last_error_code,
                    "attempts": audio_runtime.recovery_attempts,
                    "count": audio_runtime.recovery_count,
                    "failures": audio_runtime.recovery_failures,
                },
            },
            "buffers": {
                "playback": queues,
                "presenterVideoFrames": channel(self.video_frames.len(), self.video_frames.capacity()),
                "presenterAudioFrames": channel(self.audio_frames.len(), self.audio_frames.capacity()),
                "presenterSubtitleFrames": channel(self.subtitle_frames.len(), self.subtitle_frames.capacity()),
                "audioOutputQueuedMs": diag::optional_duration_ms(snapshot.audio_output_queued_duration),
            },
            "timing": {
                "scheduler": timing.scheduler_json(),
                "presentation": self.presentation.to_json(now),
                "clock": timing.clock_json(),
            },
        })
    }

    fn pump_video(&mut self) {
        let started = Instant::now();
        let mut pumped = 0usize;
        loop {
            if pumped >= VIDEO_PUMP_FRAME_LIMIT || started.elapsed() >= VIDEO_PUMP_TIME_BUDGET {
                break;
            }
            match self.video_frames.try_recv() {
                Ok(mut frame) => {
                    if frame.generation != self.player.playback_generation() {
                        continue;
                    }
                    if frame.generation != self.last_scene_luma_generation {
                        // New playback: start the smoothing state fresh so a
                        // seek cannot carry the old scene's brightness over.
                        self.scene_luma_smoother.reset();
                        self.last_scene_luma_generation = frame.generation;
                    }
                    if let Some(scene_avg) =
                        measure_frame_scene_avg(&frame, &mut self.scene_luma_smoother)
                    {
                        frame.scene_avg_nits = Some(scene_avg);
                    }
                    #[cfg(target_os = "android")]
                    let mediacodec_surface = frame.frame.is_mediacodec();
                    #[cfg(not(target_os = "android"))]
                    let mediacodec_surface = false;
                    let import_route = RejectedVideoImportRoute::new(
                        frame.decode_backend,
                        mediacodec_surface,
                        frame.generation,
                    );
                    if should_reject_video_import(self.rejected_video_import_route, import_route) {
                        continue;
                    }
                    self.stats.decoded_video_frames += 1;
                    match self.renderer.upload_player_frame(&frame) {
                        Ok(()) => {
                            if self.rejected_video_import_route.is_some() {
                                self.rejected_video_import_route = None;
                            }
                            let pts = frame.pts.unwrap_or(frame.media_time);
                            self.presentation.on_upload(pts, frame.generation);
                            self.current_media_time = pts;
                            self.current_generation =
                                frame.generation.max(self.danmaku_generation).max(1);
                            self.update_overlay(
                                pts,
                                frame.generation,
                                frame.frame.width() as usize,
                                frame.frame.height() as usize,
                            );
                            let plan_time =
                                self.current_danmaku.as_ref().map(|plan| plan.media_time);
                            let plan_generation =
                                self.current_danmaku.as_ref().map(|plan| plan.generation);
                            let plan_items = self
                                .current_danmaku
                                .as_ref()
                                .map_or(0, |plan| plan.items.len());
                            self.trace_danmaku_time(
                                "video_frame",
                                pts,
                                self.current_generation,
                                None,
                                Some(pts),
                                plan_time,
                                plan_generation,
                                plan_items,
                            );
                            pumped += 1;
                        }
                        Err(PlayerError::RendererBackpressure(reason)) => {
                            self.stats.video_frame_backpressure_drops =
                                self.stats.video_frame_backpressure_drops.saturating_add(1);
                            let drop_count = self.stats.video_frame_backpressure_drops;
                            if should_report_video_frame_backpressure(drop_count) {
                                trace::diagnostic(
                                    serde_json::json!({
                                        "event": "video_frame_backpressure",
                                        "stage": "renderer_upload_drop",
                                        "backend": frame.decode_backend.as_str(),
                                        "mediaCodecMode": if import_route.mediacodec_surface {
                                            Some("surface_ahardwarebuffer")
                                        } else if frame.decode_backend == DecoderBackend::MediaCodec {
                                            Some("bytebuffer_cpu_upload")
                                        } else {
                                            None
                                        },
                                        "generation": frame.generation,
                                        "dropCount": drop_count,
                                        "action": "drop_current_frame_keep_decoder_and_render_previous",
                                        "reason": reason.as_str(),
                                    })
                                    .to_string(),
                                );
                            }
                            // Capacity pressure is expected under a temporarily
                            // saturated GPU. Keep the decoder route active, do not
                            // advance media time to a frame that was not uploaded,
                            // and leave queued frames for the next tick.
                            break;
                        }
                        Err(error) => {
                            self.stats.import_failures += 1;
                            let failure = VideoFrameImportFailure {
                                decode_backend: frame.decode_backend,
                                mediacodec_surface: import_route.mediacodec_surface,
                                codec: self.selected_video_codec(),
                                pixel_format: frame.frame.pixel_format(),
                                line_sizes: frame.frame.line_sizes(),
                                width: frame.frame.width(),
                                height: frame.frame.height(),
                                generation: frame.generation,
                                reason: error.to_string(),
                            };
                            trace::diagnostic(failure.structured_message());
                            self.rejected_video_import_route = Some(import_route);
                            // The decoder transition must not race a local
                            // frame, queued frames, or the Android recovery
                            // renderer's retained current payload.
                            drop(frame);
                            let quiesced = match self.quiesce_frame_output("video_import_failure") {
                                Ok(quiesced) => quiesced,
                                Err(error) => {
                                    trace::diagnostic(
                                        serde_json::json!({
                                            "event": "video_frame_import_feedback_failed",
                                            "backend": import_route.backend.as_str(),
                                            "stage": "quiesce",
                                            "reason": error.to_string(),
                                        })
                                        .to_string(),
                                    );
                                    break;
                                }
                            };
                            self.release_current_video_frame();
                            self.drain_pending_video_frames();
                            if let Err(report_error) =
                                self.player.report_video_frame_import_failure(failure)
                            {
                                trace::diagnostic(
                                    serde_json::json!({
                                        "event": "video_frame_import_feedback_failed",
                                        "backend": import_route.backend.as_str(),
                                        "reason": report_error.to_string(),
                                    })
                                    .to_string(),
                                );
                            }
                            if let Err(error) = self.finish_frame_output_transition(
                                "video_import_failure",
                                quiesced,
                                false,
                            ) {
                                trace::diagnostic(
                                    serde_json::json!({
                                        "event": "video_frame_import_feedback_failed",
                                        "backend": import_route.backend.as_str(),
                                        "stage": "transition_finish",
                                        "reason": error.to_string(),
                                    })
                                    .to_string(),
                                );
                            }
                            break;
                        }
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
    }

    fn selected_video_codec(&self) -> Option<String> {
        let selected = self.player.track_selection().video?;
        self.player
            .tracks()
            .into_iter()
            .find(|track| track.id == selected)
            .and_then(|track| track.codec)
    }

    fn update_overlay(&mut self, pts: Duration, generation: u64, width: usize, height: usize) {
        let viewport = DanmakuViewport::new(
            width.min(u32::MAX as usize) as u32,
            height.min(u32::MAX as usize) as u32,
        );
        let source = OverlayViewport::new(viewport.width, viewport.height);
        self.subtitle_resolution.source = Some(source);
        let overlay = self.render_playback_overlay(pts, source);
        if subtitle_diag_enabled() {
            eprintln!(
                "[erika-subtitle-diag] stage=update_overlay pts={} gen={} video={}x{} overlay={}",
                duration_label(Some(pts)),
                generation,
                viewport.width,
                viewport.height,
                overlay_debug_summary(&overlay),
            );
        }
        if !overlay.is_empty() {
            self.stats.overlay_frames += 1;
        }
        self.current_overlay = Some(overlay);
        let generation = generation.max(self.danmaku_generation).max(1);
        let candidate_viewport = self
            .current_surface_metrics
            .map(surface_metrics_to_viewport)
            .unwrap_or(viewport);
        let danmaku_viewport = self
            .current_danmaku_viewport
            .filter(|current| !danmaku_viewport_requires_relayout(*current, candidate_viewport))
            .unwrap_or(candidate_viewport);
        self.current_danmaku_viewport = Some(danmaku_viewport);
        self.request_current_danmaku_plan(pts, danmaku_viewport, generation);
    }

    fn record_current_danmaku_stats(&mut self) {
        if let Some(plan) = &self.current_danmaku {
            if !plan.is_empty() {
                self.stats.danmaku_frames += 1;
                self.stats.danmaku_items += plan.items.len() as u64;
            }
        }
    }

    fn refresh_stale_danmaku_plan(&mut self) {
        self.apply_ready_danmaku_plans();
        self.refresh_current_danmaku_plan_from_prepared();
        self.request_current_danmaku_plan_for_current_time();
    }

    fn apply_ready_danmaku_plans(&mut self) {
        while let Some(mut result) = self.danmaku_planner.try_recv() {
            let accepted = self.danmaku_plan_result_is_current(&result);
            let prepared_items = result.prepared.items().len();
            if accepted {
                result.prepared.apply_paint_config(self.danmaku.config());
                self.danmaku_plan_replacement_pending =
                    result.prepared.font_size() != self.danmaku.config().font_size;
                self.danmaku_resize_settle_at = None;
                self.current_danmaku_prepared = Some(CurrentDanmakuPrepared {
                    request: result.request,
                    prepared: result.prepared,
                    rasterizer: result.rasterizer,
                    window_start: result.window_start,
                    window_end: result.window_end,
                });
                self.last_danmaku_plan_duration = result.elapsed;
            }
            if self.danmaku_trace.enabled {
                self.trace_danmaku_time(
                    if accepted {
                        "prepared_async_ready"
                    } else {
                        "prepared_async_stale"
                    },
                    self.current_media_time,
                    self.current_generation,
                    None,
                    None,
                    Some(result.request.key.media_time),
                    Some(result.request.key.generation),
                    prepared_items,
                );
            }
        }
    }

    fn refresh_current_danmaku_plan_from_prepared(&mut self) {
        if !self.danmaku.config().enabled {
            self.current_danmaku = None;
            self.danmaku_plan_replacement_pending = false;
            return;
        }
        let Some(prepared) = &self.current_danmaku_prepared else {
            if !self.danmaku_plan_replacement_pending {
                self.current_danmaku = None;
            }
            return;
        };
        if !self.danmaku_prepared_time_is_current(prepared) {
            self.current_danmaku = None;
            return;
        }
        let layout = prepared
            .prepared
            .frame_layout(self.current_media_time, self.current_generation);
        let scale = self.danmaku.config().font_size / prepared.prepared.font_size();
        let mut plan =
            prepared
                .rasterizer
                .render_plan_scaled(&layout, scale, prepared.prepared.atlas());
        if let Some(viewport) = self.current_danmaku_viewport {
            plan.retarget(viewport);
        }
        self.current_danmaku = Some(plan);
        self.record_current_danmaku_stats();
    }

    fn request_current_danmaku_plan_for_current_time(&mut self) {
        let Some(viewport) = self.current_danmaku_viewport else {
            return;
        };
        self.request_current_danmaku_plan(
            self.current_media_time,
            viewport,
            self.current_generation,
        );
    }

    fn request_current_danmaku_plan(
        &mut self,
        media_time: Duration,
        viewport: DanmakuViewport,
        generation: u64,
    ) {
        if !self.danmaku.config().enabled {
            return;
        }
        if self
            .danmaku_resize_settle_at
            .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        let size_waiting = self
            .danmaku_font_size_update
            .is_some_and(|pending| !pending.ready(Instant::now()));
        if self.danmaku_font_size_update.is_some() && !size_waiting {
            self.danmaku_font_size_update = None;
            self.danmaku_planner.set_config(
                self.danmaku.config().clone(),
                self.danmaku.rasterizer_clone(),
            );
            self.danmaku_plan_replacement_pending = true;
        }
        if (!self.danmaku_plan_replacement_pending || size_waiting)
            && self
                .current_danmaku_prepared
                .as_ref()
                .is_some_and(|prepared| {
                    self.danmaku_prepared_covers_current_time(prepared)
                        && prepared.window_end.saturating_sub(media_time)
                            > DANMAKU_PREPARE_REFRESH_MARGIN
                })
        {
            return;
        }
        let key = DanmakuPlanKey {
            media_time: quantize_duration(media_time, DANMAKU_PLAN_REQUEST_QUANTUM),
            viewport,
            generation: generation.max(self.danmaku_generation).max(1),
        };
        self.danmaku_planner.request_plan(key);
    }

    fn danmaku_plan_result_is_current(&self, result: &AsyncDanmakuPlanResult) -> bool {
        let key = result.request.key;
        self.danmaku.config().enabled
            && key.generation == self.current_generation
            && Some(key.viewport) == self.current_danmaku_viewport
            && result.window_start <= self.current_media_time
            && self.current_media_time <= result.window_end
    }

    fn danmaku_prepared_covers_current_time(&self, prepared: &CurrentDanmakuPrepared) -> bool {
        self.current_danmaku_viewport.is_some_and(|viewport| {
            !danmaku_viewport_requires_relayout(prepared.request.key.viewport, viewport)
        }) && self.danmaku_prepared_time_is_current(prepared)
    }

    fn danmaku_prepared_time_is_current(&self, prepared: &CurrentDanmakuPrepared) -> bool {
        prepared.request.key.generation == self.current_generation
            && prepared.window_start <= self.current_media_time
            && self.current_media_time <= prepared.window_end
    }

    fn sync_media_time_from_player(&mut self) -> PlaybackSnapshot {
        self.sync_media_time_from_player_at(None)
    }

    fn sync_media_time_from_player_at(
        &mut self,
        presentation_time: Option<Instant>,
    ) -> PlaybackSnapshot {
        // One lock for time + generation + state. Danmaku, subtitles and the
        // render context all derive from this single sample, so a frame can
        // never mix a stale media time with a newer play state.
        let snapshot = self.player.playback_snapshot();
        self.apply_playback_snapshot(&snapshot, presentation_time);
        snapshot
    }

    fn apply_playback_snapshot(
        &mut self,
        snapshot: &PlaybackSnapshot,
        presentation_time: Option<Instant>,
    ) {
        let player_time = snapshot.media_time_at(presentation_time.unwrap_or_else(Instant::now));
        self.current_generation = self
            .current_generation
            .max(snapshot.generation)
            .max(self.danmaku_generation)
            .max(1);
        if player_time != self.current_media_time {
            self.current_media_time = player_time;
            if let Some(source) = self.subtitle_resolution.source {
                let overlay = self.render_playback_overlay(player_time, source);
                if subtitle_diag_enabled() {
                    eprintln!(
                        "[erika-subtitle-diag] stage=clock_overlay player={} gen={} overlay_viewport={}x{} overlay={}",
                        duration_label(Some(player_time)),
                        self.current_generation,
                        overlay.viewport.width,
                        overlay.viewport.height,
                        overlay_debug_summary(&overlay),
                    );
                }
                self.current_overlay = Some(overlay);
            }
        }
        self.trace_danmaku_time(
            "player_clock",
            player_time,
            self.current_generation,
            Some(player_time),
            None,
            None,
            None,
            0,
        );
    }

    fn refresh_current_overlay(&mut self) {
        let Some(source) = self.subtitle_resolution.source else {
            return;
        };
        self.current_overlay = Some(self.render_playback_overlay(self.current_media_time, source));
    }

    fn render_playback_overlay(&mut self, pts: Duration, source: OverlayViewport) -> OverlayFrame {
        let viewport = self.subtitle_resolution.viewport(source);
        let started = Instant::now();
        let mut overlay = self.overlay.render(pts, viewport);
        // Script coordinates and user font sizes stay in the original video
        // space. Only libass's raster canvas has a higher resolution.
        let subtitle_style = self.subtitle_ass_style(source);
        self.subtitles
            .append_to_overlay(pts, &mut overlay, &subtitle_style);
        if viewport != source
            && !overlay.subtitle_alpha_planes.is_empty()
            && self.subtitle_resolution.record(started.elapsed())
        {
            trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_resolution",
                    "stage": "budget_fallback",
                    "width": source.width,
                    "height": source.height,
                })
                .to_string(),
            );
        }
        overlay
    }

    fn subtitle_ass_style(&self, viewport: OverlayViewport) -> SubtitleAssStyle {
        let memory_fonts = self
            .subtitle_memory_fonts
            .selected_ids
            .iter()
            .filter_map(|id| {
                self.subtitle_memory_fonts
                    .registered
                    .get(id)
                    .map(|font| font.attachment.clone())
            })
            .collect::<Vec<_>>();
        SubtitleAssStyle {
            font_scale: self.subtitle_font_scale,
            play_res_width: viewport.width,
            play_res_height: viewport.height,
            style: self.subtitle_style.clone(),
            memory_fonts: Arc::from(memory_fonts),
            memory_font_revision: self.subtitle_memory_fonts.selection_generation,
        }
    }

    fn trace_danmaku_time(
        &mut self,
        stage: &'static str,
        media_time: Duration,
        generation: u64,
        player_time: Option<Duration>,
        video_time: Option<Duration>,
        plan_time: Option<Duration>,
        plan_generation: Option<u64>,
        plan_items: usize,
    ) {
        if !self.danmaku_trace.enabled {
            return;
        }
        let trace = &mut self.danmaku_trace;
        let event_rollback = trace.last_event_time.is_some_and(|last| {
            trace.last_event_generation == generation && duration_regressed(media_time, last)
        });
        let player_rollback = player_time.is_some_and(|time| {
            trace.last_player_generation == generation
                && trace
                    .last_player_time
                    .is_some_and(|last| duration_regressed(time, last))
        });
        let video_rollback = video_time.is_some_and(|time| {
            trace.last_video_generation == generation
                && trace
                    .last_video_time
                    .is_some_and(|last| duration_regressed(time, last))
        });
        let resolved_plan_generation = plan_generation.unwrap_or(generation);
        let plan_rollback = plan_time.is_some_and(|time| {
            trace.last_plan_generation == resolved_plan_generation
                && trace
                    .last_plan_time
                    .is_some_and(|last| duration_regressed(time, last))
        });
        let generation_changed =
            trace.last_event_generation != 0 && trace.last_event_generation != generation;
        let plan_mismatch = plan_time.is_some_and(|time| {
            time != media_time || plan_generation.is_some_and(|plan_gen| plan_gen != generation)
        });

        if trace.samples < 16
            || event_rollback
            || player_rollback
            || video_rollback
            || plan_rollback
            || generation_changed
            || plan_mismatch
        {
            let line = format!(
                "[erika-danmaku-trace] stage={stage} media={} gen={} player={} video={} plan={} plan_gen={} items={} last_event={} last_event_gen={} flags=event_back:{} player_back:{} video_back:{} plan_back:{} gen_change:{} plan_mismatch:{}",
                duration_label(Some(media_time)),
                generation,
                duration_label(player_time),
                duration_label(video_time),
                duration_label(plan_time),
                plan_generation
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                plan_items,
                duration_label(trace.last_event_time),
                trace.last_event_generation,
                event_rollback,
                player_rollback,
                video_rollback,
                plan_rollback,
                generation_changed,
                plan_mismatch,
            );
            eprintln!("{line}");
            if let Some(path) = &trace.log_path {
                let _ = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .and_then(|mut file| writeln!(file, "{line}"));
            }
            trace.samples = trace.samples.saturating_add(1);
        }

        trace.last_event_time = Some(media_time);
        trace.last_event_generation = generation;
        if let Some(time) = player_time {
            trace.last_player_time = Some(time);
            trace.last_player_generation = generation;
        }
        if let Some(time) = video_time {
            trace.last_video_time = Some(time);
            trace.last_video_generation = generation;
        }
        if let Some(time) = plan_time {
            trace.last_plan_time = Some(time);
            trace.last_plan_generation = resolved_plan_generation;
        }
    }

    fn trace_current_danmaku_motion(&mut self, host_time_seconds: f64) {
        if !self.danmaku_trace.enabled {
            return;
        }

        let now = Instant::now();
        let periodic_sample_due = self
            .danmaku_trace
            .last_motion_log_at
            .is_none_or(|last| last.elapsed() >= DANMAKU_MOTION_TRACE_INTERVAL);
        let authoritative_time = self.current_media_time;
        let player_time = self.player.current_media_time();
        let prepared_key = self
            .current_danmaku_prepared
            .as_ref()
            .map(|prepared| prepared.request.key);
        let prepared_window = self
            .current_danmaku_prepared
            .as_ref()
            .map(|prepared| (prepared.window_start, prepared.window_end));

        let Some(plan) = self.current_danmaku.as_ref() else {
            let plan_disappeared = self.danmaku_trace.last_motion_had_plan;
            if periodic_sample_due || plan_disappeared {
                let line = format!(
                    "[erika-danmaku-motion] event={} host={host_time_seconds:.6} authoritative={} player={} gen={} playing={} plan=- pending={} prepared_key={} prepared_window={} viewport={} flags=plan_disappeared:{}",
                    if plan_disappeared {
                        "plan_missing"
                    } else {
                        "sample"
                    },
                    duration_label(Some(authoritative_time)),
                    duration_label(Some(player_time)),
                    self.current_generation,
                    self.is_playing(),
                    self.danmaku_plan_replacement_pending,
                    prepared_key
                        .map(|key| format!(
                            "{:.3}/{}",
                            key.media_time.as_secs_f64(),
                            key.generation
                        ))
                        .unwrap_or_else(|| "-".to_string()),
                    prepared_window
                        .map(|(start, end)| {
                            format!("{:.3}..{:.3}", start.as_secs_f64(), end.as_secs_f64())
                        })
                        .unwrap_or_else(|| "-".to_string()),
                    self.current_danmaku_viewport
                        .map(viewport_label)
                        .unwrap_or_else(|| "-".to_string()),
                    plan_disappeared,
                );
                self.danmaku_trace.write_line(&line);
                self.danmaku_trace.last_motion_log_at = Some(now);
            }
            self.danmaku_trace.last_motion_had_plan = false;
            return;
        };

        let plan_time = plan.media_time;
        let plan_generation = plan.generation;
        let viewport = plan.viewport;
        let atlas_version = plan.atlas.as_ref().map_or(0, |atlas| atlas.version);
        let glyph_count = plan.items.len();
        let mode_by_id = self
            .current_danmaku_prepared
            .as_ref()
            .map(|prepared| {
                prepared
                    .prepared
                    .items()
                    .iter()
                    .map(|item| (item.id, item.mode))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let mut positions = HashMap::new();
        for glyph in &plan.items {
            positions.entry(glyph.item_id).or_insert(glyph.rect[0]);
        }

        let mut backstep_count = 0usize;
        let mut worst_backstep: Option<(u64, DanmakuMode, f32, f32, f32)> = None;
        let mut selected: Option<(u64, DanmakuMode, f32, Option<f32>)> = None;
        for (&item_id, &x) in &positions {
            let Some(&mode) = mode_by_id.get(&item_id) else {
                continue;
            };
            let previous = self.danmaku_trace.motion_samples.get(&item_id);
            if selected.is_none() || previous.is_some() {
                selected = Some((item_id, mode, x, previous.map(|sample| sample.x)));
            }
            let Some(previous) = previous else {
                continue;
            };
            if previous.generation != plan_generation
                || previous.viewport != viewport
                || previous.mode != mode
                || plan_time < previous.plan_time
            {
                continue;
            }
            let opposite_delta = danmaku_motion_backstep(mode, previous.x, x);
            if opposite_delta > DANMAKU_MOTION_BACKSTEP_EPSILON {
                backstep_count += 1;
                if worst_backstep
                    .as_ref()
                    .is_none_or(|(_, _, _, _, worst)| opposite_delta > *worst)
                {
                    worst_backstep = Some((item_id, mode, previous.x, x, opposite_delta));
                }
            }
        }

        let plan_appeared = !self.danmaku_trace.last_motion_had_plan;
        let viewport_changed = self.danmaku_trace.last_motion_viewport != Some(viewport);
        let prepared_changed = self.danmaku_trace.last_motion_prepared_key != prepared_key;
        let atlas_changed = self.danmaku_trace.last_motion_atlas_version != atlas_version;
        if backstep_count > 0 {
            self.danmaku_trace.motion_backsteps = self
                .danmaku_trace
                .motion_backsteps
                .saturating_add(backstep_count as u64);
        }

        if periodic_sample_due
            || plan_appeared
            || viewport_changed
            || prepared_changed
            || atlas_changed
            || backstep_count > 0
        {
            let plan_drift_ms =
                (plan_time.as_secs_f64() - authoritative_time.as_secs_f64()) * 1000.0;
            let selected_label = selected
                .map(|(item_id, mode, x, previous_x)| {
                    format!(
                        "{item_id}/{mode:?}/x:{x:.3}/prev:{}/dx:{}",
                        previous_x
                            .map(|value| format!("{value:.3}"))
                            .unwrap_or_else(|| "-".to_string()),
                        previous_x
                            .map(|value| format!("{:.3}", x - value))
                            .unwrap_or_else(|| "-".to_string()),
                    )
                })
                .unwrap_or_else(|| "-".to_string());
            let worst_label = worst_backstep
                .map(|(item_id, mode, previous_x, x, delta)| {
                    format!("{item_id}/{mode:?}/prev:{previous_x:.3}/x:{x:.3}/back:{delta:.3}")
                })
                .unwrap_or_else(|| "-".to_string());
            let line = format!(
                "[erika-danmaku-motion] event={} host={host_time_seconds:.6} authoritative={} player={} plan={} plan_drift_ms={plan_drift_ms:.3} gen={}/{} playing={} viewport={} prepared_key={} prepared_window={} atlas={} glyphs={} unique_items={} selected={} cpu_backsteps={} cpu_backsteps_total={} worst={} flags=plan_appeared:{} viewport_changed:{} prepared_changed:{} atlas_changed:{} pending:{}",
                if backstep_count > 0 {
                    "cpu_backstep"
                } else {
                    "sample"
                },
                duration_label(Some(authoritative_time)),
                duration_label(Some(player_time)),
                duration_label(Some(plan_time)),
                self.current_generation,
                plan_generation,
                self.is_playing(),
                viewport_label(viewport),
                prepared_key
                    .map(|key| format!("{:.3}/{}", key.media_time.as_secs_f64(), key.generation))
                    .unwrap_or_else(|| "-".to_string()),
                prepared_window
                    .map(|(start, end)| {
                        format!("{:.3}..{:.3}", start.as_secs_f64(), end.as_secs_f64())
                    })
                    .unwrap_or_else(|| "-".to_string()),
                atlas_version,
                glyph_count,
                positions.len(),
                selected_label,
                backstep_count,
                self.danmaku_trace.motion_backsteps,
                worst_label,
                plan_appeared,
                viewport_changed,
                prepared_changed,
                atlas_changed,
                self.danmaku_plan_replacement_pending,
            );
            self.danmaku_trace.write_line(&line);
            self.danmaku_trace.last_motion_log_at = Some(now);
        }

        self.danmaku_trace.motion_samples = positions
            .into_iter()
            .filter_map(|(item_id, x)| {
                mode_by_id.get(&item_id).copied().map(|mode| {
                    (
                        item_id,
                        DanmakuMotionSample {
                            x,
                            plan_time,
                            generation: plan_generation,
                            mode,
                            viewport,
                        },
                    )
                })
            })
            .collect();
        self.danmaku_trace.last_motion_atlas_version = atlas_version;
        self.danmaku_trace.last_motion_prepared_key = prepared_key;
        self.danmaku_trace.last_motion_viewport = Some(viewport);
        self.danmaku_trace.last_motion_had_plan = true;
    }

    fn bump_danmaku_generation(&mut self) {
        bump_generation(&mut self.current_generation, &mut self.danmaku_generation);
        self.danmaku_planner.invalidate_requests();
        self.invalidate_current_danmaku_plan();
    }

    fn bump_danmaku_generation_for_config_change(&mut self) {
        bump_generation(&mut self.current_generation, &mut self.danmaku_generation);
        self.danmaku_planner.invalidate_requests();

        // Style/layout preparation runs on the async planner. Keep the last
        // complete geometry moving until its replacement is ready so a stream
        // of slider updates cannot freeze, flash, or restart current comments.
        // Retagging both the prepared layout and its current render plan lets
        // renderers accept this short-lived fallback for the new generation.
        self.danmaku_plan_replacement_pending = retain_danmaku_state_for_config_change(
            &mut self.current_danmaku,
            &mut self.current_danmaku_prepared,
            self.danmaku.config().enabled,
            self.current_generation,
        );
    }

    fn invalidate_current_danmaku_plan(&mut self) {
        if self.danmaku_font_size_update.take().is_some() {
            self.danmaku_planner.set_config(
                self.danmaku.config().clone(),
                self.danmaku.rasterizer_clone(),
            );
        }
        self.current_danmaku = None;
        self.current_danmaku_prepared = None;
        self.danmaku_plan_replacement_pending = false;
        self.danmaku_resize_settle_at = None;
    }

    fn clear_current_danmaku_state(&mut self) {
        self.invalidate_current_danmaku_plan();
        self.current_danmaku_viewport = None;
    }

    fn clear_playback_visual_state(
        &mut self,
        media_time: Duration,
        frame_policy: TransitionFramePolicy,
    ) {
        self.rejected_video_import_route = None;
        self.current_overlay = None;
        self.subtitle_resolution.source = None;
        self.subtitles.clear();
        self.clear_current_danmaku_state();
        self.current_media_time = media_time;
        self.last_audio_clock_report = None;
        match frame_policy {
            TransitionFramePolicy::Clear => self.release_current_video_frame(),
            TransitionFramePolicy::PreserveRendererSnapshot => {
                self.preserve_current_video_frame_for_transition()
            }
            TransitionFramePolicy::PreserveTrackSwitchFrame => {
                self.preserve_current_video_frame_for_track_transition()
            }
        }
    }

    fn release_current_video_frame(&mut self) {
        if let Err(error) = self.renderer.clear_current_frame() {
            self.stats.render_failures += 1;
            trace::diagnostic_at(
                trace::LogLevel::Warn,
                format!("Erika presenter renderer clear failed: {error}"),
            );
        }
    }

    fn preserve_current_video_frame_for_transition(&mut self) {
        if let Err(error) = self.renderer.preserve_current_frame_for_transition() {
            self.stats.render_failures += 1;
            trace::diagnostic(
                serde_json::json!({
                    "event": "frame_transition_snapshot",
                    "stage": "preserve_failed",
                    "reason": error.to_string(),
                    "action": "clear_current_frame",
                })
                .to_string(),
            );
            self.release_current_video_frame();
        }
    }

    fn preserve_current_video_frame_for_track_transition(&mut self) {
        if let Err(error) = self.renderer.preserve_current_frame_for_track_transition() {
            self.stats.render_failures += 1;
            trace::diagnostic(
                serde_json::json!({
                    "event": "frame_transition_snapshot",
                    "stage": "track_transition_preserve_failed",
                    "reason": error.to_string(),
                    "action": "clear_current_frame",
                })
                .to_string(),
            );
            self.release_current_video_frame();
        }
    }

    fn quiesce_frame_output(&mut self, operation: &'static str) -> Result<bool> {
        let started = Instant::now();
        trace::diagnostic(
            serde_json::json!({
                "event": "player_frame_output_transition",
                "stage": "quiesce_request",
                "operation": operation,
            })
            .to_string(),
        );
        match self.player.set_frame_output_quiesced(true) {
            Ok(active) => {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_frame_output_transition",
                        "stage": "quiesce_acknowledged",
                        "operation": operation,
                        "active": active,
                        "elapsedMs": started.elapsed().as_millis(),
                    })
                    .to_string(),
                );
                Ok(active)
            }
            Err(error) => {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_frame_output_transition",
                        "stage": "quiesce_failed",
                        "operation": operation,
                        "elapsedMs": started.elapsed().as_millis(),
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
                Err(error)
            }
        }
    }

    fn finish_frame_output_transition(
        &mut self,
        operation: &'static str,
        active: bool,
        drain_all_streams: bool,
    ) -> Result<()> {
        if !active {
            if drain_all_streams {
                self.drain_pending_player_frames();
            } else {
                self.drain_pending_video_frames();
            }
            return Ok(());
        }
        // A second quiesce command is a FIFO barrier behind the transition
        // command. Its ACK proves decoder replacement finished while output
        // remained disabled, so both drains are race-free.
        let barrier_started = Instant::now();
        trace::diagnostic(
            serde_json::json!({
                "event": "player_frame_output_transition",
                "stage": "transition_barrier_request",
                "operation": operation,
            })
            .to_string(),
        );
        let barrier_error = match self.player.set_frame_output_quiesced(true) {
            Ok(_) => {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_frame_output_transition",
                        "stage": "transition_barrier_acknowledged",
                        "operation": operation,
                        "elapsedMs": barrier_started.elapsed().as_millis(),
                    })
                    .to_string(),
                );
                if drain_all_streams {
                    self.drain_pending_player_frames();
                } else {
                    self.drain_pending_video_frames();
                }
                None
            }
            Err(error) => {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_frame_output_transition",
                        "stage": "transition_barrier_failed",
                        "operation": operation,
                        "elapsedMs": barrier_started.elapsed().as_millis(),
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
                Some(error)
            }
        };

        let resume_started = Instant::now();
        trace::diagnostic(
            serde_json::json!({
                "event": "player_frame_output_transition",
                "stage": "resume_request",
                "operation": operation,
            })
            .to_string(),
        );
        let resume_result = self.player.set_frame_output_quiesced(false);
        match &resume_result {
            Ok(_) => {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_frame_output_transition",
                        "stage": "resume_acknowledged",
                        "operation": operation,
                        "elapsedMs": resume_started.elapsed().as_millis(),
                    })
                    .to_string(),
                );
                // If the first barrier timed out but resume was acknowledged,
                // FIFO ordering still proves the transition completed before
                // output resumed, so the pending receivers are now safe to drain.
                if barrier_error.is_some() {
                    if drain_all_streams {
                        self.drain_pending_player_frames();
                    } else {
                        self.drain_pending_video_frames();
                    }
                }
            }
            Err(error) => {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "player_frame_output_transition",
                        "stage": "resume_failed",
                        "operation": operation,
                        "elapsedMs": resume_started.elapsed().as_millis(),
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
            }
        }

        match (barrier_error, resume_result) {
            (None, result) => result.map(|_| ()),
            (Some(barrier_error), Ok(_)) => Err(barrier_error),
            (Some(barrier_error), Err(resume_error)) => Err(PlayerError::Playback(format!(
                "frame output transition barrier failed: {barrier_error}; resume failed: {resume_error}",
            ))),
        }
    }

    fn sync_danmaku_engine_timeline(&mut self) {
        let timeline = self.danmaku_session.active_timeline_clone();
        self.danmaku.sync_timeline(&timeline);
        self.danmaku_planner.set_timeline(timeline);
        self.invalidate_current_danmaku_plan();
    }

    fn pump_subtitles(&mut self) {
        loop {
            match self.subtitle_frames.try_recv() {
                Ok(frame) => {
                    if frame.generation != self.player.playback_generation() {
                        continue;
                    }
                    if subtitle_diag_enabled() {
                        eprintln!(
                            "[erika-subtitle-diag] stage=pump_subtitle gen={} track={} start={} end={} text_segments={} bitmap_planes={} empty={}",
                            frame.generation,
                            frame.frame.track_id,
                            duration_label(frame.frame.start),
                            duration_label(frame.frame.end),
                            frame.frame.text.len(),
                            frame.frame.bitmap.planes.len(),
                            frame.frame.is_empty(),
                        );
                    }
                    self.stats.decoded_subtitle_frames += 1;
                    self.subtitles.push(frame);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
    }

    fn pump_audio(&mut self) {
        let started = Instant::now();
        let mut pumped = 0usize;
        let (frame_limit, time_budget) = self.audio_pump_limits();
        loop {
            if pumped >= frame_limit || started.elapsed() >= time_budget {
                break;
            }
            if !self.audio_output.can_accept_audio_frame() {
                break;
            }
            match self.audio_frames.try_recv() {
                Ok(frame) => {
                    if frame.generation != self.player.playback_generation() {
                        continue;
                    }
                    self.push_audio(frame);
                    pumped += 1;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
        self.ensure_audio_started();
        if self.audio_started {
            self.report_audio_clock_snapshot();
        }
    }

    fn audio_pump_limits(&self) -> (usize, Duration) {
        let rate = self
            .pending_playback_rate
            .map_or(self.playback_rate, |pending| pending.rate);
        audio_pump_limits_for_rate(rate)
    }

    fn report_audio_clock_snapshot(&mut self) {
        // During a rate transition the audio ring already uses the requested
        // rate while the player clock still uses the old one. Do not enqueue a
        // mixed-rate clock sample; the first sample after commit is coherent.
        if self.pending_playback_rate.is_some() {
            return;
        }
        // configure/start/push can recover the device during this pump. Publish
        // its new epoch before sampling, not only after feedback is enqueued.
        self.report_audio_output_runtime_stats();
        let Some(observation) = self
            .player
            .capture_audio_clock(|| self.audio_output.clock_snapshot())
        else {
            return;
        };
        let snapshot = observation.snapshot();
        // Report queue/underflow movement even when the engine later rejects the clock for sync.
        if !self.should_report_audio_clock(snapshot) {
            return;
        }
        trace::log(format!(
            "[erika-clock-trace] stage=presenter_audio_snapshot media={} queued={} queued_frames={} read={} written={} underflow={}",
            trace::duration_label(snapshot.media_time),
            trace::duration_label(snapshot.queued_duration),
            snapshot.queued_frames,
            snapshot.read_frames,
            snapshot.written_frames,
            snapshot.underflow_frames,
        ));
        let _ = self.player.update_audio_clock_observation(observation);
    }

    fn should_report_audio_clock(&mut self, snapshot: AudioClockSnapshot) -> bool {
        let Some(media_time) = snapshot.media_time else {
            return false;
        };
        let next = AudioClockReportState {
            media_time,
            queued_frames: snapshot.queued_frames,
            read_frames: snapshot.read_frames,
            underflow_frames: snapshot.underflow_frames,
        };
        let should_report = self.last_audio_clock_report.is_none_or(|previous| {
            snapshot.read_frames > previous.read_frames
                || snapshot.underflow_frames > previous.underflow_frames
                || snapshot.queued_frames != previous.queued_frames
                || media_time > previous.media_time
        });
        if should_report {
            self.last_audio_clock_report = Some(next);
        }
        should_report
    }

    fn push_audio(&mut self, frame: PlayerAudioFrame) {
        if !self.audio_configured {
            self.player.invalidate_audio_clock();
            if let Err(error) = self.audio_output.configure(frame.frame.format) {
                self.stats.audio_failures += 1;
                trace::diagnostic_at(
                    trace::LogLevel::Warn,
                    format!("Erika presenter audio configure failed: {error}"),
                );
                return;
            }
            self.audio_configured = true;
            self.last_audio_clock_report = None;
        }
        match self.audio_output.push(frame.frame) {
            Ok(_) => self.stats.pushed_audio_frames += 1,
            Err(error) => {
                self.stats.audio_failures += 1;
                trace::diagnostic_at(
                    trace::LogLevel::Warn,
                    format!("Erika presenter audio push failed: {error}"),
                );
                return;
            }
        }

        self.ensure_audio_started();
    }

    fn ensure_audio_started(&mut self) {
        if !self.is_playing()
            || self.audio_started
            || !self.audio_output_ready_to_start()
            || !self.audio_start_allowed()
        {
            return;
        }
        if let Err(error) = self.audio_output.start() {
            self.stats.audio_failures += 1;
            trace::diagnostic_at(
                trace::LogLevel::Warn,
                format!("Erika presenter audio start failed: {error}"),
            );
            return;
        }
        self.audio_started = true;
        self.last_audio_clock_report = None;
    }

    fn audio_output_ready_to_start(&self) -> bool {
        self.audio_output
            .clock_snapshot()
            .and_then(|snapshot| snapshot.queued_duration)
            .is_some_and(|queued| queued >= AUDIO_START_BUFFER)
    }

    fn audio_start_allowed(&self) -> bool {
        self.player.track_selection().video.is_none() || self.stats.rendered_video_frames > 0
    }

    fn reset_audio_output(&mut self) {
        self.player.invalidate_audio_clock();
        if let Err(error) = self.audio_output.stop() {
            self.stats.audio_failures += 1;
            trace::diagnostic_at(
                trace::LogLevel::Warn,
                format!("Erika presenter audio reset failed: {error}"),
            );
        }
        self.audio_configured = false;
        self.audio_started = false;
        self.last_audio_clock_report = None;
    }

    fn reset_audio_output_with_committed_rate(&mut self) {
        self.pending_playback_rate = None;
        self.reset_audio_output();
        self.audio_output.set_playback_rate(self.playback_rate);
    }

    fn commit_pending_playback_rate(&mut self) -> Result<()> {
        let Some(pending) = self.pending_playback_rate else {
            return Ok(());
        };
        if !self.is_playing() || Instant::now() < pending.commit_at {
            return Ok(());
        }
        self.commit_pending_playback_rate_now()
    }

    fn commit_pending_playback_rate_now(&mut self) -> Result<()> {
        let Some(rate) = self.pending_playback_rate.map(|pending| pending.rate) else {
            return Ok(());
        };
        if let Err(error) = self.player.set_playback_rate(rate) {
            self.reset_audio_output_with_committed_rate();
            return Err(error);
        }
        self.playback_rate = rate;
        self.pending_playback_rate = None;
        self.player.invalidate_audio_clock();
        self.last_audio_clock_report = None;
        Ok(())
    }

    fn report_audio_output_runtime_stats(&mut self) {
        let stats = self.audio_output.runtime_stats();
        if stats.transition_sequence == self.last_audio_runtime_stats.transition_sequence {
            return;
        }
        self.player.invalidate_audio_clock();
        self.last_audio_clock_report = None;
        self.last_audio_runtime_stats = stats;
        let event = AudioOutputEvent { stats };
        trace::diagnostic(event.structured_message());
        self.player.report_audio_output_event(event);
    }

    fn drain_pending_player_frames(&mut self) {
        self.drain_pending_video_frames();
        while self.audio_frames.try_recv().is_ok() {}
        while self.subtitle_frames.try_recv().is_ok() {}
    }

    fn drain_pending_video_frames(&mut self) {
        while self.video_frames.try_recv().is_ok() {}
    }
}

fn normalize_playback_rate(rate: f64) -> f64 {
    if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        1.0
    }
}

fn playback_rate_matches(lhs: f64, rhs: f64) -> bool {
    (lhs - rhs).abs() <= PLAYBACK_RATE_EPSILON
}

fn playback_rate_request_is_idempotent(
    current_rate: f64,
    pending: Option<PendingPlaybackRate>,
    next_rate: f64,
) -> bool {
    pending.is_some_and(|pending| playback_rate_matches(pending.rate, next_rate))
        || (pending.is_none() && playback_rate_matches(current_rate, next_rate))
}

fn audio_transition_bridge(
    snapshot: Option<AudioClockSnapshot>,
    queued_output_duration: Duration,
) -> Option<Duration> {
    snapshot
        .and_then(|snapshot| snapshot.queued_duration)
        .map(|duration| duration.saturating_add(queued_output_duration))
        .filter(|duration| !duration.is_zero())
}

fn audio_pump_limits_for_rate(rate: f64) -> (usize, Duration) {
    if (rate - 1.0).abs() > PLAYBACK_RATE_EPSILON {
        (
            AUDIO_FAST_RATE_PUMP_FRAME_LIMIT,
            AUDIO_FAST_RATE_PUMP_TIME_BUDGET,
        )
    } else {
        (AUDIO_PUMP_FRAME_LIMIT, AUDIO_PUMP_TIME_BUDGET)
    }
}

#[cfg(test)]
fn refresh_danmaku_plan(
    current_plan: &mut Option<DanmakuRenderPlan>,
    viewport: Option<DanmakuViewport>,
    engine: &mut DfmLayoutEngine,
    media_time: Duration,
    generation: u64,
) {
    let Some(viewport) = viewport else {
        return;
    };
    *current_plan = Some(engine.render_plan(media_time, viewport, generation));
}

fn run_async_danmaku_planner(
    shared: Arc<(Mutex<AsyncDanmakuPlannerState>, Condvar)>,
    results: Sender<AsyncDanmakuPlanResult>,
    mut engine: DfmLayoutEngine,
) {
    let (mut timeline, mut config, mut applied_config_revision) = {
        let (lock, _) = &*shared;
        let state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            state.timeline.clone(),
            state.config.clone(),
            state.config_revision,
        )
    };
    let mut seen_revision = 0u64;

    loop {
        let (request, config_update) = {
            let (lock, cvar) = &*shared;
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while !state.shutdown && state.revision == seen_revision {
                state = cvar
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.shutdown {
                return;
            }
            seen_revision = state.revision;
            let config_update = (state.config_revision != applied_config_revision).then(|| {
                let invalidate_placement_history = state.invalidate_placement_history;
                state.invalidate_placement_history = false;
                (
                    state.config_revision,
                    state.timeline.clone(),
                    state.config.clone(),
                    state.rasterizer.clone(),
                    invalidate_placement_history,
                )
            });
            (state.latest_request, config_update)
        };

        if let Some((
            revision,
            next_timeline,
            next_config,
            next_rasterizer,
            invalidate_placement_history,
        )) = config_update
        {
            timeline = next_timeline;
            config = next_config;
            if invalidate_placement_history {
                engine.invalidate_placement_history();
            }
            engine.set_config_with_rasterizer(config.clone(), next_rasterizer);
            applied_config_revision = revision;
        }

        if let Some(request) = request {
            let started = Instant::now();
            let (window, window_start, window_end) = danmaku_plan_window(
                &timeline,
                request.key.media_time,
                &config,
                request.key.viewport,
            );
            engine.sync_timeline(&window);
            engine.recycle_raster_caches_if_needed();
            let prepared = engine.prepare(request.key.viewport, request.key.generation);
            let rasterizer = engine.rasterizer_clone();
            let elapsed = started.elapsed();
            if results
                .send(AsyncDanmakuPlanResult {
                    request,
                    prepared,
                    rasterizer,
                    window_start,
                    window_end,
                    elapsed,
                })
                .is_err()
            {
                return;
            }
        }
    }
}

fn danmaku_plan_window(
    timeline: &DanmakuTimeline,
    media_time: Duration,
    config: &DanmakuLayoutConfig,
    viewport: DanmakuViewport,
) -> (DanmakuTimeline, Duration, Duration) {
    let lookback = scroll_duration_for_viewport(config, viewport) + DANMAKU_PLAN_LOOKBACK_PADDING;
    let start = media_time.checked_sub(lookback).unwrap_or(Duration::ZERO);
    let end = media_time + DANMAKU_PLAN_LOOKAHEAD;
    (timeline.window(start, end), start, end)
}

fn surface_metrics_to_viewport(metrics: SurfaceMetrics) -> DanmakuViewport {
    let (pixel_width, pixel_height) = metrics.physical_size();
    DanmakuViewport::with_scale(pixel_width, pixel_height, metrics.content_scale as f32)
}

fn surface_metrics_label(metrics: SurfaceMetrics) -> String {
    format!(
        "{}x{}@{:.4}",
        metrics.physical_extent.width, metrics.physical_extent.height, metrics.content_scale
    )
}

fn viewport_label(viewport: DanmakuViewport) -> String {
    format!(
        "{}x{}@{:.4}",
        viewport.width, viewport.height, viewport.scale_factor
    )
}

fn danmaku_motion_backstep(mode: DanmakuMode, previous_x: f32, next_x: f32) -> f32 {
    match mode {
        DanmakuMode::Scroll => next_x - previous_x,
        DanmakuMode::ScrollReverse => previous_x - next_x,
        DanmakuMode::Top | DanmakuMode::Bottom | DanmakuMode::Special => 0.0,
    }
}

fn danmaku_viewport_requires_relayout(current: DanmakuViewport, next: DanmakuViewport) -> bool {
    let current_logical_width = current.width as f32 / current.scale_factor;
    let current_logical_height = current.height as f32 / current.scale_factor;
    let next_logical_width = next.width as f32 / next.scale_factor;
    let next_logical_height = next.height as f32 / next.scale_factor;

    current.width.abs_diff(next.width) >= 2
        || current.height.abs_diff(next.height) >= 2
        || (current_logical_width - next_logical_width).abs() >= 2.0
        || (current_logical_height - next_logical_height).abs() >= 2.0
        || (current.scale_factor - next.scale_factor).abs() >= 0.01
}

fn normalize_subtitle_font_scale(scale: f64) -> f64 {
    if scale.is_finite() {
        scale.clamp(0.25, 4.0)
    } else {
        DEFAULT_SUBTITLE_FONT_SCALE
    }
}

fn bump_generation(current_generation: &mut u64, danmaku_generation: &mut u64) {
    *danmaku_generation = danmaku_generation.saturating_add(1).max(1);
    *current_generation = current_generation
        .saturating_add(1)
        .max(*danmaku_generation);
}

fn retain_danmaku_state_for_config_change(
    current: &mut Option<DanmakuRenderPlan>,
    prepared: &mut Option<CurrentDanmakuPrepared>,
    enabled: bool,
    generation: u64,
) -> bool {
    if !enabled {
        *current = None;
        *prepared = None;
        return false;
    }
    if let Some(plan) = current.as_mut() {
        plan.generation = generation;
    }
    if let Some(prepared) = prepared.as_mut() {
        prepared.request.key.generation = generation;
    }
    current.is_some() || prepared.is_some()
}

fn duration_regressed(next: Duration, previous: Duration) -> bool {
    previous
        .checked_sub(next)
        .is_some_and(|delta| delta > Duration::from_millis(5))
}

fn quantize_duration(value: Duration, quantum: Duration) -> Duration {
    if quantum.is_zero() {
        return value;
    }
    let quantum_micros = quantum.as_micros();
    let quantized = (value.as_micros() / quantum_micros) * quantum_micros;
    Duration::from_micros(quantized.min(u128::from(u64::MAX)) as u64)
}

fn duration_label(value: Option<Duration>) -> String {
    value
        .map(|duration| format!("{:.3}", duration.as_secs_f64()))
        .unwrap_or_else(|| "-".to_string())
}

fn subtitle_diag_enabled() -> bool {
    trace::env_flag("ERIKA_SUBTITLE_DIAG")
}

fn overlay_debug_summary(overlay: &OverlayFrame) -> String {
    let first_plane = overlay
        .subtitle_planes
        .first()
        .map(|plane| {
            let max_alpha = plane
                .rgba
                .chunks_exact(4)
                .map(|pixel| pixel[3])
                .max()
                .unwrap_or(0);
            format!(
                "first_rgba=x:{} y:{} w:{} h:{} max_a:{} bytes:{}",
                plane.x,
                plane.y,
                plane.width,
                plane.height,
                max_alpha,
                plane.rgba.len(),
            )
        })
        .unwrap_or_else(|| "first_rgba=none".to_string());
    let first_alpha = overlay
        .subtitle_alpha_planes
        .first()
        .map(|plane| {
            let max_alpha = plane.alpha.iter().copied().max().unwrap_or(0);
            format!(
                "first_alpha=x:{} y:{} w:{} h:{} max_a:{} bytes:{}",
                plane.placement.x,
                plane.placement.y,
                plane.placement.width,
                plane.placement.height,
                max_alpha,
                plane.alpha.len(),
            )
        })
        .unwrap_or_else(|| "first_alpha=none".to_string());
    format!(
        "viewport={}x{} rgba_planes={} alpha_planes={} changed={} {} {}",
        overlay.viewport.width,
        overlay.viewport.height,
        overlay.subtitle_planes.len(),
        overlay.subtitle_alpha_planes.len(),
        overlay.subtitle_changed,
        first_plane,
        first_alpha,
    )
}

impl Drop for PresenterRuntime {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn build_renderer(
    preference: RendererBackendPreference,
    _metal_config: MetalRendererConfig,
) -> Result<Box<dyn RendererBackend>> {
    match preference {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
        RendererBackendPreference::PlatformNative | RendererBackendPreference::Auto => {
            Ok(Box::new(MetalRenderer::with_config(_metal_config)?))
        }
        #[cfg(target_os = "windows")]
        RendererBackendPreference::PlatformNative | RendererBackendPreference::Auto => {
            Ok(Box::new(
                crate::renderer::d3d11::D3d11Renderer::with_config(_metal_config)?,
            ))
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "windows"
        )))]
        RendererBackendPreference::PlatformNative | RendererBackendPreference::Auto => {
            build_wgpu_renderer(_metal_config)
        }
        RendererBackendPreference::WgpuFallback => build_wgpu_renderer(_metal_config),
        RendererBackendPreference::FlutterTexture => Err(PlayerError::Renderer(
            "Flutter texture backend is not supported by the presenter runtime".to_string(),
        )),
    }
}

#[cfg(target_os = "windows")]
fn resolve_presenter_player_config(
    player: &mut PlayerConfig,
    renderer_preference: RendererBackendPreference,
    _supports_mediacodec_surface: bool,
) {
    if matches!(renderer_preference, RendererBackendPreference::WgpuFallback)
        && player.playback.video_decode == VideoDecodePreference::D3d11va
    {
        player.playback.video_decode = VideoDecodePreference::Software;
    }
}

#[cfg(target_os = "android")]
fn resolve_presenter_player_config(
    player: &mut PlayerConfig,
    _renderer_preference: RendererBackendPreference,
    supports_mediacodec_surface: bool,
) {
    if player.playback.video_decode == VideoDecodePreference::MediaCodec
        && !supports_mediacodec_surface
    {
        player.playback.video_decode = VideoDecodePreference::MediaCodecByteBuffer;
        trace::diagnostic(
            serde_json::json!({
                "event": "android_mediacodec_fallback",
                "stage": "renderer_capability_to_bytebuffer",
                "fromMode": "surface_ahardwarebuffer",
                "toMode": "bytebuffer_cpu_upload",
                "surfaceZeroCopyDisabled": true,
                "configurationFallback": true,
                "fallbackCount": 1,
                "reason": "active renderer does not expose Vulkan AHardwareBuffer import capability",
            })
            .to_string(),
        );
    }
}

#[cfg(not(any(target_os = "windows", target_os = "android")))]
fn resolve_presenter_player_config(
    _player: &mut PlayerConfig,
    _renderer_preference: RendererBackendPreference,
    _supports_mediacodec_surface: bool,
) {
}

fn build_audio_output(config: PresenterAudioConfig) -> Box<dyn AudioOutputBackend> {
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        Box::new(PulseAudioOutput::new(config.ring_buffer))
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(CoreAudioOutput::new(CoreAudioOutputConfig {
            ring_buffer: config.ring_buffer,
        }))
    }
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    {
        Box::new(IosAudioQueueOutput::new(IosAudioQueueOutputConfig {
            ring_buffer: config.ring_buffer,
        }))
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(WasapiAudioOutput::new(WasapiAudioOutputConfig {
            ring_buffer: config.ring_buffer,
        }))
    }

    #[cfg(target_os = "android")]
    {
        Box::new(AAudioOutput::new(AAudioOutputConfig {
            ring_buffer: config.ring_buffer,
        }))
    }
    #[cfg(target_env = "ohos")]
    {
        Box::new(OHAudioOutput::new(OHAudioOutputConfig {
            ring_buffer: config.ring_buffer,
        }))
    }
    #[cfg(not(any(
        target_os = "android",
        target_os = "macos",
        any(target_os = "ios", target_os = "tvos"),
        target_os = "windows",
        target_os = "linux",
        target_env = "ohos"
    )))]
    {
        Box::new(BufferedAudioOutput::new(config.ring_buffer))
    }
}

#[cfg(feature = "wgpu")]
fn build_wgpu_renderer(config: MetalRendererConfig) -> Result<Box<dyn RendererBackend>> {
    #[cfg(target_os = "android")]
    let mut renderer =
        crate::renderer::wgpu::AndroidRecoveringWgpuRenderer::new_with_config(config)?;
    #[cfg(not(target_os = "android"))]
    let mut renderer = crate::renderer::wgpu::WgpuRenderer::new_with_config(config)?;
    renderer.set_luma_upscaler(config.luma_upscaler);
    #[cfg(not(target_os = "android"))]
    if config.output_mode.is_edr() {
        trace::diagnostic(
            serde_json::json!({
                "event": "video_output_mode",
                "stage": "configuration_fallback",
                "renderer": "wgpu",
                "requested": "apple_edr",
                "active": "sdr",
                "reason": "Apple EDR is unavailable on the active wgpu platform surface",
            })
            .to_string(),
        );
    }
    Ok(Box::new(renderer))
}

#[cfg(not(feature = "wgpu"))]
fn build_wgpu_renderer(_config: MetalRendererConfig) -> Result<Box<dyn RendererBackend>> {
    Err(PlayerError::Renderer(
        "wgpu renderer backend requires the `wgpu` cargo feature".to_string(),
    ))
}

fn subtitle_is_active(frame: &PlayerSubtitleFrame, pts: Duration) -> bool {
    if frame.frame.is_empty() {
        return false;
    }
    if subtitle_start(frame).is_some_and(|start| pts < start) {
        return false;
    }
    if frame.frame.end.is_some_and(|end| pts >= end) {
        return false;
    }
    true
}

fn subtitle_start(frame: &PlayerSubtitleFrame) -> Option<Duration> {
    frame.frame.start.or(frame.pts)
}

#[derive(Debug, Default)]
struct SubtitleFrameState {
    frames: Vec<PlayerSubtitleFrame>,
    #[cfg(feature = "libass")]
    ass_renderer: CachedAssTrackRenderer,
    #[cfg(feature = "libass")]
    text_renderer: CachedLibassTextRenderer,
}

impl SubtitleFrameState {
    fn clear(&mut self) {
        self.frames.clear();
        #[cfg(feature = "libass")]
        {
            self.ass_renderer.clear();
            self.text_renderer.clear();
        }
    }

    fn push(&mut self, mut frame: PlayerSubtitleFrame) {
        self.retain_at(subtitle_start(&frame).unwrap_or(frame.media_time));
        if frame.frame.is_empty() {
            #[cfg(feature = "libass")]
            {
                self.ass_renderer.clear_track(frame.frame.track_id);
                self.text_renderer.clear();
            }
            self.frames
                .retain(|current| current.frame.track_id != frame.frame.track_id);
            return;
        }

        #[cfg(feature = "libass")]
        match self
            .ass_renderer
            .process_frame(&frame.frame, frame.generation)
        {
            Ok(true) => frame
                .frame
                .text
                .retain(|segment| segment.format != crate::subtitle::SubtitleTextFormat::Ass),
            Ok(false) => {}
            Err(error) => crate::trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_ass_track",
                    "stage": "chunk_rejected",
                    "trackId": frame.frame.track_id,
                    "generation": frame.generation,
                    "reason": error.to_string(),
                })
                .to_string(),
            ),
        }

        if frame.frame.is_empty() {
            return;
        }
        if frame.frame.end.is_none() {
            self.frames
                .retain(|current| current.frame.track_id != frame.frame.track_id);
        }
        self.frames.push(frame);
        self.frames
            .sort_by_key(|frame| subtitle_start(frame).unwrap_or(frame.media_time));
    }

    fn retain_at(&mut self, pts: Duration) {
        self.frames
            .retain(|frame| !frame.frame.is_empty() && frame.frame.end.is_none_or(|end| pts < end));
    }

    fn append_to_overlay(
        &mut self,
        pts: Duration,
        overlay: &mut OverlayFrame,
        style: &SubtitleAssStyle,
    ) {
        self.retain_at(pts);
        let mut subtitle_changed = false;

        #[cfg(feature = "libass")]
        match self.ass_renderer.render(pts, overlay.viewport, style) {
            Ok(Some(bitmaps)) => {
                subtitle_changed |= bitmaps.changed;
                overlay.subtitle_alpha_planes.extend(bitmaps.parts);
            }
            Ok(None) => {}
            Err(error) => crate::trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_ass_track",
                    "stage": "render_failed",
                    "reason": error.to_string(),
                })
                .to_string(),
            ),
        }

        let active = self
            .frames
            .iter()
            .filter(|frame| subtitle_is_active(frame, pts))
            .collect::<Vec<_>>();
        for frame in &active {
            if !frame.frame.bitmap.planes.is_empty() {
                overlay
                    .subtitle_planes
                    .extend(frame.frame.bitmap.planes.iter().cloned().map(|plane| {
                        if plane.canvas_width == 0 || plane.canvas_height == 0 {
                            plane.with_canvas(style.play_res_width, style.play_res_height)
                        } else {
                            plane
                        }
                    }));
                subtitle_changed = true;
            }
        }

        let text_frames = active
            .iter()
            .filter(|frame| frame.frame.has_text())
            .map(|frame| frame.frame.clone())
            .collect::<Vec<_>>();
        if !text_frames.is_empty() {
            subtitle_changed |= self.append_text_subtitles(pts, overlay, &text_frames, style);
        }

        overlay.subtitle_changed |= subtitle_changed;
    }

    #[cfg(feature = "libass")]
    fn append_text_subtitles(
        &mut self,
        pts: Duration,
        overlay: &mut OverlayFrame,
        frames: &[DecodedSubtitleFrame],
        style: &SubtitleAssStyle,
    ) -> bool {
        match self
            .text_renderer
            .render(pts, overlay.viewport, frames, style)
        {
            Ok(Some(bitmaps)) => {
                let changed = bitmaps.changed;
                overlay.subtitle_alpha_planes.extend(bitmaps.parts);
                changed
            }
            Ok(None) => false,
            Err(error) => {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "subtitle_text",
                        "stage": "render_failed",
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
                append_text_subtitles_debug(pts, overlay, frames);
                true
            }
        }
    }

    #[cfg(not(feature = "libass"))]
    fn append_text_subtitles(
        &mut self,
        pts: Duration,
        overlay: &mut OverlayFrame,
        frames: &[DecodedSubtitleFrame],
        _style: &SubtitleAssStyle,
    ) -> bool {
        append_text_subtitles_debug(pts, overlay, frames);
        true
    }
}

#[cfg(feature = "libass")]
#[derive(Debug, Default)]
struct CachedAssTrackRenderer {
    generation: u64,
    track_id: Option<i64>,
    resources: Option<Arc<AssTrackResources>>,
    renderer: Option<LibassSubtitleRenderer>,
    style: SubtitleStyleConfig,
    memory_font_revision: u64,
    chunks: Vec<(String, Duration, Option<Duration>)>,
}

#[cfg(feature = "libass")]
impl CachedAssTrackRenderer {
    fn clear(&mut self) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.flush_events();
        }
        self.generation = 0;
        self.track_id = None;
        self.resources = None;
        self.renderer = None;
        self.memory_font_revision = 0;
        self.chunks.clear();
    }

    fn clear_track(&mut self, track_id: i64) {
        if self.track_id == Some(track_id) {
            self.clear();
        }
    }

    fn process_frame(
        &mut self,
        frame: &DecodedSubtitleFrame,
        generation: u64,
    ) -> crate::subtitle::Result<bool> {
        if !frame.has_ass_chunks() {
            return Ok(false);
        }
        let resources = frame.ass_track.as_ref().ok_or_else(|| {
            SubtitleError::Libass("decoded ASS event has no track resources".to_string())
        })?;
        let resources_changed = self
            .resources
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, resources));
        if self.generation != generation
            || self.track_id != Some(frame.track_id)
            || resources_changed
        {
            self.clear();
            self.renderer = Some(LibassSubtitleRenderer::from_ass_track_with_style(
                frame.track_id,
                resources,
                LibassRenderConfig::default(),
                &self.style,
            )?);
            self.generation = generation;
            self.track_id = Some(frame.track_id);
            self.resources = Some(resources.clone());
        }

        let renderer = self.renderer.as_mut().ok_or_else(|| {
            SubtitleError::Libass("ASS track renderer is unavailable".to_string())
        })?;
        let start = frame.start.unwrap_or(Duration::ZERO);
        for segment in frame
            .text
            .iter()
            .filter(|segment| segment.format == crate::subtitle::SubtitleTextFormat::Ass)
        {
            renderer.process_chunk(&segment.text, start, frame.end)?;
            self.chunks.push((segment.text.clone(), start, frame.end));
        }
        Ok(true)
    }

    fn render(
        &mut self,
        pts: Duration,
        viewport: OverlayViewport,
        style: &SubtitleAssStyle,
    ) -> crate::subtitle::Result<Option<SubtitleBitmapSet>> {
        if self.memory_font_revision != style.memory_font_revision {
            if let (Some(track_id), Some(resources)) = (self.track_id, self.resources.as_ref()) {
                let mut renderer = LibassSubtitleRenderer::from_ass_track_with_style_and_fonts(
                    track_id,
                    resources,
                    LibassRenderConfig::default(),
                    &style.style,
                    style.memory_fonts.clone(),
                )?;
                for (chunk, start, end) in &self.chunks {
                    renderer.process_chunk(chunk, *start, *end)?;
                }
                self.renderer = Some(renderer);
            }
            self.memory_font_revision = style.memory_font_revision;
        }
        if self.style != style.style {
            self.style = style.style.clone();
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return Ok(None);
        };
        renderer.set_play_res_height(style.play_res_height);
        renderer.set_style(&style.style);
        renderer.set_font_scale(style.font_scale);
        match renderer.render(SubtitleRenderRequest::new(
            pts,
            viewport.width,
            viewport.height,
        ))? {
            SubtitleRenderOutput::Alpha(bitmaps) => Ok(Some(bitmaps)),
            SubtitleRenderOutput::Rgba(_) => Err(SubtitleError::Libass(
                "libass renderer returned RGBA output".to_string(),
            )),
        }
    }
}

#[cfg(feature = "libass")]
#[derive(Debug, Default)]
struct CachedLibassTextRenderer {
    script: Option<String>,
    renderer: Option<LibassSubtitleRenderer>,
    memory_font_revision: u64,
}

#[cfg(feature = "libass")]
impl CachedLibassTextRenderer {
    fn clear(&mut self) {
        self.script = None;
        self.renderer = None;
    }

    fn render(
        &mut self,
        pts: Duration,
        viewport: OverlayViewport,
        frames: &[DecodedSubtitleFrame],
        style: &SubtitleAssStyle,
    ) -> crate::subtitle::Result<Option<SubtitleBitmapSet>> {
        let fallback_end = pts.saturating_add(Duration::from_secs(24 * 60 * 60));
        let Some(script) =
            decoded_subtitle_frames_to_ass_script_with_style(frames.iter(), fallback_end, style)
        else {
            self.script = None;
            self.renderer = None;
            return Ok(None);
        };
        if self.script.as_ref() != Some(&script)
            || self.memory_font_revision != style.memory_font_revision
        {
            self.renderer = Some(
                LibassSubtitleRenderer::from_ass_script_with_style_and_fonts(
                    script.as_bytes(),
                    LibassRenderConfig::default(),
                    &style.style,
                    style.memory_fonts.clone(),
                )?,
            );
            self.script = Some(script);
            self.memory_font_revision = style.memory_font_revision;
        }

        let Some(renderer) = self.renderer.as_mut() else {
            return Ok(None);
        };
        renderer.set_override_font_scale(style.font_scale);
        renderer.set_play_res_height(style.play_res_height);
        renderer.set_style(&style.style);
        match renderer.render(SubtitleRenderRequest::new(
            pts,
            viewport.width,
            viewport.height,
        ))? {
            SubtitleRenderOutput::Alpha(bitmaps) => Ok(Some(bitmaps)),
            SubtitleRenderOutput::Rgba(_) => Err(SubtitleError::Libass(
                "libass renderer returned RGBA output".to_string(),
            )),
        }
    }
}

fn append_text_subtitles_debug(
    pts: Duration,
    overlay: &mut OverlayFrame,
    frames: &[DecodedSubtitleFrame],
) {
    let fallback_end = pts.saturating_add(Duration::from_secs(24 * 60 * 60));
    let timeline = decoded_subtitle_frames_to_timeline(frames.iter(), fallback_end);
    let frame = SubtitleRendererCore::new_debug(timeline)
        .render(
            pts,
            SubtitleViewport::new(overlay.viewport.width, overlay.viewport.height),
        )
        .frame;
    overlay.subtitle_planes.extend(frame.planes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "ios")]
    #[test]
    fn ios_display_target_samples_one_clock_for_visuals_and_preserves_commands() {
        use crate::core::PlayerState;
        use crate::playback::PlaybackClock;

        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let origin = Instant::now();
        let mut snapshot = PlaybackSnapshot {
            clock: PlaybackClock::running_at(Duration::from_secs(10), origin),
            generation: 2,
            state: PlayerState::Playing,
        };
        presenter.current_overlay = Some(OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(640, 360),
            subtitle_planes: vec![],
            subtitle_alpha_planes: vec![],
            subtitle_changed: false,
        });
        presenter.subtitle_resolution.source = Some(OverlayViewport::new(640, 360));
        // Replaying the same display target after other work must not move it.
        for hz in [60.0, 120.0] {
            for frame in 1..=6 {
                let elapsed = Duration::from_secs_f64(frame as f64 / hz);
                let target = origin + elapsed;
                presenter.apply_playback_snapshot(&snapshot, Some(target));
                assert_eq!(
                    presenter.current_media_time,
                    Duration::from_secs(10) + elapsed
                );
                let before = presenter.current_media_time;
                presenter.apply_playback_snapshot(&snapshot, Some(target));
                assert_eq!(presenter.current_media_time, before);
                assert_eq!(presenter.current_overlay.as_ref().unwrap().pts, before);
                assert_eq!(presenter.current_generation, 2);
            }
        }
        // Pause and both long and short seeks retain the authoritative epoch.
        snapshot.clock.pause(origin + Duration::from_millis(100));
        snapshot.state = PlayerState::Paused;
        for (generation, position_ms) in [(3, 20000), (4, 8000), (5, 8100), (6, 8000)] {
            snapshot
                .clock
                .seek(Duration::from_millis(position_ms), origin);
            snapshot.generation = generation;
            presenter.apply_playback_snapshot(&snapshot, Some(origin + Duration::from_secs(2)));
            assert_eq!(
                presenter.current_media_time,
                Duration::from_millis(position_ms)
            );
            assert_eq!(presenter.current_generation, generation);
        }
        for rate in [0.5, 1.0, 2.0] {
            snapshot.clock = PlaybackClock::running_at(Duration::from_secs(8), origin);
            snapshot.clock.set_rate(rate, origin);
            snapshot.state = PlayerState::Playing;
            presenter.apply_playback_snapshot(&snapshot, Some(origin + Duration::from_secs(1)));
            assert_eq!(
                presenter.current_media_time,
                Duration::from_secs_f64(8.0 + rate)
            );
            assert_eq!(snapshot.clock.media_time_at(origin), Duration::from_secs(8));
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct CapturedComposition {
        has_overlay: bool,
        has_danmaku: bool,
        width: u32,
        height: u32,
    }

    struct CaptureCompositionProbe {
        captured: Arc<Mutex<Option<CapturedComposition>>>,
    }

    impl RendererBackend for CaptureCompositionProbe {
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

        fn render_current_frame(&mut self, _context: RenderFrameContext<'_>) -> Result<bool> {
            Ok(false)
        }

        fn capture_current_frame(
            &mut self,
            context: RenderFrameContext<'_>,
            width: u32,
            height: u32,
        ) -> Result<Option<crate::core::RendererFrameCapture>> {
            *self.captured.lock().expect("capture probe mutex poisoned") =
                Some(CapturedComposition {
                    has_overlay: context.overlay.is_some(),
                    has_danmaku: context.danmaku.is_some(),
                    width,
                    height,
                });
            Ok(Some(crate::core::RendererFrameCapture {
                width,
                height,
                rgba: vec![0; width as usize * height as usize * 4],
            }))
        }
    }

    #[test]
    fn rejected_mediacodec_surface_route_allows_bytebuffer_recovery() {
        let rejected = RejectedVideoImportRoute::new(DecoderBackend::MediaCodec, true, 7);
        let byte_buffer = RejectedVideoImportRoute::new(DecoderBackend::MediaCodec, false, 7);

        assert!(!should_reject_video_import(Some(rejected), byte_buffer));
    }

    #[test]
    fn rejected_mediacodec_bytebuffer_route_suppresses_repeat_failures() {
        let rejected = RejectedVideoImportRoute::new(DecoderBackend::MediaCodec, false, 7);
        let next_byte_buffer = RejectedVideoImportRoute::new(DecoderBackend::MediaCodec, false, 7);
        let software = RejectedVideoImportRoute::new(DecoderBackend::Software, false, 7);

        assert!(should_reject_video_import(Some(rejected), next_byte_buffer));
        assert!(!should_reject_video_import(Some(rejected), software));
    }

    #[test]
    fn rejected_route_does_not_suppress_the_same_route_in_a_new_generation() {
        let rejected = RejectedVideoImportRoute::new(DecoderBackend::MediaCodec, true, 7);
        let reopened_surface = RejectedVideoImportRoute::new(DecoderBackend::MediaCodec, true, 8);

        assert!(!should_reject_video_import(
            Some(rejected),
            reopened_surface
        ));
    }

    #[test]
    fn video_frame_backpressure_logging_is_exponentially_throttled() {
        let reported = (1..=10)
            .filter(|count| should_report_video_frame_backpressure(*count))
            .collect::<Vec<_>>();

        assert_eq!(reported, vec![1, 2, 4, 8]);
    }
    use crate::danmaku::{DanmakuColor, DanmakuItem, DanmakuMode};
    use crate::subtitle::{
        DecodedSubtitleFrame, SubtitleBitmapPlane, SubtitleTextFormat, SubtitleTextSegment,
    };

    fn subtitle_frame(start: Duration, end: Option<Duration>) -> PlayerSubtitleFrame {
        let mut frame = DecodedSubtitleFrame::new(2, Some(start), end);
        frame.push_bitmap_plane(
            SubtitleBitmapPlane::new(0, 0, 1, 1, vec![255, 255, 255, 255]),
            false,
        );
        PlayerSubtitleFrame {
            frame,
            pts: Some(start),
            media_time: start,
            late_by: None,
            generation: 1,
        }
    }

    fn text_subtitle_frame(
        track_id: i64,
        start: Duration,
        end: Option<Duration>,
        text: &str,
    ) -> PlayerSubtitleFrame {
        let mut frame = DecodedSubtitleFrame::new(track_id, Some(start), end);
        frame.push_text(SubtitleTextSegment::new(
            SubtitleTextFormat::PlainText,
            text,
        ));
        PlayerSubtitleFrame {
            frame,
            pts: Some(start),
            media_time: start,
            late_by: None,
            generation: 1,
        }
    }

    #[cfg(feature = "libass")]
    fn ass_subtitle_frame(
        track_id: i64,
        generation: u64,
        start: Duration,
        end: Duration,
        read_order: u64,
        resources: Arc<AssTrackResources>,
    ) -> PlayerSubtitleFrame {
        let mut frame = DecodedSubtitleFrame::new(track_id, Some(start), Some(end));
        frame.push_text(SubtitleTextSegment::new(
            SubtitleTextFormat::Ass,
            format!("{read_order},0,Default,,0,0,0,,{{\\pos(100,80)\\fad(50,50)}}streamed"),
        ));
        frame.ass_track = Some(resources);
        PlayerSubtitleFrame {
            frame,
            pts: Some(start),
            media_time: start,
            late_by: None,
            generation,
        }
    }

    #[cfg(feature = "libass")]
    fn ass_test_header() -> String {
        r#"[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,32,&H00FFFFFF,&H000000FF,&H80000000,&H80000000,0,0,0,0,100,100,0,0,1,2,0,2,20,20,24,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"#
        .to_string()
    }

    fn empty_overlay() -> OverlayFrame {
        OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(640, 360),
            subtitle_planes: Vec::new(),
            subtitle_alpha_planes: Vec::new(),
            subtitle_changed: false,
        }
    }

    fn danmaku_item(id: u64, time: f64, text: &str) -> DanmakuItem {
        DanmakuItem {
            id,
            pts: Duration::from_secs_f64(time),
            text: text.to_string(),
            mode: DanmakuMode::Scroll,
            font_size: 24.0,
            color: DanmakuColor::WHITE,
            opacity: 1.0,
            is_self: false,
        }
    }

    fn danmaku_engine(text: &str) -> DfmLayoutEngine {
        let timeline = DanmakuTimeline::new(vec![danmaku_item(1, 1.0, text)]).unwrap();
        DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default())
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_wgpu_presenter_uses_software_decode_until_zero_copy_interop_exists() {
        let mut player = PlayerConfig::default();
        player.renderer = RendererBackendPreference::WgpuFallback;
        player.playback.video_decode = VideoDecodePreference::D3d11va;

        resolve_presenter_player_config(
            &mut player,
            RendererBackendPreference::WgpuFallback,
            false,
        );

        assert_eq!(
            player.playback.video_decode,
            VideoDecodePreference::Software
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_native_presenter_keeps_d3d11va_for_zero_copy_interop() {
        let mut player = PlayerConfig::default();
        player.renderer = RendererBackendPreference::PlatformNative;
        player.playback.video_decode = VideoDecodePreference::D3d11va;

        resolve_presenter_player_config(
            &mut player,
            RendererBackendPreference::PlatformNative,
            false,
        );

        assert_eq!(player.playback.video_decode, VideoDecodePreference::D3d11va);
    }

    #[cfg(target_os = "android")]
    #[test]
    fn android_presenter_uses_bytebuffer_when_ahb_interop_is_unavailable() {
        let mut player = PlayerConfig::default();
        player.playback.video_decode = VideoDecodePreference::MediaCodec;

        resolve_presenter_player_config(&mut player, RendererBackendPreference::Auto, false);

        assert_eq!(
            player.playback.video_decode,
            VideoDecodePreference::MediaCodecByteBuffer
        );
    }

    #[cfg(target_os = "android")]
    #[test]
    fn android_presenter_keeps_surface_mediacodec_when_ahb_interop_is_available() {
        let mut player = PlayerConfig::default();
        player.playback.video_decode = VideoDecodePreference::MediaCodec;

        resolve_presenter_player_config(&mut player, RendererBackendPreference::Auto, true);

        assert_eq!(
            player.playback.video_decode,
            VideoDecodePreference::MediaCodec
        );
    }

    #[cfg(target_os = "android")]
    #[test]
    fn android_presenter_preserves_explicit_non_surface_decode_modes() {
        for mode in [
            VideoDecodePreference::Software,
            VideoDecodePreference::MediaCodecByteBuffer,
        ] {
            let mut player = PlayerConfig::default();
            player.playback.video_decode = mode;

            resolve_presenter_player_config(&mut player, RendererBackendPreference::Auto, false);

            assert_eq!(player.playback.video_decode, mode);
        }
    }

    #[test]
    fn subtitle_active_window_respects_start_end_and_empty_frames() {
        let active = subtitle_frame(Duration::from_secs(1), Some(Duration::from_secs(3)));

        assert!(!subtitle_is_active(&active, Duration::from_millis(999)));
        assert!(subtitle_is_active(&active, Duration::from_secs(1)));
        assert!(subtitle_is_active(&active, Duration::from_millis(2999)));
        assert!(!subtitle_is_active(&active, Duration::from_secs(3)));

        let empty = PlayerSubtitleFrame {
            frame: DecodedSubtitleFrame::new(2, Some(Duration::ZERO), None),
            pts: Some(Duration::ZERO),
            media_time: Duration::ZERO,
            late_by: None,
            generation: 1,
        };
        assert!(!subtitle_is_active(&empty, Duration::ZERO));
    }

    #[test]
    fn subtitle_state_keeps_overlapping_bitmap_frames() {
        let mut state = SubtitleFrameState::default();
        state.push(subtitle_frame(
            Duration::from_secs(1),
            Some(Duration::from_secs(4)),
        ));
        state.push(subtitle_frame(
            Duration::from_secs(2),
            Some(Duration::from_secs(5)),
        ));
        let mut overlay = empty_overlay();

        state.append_to_overlay(
            Duration::from_secs(3),
            &mut overlay,
            &SubtitleAssStyle::default(),
        );

        assert_eq!(overlay.subtitle_planes.len(), 2);
        assert!(overlay.subtitle_changed);
    }

    #[test]
    fn subtitle_supersampling_keeps_bitmap_pixels_and_supplies_missing_canvas() {
        let mut state = SubtitleFrameState::default();
        state.push(subtitle_frame(Duration::ZERO, Some(Duration::from_secs(3))));
        let mut overlay = empty_overlay();
        overlay.viewport = OverlayViewport::new(1280, 720);
        let style = SubtitleAssStyle {
            play_res_width: 640,
            play_res_height: 360,
            ..SubtitleAssStyle::default()
        };
        state.append_to_overlay(Duration::from_secs(1), &mut overlay, &style);
        let plane = &overlay.subtitle_planes[0];
        assert_eq!(plane.rgba.len(), 4);
        assert_eq!(plane.scaled_rect(1280, 720), (0, 0, 2, 2));
    }

    #[test]
    fn subtitle_state_expires_old_frames_and_empty_frame_clears_track() {
        let mut state = SubtitleFrameState::default();
        state.push(subtitle_frame(
            Duration::from_secs(1),
            Some(Duration::from_secs(2)),
        ));
        state.push(subtitle_frame(
            Duration::from_secs(3),
            Some(Duration::from_secs(5)),
        ));
        let mut overlay = empty_overlay();

        state.append_to_overlay(
            Duration::from_secs(4),
            &mut overlay,
            &SubtitleAssStyle::default(),
        );

        assert_eq!(overlay.subtitle_planes.len(), 1);

        state.push(PlayerSubtitleFrame {
            frame: DecodedSubtitleFrame::new(2, Some(Duration::from_secs(4)), None),
            pts: Some(Duration::from_secs(4)),
            media_time: Duration::from_secs(4),
            late_by: None,
            generation: 1,
        });
        let mut overlay = empty_overlay();
        state.append_to_overlay(
            Duration::from_millis(4500),
            &mut overlay,
            &SubtitleAssStyle::default(),
        );

        assert!(overlay.subtitle_planes.is_empty());
    }

    #[test]
    fn subtitle_state_clear_removes_open_ended_bitmap_frames() {
        let mut state = SubtitleFrameState::default();
        state.push(subtitle_frame(Duration::from_secs(1), None));
        let mut overlay = empty_overlay();

        state.append_to_overlay(
            Duration::from_secs(2),
            &mut overlay,
            &SubtitleAssStyle::default(),
        );
        assert_eq!(overlay.subtitle_planes.len(), 1);

        state.clear();
        let mut overlay = empty_overlay();
        state.append_to_overlay(
            Duration::from_secs(3),
            &mut overlay,
            &SubtitleAssStyle::default(),
        );

        assert!(overlay.subtitle_planes.is_empty());
    }

    #[test]
    fn subtitle_state_renders_text_frames_into_overlay() {
        let mut state = SubtitleFrameState::default();
        state.push(text_subtitle_frame(
            7,
            Duration::from_secs(1),
            Some(Duration::from_secs(3)),
            "hello",
        ));
        let mut overlay = empty_overlay();

        state.append_to_overlay(
            Duration::from_secs(2),
            &mut overlay,
            &SubtitleAssStyle::default(),
        );

        #[cfg(feature = "libass")]
        assert!(!overlay.subtitle_alpha_planes.is_empty());
        #[cfg(not(feature = "libass"))]
        assert!(!overlay.subtitle_planes.is_empty());
        assert!(overlay.subtitle_changed);
    }

    #[cfg(feature = "libass")]
    #[test]
    fn supersampling_preserves_text_and_ass_size_with_and_without_user_overrides() {
        for ass in [false, true] {
            for override_mask in [0, crate::subtitle::SUBTITLE_OVERRIDE_ALL] {
                let mut state = SubtitleFrameState::default();
                state.push(if ass {
                    ass_subtitle_frame(
                        2,
                        1,
                        Duration::ZERO,
                        Duration::from_secs(2),
                        1,
                        Arc::new(AssTrackResources::new(
                            2,
                            Arc::<[u8]>::from(ass_test_header().as_bytes()),
                            Arc::<[SubtitleFontAttachment]>::from([]),
                        )),
                    )
                } else {
                    text_subtitle_frame(
                        2,
                        Duration::ZERO,
                        Some(Duration::from_secs(2)),
                        "Sharper subtitles",
                    )
                });
                let style = SubtitleAssStyle {
                    play_res_width: 640,
                    play_res_height: 360,
                    font_scale: 1.25,
                    style: SubtitleStyleConfig {
                        override_mask,
                        ..SubtitleStyleConfig::default()
                    },
                    ..SubtitleAssStyle::default()
                };
                let mut normal = empty_overlay();
                state.append_to_overlay(Duration::from_millis(500), &mut normal, &style);
                let mut high = empty_overlay();
                high.viewport = OverlayViewport::new(1280, 720);
                state.append_to_overlay(Duration::from_millis(500), &mut high, &style);
                let bounds = |overlay: &OverlayFrame| {
                    let parts = &overlay.subtitle_alpha_planes;
                    assert!(!parts.is_empty());
                    // libass pads bitmaps for SIMD alignment. Compare visible
                    // coverage, not that transparent allocation padding.
                    let mut bounds = [
                        f64::INFINITY,
                        f64::INFINITY,
                        f64::NEG_INFINITY,
                        f64::NEG_INFINITY,
                    ];
                    for part in parts {
                        for y in 0..part.placement.height as usize {
                            for x in 0..part.placement.width as usize {
                                if part.alpha[y * part.stride + x] < 8 {
                                    continue;
                                }
                                let x = part.placement.x as f64 + x as f64;
                                let y = part.placement.y as f64 + y as f64;
                                bounds[0] = bounds[0].min(x);
                                bounds[1] = bounds[1].min(y);
                                bounds[2] = bounds[2].max(x + 1.0);
                                bounds[3] = bounds[3].max(y + 1.0);
                            }
                        }
                    }
                    bounds
                };
                for (normal, high) in bounds(&normal).into_iter().zip(bounds(&high)) {
                    assert!(
                        (normal - high / 2.0).abs() <= 3.0,
                        "ass={ass} override={override_mask}: normal={normal}, high={high}"
                    );
                }
                // Returning to native resolution must invalidate libass's atlas.
                let mut restored = empty_overlay();
                state.append_to_overlay(Duration::from_millis(500), &mut restored, &style);
                assert!(restored.subtitle_changed);
                assert_eq!(bounds(&restored), bounds(&normal));
            }
        }
    }

    #[cfg(all(feature = "libass", any(target_os = "macos", feature = "wgpu")))]
    #[test]
    fn subtitle_quality_toggles_refresh_paused_frames_without_accumulating_scale() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter.current_media_time = Duration::from_secs(1);
        presenter.subtitles.push(text_subtitle_frame(
            2,
            Duration::ZERO,
            Some(Duration::from_secs(3)),
            "Paused subtitle",
        ));
        presenter.update_overlay(Duration::from_secs(1), 1, 640, 360);
        assert_eq!(
            presenter.current_overlay.as_ref().unwrap().viewport,
            OverlayViewport::new(1280, 720)
        );
        presenter.set_subtitle_supersampling(false);
        assert_eq!(
            presenter.current_overlay.as_ref().unwrap().viewport,
            OverlayViewport::new(640, 360)
        );
        presenter.set_subtitle_supersampling(true);
        for _ in 0..3 {
            presenter.refresh_current_overlay();
            assert_eq!(
                presenter.current_overlay.as_ref().unwrap().viewport,
                OverlayViewport::new(1280, 720)
            );
        }
        presenter.set_subtitle_supersampling(false);
        assert_eq!(
            presenter.current_overlay.as_ref().unwrap().viewport,
            OverlayViewport::new(640, 360)
        );
        assert!(
            !presenter
                .current_overlay
                .as_ref()
                .unwrap()
                .subtitle_alpha_planes
                .is_empty()
        );
    }

    #[cfg(feature = "libass")]
    #[test]
    fn subtitle_state_streams_ass_into_alpha_planes_and_clears_track() {
        let header = ass_test_header();
        let resources = Arc::new(AssTrackResources::new(
            2,
            Arc::<[u8]>::from(header.as_bytes()),
            Arc::<[crate::subtitle::SubtitleFontAttachment]>::from([]),
        ));
        let mut state = SubtitleFrameState::default();
        state.push(ass_subtitle_frame(
            2,
            3,
            Duration::ZERO,
            Duration::from_secs(2),
            7,
            resources,
        ));
        assert!(state.frames.is_empty());

        let mut overlay = empty_overlay();
        state.append_to_overlay(
            Duration::from_millis(500),
            &mut overlay,
            &SubtitleAssStyle::default(),
        );
        assert!(overlay.subtitle_planes.is_empty());
        assert!(!overlay.subtitle_alpha_planes.is_empty());
        assert!(overlay.subtitle_changed);

        state.push(PlayerSubtitleFrame {
            frame: DecodedSubtitleFrame::new(2, Some(Duration::from_secs(1)), None),
            pts: Some(Duration::from_secs(1)),
            media_time: Duration::from_secs(1),
            late_by: None,
            generation: 4,
        });
        let mut cleared = empty_overlay();
        state.append_to_overlay(
            Duration::from_millis(750),
            &mut cleared,
            &SubtitleAssStyle::default(),
        );
        assert!(cleared.subtitle_alpha_planes.is_empty());
        assert!(state.ass_renderer.renderer.is_none());
    }

    #[cfg(feature = "libass")]
    #[test]
    fn subtitle_state_rebuilds_ass_track_on_generation_change() {
        let header = ass_test_header();
        let resources = Arc::new(AssTrackResources::new(
            2,
            Arc::<[u8]>::from(header.as_bytes()),
            Arc::<[crate::subtitle::SubtitleFontAttachment]>::from([]),
        ));
        let mut state = SubtitleFrameState::default();
        state.push(ass_subtitle_frame(
            2,
            1,
            Duration::ZERO,
            Duration::from_secs(2),
            1,
            resources.clone(),
        ));
        state.push(ass_subtitle_frame(
            2,
            2,
            Duration::from_secs(10),
            Duration::from_secs(12),
            1,
            resources,
        ));

        assert_eq!(state.ass_renderer.generation, 2);
        let mut overlay = empty_overlay();
        state.append_to_overlay(
            Duration::from_millis(500),
            &mut overlay,
            &SubtitleAssStyle::default(),
        );
        assert!(overlay.subtitle_alpha_planes.is_empty());
    }

    #[cfg(feature = "libass")]
    #[test]
    fn subtitle_state_resets_memory_font_revision_on_clear() {
        let header = ass_test_header();
        let resources = Arc::new(AssTrackResources::new(
            2,
            Arc::<[u8]>::from(header.as_bytes()),
            Arc::<[crate::subtitle::SubtitleFontAttachment]>::from([]),
        ));
        let mut state = SubtitleFrameState::default();

        // Simulate memory fonts having been installed at revision 5.
        state.ass_renderer.memory_font_revision = 5;

        // Push an ASS frame with a different track_id to trigger clear() via
        // process_frame when the track_id doesn't match.
        state.push(ass_subtitle_frame(
            3,
            1,
            Duration::ZERO,
            Duration::from_secs(2),
            1,
            resources,
        ));

        // Regression: clear() must reset memory_font_revision so that the
        // next render() call detects the mismatch and rebuilds the renderer
        // with the current memory-font snapshot.
        assert_eq!(
            state.ass_renderer.memory_font_revision, 0,
            "memory_font_revision should be reset to 0 after clear()"
        );

        // When render() is called with a non-zero revision, it should
        // trigger a rebuild with memory fonts and update the field.
        let mut overlay = empty_overlay();
        let mut style = SubtitleAssStyle::default();
        style.memory_font_revision = 5;
        state.append_to_overlay(Duration::from_millis(500), &mut overlay, &style);

        assert_eq!(
            state.ass_renderer.memory_font_revision, 5,
            "memory_font_revision should be updated after render() rebuilds with memory fonts"
        );
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn registering_an_unselected_memory_font_does_not_invalidate_renderers() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let danmaku_generation = presenter.danmaku_font_selection().generation;
        let subtitle_revision = presenter
            .subtitle_ass_style(OverlayViewport::new(640, 360))
            .memory_font_revision;

        let id = presenter
            .register_subtitle_font_bytes(crate::NIPAPLAY_FALLBACK_FONT)
            .unwrap();

        assert!(id > 0);
        assert_eq!(presenter.subtitle_memory_font_status().generation, 1);
        assert_eq!(
            presenter.danmaku_font_selection().generation,
            danmaku_generation
        );
        assert_eq!(
            presenter
                .subtitle_ass_style(OverlayViewport::new(640, 360))
                .memory_font_revision,
            subtitle_revision
        );
    }

    #[test]
    fn danmaku_generation_bump_clears_stale_plans_after_seek() {
        let mut generation = 7;
        let mut danmaku_generation = 4;

        bump_generation(&mut generation, &mut danmaku_generation);

        assert_eq!(danmaku_generation, 5);
        assert_eq!(generation, 8);
    }

    #[test]
    fn danmaku_motion_trace_detects_only_opposite_scroll_direction() {
        assert!(danmaku_motion_backstep(DanmakuMode::Scroll, 500.0, 490.0) <= 0.0);
        assert_eq!(
            danmaku_motion_backstep(DanmakuMode::Scroll, 490.0, 495.0),
            5.0
        );
        assert!(danmaku_motion_backstep(DanmakuMode::ScrollReverse, 100.0, 110.0) <= 0.0);
        assert_eq!(
            danmaku_motion_backstep(DanmakuMode::ScrollReverse, 110.0, 104.0),
            6.0
        );
        assert_eq!(danmaku_motion_backstep(DanmakuMode::Top, 100.0, 140.0), 0.0);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_danmaku_worker_exits_before_presenter_can_unload() {
        for _ in 0..32 {
            let planner = AsyncDanmakuPlanner::new(
                danmaku_engine("shutdown"),
                DanmakuTimeline::default(),
                DanmakuLayoutConfig::default(),
            );
            let shared = Arc::clone(&planner.shared);
            drop(planner);
            // No worker, including one not yet scheduled, may retain state
            // and execute Rust code after the containing DLL is unloaded.
            assert_eq!(Arc::strong_count(&shared), 1);
        }
    }

    #[test]
    fn async_danmaku_planner_applies_font_selection_generation() {
        let engine = danmaku_engine("async font");
        let timeline = DanmakuTimeline::new(vec![danmaku_item(1, 1.0, "async font")]).unwrap();
        let mut planner =
            AsyncDanmakuPlanner::new(engine, timeline, DanmakuLayoutConfig::default());
        let selection = DanmakuFontSelection::new(
            9,
            Arc::from(vec![SubtitleFontAttachment::new(
                "memory",
                None,
                Vec::new(),
                Arc::<[u8]>::from(crate::NIPAPLAY_FALLBACK_FONT),
            )]),
        );
        planner.set_font_selection(selection);
        let key = DanmakuPlanKey {
            media_time: Duration::from_secs(1),
            viewport: DanmakuViewport::new(640, 360),
            generation: 9,
        };
        planner.request_plan(key);

        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            if let Some(result) = planner.try_recv() {
                break result;
            }
            assert!(Instant::now() < deadline, "async planner timed out");
            thread::sleep(Duration::from_millis(10));
        };

        assert_eq!(result.request.key, key);
        assert!(!result.prepared.items().is_empty());
    }

    #[test]
    fn presenter_config_disables_idle_test_pattern_by_default() {
        assert!(!PresenterConfig::default().render_test_pattern_when_idle);
    }

    #[test]
    fn repeated_playback_rate_request_keeps_pending_transition_deadline() {
        let deadline = Instant::now() + Duration::from_millis(250);
        let pending = PendingPlaybackRate {
            rate: 2.0,
            commit_at: deadline,
        };

        assert!(playback_rate_request_is_idempotent(1.0, Some(pending), 2.0));
        assert_eq!(pending.commit_at, deadline);
        assert!(playback_rate_request_is_idempotent(2.0, None, 2.0));
        assert!(!playback_rate_request_is_idempotent(
            1.0,
            Some(pending),
            1.5
        ));
    }

    #[test]
    fn audio_transition_bridge_includes_platform_output_duration() {
        let snapshot = AudioClockSnapshot {
            media_time: Some(Duration::from_secs(1)),
            queued_duration: Some(Duration::from_millis(250)),
            queued_frames: 12_000,
            read_frames: 0,
            written_frames: 12_000,
            underflow_frames: 0,
        };

        assert_eq!(
            audio_transition_bridge(Some(snapshot), Duration::from_millis(60)),
            Some(Duration::from_millis(310))
        );
        assert_eq!(
            audio_transition_bridge(
                Some(AudioClockSnapshot {
                    queued_duration: Some(Duration::ZERO),
                    ..snapshot
                }),
                Duration::from_millis(60),
            ),
            Some(Duration::from_millis(60))
        );
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn audio_reset_preserves_pending_rate_for_transition() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter.pending_playback_rate = Some(PendingPlaybackRate {
            rate: 2.0,
            commit_at: Instant::now(),
        });

        presenter.reset_audio_output();
        assert_eq!(
            presenter.pending_playback_rate.map(|pending| pending.rate),
            Some(2.0)
        );
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn terminal_audio_reset_restores_committed_rate() {
        use crate::audio::BufferedAudioOutput;
        use crate::ffmpeg::{PcmAudioFrame, PcmFormat};

        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let mut output = BufferedAudioOutput::new(AudioRingBufferConfig::default());
        output.set_playback_rate(2.0);
        presenter.audio_output = Box::new(output);
        presenter.playback_rate = 1.0;
        presenter.pending_playback_rate = Some(PendingPlaybackRate {
            rate: 2.0,
            commit_at: Instant::now(),
        });

        presenter.reset_audio_output_with_committed_rate();
        let format = PcmFormat::f32_interleaved(48_000, 2);
        presenter
            .audio_output
            .push(PcmAudioFrame {
                format,
                pts: Some(Duration::ZERO),
                frames: 24_000,
                samples: vec![0.0; 48_000],
            })
            .unwrap();

        assert_eq!(
            presenter
                .audio_output
                .clock_snapshot()
                .unwrap()
                .queued_frames,
            24_000
        );
        assert!(presenter.pending_playback_rate.is_none());
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn failed_pending_rate_commit_clears_transition_state() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter.pending_playback_rate = Some(PendingPlaybackRate {
            rate: 2.0,
            commit_at: Instant::now(),
        });

        assert!(presenter.commit_pending_playback_rate_now().is_err());
        assert!(presenter.pending_playback_rate.is_none());
        assert_eq!(presenter.playback_rate, 1.0);
    }

    #[test]
    fn playback_rate_uses_fast_audio_pump_limits() {
        assert_eq!(
            audio_pump_limits_for_rate(1.0),
            (AUDIO_PUMP_FRAME_LIMIT, AUDIO_PUMP_TIME_BUDGET)
        );
        assert_eq!(
            audio_pump_limits_for_rate(2.0),
            (
                AUDIO_FAST_RATE_PUMP_FRAME_LIMIT,
                AUDIO_FAST_RATE_PUMP_TIME_BUDGET
            )
        );
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn layout_config_generation_does_not_disturb_the_playback_clock() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let playback_generation = presenter.player.playback_generation();
        presenter.current_media_time = Duration::from_millis(2050);

        let original_layout_generation = presenter.current_generation;
        let mut config = DanmakuLayoutConfig::default();
        config.font_size += 1.0;
        presenter.set_danmaku_config(config);

        // A danmaku layout generation invalidates prepared plans; it is not a
        // seek, so it must leave the shared playback clock untouched.
        assert!(presenter.current_generation > original_layout_generation);
        assert_eq!(presenter.player.playback_generation(), playback_generation);
        assert_eq!(presenter.current_media_time, Duration::from_millis(2050));
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn idle_tick_does_not_render_test_pattern_by_default() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();

        let stats = presenter.render_tick(0.0).unwrap();

        assert_eq!(stats.rendered_test_frames, 0);
        assert_eq!(
            presenter.runtime_snapshot().last_render_test_duration,
            Duration::ZERO
        );
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn foreground_tick_keeps_video_resume_pending_until_surface_is_ready() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter.audio_only_tick_active = true;

        presenter.render_tick(0.0).unwrap();
        assert!(presenter.audio_only_tick_active);
        assert!(presenter.resume_pending);

        presenter.render_tick(0.016).unwrap();
        assert!(presenter.audio_only_tick_active);
        assert!(presenter.resume_pending);

        presenter.audio_only_tick().unwrap();
        assert!(presenter.audio_only_tick_active);
        assert!(!presenter.resume_pending);
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn foreground_video_resume_stops_retrying_after_the_attempt_budget() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter.audio_only_tick_active = true;
        presenter.resume_pending = true;

        for attempt in 1..MAX_VIDEO_DECODE_RESUME_ATTEMPTS {
            presenter.note_video_decode_resume_failure("playback worker is not running");
            assert_eq!(presenter.video_decode_resume_attempts, attempt);
            assert!(presenter.resume_pending);
        }

        presenter.note_video_decode_resume_failure("playback worker is not running");
        assert_eq!(
            presenter.video_decode_resume_attempts,
            MAX_VIDEO_DECODE_RESUME_ATTEMPTS
        );
        assert!(!presenter.resume_pending);

        // A spent budget must not be re-armed by the next display frame, or the
        // wedged worker would be probed once per tick again.
        presenter.render_tick(0.016).unwrap();
        assert!(presenter.audio_only_tick_active);
        assert!(!presenter.resume_pending);
        assert_eq!(
            presenter.video_decode_resume_attempts,
            MAX_VIDEO_DECODE_RESUME_ATTEMPTS
        );

        // A new background/foreground cycle opens a fresh window.
        presenter.audio_only_tick().unwrap();
        assert_eq!(presenter.video_decode_resume_attempts, 0);
        presenter.render_tick(0.032).unwrap();
        assert!(presenter.resume_pending);
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn stop_keeps_the_video_decode_resume_owed_to_the_reused_worker() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter.audio_only_tick_active = true;
        presenter.resume_pending = true;
        presenter.video_decode_resume_attempts = MAX_VIDEO_DECODE_RESUME_ATTEMPTS;

        // `stop` reuses the current engine and never clears its
        // `video_decode_suspended` flag, so the presenter must keep owing it a
        // resume; only the in-flight attempt and its budget are dropped.
        let _ = presenter.stop();
        assert!(presenter.audio_only_tick_active);
        assert!(!presenter.resume_pending);
        assert_eq!(presenter.video_decode_resume_attempts, 0);

        // Returning to the foreground still arms the resume, so playback does
        // not stay audio-only with a frozen frame.
        presenter.render_tick(0.0).unwrap();
        assert!(presenter.resume_pending);
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn only_engine_replacing_boundaries_forget_the_video_decode_resume() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();

        // `open` builds a fresh engine and `close` retires the worker, and a
        // new engine decodes video, so the presenter owes it no resume.
        presenter.audio_only_tick_active = true;
        presenter.resume_pending = true;
        presenter.video_decode_resume_attempts = 2;
        presenter.reset_video_decode_resume_state();
        assert!(!presenter.audio_only_tick_active);
        assert!(!presenter.resume_pending);
        assert_eq!(presenter.video_decode_resume_attempts, 0);

        // `stop` and a stopped-at-end replay reuse the engine, which keeps its
        // `video_decode_suspended` flag set, so the obligation must survive.
        presenter.audio_only_tick_active = true;
        presenter.resume_pending = true;
        presenter.video_decode_resume_attempts = 2;
        presenter.cancel_pending_video_decode_resume();
        assert!(presenter.audio_only_tick_active);
        assert!(!presenter.resume_pending);
        assert_eq!(presenter.video_decode_resume_attempts, 0);
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn repeated_danmaku_config_does_not_bump_generation() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let original_generation = presenter.current_generation;

        presenter.set_danmaku_config(DanmakuLayoutConfig::default());

        assert_eq!(presenter.current_generation, original_generation);

        let mut config = DanmakuLayoutConfig::default();
        config.font_size += 1.0;
        presenter.set_danmaku_config(config.clone());
        let changed_generation = presenter.current_generation;

        assert!(changed_generation > original_generation);
        presenter.set_danmaku_config(config);

        assert_eq!(presenter.current_generation, changed_generation);
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn paint_only_danmaku_config_does_not_bump_generation() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let original_generation = presenter.current_generation;
        let mut config = DanmakuLayoutConfig::default();
        config.opacity = 0.45;
        config.shadow_style = crate::danmaku::DanmakuShadowStyle::None;

        presenter.set_danmaku_config(config);

        assert_eq!(presenter.current_generation, original_generation);
        assert_eq!(presenter.danmaku.config().opacity, 0.45);
        assert_eq!(
            presenter.danmaku.config().shadow_style,
            crate::danmaku::DanmakuShadowStyle::None
        );
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn re_enabling_danmaku_requests_the_current_window_immediately() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let viewport = DanmakuViewport::new(1920, 1080);
        presenter.current_danmaku_viewport = Some(viewport);
        presenter.current_media_time = Duration::from_secs(5);
        let original_generation = presenter.current_generation;
        presenter.current_danmaku = Some(DanmakuRenderPlan::empty(
            presenter.current_media_time,
            original_generation,
            viewport,
        ));

        presenter.set_danmaku_enabled(false);
        assert_eq!(
            presenter.current_generation, original_generation,
            "hiding danmaku must not start an asynchronous layout transition"
        );
        assert!(
            presenter.current_danmaku.is_none(),
            "hiding danmaku must clear the visible plan synchronously"
        );

        presenter.set_danmaku_enabled(true);

        assert!(presenter.current_generation > original_generation);
        assert!(presenter.current_danmaku_prepared.is_none());
        let request = presenter
            .danmaku_planner
            .last_requested
            .expect("re-enabling danmaku must enqueue a replacement layout");
        assert_eq!(request.generation, presenter.current_generation);
        assert_eq!(request.viewport, viewport);
        assert_eq!(
            request.media_time,
            quantize_duration(presenter.current_media_time, DANMAKU_PLAN_REQUEST_QUANTUM)
        );
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn disabling_danmaku_rejects_an_in_flight_enabled_plan() {
        let viewport = DanmakuViewport::new(640, 360);
        let timeline = DanmakuTimeline::new(vec![crate::danmaku::DanmakuItem {
            id: 1,
            pts: Duration::from_secs(5),
            text: "stale".to_string(),
            mode: DanmakuMode::Top,
            font_size: 24.0,
            color: crate::danmaku::DanmakuColor::WHITE,
            opacity: 1.0,
            is_self: false,
        }])
        .unwrap();
        let mut stale_engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let stale_prepared = stale_engine.prepare(viewport, 1);
        let stale_rasterizer = stale_engine.rasterizer_clone();

        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter.current_danmaku_viewport = Some(viewport);
        presenter.current_media_time = Duration::from_secs(5);
        let generation = presenter.current_generation;
        presenter.set_danmaku_enabled(false);

        // Simulate a planner result that began before the visibility toggle
        // and reaches the presenter immediately after it.
        presenter.current_danmaku_prepared = Some(CurrentDanmakuPrepared {
            request: AsyncDanmakuPlanRequest {
                key: DanmakuPlanKey {
                    media_time: presenter.current_media_time,
                    viewport,
                    generation,
                },
            },
            prepared: stale_prepared,
            rasterizer: stale_rasterizer,
            window_start: Duration::ZERO,
            window_end: Duration::from_secs(10),
        });
        presenter.current_danmaku = Some(DanmakuRenderPlan::empty(
            presenter.current_media_time,
            generation,
            viewport,
        ));

        presenter.refresh_current_danmaku_plan_from_prepared();
        assert!(
            presenter.current_danmaku.is_none(),
            "an enabled plan completed before hiding must not become visible again"
        );

        presenter.current_danmaku_prepared = None;
        presenter.request_current_danmaku_plan_for_current_time();
        assert!(
            presenter.danmaku_planner.last_requested.is_none(),
            "the hidden state must not enqueue empty replacement layouts"
        );
    }

    #[cfg(any(target_os = "macos", feature = "wgpu"))]
    fn live_danmaku_presenter() -> PresenterRuntime {
        let viewport = DanmakuViewport::with_scale(1280, 720, 2.0);
        let timeline = DanmakuTimeline::new(
            (0..30)
                .map(|i| crate::danmaku::DanmakuItem {
                    id: i + 1,
                    pts: Duration::from_secs(i),
                    text: "实时调整弹幕".into(),
                    mode: DanmakuMode::Scroll,
                    font_size: 25.0,
                    color: crate::danmaku::DanmakuColor::WHITE,
                    opacity: 1.0,
                    is_self: false,
                })
                .collect(),
        )
        .unwrap();
        let mut presenter = PresenterRuntime::new(PresenterConfig {
            danmaku: Some(timeline),
            ..PresenterConfig::default()
        })
        .unwrap();
        presenter.renderer = Box::new(CaptureCompositionProbe {
            captured: Arc::new(Mutex::new(None)),
        });
        presenter.current_media_time = Duration::from_secs(1);
        presenter.current_danmaku_viewport = Some(viewport);
        presenter.current_danmaku_prepared = Some(CurrentDanmakuPrepared {
            request: AsyncDanmakuPlanRequest {
                key: DanmakuPlanKey {
                    media_time: presenter.current_media_time,
                    viewport,
                    generation: presenter.current_generation,
                },
            },
            prepared: presenter
                .danmaku
                .prepare(viewport, presenter.current_generation),
            rasterizer: presenter.danmaku.rasterizer_clone(),
            window_start: Duration::ZERO,
            window_end: Duration::from_secs(10),
        });
        presenter.refresh_current_danmaku_plan_from_prepared();
        presenter
    }

    #[test]
    #[cfg(any(target_os = "macos", feature = "wgpu"))]
    fn live_opacity_with_unchanged_delay_keeps_layout_and_atlas() {
        let mut presenter = live_danmaku_presenter();
        let original = presenter.current_danmaku.as_ref().unwrap().clone();
        let generation = presenter.current_generation;
        for opacity in [0.8, 0.2, 0.5, 0.0, 1.0] {
            let mut config = presenter.danmaku.config().clone();
            config.opacity = opacity;
            presenter.set_danmaku_config(config);
            // The application sends both on every slider update.
            presenter.set_danmaku_global_offset(0);
            assert_eq!(presenter.current_generation, generation);
            assert!(presenter.danmaku_planner.last_requested.is_none());
            let plan = presenter.current_danmaku.as_ref().unwrap();
            if opacity == 0.0 {
                assert!(plan.items.is_empty());
            } else {
                assert_eq!(plan.items.len(), original.items.len());
                assert_eq!(
                    plan.atlas.as_ref().unwrap().version,
                    original.atlas.as_ref().unwrap().version
                );
                for (glyph, before) in plan.items.iter().zip(&original.items) {
                    assert_eq!(glyph.rect, before.rect);
                    assert_eq!(glyph.color_rgba, [1.0, 1.0, 1.0, opacity]);
                }
            }
        }
        presenter.set_danmaku_global_offset(100_000);
        assert!(
            presenter.current_generation > generation,
            "a real delay change must still invalidate the timeline"
        );
    }

    #[test]
    #[cfg(any(target_os = "macos", feature = "wgpu"))]
    fn live_font_size_previews_reuse_atlas_then_replace_while_paused() {
        let mut presenter = live_danmaku_presenter();
        let original = presenter.current_danmaku.as_ref().unwrap().clone();
        let base_size = presenter.danmaku.config().font_size;
        let generation = presenter.current_generation;
        for font_size in [22.0, 38.0, 24.0, 36.0, 40.0] {
            let mut config = presenter.danmaku.config().clone();
            config.font_size = font_size;
            presenter.set_danmaku_config(config);
            presenter.set_danmaku_global_offset(0);
            let plan = presenter.current_danmaku.as_ref().unwrap();
            assert_eq!(presenter.current_generation, generation);
            assert!(
                presenter.danmaku_planner.last_requested.is_none(),
                "pointer events must not each shape a new font size"
            );
            assert_eq!(plan.items.len(), original.items.len());
            assert_eq!(
                plan.atlas.as_ref().unwrap().version,
                original.atlas.as_ref().unwrap().version
            );
            assert_eq!(plan.items[0].tex_rect, original.items[0].tex_rect);
            assert!(
                (plan.items[0].rect[2] / original.items[0].rect[2] - font_size / base_size).abs()
                    < 1e-5
            );
        }
        // Opacity changes during the size preview must not submit that size early.
        let mut config = presenter.danmaku.config().clone();
        config.opacity = 0.5;
        presenter.set_danmaku_config(config);
        assert_eq!(
            presenter
                .danmaku_planner
                .shared
                .0
                .lock()
                .unwrap()
                .config
                .font_size,
            base_size
        );
        let previous_x = presenter.current_danmaku.as_ref().unwrap().items[0].rect[0];
        presenter.current_media_time += Duration::from_millis(100);
        presenter.refresh_current_danmaku_plan_from_prepared();
        assert!(presenter.current_danmaku.as_ref().unwrap().items[0].rect[0] < previous_x);

        // No clock/video advance: the final replacement must finish while paused.
        presenter
            .danmaku_font_size_update
            .as_mut()
            .unwrap()
            .settle_at = Instant::now();
        presenter.request_current_danmaku_plan_for_current_time();
        let deadline = Instant::now() + Duration::from_secs(5);
        while presenter.danmaku_plan_replacement_pending && Instant::now() < deadline {
            presenter.refresh_stale_danmaku_plan();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!presenter.danmaku_plan_replacement_pending);
        assert_eq!(
            presenter
                .current_danmaku_prepared
                .as_ref()
                .unwrap()
                .prepared
                .font_size(),
            40.0
        );
        let plan = presenter.current_danmaku.as_ref().unwrap();
        assert!(!plan.is_empty());
        assert_eq!(plan.items[0].color_rgba[3], 0.5);
        assert_eq!(plan.media_time, presenter.current_media_time);
    }

    #[test]
    fn continuous_font_input_has_a_bounded_prepare_deadline() {
        let start = Instant::now();
        let mut pending = None;
        for millis in (0..500).step_by(16) {
            DanmakuFontSizeUpdate::retarget(&mut pending, start + Duration::from_millis(millis));
        }
        assert!(pending.unwrap().ready(start + Duration::from_millis(500)));
    }

    #[test]
    #[cfg(any(target_os = "macos", feature = "wgpu"))]
    fn font_size_drag_refreshes_time_window_and_seek_flushes_pending_size() {
        let mut presenter = live_danmaku_presenter();
        let mut config = presenter.danmaku.config().clone();
        config.font_size = 40.0;
        presenter.set_danmaku_config(config);
        // A drag near the old window boundary can still request the next
        // time window using its cached font size, without waiting for idle.
        presenter.current_media_time = Duration::from_secs(9);
        presenter.request_current_danmaku_plan_for_current_time();
        let deadline = Instant::now() + Duration::from_secs(5);
        while presenter
            .current_danmaku_prepared
            .as_ref()
            .unwrap()
            .window_end
            <= Duration::from_secs(10)
            && Instant::now() < deadline
        {
            presenter.apply_ready_danmaku_plans();
            thread::sleep(Duration::from_millis(5));
        }
        presenter.current_media_time = Duration::from_secs(11);
        presenter.refresh_current_danmaku_plan_from_prepared();
        assert!(!presenter.current_danmaku.as_ref().unwrap().is_empty());
        presenter.bump_danmaku_generation();
        assert!(presenter.danmaku_font_size_update.is_none());
        assert_eq!(
            presenter
                .danmaku_planner
                .shared
                .0
                .lock()
                .unwrap()
                .config
                .font_size,
            40.0
        );
    }

    #[test]
    #[cfg(any(target_os = "macos", feature = "wgpu"))]
    fn danmaku_resize_retains_motion_and_replaces_layout_while_paused() {
        let viewport = DanmakuViewport::with_scale(1280, 720, 2.0);
        let timeline = DanmakuTimeline::new(vec![crate::danmaku::DanmakuItem {
            id: 1,
            pts: Duration::ZERO,
            text: "持续移动的弹幕".into(),
            mode: DanmakuMode::Scroll,
            font_size: 25.0,
            color: crate::danmaku::DanmakuColor::WHITE,
            opacity: 1.0,
            is_self: false,
        }])
        .unwrap();
        let mut presenter = PresenterRuntime::new(PresenterConfig {
            danmaku: Some(timeline),
            ..PresenterConfig::default()
        })
        .unwrap();
        presenter.renderer = Box::new(CaptureCompositionProbe {
            captured: Arc::new(Mutex::new(None)),
        });
        presenter.current_media_time = Duration::from_secs(1);
        presenter.current_danmaku_viewport = Some(viewport);
        presenter.current_danmaku_prepared = Some(CurrentDanmakuPrepared {
            request: AsyncDanmakuPlanRequest {
                key: DanmakuPlanKey {
                    media_time: presenter.current_media_time,
                    viewport,
                    generation: presenter.current_generation,
                },
            },
            prepared: presenter
                .danmaku
                .prepare(viewport, presenter.current_generation),
            rasterizer: presenter.danmaku.rasterizer_clone(),
            window_start: Duration::ZERO,
            window_end: Duration::from_secs(10),
        });
        presenter.refresh_current_danmaku_plan_from_prepared();
        let original = presenter.current_danmaku.as_ref().unwrap().clone();
        let stale = presenter.current_danmaku_prepared.as_ref().unwrap().clone();
        let mut previous_x = original.items[0].rect[0] / viewport.width as f32;
        for (step, (width, height, scale)) in [(1152, 648, 1.8), (1024, 576, 1.6), (960, 540, 1.5)]
            .into_iter()
            .enumerate()
        {
            presenter.resize_surface(width, height, scale).unwrap();
            let immediate = presenter.current_danmaku.as_ref().unwrap();
            assert!(
                !immediate.is_empty(),
                "resize must never clear the visible plan"
            );
            presenter.current_media_time = Duration::from_millis(1100 + step as u64 * 100);
            presenter.refresh_current_danmaku_plan_from_prepared();
            let plan = presenter.current_danmaku.as_ref().unwrap();
            assert_eq!(
                plan.viewport,
                DanmakuViewport::with_scale(width, height, scale as f32)
            );
            assert!(Arc::ptr_eq(
                plan.atlas.as_ref().unwrap(),
                original.atlas.as_ref().unwrap()
            ));
            let x = plan.items[0].rect[0] / width as f32;
            assert!(
                x < previous_x,
                "retained comments must keep moving during resize"
            );
            previous_x = x;
            assert!(
                presenter.danmaku_planner.last_requested.is_none(),
                "animation must not rasterize every intermediate size"
            );
        }
        let stale_result = AsyncDanmakuPlanResult {
            request: stale.request,
            prepared: stale.prepared,
            rasterizer: stale.rasterizer,
            window_start: stale.window_start,
            window_end: stale.window_end,
            elapsed: Duration::ZERO,
        };
        assert!(!presenter.danmaku_plan_result_is_current(&stale_result));
        // No video frames or clock advancement: a paused resize must still
        // request, accept, and display a replacement at the final dimensions.
        presenter.danmaku_resize_settle_at = Some(Instant::now());
        presenter.request_current_danmaku_plan_for_current_time();
        assert_eq!(
            presenter.danmaku_planner.last_requested.unwrap().viewport,
            presenter.current_danmaku_viewport.unwrap()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while presenter.danmaku_plan_replacement_pending && Instant::now() < deadline {
            presenter.refresh_stale_danmaku_plan();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!presenter.danmaku_plan_replacement_pending);
        let plan = presenter.current_danmaku.as_ref().unwrap();
        assert!(!plan.is_empty());
        assert_eq!(plan.media_time, presenter.current_media_time);
        assert_eq!(
            presenter
                .current_danmaku_prepared
                .as_ref()
                .unwrap()
                .request
                .key
                .viewport,
            plan.viewport
        );
        // A seek/generation change must not bring the retained old comments back.
        presenter.current_generation += 1;
        presenter.refresh_current_danmaku_plan_from_prepared();
        assert!(presenter.current_danmaku.is_none());
    }

    #[test]
    fn danmaku_viewport_ignores_one_pixel_surface_jitter() {
        let current = DanmakuViewport::with_scale(2560, 1440, 2.0);
        let jittered = DanmakuViewport::with_scale(2559, 1441, 2.0);
        let resized = DanmakuViewport::with_scale(2500, 1400, 2.0);
        let other_density = DanmakuViewport::with_scale(1280, 720, 1.0);

        assert!(!danmaku_viewport_requires_relayout(current, jittered));
        assert!(danmaku_viewport_requires_relayout(current, resized));
        assert!(danmaku_viewport_requires_relayout(current, other_density));
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn danmaku_config_change_retains_current_plan_until_replacement_is_ready() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let viewport = DanmakuViewport::new(1920, 1080);
        presenter.current_danmaku_viewport = Some(viewport);
        presenter.current_danmaku = Some(DanmakuRenderPlan::empty(
            Duration::from_secs(5),
            presenter.current_generation,
            viewport,
        ));

        let mut config = DanmakuLayoutConfig::default();
        config.font_size += 1.0;
        presenter.set_danmaku_config(config);

        assert!(presenter.danmaku_plan_replacement_pending);
        assert!(presenter.current_danmaku_prepared.is_none());
        assert_eq!(
            presenter
                .current_danmaku
                .as_ref()
                .map(|plan| plan.generation),
            Some(presenter.current_generation)
        );

        presenter.refresh_current_danmaku_plan_from_prepared();

        assert!(
            presenter.current_danmaku.is_some(),
            "the last complete plan must stay visible while async layout is pending"
        );
    }

    #[test]
    fn config_plan_retention_retags_enabled_plan_and_clears_disabled_plan() {
        let viewport = DanmakuViewport::new(1920, 1080);
        let mut current = Some(DanmakuRenderPlan::empty(
            Duration::from_secs(5),
            7,
            viewport,
        ));
        let mut prepared = None;

        assert!(retain_danmaku_state_for_config_change(
            &mut current,
            &mut prepared,
            true,
            8
        ));
        assert_eq!(current.as_ref().map(|plan| plan.generation), Some(8));

        assert!(!retain_danmaku_state_for_config_change(
            &mut current,
            &mut prepared,
            false,
            9
        ));
        assert!(current.is_none());
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn retained_config_fallback_keeps_scrolling_while_relayout_is_pending() {
        let viewport = DanmakuViewport::new(640, 360);
        let timeline = DanmakuTimeline::new(vec![crate::danmaku::DanmakuItem {
            id: 1,
            pts: Duration::ZERO,
            text: "moving fallback".to_string(),
            mode: DanmakuMode::Scroll,
            font_size: 25.0,
            color: crate::danmaku::DanmakuColor::WHITE,
            opacity: 1.0,
            is_self: false,
        }])
        .unwrap();
        let mut fallback_engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let fallback_prepared = fallback_engine.prepare(viewport, 1);
        let fallback_rasterizer = fallback_engine.rasterizer_clone();

        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter.current_danmaku_viewport = Some(viewport);
        presenter.current_media_time = Duration::from_secs(1);
        presenter.current_danmaku_prepared = Some(CurrentDanmakuPrepared {
            request: AsyncDanmakuPlanRequest {
                key: DanmakuPlanKey {
                    media_time: presenter.current_media_time,
                    viewport,
                    generation: presenter.current_generation,
                },
            },
            prepared: fallback_prepared,
            rasterizer: fallback_rasterizer,
            window_start: Duration::ZERO,
            window_end: Duration::from_secs(10),
        });
        presenter.refresh_current_danmaku_plan_from_prepared();
        let first_x = presenter.current_danmaku.as_ref().unwrap().items[0].rect[0];

        let mut config = DanmakuLayoutConfig::default();
        // Font-size-only updates now preview the retained layout without a
        // generation change. Exercise a full relayout for this fallback test.
        config.scroll_duration_seconds += 4.0;
        presenter.set_danmaku_config(config);
        assert!(presenter.danmaku_plan_replacement_pending);
        assert!(presenter.current_danmaku_prepared.is_some());

        presenter.current_media_time = Duration::from_millis(1200);
        presenter.refresh_current_danmaku_plan_from_prepared();
        let next = presenter.current_danmaku.as_ref().unwrap();

        assert_eq!(next.media_time, Duration::from_millis(1200));
        assert!(
            next.items[0].rect[0] < first_x,
            "the retained right-to-left comment must keep moving"
        );
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn audio_clock_report_tracks_queue_and_underflow_changes() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();

        let first = AudioClockSnapshot {
            media_time: Some(Duration::from_secs(1)),
            queued_duration: Some(Duration::from_millis(500)),
            queued_frames: 24_000,
            read_frames: 0,
            written_frames: 24_000,
            underflow_frames: 0,
        };
        assert!(presenter.should_report_audio_clock(first));
        assert!(!presenter.should_report_audio_clock(first));
        assert!(presenter.should_report_audio_clock(AudioClockSnapshot {
            media_time: Some(Duration::from_secs(1)),
            queued_duration: Some(Duration::from_millis(300)),
            queued_frames: 14_400,
            read_frames: 0,
            written_frames: 19_200,
            underflow_frames: 0,
        }));
        assert!(presenter.should_report_audio_clock(AudioClockSnapshot {
            media_time: Some(Duration::from_millis(100)),
            queued_duration: Some(Duration::ZERO),
            queued_frames: 0,
            read_frames: 0,
            written_frames: 24_000,
            underflow_frames: 512,
        }));

        presenter.playback_rate = 2.0;
        assert!(presenter.should_report_audio_clock(AudioClockSnapshot {
            media_time: Some(Duration::from_secs(1)),
            queued_duration: Some(Duration::from_millis(300)),
            queued_frames: 14_400,
            read_frames: 14_400,
            written_frames: 28_800,
            underflow_frames: 0,
        }));
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn audio_start_is_blocked_while_player_is_not_playing() {
        use crate::audio::{AudioOutputState, BufferedAudioOutput};
        use crate::ffmpeg::{PcmAudioFrame, PcmFormat};

        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let format = PcmFormat::f32_interleaved(48_000, 2);
        let mut output = BufferedAudioOutput::new(AudioRingBufferConfig::default());
        output.configure(format).unwrap();
        output
            .push(PcmAudioFrame {
                format,
                pts: Some(Duration::ZERO),
                frames: 12_000,
                samples: vec![0.0; 24_000],
            })
            .unwrap();
        presenter.audio_output = Box::new(output);

        presenter.ensure_audio_started();

        assert!(!presenter.audio_started);
        assert_eq!(presenter.audio_output.state(), AudioOutputState::Stopped);
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn presenter_volume_is_clamped() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();

        assert_eq!(presenter.volume(), 1.0);
        presenter.set_volume(0.4);
        assert!((presenter.volume() - 0.4).abs() < 0.000_001);
        presenter.set_volume(-1.0);
        assert_eq!(presenter.volume(), 0.0);
        presenter.set_volume(f64::NAN);
        assert_eq!(presenter.volume(), 1.0);
    }

    #[test]
    fn surface_dimensions_are_converted_to_full_output_danmaku_viewport() {
        let viewport = surface_metrics_to_viewport(SurfaceMetrics::new(1600, 900, 2.0));

        assert_eq!(viewport, DanmakuViewport::with_scale(1600, 900, 2.0));
    }

    #[test]
    fn physical_surface_extent_keeps_pixels_and_applies_content_scale() {
        let viewport = surface_metrics_to_viewport(SurfaceMetrics::new(1081, 607, 2.625));

        assert_eq!(viewport, DanmakuViewport::with_scale(1081, 607, 2.625));
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn resized_capture_rebuilds_subtitle_overlay_for_capture_viewport() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter.subtitles.push(subtitle_frame(
            Duration::from_secs(1),
            Some(Duration::from_secs(3)),
        ));
        presenter.danmaku = danmaku_engine("capture viewport");
        presenter.current_media_time = Duration::from_millis(1500);
        presenter.current_generation = 9;

        let overlay = presenter.capture_overlay(320, 180);

        assert_eq!(overlay.viewport, OverlayViewport::new(320, 180));
        assert_eq!(overlay.subtitle_planes.len(), 1);
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn screenshot_capture_context_omits_danmaku() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let captured = Arc::new(Mutex::new(None));
        presenter.renderer = Box::new(CaptureCompositionProbe {
            captured: Arc::clone(&captured),
        });
        presenter.danmaku = danmaku_engine("screen-only danmaku");
        presenter.current_media_time = Duration::from_millis(1500);
        presenter.current_generation = 9;

        let screenshot = presenter.capture_frame_rgba(320, 180).unwrap();

        assert_eq!(screenshot.as_ref().map(Vec::len), Some(320 * 180 * 4));
        assert_eq!(
            *captured.lock().expect("capture probe mutex poisoned"),
            Some(CapturedComposition {
                has_overlay: true,
                has_danmaku: false,
                width: 320,
                height: 180,
            })
        );
    }

    #[test]
    fn stale_danmaku_plan_refreshes_without_new_video_frame() {
        let mut engine = danmaku_engine("first track");
        let mut current_plan = Some(engine.render_plan(
            Duration::from_millis(1500),
            DanmakuViewport::new(640, 360),
            1,
        ));
        let first_item_id = current_plan.as_ref().unwrap().items[0].item_id;

        engine.set_timeline(
            DanmakuTimeline::new(vec![danmaku_item(2, 1.0, "switched track")]).unwrap(),
        );
        refresh_danmaku_plan(
            &mut current_plan,
            Some(DanmakuViewport::new(640, 360)),
            &mut engine,
            Duration::from_millis(1500),
            2,
        );

        let refreshed = current_plan.unwrap();
        assert_eq!(first_item_id, 1);
        assert_eq!(refreshed.generation, 2);
        assert_eq!(refreshed.media_time, Duration::from_millis(1500));
        assert_eq!(refreshed.items[0].item_id, 2);
    }

    #[test]
    #[cfg(feature = "wgpu")]
    fn presenter_danmaku_session_merges_tracks_and_applies_track_controls() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let first = DanmakuTimeline::new(vec![danmaku_item(1, 1.0, "first")]).unwrap();
        let second = DanmakuTimeline::new(vec![danmaku_item(2, 2.0, "second")]).unwrap();

        let first_id = presenter.add_danmaku_track(first, "first", DanmakuTrackSource::Json, 0);
        let second_id =
            presenter.add_danmaku_track(second, "second", DanmakuTrackSource::Json, -1_000_000);

        assert_eq!(presenter.danmaku_tracks().len(), 2);
        let layout = presenter.danmaku.frame_layout(
            Duration::from_millis(1500),
            DanmakuViewport::new(640, 360),
            1,
        );
        assert!(
            layout
                .items
                .iter()
                .any(|item| item.text.as_ref() == "first")
        );
        assert!(
            layout
                .items
                .iter()
                .any(|item| item.text.as_ref() == "second")
        );
        assert_eq!(
            layout
                .items
                .iter()
                .map(|item| item.item_id)
                .collect::<HashSet<_>>()
                .len(),
            2
        );

        assert!(presenter.set_danmaku_track_enabled(first_id, false));
        let layout = presenter.danmaku.frame_layout(
            Duration::from_millis(1500),
            DanmakuViewport::new(640, 360),
            2,
        );
        assert_eq!(layout.items.len(), 1);
        assert_eq!(layout.items[0].text.as_ref(), "second");

        assert!(presenter.remove_danmaku_track(second_id));
        assert_eq!(presenter.danmaku_tracks().len(), 1);
        assert!(presenter.remove_danmaku_track(first_id));
        assert!(presenter.danmaku_tracks().is_empty());
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn diagnostics_snapshot_reports_every_section_with_nulls_for_unknowns() {
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        let snapshot = presenter.diagnostics_snapshot();
        assert_eq!(snapshot["schema"], crate::diagnostics::DIAGNOSTICS_SCHEMA);
        assert!(snapshot["capturedAtUnixMs"].as_u64().unwrap() > 0);
        for section in [
            "playback",
            "media",
            "decoder",
            "video",
            "presenter",
            "output",
            "hdr",
            "resources",
            "upscaler",
            "audio",
            "buffers",
            "timing",
        ] {
            assert!(
                snapshot.get(section).is_some(),
                "missing {section}: {snapshot}"
            );
        }
        // Nothing is open: unknown values are null rather than zero.
        assert_eq!(snapshot["playback"]["state"], "idle");
        assert!(snapshot["playback"]["durationSeconds"].is_null());
        assert_eq!(snapshot["playback"]["buffering"]["active"], false);
        assert!(snapshot["media"]["video"].is_null());
        assert!(snapshot["decoder"].is_null());
        assert!(snapshot["buffers"]["playback"].is_null());
        assert!(snapshot["timing"]["scheduler"]["dropToleranceMs"].is_null());
        assert!(snapshot["timing"]["clock"]["lastDriftMs"].is_null());
        assert_eq!(snapshot["timing"]["scheduler"]["presentedFrames"], 0);
        assert_eq!(snapshot["timing"]["presentation"]["newFramesPresented"], 0);
        assert!(snapshot["timing"]["presentation"]["lastPresentAgoMs"].is_null());
        assert_eq!(snapshot["output"]["requestedMode"], "sdr");
        assert!(snapshot["output"]["fallbackReason"].is_string());
        assert_eq!(snapshot["upscaler"]["requestedMode"], "off");
        assert_eq!(snapshot["audio"]["recovery"]["state"], "stable");
        assert!(snapshot["buffers"]["presenterVideoFrames"]["queued"].is_u64());

        // The latest decoder transition becomes queryable instead of being
        // available only to event listeners that were attached in time.
        presenter.latest_video_decoder = Some(VideoDecoderEvent {
            stage: "open_avcodec_to_software".to_string(),
            requested_backend: DecoderBackend::VideoToolbox,
            previous_backend: Some(DecoderBackend::VideoToolbox),
            active_backend: DecoderBackend::Software,
            fallback_count: 1,
            codec: Some("hevc".to_string()),
            pixel_format: Some("yuv420p10le".to_string()),
            line_sizes: None,
            reason: Some("unsupported profile".to_string()),
        });
        let snapshot = presenter.diagnostics_snapshot();
        assert_eq!(snapshot["decoder"]["activeBackend"], "software");
        assert_eq!(snapshot["decoder"]["requestedBackend"], "videotoolbox");
        assert_eq!(snapshot["decoder"]["hardware"], false);
        assert_eq!(snapshot["decoder"]["fellBack"], true);
        assert_eq!(snapshot["decoder"]["reason"], "unsupported profile");
    }
}
