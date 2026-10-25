//! Agent-facing diagnostics.
//!
//! * The process-wide log ring ([`export_logs`]), fed by every operational
//!   diagnostic, the operational `eprintln!` sites and FFmpeg's `av_log`
//!   (warnings and above).
//! * [`PlaybackTimingCounters`]: per-player scheduler, clock-discipline,
//!   buffering and queue-depth counters. The playback worker writes them with
//!   relaxed atomics; the presenter reads them without locking, so a
//!   diagnostics snapshot never waits on the worker.
//! * Presenter-thread presentation cadence (`PresentationStats`).
//!
//! Durations in JSON are milliseconds with microsecond precision. Values that
//! are not known are `null`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::core::{
    LumaUpscalerBackendStatus, RendererResourceStats, RendererRuntimeStats, VideoDecoderEvent,
};
use crate::ffmpeg::DecoderBackend;
use crate::playback::{
    ClockCorrection, ClockCorrectionDirection, PlaybackClockMode, PlaybackTimingConfig,
};
use crate::renderer::output::{OutputMode, OutputRuntimeStatus};
use crate::renderer::pipeline::LumaUpscalerMode;

pub use crate::trace::{
    FfmpegLogCapture, LogLevel, LogQuery, LogSource, export_logs, ffmpeg_log_capture,
};

/// Version of the diagnostics snapshot document. Fields are only added within
/// a version; removals or meaning changes bump it.
pub const DIAGNOSTICS_SCHEMA: u32 = 1;
/// Samples kept by each rolling window (late-by, drift, presentation cadence).
pub const TIMING_WINDOW: usize = 128;
/// Upper bounds (milliseconds) of the scheduler late-by histogram buckets; the
/// last bucket holds everything later than 120 ms.
pub const LATE_HISTOGRAM_BOUNDS_MS: [u64; 5] = [10, 20, 40, 80, 120];
const LATE_HISTOGRAM_LABELS: [&str; 6] =
    ["le10ms", "le20ms", "le40ms", "le80ms", "le120ms", "gt120ms"];
/// A presented frame counts as long when its on-screen interval exceeds the
/// source frame interval (scaled by playback rate) by this factor.
pub const LONG_FRAME_FACTOR: f64 = 1.5;

const RELAXED: Ordering = Ordering::Relaxed;

/// Microseconds since a process-wide base, offset by one so `0` can mean
/// "never" in atomics.
fn monotonic_micros() -> u64 {
    monotonic_micros_at(Instant::now())
}

fn monotonic_micros_at(now: Instant) -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = *BASE.get_or_init(Instant::now);
    duration_micros(now.saturating_duration_since(base)).saturating_add(1)
}

fn duration_micros(value: Duration) -> u64 {
    value.as_micros().min(u128::from(u64::MAX)) as u64
}

fn micros_to_ms(value: u64) -> f64 {
    round_ms(value as f64 / 1000.0)
}

fn round_ms(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn duration_ms(value: Duration) -> f64 {
    micros_to_ms(duration_micros(value))
}

fn ago_ms(at: u64, now: u64) -> Value {
    if at == 0 {
        Value::Null
    } else {
        json!(micros_to_ms(now.saturating_sub(at)))
    }
}

/// Summary of a rolling window of durations.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WindowStats {
    pub samples: usize,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

impl WindowStats {
    fn from_micros(mut values: Vec<u32>) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        values.sort_unstable();
        let samples = values.len();
        let sum: u64 = values.iter().map(|value| u64::from(*value)).sum();
        let percentile = |fraction: f64| {
            let rank = (fraction * samples as f64).ceil() as usize;
            u64::from(values[rank.clamp(1, samples) - 1])
        };
        Self {
            samples,
            mean_ms: round_ms(sum as f64 / samples as f64 / 1000.0),
            p50_ms: micros_to_ms(percentile(0.50)),
            p95_ms: micros_to_ms(percentile(0.95)),
            max_ms: micros_to_ms(u64::from(values[samples - 1])),
        }
    }

    pub fn to_json(&self) -> Value {
        if self.samples == 0 {
            return json!({
                "samples": 0, "meanMs": null, "p50Ms": null, "p95Ms": null, "maxMs": null,
            });
        }
        json!({
            "samples": self.samples,
            "meanMs": self.mean_ms,
            "p50Ms": self.p50_ms,
            "p95Ms": self.p95_ms,
            "maxMs": self.max_ms,
        })
    }
}

fn clamp_u32(value: u64) -> u32 {
    value.min(u64::from(u32::MAX)) as u32
}

/// Lock-free rolling window: the writer overwrites the oldest slot, readers
/// copy whatever is there. A reader racing a writer can see one sample from
/// the previous lap, which is acceptable for statistics.
#[derive(Debug)]
struct AtomicWindow {
    values: [AtomicU32; TIMING_WINDOW],
    next: AtomicU64,
}

impl AtomicWindow {
    fn new() -> Self {
        Self {
            values: std::array::from_fn(|_| AtomicU32::new(0)),
            next: AtomicU64::new(0),
        }
    }

    fn push(&self, micros: u64) {
        let slot = (self.next.fetch_add(1, RELAXED) % TIMING_WINDOW as u64) as usize;
        self.values[slot].store(clamp_u32(micros), RELAXED);
    }

    fn reset(&self) {
        self.next.store(0, RELAXED);
    }

    fn stats(&self) -> WindowStats {
        let filled = self.next.load(RELAXED).min(TIMING_WINDOW as u64) as usize;
        WindowStats::from_micros(
            self.values[..filled]
                .iter()
                .map(|value| value.load(RELAXED))
                .collect(),
        )
    }
}

