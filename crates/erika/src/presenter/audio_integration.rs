// SPDX-License-Identifier: MPL-2.0
// Real FFmpeg/Player/Presenter/audio-owner integration. Only rendering and the
// device callback are simulated; there is no test-only PCM feeder.
use super::*;
use crate::audio::{
    AudioClockSnapshot, AudioOutputBackend, AudioOutputState, AudioPushResult,
    AudioRingBufferStats, BufferedAudioOutput,
};
use crate::ffmpeg::{PcmAudioFrame, PcmFormat};
use crate::playback::VideoDecodePreference;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct Output {
    ring: Arc<Mutex<BufferedAudioOutput>>,
    owner: thread::ThreadId,
}

impl Output {
    fn check_owner(&self) {
        assert_eq!(self.owner, thread::current().id());
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.check_owner();
    }
}

impl AudioOutputBackend for Output {
    fn configure(&mut self, format: PcmFormat) -> crate::audio::Result<()> {
        self.check_owner();
        self.ring.lock().unwrap().configure(format)
    }
    fn start(&mut self) -> crate::audio::Result<()> {
        self.check_owner();
        self.ring.lock().unwrap().start()
    }
    fn pause(&mut self) -> crate::audio::Result<()> {
        self.check_owner();
        self.ring.lock().unwrap().pause()
    }
    fn stop(&mut self) -> crate::audio::Result<()> {
        self.check_owner();
        self.ring.lock().unwrap().stop()
    }
    fn set_volume(&mut self, value: f32) {
        self.check_owner();
        self.ring.lock().unwrap().set_volume(value);
    }
    fn volume(&self) -> f32 {
        self.check_owner();
        self.ring.lock().unwrap().volume()
    }
    fn set_playback_rate(&mut self, value: f64) {
        self.check_owner();
        self.ring.lock().unwrap().set_playback_rate(value);
    }
    fn push(&mut self, frame: PcmAudioFrame) -> crate::audio::Result<AudioPushResult> {
        self.check_owner();
        self.ring.lock().unwrap().push(frame)
    }
    fn state(&self) -> AudioOutputState {
        self.check_owner();
        self.ring.lock().unwrap().state()
    }
    fn stats(&self) -> AudioRingBufferStats {
        self.check_owner();
        self.ring.lock().unwrap().stats()
    }
    fn clock_snapshot(&self) -> Option<AudioClockSnapshot> {
        self.check_owner();
        Some(self.ring.lock().unwrap().clock_snapshot())
    }
}

struct Renderer {
    stall_ms: Arc<AtomicU64>,
    uploaded: bool,
}

impl RendererBackend for Renderer {
    fn attach_surface(&mut self, _: PlatformSurface) -> Result<()> {
        Ok(())
    }
    fn detach_surface(&mut self) -> Result<()> {
        Ok(())
    }
    fn resize_surface(&mut self, _: SurfaceMetrics) -> Result<()> {
        Ok(())
    }
    fn render_test_frame(&mut self, _: f64) -> Result<()> {
        Ok(())
    }
    fn upload_player_frame(&mut self, _: &PlayerVideoFrame) -> Result<()> {
        self.uploaded = true;
        Ok(())
    }
    fn render_current_frame(&mut self, _: RenderFrameContext<'_>) -> Result<bool> {
        let millis = self.stall_ms.swap(0, Ordering::SeqCst);
        if millis > 0 {
            thread::sleep(Duration::from_millis(millis));
        }
        Ok(self.uploaded)
    }

    fn runtime_stats(&self) -> RendererRuntimeStats {
        RendererRuntimeStats {
            attached: true,
            surface_width: 64,
            surface_height: 36,
            ..Default::default()
        }
    }
}

struct Harness {
    presenter: PresenterRuntime,
    ring: Arc<Mutex<BufferedAudioOutput>>,
    stall_ms: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
    callback: Option<thread::JoinHandle<()>>,
}

