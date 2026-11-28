use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;
use std::sync::OnceLock;

pub(crate) mod mediacodec;

/// An owned reference to an Android `ANativeWindow`.
///
/// Flutter/Java owns the original window. The renderer keeps its own reference so
/// a surface callback can hand control back to the UI thread without invalidating
/// a live Vulkan surface.
pub(crate) struct AndroidNativeWindow {
    raw: NonNull<c_void>,
}

impl AndroidNativeWindow {
    /// Acquires one native reference to `raw`.
    ///
    /// The caller must ensure `raw` points to a live `ANativeWindow` when this
    /// function is called.
    pub(crate) unsafe fn acquire(raw: NonNull<c_void>) -> Self {
        unsafe { ANativeWindow_acquire(raw.as_ptr()) };
        Self { raw }
    }

    pub(crate) fn as_non_null(&self) -> NonNull<c_void> {
        self.raw
    }

    /// Verifies the dataspace installed by Android Vulkan WSI for an FP16
    /// extended-linear swapchain. API 28 symbols are resolved dynamically so
    /// liberika still loads on its API 26 minimum; callers must fall back to SDR
    /// when this verification is unavailable or fails.
    pub(crate) fn ensure_scrgb_linear_data_space(
        &self,
    ) -> Result<AndroidDataSpaceVerification, AndroidDataSpaceError> {
        let api = native_window_data_space_api().map_err(AndroidDataSpaceError::api_unavailable)?;
        let before = unsafe { (api.get)(self.raw.as_ptr()) };
        if before == ADATASPACE_SCRGB_LINEAR {
            return Ok(AndroidDataSpaceVerification {
                before,
                after: before,
                corrected: false,
            });
        }

        let result = unsafe { (api.set)(self.raw.as_ptr(), ADATASPACE_SCRGB_LINEAR) };
        if result != 0 {
            return Err(AndroidDataSpaceError::verification_failed(format!(
                "ANativeWindow_setBuffersDataSpace(SCRGB_LINEAR) failed with {result}; observed_before=0x{before:08x}"
            )));
        }
        let after = unsafe { (api.get)(self.raw.as_ptr()) };
        if after != ADATASPACE_SCRGB_LINEAR {
            return Err(AndroidDataSpaceError::verification_failed(format!(
                "ANativeWindow SCRGB_LINEAR readback mismatch: expected=0x{ADATASPACE_SCRGB_LINEAR:08x} observed_before=0x{before:08x} observed_after=0x{after:08x}"
            )));
        }
        Ok(AndroidDataSpaceVerification {
            before,
            after,
            corrected: true,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AndroidDataSpaceErrorKind {
    ApiUnavailable,
    VerificationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AndroidDataSpaceError {
    pub(crate) kind: AndroidDataSpaceErrorKind,
    message: String,
}

impl AndroidDataSpaceError {
    fn api_unavailable(message: String) -> Self {
        Self {
            kind: AndroidDataSpaceErrorKind::ApiUnavailable,
            message,
        }
    }

    fn verification_failed(message: String) -> Self {
        Self {
            kind: AndroidDataSpaceErrorKind::VerificationFailed,
            message,
        }
    }
}

impl fmt::Display for AndroidDataSpaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AndroidDataSpaceError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AndroidDataSpaceVerification {
    pub(crate) before: i32,
    pub(crate) after: i32,
    pub(crate) corrected: bool,
}

pub(crate) const ADATASPACE_SCRGB_LINEAR: i32 = 0x1841_0000;

type SetBuffersDataSpace = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type GetBuffersDataSpace = unsafe extern "C" fn(*mut c_void) -> i32;

struct NativeWindowDataSpaceApi {
    _library: *mut c_void,
    set: SetBuffersDataSpace,
    get: GetBuffersDataSpace,
}

unsafe impl Send for NativeWindowDataSpaceApi {}
unsafe impl Sync for NativeWindowDataSpaceApi {}

static NATIVE_WINDOW_DATA_SPACE_API: OnceLock<Result<NativeWindowDataSpaceApi, String>> =
    OnceLock::new();

fn native_window_data_space_api() -> Result<&'static NativeWindowDataSpaceApi, String> {
    match NATIVE_WINDOW_DATA_SPACE_API.get_or_init(load_native_window_data_space_api) {
        Ok(api) => Ok(api),
        Err(error) => Err(error.clone()),
    }
}

fn load_native_window_data_space_api() -> Result<NativeWindowDataSpaceApi, String> {
    let library =
        unsafe { libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if library.is_null() {
        return Err(
            "native_window_dataspace_api_unavailable: dlopen(libandroid.so) failed".to_string(),
        );
    }

    let set = unsafe { libc::dlsym(library, c"ANativeWindow_setBuffersDataSpace".as_ptr()) };
    let get = unsafe { libc::dlsym(library, c"ANativeWindow_getBuffersDataSpace".as_ptr()) };
    if set.is_null() || get.is_null() {
        // Keep the handle open for process lifetime. Android's loader owns the
        // actual library mapping and another thread may have resolved one symbol.
        return Err(
            "native_window_dataspace_api_unavailable: API 28 ANativeWindow dataspace symbols are missing"
                .to_string(),
        );
    }

    Ok(NativeWindowDataSpaceApi {
        _library: library,
        set: unsafe { std::mem::transmute::<*mut c_void, SetBuffersDataSpace>(set) },
        get: unsafe { std::mem::transmute::<*mut c_void, GetBuffersDataSpace>(get) },
    })
}

impl Clone for AndroidNativeWindow {
    fn clone(&self) -> Self {
        // SAFETY: `self` owns a live reference for the duration of this call.
        unsafe { Self::acquire(self.raw) }
    }
}

impl Drop for AndroidNativeWindow {
    fn drop(&mut self) {
        unsafe { ANativeWindow_release(self.raw.as_ptr()) };
    }
}

#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_acquire(window: *mut c_void);
    fn ANativeWindow_release(window: *mut c_void);
}

pub mod aaudio {
    use std::collections::VecDeque;
    use std::ffi::{CStr, c_char, c_void};
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use thiserror::Error;

    use crate::audio::spsc::{PcmSpscPushResult, PcmSpscRing};
    use crate::audio::{
        AudioClockSnapshot, AudioOutputBackend, AudioOutputRuntimeStats, AudioOutputState,
        AudioPushResult, AudioRecoveryState, AudioRingBuffer, AudioRingBufferConfig,
        AudioRingBufferStats, audio_output_queue_has_capacity, normalize_volume,
    };
    use crate::ffmpeg::{PcmAudioFrame, PcmFormat, PcmSampleFormat};
    use crate::trace;

    type AAudioResult = i32;
    type AAudioFormat = i32;
    type AAudioDataCallbackResult = i32;

    const AAUDIO_OK: AAudioResult = 0;
    const AAUDIO_ERROR_DISCONNECTED: AAudioResult = -899;
    const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
    const AAUDIO_FORMAT_PCM_FLOAT: AAudioFormat = 2;
    const AAUDIO_SHARING_MODE_SHARED: i32 = 1;
    const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
    const AAUDIO_CALLBACK_RESULT_CONTINUE: AAudioDataCallbackResult = 0;
    const AAUDIO_CALLBACK_RESULT_STOP: AAudioDataCallbackResult = 1;

    const STATE_STOPPED: u8 = 0;
    const STATE_PLAYING: u8 = 1;
    const STATE_PAUSED: u8 = 2;

    const RECOVERY_STABLE: u8 = 0;
    const RECOVERY_DISCONNECTED: u8 = 1;
    const RECOVERY_RECOVERING: u8 = 2;
    const RECOVERY_FAILED: u8 = 3;
    const RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(250);

    #[repr(C)]
    struct AAudioStreamBuilder {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct AAudioStream {
        _private: [u8; 0],
    }

    type AAudioStreamDataCallback = unsafe extern "C" fn(
        stream: *mut AAudioStream,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        num_frames: i32,
    ) -> AAudioDataCallbackResult;

    type AAudioStreamErrorCallback = unsafe extern "C" fn(
        stream: *mut AAudioStream,
        user_data: *mut c_void,
        error: AAudioResult,
    );

    #[link(name = "aaudio")]
    unsafe extern "C" {
        fn AAudio_createStreamBuilder(builder: *mut *mut AAudioStreamBuilder) -> AAudioResult;
        fn AAudioStreamBuilder_delete(builder: *mut AAudioStreamBuilder) -> AAudioResult;
        fn AAudioStreamBuilder_setDirection(builder: *mut AAudioStreamBuilder, direction: i32);
        fn AAudioStreamBuilder_setFormat(builder: *mut AAudioStreamBuilder, format: AAudioFormat);
        fn AAudioStreamBuilder_setSampleRate(builder: *mut AAudioStreamBuilder, sample_rate: i32);
        fn AAudioStreamBuilder_setChannelCount(builder: *mut AAudioStreamBuilder, channels: i32);
        fn AAudioStreamBuilder_setSharingMode(builder: *mut AAudioStreamBuilder, sharing_mode: i32);
        fn AAudioStreamBuilder_setPerformanceMode(
            builder: *mut AAudioStreamBuilder,
            performance_mode: i32,
        );
        fn AAudioStreamBuilder_setDataCallback(
            builder: *mut AAudioStreamBuilder,
            callback: Option<AAudioStreamDataCallback>,
            user_data: *mut c_void,
        );
        fn AAudioStreamBuilder_setErrorCallback(
            builder: *mut AAudioStreamBuilder,
            callback: Option<AAudioStreamErrorCallback>,
            user_data: *mut c_void,
        );
        fn AAudioStreamBuilder_openStream(
            builder: *mut AAudioStreamBuilder,
            stream: *mut *mut AAudioStream,
        ) -> AAudioResult;
        fn AAudioStream_requestStart(stream: *mut AAudioStream) -> AAudioResult;
        fn AAudioStream_requestPause(stream: *mut AAudioStream) -> AAudioResult;
        fn AAudioStream_requestStop(stream: *mut AAudioStream) -> AAudioResult;
        fn AAudioStream_close(stream: *mut AAudioStream) -> AAudioResult;
        fn AAudioStream_getFormat(stream: *mut AAudioStream) -> AAudioFormat;
        fn AAudioStream_getSampleRate(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_getChannelCount(stream: *mut AAudioStream) -> i32;
        fn AAudio_convertResultToText(result: AAudioResult) -> *const c_char;
    }

    #[derive(Debug, Error)]
    pub enum AAudioOutputError {
        #[error("audio error: {0}")]
        Audio(#[from] crate::audio::AudioError),
        #[error("AAudio output is not configured")]
        NotConfigured,
        #[error("AAudio output lock was poisoned")]
        LockPoisoned,
        #[error("unsupported AAudio PCM format: {sample_rate} Hz, {channels} channels")]
        InvalidFormat { sample_rate: u32, channels: u32 },
        #[error("invalid AAudio ring capacity: {capacity_frames} frames x {channels} channels")]
        InvalidRingCapacity {
            capacity_frames: usize,
            channels: usize,
        },
        #[error("AAudio {operation} failed with {result} ({message})")]
        AAudio {
            operation: &'static str,
            result: AAudioResult,
            message: String,
        },
        #[error(
            "AAudio opened {actual_sample_rate} Hz/{actual_channels} ch instead of {requested_sample_rate} Hz/{requested_channels} ch"
        )]
        FormatNegotiation {
            requested_sample_rate: u32,
            requested_channels: u32,
            actual_sample_rate: i32,
            actual_channels: i32,
        },
    }

    pub type Result<T> = std::result::Result<T, AAudioOutputError>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AAudioOutputConfig {
        pub ring_buffer: AudioRingBufferConfig,
    }

    impl Default for AAudioOutputConfig {
        fn default() -> Self {
            Self {
                ring_buffer: AudioRingBufferConfig {
                    capacity_frames: 192_000,
                    drop_oldest_on_overflow: true,
                },
            }
        }
    }

    struct OutputSignals {
        volume: AtomicU32,
        recovery_state: AtomicU8,
        last_error_code: AtomicI32,
        recovery_attempts: AtomicU64,
        recovery_count: AtomicU64,
        recovery_failures: AtomicU64,
        transition_sequence: AtomicU64,
    }

    impl OutputSignals {
        fn new() -> Self {
            Self {
                volume: AtomicU32::new(1.0f32.to_bits()),
                recovery_state: AtomicU8::new(RECOVERY_STABLE),
                last_error_code: AtomicI32::new(AAUDIO_OK),
                recovery_attempts: AtomicU64::new(0),
                recovery_count: AtomicU64::new(0),
                recovery_failures: AtomicU64::new(0),
                transition_sequence: AtomicU64::new(0),
            }
        }

        fn volume(&self) -> f32 {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        }

        fn set_disconnected_from_callback(&self, error: AAudioResult) {
            self.last_error_code.store(error, Ordering::Relaxed);
            self.recovery_state
                .store(RECOVERY_DISCONNECTED, Ordering::Release);
            self.transition_sequence.fetch_add(1, Ordering::Release);
        }

        fn begin_recovery(&self) -> AudioOutputRuntimeStats {
            self.recovery_attempts.fetch_add(1, Ordering::Relaxed);
            self.recovery_state
                .store(RECOVERY_RECOVERING, Ordering::Release);
            self.transition_sequence.fetch_add(1, Ordering::Release);
            self.snapshot()
        }

        fn recovery_succeeded(&self) -> Option<AudioOutputRuntimeStats> {
            if self
                .recovery_state
                .compare_exchange(
                    RECOVERY_RECOVERING,
                    RECOVERY_STABLE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return None;
            }
            self.recovery_count.fetch_add(1, Ordering::Relaxed);
            self.transition_sequence.fetch_add(1, Ordering::Release);
            Some(self.snapshot())
        }

        fn recovery_failed(&self, error_code: AAudioResult) -> AudioOutputRuntimeStats {
            self.last_error_code.store(error_code, Ordering::Relaxed);
            self.recovery_failures.fetch_add(1, Ordering::Relaxed);
            self.recovery_state
                .store(RECOVERY_FAILED, Ordering::Release);
            self.transition_sequence.fetch_add(1, Ordering::Release);
            self.snapshot()
        }

        fn mark_disconnected(&self, error_code: AAudioResult) -> AudioOutputRuntimeStats {
            self.last_error_code.store(error_code, Ordering::Relaxed);
            self.recovery_state
                .store(RECOVERY_DISCONNECTED, Ordering::Release);
            self.transition_sequence.fetch_add(1, Ordering::Release);
            self.snapshot()
        }

        fn reset_current_error(&self) {
            self.last_error_code.store(AAUDIO_OK, Ordering::Relaxed);
            self.recovery_state
                .store(RECOVERY_STABLE, Ordering::Release);
            self.transition_sequence.fetch_add(1, Ordering::Release);
        }

        fn snapshot(&self) -> AudioOutputRuntimeStats {
            loop {
                let sequence_before = self.transition_sequence.load(Ordering::Acquire);
                let snapshot = AudioOutputRuntimeStats {
                    recovery_state: decode_recovery_state(
                        self.recovery_state.load(Ordering::Acquire),
                    ),
                    last_error_code: self.last_error_code.load(Ordering::Relaxed),
                    recovery_attempts: self.recovery_attempts.load(Ordering::Relaxed),
                    recovery_count: self.recovery_count.load(Ordering::Relaxed),
                    recovery_failures: self.recovery_failures.load(Ordering::Relaxed),
                    transition_sequence: sequence_before,
                };
                if sequence_before == self.transition_sequence.load(Ordering::Acquire) {
                    return snapshot;
                }
            }
        }
    }

    struct CallbackState {
        ring: Arc<PcmSpscRing>,
        signals: Arc<OutputSignals>,
    }

    struct StreamBuilder(NonNull<AAudioStreamBuilder>);

    impl Drop for StreamBuilder {
        fn drop(&mut self) {
            unsafe {
                let _ = AAudioStreamBuilder_delete(self.0.as_ptr());
            }
        }
    }

    struct StreamHandle {
        raw: NonNull<AAudioStream>,
        _callback: Arc<CallbackState>,
    }

    // AAudio permits stream control from application threads other than its
    // realtime callback. The pointer remains owned until `AAudioStream_close`.
    unsafe impl Send for StreamHandle {}

    impl StreamHandle {
        fn request_start(&self) -> Result<()> {
            check_result(
                unsafe { AAudioStream_requestStart(self.raw.as_ptr()) },
                "requestStart",
            )
        }

        fn request_pause(&self) -> Result<()> {
            check_result(
                unsafe { AAudioStream_requestPause(self.raw.as_ptr()) },
                "requestPause",
            )
        }

        fn request_stop(&self) -> Result<()> {
            check_result(
                unsafe { AAudioStream_requestStop(self.raw.as_ptr()) },
                "requestStop",
            )
        }
    }

    impl Drop for StreamHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = AAudioStream_close(self.raw.as_ptr());
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct OutputTimelineSegment {
        start_position: u64,
        end_position: u64,
        media_time: Option<Duration>,
        media_frames_per_output_frame: f64,
    }

    struct OutputControl {
        stream: Option<StreamHandle>,
        callback: Option<Arc<CallbackState>>,
        format: Option<PcmFormat>,
        processor: AudioRingBuffer,
        timeline: VecDeque<OutputTimelineSegment>,
        last_media_time: Option<Duration>,
        playback_rate: f64,
        next_recovery_at: Option<Instant>,
    }

    impl OutputControl {
        fn new(config: AudioRingBufferConfig) -> Self {
            Self {
                stream: None,
                callback: None,
                format: None,
                processor: AudioRingBuffer::new(config),
                timeline: VecDeque::new(),
                last_media_time: None,
                playback_rate: 1.0,
                next_recovery_at: None,
            }
        }

        fn clear_queue(&mut self) {
            if let Some(callback) = &self.callback {
                callback.ring.clear();
            }
            self.processor.clear();
            self.timeline.clear();
            self.last_media_time = None;
        }

        fn append_timeline(&mut self, pushed: PcmSpscPushResult, segment_start: Option<Duration>) {
            self.prune_timeline();
            if pushed.accepted_frames == 0 {
                return;
            }
            let format = self.format.expect("configured AAudio format");
            let adjusted_start = segment_start.and_then(|start| {
                offset_pts_scaled(
                    start,
                    pushed.input_offset_frames,
                    format.sample_rate,
                    self.playback_rate,
                )
            });
            self.timeline.push_back(OutputTimelineSegment {
                start_position: pushed.start_position,
                end_position: pushed
                    .start_position
                    .saturating_add(pushed.accepted_frames as u64),
                media_time: adjusted_start,
                media_frames_per_output_frame: self.playback_rate,
            });
        }

        fn prune_timeline(&mut self) {
            let Some(callback) = &self.callback else {
                self.timeline.clear();
                return;
            };
            let read_position = callback.ring.read_position();
            while self
                .timeline
                .front()
                .is_some_and(|segment| segment.end_position <= read_position)
            {
                let segment = self.timeline.pop_front().expect("timeline front exists");
                self.last_media_time = segment.media_time.and_then(|start| {
                    offset_pts_scaled(
                        start,
                        segment.end_position.saturating_sub(segment.start_position) as usize,
                        self.format.map_or(0, |format| format.sample_rate),
                        segment.media_frames_per_output_frame,
                    )
                });
            }
        }

        fn clock_snapshot(&mut self) -> AudioClockSnapshot {
            self.prune_timeline();
            let stats = self
                .callback
                .as_ref()
                .map_or_else(AudioRingBufferStats::default, |callback| {
                    callback.ring.stats()
                });
            let read_position = self
                .callback
                .as_ref()
                .map_or(0, |callback| callback.ring.read_position());
            let media_time = self
                .timeline
                .front()
                .and_then(|segment| {
                    if read_position < segment.start_position
                        || read_position >= segment.end_position
                    {
                        return segment.media_time;
                    }
                    segment.media_time.and_then(|start| {
                        offset_pts_scaled(
                            start,
                            read_position.saturating_sub(segment.start_position) as usize,
                            self.format.map_or(0, |format| format.sample_rate),
                            segment.media_frames_per_output_frame,
                        )
                    })
                })
                .or(self.last_media_time);
            let queued_duration = self.format.and_then(|format| {
                (format.sample_rate > 0).then(|| {
                    Duration::from_secs_f64(stats.queued_frames as f64 / format.sample_rate as f64)
                })
            });
            AudioClockSnapshot {
                media_time,
                queued_duration,
                queued_frames: stats.queued_frames,
                read_frames: stats.read_frames,
                written_frames: stats.written_frames,
                underflow_frames: stats.underflow_frames,
            }
        }
    }

    pub struct AAudioOutput {
        config: AAudioOutputConfig,
        control: Mutex<OutputControl>,
        signals: Arc<OutputSignals>,
        state: AtomicU8,
    }

    impl AAudioOutput {
        pub fn new(config: AAudioOutputConfig) -> Self {
            Self {
                config,
                control: Mutex::new(OutputControl::new(config.ring_buffer)),
                signals: Arc::new(OutputSignals::new()),
                state: AtomicU8::new(STATE_STOPPED),
            }
        }

        pub fn configure(&self, format: PcmFormat) -> Result<()> {
            validate_format(format)?;
            let ring = Arc::new(
                PcmSpscRing::new(
                    self.config.ring_buffer.capacity_frames,
                    format.channels as usize,
                )
                .ok_or(AAudioOutputError::InvalidRingCapacity {
                    capacity_frames: self.config.ring_buffer.capacity_frames,
                    channels: format.channels as usize,
                })?,
            );
            // Seed both ramp endpoints: the stream has not started, so there
            // is no previous gain to ramp from.
            ring.snap_volume(self.signals.volume());
            let callback = Arc::new(CallbackState {
                ring,
                signals: Arc::clone(&self.signals),
            });

            self.state.store(STATE_STOPPED, Ordering::Release);
            let mut control = lock(&self.control)?;
            self.close_stream_locked(&mut control, false)?;
            control.processor.configure(format)?;
            control.timeline.clear();
            control.last_media_time = None;
            control.next_recovery_at = None;
            control.format = Some(format);
            control.callback = Some(Arc::clone(&callback));
            self.signals.reset_current_error();
            match self.open_stream(format, callback) {
                Ok(stream) => {
                    control.stream = Some(stream);
                    Ok(())
                }
                Err(error) => {
                    let stats = self
                        .signals
                        .recovery_failed(error_result_code(&error).unwrap_or(AAUDIO_OK));
                    trace_recovery("configure_failed", stats, Some(&error.to_string()));
                    Err(error)
                }
            }
        }

        pub fn start(&self) -> Result<()> {
            let previous_state = self.state.swap(STATE_PLAYING, Ordering::AcqRel);
            let mut control = lock(&self.control)?;
            let result = self.start_or_recover_stream_locked(&mut control);
            if result.is_err() {
                self.state.store(previous_state, Ordering::Release);
            }
            result
        }

        pub fn pause(&self) -> Result<()> {
            let previous_state = self.state.swap(STATE_PAUSED, Ordering::AcqRel);
            if previous_state != STATE_PLAYING {
                return Ok(());
            }
            let control = lock(&self.control)?;
            if let Some(stream) = control.stream.as_ref()
                && let Err(error) = stream.request_pause()
            {
                if let Some(error_code) = error_result_code(&error) {
                    let stats = self.signals.mark_disconnected(error_code);
                    trace_recovery("pause_disconnected", stats, Some(&error.to_string()));
                }
                if is_disconnected_error(&error) {
                    return Ok(());
                }
                return Err(error);
            }
            Ok(())
        }

        pub fn stop(&self) -> Result<()> {
            let previous_state = self.state.swap(STATE_STOPPED, Ordering::AcqRel);
            let mut control = lock(&self.control)?;
            let close_result = self.close_stream_locked(
                &mut control,
                // Closing a paused AAudio stream already performs the required
                // teardown. Some implementations reject requestStop while the
                // stream is paused, which would otherwise turn a successful
                // reset into a spurious presenter audio failure.
                previous_state == STATE_PLAYING,
            );
            control.clear_queue();
            control.callback = None;
            control.format = None;
            control.next_recovery_at = None;
            self.signals.reset_current_error();
            close_result
        }

        pub fn set_volume(&self, volume: f32) {
            let volume = normalize_volume(volume);
            self.signals
                .volume
                .store(volume.to_bits(), Ordering::Relaxed);
            // The ring only records the new target; the realtime callback ramps
            // toward it so queued samples are never rewritten under the reader.
            if let Ok(control) = self.control.lock()
                && let Some(callback) = &control.callback
            {
                callback.ring.set_volume(volume);
            }
        }

        pub fn volume(&self) -> f32 {
            self.signals.volume()
        }

        pub fn set_playback_rate(&self, rate: f64) {
            if let Ok(mut control) = self.control.lock() {
                control.playback_rate = normalize_playback_rate(rate);
                let playback_rate = control.playback_rate;
                control.processor.set_playback_rate(playback_rate);
            }
        }

        pub fn can_accept_audio_frame(&self) -> bool {
            let Ok(control) = self.control.lock() else {
                return true;
            };
            let (Some(callback), Some(format)) = (&control.callback, control.format) else {
                return true;
            };
            audio_output_queue_has_capacity(callback.ring.stats().queued_frames, format.sample_rate)
        }

        pub fn push(&self, frame: PcmAudioFrame) -> Result<AudioPushResult> {
            let mut control = lock(&self.control)?;
            self.recover_disconnected_stream_locked(&mut control)?;
            let callback = Arc::clone(
                control
                    .callback
                    .as_ref()
                    .ok_or(AAudioOutputError::NotConfigured)?,
            );
            control.processor.push_frame(frame)?;
            let prepared = control.processor.clock_snapshot();
            let frames = prepared.queued_frames;
            let sample_count = frames.checked_mul(callback.ring.channels()).ok_or(
                AAudioOutputError::InvalidRingCapacity {
                    capacity_frames: frames,
                    channels: callback.ring.channels(),
                },
            )?;
            let mut samples = vec![0.0f32; sample_count];
            control.processor.read_interleaved(&mut samples)?;
            let pushed = callback
                .ring
                .push_interleaved(&samples, self.config.ring_buffer.drop_oldest_on_overflow);
            control.append_timeline(pushed, prepared.media_time);
            Ok(pushed.into())
        }

        pub fn state(&self) -> AudioOutputState {
            decode_state(self.state.load(Ordering::Acquire))
        }

        pub fn stats(&self) -> Result<AudioRingBufferStats> {
            let mut control = lock(&self.control)?;
            self.recover_disconnected_stream_locked(&mut control)?;
            Ok(control
                .callback
                .as_ref()
                .map_or_else(AudioRingBufferStats::default, |callback| {
                    callback.ring.stats()
                }))
        }

        pub fn runtime_stats(&self) -> AudioOutputRuntimeStats {
            self.signals.snapshot()
        }

        pub fn clock_snapshot(&self) -> Result<AudioClockSnapshot> {
            let mut control = lock(&self.control)?;
            self.recover_disconnected_stream_locked(&mut control)?;
            Ok(control.clock_snapshot())
        }

        fn start_or_recover_stream_locked(&self, control: &mut OutputControl) -> Result<()> {
            if self.recover_disconnected_stream_locked(control)? {
                return Ok(());
            }
            let stream = control
                .stream
                .as_ref()
                .ok_or(AAudioOutputError::NotConfigured)?;
            if let Err(error) = stream.request_start() {
                if let Some(error_code) = error_result_code(&error) {
                    let stats = self.signals.mark_disconnected(error_code);
                    trace_recovery("start_disconnected", stats, Some(&error.to_string()));
                }
                return Err(error);
            }
            Ok(())
        }

        fn recover_disconnected_stream_locked(&self, control: &mut OutputControl) -> Result<bool> {
            let runtime_stats = self.signals.snapshot();
            if self.state.load(Ordering::Acquire) != STATE_PLAYING
                || runtime_stats.recovery_state == AudioRecoveryState::Stable
            {
                return Ok(false);
            }
            if runtime_stats.recovery_state == AudioRecoveryState::Failed
                && control
                    .next_recovery_at
                    .is_some_and(|deadline| Instant::now() < deadline)
            {
                return Ok(true);
            }
            let format = control.format.ok_or(AAudioOutputError::NotConfigured)?;
            let callback = Arc::clone(
                control
                    .callback
                    .as_ref()
                    .ok_or(AAudioOutputError::NotConfigured)?,
            );
            let old_stream = control.stream.take();
            drop(old_stream);
            // Closing the old stream may itself deliver its final error callback.
            // Publish Recovering only after close returns so a stale callback
            // cannot overwrite the new attempt's state.
            let stats = self.signals.begin_recovery();
            let error_text = result_text(stats.last_error_code);
            trace_recovery("recovery_started", stats, Some(&error_text));
            let new_stream = match self.open_stream(format, callback) {
                Ok(stream) => stream,
                Err(error) => {
                    let error_code = error_result_code(&error)
                        .unwrap_or_else(|| self.signals.snapshot().last_error_code);
                    let stats = self.signals.recovery_failed(error_code);
                    control.next_recovery_at = Some(Instant::now() + RECOVERY_RETRY_DELAY);
                    trace_recovery("recovery_open_failed", stats, Some(&error.to_string()));
                    return Err(error);
                }
            };
            if let Err(error) = new_stream.request_start() {
                let error_code = error_result_code(&error)
                    .unwrap_or_else(|| self.signals.snapshot().last_error_code);
                let stats = self.signals.recovery_failed(error_code);
                control.next_recovery_at = Some(Instant::now() + RECOVERY_RETRY_DELAY);
                trace_recovery("recovery_start_failed", stats, Some(&error.to_string()));
                drop(new_stream);
                return Err(error);
            }
            control.stream = Some(new_stream);
            let stats = self
                .signals
                .recovery_succeeded()
                .unwrap_or_else(|| self.signals.snapshot());
            control.next_recovery_at = None;
            let stage = if stats.recovery_state == AudioRecoveryState::Stable {
                "recovered"
            } else {
                // The newly started stream disconnected asynchronously before
                // recovery could commit. Keep it visible and retry next tick.
                "recovery_redisconnected"
            };
            trace_recovery(stage, stats, None);
            Ok(true)
        }

        fn close_stream_locked(
            &self,
            control: &mut OutputControl,
            request_stop: bool,
        ) -> Result<()> {
            let stream = control.stream.take();
            if let Some(stream) = stream {
                let result = if request_stop {
                    stream.request_stop()
                } else {
                    Ok(())
                };
                drop(stream);
                if let Err(error) = result
                    && !is_disconnected_error(&error)
                {
                    return Err(error);
                }
            }
            Ok(())
        }

        fn open_stream(
            &self,
            format: PcmFormat,
            callback: Arc<CallbackState>,
        ) -> Result<StreamHandle> {
            let mut raw_builder = ptr::null_mut();
            check_result(
                unsafe { AAudio_createStreamBuilder(&mut raw_builder) },
                "createStreamBuilder",
            )?;
            let builder =
                StreamBuilder(NonNull::new(raw_builder).ok_or(AAudioOutputError::NotConfigured)?);
            let user_data = Arc::as_ptr(&callback).cast_mut().cast::<c_void>();
            unsafe {
                AAudioStreamBuilder_setDirection(builder.0.as_ptr(), AAUDIO_DIRECTION_OUTPUT);
                AAudioStreamBuilder_setFormat(builder.0.as_ptr(), AAUDIO_FORMAT_PCM_FLOAT);
                AAudioStreamBuilder_setSampleRate(builder.0.as_ptr(), format.sample_rate as i32);
                AAudioStreamBuilder_setChannelCount(builder.0.as_ptr(), format.channels as i32);
                AAudioStreamBuilder_setSharingMode(builder.0.as_ptr(), AAUDIO_SHARING_MODE_SHARED);
                AAudioStreamBuilder_setPerformanceMode(
                    builder.0.as_ptr(),
                    AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
                );
                AAudioStreamBuilder_setDataCallback(
                    builder.0.as_ptr(),
                    Some(audio_data_callback),
                    user_data,
                );
                AAudioStreamBuilder_setErrorCallback(
                    builder.0.as_ptr(),
                    Some(audio_error_callback),
                    user_data,
                );
            }

            let mut raw_stream = ptr::null_mut();
            check_result(
                unsafe { AAudioStreamBuilder_openStream(builder.0.as_ptr(), &mut raw_stream) },
                "openStream",
            )?;
            let stream = StreamHandle {
                raw: NonNull::new(raw_stream).ok_or(AAudioOutputError::NotConfigured)?,
                _callback: callback,
            };
            let actual_format = unsafe { AAudioStream_getFormat(stream.raw.as_ptr()) };
            let actual_sample_rate = unsafe { AAudioStream_getSampleRate(stream.raw.as_ptr()) };
            let actual_channels = unsafe { AAudioStream_getChannelCount(stream.raw.as_ptr()) };
            if actual_format != AAUDIO_FORMAT_PCM_FLOAT
                || actual_sample_rate != format.sample_rate as i32
                || actual_channels != format.channels as i32
            {
                return Err(AAudioOutputError::FormatNegotiation {
                    requested_sample_rate: format.sample_rate,
                    requested_channels: format.channels,
                    actual_sample_rate,
                    actual_channels,
                });
            }
            Ok(stream)
        }
    }

    impl Default for AAudioOutput {
        fn default() -> Self {
            Self::new(AAudioOutputConfig::default())
        }
    }

    impl Drop for AAudioOutput {
        fn drop(&mut self) {
            self.state.store(STATE_STOPPED, Ordering::Release);
            if let Ok(control) = self.control.get_mut()
                && let Some(stream) = control.stream.take()
            {
                let _ = stream.request_stop();
                drop(stream);
            }
        }
    }

    impl AudioOutputBackend for AAudioOutput {
        fn configure(&mut self, format: PcmFormat) -> crate::audio::Result<()> {
            AAudioOutput::configure(self, format)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn start(&mut self) -> crate::audio::Result<()> {
            AAudioOutput::start(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn pause(&mut self) -> crate::audio::Result<()> {
            AAudioOutput::pause(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn stop(&mut self) -> crate::audio::Result<()> {
            AAudioOutput::stop(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn set_volume(&mut self, volume: f32) {
            AAudioOutput::set_volume(self, volume);
        }

        fn volume(&self) -> f32 {
            AAudioOutput::volume(self)
        }

        fn set_playback_rate(&mut self, rate: f64) {
            AAudioOutput::set_playback_rate(self, rate);
        }

        fn can_accept_audio_frame(&self) -> bool {
            AAudioOutput::can_accept_audio_frame(self)
        }

        fn push(&mut self, frame: PcmAudioFrame) -> crate::audio::Result<AudioPushResult> {
            AAudioOutput::push(self, frame)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn state(&self) -> AudioOutputState {
            AAudioOutput::state(self)
        }

        fn stats(&self) -> AudioRingBufferStats {
            AAudioOutput::stats(self).unwrap_or_default()
        }

        fn clock_snapshot(&self) -> Option<AudioClockSnapshot> {
            AAudioOutput::clock_snapshot(self).ok()
        }

        fn runtime_stats(&self) -> AudioOutputRuntimeStats {
            AAudioOutput::runtime_stats(self)
        }
    }

    unsafe extern "C" fn audio_data_callback(
        _stream: *mut AAudioStream,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        num_frames: i32,
    ) -> AAudioDataCallbackResult {
        if user_data.is_null() || audio_data.is_null() || num_frames <= 0 {
            return AAUDIO_CALLBACK_RESULT_STOP;
        }
        let state = unsafe { &*user_data.cast::<CallbackState>() };
        let Some(sample_count) = (num_frames as usize).checked_mul(state.ring.channels()) else {
            return AAUDIO_CALLBACK_RESULT_STOP;
        };
        let output =
            unsafe { std::slice::from_raw_parts_mut(audio_data.cast::<f32>(), sample_count) };
        state.ring.read_interleaved(output);
        AAUDIO_CALLBACK_RESULT_CONTINUE
    }

    unsafe extern "C" fn audio_error_callback(
        _stream: *mut AAudioStream,
        user_data: *mut c_void,
        error: AAudioResult,
    ) {
        if user_data.is_null() {
            return;
        }
        let state = unsafe { &*user_data.cast::<CallbackState>() };
        // AAudio requires close/rebuild outside the callback. This path remains
        // realtime-safe: it only publishes the error and recovery transition.
        state.signals.set_disconnected_from_callback(error);
    }

    fn validate_format(format: PcmFormat) -> Result<()> {
        match format.sample_format {
            PcmSampleFormat::F32Interleaved => {}
        }
        if format.sample_rate == 0
            || format.sample_rate > i32::MAX as u32
            || format.channels == 0
            || format.channels > i32::MAX as u32
        {
            return Err(AAudioOutputError::InvalidFormat {
                sample_rate: format.sample_rate,
                channels: format.channels,
            });
        }
        Ok(())
    }

    fn check_result(result: AAudioResult, operation: &'static str) -> Result<()> {
        if result == AAUDIO_OK {
            return Ok(());
        }
        Err(AAudioOutputError::AAudio {
            operation,
            result,
            message: result_text(result),
        })
    }

    fn result_text(result: AAudioResult) -> String {
        let text = unsafe { AAudio_convertResultToText(result) };
        if text.is_null() {
            return "unknown error".to_string();
        }
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }

    fn error_result_code(error: &AAudioOutputError) -> Option<AAudioResult> {
        match error {
            AAudioOutputError::AAudio { result, .. } => Some(*result),
            _ => None,
        }
    }

    fn is_disconnected_error(error: &AAudioOutputError) -> bool {
        matches!(
            error,
            AAudioOutputError::AAudio {
                result: AAUDIO_ERROR_DISCONNECTED,
                ..
            }
        )
    }

    fn trace_recovery(stage: &'static str, stats: AudioOutputRuntimeStats, reason: Option<&str>) {
        trace::diagnostic(
            serde_json::json!({
                "event": "aaudio_recovery",
                "stage": stage,
                "state": stats.recovery_state.as_str(),
                "lastErrorCode": stats.last_error_code,
                "recoveryAttempts": stats.recovery_attempts,
                "recoveryCount": stats.recovery_count,
                "recoveryFailures": stats.recovery_failures,
                "transitionSequence": stats.transition_sequence,
                "reason": reason,
            })
            .to_string(),
        );
    }

    fn offset_pts_scaled(
        pts: Duration,
        frames: usize,
        sample_rate: u32,
        media_frames_per_output_frame: f64,
    ) -> Option<Duration> {
        if sample_rate == 0 {
            return Some(pts);
        }
        let media_frames = frames as f64 * media_frames_per_output_frame.max(0.0);
        Some(pts + Duration::from_secs_f64(media_frames / sample_rate as f64))
    }

    fn normalize_playback_rate(rate: f64) -> f64 {
        if rate.is_finite() && rate > 0.0 {
            rate.clamp(0.25, 16.0)
        } else {
            1.0
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
        mutex.lock().map_err(|_| AAudioOutputError::LockPoisoned)
    }

    fn decode_state(state: u8) -> AudioOutputState {
        match state {
            STATE_PLAYING => AudioOutputState::Playing,
            STATE_PAUSED => AudioOutputState::Paused,
            _ => AudioOutputState::Stopped,
        }
    }

    fn decode_recovery_state(state: u8) -> AudioRecoveryState {
        match state {
            RECOVERY_DISCONNECTED => AudioRecoveryState::Disconnected,
            RECOVERY_RECOVERING => AudioRecoveryState::Recovering,
            RECOVERY_FAILED => AudioRecoveryState::Failed,
            _ => AudioRecoveryState::Stable,
        }
    }
}
