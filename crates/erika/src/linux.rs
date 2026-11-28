//! Linux audio through PulseAudio (including PipeWire's PulseAudio server and WSLg).

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use crate::audio::{
    AudioClockSnapshot, AudioError, AudioOutputBackend, AudioOutputRuntimeStats, AudioOutputState,
    AudioPushResult, AudioRecoveryState, AudioRingBuffer, AudioRingBufferConfig,
    AudioRingBufferStats, Result, apply_volume_ramp, audio_output_queue_has_capacity,
    normalize_volume,
};
use crate::ffmpeg::{PcmAudioFrame, PcmFormat};

unsafe extern "C" {
    fn erika_pulse_open(
        rate: u32,
        channels: u8,
        fill: unsafe extern "C" fn(*mut c_void, *mut f32, usize),
        userdata: *mut c_void,
        error: *mut c_int,
    ) -> *mut c_void;
    fn erika_pulse_close(output: *mut c_void);
    fn erika_pulse_control(output: *mut c_void, running: c_int, flush: c_int) -> c_int;
    fn erika_pulse_latency(output: *mut c_void) -> u64;
    fn erika_pulse_status(output: *mut c_void) -> c_int;
    fn pa_strerror(error: c_int) -> *const c_char;
}

struct CallbackBuffer {
    ring: AudioRingBuffer,
    last_gain: f32,
}

struct CallbackState {
    buffer: Mutex<CallbackBuffer>,
    volume: AtomicU32,
    active: AtomicBool,
}

/// A corkable PulseAudio stream. The callback owns no reference to the presenter.
/// Stream teardown joins its callback thread before releasing the callback state.
pub struct PulseAudioOutput {
    stream: Option<NonNull<c_void>>,
    stream_running: bool,
    keep_remote_sink_running: bool,
    callback: Box<CallbackState>,
    state: AudioOutputState,
}

impl PulseAudioOutput {
    pub fn new(config: AudioRingBufferConfig) -> Self {
        Self {
            stream: None,
            stream_running: false,
            // WSLg's RDP sink delays uncork by the duration of a long pause.
            // Keep its transport clock alive with silence while our ring is
            // paused. Native PulseAudio/PipeWire still uses normal corking.
            keep_remote_sink_running: std::env::var("PULSE_SERVER")
                .is_ok_and(|server| server.contains("/mnt/wslg/")),
            callback: Box::new(CallbackState {
                buffer: Mutex::new(CallbackBuffer {
                    ring: AudioRingBuffer::new(config),
                    last_gain: 1.0,
                }),
                volume: AtomicU32::new(1.0_f32.to_bits()),
                active: AtomicBool::new(false),
            }),
            state: AudioOutputState::Stopped,
        }
    }

    fn close_stream(&mut self) {
        self.callback.active.store(false, Ordering::Release);
        if let Some(stream) = self.stream.take() {
            // SAFETY: this is the unique owner; close joins the callback thread.
            unsafe { erika_pulse_close(stream.as_ptr()) };
        }
        self.state = AudioOutputState::Stopped;
        self.stream_running = false;
    }

    fn control(&self, running: bool, flush: bool) -> Result<()> {
        let stream = self.stream.ok_or(AudioError::FormatNotConfigured)?;
        // SAFETY: the stream remains alive for this call and locks its mainloop.
        pulse_result(unsafe { erika_pulse_control(stream.as_ptr(), running.into(), flush.into()) })
    }

    fn latency(&self) -> Duration {
        self.stream.map_or(Duration::ZERO, |stream| {
            // SAFETY: queries are serialized with the audio callback in C.
            Duration::from_micros(unsafe { erika_pulse_latency(stream.as_ptr()) })
        })
    }
}

impl Drop for PulseAudioOutput {
    fn drop(&mut self) {
        self.close_stream();
    }
}