impl Harness {
    fn new() -> Self {
        let mut config = PresenterConfig::default();
        config.player.playback.video_decode = VideoDecodePreference::Software;
        let mut presenter = PresenterRuntime::new(config).unwrap();
        let ring = Arc::new(Mutex::new(BufferedAudioOutput::new(
            AudioRingBufferConfig::default(),
        )));
        let output_ring = ring.clone();
        presenter.audio = AudioService::with_factory(
            presenter.player.clone(),
            presenter.player.subscribe_audio_frames(),
            move || {
                Box::new(Output {
                    ring: output_ring,
                    owner: thread::current().id(),
                })
            },
        )
        .unwrap();
        let stall_ms = Arc::new(AtomicU64::new(0));
        presenter.renderer = Box::new(Renderer {
            stall_ms: stall_ms.clone(),
            uploaded: false,
        });
        presenter.current_surface_metrics = Some(SurfaceMetrics::new(64, 36, 1.0));
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/playback/playback-fixture.mkv");
        presenter
            .open(MediaRequest::new(fixture.to_string_lossy()))
            .unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let callback_ring = ring.clone();
        let callback_done = done.clone();
        let callback = thread::spawn(move || {
            let mut last = Instant::now();
            let mut fractional = 0.0;
            while !callback_done.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(2));
                let now = Instant::now();
                let elapsed = now.duration_since(last);
                last = now;
                let mut output = callback_ring.lock().unwrap();
                if output.state() == AudioOutputState::Playing {
                    let format = output.buffer().format().unwrap();
                    let exact = elapsed.as_secs_f64() * format.sample_rate as f64 + fractional;
                    let frames = exact.floor() as usize;
                    fractional = exact - frames as f64;
                    output
                        .read_interleaved(&mut vec![0.0; frames * format.channels as usize])
                        .unwrap();
                } else {
                    fractional = 0.0;
                }
            }
        });
        Self {
            presenter,
            ring,
            stall_ms,
            done,
            callback: Some(callback),
        }
    }

    fn tick_for(&mut self, duration: Duration) {
        let started = Instant::now();
        while started.elapsed() < duration {
            self.presenter
                .render_tick(started.elapsed().as_secs_f64())
                .unwrap();
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn snapshot(&self) -> AudioClockSnapshot {
        self.ring.lock().unwrap().clock_snapshot()
    }

    fn wait_for_background_audio(&self) -> AudioClockSnapshot {
        let initial = self.snapshot().read_frames;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let current = self.snapshot();
            if current.read_frames >= initial + 4800 {
                assert_eq!(self.ring.lock().unwrap().state(), AudioOutputState::Playing);
                return current;
            }
            assert!(
                Instant::now() < deadline,
                "device did not consume PCM without display ticks: {current:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn start(&mut self) {
        self.presenter.play().unwrap();
        self.tick_for(Duration::from_millis(1000));
        assert_eq!(self.ring.lock().unwrap().state(), AudioOutputState::Playing);
        assert!(self.snapshot().read_frames > 0);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.presenter.close();
        self.done.store(true, Ordering::SeqCst);
        if let Some(callback) = self.callback.take() {
            let _ = callback.join();
        }
    }
}

#[test]
fn render_stalls_do_not_starve_the_production_audio_owner() {
    for stall_ms in [733, 1600] {
        let mut h = Harness::new();
        h.start();
        let before = h.snapshot();
        assert!(before.queued_duration.unwrap() < Duration::from_millis(300));
        h.stall_ms.store(stall_ms, Ordering::SeqCst);
        let started = Instant::now();
        h.presenter.render_tick(1.0).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(stall_ms));
        let after = h.snapshot();
        let underflow = Duration::from_secs_f64(
            (after.underflow_frames - before.underflow_frames) as f64 / 48000.0,
        );
        let gap = h
            .presenter
            .player
            .current_media_time()
            .abs_diff(after.media_time.unwrap());
        eprintln!(
            "audio-owner stall={stall_ms}ms queued_before={:?} underflow={underflow:?} clock_gap={gap:?}",
            before.queued_duration
        );
        assert!(underflow < Duration::from_millis(30));
        assert!(after.read_frames > before.read_frames + stall_ms * 40);
        assert!(gap < Duration::from_millis(40));
        h.tick_for(Duration::from_millis(100));
    }
}

#[test]
fn rate_commit_and_audio_progress_do_not_wait_for_rendering() {
    let mut h = Harness::new();
    h.start();
    h.presenter.set_playback_rate(2.0).unwrap();
    let before = h.snapshot();
    h.stall_ms.store(1600, Ordering::SeqCst);
    h.presenter.render_tick(1.0).unwrap();
    let after = h.snapshot();
    assert_eq!(h.presenter.audio.snapshot().playback_rate, 2.0);
    assert_eq!(h.presenter.player.playback_snapshot().clock.rate(), 2.0);
    assert!(after.media_time.unwrap() - before.media_time.unwrap() > Duration::from_secs(2));
    assert!(after.underflow_frames - before.underflow_frames < 1440);
    assert!(
        h.presenter
            .player
            .current_media_time()
            .abs_diff(after.media_time.unwrap())
            < Duration::from_millis(50)
    );
}

#[test]
fn background_audio_continues_and_foreground_video_resume_keeps_audio_alive() {
    let mut h = Harness::new();
    h.start();
    h.presenter.audio_only_tick().unwrap();
    let before = h.snapshot();
    // No host ticks are required to keep the opted-in audio path supplied.
    thread::sleep(Duration::from_millis(400));
    let background = h.snapshot();
    assert!(background.read_frames > before.read_frames + 16000);
    assert!(background.underflow_frames - before.underflow_frames < 1440);
    h.tick_for(Duration::from_millis(500));
    assert!(!h.presenter.audio_only_tick_active);
    assert!(h.snapshot().read_frames > background.read_frames);
    assert!(h.snapshot().underflow_frames - background.underflow_frames < 1440);
    assert!(
        h.presenter
            .player
            .current_media_time()
            .abs_diff(h.snapshot().media_time.unwrap())
            < Duration::from_millis(50)
    );
}

#[test]
fn track_switch_and_open_clear_queued_audio_and_pending_rate() {
    let mut h = Harness::new();
    h.start();
    h.presenter.set_volume(0.35);
    let before = h.snapshot();
    let generation = h.presenter.player.playback_generation();
    h.presenter.select_audio_track(Some(2)).unwrap();
    h.tick_for(Duration::from_millis(500));
    assert_eq!(h.presenter.player.track_selection().audio, Some(2));
    assert!(h.presenter.player.playback_generation() > generation);
    assert!(h.snapshot().media_time.unwrap() >= before.media_time.unwrap());
    assert!(h.snapshot().read_frames > before.read_frames);
    h.presenter.set_playback_rate(2.0).unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/playback/playback-fixture.mkv");
    h.presenter
        .open(MediaRequest::new(fixture.to_string_lossy()))
        .unwrap();
    assert_eq!(h.snapshot().queued_frames, 0);
    assert_eq!(h.presenter.player.state(), crate::core::PlayerState::Ready);
    assert_eq!(h.presenter.audio.snapshot().playback_rate, 1.0);
    assert!((h.presenter.volume() - 0.35).abs() < 0.00001);
    h.presenter.play().unwrap();
    h.tick_for(Duration::from_millis(500));
    assert!(h.snapshot().media_time.unwrap() < Duration::from_secs(2));
    assert_eq!(h.ring.lock().unwrap().state(), AudioOutputState::Playing);
}

#[test]
fn natural_eof_drains_audio_and_replay_restarts_the_owner() {
    let mut h = Harness::new();
    h.start();
    let duration = h.presenter.player.duration().unwrap();
    h.presenter.seek(duration - Duration::from_secs(1)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline && !h.presenter.player.is_stopped_at_end() {
        h.tick_for(Duration::from_millis(20));
    }
    assert!(h.presenter.player.is_stopped_at_end());
    let final_position = h.presenter.player.current_media_time();
    h.tick_for(Duration::from_millis(400));
    assert_eq!(h.presenter.player.current_media_time(), final_position);
    assert_eq!(h.snapshot().queued_frames, 0);
    let read_frames = h.snapshot().read_frames;
    thread::sleep(Duration::from_millis(20));
    assert_eq!(h.snapshot().read_frames, read_frames);
    h.presenter.play().unwrap();
    h.tick_for(Duration::from_millis(500));
    assert_eq!(h.ring.lock().unwrap().state(), AudioOutputState::Playing);
    assert!(h.snapshot().media_time.unwrap() < Duration::from_secs(2));
}

#[test]
fn pause_seek_stop_and_replay_keep_audio_on_the_current_timeline() {
    let mut h = Harness::new();
    h.start();
    let old = h
        .presenter
        .player
        .capture_audio_clock(|| Some(h.snapshot()))
        .unwrap();
    h.presenter.set_playback_rate(2.0).unwrap();
    h.presenter.pause().unwrap();
    // Sample after the worker has committed its final pause position.
    h.presenter.player.set_frame_output_quiesced(false).unwrap();
    let paused = h.presenter.player.current_media_time();
    let read = h.snapshot().read_frames;
    h.tick_for(Duration::from_millis(60));
    assert_eq!(h.presenter.player.current_media_time(), paused);
    assert_eq!(h.snapshot().read_frames, read);
    assert_ne!(h.ring.lock().unwrap().state(), AudioOutputState::Playing);
    h.presenter.seek(Duration::from_secs(3)).unwrap();
    h.presenter
        .player
        .update_audio_clock_observation(old)
        .unwrap();
    h.tick_for(Duration::from_millis(100));
    assert_eq!(
        h.presenter.player.current_media_time(),
        Duration::from_secs(3)
    );
    assert_eq!(h.snapshot().read_frames, read);
    assert!(
        h.snapshot()
            .media_time
            .is_none_or(|time| time >= Duration::from_secs(3))
    );
    h.presenter.play().unwrap();
    h.tick_for(Duration::from_millis(500));
    assert!(h.snapshot().media_time.unwrap() >= Duration::from_secs(3));
    assert!(h.snapshot().read_frames > read);
    h.presenter.stop().unwrap();
    assert_eq!(h.snapshot().queued_frames, 0);
    assert_eq!(h.ring.lock().unwrap().state(), AudioOutputState::Stopped);
    h.presenter.set_playback_rate(1.0).unwrap();
    h.presenter.play().unwrap();
    h.tick_for(Duration::from_millis(500));
    assert!(h.snapshot().media_time.unwrap() < Duration::from_secs(2));
    h.presenter.close().unwrap();
    let closed = h.snapshot();
    thread::sleep(Duration::from_millis(20));
    assert_eq!(h.snapshot(), closed);
    assert_eq!(closed.queued_frames, 0);
    assert!(h.presenter.play().is_err());
}

#[test]
fn background_seek_and_track_change_restart_audio_without_display_ticks() {
    let mut h = Harness::new();
    h.start();
    h.presenter.audio_only_tick().unwrap();
    let generation = h.presenter.player.playback_generation();
    let target = Duration::from_secs(3);
    h.presenter.seek(target).unwrap();
    assert!(h.presenter.player.playback_generation() > generation);
    assert!(h.wait_for_background_audio().media_time.unwrap() >= target);

    h.presenter.pause().unwrap();
    let target = Duration::from_secs(1);
    h.presenter.seek(target).unwrap();
    let read = h.snapshot().read_frames;
    thread::sleep(Duration::from_millis(100));
    assert_eq!(h.snapshot().read_frames, read);
    assert_eq!(h.presenter.player.current_media_time(), target);
    h.presenter.play().unwrap();
    let resumed = h.wait_for_background_audio().media_time.unwrap();
    assert!(resumed >= target && resumed < Duration::from_secs(3));

    h.presenter.select_audio_track(Some(2)).unwrap();
    assert_eq!(h.presenter.player.track_selection().audio, Some(2));
    assert!(h.wait_for_background_audio().media_time.unwrap() >= resumed);
}

#[test]
fn background_natural_eof_and_replay_do_not_require_display_ticks() {
    let mut h = Harness::new();
    h.start();
    h.presenter.audio_only_tick().unwrap();
    let end = h.presenter.player.duration().unwrap();
    let before = h.snapshot().read_frames;
    // The final 100 ms of this 48 kHz fixture is shorter than the usual
    // 250 ms device startup threshold. It must still reach the callback.
    h.presenter.seek(end - Duration::from_millis(100)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let snapshot = h.snapshot();
        if h.presenter.player.is_stopped_at_end()
            && snapshot.queued_frames == 0
            && snapshot.read_frames >= before + 4_000
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "short EOF tail did not play: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    h.presenter.play().unwrap();
    let replay = h.wait_for_background_audio();
    assert!(replay.media_time.unwrap() < Duration::from_secs(2));
}