/// Single-threaded rolling window used on the presenter thread.
#[derive(Debug, Clone)]
struct Window {
    values: [u32; TIMING_WINDOW],
    next: u64,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            values: [0; TIMING_WINDOW],
            next: 0,
        }
    }
}

impl Window {
    fn push(&mut self, micros: u64) {
        let slot = (self.next % TIMING_WINDOW as u64) as usize;
        self.values[slot] = clamp_u32(micros);
        self.next = self.next.saturating_add(1);
    }

    fn stats(&self) -> WindowStats {
        let filled = self.next.min(TIMING_WINDOW as u64) as usize;
        WindowStats::from_micros(self.values[..filled].to_vec())
    }
}

/// Queue depths published by the playback worker once per loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueDepths {
    pub demux_packets: usize,
    pub demux_packet_capacity: usize,
    pub pending_video_packets: usize,
    pub video_frames: usize,
    pub video_frame_limit: usize,
    pub audio_frames: usize,
    pub audio_frame_limit: usize,
    pub subtitle_frames: usize,
    pub subtitle_frame_limit: usize,
    pub demux_eof: bool,
}

/// Scheduler, clock-discipline, buffering and queue counters of one player.
///
/// Shared between the playback worker (the only writer) and readers on any
/// thread. All accesses are relaxed atomics: counters may be momentarily
/// inconsistent with each other, but reads never block playback. Counters
/// restart when the player opens new media.
#[derive(Debug)]
pub struct PlaybackTimingCounters {
    config_known: AtomicBool,
    clock_mode_audio: AtomicBool,
    lead_time_us: AtomicU64,
    drop_tolerance_us: AtomicU64,
    max_consecutive_drops: AtomicU64,
    audio_lead_time_us: AtomicU64,
    sync_enabled: AtomicBool,
    deadband_us: AtomicU64,
    max_correction_us: AtomicU64,
    snap_threshold_us: AtomicU64,

    presented_frames: AtomicU64,
    late_dropped_frames: AtomicU64,
    forced_late_presents: AtomicU64,
    max_drop_burst: AtomicU64,
    max_dropped_late_us: AtomicU64,
    last_drop_at: AtomicU64,
    seek_preroll_dropped_frames: AtomicU64,
    late_histogram: [AtomicU64; 6],
    late_window: AtomicWindow,

    clock_observations: AtomicU64,
    clock_corrections: AtomicU64,
    clock_snaps: AtomicU64,
    clock_forward: AtomicU64,
    clock_backward: AtomicU64,
    last_drift_us: AtomicI64,
    has_drift: AtomicBool,
    max_abs_drift_us: AtomicU64,
    applied_total_us: AtomicU64,
    last_correction_at: AtomicU64,
    last_snap_at: AtomicU64,
    drift_window: AtomicWindow,

    buffering_active: AtomicBool,
    buffering_events: AtomicU64,
    buffering_started_at: AtomicU64,
    buffering_total_us: AtomicU64,

    queues_updated_at: AtomicU64,
    demux_packets: AtomicU64,
    demux_packet_capacity: AtomicU64,
    pending_video_packets: AtomicU64,
    video_frames: AtomicU64,
    video_frame_limit: AtomicU64,
    audio_frames: AtomicU64,
    audio_frame_limit: AtomicU64,
    subtitle_frames: AtomicU64,
    subtitle_frame_limit: AtomicU64,
    demux_eof: AtomicBool,
}

impl Default for PlaybackTimingCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackTimingCounters {
    pub fn new() -> Self {
        Self {
            config_known: AtomicBool::new(false),
            clock_mode_audio: AtomicBool::new(false),
            lead_time_us: AtomicU64::new(0),
            drop_tolerance_us: AtomicU64::new(0),
            max_consecutive_drops: AtomicU64::new(0),
            audio_lead_time_us: AtomicU64::new(0),
            sync_enabled: AtomicBool::new(false),
            deadband_us: AtomicU64::new(0),
            max_correction_us: AtomicU64::new(0),
            snap_threshold_us: AtomicU64::new(0),
            presented_frames: AtomicU64::new(0),
            late_dropped_frames: AtomicU64::new(0),
            forced_late_presents: AtomicU64::new(0),
            max_drop_burst: AtomicU64::new(0),
            max_dropped_late_us: AtomicU64::new(0),
            last_drop_at: AtomicU64::new(0),
            seek_preroll_dropped_frames: AtomicU64::new(0),
            late_histogram: std::array::from_fn(|_| AtomicU64::new(0)),
            late_window: AtomicWindow::new(),
            clock_observations: AtomicU64::new(0),
            clock_corrections: AtomicU64::new(0),
            clock_snaps: AtomicU64::new(0),
            clock_forward: AtomicU64::new(0),
            clock_backward: AtomicU64::new(0),
            last_drift_us: AtomicI64::new(0),
            has_drift: AtomicBool::new(false),
            max_abs_drift_us: AtomicU64::new(0),
            applied_total_us: AtomicU64::new(0),
            last_correction_at: AtomicU64::new(0),
            last_snap_at: AtomicU64::new(0),
            drift_window: AtomicWindow::new(),
            buffering_active: AtomicBool::new(false),
            buffering_events: AtomicU64::new(0),
            buffering_started_at: AtomicU64::new(0),
            buffering_total_us: AtomicU64::new(0),
            queues_updated_at: AtomicU64::new(0),
            demux_packets: AtomicU64::new(0),
            demux_packet_capacity: AtomicU64::new(0),
            pending_video_packets: AtomicU64::new(0),
            video_frames: AtomicU64::new(0),
            video_frame_limit: AtomicU64::new(0),
            audio_frames: AtomicU64::new(0),
            audio_frame_limit: AtomicU64::new(0),
            subtitle_frames: AtomicU64::new(0),
            subtitle_frame_limit: AtomicU64::new(0),
            demux_eof: AtomicBool::new(false),
        }
    }

