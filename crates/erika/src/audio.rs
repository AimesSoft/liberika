use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use soundtouch::{Setting, SoundTouch};
use thiserror::Error;

use crate::ffmpeg::{PcmAudioFrame, PcmFormat};
use crate::trace;

#[cfg(any(target_os = "android", target_env = "ohos", test))]
pub(crate) mod spsc;

// Keep enough old-rate PCM to cover SoundTouch startup and one normal output
// prefill. The presenter commits the media clock at the end of this bridge.
pub(crate) const AUDIO_OUTPUT_QUEUE_HIGH_WATER: Duration = Duration::from_millis(250);
const RATE_CHANGE_AUDIO_BRIDGE: Duration = AUDIO_OUTPUT_QUEUE_HIGH_WATER;
const SOUNDTOUCH_SEQUENCE_MS: i32 = 25;
const SOUNDTOUCH_SEEK_WINDOW_MS: i32 = 12;
const SOUNDTOUCH_OVERLAP_MS: i32 = 6;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AudioError {
    #[error("audio format changed from {expected:?} to {actual:?}")]
    FormatChanged {
        expected: PcmFormat,
        actual: PcmFormat,
    },
    #[error("audio format is not configured")]
    FormatNotConfigured,
    #[error("audio channel count must be greater than zero")]
    InvalidChannelCount,
    #[error("audio backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, AudioError>;

/// Whether another decoded frame may enter an output queue without exceeding
/// the bounded latency budget used for playback-rate transitions.
#[cfg(any(test, target_os = "android", target_os = "linux", target_env = "ohos"))]
pub(crate) fn audio_output_queue_has_capacity(queued_frames: usize, sample_rate: u32) -> bool {
    if sample_rate == 0 {
        return true;
    }
    let high_water_frames = usize::try_from(
        (sample_rate as u64).saturating_mul(AUDIO_OUTPUT_QUEUE_HIGH_WATER.as_millis() as u64)
            / 1_000,
    )
    .unwrap_or(usize::MAX)
    .max(1);
    queued_frames < high_water_frames
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioOutputState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioRingBufferConfig {
    pub capacity_frames: usize,
    pub drop_oldest_on_overflow: bool,
}

impl Default for AudioRingBufferConfig {
    fn default() -> Self {
        Self {
            capacity_frames: 192_000,
            drop_oldest_on_overflow: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioRingBufferStats {
    pub queued_frames: usize,
    pub queued_samples: usize,
    pub written_frames: u64,
    pub read_frames: u64,
    pub dropped_frames: u64,
    pub underflow_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPushResult {
    pub accepted_frames: usize,
    pub dropped_frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioReadResult {
    pub frames: usize,
    pub samples: usize,
    pub underflow_frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioClockSnapshot {
    pub media_time: Option<Duration>,
    pub queued_duration: Option<Duration>,
    pub queued_frames: usize,
    pub read_frames: u64,
    pub written_frames: u64,
    pub underflow_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioRecoveryState {
    #[default]
    Stable,
    Disconnected,
    Recovering,
    Failed,
}

impl AudioRecoveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Disconnected => "disconnected",
            Self::Recovering => "recovering",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioOutputRuntimeStats {
    pub recovery_state: AudioRecoveryState,
    pub last_error_code: i32,
    pub recovery_attempts: u64,
    pub recovery_count: u64,
    pub recovery_failures: u64,
    pub transition_sequence: u64,
}

/// Exponential backoff schedule for audio device-loss recovery attempts.
///
/// Platform backends drive the schedule from their render/callback threads;
/// the schedule itself is platform independent so it stays unit testable on
/// any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryBackoff {
    initial_delay: Duration,
    max_attempts: u32,
    attempts: u32,
}

impl RecoveryBackoff {
    pub fn new(initial_delay: Duration, max_attempts: u32) -> Self {
        Self {
            initial_delay,
            max_attempts,
            attempts: 0,
        }
    }

    /// Returns the delay to wait before the next recovery attempt, doubling
    /// on every call, or `None` once the attempt budget is exhausted.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.attempts >= self.max_attempts {
            return None;
        }
        let exponent = self.attempts.min(31);
        self.attempts += 1;
        Some(self.initial_delay.saturating_mul(1u32 << exponent))
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn is_exhausted(&self) -> bool {
        self.attempts >= self.max_attempts
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
    }
}

const RECOVERY_STATE_STABLE: u8 = 0;
const RECOVERY_STATE_DISCONNECTED: u8 = 1;
const RECOVERY_STATE_RECOVERING: u8 = 2;
const RECOVERY_STATE_FAILED: u8 = 3;

fn decode_recovery_state(state: u8) -> AudioRecoveryState {
    match state {
        RECOVERY_STATE_DISCONNECTED => AudioRecoveryState::Disconnected,
        RECOVERY_STATE_RECOVERING => AudioRecoveryState::Recovering,
        RECOVERY_STATE_FAILED => AudioRecoveryState::Failed,
        _ => AudioRecoveryState::Stable,
    }
}

/// Lock-free publication of audio device-loss recovery progress.
///
/// A backend's render thread records state transitions while control threads
/// read [`RecoverySignals::snapshot`] for
/// [`AudioOutputBackend::runtime_stats`]. The transitions mirror the AAudio
/// recovery state machine in `android.rs`.
#[derive(Debug, Default)]
pub struct RecoverySignals {
    recovery_state: AtomicU8,
    last_error_code: AtomicI32,
    recovery_attempts: AtomicU64,
    recovery_count: AtomicU64,
    recovery_failures: AtomicU64,
    transition_sequence: AtomicU64,
}

impl RecoverySignals {
    pub fn mark_disconnected(&self, error_code: i32) -> AudioOutputRuntimeStats {
        self.last_error_code.store(error_code, Ordering::Relaxed);
        self.recovery_state
            .store(RECOVERY_STATE_DISCONNECTED, Ordering::Release);
        self.transition_sequence.fetch_add(1, Ordering::Release);
        self.snapshot()
    }

    pub fn begin_recovery(&self) -> AudioOutputRuntimeStats {
        self.recovery_attempts.fetch_add(1, Ordering::Relaxed);
        self.recovery_state
            .store(RECOVERY_STATE_RECOVERING, Ordering::Release);
        self.transition_sequence.fetch_add(1, Ordering::Release);
        self.snapshot()
    }

    /// Commits a successful recovery. Returns `None` when the state moved on
    /// concurrently (for example a fresh disconnect landed before the commit),
    /// in which case the caller must keep the newer state visible.
    pub fn recovery_succeeded(&self) -> Option<AudioOutputRuntimeStats> {
        if self
            .recovery_state
            .compare_exchange(
                RECOVERY_STATE_RECOVERING,
                RECOVERY_STATE_STABLE,
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

    pub fn recovery_failed(&self, error_code: i32) -> AudioOutputRuntimeStats {
        self.last_error_code.store(error_code, Ordering::Relaxed);
        self.recovery_failures.fetch_add(1, Ordering::Relaxed);
        self.recovery_state
            .store(RECOVERY_STATE_FAILED, Ordering::Release);
        self.transition_sequence.fetch_add(1, Ordering::Release);
        self.snapshot()
    }

    pub fn reset(&self) {
        self.last_error_code.store(0, Ordering::Relaxed);
        self.recovery_state
            .store(RECOVERY_STATE_STABLE, Ordering::Release);
        self.transition_sequence.fetch_add(1, Ordering::Release);
    }

    pub fn snapshot(&self) -> AudioOutputRuntimeStats {
        loop {
            let sequence_before = self.transition_sequence.load(Ordering::Acquire);
            let snapshot = AudioOutputRuntimeStats {
                recovery_state: decode_recovery_state(self.recovery_state.load(Ordering::Acquire)),
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

#[derive(Debug, Clone, Copy, PartialEq)]
struct AudioTimelineSegment {
    start: Option<Duration>,
    frames: usize,
    media_frames_per_output_frame: f64,
}

impl AudioTimelineSegment {
    fn new(start: Option<Duration>, frames: usize, media_frames_per_output_frame: f64) -> Self {
        Self {
            start,
            frames,
            media_frames_per_output_frame,
        }
    }
}

#[derive(Debug)]
struct AudioTempoProcessor {
    format: PcmFormat,
    playback_rate: f64,
    processor: SoundTouch,
    pending_pts: Option<Duration>,
    scratch: Vec<f32>,
}

impl AudioTempoProcessor {
    fn new(format: PcmFormat, playback_rate: f64) -> Self {
        let playback_rate = normalize_playback_rate(playback_rate);
        let mut processor = SoundTouch::new();
        processor
            .set_sample_rate(format.sample_rate.max(8_000))
            .set_channels(format.channels.max(1) as u32)
            .set_tempo(playback_rate)
            .set_setting(Setting::UseQuickseek, 1)
            .set_setting(Setting::SequenceMs, SOUNDTOUCH_SEQUENCE_MS)
            .set_setting(Setting::SeekwindowMs, SOUNDTOUCH_SEEK_WINDOW_MS)
            .set_setting(Setting::OverlapMs, SOUNDTOUCH_OVERLAP_MS);
        Self {
            format,
            playback_rate,
            processor,
            pending_pts: None,
            scratch: Vec::new(),
        }
    }

    fn matches(&self, format: PcmFormat, playback_rate: f64) -> bool {
        self.format == format
            && (self.playback_rate - normalize_playback_rate(playback_rate)).abs() < 0.001
    }

    fn process(&mut self, mut frame: PcmAudioFrame) -> (Vec<f32>, Option<Duration>, f64) {
        let channels = self.format.channels.max(1) as usize;
        let input_frames = frame.samples.len() / channels;
        if input_frames == 0 {
            return (Vec::new(), frame.pts, self.playback_rate);
        }
        if self.pending_pts.is_none() {
            self.pending_pts = frame.pts;
        }
        self.processor.put_samples(&frame.samples, input_frames);
        // Reuse the decoder-owned allocation for the transformed output and
        // keep the SoundTouch scratch buffer on the processor. This path runs
        // from the display-driven presenter, so avoiding fresh allocations
        // directly protects the next frame deadline after a rate change.
        frame.samples.clear();
        self.receive_available_into(&mut frame.samples);
        let start = self.pending_pts;
        let output_frames = frame.samples.len() / channels;
        if output_frames > 0 {
            if let Some(start) = self.pending_pts {
                self.pending_pts = offset_pts_scaled(
                    start,
                    output_frames,
                    self.format.sample_rate,
                    self.playback_rate,
                );
            }
        }
        (frame.samples, start, self.playback_rate)
    }

    fn receive_available_into(&mut self, output: &mut Vec<f32>) {
        const OUTPUT_FRAMES: usize = 4096;
        let channels = self.format.channels.max(1) as usize;
        self.scratch.resize(OUTPUT_FRAMES * channels, 0.0);
        loop {
            let frames = self
                .processor
                .receive_samples(&mut self.scratch, OUTPUT_FRAMES);
            if frames == 0 {
                break;
            }
            output.extend_from_slice(&self.scratch[..frames * channels]);
            if frames < OUTPUT_FRAMES {
                break;
            }
        }
    }
}

#[derive(Debug)]
pub struct AudioRingBuffer {
    config: AudioRingBufferConfig,
    format: Option<PcmFormat>,
    samples: VecDeque<f32>,
    timeline: VecDeque<AudioTimelineSegment>,
    last_media_time: Option<Duration>,
    playback_rate: f64,
    tempo_processor: Option<AudioTempoProcessor>,
    stats: AudioRingBufferStats,
}

impl AudioRingBuffer {
    pub fn new(config: AudioRingBufferConfig) -> Self {
        Self {
            config,
            format: None,
            samples: VecDeque::new(),
            timeline: VecDeque::new(),
            last_media_time: None,
            playback_rate: 1.0,
            tempo_processor: None,
            stats: AudioRingBufferStats::default(),
        }
    }

    pub fn with_format(config: AudioRingBufferConfig, format: PcmFormat) -> Result<Self> {
        let mut buffer = Self::new(config);
        buffer.configure(format)?;
        Ok(buffer)
    }

    pub fn configure(&mut self, format: PcmFormat) -> Result<()> {
        if format.channels == 0 {
            return Err(AudioError::InvalidChannelCount);
        }
        self.format = Some(format);
        self.clear();
        Ok(())
    }

    pub fn format(&self) -> Option<PcmFormat> {
        self.format
    }

    pub fn capacity_frames(&self) -> usize {
        self.config.capacity_frames
    }

    pub fn queued_frames(&self) -> usize {
        let Some(format) = self.format else {
            return 0;
        };
        self.samples.len() / format.channels as usize
    }

    pub fn queued_duration(&self) -> Option<Duration> {
        let format = self.format?;
        if format.sample_rate == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(
            self.queued_frames() as f64 / format.sample_rate as f64,
        ))
    }

    pub fn stats(&self) -> AudioRingBufferStats {
        AudioRingBufferStats {
            queued_frames: self.queued_frames(),
            queued_samples: self.samples.len(),
            ..self.stats
        }
    }

    pub fn clock_snapshot(&self) -> AudioClockSnapshot {
        AudioClockSnapshot {
            media_time: self
                .timeline
                .front()
                .and_then(|segment| segment.start)
                .or(self.last_media_time),
            queued_duration: self.queued_duration(),
            queued_frames: self.queued_frames(),
            read_frames: self.stats.read_frames,
            written_frames: self.stats.written_frames,
            underflow_frames: self.stats.underflow_frames,
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.timeline.clear();
        self.last_media_time = None;
        self.tempo_processor = None;
        self.stats.queued_frames = 0;
        self.stats.queued_samples = 0;
    }

    pub fn set_playback_rate(&mut self, rate: f64) {
        let rate = normalize_playback_rate(rate);
        if (self.playback_rate - rate).abs() <= 0.001 {
            return;
        }

        // Samples already in this queue were processed at the previous rate.
        // Retain a bounded bridge while SoundTouch warms up for the new rate;
        // the presenter changes the media clock when this bridge has played.
        self.playback_rate = rate;
        self.tempo_processor = None;
        self.trim_queued_to_front_frames(self.rate_change_bridge_frames());
    }

    pub fn push_frame(&mut self, frame: PcmAudioFrame) -> Result<AudioPushResult> {
        match self.format {
            Some(format) if format != frame.format => {
                return Err(AudioError::FormatChanged {
                    expected: format,
                    actual: frame.format,
                });
            }
            Some(_) => {}
            None => self.configure(frame.format)?,
        }

        let format = self.format.expect("audio format exists");
        let channels = format.channels as usize;
        let original_frames = frame.samples.len() / channels;
        let mut dropped_frames = 0usize;
        let frame_pts = frame.pts;
        let (frame_samples, segment_start, media_frames_per_output_frame) =
            self.prepare_frame_samples(frame, format);
        let incoming_frames = frame_samples.len() / channels;
        if incoming_frames == 0 {
            return Ok(AudioPushResult {
                accepted_frames: 0,
                dropped_frames: 0,
            });
        }
        let media_frames_per_output_frame = if media_frames_per_output_frame > 0.0 {
            media_frames_per_output_frame
        } else {
            original_frames as f64 / incoming_frames.max(1) as f64
        };

        if self.config.drop_oldest_on_overflow {
            while self.queued_frames() + incoming_frames > self.config.capacity_frames {
                if !self.drop_oldest_frame(channels) {
                    break;
                }
                dropped_frames += 1;
            }
        }

        let skip_incoming_frames = if self.config.drop_oldest_on_overflow {
            incoming_frames.saturating_sub(self.config.capacity_frames)
        } else {
            0
        };
        let skipped_incoming_samples = skip_incoming_frames * channels;
        let free_frames = self
            .config
            .capacity_frames
            .saturating_sub(self.queued_frames());
        let accepted_frames = incoming_frames
            .saturating_sub(skip_incoming_frames)
            .min(free_frames);
        let accepted_samples = accepted_frames * channels;
        self.samples.extend(
            frame_samples
                .into_iter()
                .skip(skipped_incoming_samples)
                .take(accepted_samples),
        );
        self.push_timeline_segment(
            segment_start.and_then(|pts| {
                offset_pts_scaled(
                    pts,
                    skip_incoming_frames,
                    format.sample_rate,
                    media_frames_per_output_frame,
                )
            }),
            accepted_frames,
            media_frames_per_output_frame,
        );
        self.stats.written_frames += accepted_frames as u64;
        self.stats.dropped_frames +=
            (dropped_frames + incoming_frames.saturating_sub(accepted_frames)) as u64;

        if trace::enabled()
            && (dropped_frames > 0
                || accepted_frames < incoming_frames
                || self.queued_frames() >= self.config.capacity_frames.saturating_sub(256))
        {
            trace::log(format!(
                "[erika-audio-trace] stage=push pts={} incoming_frames={} accepted_frames={} dropped_frames={} queued_frames={} queued_duration={} written_frames={} dropped_total={} sample_rate={} channels={} capacity_frames={} drop_oldest={}",
                frame_pts.map_or_else(
                    || "-".to_string(),
                    |pts| format!("{:.3}", pts.as_secs_f64())
                ),
                incoming_frames,
                accepted_frames,
                dropped_frames + incoming_frames.saturating_sub(accepted_frames),
                self.queued_frames(),
                self.queued_duration().map_or_else(
                    || "-".to_string(),
                    |duration| format!("{:.3}", duration.as_secs_f64())
                ),
                self.stats.written_frames,
                self.stats.dropped_frames,
                format!("{}", format.sample_rate),
                channels,
                self.config.capacity_frames,
                self.config.drop_oldest_on_overflow,
            ));
        }

        Ok(AudioPushResult {
            accepted_frames,
            dropped_frames: dropped_frames + incoming_frames.saturating_sub(accepted_frames),
        })
    }

    pub fn read_interleaved(&mut self, output: &mut [f32]) -> Result<AudioReadResult> {
        let format = self.format.ok_or(AudioError::FormatNotConfigured)?;
        let channels = format.channels as usize;
        if channels == 0 {
            return Err(AudioError::InvalidChannelCount);
        }

        let requested_frames = output.len() / channels;
        let requested_samples = requested_frames * channels;
        let mut read_samples = 0usize;
        for sample in output.iter_mut().take(requested_samples) {
            if let Some(value) = self.samples.pop_front() {
                *sample = value;
                read_samples += 1;
            } else {
                *sample = 0.0;
            }
        }
        for sample in output.iter_mut().skip(requested_samples) {
            *sample = 0.0;
        }

        let read_frames = read_samples / channels;
        let underflow_frames = requested_frames.saturating_sub(read_frames);
        self.advance_timeline(read_frames, format.sample_rate);
        self.stats.read_frames += read_frames as u64;
        self.stats.underflow_frames += underflow_frames as u64;

        if trace::enabled() && (underflow_frames > 0 || read_frames == 0 && requested_frames > 0) {
            trace::log(format!(
                "[erika-audio-trace] stage=read requested_frames={} read_frames={} underflow_frames={} queued_frames={} queued_duration={} read_total={} underflow_total={} sample_rate={} channels={} state=buffered",
                requested_frames,
                read_frames,
                underflow_frames,
                self.queued_frames(),
                self.queued_duration().map_or_else(
                    || "-".to_string(),
                    |duration| format!("{:.3}", duration.as_secs_f64())
                ),
                self.stats.read_frames,
                self.stats.underflow_frames,
                format.sample_rate,
                channels,
            ));
        }

        Ok(AudioReadResult {
            frames: read_frames,
            samples: read_samples,
            underflow_frames,
        })
    }

    fn prepare_frame_samples(
        &mut self,
        frame: PcmAudioFrame,
        format: PcmFormat,
    ) -> (Vec<f32>, Option<Duration>, f64) {
        if (self.playback_rate - 1.0).abs() <= 0.001 {
            self.tempo_processor = None;
            return (frame.samples, frame.pts, 1.0);
        }
        let processor = self
            .tempo_processor
            .get_or_insert_with(|| AudioTempoProcessor::new(format, self.playback_rate));
        if !processor.matches(format, self.playback_rate) {
            *processor = AudioTempoProcessor::new(format, self.playback_rate);
        }
        processor.process(frame)
    }

    fn drop_oldest_frame(&mut self, channels: usize) -> bool {
        if self.samples.len() < channels {
            self.samples.clear();
            return false;
        }
        for _ in 0..channels {
            let _ = self.samples.pop_front();
        }
        self.advance_timeline(1, self.format.map_or(0, |format| format.sample_rate));
        true
    }

    fn rate_change_bridge_frames(&self) -> usize {
        self.format.map_or(0, |format| {
            (format.sample_rate as f64 * RATE_CHANGE_AUDIO_BRIDGE.as_secs_f64()).round() as usize
        })
    }

    fn trim_queued_to_front_frames(&mut self, frames: usize) {
        let Some(format) = self.format else {
            self.samples.clear();
            self.timeline.clear();
            return;
        };
        let channels = format.channels as usize;
        let target_samples = frames.saturating_mul(channels);
        if self.samples.len() > target_samples {
            self.samples.truncate(target_samples);
        }
        self.trim_timeline_to_front_frames(self.samples.len() / channels.max(1));
    }

    fn trim_timeline_to_front_frames(&mut self, mut frames: usize) {
        let mut trimmed = VecDeque::new();
        while frames > 0 {
            let Some(mut segment) = self.timeline.pop_front() else {
                break;
            };
            if segment.frames <= frames {
                frames -= segment.frames;
                trimmed.push_back(segment);
            } else {
                segment.frames = frames;
                trimmed.push_back(segment);
                break;
            }
        }
        self.timeline = trimmed;
    }

    fn push_timeline_segment(
        &mut self,
        start: Option<Duration>,
        frames: usize,
        media_frames_per_output_frame: f64,
    ) {
        if frames == 0 {
            return;
        }
        self.timeline.push_back(AudioTimelineSegment::new(
            start,
            frames,
            media_frames_per_output_frame,
        ));
    }

    fn advance_timeline(&mut self, mut frames: usize, sample_rate: u32) {
        while frames > 0 {
            let Some(front) = self.timeline.front_mut() else {
                break;
            };
            let consumed = frames.min(front.frames);
            if let Some(start) = front.start {
                self.last_media_time = offset_pts_scaled(
                    start,
                    consumed,
                    sample_rate,
                    front.media_frames_per_output_frame,
                );
                front.start = self.last_media_time;
            }
            front.frames -= consumed;
            frames -= consumed;
            if front.frames == 0 {
                let _ = self.timeline.pop_front();
            }
        }
    }
}

pub trait AudioOutputBackend {
    fn configure(&mut self, format: PcmFormat) -> Result<()>;
    fn start(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn set_volume(&mut self, volume: f32);
    fn volume(&self) -> f32;
    fn set_playback_rate(&mut self, _rate: f64) {}
    /// Returns false while the output owns enough queued PCM to preserve the
    /// bounded playback-rate transition latency. The presenter leaves decoded
    /// frames in the worker channel, which applies backpressure safely.
    fn can_accept_audio_frame(&self) -> bool {
        true
    }
    fn push(&mut self, frame: PcmAudioFrame) -> Result<AudioPushResult>;
    fn state(&self) -> AudioOutputState;
    fn stats(&self) -> AudioRingBufferStats;
    fn clock_snapshot(&self) -> Option<AudioClockSnapshot> {
        None
    }
    /// Duration of PCM already submitted to the platform output but not
    /// represented by `AudioClockSnapshot::queued_duration`.
    fn queued_output_duration(&self) -> Duration {
        Duration::ZERO
    }
    fn runtime_stats(&self) -> AudioOutputRuntimeStats {
        AudioOutputRuntimeStats::default()
    }
}

#[derive(Debug)]
pub struct BufferedAudioOutput {
    state: AudioOutputState,
    buffer: AudioRingBuffer,
    volume: f32,
}

impl BufferedAudioOutput {
    pub fn new(config: AudioRingBufferConfig) -> Self {
        Self {
            state: AudioOutputState::Stopped,
            buffer: AudioRingBuffer::new(config),
            volume: 1.0,
        }
    }

    pub fn buffer(&self) -> &AudioRingBuffer {
        &self.buffer
    }

    pub fn buffer_mut(&mut self) -> &mut AudioRingBuffer {
        &mut self.buffer
    }

    pub fn read_interleaved(&mut self, output: &mut [f32]) -> Result<AudioReadResult> {
        let result = self.buffer.read_interleaved(output)?;
        apply_volume(output, self.volume);
        Ok(result)
    }

    pub fn clock_snapshot(&self) -> AudioClockSnapshot {
        self.buffer.clock_snapshot()
    }
}

impl AudioOutputBackend for BufferedAudioOutput {
    fn configure(&mut self, format: PcmFormat) -> Result<()> {
        self.buffer.configure(format)
    }

    fn start(&mut self) -> Result<()> {
        self.state = AudioOutputState::Playing;
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.state = AudioOutputState::Paused;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.state = AudioOutputState::Stopped;
        self.buffer.clear();
        Ok(())
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = normalize_volume(volume);
    }

    fn volume(&self) -> f32 {
        self.volume
    }

    fn set_playback_rate(&mut self, rate: f64) {
        self.buffer.set_playback_rate(rate);
    }

    fn push(&mut self, frame: PcmAudioFrame) -> Result<AudioPushResult> {
        self.buffer.push_frame(frame)
    }

    fn state(&self) -> AudioOutputState {
        self.state
    }

    fn stats(&self) -> AudioRingBufferStats {
        self.buffer.stats()
    }

    fn clock_snapshot(&self) -> Option<AudioClockSnapshot> {
        Some(self.buffer.clock_snapshot())
    }
}

pub fn normalize_volume(volume: f32) -> f32 {
    if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

pub fn apply_volume(samples: &mut [f32], volume: f32) {
    let volume = normalize_volume(volume);
    if (volume - 1.0).abs() <= f32::EPSILON {
        return;
    }
    for sample in samples {
        *sample *= volume;
    }
}

/// Applies a linear per-frame gain ramp from `from` to `to` across `samples`,
/// returning the gain the ramp actually reached.
///
/// Volume changes are atomic steps on the control side; multiplying a whole
/// buffer by the new value produces an audible discontinuity (zipper noise).
/// Realtime read callbacks instead ramp from the gain they last applied toward
/// the current target, then carry the returned gain into the next callback.
/// All channels of a frame share one gain step and `from == to` degrades to a
/// constant [`apply_volume`].
///
/// `audible_frames` is how many leading frames actually carry audio. The ramp
/// is always *planned* across the full buffer so its slope does not depend on
/// how much the ring could supply, but a short read only advances part of that
/// plan and only that part is reported back. Ramping across a zero-filled
/// underflow tail and then recording `to` as applied would put the next
/// callback at a gain the audible samples never reached, recreating the very
/// discontinuity this exists to avoid.
pub fn apply_volume_ramp(
    samples: &mut [f32],
    channels: usize,
    from: f32,
    to: f32,
    audible_frames: usize,
) -> f32 {
    let from = normalize_volume(from);
    let to = normalize_volume(to);
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    if frames == 0 || from == to {
        apply_volume(samples, to);
        return to;
    }
    let audible_frames = audible_frames.min(frames);
    if audible_frames == 0 {
        return from;
    }
    let span = to - from;
    let frame_count = frames as f32;
    for (index, frame) in samples
        .chunks_exact_mut(channels)
        .take(audible_frames)
        .enumerate()
    {
        let gain = from + span * ((index + 1) as f32 / frame_count);
        for sample in frame {
            *sample *= gain;
        }
    }
    let reached = from + span * (audible_frames as f32 / frame_count);
    // A trailing partial frame cannot ramp; give it the gain we reached.
    apply_volume(&mut samples[frames * channels..], reached);
    reached
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::PcmSampleFormat;

    #[test]
    fn playback_rate_supports_short_form_video_acceleration() {
        assert_eq!(normalize_playback_rate(4.8), 4.8);
        assert_eq!(normalize_playback_rate(32.0), 16.0);
    }

    fn stereo_format() -> PcmFormat {
        PcmFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_format: PcmSampleFormat::F32Interleaved,
        }
    }

    fn frame(samples: Vec<f32>) -> PcmAudioFrame {
        PcmAudioFrame {
            format: stereo_format(),
            pts: None,
            frames: samples.len() / 2,
            samples,
        }
    }

    fn timed_frame(pts: Duration, frames: usize) -> PcmAudioFrame {
        PcmAudioFrame {
            format: stereo_format(),
            pts: Some(pts),
            frames,
            samples: vec![0.0; frames * 2],
        }
    }

    #[test]
    fn ring_buffer_reads_interleaved_samples_and_zero_fills_underflow() {
        let mut buffer = AudioRingBuffer::with_format(
            AudioRingBufferConfig {
                capacity_frames: 8,
                drop_oldest_on_overflow: true,
            },
            stereo_format(),
        )
        .unwrap();
        buffer.push_frame(frame(vec![0.1, 0.2, 0.3, 0.4])).unwrap();

        let mut output = [1.0; 6];
        let result = buffer.read_interleaved(&mut output).unwrap();

        assert_eq!(result.frames, 2);
        assert_eq!(result.underflow_frames, 1);
        assert_eq!(output, [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
    }

    #[test]
    fn ring_buffer_drops_oldest_frames_on_overflow() {
        let mut buffer = AudioRingBuffer::with_format(
            AudioRingBufferConfig {
                capacity_frames: 2,
                drop_oldest_on_overflow: true,
            },
            stereo_format(),
        )
        .unwrap();

        let result = buffer
            .push_frame(frame(vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6]))
            .unwrap();
        let mut output = [0.0; 4];
        buffer.read_interleaved(&mut output).unwrap();

        assert_eq!(result.accepted_frames, 2);
        assert_eq!(result.dropped_frames, 1);
        assert_eq!(output, [0.3, 0.4, 0.5, 0.6]);
    }

    #[test]
    fn buffered_audio_output_tracks_state() {
        let mut output = BufferedAudioOutput::new(AudioRingBufferConfig::default());

        output.configure(stereo_format()).unwrap();
        output.start().unwrap();
        assert_eq!(output.state(), AudioOutputState::Playing);
        output.pause().unwrap();
        assert_eq!(output.state(), AudioOutputState::Paused);
        output.stop().unwrap();
        assert_eq!(output.state(), AudioOutputState::Stopped);
    }

    #[test]
    fn volume_helpers_clamp_and_apply_gain() {
        assert_eq!(normalize_volume(1.0), 1.0);
        assert_eq!(normalize_volume(-1.0), 0.0);
        assert_eq!(normalize_volume(2.0), 1.0);
        assert_eq!(normalize_volume(f32::NAN), 1.0);

        let mut samples = [1.0, -0.5, 0.25, 0.0];
        apply_volume(&mut samples, 0.5);
        assert_eq!(samples, [0.5, -0.25, 0.125, 0.0]);
    }

    #[test]
    fn buffered_audio_output_volume_is_clamped() {
        let mut output = BufferedAudioOutput::new(AudioRingBufferConfig::default());

        assert_eq!(output.volume(), 1.0);
        output.set_volume(0.25);
        assert_eq!(output.volume(), 0.25);
        output.set_volume(-1.0);
        assert_eq!(output.volume(), 0.0);
        output.set_volume(f32::NAN);
        assert_eq!(output.volume(), 1.0);
    }

    #[test]
    fn output_queue_high_water_bounds_rate_transition_latency() {
        assert!(audio_output_queue_has_capacity(11_999, 48_000));
        assert!(!audio_output_queue_has_capacity(12_000, 48_000));
        assert!(audio_output_queue_has_capacity(11_024, 44_100));
        assert!(!audio_output_queue_has_capacity(11_025, 44_100));
        assert!(audio_output_queue_has_capacity(usize::MAX, 0));
    }

    #[test]
    fn volume_ramp_interpolates_per_frame_and_lands_on_target() {
        let mut samples = [1.0f32; 8];
        let reached = apply_volume_ramp(&mut samples, 2, 0.0, 1.0, 4);
        assert_eq!(samples, [0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0]);
        assert_eq!(reached, 1.0);
    }

    #[test]
    fn volume_ramp_with_equal_endpoints_is_constant_gain() {
        let mut samples = [1.0f32, -0.5, 0.25, 0.0];
        let reached = apply_volume_ramp(&mut samples, 2, 0.5, 0.5, 2);
        assert_eq!(samples, [0.5, -0.25, 0.125, 0.0]);
        assert_eq!(reached, 0.5);
    }

    #[test]
    fn volume_ramp_is_monotonic_and_clamps_invalid_endpoints() {
        let mut samples = [1.0f32; 32];
        let reached = apply_volume_ramp(&mut samples, 1, 2.0, -1.0, 32);

        for pair in samples.windows(2) {
            assert!(pair[1] <= pair[0], "ramp regressed: {pair:?}");
        }
        assert_eq!(samples[0], 1.0 - 1.0 / 32.0);
        assert_eq!(samples[31], 0.0);
        assert_eq!(reached, 0.0);
    }

    #[test]
    fn volume_ramp_stops_at_the_underflow_boundary() {
        // Four frames requested, one supplied: the audible frame takes the
        // first ramp step and the zero-filled tail is left alone, so the next
        // callback resumes from 0.875 rather than jumping straight to 0.5.
        let mut samples = [1.0f32, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let reached = apply_volume_ramp(&mut samples, 2, 1.0, 0.5, 1);

        assert_eq!(samples[0], 0.875);
        assert_eq!(samples[1], 0.875);
        assert_eq!(reached, 0.875);
        assert!(
            samples[2..].iter().all(|sample| *sample == 0.0),
            "underflow tail must stay silent: {samples:?}"
        );
    }

    #[test]
    fn volume_ramp_makes_no_progress_without_audible_frames() {
        let mut samples = [0.0f32; 8];
        let reached = apply_volume_ramp(&mut samples, 2, 1.0, 0.5, 0);
        assert_eq!(reached, 1.0);
        assert!(samples.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn volume_ramp_handles_empty_buffers_and_partial_frames() {
        let mut empty: [f32; 0] = [];
        assert_eq!(apply_volume_ramp(&mut empty, 2, 0.0, 1.0, 0), 1.0);

        // One full stereo frame plus a trailing partial frame: a single frame
        // lands directly on the target and the tail takes the same gain.
        let mut samples = [1.0f32, 1.0, 1.0];
        let reached = apply_volume_ramp(&mut samples, 2, 0.0, 0.5, 1);
        assert_eq!(samples, [0.5, 0.5, 0.5]);
        assert_eq!(reached, 0.5);
    }

    #[test]
    fn recovery_backoff_doubles_until_attempts_are_exhausted() {
        let mut backoff = RecoveryBackoff::new(Duration::from_millis(200), 5);

        let delays: Vec<_> = std::iter::from_fn(|| backoff.next_delay()).collect();

        assert_eq!(
            delays,
            [200, 400, 800, 1600, 3200].map(Duration::from_millis)
        );
        assert!(backoff.is_exhausted());
        assert_eq!(backoff.attempts(), 5);

        backoff.reset();
        assert!(!backoff.is_exhausted());
        assert_eq!(backoff.next_delay(), Some(Duration::from_millis(200)));
    }

    #[test]
    fn recovery_signals_track_disconnect_recover_cycle() {
        let signals = RecoverySignals::default();
        assert_eq!(
            signals.snapshot().recovery_state,
            AudioRecoveryState::Stable
        );

        let stats = signals.mark_disconnected(-77);
        assert_eq!(stats.recovery_state, AudioRecoveryState::Disconnected);
        assert_eq!(stats.last_error_code, -77);

        let stats = signals.begin_recovery();
        assert_eq!(stats.recovery_state, AudioRecoveryState::Recovering);
        assert_eq!(stats.recovery_attempts, 1);

        let stats = signals
            .recovery_succeeded()
            .expect("recovering commits to stable");
        assert_eq!(stats.recovery_state, AudioRecoveryState::Stable);
        assert_eq!(stats.recovery_count, 1);

        // A success must not commit when a fresh disconnect superseded it.
        signals.mark_disconnected(-78);
        assert!(signals.recovery_succeeded().is_none());
        assert_eq!(
            signals.snapshot().recovery_state,
            AudioRecoveryState::Disconnected
        );

        signals.begin_recovery();
        let stats = signals.recovery_failed(-79);
        assert_eq!(stats.recovery_state, AudioRecoveryState::Failed);
        assert_eq!(stats.recovery_failures, 1);
        assert_eq!(stats.last_error_code, -79);

        signals.reset();
        let stats = signals.snapshot();
        assert_eq!(stats.recovery_state, AudioRecoveryState::Stable);
        assert_eq!(stats.last_error_code, 0);
    }

    #[test]
    fn recovery_signals_snapshot_sequence_advances_per_transition() {
        let signals = RecoverySignals::default();
        let initial = signals.snapshot().transition_sequence;

        signals.mark_disconnected(-1);
        signals.begin_recovery();
        signals.recovery_succeeded();

        assert_eq!(signals.snapshot().transition_sequence, initial + 3);
    }

    #[test]
    fn ring_buffer_clock_snapshot_tracks_front_audio_pts() {
        let mut buffer = AudioRingBuffer::with_format(
            AudioRingBufferConfig {
                capacity_frames: 48_000,
                drop_oldest_on_overflow: true,
            },
            stereo_format(),
        )
        .unwrap();
        buffer
            .push_frame(timed_frame(Duration::from_secs(10), 480))
            .unwrap();

        assert_eq!(
            buffer.clock_snapshot().media_time,
            Some(Duration::from_secs(10))
        );

        let mut output = vec![0.0; 240 * 2];
        buffer.read_interleaved(&mut output).unwrap();

        assert_eq!(
            buffer.clock_snapshot().media_time,
            Some(Duration::from_millis(10_005))
        );
        assert_eq!(buffer.clock_snapshot().queued_frames, 240);
    }

    #[test]
    fn ring_buffer_fast_rate_preserves_pitch_and_media_timeline() {
        let mut buffer = AudioRingBuffer::with_format(
            AudioRingBufferConfig {
                capacity_frames: 48_000,
                drop_oldest_on_overflow: true,
            },
            stereo_format(),
        )
        .unwrap();
        buffer.set_playback_rate(2.0);
        for index in 0..8 {
            buffer
                .push_frame(timed_frame(
                    Duration::from_secs(10)
                        + Duration::from_secs_f64(index as f64 * 2048.0 / 48_000.0),
                    2048,
                ))
                .unwrap();
        }

        let queued_frames = buffer.clock_snapshot().queued_frames;
        assert!(queued_frames > 0);
        assert!(queued_frames < 16_384);

        let mut output = vec![0.0; queued_frames * 2];
        let result = buffer.read_interleaved(&mut output).unwrap();

        assert_eq!(result.frames, queued_frames);
        let media_time = buffer.clock_snapshot().media_time.unwrap();
        let expected = Duration::from_secs(10)
            + Duration::from_secs_f64(queued_frames as f64 * 2.0 / 48_000.0);
        assert!(duration_abs_diff(media_time, expected) < Duration::from_millis(2));
    }

    #[test]
    fn ring_buffer_rate_change_keeps_a_full_prefill_bridge() {
        let mut buffer = AudioRingBuffer::with_format(
            AudioRingBufferConfig {
                capacity_frames: 48_000,
                drop_oldest_on_overflow: true,
            },
            stereo_format(),
        )
        .unwrap();
        buffer
            .push_frame(timed_frame(Duration::from_secs(4), 24_000))
            .unwrap();

        buffer.set_playback_rate(2.0);

        assert_eq!(buffer.clock_snapshot().queued_frames, 12_000);
        assert_eq!(
            buffer.clock_snapshot().media_time,
            Some(Duration::from_secs(4))
        );

        let mut output = vec![0.0; 12_000 * 2];
        let result = buffer.read_interleaved(&mut output).unwrap();
        assert_eq!(result.frames, 12_000);
        assert_eq!(result.underflow_frames, 0);
    }

    #[test]
    fn ring_buffer_clock_survives_frame_drop() {
        let mut buffer = AudioRingBuffer::with_format(
            AudioRingBufferConfig {
                capacity_frames: 2,
                drop_oldest_on_overflow: true,
            },
            stereo_format(),
        )
        .unwrap();
        buffer
            .push_frame(timed_frame(Duration::from_secs(1), 4))
            .unwrap();

        assert_eq!(
            buffer.clock_snapshot().media_time,
            Some(Duration::from_nanos(1_000_041_667))
        );
    }

    fn duration_abs_diff(a: Duration, b: Duration) -> Duration {
        a.checked_sub(b)
            .or_else(|| b.checked_sub(a))
            .unwrap_or(Duration::ZERO)
    }
}
