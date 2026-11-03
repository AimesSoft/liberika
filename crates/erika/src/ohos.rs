pub mod avcodec;
#[cfg(feature = "wgpu")]
pub mod gles;
pub mod pixel_layout;

pub mod ohaudio {
    use std::collections::VecDeque;
    use std::ffi::c_void;
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

    type OHAudioResult = i32;
    type OHAudioFormat = i32;
    type OHAudioDataCallbackResult = i32;

    const OHAUDIO_SUCCESS: OHAudioResult = 0;
    const OHAUDIO_ERROR_ILLEGAL_STATE: OHAudioResult = 2;
    const OHAUDIO_ERROR_SYSTEM: OHAudioResult = 3;
    const OHAUDIO_STREAM_TYPE_RENDERER: i32 = 1;
    const OHAUDIO_SAMPLE_F32LE: OHAudioFormat = 4;
    const OHAUDIO_ENCODING_RAW: i32 = 0;
    const OHAUDIO_LATENCY_NORMAL: i32 = 0;
    const OHAUDIO_USAGE_MUSIC: i32 = 1;
    const OHAUDIO_CALLBACK_RESULT_INVALID: OHAudioDataCallbackResult = -1;
    const OHAUDIO_CALLBACK_RESULT_VALID: OHAudioDataCallbackResult = 0;

    const STATE_STOPPED: u8 = 0;
    const STATE_PLAYING: u8 = 1;
    const STATE_PAUSED: u8 = 2;

    const RECOVERY_STABLE: u8 = 0;
    const RECOVERY_DISCONNECTED: u8 = 1;
    const RECOVERY_RECOVERING: u8 = 2;
    const RECOVERY_FAILED: u8 = 3;
    const RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(250);

    #[repr(C)]
    struct OHAudioStreamBuilder {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct OHAudioRenderer {
        _private: [u8; 0],
    }

    type OHAudioStreamDataCallback = unsafe extern "C" fn(
        renderer: *mut OHAudioRenderer,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        audio_data_size: i32,
    ) -> OHAudioDataCallbackResult;

    type OHAudioStreamEventCallback = unsafe extern "C" fn(
        renderer: *mut OHAudioRenderer,
        user_data: *mut c_void,
        event: i32,
    ) -> i32;

    type OHAudioInterruptCallback = unsafe extern "C" fn(
        renderer: *mut OHAudioRenderer,
        user_data: *mut c_void,
        force_type: i32,
        hint: i32,
    ) -> i32;

    type OHAudioStreamErrorCallback = unsafe extern "C" fn(
        renderer: *mut OHAudioRenderer,
        user_data: *mut c_void,
        error: OHAudioResult,
    ) -> i32;

    #[repr(C)]
    struct OHAudioRendererCallbacks {
        on_write_data: Option<OHAudioStreamDataCallback>,
        on_stream_event: Option<OHAudioStreamEventCallback>,
        on_interrupt_event: Option<OHAudioInterruptCallback>,
        on_error: Option<OHAudioStreamErrorCallback>,
    }

