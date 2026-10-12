#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayHdrCapabilities {
    pub supports_edr: bool,
    pub maximum_potential_edr_headroom: f64,
}

impl Default for DisplayHdrCapabilities {
    fn default() -> Self {
        Self {
            supports_edr: false,
            maximum_potential_edr_headroom: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppleDecodeBackend {
    VideoToolbox,
    Software,
}

#[cfg(any(target_os = "ios", target_os = "tvos"))]
pub mod iosaudio {
    use std::ffi::c_void;
    use std::ptr::{self, NonNull};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    };
    use std::time::Duration;

    use thiserror::Error;

    use crate::audio::{
        AudioClockSnapshot, AudioOutputBackend, AudioOutputState, AudioPushResult, AudioRingBuffer,
        AudioRingBufferConfig, AudioRingBufferStats, apply_volume_ramp, normalize_volume,
    };
    use crate::ffmpeg::{PcmAudioFrame, PcmFormat, PcmSampleFormat};

    type OSStatus = i32;
    type UInt32 = u32;
    type Float64 = f64;
    type Boolean = u8;
    type AudioQueueRef = *mut c_void;
    type AudioQueueBufferRef = *mut AudioQueueBuffer;
    type CFRunLoopRef = *const c_void;
    type CFStringRef = *const c_void;

    const NO_ERR: OSStatus = 0;
    const BUFFER_COUNT: usize = 3;
    const BUFFER_MILLIS: u32 = 20;
    // One callback fills one ~BUFFER_MILLIS buffer; a 100 ms drain bound is
    // generous and only ever paid when dispose races a live callback.
    const IOS_DISPOSE_CALLBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
    const K_AUDIO_FORMAT_LINEAR_PCM: UInt32 = 0x6c70_636d;
    const K_AUDIO_FORMAT_FLAG_IS_FLOAT: UInt32 = 1 << 0;
    const K_AUDIO_FORMAT_FLAG_IS_PACKED: UInt32 = 1 << 3;

    #[repr(C)]
    struct AudioStreamBasicDescription {
        sample_rate: Float64,
        format_id: UInt32,
        format_flags: UInt32,
        bytes_per_packet: UInt32,
        frames_per_packet: UInt32,
        bytes_per_frame: UInt32,
        channels_per_frame: UInt32,
        bits_per_channel: UInt32,
        reserved: UInt32,
    }

    #[repr(C)]
    struct AudioQueueBuffer {
        audio_data_bytes_capacity: UInt32,
        audio_data: *mut c_void,
        audio_data_byte_size: UInt32,
        user_data: *mut c_void,
        packet_description_capacity: UInt32,
        packet_descriptions: *mut c_void,
        packet_description_count: UInt32,
    }

    type AudioQueueOutputCallback = unsafe extern "C" fn(
        user_data: *mut c_void,
        queue: AudioQueueRef,
        buffer: AudioQueueBufferRef,
    );

    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn AudioQueueNewOutput(
            format: *const AudioStreamBasicDescription,
            callback: AudioQueueOutputCallback,
            user_data: *mut c_void,
            run_loop: CFRunLoopRef,
            run_loop_mode: CFStringRef,
            flags: UInt32,
            out_queue: *mut AudioQueueRef,
        ) -> OSStatus;
        fn AudioQueueAllocateBuffer(
            queue: AudioQueueRef,
            buffer_byte_size: UInt32,
            out_buffer: *mut AudioQueueBufferRef,
        ) -> OSStatus;
        fn AudioQueueEnqueueBuffer(
            queue: AudioQueueRef,
            buffer: AudioQueueBufferRef,
            packet_description_count: UInt32,
            packet_descriptions: *const c_void,
        ) -> OSStatus;
        fn AudioQueueStart(queue: AudioQueueRef, start_time: *const c_void) -> OSStatus;
        fn AudioQueuePause(queue: AudioQueueRef) -> OSStatus;
        fn AudioQueueDispose(queue: AudioQueueRef, immediate: Boolean) -> OSStatus;
    }

    #[derive(Debug, Error)]
    pub enum IosAudioQueueOutputError {
        #[error("audio error: {0}")]
        Audio(#[from] crate::audio::AudioError),
        #[error("AudioQueue {operation} failed with OSStatus {status}")]
        AudioQueue {
            operation: &'static str,
            status: OSStatus,
        },
        #[error("AudioQueue output buffer is not configured")]
        NotConfigured,
        #[error("AudioQueue output lock was poisoned")]
        LockPoisoned,
    }

    pub type Result<T> = std::result::Result<T, IosAudioQueueOutputError>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct IosAudioQueueOutputConfig {
        pub ring_buffer: AudioRingBufferConfig,
    }

    impl Default for IosAudioQueueOutputConfig {
        fn default() -> Self {
            Self {
                ring_buffer: AudioRingBufferConfig {
                    capacity_frames: 192_000,
                    drop_oldest_on_overflow: true,
                },
            }
        }
    }

    struct CallbackState {
        buffer: Arc<Mutex<AudioRingBuffer>>,
        volume: Arc<AtomicU32>,
        // Gain the previous callback ended on; only the serialized AudioQueue
        // callback reads and writes it, so Relaxed ordering suffices.
        last_applied_volume: AtomicU32,
        // AudioQueue keeps these buffers in flight after the ring has handed
        // them to the platform. The presenter includes their duration in a
        // playback-rate transition bridge.
        in_flight_buffers: AtomicU32,
        // Callbacks currently executing on the AudioQueue's internal thread.
        // AudioQueueDispose does not guarantee an in-flight callback has
        // finished when it returns; the dispose path waits on this counter
        // before freeing the state.
        active_callbacks: AtomicU32,
        buffer_duration: Duration,
        channels: usize,
    }

    pub struct IosAudioQueueOutput {
        state: AudioOutputState,
        format: Option<PcmFormat>,
        queue: Option<AudioQueueRef>,
        buffers: Vec<AudioQueueBufferRef>,
        callback_state: Option<NonNull<CallbackState>>,
        buffer: Arc<Mutex<AudioRingBuffer>>,
        volume: Arc<AtomicU32>,
    }

    impl IosAudioQueueOutput {
        pub fn new(config: IosAudioQueueOutputConfig) -> Self {
            Self {
                state: AudioOutputState::Stopped,
                format: None,
                queue: None,
                buffers: Vec::new(),
                callback_state: None,
                buffer: Arc::new(Mutex::new(AudioRingBuffer::new(config.ring_buffer))),
                volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            }
        }

        pub fn configure(&mut self, format: PcmFormat) -> Result<()> {
            self.dispose_queue(true)?;
            configure_buffer(&self.buffer, format)?;
            let description = audio_stream_description(format);
            let buffer_frames = audio_queue_buffer_frames(format);
            let state = Box::new(CallbackState {
                buffer: Arc::clone(&self.buffer),
                volume: Arc::clone(&self.volume),
                last_applied_volume: AtomicU32::new(self.volume.load(Ordering::Relaxed)),
                in_flight_buffers: AtomicU32::new(0),
                active_callbacks: AtomicU32::new(0),
                buffer_duration: Duration::from_secs_f64(
                    buffer_frames as f64 / format.sample_rate.max(1) as f64,
                ),
                channels: format.channels.max(1) as usize,
            });
            let state = NonNull::new(Box::into_raw(state)).expect("Box::into_raw is non-null");
            let mut queue: AudioQueueRef = ptr::null_mut();
            check_status(
                unsafe {
                    AudioQueueNewOutput(
                        &description,
                        audio_queue_output_callback,
                        state.as_ptr().cast(),
                        ptr::null(),
                        ptr::null(),
                        0,
                        &mut queue,
                    )
                },
                "AudioQueueNewOutput",
            )?;
            if queue.is_null() {
                unsafe { drop(Box::from_raw(state.as_ptr())) };
                return Err(IosAudioQueueOutputError::NotConfigured);
            }

            let buffer_byte_size = audio_queue_buffer_size(format);
            let mut buffers = Vec::with_capacity(BUFFER_COUNT);
            for _ in 0..BUFFER_COUNT {
                let mut audio_buffer: AudioQueueBufferRef = ptr::null_mut();
                if let Err(error) = check_status(
                    unsafe { AudioQueueAllocateBuffer(queue, buffer_byte_size, &mut audio_buffer) },
                    "AudioQueueAllocateBuffer",
                ) {
                    unsafe {
                        let _ = AudioQueueDispose(queue, true as Boolean);
                        drop(Box::from_raw(state.as_ptr()));
                    }
                    return Err(error);
                }
                buffers.push(audio_buffer);
            }

            self.queue = Some(queue);
            self.buffers = buffers;
            self.callback_state = Some(state);
            self.format = Some(format);
            self.state = AudioOutputState::Stopped;
            Ok(())
        }

        pub fn set_volume(&mut self, volume: f32) {
            self.volume
                .store(normalize_volume(volume).to_bits(), Ordering::Relaxed);
        }

        pub fn volume(&self) -> f32 {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        }

        pub fn start(&mut self) -> Result<()> {
            let queue = self.queue.ok_or(IosAudioQueueOutputError::NotConfigured)?;
            if self.state != AudioOutputState::Paused {
                let state = self
                    .callback_state
                    .ok_or(IosAudioQueueOutputError::NotConfigured)?;
                for &buffer in &self.buffers {
                    // SAFETY: the callback state stays alive until dispose_queue.
                    fill_audio_queue_buffer(queue, buffer, unsafe { state.as_ref() })?;
                }
            }
            check_status(
                unsafe { AudioQueueStart(queue, ptr::null()) },
                "AudioQueueStart",
            )?;
            self.state = AudioOutputState::Playing;
            Ok(())
        }

        pub fn pause(&mut self) -> Result<()> {
            if let Some(queue) = self.queue {
                check_status(unsafe { AudioQueuePause(queue) }, "AudioQueuePause")?;
            }
            self.state = AudioOutputState::Paused;
            Ok(())
        }

        pub fn stop(&mut self) -> Result<()> {
            self.dispose_queue(true)?;
            clear_buffer(&self.buffer)?;
            self.format = None;
            self.state = AudioOutputState::Stopped;
            Ok(())
        }

        pub fn push(&mut self, frame: PcmAudioFrame) -> Result<AudioPushResult> {
            let mut buffer = self
                .buffer
                .lock()
                .map_err(|_| IosAudioQueueOutputError::LockPoisoned)?;
            Ok(buffer.push_frame(frame)?)
        }

        pub fn state(&self) -> AudioOutputState {
            self.state
        }

        pub fn stats(&self) -> Result<AudioRingBufferStats> {
            let buffer = self
                .buffer
                .lock()
                .map_err(|_| IosAudioQueueOutputError::LockPoisoned)?;
            Ok(buffer.stats())
        }

        pub fn clock_snapshot(&self) -> Result<AudioClockSnapshot> {
            let buffer = self
                .buffer
                .lock()
                .map_err(|_| IosAudioQueueOutputError::LockPoisoned)?;
            Ok(buffer.clock_snapshot())
        }

        pub fn queued_output_duration(&self) -> Duration {
            let in_flight = self
                .callback_state
                .map(|state| unsafe {
                    let state = state.as_ref();
                    state
                        .buffer_duration
                        .saturating_mul(state.in_flight_buffers.load(Ordering::Acquire))
                })
                .unwrap_or(Duration::ZERO);
            in_flight
        }

        fn dispose_queue(&mut self, immediate: bool) -> Result<()> {
            if let Some(queue) = self.queue.take() {
                let status = unsafe { AudioQueueDispose(queue, immediate as Boolean) };
                self.buffers.clear();
                if let Some(state) = self.callback_state.take() {
                    // AudioQueueDispose does not guarantee that an in-flight
                    // callback has finished when it returns. Wait (bounded) for
                    // the callback to leave the state before freeing it; if the
                    // wait times out, deliberately leak one callback state --
                    // that beats a use-after-free on the queue's thread.
                    let deadline = std::time::Instant::now() + IOS_DISPOSE_CALLBACK_DRAIN_TIMEOUT;
                    let mut still_active =
                        unsafe { state.as_ref().active_callbacks.load(Ordering::Acquire) } > 0;
                    while still_active && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        still_active =
                            unsafe { state.as_ref().active_callbacks.load(Ordering::Acquire) } > 0;
                    }
                    let boxed = unsafe { Box::from_raw(state.as_ptr()) };
                    if still_active {
                        std::mem::forget(boxed);
                        crate::trace::log(
                            "[erika-audio-trace] stage=ios_audioqueue_state_leaked reason=dispose_callback_drain_timeout",
                        );
                    }
                }
                check_status(status, "AudioQueueDispose")?;
            }
            Ok(())
        }
    }

    impl Default for IosAudioQueueOutput {
        fn default() -> Self {
            Self::new(IosAudioQueueOutputConfig::default())
        }
    }

    impl Drop for IosAudioQueueOutput {
        fn drop(&mut self) {
            let _ = self.dispose_queue(true);
        }
    }

    impl AudioOutputBackend for IosAudioQueueOutput {
        fn configure(&mut self, format: PcmFormat) -> crate::audio::Result<()> {
            IosAudioQueueOutput::configure(self, format)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn start(&mut self) -> crate::audio::Result<()> {
            IosAudioQueueOutput::start(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn pause(&mut self) -> crate::audio::Result<()> {
            IosAudioQueueOutput::pause(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn stop(&mut self) -> crate::audio::Result<()> {
            IosAudioQueueOutput::stop(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn set_volume(&mut self, volume: f32) {
            IosAudioQueueOutput::set_volume(self, volume);
        }

        fn volume(&self) -> f32 {
            IosAudioQueueOutput::volume(self)
        }

        fn set_playback_rate(&mut self, rate: f64) {
            if let Ok(mut buffer) = self.buffer.lock() {
                buffer.set_playback_rate(rate);
            }
        }

        fn push(&mut self, frame: PcmAudioFrame) -> crate::audio::Result<AudioPushResult> {
            IosAudioQueueOutput::push(self, frame)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn state(&self) -> AudioOutputState {
            self.state()
        }

        fn stats(&self) -> AudioRingBufferStats {
            self.stats().unwrap_or_default()
        }

        fn clock_snapshot(&self) -> Option<AudioClockSnapshot> {
            self.clock_snapshot().ok()
        }

        fn queued_output_duration(&self) -> Duration {
            IosAudioQueueOutput::queued_output_duration(self)
        }
    }

    unsafe extern "C" fn audio_queue_output_callback(
        user_data: *mut c_void,
        queue: AudioQueueRef,
        audio_buffer: AudioQueueBufferRef,
    ) {
        if user_data.is_null() || queue.is_null() || audio_buffer.is_null() {
            return;
        }
        let state = unsafe { &*(user_data as *const CallbackState) };
        // Marked before any early return below so the dispose path can wait
        // for this callback to leave the state before freeing it.
        state.active_callbacks.fetch_add(1, Ordering::AcqRel);
        let _active_guard = CallbackActiveGuard(&state.active_callbacks);
        let _ =
            state
                .in_flight_buffers
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_sub(1)
                });
        let _ = fill_audio_queue_buffer(queue, audio_buffer, state);
    }

    struct CallbackActiveGuard<'a>(&'a AtomicU32);

    impl Drop for CallbackActiveGuard<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn fill_audio_queue_buffer(
        queue: AudioQueueRef,
        audio_buffer: AudioQueueBufferRef,
        state: &CallbackState,
    ) -> Result<()> {
        if queue.is_null() || audio_buffer.is_null() {
            return Err(IosAudioQueueOutputError::NotConfigured);
        }
        let buffer = unsafe { &mut *audio_buffer };
        let sample_count = (buffer.audio_data_bytes_capacity as usize) / std::mem::size_of::<f32>();
        if buffer.audio_data.is_null() || sample_count == 0 {
            buffer.audio_data_byte_size = 0;
            return Ok(());
        }
        let samples = unsafe {
            std::slice::from_raw_parts_mut(buffer.audio_data.cast::<f32>(), sample_count)
        };
        let audible_frames = match state.buffer.lock() {
            Ok(mut ring) => match ring.read_interleaved(samples) {
                Ok(read) => read.frames,
                Err(_) => {
                    samples.fill(0.0);
                    0
                }
            },
            Err(_) => {
                samples.fill(0.0);
                0
            }
        };
        // Ramp from the gain the previous buffer ended on so an atomic volume
        // step never lands as an audible discontinuity (zipper noise). Only the
        // frames the ring actually supplied advance the ramp.
        let from = f32::from_bits(state.last_applied_volume.load(Ordering::Relaxed));
        let to = f32::from_bits(state.volume.load(Ordering::Relaxed));
        let reached = apply_volume_ramp(samples, state.channels, from, to, audible_frames);
        state
            .last_applied_volume
            .store(reached.to_bits(), Ordering::Relaxed);
        buffer.audio_data_byte_size = buffer.audio_data_bytes_capacity;
        state.in_flight_buffers.fetch_add(1, Ordering::AcqRel);
        let result = check_status(
            unsafe { AudioQueueEnqueueBuffer(queue, audio_buffer, 0, ptr::null()) },
            "AudioQueueEnqueueBuffer",
        );
        if result.is_err() {
            let _ = state.in_flight_buffers.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |count| count.checked_sub(1),
            );
        }
        result
    }

    fn audio_stream_description(format: PcmFormat) -> AudioStreamBasicDescription {
        match format.sample_format {
            PcmSampleFormat::F32Interleaved => {
                let channels = format.channels.max(1);
                let bytes_per_frame = channels * std::mem::size_of::<f32>() as u32;
                AudioStreamBasicDescription {
                    sample_rate: format.sample_rate.max(1) as f64,
                    format_id: K_AUDIO_FORMAT_LINEAR_PCM,
                    format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
                    bytes_per_packet: bytes_per_frame,
                    frames_per_packet: 1,
                    bytes_per_frame,
                    channels_per_frame: channels,
                    bits_per_channel: 32,
                    reserved: 0,
                }
            }
        }
    }

    fn audio_queue_buffer_frames(format: PcmFormat) -> usize {
        ((format.sample_rate.max(1) as u64 * BUFFER_MILLIS as u64) / 1_000).clamp(256, 4_096)
            as usize
    }

    fn audio_queue_buffer_size(format: PcmFormat) -> UInt32 {
        let channels = format.channels.max(1) as usize;
        let frames = audio_queue_buffer_frames(format);
        (frames * channels * std::mem::size_of::<f32>()).min(UInt32::MAX as usize) as UInt32
    }

    fn configure_buffer(buffer: &Arc<Mutex<AudioRingBuffer>>, format: PcmFormat) -> Result<()> {
        let mut buffer = buffer
            .lock()
            .map_err(|_| IosAudioQueueOutputError::LockPoisoned)?;
        Ok(buffer.configure(format)?)
    }

    fn clear_buffer(buffer: &Arc<Mutex<AudioRingBuffer>>) -> Result<()> {
        let mut buffer = buffer
            .lock()
            .map_err(|_| IosAudioQueueOutputError::LockPoisoned)?;
        buffer.clear();
        Ok(())
    }

    fn check_status(status: OSStatus, operation: &'static str) -> Result<()> {
        if status == NO_ERR {
            Ok(())
        } else {
            Err(IosAudioQueueOutputError::AudioQueue { operation, status })
        }
    }
}

#[cfg(target_os = "macos")]
pub mod coreaudio {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    };

    use coreaudio::audio_unit::audio_format::LinearPcmFlags;
    use coreaudio::audio_unit::render_callback::{self, data};
    use coreaudio::audio_unit::{AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat};
    use thiserror::Error;

    use crate::audio::{
        AudioClockSnapshot, AudioOutputBackend, AudioOutputState, AudioPushResult, AudioReadResult,
        AudioRingBuffer, AudioRingBufferConfig, AudioRingBufferStats, apply_volume_ramp,
        normalize_volume,
    };
    use crate::ffmpeg::{PcmAudioFrame, PcmFormat, PcmSampleFormat};

    #[derive(Debug, Error)]
    pub enum CoreAudioOutputError {
        #[error("audio error: {0}")]
        Audio(#[from] crate::audio::AudioError),
        #[error("CoreAudio error: {0:?}")]
        CoreAudio(#[from] coreaudio::Error),
        #[error("CoreAudio output buffer is not configured")]
        NotConfigured,
        #[error("CoreAudio output lock was poisoned")]
        LockPoisoned,
    }

    pub type Result<T> = std::result::Result<T, CoreAudioOutputError>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CoreAudioOutputConfig {
        pub ring_buffer: AudioRingBufferConfig,
    }

    impl Default for CoreAudioOutputConfig {
        fn default() -> Self {
            Self {
                ring_buffer: AudioRingBufferConfig {
                    capacity_frames: 192_000,
                    drop_oldest_on_overflow: true,
                },
            }
        }
    }

    pub struct CoreAudioOutput {
        state: AudioOutputState,
        audio_unit: Option<AudioUnit>,
        buffer: Arc<Mutex<AudioRingBuffer>>,
        volume: Arc<AtomicU32>,
    }

    impl CoreAudioOutput {
        pub fn new(config: CoreAudioOutputConfig) -> Self {
            Self {
                state: AudioOutputState::Stopped,
                audio_unit: None,
                buffer: Arc::new(Mutex::new(AudioRingBuffer::new(config.ring_buffer))),
                volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            }
        }

        pub fn configure(&mut self, format: PcmFormat) -> Result<()> {
            configure_buffer(&self.buffer, format)?;
            let mut audio_unit = AudioUnit::new_uninitialized(IOType::DefaultOutput)?;
            audio_unit.set_stream_format(
                coreaudio_stream_format(format),
                Scope::Input,
                Element::Output,
            )?;
            install_render_callback(
                &mut audio_unit,
                Arc::clone(&self.buffer),
                Arc::clone(&self.volume),
                format.channels.max(1) as usize,
            )?;
            audio_unit.initialize()?;
            self.audio_unit = Some(audio_unit);
            Ok(())
        }

        pub fn set_volume(&mut self, volume: f32) {
            self.volume
                .store(normalize_volume(volume).to_bits(), Ordering::Relaxed);
        }

        pub fn volume(&self) -> f32 {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        }

        pub fn start(&mut self) -> Result<()> {
            let Some(audio_unit) = &mut self.audio_unit else {
                return Err(CoreAudioOutputError::NotConfigured);
            };
            audio_unit.start()?;
            self.state = AudioOutputState::Playing;
            Ok(())
        }

        pub fn pause(&mut self) -> Result<()> {
            if let Some(audio_unit) = &mut self.audio_unit {
                audio_unit.stop()?;
            }
            self.state = AudioOutputState::Paused;
            Ok(())
        }

        pub fn stop(&mut self) -> Result<()> {
            if let Some(audio_unit) = &mut self.audio_unit {
                audio_unit.stop()?;
            }
            clear_buffer(&self.buffer)?;
            self.state = AudioOutputState::Stopped;
            Ok(())
        }

        pub fn push(&mut self, frame: PcmAudioFrame) -> Result<AudioPushResult> {
            let mut buffer = self
                .buffer
                .lock()
                .map_err(|_| CoreAudioOutputError::LockPoisoned)?;
            Ok(buffer.push_frame(frame)?)
        }

        pub fn read_for_test(&mut self, output: &mut [f32]) -> Result<AudioReadResult> {
            let mut buffer = self
                .buffer
                .lock()
                .map_err(|_| CoreAudioOutputError::LockPoisoned)?;
            Ok(buffer.read_interleaved(output)?)
        }

        pub fn state(&self) -> AudioOutputState {
            self.state
        }

        pub fn stats(&self) -> Result<AudioRingBufferStats> {
            let buffer = self
                .buffer
                .lock()
                .map_err(|_| CoreAudioOutputError::LockPoisoned)?;
            Ok(buffer.stats())
        }

        pub fn clock_snapshot(&self) -> Result<AudioClockSnapshot> {
            let buffer = self
                .buffer
                .lock()
                .map_err(|_| CoreAudioOutputError::LockPoisoned)?;
            Ok(buffer.clock_snapshot())
        }
    }

    impl Default for CoreAudioOutput {
        fn default() -> Self {
            Self::new(CoreAudioOutputConfig::default())
        }
    }

    impl AudioOutputBackend for CoreAudioOutput {
        fn configure(&mut self, format: PcmFormat) -> crate::audio::Result<()> {
            CoreAudioOutput::configure(self, format)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn start(&mut self) -> crate::audio::Result<()> {
            CoreAudioOutput::start(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn pause(&mut self) -> crate::audio::Result<()> {
            CoreAudioOutput::pause(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn stop(&mut self) -> crate::audio::Result<()> {
            CoreAudioOutput::stop(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn set_volume(&mut self, volume: f32) {
            CoreAudioOutput::set_volume(self, volume);
        }

        fn volume(&self) -> f32 {
            CoreAudioOutput::volume(self)
        }

        fn set_playback_rate(&mut self, rate: f64) {
            if let Ok(mut buffer) = self.buffer.lock() {
                buffer.set_playback_rate(rate);
            }
        }

        fn push(&mut self, frame: PcmAudioFrame) -> crate::audio::Result<AudioPushResult> {
            CoreAudioOutput::push(self, frame)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn state(&self) -> AudioOutputState {
            self.state
        }

        fn stats(&self) -> AudioRingBufferStats {
            self.stats().unwrap_or_default()
        }

        fn clock_snapshot(&self) -> Option<AudioClockSnapshot> {
            self.clock_snapshot().ok()
        }
    }

    fn coreaudio_stream_format(format: PcmFormat) -> StreamFormat {
        match format.sample_format {
            PcmSampleFormat::F32Interleaved => StreamFormat {
                sample_rate: format.sample_rate as f64,
                sample_format: SampleFormat::F32,
                flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
                channels: format.channels,
            },
        }
    }

    fn install_render_callback(
        audio_unit: &mut AudioUnit,
        buffer: Arc<Mutex<AudioRingBuffer>>,
        volume: Arc<AtomicU32>,
        channels: usize,
    ) -> Result<()> {
        type Args = render_callback::Args<data::Interleaved<f32>>;
        // Owned by the render callback closure; CoreAudio serializes calls, so
        // the gain the previous callback ended on needs no synchronization.
        let mut last_applied_volume = f32::from_bits(volume.load(Ordering::Relaxed));
        audio_unit.set_render_callback(move |mut args: Args| {
            let read_result = buffer
                .lock()
                .map_err(|_| ())
                .and_then(|mut buffer| buffer.read_interleaved(args.data.buffer).map_err(|_| ()))?;
            // Ramp from the previous callback's gain so an atomic volume step
            // never lands as an audible discontinuity (zipper noise). Only the
            // frames the ring actually supplied advance the ramp.
            let target = f32::from_bits(volume.load(Ordering::Relaxed));
            last_applied_volume = apply_volume_ramp(
                args.data.buffer,
                channels,
                last_applied_volume,
                target,
                read_result.frames,
            );
            if read_result.underflow_frames > 0 {
                args.flags
                    .insert(render_callback::ActionFlags::OUTPUT_IS_SILENCE);
            }
            Ok(())
        })?;
        Ok(())
    }

    fn configure_buffer(buffer: &Arc<Mutex<AudioRingBuffer>>, format: PcmFormat) -> Result<()> {
        let mut buffer = buffer
            .lock()
            .map_err(|_| CoreAudioOutputError::LockPoisoned)?;
        Ok(buffer.configure(format)?)
    }

    fn clear_buffer(buffer: &Arc<Mutex<AudioRingBuffer>>) -> Result<()> {
        let mut buffer = buffer
            .lock()
            .map_err(|_| CoreAudioOutputError::LockPoisoned)?;
        buffer.clear();
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::audio::apply_volume;

        #[test]
        fn volume_is_clamped_and_applied_to_pcm_samples() {
            let mut output = CoreAudioOutput::default();

            assert_eq!(output.volume(), 1.0);
            output.set_volume(0.25);
            assert_eq!(output.volume(), 0.25);
            output.set_volume(-1.0);
            assert_eq!(output.volume(), 0.0);
            output.set_volume(f32::NAN);
            assert_eq!(output.volume(), 1.0);

            let mut samples = [1.0, -0.5, 0.25, 0.0];
            apply_volume(&mut samples, 0.5);
            assert_eq!(samples, [0.5, -0.25, 0.125, 0.0]);
        }
    }
}
