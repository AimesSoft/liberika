#[cfg(target_os = "windows")]
pub mod wasapi {
    use std::slice;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    };
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use crossbeam_channel::{Receiver, Sender};
    use thiserror::Error;

    use crate::audio::{
        AudioClockSnapshot, AudioOutputBackend, AudioOutputRuntimeStats, AudioOutputState,
        AudioPushResult, AudioReadResult, AudioRingBuffer, AudioRingBufferConfig,
        AudioRingBufferStats, RecoveryBackoff, RecoverySignals, apply_volume, apply_volume_ramp,
        normalize_volume,
    };
    use crate::ffmpeg::{PcmAudioFrame, PcmFormat, PcmSampleFormat};

    use ::windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, IAudioRenderClient,
        IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
    };
    use ::windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };

    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WASAPI_BUFFER_DURATION_HNS: i64 = 1_000_000;
    const RENDER_POLL_INTERVAL: Duration = Duration::from_millis(5);
    const RECOVERY_INITIAL_DELAY: Duration = Duration::from_millis(200);
    const RECOVERY_MAX_ATTEMPTS: u32 = 5;

    // Device-loss class HRESULTs that warrant rebuilding the client against the
    // (possibly new) default endpoint instead of tearing the render thread down.
    const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
    const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889_000A_u32 as i32;
    const AUDCLNT_E_ENDPOINT_CREATE_FAILED: i32 = 0x8889_000F_u32 as i32;
    const AUDCLNT_E_SERVICE_NOT_RUNNING: i32 = 0x8889_0010_u32 as i32;
    const AUDCLNT_E_RESOURCES_INVALIDATED: i32 = 0x8889_0026_u32 as i32;
    /// HRESULT_FROM_WIN32(ERROR_DEVICE_NOT_CONNECTED)
    const E_DEVICE_NOT_CONNECTED: i32 = 0x8007_048F_u32 as i32;

    fn is_device_loss_hresult(code: i32) -> bool {
        matches!(
            code,
            AUDCLNT_E_DEVICE_INVALIDATED
                | AUDCLNT_E_DEVICE_IN_USE
                | AUDCLNT_E_ENDPOINT_CREATE_FAILED
                | AUDCLNT_E_SERVICE_NOT_RUNNING
                | AUDCLNT_E_RESOURCES_INVALIDATED
                | E_DEVICE_NOT_CONNECTED
        )
    }

    /// What the render thread should do after a device loss or a failed reopen.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RecoveryStep {
        RetryAfter(Duration),
        GiveUp,
    }

    /// Retry schedule for rebuilding the WASAPI client after a device loss.
    ///
    /// A thin wrapper over the host-testable [`RecoveryBackoff`] in `audio.rs`
    /// (`lib.rs` only compiles this module on Windows, so the platform-neutral
    /// schedule and its unit tests live there).
    #[derive(Debug)]
    struct RenderRecoveryPlan {
        backoff: RecoveryBackoff,
    }

    impl RenderRecoveryPlan {
        fn new() -> Self {
            Self {
                backoff: RecoveryBackoff::new(RECOVERY_INITIAL_DELAY, RECOVERY_MAX_ATTEMPTS),
            }
        }

        fn next_step(&mut self) -> RecoveryStep {
            match self.backoff.next_delay() {
                Some(delay) => RecoveryStep::RetryAfter(delay),
                None => RecoveryStep::GiveUp,
            }
        }

        fn reset(&mut self) {
            self.backoff.reset();
        }
    }

    #[derive(Debug, Error)]
    pub enum WasapiAudioOutputError {
        #[error("audio error: {0}")]
        Audio(#[from] crate::audio::AudioError),
        #[error("WASAPI {operation} failed with HRESULT 0x{code:08X}: {message}")]
        Wasapi {
            operation: &'static str,
            code: i32,
            message: String,
        },
        #[error("WASAPI output buffer is not configured")]
        NotConfigured,
        #[error("WASAPI output lock was poisoned")]
        LockPoisoned,
        #[error("WASAPI render thread stopped before initialization")]
        ThreadStopped,
        #[error("unsupported PCM format for WASAPI: {0:?}")]
        UnsupportedFormat(PcmSampleFormat),
        #[error("invalid WASAPI format: sample_rate={sample_rate}, channels={channels}")]
        InvalidFormat { sample_rate: u32, channels: u32 },
    }

    pub type Result<T> = std::result::Result<T, WasapiAudioOutputError>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct WasapiAudioOutputConfig {
        pub ring_buffer: AudioRingBufferConfig,
    }

    impl Default for WasapiAudioOutputConfig {
        fn default() -> Self {
            Self {
                ring_buffer: AudioRingBufferConfig {
                    capacity_frames: 192_000,
                    drop_oldest_on_overflow: true,
                },
            }
        }
    }

    pub struct WasapiAudioOutput {
        state: AudioOutputState,
        format: Option<PcmFormat>,
        render_thread: Option<WasapiRenderThread>,
        buffer: Arc<Mutex<AudioRingBuffer>>,
        volume: Arc<AtomicU32>,
        endpoint_queued_frames: Arc<AtomicU32>,
        signals: Arc<RecoverySignals>,
    }

    impl WasapiAudioOutput {
        pub fn new(config: WasapiAudioOutputConfig) -> Self {
            Self {
                state: AudioOutputState::Stopped,
                format: None,
                render_thread: None,
                buffer: Arc::new(Mutex::new(AudioRingBuffer::new(config.ring_buffer))),
                volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
                endpoint_queued_frames: Arc::new(AtomicU32::new(0)),
                signals: Arc::new(RecoverySignals::default()),
            }
        }

        pub fn configure(&mut self, format: PcmFormat) -> Result<()> {
            stop_render_thread(&mut self.render_thread);
            configure_buffer(&self.buffer, format)?;
            self.signals.reset();
            self.endpoint_queued_frames.store(0, Ordering::Release);
            let render_thread = WasapiRenderThread::spawn(
                format,
                Arc::clone(&self.buffer),
                Arc::clone(&self.volume),
                Arc::clone(&self.endpoint_queued_frames),
                Arc::clone(&self.signals),
            )?;
            self.render_thread = Some(render_thread);
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
            let render_thread = self
                .render_thread
                .as_ref()
                .ok_or(WasapiAudioOutputError::NotConfigured)?;
            render_thread.send(WasapiRenderCommand::Start)?;
            self.state = AudioOutputState::Playing;
            Ok(())
        }

        pub fn pause(&mut self) -> Result<()> {
            if let Some(render_thread) = &self.render_thread {
                render_thread.send(WasapiRenderCommand::Pause)?;
            }
            self.state = AudioOutputState::Paused;
            Ok(())
        }

        pub fn stop(&mut self) -> Result<()> {
            if let Some(render_thread) = &self.render_thread {
                render_thread.send(WasapiRenderCommand::Stop)?;
            }
            clear_buffer(&self.buffer)?;
            self.state = AudioOutputState::Stopped;
            Ok(())
        }

        pub fn push(&mut self, frame: PcmAudioFrame) -> Result<AudioPushResult> {
            let mut buffer = self
                .buffer
                .lock()
                .map_err(|_| WasapiAudioOutputError::LockPoisoned)?;
            Ok(buffer.push_frame(frame)?)
        }

        pub fn read_for_test(&mut self, output: &mut [f32]) -> Result<AudioReadResult> {
            let mut buffer = self
                .buffer
                .lock()
                .map_err(|_| WasapiAudioOutputError::LockPoisoned)?;
            let result = buffer.read_interleaved(output)?;
            apply_volume(output, self.volume());
            Ok(result)
        }

        pub fn state(&self) -> AudioOutputState {
            self.state
        }

        pub fn stats(&self) -> Result<AudioRingBufferStats> {
            let buffer = self
                .buffer
                .lock()
                .map_err(|_| WasapiAudioOutputError::LockPoisoned)?;
            Ok(buffer.stats())
        }

        pub fn clock_snapshot(&self) -> Result<AudioClockSnapshot> {
            let buffer = self
                .buffer
                .lock()
                .map_err(|_| WasapiAudioOutputError::LockPoisoned)?;
            Ok(buffer.clock_snapshot())
        }

        pub fn runtime_stats(&self) -> AudioOutputRuntimeStats {
            self.signals.snapshot()
        }

        pub fn format(&self) -> Option<PcmFormat> {
            self.format
        }
    }

    impl Default for WasapiAudioOutput {
        fn default() -> Self {
            Self::new(WasapiAudioOutputConfig::default())
        }
    }

    impl Drop for WasapiAudioOutput {
        fn drop(&mut self) {
            stop_render_thread(&mut self.render_thread);
        }
    }

    impl AudioOutputBackend for WasapiAudioOutput {
        fn configure(&mut self, format: PcmFormat) -> crate::audio::Result<()> {
            WasapiAudioOutput::configure(self, format)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn start(&mut self) -> crate::audio::Result<()> {
            WasapiAudioOutput::start(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn pause(&mut self) -> crate::audio::Result<()> {
            WasapiAudioOutput::pause(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn stop(&mut self) -> crate::audio::Result<()> {
            WasapiAudioOutput::stop(self)
                .map_err(|error| crate::audio::AudioError::Backend(error.to_string()))
        }

        fn set_volume(&mut self, volume: f32) {
            WasapiAudioOutput::set_volume(self, volume);
        }

        fn volume(&self) -> f32 {
            WasapiAudioOutput::volume(self)
        }

        fn set_playback_rate(&mut self, rate: f64) {
            if let Ok(mut buffer) = self.buffer.lock() {
                buffer.set_playback_rate(rate);
            }
        }

        fn push(&mut self, frame: PcmAudioFrame) -> crate::audio::Result<AudioPushResult> {
            WasapiAudioOutput::push(self, frame)
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

        fn queued_output_duration(&self) -> Duration {
            let frames = self.endpoint_queued_frames.load(Ordering::Acquire) as u64;
            let Some(sample_rate) = self.format.map(|format| u64::from(format.sample_rate)) else {
                return Duration::ZERO;
            };
            if sample_rate == 0 {
                return Duration::ZERO;
            }
            Duration::from_secs_f64(frames as f64 / sample_rate as f64)
        }

        fn runtime_stats(&self) -> AudioOutputRuntimeStats {
            WasapiAudioOutput::runtime_stats(self)
        }
    }

    struct WasapiRenderThread {
        commands: Sender<WasapiRenderCommand>,
        worker: Option<JoinHandle<()>>,
    }

    impl WasapiRenderThread {
        fn spawn(
            format: PcmFormat,
            buffer: Arc<Mutex<AudioRingBuffer>>,
            volume: Arc<AtomicU32>,
            endpoint_queued_frames: Arc<AtomicU32>,
            signals: Arc<RecoverySignals>,
        ) -> Result<Self> {
            let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
            let (init_tx, init_rx) = crossbeam_channel::bounded(1);
            let worker = thread::Builder::new()
                .name("erika-wasapi-render".to_string())
                .spawn(move || {
                    let result = run_render_thread(
                        format,
                        buffer,
                        volume,
                        endpoint_queued_frames,
                        signals,
                        commands_rx,
                        init_tx,
                    );
                    if let Err(error) = result {
                        eprintln!("erika WASAPI render thread stopped: {error}");
                    }
                })
                .map_err(|error| WasapiAudioOutputError::Wasapi {
                    operation: "thread spawn",
                    code: 0,
                    message: error.to_string(),
                })?;

            match init_rx
                .recv()
                .map_err(|_| WasapiAudioOutputError::ThreadStopped)?
            {
                Ok(()) => Ok(Self {
                    commands: commands_tx,
                    worker: Some(worker),
                }),
                Err(error) => {
                    let _ = commands_tx.send(WasapiRenderCommand::Shutdown);
                    let _ = worker.join();
                    Err(error)
                }
            }
        }

        fn send(&self, command: WasapiRenderCommand) -> Result<()> {
            self.commands
                .send(command)
                .map_err(|_| WasapiAudioOutputError::ThreadStopped)
        }
    }

    impl Drop for WasapiRenderThread {
        fn drop(&mut self) {
            let _ = self.commands.send(WasapiRenderCommand::Shutdown);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum WasapiRenderCommand {
        Start,
        Pause,
        Stop,
        Shutdown,
    }

    fn run_render_thread(
        format: PcmFormat,
        buffer: Arc<Mutex<AudioRingBuffer>>,
        volume: Arc<AtomicU32>,
        endpoint_queued_frames: Arc<AtomicU32>,
        signals: Arc<RecoverySignals>,
        commands: Receiver<WasapiRenderCommand>,
        init_tx: Sender<Result<()>>,
    ) -> Result<()> {
        let mut render_state = match initialize_render_client(format) {
            Ok(state) => {
                let _ = init_tx.send(Ok(()));
                Some(state)
            }
            Err(error) => {
                let _ = init_tx.send(Err(error));
                return Ok(());
            }
        };

        let mut playing = false;
        // Gain the previous render pass ended on; the ring buffer stays alive
        // across device-loss rebuilds, so the ramp continues seamlessly too.
        let mut last_applied_volume = f32::from_bits(volume.load(Ordering::Relaxed));
        let mut recovery_plan = RenderRecoveryPlan::new();
        let mut next_recovery_at: Option<Instant> = None;

        // Nothing on this thread may exit through a propagated error: the
        // producer keeps pushing into the ring buffer and would only see the
        // thread's death as silent drop-oldest starvation with runtime_stats
        // still reporting Stable. Every error therefore drops the client and
        // drives the bounded recovery schedule -- known device-loss HRESULTs
        // keep their code, anything else reports its own HRESULT the same way.
        let on_render_error = |error: WasapiAudioOutputError,
                               render_state: &mut Option<WasapiRenderState>,
                               recovery_plan: &mut RenderRecoveryPlan,
                               next_recovery_at: &mut Option<Instant>|
         -> Result<()> {
            let code = device_loss_code(&error).unwrap_or_else(|| error_hresult(&error));
            *render_state = None;
            endpoint_queued_frames.store(0, Ordering::Release);
            signals.mark_disconnected(code);
            recovery_plan.reset();
            match recovery_plan.next_step() {
                RecoveryStep::RetryAfter(delay) => {
                    *next_recovery_at = Some(Instant::now() + delay);
                }
                RecoveryStep::GiveUp => {
                    signals.recovery_failed(code);
                    *next_recovery_at = None;
                }
            }
            eprintln!("erika WASAPI render error (HRESULT 0x{code:08X}): {error}");
            Ok(())
        };

        loop {
            for command in commands.try_iter() {
                match command {
                    WasapiRenderCommand::Start => {
                        match &render_state {
                            Some(state) => {
                                if !playing && let Err(error) = state.client_start() {
                                    on_render_error(
                                        error,
                                        &mut render_state,
                                        &mut recovery_plan,
                                        &mut next_recovery_at,
                                    )?;
                                }
                            }
                            None => {
                                // A fresh Start re-arms an exhausted recovery
                                // schedule and retries at once. This must not
                                // be gated on `!playing`: a device loss leaves
                                // the logical state playing, so an exhausted
                                // schedule would otherwise stay disarmed and
                                // strand playback silent until the caller
                                // paused first.
                                if next_recovery_at.is_none() {
                                    recovery_plan.reset();
                                    let _ = recovery_plan.next_step();
                                    next_recovery_at = Some(Instant::now());
                                }
                            }
                        }
                        playing = true;
                    }
                    WasapiRenderCommand::Pause => {
                        if playing {
                            if let Some(state) = &render_state
                                && let Err(error) = state.client_stop()
                            {
                                on_render_error(
                                    error,
                                    &mut render_state,
                                    &mut recovery_plan,
                                    &mut next_recovery_at,
                                )?;
                            }
                            playing = false;
                        }
                    }
                    WasapiRenderCommand::Stop => {
                        if let Some(state) = &render_state {
                            let result = if playing {
                                state.client_stop().and_then(|()| state.client_reset())
                            } else {
                                state.client_reset()
                            };
                            if let Err(error) = result {
                                on_render_error(
                                    error,
                                    &mut render_state,
                                    &mut recovery_plan,
                                    &mut next_recovery_at,
                                )?;
                            }
                        }
                        endpoint_queued_frames.store(0, Ordering::Release);
                        playing = false;
                    }
                    WasapiRenderCommand::Shutdown => {
                        if playing && let Some(state) = &render_state {
                            let _ = state.client_stop();
                        }
                        endpoint_queued_frames.store(0, Ordering::Release);
                        return Ok(());
                    }
                }
            }

            if render_state.is_none()
                && let Some(deadline) = next_recovery_at
                && Instant::now() >= deadline
            {
                signals.begin_recovery();
                match reopen_render_client(format, playing) {
                    Ok(state) => {
                        render_state = Some(state);
                        next_recovery_at = None;
                        recovery_plan.reset();
                        signals.recovery_succeeded();
                    }
                    Err(error) => {
                        let code = error_hresult(&error);
                        match recovery_plan.next_step() {
                            RecoveryStep::RetryAfter(delay) => {
                                // Budget remains: report Disconnected and try
                                // again after the backed-off delay.
                                signals.mark_disconnected(code);
                                next_recovery_at = Some(Instant::now() + delay);
                            }
                            RecoveryStep::GiveUp => {
                                // Budget exhausted: stay Failed until a new
                                // Start command re-arms the schedule.
                                signals.recovery_failed(code);
                                next_recovery_at = None;
                            }
                        }
                        eprintln!("erika WASAPI device recovery failed: {error}");
                    }
                }
            }

            if playing && let Some(state) = &mut render_state {
                let target_volume = f32::from_bits(volume.load(Ordering::Relaxed));
                match state.render_available_frames(
                    &buffer,
                    last_applied_volume,
                    target_volume,
                    &endpoint_queued_frames,
                ) {
                    Ok(reached) => last_applied_volume = reached,
                    Err(error) => on_render_error(
                        error,
                        &mut render_state,
                        &mut recovery_plan,
                        &mut next_recovery_at,
                    )?,
                }
            }
            thread::sleep(RENDER_POLL_INTERVAL);
        }
    }

    /// Rebuilds the client against the current default endpoint after a device
    /// loss and, when playback was active, restarts it immediately. The ring
    /// buffer and volume are untouched so queued audio survives the swap.
    fn reopen_render_client(format: PcmFormat, playing: bool) -> Result<WasapiRenderState> {
        let state = initialize_render_client(format)?;
        if playing {
            state.client_start()?;
        }
        Ok(state)
    }

    fn error_hresult(error: &WasapiAudioOutputError) -> i32 {
        match error {
            WasapiAudioOutputError::Wasapi { code, .. } => *code,
            _ => 0,
        }
    }

    fn device_loss_code(error: &WasapiAudioOutputError) -> Option<i32> {
        let code = error_hresult(error);
        is_device_loss_hresult(code).then_some(code)
    }

    struct WasapiRenderState {
        client: IAudioClient,
        render_client: IAudioRenderClient,
        buffer_frames: u32,
        channels: usize,
        // Drop the COM interfaces before uninitializing the apartment.
        _com: ComApartment,
    }

    impl WasapiRenderState {
        fn client_start(&self) -> Result<()> {
            unsafe { self.client.Start() }
                .map_err(|error| wasapi_error("IAudioClient::Start", error))
        }

        fn client_stop(&self) -> Result<()> {
            unsafe { self.client.Stop() }.map_err(|error| wasapi_error("IAudioClient::Stop", error))
        }

        fn client_reset(&self) -> Result<()> {
            unsafe { self.client.Reset() }
                .map_err(|error| wasapi_error("IAudioClient::Reset", error))
        }

        /// Renders whatever the endpoint has room for, returning the gain the
        /// volume ramp actually reached so the next pass resumes from there.
        fn render_available_frames(
            &mut self,
            buffer: &Arc<Mutex<AudioRingBuffer>>,
            from_volume: f32,
            to_volume: f32,
            endpoint_queued_frames: &AtomicU32,
        ) -> Result<f32> {
            let padding = unsafe { self.client.GetCurrentPadding() }
                .map_err(|error| wasapi_error("IAudioClient::GetCurrentPadding", error))?;
            // Publish how much decoded PCM the endpoint still holds beyond the
            // ring buffer: the presenter's playback-rate transition bridge has
            // to cover it, or the rate switch lands ~100 ms early (the shared
            // poll model keeps this buffer nearly full).
            endpoint_queued_frames.store(padding, Ordering::Release);
            let frames = self.buffer_frames.saturating_sub(padding);
            if frames == 0 {
                // Nothing was written, so the ramp made no progress.
                return Ok(normalize_volume(from_volume));
            }

            let data = unsafe { self.render_client.GetBuffer(frames) }
                .map_err(|error| wasapi_error("IAudioRenderClient::GetBuffer", error))?;
            let sample_count = frames as usize * self.channels;
            let mut silent = false;
            let mut reached_volume = normalize_volume(from_volume);
            let result = (|| {
                let output = unsafe { slice::from_raw_parts_mut(data.cast::<f32>(), sample_count) };
                let read_result = {
                    let mut buffer = buffer
                        .lock()
                        .map_err(|_| WasapiAudioOutputError::LockPoisoned)?;
                    buffer.read_interleaved(output)?
                };
                // Ramp from the gain the previous pass ended on so an atomic
                // volume step never lands as a discontinuity (zipper noise).
                // Only the frames the ring supplied advance the ramp.
                reached_volume = apply_volume_ramp(
                    output,
                    self.channels,
                    from_volume,
                    to_volume,
                    read_result.frames,
                );
                silent = read_result.frames == 0;
                Ok(())
            })();

            let release_flags = if silent {
                AUDCLNT_BUFFERFLAGS_SILENT.0 as u32
            } else {
                0
            };
            let release_result = unsafe { self.render_client.ReleaseBuffer(frames, release_flags) }
                .map_err(|error| wasapi_error("IAudioRenderClient::ReleaseBuffer", error));
            result.and(release_result).map(|()| reached_volume)
        }
    }

    fn initialize_render_client(format: PcmFormat) -> Result<WasapiRenderState> {
        let _com = ComApartment::new()?;
        let wave_format = wave_format(format)?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| wasapi_error("CoCreateInstance(MMDeviceEnumerator)", error))?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|error| wasapi_error("IMMDeviceEnumerator::GetDefaultAudioEndpoint", error))?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|error| wasapi_error("IMMDevice::Activate(IAudioClient)", error))?;
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                WASAPI_BUFFER_DURATION_HNS,
                0,
                &wave_format,
                None,
            )
        }
        .map_err(|error| wasapi_error("IAudioClient::Initialize", error))?;
        let buffer_frames = unsafe { client.GetBufferSize() }
            .map_err(|error| wasapi_error("IAudioClient::GetBufferSize", error))?;
        let render_client: IAudioRenderClient = unsafe { client.GetService() }
            .map_err(|error| wasapi_error("IAudioClient::GetService(IAudioRenderClient)", error))?;

        Ok(WasapiRenderState {
            client,
            render_client,
            buffer_frames,
            channels: format.channels as usize,
            _com,
        })
    }

    fn wave_format(format: PcmFormat) -> Result<WAVEFORMATEX> {
        match format.sample_format {
            PcmSampleFormat::F32Interleaved => {}
        }
        if format.sample_rate == 0 || format.channels == 0 || format.channels > u16::MAX as u32 {
            return Err(WasapiAudioOutputError::InvalidFormat {
                sample_rate: format.sample_rate,
                channels: format.channels,
            });
        }
        let channels = format.channels as u16;
        let block_align = channels
            .checked_mul(std::mem::size_of::<f32>() as u16)
            .ok_or(WasapiAudioOutputError::InvalidFormat {
                sample_rate: format.sample_rate,
                channels: format.channels,
            })?;
        Ok(WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: channels,
            nSamplesPerSec: format.sample_rate,
            nAvgBytesPerSec: format.sample_rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: 0,
        })
    }

    struct ComApartment;

    impl ComApartment {
        fn new() -> Result<Self> {
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .map_err(|error| wasapi_error("CoInitializeEx", error))?;
            Ok(Self)
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }

    fn configure_buffer(buffer: &Arc<Mutex<AudioRingBuffer>>, format: PcmFormat) -> Result<()> {
        let mut buffer = buffer
            .lock()
            .map_err(|_| WasapiAudioOutputError::LockPoisoned)?;
        Ok(buffer.configure(format)?)
    }

    fn clear_buffer(buffer: &Arc<Mutex<AudioRingBuffer>>) -> Result<()> {
        let mut buffer = buffer
            .lock()
            .map_err(|_| WasapiAudioOutputError::LockPoisoned)?;
        buffer.clear();
        Ok(())
    }

    fn stop_render_thread(render_thread: &mut Option<WasapiRenderThread>) {
        if let Some(render_thread) = render_thread.take() {
            drop(render_thread);
        }
    }

    fn wasapi_error(
        operation: &'static str,
        error: ::windows::core::Error,
    ) -> WasapiAudioOutputError {
        WasapiAudioOutputError::Wasapi {
            operation,
            code: error.code().0,
            message: error.to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn stereo_format() -> PcmFormat {
            PcmFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_format: PcmSampleFormat::F32Interleaved,
            }
        }

        #[test]
        fn wave_format_uses_interleaved_float32_pcm() {
            let wave = wave_format(stereo_format()).unwrap();
            let tag = wave.wFormatTag;
            let channels = wave.nChannels;
            let sample_rate = wave.nSamplesPerSec;
            let block_align = wave.nBlockAlign;
            let avg_bytes_per_sec = wave.nAvgBytesPerSec;
            let bits_per_sample = wave.wBitsPerSample;

            assert_eq!(tag, WAVE_FORMAT_IEEE_FLOAT);
            assert_eq!(channels, 2);
            assert_eq!(sample_rate, 48_000);
            assert_eq!(block_align, 8);
            assert_eq!(avg_bytes_per_sec, 384_000);
            assert_eq!(bits_per_sample, 32);
        }

        #[test]
        fn volume_is_clamped_and_applied_to_pcm_samples() {
            let mut output = WasapiAudioOutput::default();

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