    #[link(name = "ohaudio")]
    unsafe extern "C" {
        fn OH_AudioStreamBuilder_Create(
            builder: *mut *mut OHAudioStreamBuilder,
            stream_type: i32,
        ) -> OHAudioResult;
        fn OH_AudioStreamBuilder_Destroy(builder: *mut OHAudioStreamBuilder) -> OHAudioResult;
        fn OH_AudioStreamBuilder_SetSamplingRate(
            builder: *mut OHAudioStreamBuilder,
            sample_rate: i32,
        ) -> OHAudioResult;
        fn OH_AudioStreamBuilder_SetChannelCount(
            builder: *mut OHAudioStreamBuilder,
            channels: i32,
        ) -> OHAudioResult;
        fn OH_AudioStreamBuilder_SetSampleFormat(
            builder: *mut OHAudioStreamBuilder,
            format: OHAudioFormat,
        ) -> OHAudioResult;
        fn OH_AudioStreamBuilder_SetEncodingType(
            builder: *mut OHAudioStreamBuilder,
            encoding_type: i32,
        ) -> OHAudioResult;
        fn OH_AudioStreamBuilder_SetLatencyMode(
            builder: *mut OHAudioStreamBuilder,
            latency_mode: i32,
        ) -> OHAudioResult;
        fn OH_AudioStreamBuilder_SetRendererInfo(
            builder: *mut OHAudioStreamBuilder,
            usage: i32,
        ) -> OHAudioResult;
        fn OH_AudioStreamBuilder_SetRendererCallback(
            builder: *mut OHAudioStreamBuilder,
            callbacks: OHAudioRendererCallbacks,
            user_data: *mut c_void,
        ) -> OHAudioResult;
        fn OH_AudioStreamBuilder_GenerateRenderer(
            builder: *mut OHAudioStreamBuilder,
            renderer: *mut *mut OHAudioRenderer,
        ) -> OHAudioResult;
        fn OH_AudioRenderer_Start(renderer: *mut OHAudioRenderer) -> OHAudioResult;
        fn OH_AudioRenderer_Pause(renderer: *mut OHAudioRenderer) -> OHAudioResult;
        fn OH_AudioRenderer_Stop(renderer: *mut OHAudioRenderer) -> OHAudioResult;
        fn OH_AudioRenderer_Release(renderer: *mut OHAudioRenderer) -> OHAudioResult;
        fn OH_AudioRenderer_GetSampleFormat(
            renderer: *mut OHAudioRenderer,
            format: *mut OHAudioFormat,
        ) -> OHAudioResult;
        fn OH_AudioRenderer_GetSamplingRate(
            renderer: *mut OHAudioRenderer,
            sample_rate: *mut i32,
        ) -> OHAudioResult;
        fn OH_AudioRenderer_GetChannelCount(
            renderer: *mut OHAudioRenderer,
            channels: *mut i32,
        ) -> OHAudioResult;
    }

    #[derive(Debug, Error)]
    pub enum OHAudioOutputError {
        #[error("audio error: {0}")]
        Audio(#[from] crate::audio::AudioError),
        #[error("OHAudio output is not configured")]
        NotConfigured,
        #[error("OHAudio output lock was poisoned")]
        LockPoisoned,
        #[error("unsupported OHAudio PCM format: {sample_rate} Hz, {channels} channels")]
        InvalidFormat { sample_rate: u32, channels: u32 },
        #[error("invalid OHAudio ring capacity: {capacity_frames} frames x {channels} channels")]
        InvalidRingCapacity {
            capacity_frames: usize,
            channels: usize,
        },
        #[error("OHAudio {operation} failed with {result} ({message})")]
        OHAudio {
            operation: &'static str,
            result: OHAudioResult,
            message: String,
        },
        #[error(
            "OHAudio opened {actual_sample_rate} Hz/{actual_channels} ch instead of {requested_sample_rate} Hz/{requested_channels} ch"
        )]
        FormatNegotiation {
            requested_sample_rate: u32,
            requested_channels: u32,
            actual_sample_rate: i32,
            actual_channels: i32,
        },
    }

    pub type Result<T> = std::result::Result<T, OHAudioOutputError>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OHAudioOutputConfig {
        pub ring_buffer: AudioRingBufferConfig,
    }

