// SPDX-License-Identifier: MPL-2.0
// The audio owner is independent of rendering. Only this thread creates,
// calls and drops the backend; platform audio callbacks keep their own model.
use super::PresenterAudioConfig;
#[cfg(target_os = "android")]
use crate::android::aaudio::{AAudioOutput, AAudioOutputConfig};
#[cfg(target_os = "macos")]
use crate::apple::coreaudio::{CoreAudioOutput, CoreAudioOutputConfig};
#[cfg(any(target_os = "ios", target_os = "tvos"))]
use crate::apple::iosaudio::{IosAudioQueueOutput, IosAudioQueueOutputConfig};
#[cfg(test)]
use crate::audio::AudioRingBufferConfig;
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
    AUDIO_OUTPUT_QUEUE_HIGH_WATER, AudioClockSnapshot, AudioOutputBackend, AudioOutputRuntimeStats,
    AudioOutputState, AudioRingBufferStats,
};
use crate::core::{
    AudioOutputEvent, Player, PlayerAudioFrame, PlayerState, pcm_audio_frame_playback_duration,
};
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
use crate::linux::PulseAudioOutput;
#[cfg(target_env = "ohos")]
use crate::ohos::ohaudio::{OHAudioOutput, OHAudioOutputConfig};

#[cfg(target_os = "windows")]
use crate::windows::wasapi::{WasapiAudioOutput, WasapiAudioOutputConfig};
use crate::{PlayerError, Result, trace};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
const AUDIO_START_BUFFER: Duration = Duration::from_millis(250);
// Bound each audio-owner slice by decoded PCM playback duration rather than raw
// frame count so short-frame codecs (such as 40-sample TrueHD) match the burst
// headroom of 1024-sample AAC (~341 ms for 16 frames) and can refill the start
// buffer, SoundTouch warmup, or buffering recovery in one slice while keeping
// CPU time bounded to AUDIO_PUMP_TIME_BUDGET.
const AUDIO_PUMP_DURATION_LIMIT: Duration = Duration::from_millis(350);
const AUDIO_PUMP_TIME_BUDGET: Duration = Duration::from_millis(4);
const PLAYBACK_RATE_EPSILON: f64 = 0.001;
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

const AUDIO_POLL_INTERVAL: Duration = Duration::from_millis(2);
const AUDIO_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
pub(super) struct AudioServiceSnapshot {
    pub clock: Option<AudioClockSnapshot>,
    pub runtime: AudioOutputRuntimeStats,
    pub state: AudioOutputState,
    pub configured: bool,
    pub started: bool,
    pub ring: AudioRingBufferStats,
    pub frames_queued: usize,
    pub frames_capacity: Option<usize>,
    pub playback_rate: f64,
    pub volume: f32,
    pub pushed_audio_frames: u64,
    pub audio_failures: u64,
    pub pump_duration: Duration,
}

#[derive(Debug)]
pub(super) enum AudioCommand {
    Quiesce,
    Resume,
    Drain,
    Reset,
    ResetCommitted,
    CommitPendingRate,
    Pause,
    SetRate(f64),
    SetVolume(f32),
    VideoPresented(u64),
    SetAudioOnly(bool),
}

struct Request {
    command: AudioCommand,
    reply: Sender<Result<()>>,
    deadline: Instant,
}