    /// Restarts every counter for new media and records the timing
    /// configuration the new playback engine uses.
    pub(crate) fn reset(&self, config: &PlaybackTimingConfig) {
        for counter in [
            &self.presented_frames,
            &self.late_dropped_frames,
            &self.forced_late_presents,
            &self.max_drop_burst,
            &self.max_dropped_late_us,
            &self.last_drop_at,
            &self.seek_preroll_dropped_frames,
            &self.clock_observations,
            &self.clock_corrections,
            &self.clock_snaps,
            &self.clock_forward,
            &self.clock_backward,
            &self.max_abs_drift_us,
            &self.applied_total_us,
            &self.last_correction_at,
            &self.last_snap_at,
            &self.buffering_events,
            &self.buffering_started_at,
            &self.buffering_total_us,
            &self.queues_updated_at,
            &self.demux_packets,
            &self.demux_packet_capacity,
            &self.pending_video_packets,
            &self.video_frames,
            &self.video_frame_limit,
            &self.audio_frames,
            &self.audio_frame_limit,
            &self.subtitle_frames,
            &self.subtitle_frame_limit,
        ] {
            counter.store(0, RELAXED);
        }
        for bucket in &self.late_histogram {
            bucket.store(0, RELAXED);
        }
        self.late_window.reset();
        self.drift_window.reset();
        self.last_drift_us.store(0, RELAXED);
        self.has_drift.store(false, RELAXED);
        self.buffering_active.store(false, RELAXED);
        self.demux_eof.store(false, RELAXED);
        self.set_config(config);
    }

    pub(crate) fn set_config(&self, config: &PlaybackTimingConfig) {
        self.clock_mode_audio
            .store(config.clock_mode == PlaybackClockMode::AudioMaster, RELAXED);
        self.lead_time_us
            .store(duration_micros(config.video_scheduler.lead_time), RELAXED);
        self.drop_tolerance_us.store(
            duration_micros(config.video_scheduler.drop_tolerance),
            RELAXED,
        );
        self.max_consecutive_drops
            .store(config.video_scheduler.max_consecutive_drops as u64, RELAXED);
        self.audio_lead_time_us
            .store(duration_micros(config.audio_lead_time), RELAXED);
        self.sync_enabled.store(config.audio_sync.enabled, RELAXED);
        self.deadband_us
            .store(duration_micros(config.audio_sync.deadband), RELAXED);
        self.max_correction_us.store(
            duration_micros(config.audio_sync.max_correction_per_frame),
            RELAXED,
        );
        self.snap_threshold_us
            .store(duration_micros(config.audio_sync.snap_threshold), RELAXED);
        self.config_known.store(true, RELAXED);
    }

    fn record_late_by(&self, late_by: Option<Duration>) {
        let late_us = late_by.map_or(0, duration_micros);
        let bucket = LATE_HISTOGRAM_BOUNDS_MS
            .iter()
            .position(|bound| late_us <= bound * 1000)
            .unwrap_or(LATE_HISTOGRAM_BOUNDS_MS.len());
        self.late_histogram[bucket].fetch_add(1, RELAXED);
        self.late_window.push(late_us);
    }

    /// A frame the scheduler released for presentation. `late_by` is how far
    /// the playback clock was past the frame's PTS (`None`: on time/early).
    pub(crate) fn record_present(&self, late_by: Option<Duration>) {
        self.presented_frames.fetch_add(1, RELAXED);
        self.record_late_by(late_by);
    }

    /// A frame later than the drop tolerance that was presented anyway
    /// because the consecutive-drop budget was spent.
    pub(crate) fn record_forced_late_present(&self, late_by: Duration) {
        self.forced_late_presents.fetch_add(1, RELAXED);
        self.record_present(Some(late_by));
    }

    /// A frame the scheduler discarded for being later than the drop
    /// tolerance. `burst` is the count of consecutive drops so far in this
    /// scheduling pass.
    pub(crate) fn record_late_drop(&self, late_by: Duration, burst: usize) {
        self.late_dropped_frames.fetch_add(1, RELAXED);
        self.max_drop_burst.fetch_max(burst as u64, RELAXED);
        self.max_dropped_late_us
            .fetch_max(duration_micros(late_by), RELAXED);
        self.last_drop_at.store(monotonic_micros(), RELAXED);
    }

    /// A decoded frame skipped while rolling forward to a seek target.
    pub(crate) fn record_seek_preroll_drop(&self) {
        self.seek_preroll_dropped_frames.fetch_add(1, RELAXED);
    }

    /// One audio-clock observation and the correction applied to the
    /// playback clock.
    pub(crate) fn record_clock_correction(&self, correction: &ClockCorrection) {
        self.clock_observations.fetch_add(1, RELAXED);
        let drift_us = duration_micros(correction.drift);
        let signed = match correction.direction {
            ClockCorrectionDirection::Backward => -(drift_us.min(i64::MAX as u64) as i64),
            _ => drift_us.min(i64::MAX as u64) as i64,
        };
        self.last_drift_us.store(signed, RELAXED);
        self.has_drift.store(true, RELAXED);
        self.max_abs_drift_us.fetch_max(drift_us, RELAXED);
        self.drift_window.push(drift_us);
        if correction.applied.is_zero() {
            return;
        }
        let now = monotonic_micros();
        self.applied_total_us
            .fetch_add(duration_micros(correction.applied), RELAXED);
        match correction.direction {
            ClockCorrectionDirection::Forward => {
                self.clock_forward.fetch_add(1, RELAXED);
            }
            ClockCorrectionDirection::Backward => {
                self.clock_backward.fetch_add(1, RELAXED);
            }
            ClockCorrectionDirection::None => {}
        }
        if correction.snapped {
            self.clock_snaps.fetch_add(1, RELAXED);
            self.last_snap_at.store(now, RELAXED);
        } else {
            self.clock_corrections.fetch_add(1, RELAXED);
            self.last_correction_at.store(now, RELAXED);
        }
    }