    impl Default for OHAudioOutputConfig {
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
                last_error_code: AtomicI32::new(OHAUDIO_SUCCESS),
                recovery_attempts: AtomicU64::new(0),
                recovery_count: AtomicU64::new(0),
                recovery_failures: AtomicU64::new(0),
                transition_sequence: AtomicU64::new(0),
            }
        }

        fn volume(&self) -> f32 {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        }

        fn set_disconnected_from_callback(&self, error: OHAudioResult) {
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

        fn recovery_failed(&self, error_code: OHAudioResult) -> AudioOutputRuntimeStats {
            self.last_error_code.store(error_code, Ordering::Relaxed);
            self.recovery_failures.fetch_add(1, Ordering::Relaxed);
            self.recovery_state
                .store(RECOVERY_FAILED, Ordering::Release);
            self.transition_sequence.fetch_add(1, Ordering::Release);
            self.snapshot()
        }

        fn mark_disconnected(&self, error_code: OHAudioResult) -> AudioOutputRuntimeStats {
            self.last_error_code.store(error_code, Ordering::Relaxed);
            self.recovery_state
                .store(RECOVERY_DISCONNECTED, Ordering::Release);
            self.transition_sequence.fetch_add(1, Ordering::Release);
            self.snapshot()
        }

        fn reset_current_error(&self) {
            self.last_error_code
                .store(OHAUDIO_SUCCESS, Ordering::Relaxed);
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

    struct StreamBuilder(NonNull<OHAudioStreamBuilder>);

    impl Drop for StreamBuilder {
        fn drop(&mut self) {
            unsafe {
                let _ = OH_AudioStreamBuilder_Destroy(self.0.as_ptr());
            }
        }
    }

    struct StreamHandle {
        raw: NonNull<OHAudioRenderer>,
        _callback: Arc<CallbackState>,
    }

    // OHAudio permits renderer control outside its realtime callback. The
    // pointer remains owned until `OH_AudioRenderer_Release`.
    unsafe impl Send for StreamHandle {}

    impl StreamHandle {
        fn request_start(&self) -> Result<()> {
            check_result(
                unsafe { OH_AudioRenderer_Start(self.raw.as_ptr()) },
                "requestStart",
            )
        }

        fn request_pause(&self) -> Result<()> {
            check_result(
                unsafe { OH_AudioRenderer_Pause(self.raw.as_ptr()) },
                "requestPause",
            )
        }

        fn request_stop(&self) -> Result<()> {
            check_result(
                unsafe { OH_AudioRenderer_Stop(self.raw.as_ptr()) },
                "requestStop",
            )
        }
    }

    impl Drop for StreamHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = OH_AudioRenderer_Release(self.raw.as_ptr());
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
            let format = self.format.expect("configured OHAudio format");
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

    pub struct OHAudioOutput {
        config: OHAudioOutputConfig,
        control: Mutex<OutputControl>,
        signals: Arc<OutputSignals>,
        state: AtomicU8,
    }

    impl OHAudioOutput {
        pub fn new(config: OHAudioOutputConfig) -> Self {
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
                .ok_or(OHAudioOutputError::InvalidRingCapacity {
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
                        .recovery_failed(error_result_code(&error).unwrap_or(OHAUDIO_SUCCESS));
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
                previous_state == STATE_PLAYING || previous_state == STATE_PAUSED,
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
                    .ok_or(OHAudioOutputError::NotConfigured)?,
            );
            control.processor.push_frame(frame)?;
            let prepared = control.processor.clock_snapshot();
            let frames = prepared.queued_frames;
            let sample_count = frames.checked_mul(callback.ring.channels()).ok_or(
                OHAudioOutputError::InvalidRingCapacity {
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
                .ok_or(OHAudioOutputError::NotConfigured)?;
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
            let format = control.format.ok_or(OHAudioOutputError::NotConfigured)?;
            let callback = Arc::clone(
                control
                    .callback
                    .as_ref()
                    .ok_or(OHAudioOutputError::NotConfigured)?,
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
                unsafe {
                    OH_AudioStreamBuilder_Create(&mut raw_builder, OHAUDIO_STREAM_TYPE_RENDERER)
                },
                "createStreamBuilder",
            )?;
            let builder =
                StreamBuilder(NonNull::new(raw_builder).ok_or(OHAudioOutputError::NotConfigured)?);
            let user_data = Arc::as_ptr(&callback).cast_mut().cast::<c_void>();
            check_result(
                unsafe {
                    OH_AudioStreamBuilder_SetSamplingRate(
                        builder.0.as_ptr(),
                        format.sample_rate as i32,
                    )
                },
                "setSamplingRate",
            )?;
            check_result(
                unsafe {
                    OH_AudioStreamBuilder_SetChannelCount(
                        builder.0.as_ptr(),
                        format.channels as i32,
                    )
                },
                "setChannelCount",
            )?;
            check_result(
                unsafe {
                    OH_AudioStreamBuilder_SetSampleFormat(builder.0.as_ptr(), OHAUDIO_SAMPLE_F32LE)
                },
                "setSampleFormat",
            )?;
            check_result(
                unsafe {
                    OH_AudioStreamBuilder_SetEncodingType(builder.0.as_ptr(), OHAUDIO_ENCODING_RAW)
                },
                "setEncodingType",
            )?;
            check_result(
                unsafe {
                    OH_AudioStreamBuilder_SetLatencyMode(builder.0.as_ptr(), OHAUDIO_LATENCY_NORMAL)
                },
                "setLatencyMode",
            )?;
            check_result(
                unsafe {
                    OH_AudioStreamBuilder_SetRendererInfo(builder.0.as_ptr(), OHAUDIO_USAGE_MUSIC)
                },
                "setRendererInfo",
            )?;
            check_result(
                unsafe {
                    OH_AudioStreamBuilder_SetRendererCallback(
                        builder.0.as_ptr(),
                        OHAudioRendererCallbacks {
                            on_write_data: Some(audio_data_callback),
                            on_stream_event: Some(audio_stream_event_callback),
                            on_interrupt_event: Some(audio_interrupt_callback),
                            on_error: Some(audio_error_callback),
                        },
                        user_data,
                    )
                },
                "setRendererCallback",
            )?;

            let mut raw_stream = ptr::null_mut();
            check_result(
                unsafe {
                    OH_AudioStreamBuilder_GenerateRenderer(builder.0.as_ptr(), &mut raw_stream)
                },
                "generateRenderer",
            )?;
            let stream = StreamHandle {
                raw: NonNull::new(raw_stream).ok_or(OHAudioOutputError::NotConfigured)?,
                _callback: callback,
            };
            let mut actual_format = -1;
            let mut actual_sample_rate = 0;
            let mut actual_channels = 0;
            check_result(
                unsafe {
                    OH_AudioRenderer_GetSampleFormat(stream.raw.as_ptr(), &mut actual_format)
                },
                "getSampleFormat",
            )?;
            check_result(
                unsafe {
                    OH_AudioRenderer_GetSamplingRate(stream.raw.as_ptr(), &mut actual_sample_rate)
                },
                "getSamplingRate",
            )?;
            check_result(
                unsafe {
                    OH_AudioRenderer_GetChannelCount(stream.raw.as_ptr(), &mut actual_channels)
                },
                "getChannelCount",
            )?;
            if actual_format != OHAUDIO_SAMPLE_F32LE
                || actual_sample_rate != format.sample_rate as i32
                || actual_channels != format.channels as i32
            {
                return Err(OHAudioOutputError::FormatNegotiation {
                    requested_sample_rate: format.sample_rate,
                    requested_channels: format.channels,
                    actual_sample_rate,
                    actual_channels,
                });
            }
            Ok(stream)
        }
    }

    impl Default for OHAudioOutput {
        fn default() -> Self {
            Self::new(OHAudioOutputConfig::default())
        }
    }

    impl Drop for OHAudioOutput {
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

    impl AudioOutputBackend for OHAudioOutput {
        fn configure(&mut self, format: PcmFormat) -> crate::audio::Result<()> {
            OHAudioOutput::configure(self, format)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn start(&mut self) -> crate::audio::Result<()> {
            OHAudioOutput::start(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn pause(&mut self) -> crate::audio::Result<()> {
            OHAudioOutput::pause(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn stop(&mut self) -> crate::audio::Result<()> {
            OHAudioOutput::stop(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn set_volume(&mut self, volume: f32) {
            OHAudioOutput::set_volume(self, volume);
        }

        fn volume(&self) -> f32 {
            OHAudioOutput::volume(self)
        }

        fn set_playback_rate(&mut self, rate: f64) {
            OHAudioOutput::set_playback_rate(self, rate);
        }

        fn can_accept_audio_frame(&self) -> bool {
            OHAudioOutput::can_accept_audio_frame(self)
        }

        fn push(&mut self, frame: PcmAudioFrame) -> crate::audio::Result<AudioPushResult> {
            OHAudioOutput::push(self, frame)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn state(&self) -> AudioOutputState {
            OHAudioOutput::state(self)
        }

        fn stats(&self) -> AudioRingBufferStats {
            OHAudioOutput::stats(self).unwrap_or_default()
        }

        fn clock_snapshot(&self) -> Option<AudioClockSnapshot> {
            OHAudioOutput::clock_snapshot(self).ok()
        }

        fn runtime_stats(&self) -> AudioOutputRuntimeStats {
            OHAudioOutput::runtime_stats(self)
        }
    }

    unsafe extern "C" fn audio_data_callback(
        _renderer: *mut OHAudioRenderer,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        audio_data_size: i32,
    ) -> OHAudioDataCallbackResult {
        if user_data.is_null() || audio_data.is_null() || audio_data_size <= 0 {
            return OHAUDIO_CALLBACK_RESULT_INVALID;
        }
        let state = unsafe { &*user_data.cast::<CallbackState>() };
        let sample_count = audio_data_size as usize / std::mem::size_of::<f32>();
        if sample_count == 0 || !sample_count.is_multiple_of(state.ring.channels()) {
            return OHAUDIO_CALLBACK_RESULT_INVALID;
        }
        let output =
            unsafe { std::slice::from_raw_parts_mut(audio_data.cast::<f32>(), sample_count) };
        state.ring.read_interleaved(output);
        OHAUDIO_CALLBACK_RESULT_VALID
    }

    unsafe extern "C" fn audio_stream_event_callback(
        _renderer: *mut OHAudioRenderer,
        _user_data: *mut c_void,
        _event: i32,
    ) -> i32 {
        OHAUDIO_SUCCESS
    }

    unsafe extern "C" fn audio_interrupt_callback(
        _renderer: *mut OHAudioRenderer,
        _user_data: *mut c_void,
        _force_type: i32,
        _hint: i32,
    ) -> i32 {
        OHAUDIO_SUCCESS
    }

    unsafe extern "C" fn audio_error_callback(
        _renderer: *mut OHAudioRenderer,
        user_data: *mut c_void,
        error: OHAudioResult,
    ) -> i32 {
        if user_data.is_null() {
            return OHAUDIO_SUCCESS;
        }
        let state = unsafe { &*user_data.cast::<CallbackState>() };
        // OHAudio requires close/rebuild outside the callback. This path remains
        // realtime-safe: it only publishes the error and recovery transition.
        state.signals.set_disconnected_from_callback(error);
        OHAUDIO_SUCCESS
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
            return Err(OHAudioOutputError::InvalidFormat {
                sample_rate: format.sample_rate,
                channels: format.channels,
            });
        }
        Ok(())
    }

    fn check_result(result: OHAudioResult, operation: &'static str) -> Result<()> {
        if result == OHAUDIO_SUCCESS {
            return Ok(());
        }
        Err(OHAudioOutputError::OHAudio {
            operation,
            result,
            message: result_text(result),
        })
    }

    fn result_text(result: OHAudioResult) -> String {
        match result {
            OHAUDIO_SUCCESS => "success",
            1 => "invalid parameter",
            OHAUDIO_ERROR_ILLEGAL_STATE => "illegal state",
            OHAUDIO_ERROR_SYSTEM => "system error",
            4 => "unsupported format",
            _ => "unknown error",
        }
        .to_string()
    }

    fn error_result_code(error: &OHAudioOutputError) -> Option<OHAudioResult> {
        match error {
            OHAudioOutputError::OHAudio { result, .. } => Some(*result),
            _ => None,
        }
    }

    fn is_disconnected_error(error: &OHAudioOutputError) -> bool {
        matches!(
            error,
            OHAudioOutputError::OHAudio {
                result: OHAUDIO_ERROR_ILLEGAL_STATE | OHAUDIO_ERROR_SYSTEM,
                ..
            }
        )
    }

    fn trace_recovery(stage: &'static str, stats: AudioOutputRuntimeStats, reason: Option<&str>) {
        trace::diagnostic(
            serde_json::json!({
                "event": "ohaudio_recovery",
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
        mutex.lock().map_err(|_| OHAudioOutputError::LockPoisoned)
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