impl AudioOutputBackend for PulseAudioOutput {
    fn configure(&mut self, format: PcmFormat) -> Result<()> {
        if format.sample_rate == 0
            || format.sample_rate > 384_000
            || !(1..=32).contains(&format.channels)
        {
            return Err(AudioError::Backend(format!(
                "invalid PulseAudio format: {format:?}"
            )));
        }
        self.close_stream();
        {
            let mut buffer = self.callback.buffer.lock().map_err(|_| lock_error())?;
            buffer.ring.configure(format)?;
        }
        let mut error = 0;
        // SAFETY: the Box has a stable address and outlives the native stream.
        let stream = unsafe {
            erika_pulse_open(
                format.sample_rate,
                format.channels as u8,
                fill_audio,
                (&mut *self.callback as *mut CallbackState).cast(),
                &mut error,
            )
        };
        self.stream = NonNull::new(stream);
        if self.stream.is_none() {
            return Err(pulse_error(error));
        }
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        self.callback.active.store(true, Ordering::Release);
        if !self.stream_running {
            if let Err(error) = self.control(true, false) {
                self.callback.active.store(false, Ordering::Release);
                return Err(error);
            }
            self.stream_running = true;
        }
        self.state = AudioOutputState::Playing;
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.callback.active.store(false, Ordering::Release);
        if self.stream.is_some() && !self.keep_remote_sink_running {
            self.control(false, false)?;
            self.stream_running = false;
        }
        self.state = AudioOutputState::Paused;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.callback.active.store(false, Ordering::Release);
        let result = if self.stream.is_some() {
            if self.keep_remote_sink_running {
                self.control(true, true)
            } else {
                self.stream_running = false;
                self.control(false, false)
                    .and_then(|_| self.control(false, true))
            }
        } else {
            Ok(())
        };
        self.state = AudioOutputState::Stopped;
        let mut buffer = self.callback.buffer.lock().map_err(|_| lock_error())?;
        buffer.ring.clear();
        result
    }

    fn set_volume(&mut self, volume: f32) {
        self.callback
            .volume
            .store(normalize_volume(volume).to_bits(), Ordering::Relaxed);
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.callback.volume.load(Ordering::Relaxed))
    }

    fn set_playback_rate(&mut self, rate: f64) {
        if let Ok(mut buffer) = self.callback.buffer.lock() {
            buffer.ring.set_playback_rate(rate);
        }
    }

    fn can_accept_audio_frame(&self) -> bool {
        self.callback.buffer.lock().is_ok_and(|buffer| {
            buffer.ring.format().is_none_or(|format| {
                audio_output_queue_has_capacity(buffer.ring.queued_frames(), format.sample_rate)
            })
        })
    }

    fn push(&mut self, frame: PcmAudioFrame) -> Result<AudioPushResult> {
        if let Some(stream) = self.stream {
            // SAFETY: status takes the mainloop lock before inspecting the stream.
            pulse_result(unsafe { erika_pulse_status(stream.as_ptr()) })?;
        }
        self.callback
            .buffer
            .lock()
            .map_err(|_| lock_error())?
            .ring
            .push_frame(frame)
    }

    fn state(&self) -> AudioOutputState {
        self.state
    }

    fn stats(&self) -> AudioRingBufferStats {
        self.callback
            .buffer
            .lock()
            .map(|buffer| buffer.ring.stats())
            .unwrap_or_default()
    }

    fn clock_snapshot(&self) -> Option<AudioClockSnapshot> {
        // Use the callback consumption clock, matching the other ring-based
        // outputs. PulseAudio latency includes the remote WSLg/RDP sink and
        // queued silence; subtracting it from media PTS can exceed the engine's
        // audio lead window and repeatedly starve the producer. Native output
        // latency remains available separately for draining/rate transitions.
        Some(self.callback.buffer.lock().ok()?.ring.clock_snapshot())
    }

    fn queued_output_duration(&self) -> Duration {
        self.latency()
    }

    fn runtime_stats(&self) -> AudioOutputRuntimeStats {
        let error = self
            .stream
            .map_or(0, |stream| unsafe { erika_pulse_status(stream.as_ptr()) });
        AudioOutputRuntimeStats {
            recovery_state: if error == 0 {
                AudioRecoveryState::Stable
            } else {
                AudioRecoveryState::Failed
            },
            last_error_code: error,
            ..AudioOutputRuntimeStats::default()
        }
    }
}