    /// Mirrors a `BufferingChanged` transition published by the worker.
    pub(crate) fn record_buffering(&self, active: bool) {
        let now = monotonic_micros();
        let was_active = self.buffering_active.swap(active, RELAXED);
        if active && !was_active {
            self.buffering_events.fetch_add(1, RELAXED);
            self.buffering_started_at.store(now, RELAXED);
        } else if !active && was_active {
            let started = self.buffering_started_at.load(RELAXED);
            if started != 0 {
                self.buffering_total_us
                    .fetch_add(now.saturating_sub(started), RELAXED);
            }
        }
    }

    pub(crate) fn publish_queues(&self, depths: QueueDepths) {
        self.demux_packets
            .store(depths.demux_packets as u64, RELAXED);
        self.demux_packet_capacity
            .store(depths.demux_packet_capacity as u64, RELAXED);
        self.pending_video_packets
            .store(depths.pending_video_packets as u64, RELAXED);
        self.video_frames.store(depths.video_frames as u64, RELAXED);
        self.video_frame_limit
            .store(depths.video_frame_limit as u64, RELAXED);
        self.audio_frames.store(depths.audio_frames as u64, RELAXED);
        self.audio_frame_limit
            .store(depths.audio_frame_limit as u64, RELAXED);
        self.subtitle_frames
            .store(depths.subtitle_frames as u64, RELAXED);
        self.subtitle_frame_limit
            .store(depths.subtitle_frame_limit as u64, RELAXED);
        self.demux_eof.store(depths.demux_eof, RELAXED);
        self.queues_updated_at.store(monotonic_micros(), RELAXED);
    }

    pub fn presented_frames(&self) -> u64 {
        self.presented_frames.load(RELAXED)
    }

    pub fn late_dropped_frames(&self) -> u64 {
        self.late_dropped_frames.load(RELAXED)
    }

    pub fn clock_corrections(&self) -> u64 {
        self.clock_corrections.load(RELAXED)
    }

    pub fn clock_snaps(&self) -> u64 {
        self.clock_snaps.load(RELAXED)
    }

    /// Scheduler, clock and buffering JSON (`timing.scheduler`,
    /// `timing.clock`, `playback.buffering`) plus queue depths (`buffers`).
    pub fn to_json(&self) -> Value {
        json!({
            "scheduler": self.scheduler_json(),
            "clock": self.clock_json(),
            "buffering": self.buffering_json(),
            "queues": self.queues_json(),
        })
    }

    fn config_value(&self, value: &AtomicU64) -> Value {
        if self.config_known.load(RELAXED) {
            json!(micros_to_ms(value.load(RELAXED)))
        } else {
            Value::Null
        }
    }

    pub fn scheduler_json(&self) -> Value {
        let now = monotonic_micros();
        let mut histogram = serde_json::Map::new();
        for (label, bucket) in LATE_HISTOGRAM_LABELS.iter().zip(&self.late_histogram) {
            histogram.insert((*label).into(), bucket.load(RELAXED).into());
        }
        let late_dropped = self.late_dropped_frames.load(RELAXED);
        json!({
            "leadTimeMs": self.config_value(&self.lead_time_us),
            "dropToleranceMs": self.config_value(&self.drop_tolerance_us),
            "maxConsecutiveDrops": self.config_known.load(RELAXED)
                .then(|| self.max_consecutive_drops.load(RELAXED)),
            "presentedFrames": self.presented_frames.load(RELAXED),
            "lateDroppedFrames": late_dropped,
            "forcedLatePresents": self.forced_late_presents.load(RELAXED),
            "maxDropBurst": self.max_drop_burst.load(RELAXED),
            "maxDroppedLateMs": (late_dropped > 0)
                .then(|| micros_to_ms(self.max_dropped_late_us.load(RELAXED))),
            "lastDropAgoMs": ago_ms(self.last_drop_at.load(RELAXED), now),
            "seekPrerollDroppedFrames": self.seek_preroll_dropped_frames.load(RELAXED),
            "lateByMs": self.late_window.stats().to_json(),
            "lateHistogram": histogram,
        })
    }

    pub fn clock_json(&self) -> Value {
        let now = monotonic_micros();
        let known = self.config_known.load(RELAXED);
        json!({
            "mode": known.then(|| if self.clock_mode_audio.load(RELAXED) { "audio_master" } else { "wall" }),
            "syncEnabled": known.then(|| self.sync_enabled.load(RELAXED)),
            "deadbandMs": self.config_value(&self.deadband_us),
            "maxCorrectionPerFrameMs": self.config_value(&self.max_correction_us),
            "snapThresholdMs": self.config_value(&self.snap_threshold_us),
            "audioLeadTimeMs": self.config_value(&self.audio_lead_time_us),
            "observations": self.clock_observations.load(RELAXED),
            "corrections": self.clock_corrections.load(RELAXED),
            "snaps": self.clock_snaps.load(RELAXED),
            "forwardCorrections": self.clock_forward.load(RELAXED),
            "backwardCorrections": self.clock_backward.load(RELAXED),
            "lastDriftMs": self.has_drift.load(RELAXED).then(|| {
                round_ms(self.last_drift_us.load(RELAXED) as f64 / 1000.0)
            }),
            "maxAbsDriftMs": self.has_drift.load(RELAXED)
                .then(|| micros_to_ms(self.max_abs_drift_us.load(RELAXED))),
            "appliedTotalMs": micros_to_ms(self.applied_total_us.load(RELAXED)),
            "lastCorrectionAgoMs": ago_ms(self.last_correction_at.load(RELAXED), now),
            "lastSnapAgoMs": ago_ms(self.last_snap_at.load(RELAXED), now),
            "absDriftMs": self.drift_window.stats().to_json(),
        })
    }