pub(super) struct AudioService {
    commands: Option<Sender<Request>>,
    snapshot: Arc<Mutex<AudioServiceSnapshot>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl AudioService {
    pub fn new(
        player: Player,
        frames: Receiver<PlayerAudioFrame>,
        config: PresenterAudioConfig,
    ) -> Result<Self> {
        Self::with_factory(player, frames, move || build_audio_output(config))
    }

    pub fn with_factory(
        player: Player,
        frames: Receiver<PlayerAudioFrame>,
        factory: impl FnOnce() -> Box<dyn AudioOutputBackend> + Send + 'static,
    ) -> Result<Self> {
        let (commands, requests) = bounded::<Request>(16);
        let (ready, initialized) = bounded(1);
        let worker = thread::Builder::new()
            .name("erika-audio".into())
            .spawn(move || {
                let mut pump = AudioPump::new(player, frames, factory());
                let shared = Arc::new(Mutex::new(pump.snapshot()));
                if ready.send(shared.clone()).is_err() {
                    return;
                }
                loop {
                    let request = if pump.quiesced {
                        requests.recv().map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected)
                    } else {
                        let interval = if pump.is_playing() || pump.audio_started {
                            AUDIO_POLL_INTERVAL
                        } else {
                            Duration::from_millis(100)
                        };
                        requests.recv_timeout(interval)
                    };
                    match request {
                        Ok(request) => {
                            if Instant::now() >= request.deadline {
                                let _ = request.reply.send(Err(PlayerError::Playback(
                                    "audio command expired before execution".into(),
                                )));
                                continue;
                            }
                            let action = format!("{:?}", request.command);
                            let result = pump.handle(request.command);
                            let snapshot = pump.snapshot();
                            *shared.lock().expect("audio snapshot mutex poisoned") = snapshot;
                            trace::log(format!(
                                "[erika-audio-owner] action={action} acknowledged={} generation={} quiesced={}",
                                result.is_ok(), pump.player.playback_generation(), pump.quiesced,
                            ));
                            let _ = request.reply.send(result);
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                    if !pump.quiesced {
                        let started = Instant::now();
                        if let Err(error) = pump.run_slice() {
                            pump.stats.audio_failures += 1;
                            pump.quiesced = true;
                            trace::diagnostic(format!("audio owner suspended after failure: {error}"));
                        }
                        pump.stats.pump_duration = started.elapsed();
                        let snapshot = pump.snapshot();
                        *shared.lock().expect("audio snapshot mutex poisoned") = snapshot;
                    }
                }
                // Device destruction stays on its creation thread, before join returns.
                let _ = pump.reset_audio_output();
            })
            .map_err(|error| PlayerError::Playback(format!("cannot start audio owner: {error}")))?;
        let snapshot = initialized
            .recv()
            .map_err(|_| PlayerError::Playback("audio owner failed to initialize".into()))?;
        Ok(Self {
            commands: Some(commands),
            snapshot,
            worker: Some(worker),
        })
    }

    pub fn command(&self, command: AudioCommand) -> Result<()> {
        let (reply, response) = bounded(1);
        let deadline = Instant::now() + AUDIO_COMMAND_TIMEOUT;
        self.commands
            .as_ref()
            .expect("command sender exists until drop")
            .send_timeout(
                Request {
                    command,
                    reply,
                    deadline,
                },
                AUDIO_COMMAND_TIMEOUT,
            )
            .map_err(|error| {
                PlayerError::Playback(format!("audio command send failed: {error}"))
            })?;
        response
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| {
                PlayerError::Playback(format!("audio command acknowledgement failed: {error}"))
            })?
    }

    pub fn snapshot(&self) -> AudioServiceSnapshot {
        *self.snapshot.lock().expect("audio snapshot mutex poisoned")
    }
}

impl Drop for AudioService {
    fn drop(&mut self) {
        // Disconnect before joining: even a full command queue must let the
        // owner exit once an in-flight backend call returns.
        drop(self.commands.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct AudioCounters {
    pushed_audio_frames: u64,
    audio_failures: u64,
    pump_duration: Duration,
}

struct AudioPump {
    player: Player,
    audio_frames: Receiver<PlayerAudioFrame>,
    audio_output: Box<dyn AudioOutputBackend>,
    audio_configured: bool,
    audio_started: bool,
    last_audio_clock_report: Option<AudioClockReportState>,
    last_audio_runtime_stats: AudioOutputRuntimeStats,
    playback_rate: f64,
    pending_playback_rate: Option<PendingPlaybackRate>,
    video_ready_generation: Option<u64>,
    audio_only: bool,
    quiesced: bool,
    stats: AudioCounters,
}

impl AudioPump {
    fn new(
        player: Player,
        audio_frames: Receiver<PlayerAudioFrame>,
        audio_output: Box<dyn AudioOutputBackend>,
    ) -> Self {
        Self {
            player,
            audio_frames,
            audio_output,
            audio_configured: false,
            audio_started: false,
            last_audio_clock_report: None,
            last_audio_runtime_stats: AudioOutputRuntimeStats::default(),
            playback_rate: 1.0,
            pending_playback_rate: None,
            video_ready_generation: None,
            audio_only: false,
            quiesced: true,
            stats: AudioCounters::default(),
        }
    }

    fn snapshot(&self) -> AudioServiceSnapshot {
        AudioServiceSnapshot {
            clock: self.audio_output.clock_snapshot(),
            runtime: self.audio_output.runtime_stats(),
            state: self.audio_output.state(),
            configured: self.audio_configured,
            started: self.audio_started,
            ring: self.audio_output.stats(),
            frames_queued: self.audio_frames.len(),
            frames_capacity: self.audio_frames.capacity(),
            playback_rate: self.playback_rate,
            volume: self.audio_output.volume(),
            pushed_audio_frames: self.stats.pushed_audio_frames,
            audio_failures: self.stats.audio_failures,
            pump_duration: self.stats.pump_duration,
        }
    }

    fn is_playing(&self) -> bool {
        self.player.state() == PlayerState::Playing
    }

    fn run_slice(&mut self) -> Result<()> {
        match self.player.state() {
            PlayerState::Closed | PlayerState::Error => {
                self.quiesced = true;
                return self.reset_audio_output_with_committed_rate();
            }
            PlayerState::Paused if self.audio_started => self.pause_output()?,
            _ => {}
        }
        self.commit_pending_playback_rate()?;
        self.report_audio_output_runtime_stats();
        self.pump_audio();
        self.report_audio_output_runtime_stats();
        // The worker owns EOF publication. Once all PCM has reached the device,
        // sleep until a replay/open/control command. Do not stop the backend
        // here: an empty ring does not prove its hardware tail has played.
        if self.at_natural_end()
            && self.audio_frames.is_empty()
            && self
                .audio_output
                .clock_snapshot()
                .is_none_or(|s| s.queued_frames == 0)
        {
            self.quiesced = true;
        }
        Ok(())
    }

    fn at_natural_end(&self) -> bool {
        // The worker may still publish Stopped while a replay command is
        // queued. Stay awake until the worker has processed that play intent.
        self.player.is_stopped_at_end()
            && self.player.playback_intent_state() == PlayerState::Stopped
    }

    fn pause_output(&mut self) -> Result<()> {
        if self.pending_playback_rate.is_some() {
            self.reset_audio_output()?;
            return self.commit_pending_playback_rate_now();
        }
        if let Err(error) = self.audio_output.pause() {
            self.stats.audio_failures += 1;
            trace::diagnostic_at(
                trace::LogLevel::Warn,
                format!("Erika presenter audio pause failed: {error}"),
            );
            return Err(PlayerError::Playback(error.to_string()));
        }
        self.audio_started = false;
        self.last_audio_clock_report = None;
        Ok(())
    }

    fn handle(&mut self, command: AudioCommand) -> Result<()> {
        match command {
            AudioCommand::Quiesce => {
                self.quiesced = true;
                self.player.invalidate_audio_clock();
            }
            AudioCommand::Resume => self.quiesced = false,
            AudioCommand::Drain => {
                if !self.quiesced {
                    return Err(PlayerError::Playback(
                        "audio drain requires quiesce acknowledgement".into(),
                    ));
                }
                while self.audio_frames.try_recv().is_ok() {}
            }
            AudioCommand::Reset => return self.reset_audio_output(),
            AudioCommand::ResetCommitted => return self.reset_audio_output_with_committed_rate(),
            AudioCommand::CommitPendingRate => return self.commit_pending_playback_rate_now(),
            AudioCommand::Pause => return self.pause_output(),
            AudioCommand::SetRate(rate) => return self.set_playback_rate(rate),
            AudioCommand::SetVolume(volume) => self.audio_output.set_volume(volume),
            AudioCommand::SetAudioOnly(audio_only) => self.audio_only = audio_only,
            AudioCommand::VideoPresented(generation) => {
                self.video_ready_generation = Some(generation)
            }
        }
        Ok(())
    }
    fn pump_audio(&mut self) {
        let started = Instant::now();
        let mut pumped_duration = Duration::ZERO;
        let rate = self
            .pending_playback_rate
            .map_or(self.playback_rate, |pending| pending.rate);
        loop {
            if pumped_duration >= AUDIO_PUMP_DURATION_LIMIT
                || started.elapsed() >= AUDIO_PUMP_TIME_BUDGET
            {
                break;
            }
            if !self.audio_output.can_accept_audio_frame()
                || self
                    .audio_output
                    .clock_snapshot()
                    .and_then(|s| s.queued_duration)
                    .is_some_and(|q| q >= AUDIO_OUTPUT_QUEUE_HIGH_WATER)
            {
                break;
            }
            match self.audio_frames.try_recv() {
                Ok(frame) => {
                    if frame.generation != self.player.playback_generation() {
                        continue;
                    }
                    let frame_duration = pcm_audio_frame_playback_duration(&frame.frame, rate);
                    self.push_audio(frame);
                    pumped_duration = pumped_duration.saturating_add(frame_duration);
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
        if self.audio_started {
            return;
        }
        let playing = self.is_playing();
        let ended = !playing && self.at_natural_end();
        // Once the worker has delivered the entire stream, a final short
        // buffer cannot grow to the normal startup threshold.
        if (!playing && !ended)
            || !self.audio_output_ready_to_start(ended && self.audio_frames.is_empty())
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

    fn audio_output_ready_to_start(&self, final_buffer: bool) -> bool {
        self.audio_output
            .clock_snapshot()
            .and_then(|snapshot| snapshot.queued_duration)
            .is_some_and(|queued| {
                queued >= AUDIO_START_BUFFER || (final_buffer && !queued.is_zero())
            })
    }

    fn audio_start_allowed(&self) -> bool {
        self.audio_only
            || self.player.track_selection().video.is_none()
            || self.video_ready_generation == Some(self.player.playback_generation())
    }

    fn reset_audio_output(&mut self) -> Result<()> {
        self.player.invalidate_audio_clock();
        if let Err(error) = self.audio_output.stop() {
            self.stats.audio_failures += 1;
            trace::diagnostic_at(
                trace::LogLevel::Warn,
                format!("Erika presenter audio reset failed: {error}"),
            );
            return Err(PlayerError::Playback(format!(
                "audio reset failed: {error}"
            )));
        }
        self.audio_configured = false;
        self.audio_started = false;
        self.last_audio_clock_report = None;
        Ok(())
    }

    fn reset_audio_output_with_committed_rate(&mut self) -> Result<()> {
        self.pending_playback_rate = None;
        self.reset_audio_output()?;
        self.audio_output.set_playback_rate(self.playback_rate);
        Ok(())
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
            self.reset_audio_output_with_committed_rate()?;
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
            self.reset_audio_output()?;
        }
        if let Err(error) = self.player.set_playback_rate(next_rate) {
            self.audio_output.set_playback_rate(self.playback_rate);
            return Err(error);
        }
        self.playback_rate = next_rate;
        self.pending_playback_rate = None;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlayerConfig, audio::BufferedAudioOutput};
    fn test_pump() -> AudioPump {
        let player = Player::new(PlayerConfig::default());
        let frames = player.subscribe_audio_frames();
        AudioPump::new(
            player,
            frames,
            Box::new(BufferedAudioOutput::new(AudioRingBufferConfig::default())),
        )
    }

    struct BlockingOutput {
        output: BufferedAudioOutput,
        entered: Sender<thread::ThreadId>,
        dropped_on: Option<Sender<thread::ThreadId>>,
        release: Receiver<()>,
        block_next: bool,
        fail_stop: bool,
        recover_on_push: bool,
    }

    impl Drop for BlockingOutput {
        fn drop(&mut self) {
            if let Some(dropped_on) = &self.dropped_on {
                let _ = dropped_on.send(thread::current().id());
            }
        }
    }

    impl AudioOutputBackend for BlockingOutput {
        fn configure(&mut self, format: crate::ffmpeg::PcmFormat) -> crate::audio::Result<()> {
            self.output.configure(format)
        }
        fn start(&mut self) -> crate::audio::Result<()> {
            self.output.start()
        }
        fn pause(&mut self) -> crate::audio::Result<()> {
            self.output.pause()
        }
        fn stop(&mut self) -> crate::audio::Result<()> {
            if std::mem::take(&mut self.fail_stop) {
                return Err(crate::audio::AudioError::Backend(
                    "injected stop failure".into(),
                ));
            }
            self.output.stop()
        }
        fn set_volume(&mut self, volume: f32) {
            self.output.set_volume(volume);
        }
        fn volume(&self) -> f32 {
            self.output.volume()
        }
        fn push(
            &mut self,
            frame: crate::ffmpeg::PcmAudioFrame,
        ) -> crate::audio::Result<crate::audio::AudioPushResult> {
            if self.block_next {
                self.block_next = false;
                self.entered.send(thread::current().id()).unwrap();
                self.release.recv().unwrap();
            }
            self.output.push(frame)
        }
        fn state(&self) -> crate::audio::AudioOutputState {
            self.output.state()
        }
        fn stats(&self) -> crate::audio::AudioRingBufferStats {
            self.output.stats()
        }
        fn clock_snapshot(&self) -> Option<AudioClockSnapshot> {
            Some(self.output.clock_snapshot())
        }
        fn runtime_stats(&self) -> AudioOutputRuntimeStats {
            if !self.recover_on_push {
                return AudioOutputRuntimeStats::default();
            }
            let recovered = self.output.stats().written_frames > 0;
            AudioOutputRuntimeStats {
                recovery_state: if recovered {
                    crate::audio::AudioRecoveryState::Stable
                } else {
                    crate::audio::AudioRecoveryState::Disconnected
                },
                transition_sequence: if recovered { 2 } else { 1 },
                recovery_count: u64::from(recovered),
                ..Default::default()
            }
        }
    }

    fn test_frame(generation: u64) -> PlayerAudioFrame {
        PlayerAudioFrame {
            generation,
            frame: crate::ffmpeg::PcmAudioFrame {
                format: crate::ffmpeg::PcmFormat::f32_interleaved(48000, 1),
                pts: Some(Duration::ZERO),
                frames: 480,
                samples: vec![0.0; 480],
            },
        }
    }

    #[test]
    fn quiesce_ack_waits_for_in_flight_push_and_fences_drain_and_reset() {
        let player = Player::new(PlayerConfig::default());
        let generation = player.playback_generation();
        let (frames, receiver) = bounded(4);
        let (entered, started) = bounded(1);
        let (release, resume) = bounded(1);
        let service = AudioService::with_factory(player, receiver, move || {
            Box::new(BlockingOutput {
                output: BufferedAudioOutput::new(AudioRingBufferConfig::default()),
                entered,
                dropped_on: None,
                release: resume,
                block_next: true,
                fail_stop: false,
                recover_on_push: false,
            })
        })
        .unwrap();
        frames.send(test_frame(generation)).unwrap();
        service.command(AudioCommand::Resume).unwrap();
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        thread::scope(|scope| {
            let (done, acknowledged) = bounded(1);
            let service = &service;
            scope.spawn(move || {
                done.send(service.command(AudioCommand::Quiesce)).unwrap();
            });
            assert!(
                acknowledged
                    .recv_timeout(Duration::from_millis(20))
                    .is_err()
            );
            release.send(()).unwrap();
            acknowledged
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
        });
        let pushes = service.snapshot().pushed_audio_frames;
        assert_eq!(pushes, 1);
        frames.send(test_frame(generation)).unwrap();
        service.command(AudioCommand::Drain).unwrap();
        service.command(AudioCommand::Reset).unwrap();
        service.command(AudioCommand::Resume).unwrap();
        service.command(AudioCommand::Quiesce).unwrap();
        assert_eq!(service.snapshot().pushed_audio_frames, pushes);
        assert_eq!(service.snapshot().clock.unwrap().queued_frames, 0);
    }

    #[test]
    fn drop_joins_after_a_stalled_push_even_with_a_full_command_queue() {
        let player = Player::new(PlayerConfig::default());
        let generation = player.playback_generation();
        let (frames, receiver) = bounded(1);
        let (entered, started) = bounded(1);
        let (release, resume) = bounded(1);
        let (dropped_on, device_dropped) = bounded(1);
        let service = AudioService::with_factory(player, receiver, move || {
            Box::new(BlockingOutput {
                output: BufferedAudioOutput::new(AudioRingBufferConfig::default()),
                entered,
                dropped_on: Some(dropped_on),
                release: resume,
                block_next: true,
                fail_stop: false,
                recover_on_push: false,
            })
        })
        .unwrap();
        frames.send(test_frame(generation)).unwrap();
        service.command(AudioCommand::Resume).unwrap();
        let owner_thread = started.recv_timeout(Duration::from_secs(1)).unwrap();

        // Simulate requests left queued by callers that have already timed out.
        let commands = service.commands.as_ref().unwrap();
        let (reply, _answers) = crossbeam_channel::unbounded();
        while !commands.is_full() {
            commands
                .try_send(Request {
                    command: AudioCommand::Resume,
                    reply: reply.clone(),
                    deadline: Instant::now(),
                })
                .unwrap();
        }

        let (dropping, drop_started) = bounded(0);
        let (done, dropped) = bounded(1);
        let worker = thread::spawn(move || {
            dropping.send(()).unwrap();
            drop(service);
            let _ = done.send(());
        });
        drop_started.recv().unwrap();
        assert!(
            dropped
                .recv_timeout(AUDIO_COMMAND_TIMEOUT + Duration::from_millis(100))
                .is_err()
        );
        release.send(()).unwrap();
        dropped.recv_timeout(Duration::from_secs(1)).expect(
            "dropping the last command sender must let the audio owner exit after push returns",
        );
        worker.join().unwrap();
        assert_eq!(device_dropped.try_recv().unwrap(), owner_thread);
    }

    #[test]
    fn expired_resume_cannot_reactivate_a_quiesced_owner() {
        let player = Player::new(PlayerConfig::default());
        let frames = player.subscribe_audio_frames();
        let service = AudioService::with_factory(player, frames, || {
            Box::new(BufferedAudioOutput::new(AudioRingBufferConfig::default()))
        })
        .unwrap();
        let (reply, result) = bounded(1);
        service
            .commands
            .as_ref()
            .unwrap()
            .send(Request {
                command: AudioCommand::Resume,
                reply,
                deadline: Instant::now() - Duration::from_millis(1),
            })
            .unwrap();
        assert!(
            result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        service
            .command(AudioCommand::Drain)
            .expect("expired resume must leave owner quiesced");
    }

    #[test]
    fn failed_device_reset_is_not_acknowledged_as_success() {
        let (entered, _) = bounded(1);
        let (_, release) = bounded(1);
        let mut pump = test_pump();
        pump.audio_output = Box::new(BlockingOutput {
            output: BufferedAudioOutput::new(AudioRingBufferConfig::default()),
            entered,
            dropped_on: None,
            release,
            block_next: false,
            fail_stop: true,
            recover_on_push: false,
        });
        pump.audio_configured = true;
        assert!(pump.handle(AudioCommand::Reset).is_err());
        assert!(pump.audio_configured);
        assert_eq!(pump.stats.audio_failures, 1);
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn close_releases_the_media_when_audio_reset_fails() {
        use crate::core::MediaRequest;
        use crate::presenter::{PresenterConfig, PresenterRuntime};
        let mut presenter = PresenterRuntime::new(PresenterConfig::default()).unwrap();
        presenter
            .open(MediaRequest::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/testdata/playback/playback-fixture.mkv"
            )))
            .unwrap();
        assert_eq!(presenter.player.state(), PlayerState::Ready);

        let (entered, _) = bounded(1);
        let (_, release) = bounded(1);
        presenter.audio = AudioService::with_factory(
            presenter.player.clone(),
            presenter.player.subscribe_audio_frames(),
            move || {
                Box::new(BlockingOutput {
                    output: BufferedAudioOutput::new(AudioRingBufferConfig::default()),
                    entered,
                    dropped_on: None,
                    release,
                    block_next: false,
                    fail_stop: true,
                    recover_on_push: false,
                })
            },
        )
        .unwrap();
        let result = presenter.close();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("injected stop failure")
        );
        assert_eq!(presenter.player.state(), PlayerState::Closed);
    }

    #[test]
    fn device_recovery_events_are_published_even_without_a_presenter_tick() {
        let (entered, _) = bounded(1);
        let (_, release) = bounded(1);
        let player = Player::new(PlayerConfig::default());
        let events = player.subscribe();
        let generation = player.playback_generation();
        let (sender, frames) = bounded(1);
        let mut pump = AudioPump::new(
            player,
            frames,
            Box::new(BlockingOutput {
                output: BufferedAudioOutput::new(AudioRingBufferConfig::default()),
                entered,
                dropped_on: None,
                release,
                block_next: false,
                fail_stop: false,
                recover_on_push: true,
            }),
        );
        sender.send(test_frame(generation)).unwrap();
        pump.run_slice().unwrap();
        let states: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                crate::core::PlayerEvent::AudioOutputChanged(event) => {
                    Some(event.stats.recovery_state)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![
                crate::audio::AudioRecoveryState::Disconnected,
                crate::audio::AudioRecoveryState::Stable
            ]
        );
        assert_eq!(pump.last_audio_runtime_stats.recovery_count, 1);
        assert_eq!(pump.player.state(), PlayerState::Idle);
    }

    #[test]
    fn output_ignores_pcm_from_an_old_generation() {
        let player = Player::new(PlayerConfig::default());
        let generation = player.playback_generation();
        let (sender, frames) = bounded(2);
        let mut pump = AudioPump::new(
            player,
            frames,
            Box::new(BufferedAudioOutput::new(AudioRingBufferConfig::default())),
        );
        sender.send(test_frame(generation - 1)).unwrap();
        sender.send(test_frame(generation)).unwrap();
        pump.pump_audio();
        assert_eq!(pump.stats.pushed_audio_frames, 1);
        assert_eq!(
            pump.audio_output.clock_snapshot().unwrap().queued_frames,
            480
        );
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

    #[test]
    fn audio_reset_preserves_pending_rate_for_transition() {
        let mut presenter = test_pump();
        presenter.pending_playback_rate = Some(PendingPlaybackRate {
            rate: 2.0,
            commit_at: Instant::now(),
        });

        presenter.reset_audio_output().unwrap();
        assert_eq!(
            presenter.pending_playback_rate.map(|pending| pending.rate),
            Some(2.0)
        );
    }

    #[test]
    fn terminal_audio_reset_restores_committed_rate() {
        use crate::audio::BufferedAudioOutput;
        use crate::ffmpeg::{PcmAudioFrame, PcmFormat};

        let mut presenter = test_pump();
        let mut output = BufferedAudioOutput::new(AudioRingBufferConfig::default());
        output.set_playback_rate(2.0);
        presenter.audio_output = Box::new(output);
        presenter.playback_rate = 1.0;
        presenter.pending_playback_rate = Some(PendingPlaybackRate {
            rate: 2.0,
            commit_at: Instant::now(),
        });

        presenter.reset_audio_output_with_committed_rate().unwrap();
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

    #[test]
    fn failed_pending_rate_commit_clears_transition_state() {
        let mut presenter = test_pump();
        presenter.pending_playback_rate = Some(PendingPlaybackRate {
            rate: 2.0,
            commit_at: Instant::now(),
        });

        assert!(presenter.commit_pending_playback_rate_now().is_err());
        assert!(presenter.pending_playback_rate.is_none());
        assert_eq!(presenter.playback_rate, 1.0);
    }

    #[test]
    fn audio_frame_pump_duration_uses_supported_rate_bounds() {
        let frame = crate::ffmpeg::PcmAudioFrame {
            format: crate::ffmpeg::PcmFormat::f32_interleaved(48_000, 2),
            pts: None,
            frames: 48_000,
            samples: vec![0.0; 96_000],
        };

        assert_eq!(
            pcm_audio_frame_playback_duration(&frame, f64::MIN_POSITIVE),
            Duration::from_secs(4)
        );
        assert_eq!(
            pcm_audio_frame_playback_duration(&frame, 64.0),
            Duration::from_secs_f64(1.0 / 16.0)
        );
        assert_eq!(
            pcm_audio_frame_playback_duration(&frame, f64::NAN),
            Duration::from_secs(1)
        );

        let zero_rate = crate::ffmpeg::PcmAudioFrame {
            format: crate::ffmpeg::PcmFormat::f32_interleaved(0, 2),
            pts: None,
            frames: 40,
            samples: Vec::new(),
        };
        assert_eq!(
            pcm_audio_frame_playback_duration(&zero_rate, 1.0),
            Duration::ZERO
        );

        let empty = crate::ffmpeg::PcmAudioFrame {
            format: crate::ffmpeg::PcmFormat::f32_interleaved(48_000, 2),
            pts: None,
            frames: 0,
            samples: Vec::new(),
        };
        assert_eq!(
            pcm_audio_frame_playback_duration(&empty, 1.0),
            Duration::ZERO
        );
    }

    #[test]
    fn audio_pump_duration_limit_reaches_start_buffer_for_short_and_common_frames() {
        for (sample_rate, pcm_frames, playback_rate) in [
            (48_000, 40, 1.0),    // TrueHD 48 kHz
            (48_000, 40, 2.0),    // TrueHD 48 kHz @ 2x
            (96_000, 80, 1.0),    // TrueHD 96 kHz
            (48_000, 1_024, 1.0), // AAC 48 kHz
            (44_100, 1_024, 1.0), // AAC 44.1 kHz
            (48_000, 960, 1.0),   // Opus 20 ms
            (48_000, 1_152, 1.0), // MP3 24 ms
            (48_000, 1_536, 1.0), // AC-3 32 ms
            (48_000, 1_024, 2.0), // AAC 48 kHz @ 2x
        ] {
            let frame = crate::ffmpeg::PcmAudioFrame {
                format: crate::ffmpeg::PcmFormat::f32_interleaved(sample_rate, 2),
                pts: None,
                frames: pcm_frames,
                samples: vec![0.0; pcm_frames * 2],
            };
            let frame_duration = pcm_audio_frame_playback_duration(&frame, playback_rate);
            let mut pumped_duration = Duration::ZERO;
            let mut pumped_frames = 0usize;
            while pumped_duration < AUDIO_PUMP_DURATION_LIMIT {
                pumped_duration = pumped_duration.saturating_add(frame_duration);
                pumped_frames += 1;
            }
            assert!(
                pumped_duration >= AUDIO_START_BUFFER,
                "{sample_rate} Hz ({pcm_frames} samples) at {playback_rate}x only pumped {pumped_duration:?}"
            );
            if pcm_frames == 40 {
                // Must dramatically exceed the legacy 16/24-frame limits.
                assert!(pumped_frames >= 300);
            }
        }
    }

    #[test]
    fn pump_audio_fills_start_buffer_with_short_truehd_frames_in_one_tick() {
        use crate::audio::BufferedAudioOutput;
        use crate::ffmpeg::{PcmAudioFrame, PcmFormat};

        let mut presenter = test_pump();
        presenter.audio_output =
            Box::new(BufferedAudioOutput::new(AudioRingBufferConfig::default()));
        let (tx, rx) = crossbeam_channel::bounded(600);
        presenter.audio_frames = rx;

        let format = PcmFormat::f32_interleaved(48_000, 2);
        let generation = presenter.player.playback_generation();
        // Queue 500 ms (600 x 40-sample frames) of TrueHD-sized PCM.
        for index in 0..600usize {
            tx.send(PlayerAudioFrame {
                frame: PcmAudioFrame {
                    format,
                    pts: Some(Duration::from_secs_f64((index * 40) as f64 / 48_000.0)),
                    frames: 40,
                    samples: vec![0.0; 80],
                },
                generation,
            })
            .unwrap();
        }

        presenter.pump_audio();
        // A single tick is bounded by AUDIO_PUMP_DURATION_LIMIT (350 ms = 420
        // 40-sample frames) and leaves remaining frames queued for later ticks.
        assert!(presenter.stats.pushed_audio_frames <= 421);
        assert!(!presenter.audio_frames.is_empty());

        // Under normal scheduling a single tick fills AUDIO_START_BUFFER; allow
        // up to 3 extra ticks so an unoptimized CI host preempted past
        // AUDIO_PUMP_TIME_BUDGET (4 ms) cannot flake (the legacy 16-frame cap
        // could pump at most 64 frames / 53 ms in 4 ticks).
        for _ in 0..3 {
            if presenter.audio_output_ready_to_start(false) {
                break;
            }
            presenter.pump_audio();
        }

        let queued = presenter
            .audio_output
            .clock_snapshot()
            .and_then(|snapshot| snapshot.queued_duration)
            .expect("queued audio duration");
        assert!(
            queued >= AUDIO_START_BUFFER,
            "expected at least {:?} queued, got {:?}",
            AUDIO_START_BUFFER,
            queued
        );
    }

    #[test]
    fn pump_audio_refills_buffer_at_fast_rate_with_soundtouch_warmup() {
        use crate::audio::BufferedAudioOutput;
        use crate::ffmpeg::{PcmAudioFrame, PcmFormat};

        let mut presenter = test_pump();
        let mut output = BufferedAudioOutput::new(AudioRingBufferConfig::default());
        output.set_playback_rate(2.0);
        presenter.audio_output = Box::new(output);
        presenter.playback_rate = 2.0;

        let (tx, rx) = crossbeam_channel::bounded(128);
        presenter.audio_frames = rx;
        let format = PcmFormat::f32_interleaved(48_000, 2);
        let generation = presenter.player.playback_generation();
        // Queue ~1.7 s of source PCM (~850 ms at 2x). Unoptimized debug builds
        // may hit AUDIO_PUMP_TIME_BUDGET (4 ms) inside SoundTouch, so allow a
        // few back-to-back ticks without waiting for wall-clock intervals.
        for index in 0..80usize {
            tx.send(PlayerAudioFrame {
                frame: PcmAudioFrame {
                    format,
                    pts: Some(Duration::from_secs_f64((index * 1_024) as f64 / 48_000.0)),
                    frames: 1_024,
                    samples: vec![0.0; 2_048],
                },
                generation,
            })
            .unwrap();
        }

        for _ in 0..8 {
            presenter.pump_audio();
            if presenter.audio_output_ready_to_start(false) {
                break;
            }
        }

        let queued = presenter
            .audio_output
            .clock_snapshot()
            .and_then(|snapshot| snapshot.queued_duration)
            .expect("queued audio duration at 2x");
        assert!(
            queued >= AUDIO_START_BUFFER,
            "expected SoundTouch 2x refill to reach {:?}, got {:?}",
            AUDIO_START_BUFFER,
            queued
        );
    }

    #[test]
    fn pump_audio_counts_oversized_and_failed_frames_against_the_budget() {
        use crate::audio::{
            AudioError, AudioOutputState, AudioRingBufferStats, BufferedAudioOutput,
        };
        use crate::ffmpeg::{PcmAudioFrame, PcmFormat};

        // 1. Oversized frame (> AUDIO_PUMP_DURATION_LIMIT) is admitted once per tick.
        let mut presenter = test_pump();
        presenter.audio_output =
            Box::new(BufferedAudioOutput::new(AudioRingBufferConfig::default()));
        let (tx, rx) = crossbeam_channel::bounded(4);
        presenter.audio_frames = rx;
        let format = PcmFormat::f32_interleaved(48_000, 2);
        let generation = presenter.player.playback_generation();
        for index in 0..2usize {
            tx.send(PlayerAudioFrame {
                frame: PcmAudioFrame {
                    format,
                    pts: Some(Duration::from_millis((index * 500) as u64)),
                    frames: 24_000,
                    samples: vec![0.0; 48_000],
                },
                generation,
            })
            .unwrap();
        }

        presenter.pump_audio();
        assert_eq!(presenter.stats.pushed_audio_frames, 1);
        assert_eq!(presenter.audio_frames.len(), 1);

        // A failing output drains two 200 ms frames before reaching the 350 ms limit.
        struct FailingAudioOutput;
        impl AudioOutputBackend for FailingAudioOutput {
            fn configure(&mut self, _format: PcmFormat) -> crate::audio::Result<()> {
                Ok(())
            }
            fn start(&mut self) -> crate::audio::Result<()> {
                Ok(())
            }
            fn pause(&mut self) -> crate::audio::Result<()> {
                Ok(())
            }
            fn stop(&mut self) -> crate::audio::Result<()> {
                Ok(())
            }
            fn set_volume(&mut self, _volume: f32) {}
            fn volume(&self) -> f32 {
                1.0
            }
            fn push(
                &mut self,
                _frame: PcmAudioFrame,
            ) -> crate::audio::Result<crate::audio::AudioPushResult> {
                Err(AudioError::Backend("simulated disconnect".to_string()))
            }
            fn state(&self) -> AudioOutputState {
                AudioOutputState::Stopped
            }
            fn stats(&self) -> AudioRingBufferStats {
                AudioRingBufferStats::default()
            }
        }

        let mut failing_presenter = test_pump();
        failing_presenter.audio_output = Box::new(FailingAudioOutput);
        let (tx, rx) = crossbeam_channel::bounded(4);
        failing_presenter.audio_frames = rx;
        let generation = failing_presenter.player.playback_generation();
        for _ in 0..3 {
            tx.send(PlayerAudioFrame {
                frame: PcmAudioFrame {
                    format,
                    pts: Some(Duration::ZERO),
                    frames: 9_600,
                    samples: vec![0.0; 19_200],
                },
                generation,
            })
            .unwrap();
        }

        failing_presenter.pump_audio();
        assert_eq!(failing_presenter.stats.audio_failures, 2);
        assert_eq!(failing_presenter.stats.pushed_audio_frames, 0);
        assert_eq!(failing_presenter.audio_frames.len(), 1);
    }

    #[test]
    fn audio_clock_report_tracks_queue_and_underflow_changes() {
        let mut presenter = test_pump();

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
    fn audio_start_is_blocked_while_player_is_not_playing() {
        use crate::audio::{AudioOutputState, BufferedAudioOutput};
        use crate::ffmpeg::{PcmAudioFrame, PcmFormat};

        let mut presenter = test_pump();
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
}