unsafe extern "C" fn fill_audio(userdata: *mut c_void, samples: *mut f32, length: usize) {
    // SAFETY: the bridge passes a writable float buffer and retained CallbackState.
    let state = unsafe { &*userdata.cast::<CallbackState>() };
    let output = unsafe { std::slice::from_raw_parts_mut(samples, length) };
    output.fill(0.0);
    if !state.active.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut buffer) = state.buffer.lock() {
        let channels = buffer
            .ring
            .format()
            .map_or(1, |format| format.channels as usize);
        if let Ok(read) = buffer.ring.read_interleaved(output) {
            let gain = f32::from_bits(state.volume.load(Ordering::Relaxed));
            buffer.last_gain =
                apply_volume_ramp(output, channels, buffer.last_gain, gain, read.frames);
        }
    }
}

fn lock_error() -> AudioError {
    AudioError::Backend("PulseAudio ring lock poisoned".to_string())
}
fn pulse_result(error: c_int) -> Result<()> {
    if error == 0 {
        Ok(())
    } else {
        Err(pulse_error(error))
    }
}
fn pulse_error(error: c_int) -> AudioError {
    // SAFETY: PulseAudio returns a static, NUL-terminated error description.
    let message = unsafe { CStr::from_ptr(pa_strerror(error)) }.to_string_lossy();
    AudioError::Backend(format!("PulseAudio: {message} ({error})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::PcmSampleFormat;

    #[test]
    fn rejects_invalid_formats_without_connecting() {
        let mut output = PulseAudioOutput::new(AudioRingBufferConfig::default());
        for (sample_rate, channels) in [(0, 2), (48_000, 0), (48_000, 33)] {
            assert!(
                output
                    .configure(PcmFormat {
                        sample_rate,
                        channels,
                        sample_format: PcmSampleFormat::F32Interleaved
                    })
                    .is_err()
            );
        }
        assert!(output.start().is_err());
        assert_eq!(output.state(), AudioOutputState::Stopped);
    }

    #[test]
    #[ignore = "requires a running PulseAudio or PipeWire-Pulse server"]
    fn pulse_audio_pause_resume_flush_and_reconfigure() {
        let mut output = PulseAudioOutput::new(AudioRingBufferConfig::default());
        for format in [
            PcmFormat::f32_interleaved(48_000, 2),
            PcmFormat::f32_interleaved(44_100, 1),
        ] {
            output.configure(format).unwrap();
            let previous_read_frames = output.stats().read_frames;
            let frames = format.sample_rate as usize;
            output
                .push(PcmAudioFrame {
                    format,
                    pts: Some(Duration::from_secs(3)),
                    frames,
                    samples: vec![0.0; frames * format.channels as usize],
                })
                .unwrap();
            output.start().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while output.stats().read_frames == previous_read_frames
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                output.stats().read_frames > previous_read_frames,
                "stream did not consume PCM"
            );
            output.pause().unwrap();
            let paused_frames = output.stats().read_frames;
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(
                output.stats().read_frames,
                paused_frames,
                "paused stream consumed PCM"
            );
            assert_eq!(output.state(), AudioOutputState::Paused);
            output.start().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while output.stats().read_frames == paused_frames
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                output.stats().read_frames > paused_frames,
                "resume did not consume PCM"
            );
            assert!(output.clock_snapshot().unwrap().media_time.is_some());
            output.stop().unwrap();
            assert_eq!(output.stats().queued_frames, 0);
            assert_eq!(output.state(), AudioOutputState::Stopped);
        }
    }
}