    pub fn buffering_json(&self) -> Value {
        let now = monotonic_micros();
        let active = self.buffering_active.load(RELAXED);
        let started = self.buffering_started_at.load(RELAXED);
        let current = (active && started != 0).then(|| now.saturating_sub(started));
        json!({
            "active": active,
            "events": self.buffering_events.load(RELAXED),
            "currentMs": current.map(micros_to_ms),
            "totalMs": micros_to_ms(
                self.buffering_total_us.load(RELAXED).saturating_add(current.unwrap_or(0))
            ),
        })
    }

    pub fn queues_json(&self) -> Value {
        let updated = self.queues_updated_at.load(RELAXED);
        if updated == 0 {
            return Value::Null;
        }
        let queue = |queued: &AtomicU64, capacity: &AtomicU64| json!({ "queued": queued.load(RELAXED), "capacity": capacity.load(RELAXED) });
        json!({
            "updatedAgoMs": micros_to_ms(monotonic_micros().saturating_sub(updated)),
            "demuxPackets": queue(&self.demux_packets, &self.demux_packet_capacity),
            "pendingVideoPackets": self.pending_video_packets.load(RELAXED),
            "decodedVideoFrames": queue(&self.video_frames, &self.video_frame_limit),
            "decodedAudioFrames": queue(&self.audio_frames, &self.audio_frame_limit),
            "decodedSubtitleFrames": queue(&self.subtitle_frames, &self.subtitle_frame_limit),
            "demuxEof": self.demux_eof.load(RELAXED),
        })
    }
}

/// Presentation cadence measured on the presenter thread.
///
/// `tickIntervalMs` is the spacing of the host's render ticks. A new video
/// frame is "presented" by the first successful render after it was uploaded;
/// `frameIntervalMs` is the spacing of those presentations and `jitterMs` the
/// absolute difference from the source frame spacing (PTS delta divided by the
/// playback rate). Frames uploaded and replaced before any render are
/// `supersededBeforePresent`. Erika only sees when it rendered, not the
/// display's actual vsync.
#[derive(Debug, Clone, Default)]
pub(crate) struct PresentationStats {
    last_tick: Option<Instant>,
    ticks: u64,
    tick_window: Window,
    pending_upload: Option<(Duration, u64)>,
    uploaded_frames: u64,
    superseded_uploads: u64,
    last_present: Option<(Instant, Duration, u64)>,
    new_frames_presented: u64,
    frame_interval_window: Window,
    jitter_window: Window,
    long_frames: u64,
    last_present_at: Option<Instant>,
}

/// Intervals above this are host pauses (backgrounded app, stopped display
/// link), not cadence.
const PRESENTATION_INTERVAL_LIMIT: Duration = Duration::from_secs(1);

impl PresentationStats {
    pub(crate) fn on_tick(&mut self, now: Instant) {
        if let Some(previous) = self.last_tick {
            let interval = now.saturating_duration_since(previous);
            if interval <= PRESENTATION_INTERVAL_LIMIT {
                self.tick_window.push(duration_micros(interval));
            }
        }
        self.last_tick = Some(now);
        self.ticks = self.ticks.saturating_add(1);
    }

    pub(crate) fn on_upload(&mut self, pts: Duration, generation: u64) {
        if self.pending_upload.is_some() {
            self.superseded_uploads = self.superseded_uploads.saturating_add(1);
        }
        self.pending_upload = Some((pts, generation));
        self.uploaded_frames = self.uploaded_frames.saturating_add(1);
    }

    pub(crate) fn on_render(
        &mut self,
        now: Instant,
        rendered_video: bool,
        playing: bool,
        rate: f64,
    ) {
        if !rendered_video {
            return;
        }
        let Some((pts, generation)) = self.pending_upload.take() else {
            return;
        };
        self.new_frames_presented = self.new_frames_presented.saturating_add(1);
        self.last_present_at = Some(now);
        if playing
            && let Some((previous_at, previous_pts, previous_generation)) = self.last_present
            && previous_generation == generation
            && pts > previous_pts
        {
            let interval = now.saturating_duration_since(previous_at);
            let pts_delta = pts - previous_pts;
            if interval <= PRESENTATION_INTERVAL_LIMIT && pts_delta <= PRESENTATION_INTERVAL_LIMIT {
                let rate = if rate.is_finite() && rate > 0.0 {
                    rate
                } else {
                    1.0
                };
                let expected = pts_delta.as_secs_f64() / rate;
                let interval_seconds = interval.as_secs_f64();
                self.frame_interval_window.push(duration_micros(interval));
                self.jitter_window
                    .push(((interval_seconds - expected).abs() * 1_000_000.0) as u64);
                if interval_seconds > expected * LONG_FRAME_FACTOR {
                    self.long_frames = self.long_frames.saturating_add(1);
                }
            }
        }
        self.last_present = playing.then_some((now, pts, generation));
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn to_json(&self, now: Instant) -> Value {
        json!({
            "ticks": self.ticks,
            "tickIntervalMs": self.tick_window.stats().to_json(),
            "uploadedFrames": self.uploaded_frames,
            "newFramesPresented": self.new_frames_presented,
            "supersededBeforePresent": self.superseded_uploads,
            "frameIntervalMs": self.frame_interval_window.stats().to_json(),
            "jitterMs": self.jitter_window.stats().to_json(),
            "longFrames": self.long_frames,
            "longFrameFactor": LONG_FRAME_FACTOR,
            "lastPresentAgoMs": self.last_present_at
                .map(|at| duration_ms(now.saturating_duration_since(at))),
        })
    }
}

pub(crate) fn output_mode_label(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Sdr => "sdr",
        OutputMode::AppleEdr { .. } => "apple_edr",
        OutputMode::ExtendedLinear { .. } => "extended_linear",
        OutputMode::Auto { .. } => "auto",
    }
}

