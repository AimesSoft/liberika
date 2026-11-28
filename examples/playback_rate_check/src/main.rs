//! Plays a stretch of a media in real time through `Player`, headless, and
//! reports how many pictures were presented and how many the scheduler
//! dropped for lateness: the frame rate the decoder sustains.
//!
//! usage: cargo run --release -p playback_rate_check -- <media> [start-seconds] [seconds] [software|default] [rate]
//!
//! `software` opens the video with the software decoder (as a hardware
//! fallback would). `ERIKA_VIDEO_DECODE_THREADS=1` restores FFmpeg's
//! single-threaded default for comparison. Audio frames are drained and
//! discarded. `seekToFirstPictureMs` is the time from the seek call to the
//! first picture after it (play is called in between).

use std::env;
use std::process;
use std::time::{Duration, Instant};

use erika::playback::VideoDecodePreference;
use erika::{MediaRequest, Player, PlayerConfig, PlayerEvent, PlayerState};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(uri) = args.first().cloned() else {
        eprintln!(
            "usage: playback_rate_check <media> [start-seconds] [seconds] [software|default]"
        );
        process::exit(2);
    };
    let start = args
        .get(1)
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(60.0);
    let seconds = args
        .get(2)
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(20.0);
    let software = args.get(3).is_none_or(|mode| mode == "software");
    let rate = args
        .get(4)
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(1.0);

    let mut config = PlayerConfig::default();
    if software {
        config.playback.video_decode = VideoDecodePreference::Software;
    }
    let player = Player::new(config);
    let events = player.subscribe();
    let frames = player.subscribe_video_frames();
    let audio = player.subscribe_audio_frames();
    player
        .open(MediaRequest::new(uri))
        .unwrap_or_else(|error| fail(error));
    wait_ready(&events);
    let seek_started = Instant::now();
    player
        .seek(Duration::from_secs_f64(start))
        .unwrap_or_else(|error| fail(error));
    // Let the seek land and the first picture decode before timing.
    let mut first = None;
    if rate != 1.0 {
        player
            .set_playback_rate(rate)
            .unwrap_or_else(|error| fail(error));
    }
    player.play().unwrap_or_else(|error| fail(error));
    while first.is_none() {
        while audio.try_recv().is_ok() {}
        if let Ok(frame) = frames.recv_timeout(Duration::from_millis(50)) {
            first = Some(frame.media_time);
        }
    }
    let first_picture_ms = seek_started.elapsed().as_secs_f64() * 1000.0;
    let presented_before = player.timing_counters().presented_frames();
    let dropped_before = player.timing_counters().late_dropped_frames();
    let started = Instant::now();
    let mut received = 0_u64;
    let mut last_media = first.unwrap_or_default();
    while started.elapsed() < Duration::from_secs_f64(seconds) {
        while audio.try_recv().is_ok() {}
        if let Ok(frame) = frames.recv_timeout(Duration::from_millis(20)) {
            received += 1;
            last_media = frame.media_time;
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let counters = player.timing_counters();
    let presented = counters.presented_frames() - presented_before;
    let dropped = counters.late_dropped_frames() - dropped_before;
    let advanced = last_media
        .saturating_sub(first.unwrap_or_default())
        .as_secs_f64();
    println!(
        "{}",
        serde_json::json!({
            "decoder": if software { "software" } else { "default" },
            "decodeThreadsEnv": env::var("ERIKA_VIDEO_DECODE_THREADS").ok(),
            "rate": rate,
            "seekToFirstPictureMs": first_picture_ms.round(),
            "wallSeconds": (elapsed * 1000.0).round() / 1000.0,
            "mediaSecondsAdvanced": (advanced * 1000.0).round() / 1000.0,
            "picturesReceived": received,
            "presented": presented,
            "lateDropped": dropped,
            "presentedFps": ((presented as f64 / elapsed) * 10.0).round() / 10.0,
            "scheduler": counters.scheduler_json(),
        })
    );
    let _ = player.close();
}

fn wait_ready(events: &crossbeam_channel::Receiver<PlayerEvent>) {
    loop {
        match events.recv_timeout(Duration::from_secs(10)) {
            Ok(PlayerEvent::StateChanged(PlayerState::Ready)) => return,
            Ok(PlayerEvent::Error(error)) => fail(error),
            Ok(_) => {}
            Err(error) => fail(error),
        }
    }
}

fn fail(error: impl std::fmt::Display) -> ! {
    eprintln!("{error}");
    process::exit(1);
}