pub(crate) fn upscaler_mode_label(mode: LumaUpscalerMode) -> &'static str {
    match mode {
        LumaUpscalerMode::Off => "off",
        LumaUpscalerMode::ArtCnnC4F16 => "artcnn_c4f16",
        LumaUpscalerMode::ArtCnnC4F16Ds => "artcnn_c4f16_ds",
        LumaUpscalerMode::ArtCnnC4F32 => "artcnn_c4f32",
    }
}

pub(crate) fn upscaler_backend_label(backend: LumaUpscalerBackendStatus) -> &'static str {
    match backend {
        LumaUpscalerBackendStatus::Off => "off",
        LumaUpscalerBackendStatus::Inactive => "inactive",
        LumaUpscalerBackendStatus::Building => "building",
        LumaUpscalerBackendStatus::Scalar => "scalar",
        LumaUpscalerBackendStatus::SimdgroupMatrix => "simdgroup_matrix",
    }
}

pub(crate) fn decoder_json(event: Option<&VideoDecoderEvent>) -> Value {
    let Some(event) = event else {
        return Value::Null;
    };
    json!({
        "requestedBackend": event.requested_backend.as_str(),
        "activeBackend": event.active_backend.as_str(),
        "previousBackend": event.previous_backend.map(DecoderBackend::as_str),
        "hardware": event.active_backend != DecoderBackend::Software,
        "fellBack": event.fallback_count > 0 || event.active_backend != event.requested_backend,
        "fallbackCount": event.fallback_count,
        "stage": event.stage.as_str(),
        "reason": event.reason.as_deref(),
        "codec": event.codec.as_deref(),
        "pixelFormat": event.pixel_format.as_deref(),
        "lineSizes": event.line_sizes,
    })
}

pub(crate) fn output_json(status: &OutputRuntimeStatus) -> Value {
    json!({
        "requestedMode": output_mode_label(status.requested_mode),
        "activeEncoding": status.active_encoding.label(),
        "surfaceFormat": status.surface_format.label(),
        "nativeDataSpace": status.native_data_space,
        "requestedHeadroom": status.requested_headroom,
        "activeHeadroom": status.active_headroom_known.then_some(status.active_headroom),
        "extendedLinearActive": status.extended_linear_active,
        "fallbackReason": status.fallback_reason.label(),
        "fallbackCount": status.fallback_count,
        "dataSpaceFailures": status.data_space_failures,
        "headroomUpdates": status.headroom_updates,
        "extendedLinearFrames": status.extended_linear_frames,
    })
}

pub(crate) fn hdr_json(renderer: &RendererRuntimeStats) -> Value {
    json!({
        "sourceFrames": renderer.hdr_source_frames,
        "hdr10OutputFrames": renderer.hdr10_output_frames,
        "sdrTonemapFrames": renderer.sdr_tonemap_frames,
        "metadataUpdates": renderer.hdr10_metadata_updates,
        "metadataFailures": renderer.hdr10_metadata_failures,
        "outputFailures": renderer.hdr10_output_failures,
        "outputActive": renderer.hdr10_output_active,
    })
}

pub(crate) fn resources_json(
    resources: &RendererResourceStats,
    cpu_danmaku_atlas_bytes: usize,
) -> Value {
    json!({
        "deviceCurrentAllocatedBytes": resources.device_current_allocated_bytes,
        "deviceRecommendedWorkingSetBytes": resources.device_recommended_working_set_bytes,
        "drawableEstimatedBytes": resources.drawable_estimated_bytes,
        "videoFrameBytes": resources.video_frame_bytes,
        "overlayAtlasBytes": resources.overlay_atlas_bytes,
        "danmakuAtlasBytes": resources.danmaku_atlas_bytes,
        "danmakuVertexBufferBytes": resources.danmaku_vertex_buffer_bytes,
        "upscalerBytes": resources.upscaler_bytes,
        "rendererTrackedBytes": resources.renderer_tracked_bytes,
        "presenterCpuDanmakuAtlasBytes": cpu_danmaku_atlas_bytes,
        "drawableCount": resources.drawable_count,
        "outputModeSwitches": resources.output_mode_switches,
    })
}

pub(crate) fn upscaler_json(renderer: &RendererRuntimeStats) -> Value {
    json!({
        "requestedMode": upscaler_mode_label(renderer.upscaler_mode),
        "activeBackend": upscaler_backend_label(renderer.upscaler_backend),
        "fallbackCount": renderer.upscaler_fallbacks,
        "upscaledFrames": renderer.upscaled_frames,
        "lastEncodeMs": duration_ms(renderer.last_upscaler_encode_duration),
    })
}

pub(crate) fn duration_ms_value(value: Duration) -> Value {
    json!(duration_ms(value))
}

pub(crate) fn optional_duration_ms(value: Option<Duration>) -> Value {
    value.map_or(Value::Null, duration_ms_value)
}

pub(crate) fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::{PlaybackClockSource, PlaybackTimingConfig};

    fn correction(
        direction: ClockCorrectionDirection,
        drift_ms: u64,
        applied_ms: u64,
        snapped: bool,
    ) -> ClockCorrection {
        ClockCorrection {
            source: PlaybackClockSource::Audio,
            direction,
            drift: Duration::from_millis(drift_ms),
            applied: Duration::from_millis(applied_ms),
            snapped,
        }
    }

    #[test]
    fn window_stats_use_nearest_rank_percentiles() {
        let stats = WindowStats::from_micros((1..=100).map(|value| value * 1000).collect());
        assert_eq!(stats.samples, 100);
        assert_eq!(stats.p50_ms, 50.0);
        assert_eq!(stats.p95_ms, 95.0);
        assert_eq!(stats.max_ms, 100.0);
        assert_eq!(stats.mean_ms, 50.5);
        assert_eq!(
            WindowStats::from_micros(Vec::new()).to_json()["meanMs"],
            Value::Null
        );
    }

    #[test]
    fn atomic_window_keeps_only_the_newest_samples() {
        let window = AtomicWindow::new();
        for value in 0..(TIMING_WINDOW as u64 + 20) {
            window.push(value * 1000);
        }
        let stats = window.stats();
        assert_eq!(stats.samples, TIMING_WINDOW);
        assert_eq!(stats.max_ms, (TIMING_WINDOW as u64 + 19) as f64);
        window.reset();
        assert_eq!(window.stats().samples, 0);
    }

    #[test]
    fn scheduler_counters_separate_late_drops_from_forced_presents() {
        let counters = PlaybackTimingCounters::new();
        counters.reset(&PlaybackTimingConfig::default());
        counters.record_present(None);
        counters.record_present(Some(Duration::from_millis(15)));
        counters.record_present(Some(Duration::from_millis(70)));
        counters.record_late_drop(Duration::from_millis(130), 1);
        counters.record_late_drop(Duration::from_millis(180), 2);
        counters.record_forced_late_present(Duration::from_millis(200));
        counters.record_seek_preroll_drop();

        let scheduler = counters.scheduler_json();
        assert_eq!(scheduler["presentedFrames"], 4);
        assert_eq!(scheduler["lateDroppedFrames"], 2);
        assert_eq!(scheduler["forcedLatePresents"], 1);
        assert_eq!(scheduler["maxDropBurst"], 2);
        assert_eq!(scheduler["maxDroppedLateMs"], 180.0);
        assert!(scheduler["lastDropAgoMs"].as_f64().is_some());
        assert_eq!(scheduler["seekPrerollDroppedFrames"], 1);
        assert_eq!(scheduler["dropToleranceMs"], 120.0);
        assert_eq!(scheduler["leadTimeMs"], 4.0);
        assert_eq!(scheduler["maxConsecutiveDrops"], 5);
        assert_eq!(scheduler["lateHistogram"]["le10ms"], 1);
        assert_eq!(scheduler["lateHistogram"]["le20ms"], 1);
        assert_eq!(scheduler["lateHistogram"]["le80ms"], 1);
        assert_eq!(scheduler["lateHistogram"]["gt120ms"], 1);
        assert_eq!(scheduler["lateByMs"]["samples"], 4);
        assert_eq!(scheduler["lateByMs"]["maxMs"], 200.0);

        counters.reset(&PlaybackTimingConfig::default());
        let scheduler = counters.scheduler_json();
        assert_eq!(scheduler["presentedFrames"], 0);
        assert_eq!(scheduler["lastDropAgoMs"], Value::Null);
        assert_eq!(scheduler["maxDroppedLateMs"], Value::Null);
        assert_eq!(scheduler["lateByMs"]["samples"], 0);
    }

    #[test]
    fn clock_counters_split_corrections_snaps_and_deadband() {
        let counters = PlaybackTimingCounters::new();
        let clock = counters.clock_json();
        assert_eq!(clock["mode"], Value::Null);
        assert_eq!(clock["lastDriftMs"], Value::Null);
        counters.reset(&PlaybackTimingConfig::default());
        counters.record_clock_correction(&correction(
            ClockCorrectionDirection::Forward,
            3,
            0,
            false,
        ));
        counters.record_clock_correction(&correction(
            ClockCorrectionDirection::Forward,
            12,
            5,
            false,
        ));
        counters.record_clock_correction(&correction(
            ClockCorrectionDirection::Backward,
            300,
            300,
            true,
        ));
        counters.record_clock_correction(&correction(
            ClockCorrectionDirection::Backward,
            8,
            5,
            false,
        ));

        let clock = counters.clock_json();
        assert_eq!(clock["mode"], "audio_master");
        assert_eq!(clock["observations"], 4);
        assert_eq!(clock["corrections"], 2);
        assert_eq!(clock["snaps"], 1);
        assert_eq!(clock["forwardCorrections"], 1);
        assert_eq!(clock["backwardCorrections"], 2);
        assert_eq!(clock["lastDriftMs"], -8.0);
        assert_eq!(clock["maxAbsDriftMs"], 300.0);
        assert_eq!(clock["appliedTotalMs"], 310.0);
        assert_eq!(clock["deadbandMs"], 5.0);
        assert_eq!(clock["snapThresholdMs"], 250.0);
        assert!(clock["lastSnapAgoMs"].as_f64().is_some());
        assert!(clock["lastCorrectionAgoMs"].as_f64().is_some());
        assert_eq!(clock["absDriftMs"]["samples"], 4);
        assert_eq!(counters.clock_corrections(), 2);
        assert_eq!(counters.clock_snaps(), 1);
    }

    #[test]
    fn buffering_counters_accumulate_episodes() {
        let counters = PlaybackTimingCounters::new();
        counters.record_buffering(true);
        counters.record_buffering(true);
        let active = counters.buffering_json();
        assert_eq!(active["active"], true);
        assert_eq!(active["events"], 1);
        assert!(active["currentMs"].as_f64().is_some());
        counters.record_buffering(false);
        counters.record_buffering(false);
        let idle = counters.buffering_json();
        assert_eq!(idle["active"], false);
        assert_eq!(idle["events"], 1);
        assert_eq!(idle["currentMs"], Value::Null);
    }

    #[test]
    fn queue_depths_are_null_until_published() {
        let counters = PlaybackTimingCounters::new();
        assert_eq!(counters.queues_json(), Value::Null);
        counters.publish_queues(QueueDepths {
            demux_packets: 40,
            demux_packet_capacity: 512,
            pending_video_packets: 2,
            video_frames: 3,
            video_frame_limit: 8,
            audio_frames: 10,
            audio_frame_limit: 64,
            subtitle_frames: 0,
            subtitle_frame_limit: 32,
            demux_eof: false,
        });
        let queues = counters.queues_json();
        assert_eq!(queues["demuxPackets"]["queued"], 40);
        assert_eq!(queues["demuxPackets"]["capacity"], 512);
        assert_eq!(queues["decodedVideoFrames"]["capacity"], 8);
        assert_eq!(queues["pendingVideoPackets"], 2);
        assert_eq!(queues["demuxEof"], false);
    }

    #[test]
    fn counters_accept_concurrent_writers_and_readers() {
        let counters = std::sync::Arc::new(PlaybackTimingCounters::new());
        counters.reset(&PlaybackTimingConfig::default());
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let counters = std::sync::Arc::clone(&counters);
                std::thread::spawn(move || {
                    for index in 0..1000u64 {
                        counters.record_present(Some(Duration::from_micros(index)));
                        if index % 10 == 0 {
                            counters.record_late_drop(Duration::from_millis(150), 1);
                        }
                    }
                })
            })
            .collect();
        let reader = {
            let counters = std::sync::Arc::clone(&counters);
            std::thread::spawn(move || {
                for _ in 0..100 {
                    let json = counters.to_json();
                    assert!(
                        json["scheduler"]["lateByMs"]["samples"].as_u64().unwrap()
                            <= TIMING_WINDOW as u64
                    );
                }
            })
        };
        for writer in writers {
            writer.join().unwrap();
        }
        reader.join().unwrap();
        assert_eq!(counters.presented_frames(), 4000);
        assert_eq!(counters.late_dropped_frames(), 400);
    }

    #[test]
    fn presentation_stats_measure_new_frame_cadence_and_long_frames() {
        let start = Instant::now();
        let mut stats = PresentationStats::default();
        let frame = Duration::from_micros(41_708);
        // Four frames on time at ~24 fps, then one that stays on screen for
        // three source intervals.
        let mut at = start;
        for index in 0..4u32 {
            stats.on_tick(at);
            stats.on_upload(frame * index, 1);
            stats.on_render(at, true, true, 1.0);
            // A re-render of the same frame is not a new presentation.
            stats.on_render(at + Duration::from_millis(16), true, true, 1.0);
            at += frame;
        }
        at += frame * 2;
        stats.on_upload(frame * 4, 1);
        stats.on_render(at, true, true, 1.0);
        // Superseded upload: two uploads before one render.
        stats.on_upload(frame * 5, 1);
        stats.on_upload(frame * 6, 1);
        stats.on_render(at + frame * 2, true, true, 1.0);

        let json = stats.to_json(at + frame * 2);
        assert_eq!(json["newFramesPresented"], 6);
        assert_eq!(json["uploadedFrames"], 7);
        assert_eq!(json["supersededBeforePresent"], 1);
        assert_eq!(json["longFrames"], 1);
        assert_eq!(json["frameIntervalMs"]["samples"], 5);
        assert!(json["jitterMs"]["maxMs"].as_f64().unwrap() > 80.0);
        assert!(json["jitterMs"]["p50Ms"].as_f64().unwrap() < 1.0);
        assert_eq!(json["ticks"], 4);
        assert_eq!(json["lastPresentAgoMs"], 0.0);
    }

    #[test]
    fn presentation_stats_ignore_pauses_seeks_and_rate_scaled_cadence() {
        let start = Instant::now();
        let mut stats = PresentationStats::default();
        let frame = Duration::from_millis(40);
        stats.on_upload(Duration::ZERO, 1);
        stats.on_render(start, true, true, 2.0);
        // At 2x the expected on-screen interval is 20 ms.
        stats.on_upload(frame, 1);
        stats.on_render(start + Duration::from_millis(20), true, true, 2.0);
        // Paused preview frame: no interval.
        stats.on_upload(frame * 2, 1);
        stats.on_render(start + Duration::from_millis(500), true, false, 2.0);
        stats.on_upload(frame * 3, 1);
        stats.on_render(start + Duration::from_millis(520), true, true, 2.0);
        // Seek: a new generation never forms an interval with the old one.
        stats.on_upload(Duration::from_secs(60), 2);
        stats.on_render(start + Duration::from_millis(540), true, true, 1.0);
        let json = stats.to_json(start + Duration::from_millis(540));
        assert_eq!(json["frameIntervalMs"]["samples"], 1);
        assert_eq!(json["longFrames"], 0);
        assert_eq!(json["newFramesPresented"], 5);
        stats.reset();
        assert_eq!(stats.to_json(start)["newFramesPresented"], 0);
    }
}
