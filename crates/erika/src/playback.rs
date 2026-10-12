use std::collections::VecDeque;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{
    Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError, bounded, unbounded,
};
use thiserror::Error;

use crate::audio::AudioClockSnapshot;
use crate::core::{
    MediaRequest, MediaSourceHint, TrackInfo, TrackKind, TrackSelection, VideoDecoderEvent,
    VideoFrameImportFailure, VideoParams,
};
use crate::ffmpeg::{
    self, AudioResampler, Decoder, DecoderBackend, DecoderConfig, DecoderOutputFrame, Demuxer,
    DoviRejectReason, Frame, OwnedCodecParameters, PcmAudioFrame, PcmFormat, StreamSelection,
    SubtitleDecoder,
};
use crate::source::{self, source_from_uri_with_hint, source_from_uri_with_options};
use crate::subtitle::{
    DecodedSubtitleFrame, SubtitleFontAttachment, SubtitleTrackConfig, SubtitleTrackSource,
};
use crate::trace;

#[derive(Debug, Error)]
pub enum PlaybackError {
    #[error("ffmpeg error: {0}")]
    Ffmpeg(#[from] ffmpeg::FfmpegError),
    #[error("source error: {0}")]
    Source(#[from] source::SourceError),
    #[error("no video track found")]
    NoVideoTrack,
    #[error("video decoder unavailable: {reason}")]
    VideoDecoderUnavailable { reason: String },
    #[error("selected decoder output is not a video frame")]
    UnexpectedDecoderOutput,
    #[error("no subtitle track found")]
    NoSubtitleTrack,
    #[error("track not found: kind={kind:?} id={track_id}")]
    TrackNotFound { kind: TrackKind, track_id: i64 },
    #[error("subtitle track is not removable: {0}")]
    SubtitleTrackNotRemovable(i64),
    #[error("demux worker error: {0}")]
    DemuxWorker(String),
    #[error("decoder EOF drain timed out: {reason}")]
    DecoderDrainTimeout { reason: String },
    #[error("decoder input stalled: {reason}")]
    DecoderInputStall { reason: String },
    #[error("audio output stalled at EOF: {reason}")]
    AudioOutputStall { reason: String },
    #[error("no decodable audio track found: {reason}")]
    AudioDecoderUnavailable { reason: String },
}

pub type Result<T> = std::result::Result<T, PlaybackError>;

const DEFAULT_VIDEO_FRAME_QUEUE_LIMIT: usize = 8;
const DEFAULT_AUDIO_FRAME_QUEUE_LIMIT: usize = 16;
/// Ceiling on decoded audio frames a *video* demux request may accumulate
/// while scanning past interleaved audio packets. At ~1024 samples per frame
/// and 48 kHz one frame is roughly 21 ms, so this is about 11 s of audio:
/// wider than any sane interleave distance, yet still a bound (~4 MB).
const VIDEO_DEMAND_AUDIO_FRAME_CEILING: usize = 512;
const STREAMING_VIDEO_FRAME_QUEUE_LIMIT: usize = 48;
const STREAMING_AUDIO_FRAME_QUEUE_LIMIT: usize = 128;
const D3D11VA_VIDEO_FRAME_QUEUE_LIMIT: usize = 8;
// NativeImage typically exposes a triple-buffered producer queue. Keep one
// slot free so decoder prebuffering cannot deadlock before the presenter calls
// OH_NativeImage_UpdateSurfaceImage for the first frame.
const AVCODEC_SURFACE_VIDEO_FRAME_QUEUE_LIMIT: usize = 2;
const SUBTITLE_FRAME_QUEUE_LIMIT: usize = 32;
const EXTERNAL_SUBTITLE_LOOKAHEAD: Duration = Duration::from_secs(5);
const DEFAULT_AUDIO_LEAD_TIME: Duration = Duration::from_millis(120);
const STREAMING_AUDIO_LEAD_TIME: Duration = Duration::from_millis(1500);
const DEFAULT_BUFFER_RECOVERY_AUDIO: Duration = Duration::from_millis(100);
const STREAMING_BUFFER_RECOVERY_AUDIO: Duration = Duration::from_millis(250);
// The worker polls Playing every 2 ms. Pace buffering drains so video cannot
// race far ahead while the audio output reports its next recovery snapshot.
const BUFFERING_VIDEO_DRAIN_INTERVAL: Duration = Duration::from_millis(8);
const BUFFERING_VIDEO_MAX_ADVANCE: Duration = Duration::from_millis(500);
const BUFFERING_VIDEO_MAX_DRAINED_FRAMES: usize = 64;
// When a surface decoder temporarily rejects a packet, audio demand may scan
// past a bounded run of interleaved video packets instead of letting that one
// packet stall audio prefill and EOF discovery. The packets remain compressed
// and ordered for later video decoding; the cap bounds memory and prevents an
// ordinary audio-only consumer from reading arbitrarily far ahead. Buffering
// recovery may advance beyond this queue cap by releasing decoded video frames
// under the stricter scan watchdog below.
const AUDIO_DEMAND_PENDING_VIDEO_PACKET_LIMIT: usize = 64;
// Buffering recovery is allowed to discard queued decoded video frames so it
// can scan past an unusually long run of interleaved video packets and reach
// audio again. Keep that exceptional scan finite even for a sparse, finished,
// or corrupt audio track; exhaustion falls back to video-clock playback rather
// than turning valid video-only tails into fatal errors.
const BUFFERING_AUDIO_VIDEO_SCAN_PACKET_LIMIT: usize = 4096;
const BUFFERING_AUDIO_VIDEO_SCAN_BYTE_LIMIT: usize = 256 * 1024 * 1024;
const BUFFERING_AUDIO_VIDEO_SCAN_TIMEOUT: Duration = Duration::from_secs(5);
const DEMUX_PACKET_QUEUE_LIMIT: usize = 512;
const SEEK_PREROLL_DECODE_TIME_BUDGET: Duration = Duration::from_millis(5);
const SEEK_PREROLL_LOG_INTERVAL: Duration = Duration::from_millis(500);
const VIDEO_PUMP_PACKET_BUDGET: usize = 64;
const VIDEO_PUMP_TIME_BUDGET: Duration = Duration::from_millis(5);
const AUDIO_SEEK_INVALID_PACKET_LIMIT: usize = 64;
const DECODER_EOF_DRAIN_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const DECODER_INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const AUDIO_EOF_OUTPUT_STALL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlaybackQueueLimits {
    video_frames: usize,
    audio_frames: usize,
    subtitle_frames: usize,
}

impl PlaybackQueueLimits {
    fn for_request(request: &MediaRequest) -> Self {
        if request_uses_http_source(request) {
            Self {
                video_frames: STREAMING_VIDEO_FRAME_QUEUE_LIMIT,
                audio_frames: STREAMING_AUDIO_FRAME_QUEUE_LIMIT,
                subtitle_frames: SUBTITLE_FRAME_QUEUE_LIMIT,
            }
        } else {
            Self::default()
        }
    }
}

impl Default for PlaybackQueueLimits {
    fn default() -> Self {
        Self {
            video_frames: DEFAULT_VIDEO_FRAME_QUEUE_LIMIT,
            audio_frames: DEFAULT_AUDIO_FRAME_QUEUE_LIMIT,
            subtitle_frames: SUBTITLE_FRAME_QUEUE_LIMIT,
        }
    }
}

enum DemuxMessage {
    Packet {
        generation: u64,
        packet: ffmpeg::Packet,
    },
    Eof {
        generation: u64,
    },
    Error {
        generation: u64,
        message: String,
    },
}

enum DemuxCommand {
    Start {
        generation: u64,
    },
    SetSelection {
        generation: u64,
        selection: StreamSelection,
    },
    Seek {
        generation: u64,
        position: Duration,
        io_generation: Option<u64>,
    },
    ReleaseBuffers {
        stale_packets: Receiver<DemuxMessage>,
        reply: Sender<()>,
    },
    Stop,
}

enum PumpInput {
    Packet(ffmpeg::Packet),
    Eof,
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackPumpDemand {
    Video,
    Audio,
    BufferingAudio,
}

impl PlaybackPumpDemand {
    fn is_audio(self) -> bool {
        matches!(self, Self::Audio | Self::BufferingAudio)
    }
}

struct AsyncDemuxer {
    packets: Receiver<DemuxMessage>,
    commands: Sender<DemuxCommand>,
    generation: u64,
    active: bool,
    worker: Option<thread::JoinHandle<()>>,
    cancellation: Option<source::SourceCancellation>,
}

impl AsyncDemuxer {
    fn spawn(demuxer: Demuxer) -> Self {
        let cancellation = demuxer.source_cancellation();
        let (packet_sender, packets) = bounded(DEMUX_PACKET_QUEUE_LIMIT);
        let (commands, command_receiver) = unbounded();
        let worker = thread::Builder::new()
            .name("erika-demux".to_string())
            .spawn(move || run_demux_worker(demuxer, packet_sender, command_receiver))
            .expect("spawn erika demux worker");
        Self {
            packets,
            commands,
            generation: 1,
            active: false,
            worker: Some(worker),
            cancellation,
        }
    }

    fn start(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        self.commands
            .send(DemuxCommand::Start {
                generation: self.generation,
            })
            .map_err(|_| PlaybackError::DemuxWorker("demux command channel closed".to_string()))?;
        self.active = true;
        Ok(())
    }

    fn set_stream_selection(&mut self, selection: StreamSelection) -> Result<()> {
        self.generation = self.generation.saturating_add(1).max(1);
        self.active = false;
        self.drain_stale_packets();
        self.commands
            .send(DemuxCommand::SetSelection {
                generation: self.generation,
                selection,
            })
            .map_err(|_| PlaybackError::DemuxWorker("demux command channel closed".to_string()))
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        let io_generation = self
            .cancellation
            .as_ref()
            .map(source::SourceCancellation::interrupt);
        self.generation = self.generation.saturating_add(1).max(1);
        self.active = false;
        self.drain_stale_packets();
        self.commands
            .send(DemuxCommand::Seek {
                generation: self.generation,
                position,
                io_generation,
            })
            .map_err(|_| PlaybackError::DemuxWorker("demux command channel closed".to_string()))
    }

    fn poll(&mut self) -> Result<PumpInput> {
        self.start()?;
        loop {
            match self.packets.try_recv() {
                Ok(DemuxMessage::Packet { generation, packet })
                    if generation == self.generation =>
                {
                    return Ok(PumpInput::Packet(packet));
                }
                Ok(DemuxMessage::Eof { generation }) if generation == self.generation => {
                    return Ok(PumpInput::Eof);
                }
                Ok(DemuxMessage::Error {
                    generation,
                    message,
                }) if generation == self.generation => {
                    return Err(PlaybackError::DemuxWorker(message));
                }
                Ok(_) => continue,
                Err(TryRecvError::Empty) => return Ok(PumpInput::Empty),
                Err(TryRecvError::Disconnected) => {
                    return Err(PlaybackError::DemuxWorker(
                        "demux packet channel closed".to_string(),
                    ));
                }
            }
        }
    }

    fn drain_stale_packets(&mut self) {
        while self.packets.try_recv().is_ok() {}
    }

    fn release_buffers(&self) -> Result<()> {
        let (reply, released) = bounded(1);
        self.commands
            .send(DemuxCommand::ReleaseBuffers {
                stale_packets: self.packets.clone(),
                reply,
            })
            .map_err(|_| PlaybackError::DemuxWorker("demux command channel closed".to_string()))?;
        released
            .recv()
            .map_err(|_| PlaybackError::DemuxWorker("demux worker stopped".to_string()))
    }
}

impl Drop for AsyncDemuxer {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
        let _ = self.commands.send(DemuxCommand::Stop);
        if let Some(worker) = self.worker.take() {
            while !worker.is_finished() {
                if matches!(
                    self.packets.recv_timeout(Duration::from_millis(10)),
                    Err(RecvTimeoutError::Disconnected)
                ) {
                    break;
                }
            }
            let _ = worker.join();
        }
    }
}

fn run_demux_worker(
    mut demuxer: Demuxer,
    packets: Sender<DemuxMessage>,
    commands: Receiver<DemuxCommand>,
) {
    let mut generation = 1u64;
    let mut eof = false;
    let mut active = false;
    let mut pending_seek = None;
    loop {
        while let Ok(command) = commands.try_recv() {
            if !handle_demux_command(
                &mut demuxer,
                &packets,
                command,
                &mut generation,
                &mut eof,
                &mut active,
                &mut pending_seek,
            ) {
                return;
            }
        }

        if eof || !active {
            match commands.recv() {
                Ok(command) => {
                    if !handle_demux_command(
                        &mut demuxer,
                        &packets,
                        command,
                        &mut generation,
                        &mut eof,
                        &mut active,
                        &mut pending_seek,
                    ) {
                        return;
                    }
                }
                Err(_) => return,
            }
            continue;
        }

        let read_started = Instant::now();
        let message = match demuxer.read_packet() {
            Ok(Some(packet)) => {
                let read_elapsed = read_started.elapsed();
                if trace::enabled() && read_elapsed > Duration::from_millis(20) {
                    trace::log(format!(
                        "[erika-playback-trace] stage=demux_read_packet gen={} stream={} elapsed_ms={:.3} queue_len={}",
                        generation,
                        packet.stream_index(),
                        read_elapsed.as_secs_f64() * 1000.0,
                        packets.len(),
                    ));
                }
                DemuxMessage::Packet { generation, packet }
            }
            Ok(None) => {
                eof = true;
                trace::log(format!(
                    "[erika-playback-trace] stage=demux_read_eof gen={} elapsed_ms={:.3} queue_len={}",
                    generation,
                    read_started.elapsed().as_secs_f64() * 1000.0,
                    packets.len(),
                ));
                DemuxMessage::Eof { generation }
            }
            Err(error) => {
                eof = true;
                trace::log(format!(
                    "[erika-playback-trace] stage=demux_read_error gen={} elapsed_ms={:.3} queue_len={} error={}",
                    generation,
                    read_started.elapsed().as_secs_f64() * 1000.0,
                    packets.len(),
                    error,
                ));
                DemuxMessage::Error {
                    generation,
                    message: error.to_string(),
                }
            }
        };
        if !send_demux_message(
            &mut demuxer,
            &packets,
            &commands,
            message,
            &mut generation,
            &mut eof,
            &mut active,
            &mut pending_seek,
        ) {
            return;
        }
    }
}

fn send_demux_message(
    demuxer: &mut Demuxer,
    packets: &Sender<DemuxMessage>,
    commands: &Receiver<DemuxCommand>,
    mut message: DemuxMessage,
    generation: &mut u64,
    eof: &mut bool,
    active: &mut bool,
    pending_seek: &mut Option<(u64, Duration)>,
) -> bool {
    let message_generation = demux_message_generation(&message);
    let started = Instant::now();
    loop {
        while let Ok(command) = commands.try_recv() {
            trace_demux_send_wait(started, packets.len(), "command");
            if !handle_demux_command(
                demuxer,
                packets,
                command,
                generation,
                eof,
                active,
                pending_seek,
            ) {
                return false;
            }
            if !*active || *generation != message_generation {
                return true;
            }
        }

        match packets.try_send(message) {
            Ok(()) => {
                trace_demux_send_wait(started, packets.len(), "sent");
                return true;
            }
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(returned)) => {
                message = returned;
                match commands.recv_timeout(Duration::from_millis(5)) {
                    Ok(command) => {
                        trace_demux_send_wait(started, packets.len(), "command_after_full");
                        if !handle_demux_command(
                            demuxer,
                            packets,
                            command,
                            generation,
                            eof,
                            active,
                            pending_seek,
                        ) {
                            return false;
                        }
                        if !*active || *generation != message_generation {
                            return true;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return false,
                }
            }
        }
    }
}

fn demux_message_generation(message: &DemuxMessage) -> u64 {
    match message {
        DemuxMessage::Packet { generation, .. }
        | DemuxMessage::Eof { generation }
        | DemuxMessage::Error { generation, .. } => *generation,
    }
}

fn trace_demux_send_wait(started: Instant, queue_len: usize, outcome: &'static str) {
    if trace::enabled() && started.elapsed() > Duration::from_millis(20) {
        trace::log(format!(
            "[erika-playback-trace] stage=demux_send_wait outcome={} elapsed_ms={:.3} queue_len={}",
            outcome,
            started.elapsed().as_secs_f64() * 1000.0,
            queue_len,
        ));
    }
}

fn handle_demux_command(
    demuxer: &mut Demuxer,
    packets: &Sender<DemuxMessage>,
    command: DemuxCommand,
    generation: &mut u64,
    eof: &mut bool,
    active: &mut bool,
    pending_seek: &mut Option<(u64, Duration)>,
) -> bool {
    match command {
        DemuxCommand::Start {
            generation: next_generation,
        } => {
            *generation = next_generation;
            if let Some((seek_generation, position)) = pending_seek.take() {
                *generation = seek_generation;
                *eof = false;
                if let Err(error) = demuxer.seek(position) {
                    let _ = packets.send(DemuxMessage::Error {
                        generation: *generation,
                        message: error.to_string(),
                    });
                    *eof = true;
                    *active = false;
                    return true;
                }
            }
            *active = true;
            true
        }
        DemuxCommand::SetSelection {
            generation: next_generation,
            selection,
        } => {
            *generation = next_generation;
            *eof = false;
            *active = false;
            if let Some((seek_generation, position)) = pending_seek.as_mut() {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "demux_pending_seek",
                        "stage": "preserved_across_stream_selection",
                        "seekGeneration": *seek_generation,
                        "selectionGeneration": next_generation,
                        "targetSeconds": position.as_secs_f64(),
                    })
                    .to_string(),
                );
                // A track selection can race with the first resume seek before
                // the demux worker receives Start. Keep that seek, but retarget
                // its packet generation to the new selection so the caller does
                // not discard every packet as stale.
                *seek_generation = next_generation;
            }
            if let Err(error) = demuxer.set_stream_selection(selection) {
                let _ = packets.send(DemuxMessage::Error {
                    generation: *generation,
                    message: error.to_string(),
                });
                *eof = true;
            }
            true
        }
        DemuxCommand::Seek {
            generation: next_generation,
            position,
            io_generation,
        } => {
            if let (Some(cancellation), Some(io_generation)) =
                (demuxer.source_cancellation(), io_generation)
            {
                cancellation.resume(io_generation);
            }
            *generation = next_generation;
            *eof = false;
            *active = false;
            *pending_seek = Some((*generation, position));
            true
        }
        DemuxCommand::ReleaseBuffers {
            stale_packets,
            reply,
        } => {
            while stale_packets.try_recv().is_ok() {}
            demuxer.release_source_buffer();
            let _ = reply.send(());
            true
        }
        DemuxCommand::Stop => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoDecodePreference {
    Software,
    LinuxAuto,
    Cuda,
    Vaapi,
    VideoToolbox,
    D3d11va,
    MediaCodec,
    MediaCodecByteBuffer,
    AvCodec,
}

impl VideoDecodePreference {
    fn decoder_config(self) -> DecoderConfig {
        match self {
            Self::Software => DecoderConfig::software(),
            Self::LinuxAuto | Self::Cuda => DecoderConfig::cuda(),
            Self::Vaapi => DecoderConfig::vaapi(),
            Self::VideoToolbox => DecoderConfig::videotoolbox(),
            Self::D3d11va => DecoderConfig::d3d11va(),
            Self::MediaCodec => DecoderConfig::mediacodec(),
            Self::MediaCodecByteBuffer => DecoderConfig::mediacodec_byte_buffer(),
            Self::AvCodec => DecoderConfig::avcodec(),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
impl Default for VideoDecodePreference {
    fn default() -> Self {
        Self::VideoToolbox
    }
}

#[cfg(target_os = "windows")]
impl Default for VideoDecodePreference {
    fn default() -> Self {
        Self::D3d11va
    }
}

#[cfg(target_os = "android")]
impl Default for VideoDecodePreference {
    fn default() -> Self {
        Self::MediaCodec
    }
}

#[cfg(target_env = "ohos")]
impl Default for VideoDecodePreference {
    fn default() -> Self {
        Self::AvCodec
    }
}

#[cfg(not(any(
    target_os = "macos",
    any(target_os = "ios", target_os = "tvos"),
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_env = "ohos"
)))]
impl Default for VideoDecodePreference {
    fn default() -> Self {
        Self::Software
    }
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
impl Default for VideoDecodePreference {
    fn default() -> Self {
        match std::env::var("ERIKA_HWDEC")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "cuda" | "nvdec" => Self::Cuda,
            "vaapi" => Self::Vaapi,
            "software" | "none" => Self::Software,
            _ => Self::LinuxAuto,
        }
    }
}

fn require_hardware_decode() -> bool {
    cfg!(all(target_os = "linux", not(target_env = "ohos")))
        && std::env::var("ERIKA_REQUIRE_HARDWARE_DECODE").is_ok_and(|value| value == "1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackSessionConfig {
    pub video_decode: VideoDecodePreference,
    pub audio_output: PcmFormat,
    pub timing: PlaybackTimingConfig,
}

#[derive(Clone, Default)]
pub(crate) struct PlaybackDecoderResources {
    #[cfg(target_env = "ohos")]
    ohos_avcodec_surface: Option<Arc<crate::ohos::avcodec::OhosAvCodecSurface>>,
}

impl PlaybackDecoderResources {
    #[cfg(target_env = "ohos")]
    pub(crate) fn with_ohos_avcodec_surface(
        surface: Option<Arc<crate::ohos::avcodec::OhosAvCodecSurface>>,
    ) -> Self {
        Self {
            ohos_avcodec_surface: surface,
        }
    }
}

impl Default for PlaybackSessionConfig {
    fn default() -> Self {
        Self {
            video_decode: VideoDecodePreference::default(),
            audio_output: PcmFormat::default(),
            timing: PlaybackTimingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenedMediaInfo {
    pub uri: String,
    pub duration: Option<Duration>,
    pub tracks: Vec<TrackInfo>,
    pub video_params: Option<VideoParams>,
    pub selected_video_track: Option<i64>,
    pub selected_audio_track: Option<i64>,
    pub selected_subtitle_track: Option<i64>,
    pub subtitle_tracks: Vec<SubtitleTrackConfig>,
    pub video_decode_backend: Option<DecoderBackend>,
    pub audio_output: Option<PcmFormat>,
}

impl OpenedMediaInfo {
    pub fn track_selection(&self) -> TrackSelection {
        TrackSelection {
            video: self.selected_video_track,
            audio: self.selected_audio_track,
            subtitle: self.selected_subtitle_track,
        }
    }
}

struct DecodedVideoFrame {
    frame: Frame,
    decode_backend: DecoderBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaCodecFallbackTarget {
    ByteBuffer,
    Software,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaCodecSeekRoute {
    Surface,
    ByteBuffer,
}

impl MediaCodecSeekRoute {
    fn decoder_config(self) -> DecoderConfig {
        match self {
            Self::Surface => DecoderConfig::mediacodec(),
            Self::ByteBuffer => DecoderConfig::mediacodec_byte_buffer(),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Surface => "surface_ahardwarebuffer",
            Self::ByteBuffer => "bytebuffer_cpu_upload",
        }
    }
}

impl DecodedVideoFrame {
    fn pts(&self) -> Option<ffmpeg::PacketTimestamp> {
        self.frame.pts()
    }
}

pub struct PlaybackSession {
    demuxer: AsyncDemuxer,
    codec_parameters: Vec<OwnedCodecParameters>,
    video_decoder: Option<Decoder>,
    #[cfg(target_env = "ohos")]
    decoder_resources: PlaybackDecoderResources,
    video_decoder_unavailable_reason: Option<String>,
    audio_decoder: Option<Decoder>,
    subtitle_decoder: Option<SubtitleDecoder>,
    subtitle_fonts: Arc<[SubtitleFontAttachment]>,
    external_subtitles: Vec<ExternalSubtitleSession>,
    audio_resampler: Option<AudioResampler>,
    audio_output: PcmFormat,
    audio_output_active: bool,
    video_decode_suspended: bool,
    info: OpenedMediaInfo,
    video_frames: VecDeque<DecodedVideoFrame>,
    audio_frames: VecDeque<PcmAudioFrame>,
    subtitle_frames: VecDeque<DecodedSubtitleFrame>,
    pending_video_packets: VecDeque<ffmpeg::Packet>,
    video_fallback_waiting_for_keyframe: bool,
    audio_seek_waiting_for_valid_packet: bool,
    audio_seek_dropped_packets: usize,
    mediacodec_surface_disabled: bool,
    video_decoder_fallbacks: u64,
    dolby_vision_profile: Option<u8>,
    dovi_rpu_diagnostics: DoviRpuDiagnostics,
    video_decoder_events: VecDeque<VideoDecoderEvent>,
    queue_limits: PlaybackQueueLimits,
    buffer_recovery_audio: Duration,
    buffering_audio_video_scan: BufferingAudioVideoScan,
    buffering_audio_recovery_suspended: bool,
    demux_eof: bool,
    video_packet_stall_polls: u64,
    video_packet_stall_logged: bool,
    video_packet_stall_started_at: Option<Instant>,
    eof_drain_polls: u64,
    eof_drain_pending_logged: bool,
    eof_drain_last_progress_at: Option<Instant>,
    eof: bool,
}

#[derive(Debug, Default)]
struct BufferingAudioVideoScan {
    active_time: Duration,
    packets: usize,
    bytes: usize,
    discarded_frames: usize,
    baseline_media_time: Option<Duration>,
}

fn open_preferred_video_decoder(
    parameters: &OwnedCodecParameters,
    preference: VideoDecodePreference,
    config: DecoderConfig,
    resources: &PlaybackDecoderResources,
) -> ffmpeg::Result<Decoder> {
    if config.backend == DecoderBackend::Software && require_hardware_decode() {
        return Err(ffmpeg::FfmpegError::Source(
            "hardware decoding is required; software decoder disabled".into(),
        ));
    }
    if preference == VideoDecodePreference::LinuxAuto && config.backend != DecoderBackend::Software
    {
        let mut last_error = None;
        for candidate in [DecoderConfig::cuda(), DecoderConfig::vaapi()] {
            match open_video_decoder(parameters, candidate, resources) {
                Ok(decoder) => return Ok(decoder),
                Err(error) => {
                    trace::diagnostic(
                        serde_json::json!({
                            "event": "linux_hardware_decoder", "stage": "device_unavailable",
                            "backend": candidate.backend.as_str(), "reason": error.to_string(),
                        })
                        .to_string(),
                    );
                    last_error = Some(error);
                }
            }
        }
        return Err(last_error.expect("hardware candidate list is nonempty"));
    }
    open_video_decoder(parameters, config, resources)
}

fn open_video_decoder(
    parameters: &OwnedCodecParameters,
    config: DecoderConfig,
    resources: &PlaybackDecoderResources,
) -> ffmpeg::Result<Decoder> {
    #[cfg(target_env = "ohos")]
    if config.backend == DecoderBackend::AvCodec {
        return Decoder::open_owned_with_ohos_avcodec_surface(
            parameters,
            resources.ohos_avcodec_surface.clone(),
        );
    }
    #[cfg(not(target_env = "ohos"))]
    let _ = resources;
    Decoder::open_owned_with_config(parameters, config)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DecoderDrainStatus {
    frames: usize,
    end_of_stream: bool,
}

impl DecoderDrainStatus {
    fn made_progress(self) -> bool {
        self.frames > 0 || self.end_of_stream
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DiscardedPlaybackQueues {
    video_frames: usize,
    audio_frames: usize,
    subtitle_frames: usize,
    video_packets: usize,
}

fn trace_discarded_playback_queues(
    stage: &'static str,
    discarded: DiscardedPlaybackQueues,
    decoder_alive: bool,
) {
    if discarded == DiscardedPlaybackQueues::default() {
        return;
    }
    trace::diagnostic(
        serde_json::json!({
            "event": "playback_queue_cleanup",
            "stage": stage,
            "discardedVideoFrames": discarded.video_frames,
            "discardedAudioFrames": discarded.audio_frames,
            "discardedSubtitleFrames": discarded.subtitle_frames,
            "discardedVideoPackets": discarded.video_packets,
            "decoderAliveDuringCleanup": decoder_alive,
        })
        .to_string(),
    );
}

unsafe impl Send for PlaybackSession {}

impl Drop for PlaybackSession {
    fn drop(&mut self) {
        // MediaCodec Surface frames keep an AVBuffer release callback that
        // calls back into the decoder's MediaCodec context. Release every
        // queued frame while video_decoder is still alive; Rust otherwise
        // drops fields in declaration order and would destroy the decoder
        // before video_frames.
        let decoder_alive = self.video_decoder.is_some();
        let discarded = self.discard_queued_frames_and_packets();
        trace_discarded_playback_queues("session_drop", discarded, decoder_alive);
    }
}

impl PlaybackSession {
    pub fn open(request: &MediaRequest, config: PlaybackSessionConfig) -> Result<Self> {
        Self::open_with_decoder_resources(request, config, PlaybackDecoderResources::default())
    }

    pub(crate) fn open_with_decoder_resources(
        request: &MediaRequest,
        config: PlaybackSessionConfig,
        decoder_resources: PlaybackDecoderResources,
    ) -> Result<Self> {
        let queue_limits = PlaybackQueueLimits::for_request(request);
        let source = source_from_uri_with_options(
            &request.uri,
            request.source_hint,
            request.http_headers.clone(),
            request.http_read_ahead_bytes,
            request.http_back_buffer_bytes,
        )?;
        let mut demuxer = Demuxer::open_source(source)?;
        let mut probe = demuxer.probe().clone();
        let subtitle_fonts = probe.subtitle_fonts.clone();
        let codec_parameters = probe
            .tracks
            .iter()
            .map(|track| demuxer.owned_codec_parameters(track.id as i32))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let selected_video_track = demuxer
            .probe()
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .map(|track| track.id as i32);
        let audio_track_candidates = demuxer
            .probe()
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
            .map(|track| track.id as i32)
            .collect::<Vec<_>>();
        let selected_subtitle_track = demuxer
            .probe()
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Subtitle)
            .map(|track| track.id as i32);

        let mut video_decoder = None;
        let mut video_decoder_fallbacks = 0u64;
        let mut video_decoder_events = VecDeque::new();
        let mut dolby_vision_profile = None;
        let mut selected_streams = Vec::new();
        if let Some(stream_index) = selected_video_track {
            selected_streams.push(stream_index);
            let parameters = codec_parameters_for(&codec_parameters, stream_index)?;
            let codec = parameters.codec_name();
            let requested_config = config.video_decode.decoder_config();
            dolby_vision_profile = parameters.dolby_vision_profile();
            let (decoder_config, decode_fallback_reason) =
                dolby_vision_decode_fallback(dolby_vision_profile, requested_config);
            video_decoder = Some(
                match open_preferred_video_decoder(
                    parameters,
                    config.video_decode,
                    decoder_config,
                    &decoder_resources,
                ) {
                    Ok(decoder) => {
                        let event = VideoDecoderEvent {
                            stage: video_decoder_open_stage(decoder_config).to_string(),
                            requested_backend: requested_config.backend,
                            previous_backend: None,
                            active_backend: decoder.backend(),
                            fallback_count: 0,
                            codec: codec.clone(),
                            pixel_format: None,
                            line_sizes: None,
                            reason: decode_fallback_reason,
                        };
                        trace::diagnostic(event.structured_message());
                        video_decoder_events.push_back(event);
                        decoder
                    }
                    Err(error) if should_fallback_video_decoder_error(decoder_config.backend) => {
                        let surface_error = error.to_string();
                        if decoder_config.backend == DecoderBackend::MediaCodec
                            && decoder_config.mediacodec_surface
                        {
                            match Decoder::open_owned_with_config(
                                parameters,
                                DecoderConfig::mediacodec_byte_buffer(),
                            ) {
                                Ok(decoder) => {
                                    video_decoder_fallbacks = 1;
                                    trace::diagnostic(
                                        serde_json::json!({
                                            "event": "android_mediacodec_fallback",
                                            "stage": "open_surface_to_bytebuffer",
                                            "fromMode": "surface_ahardwarebuffer",
                                            "toMode": "bytebuffer_cpu_upload",
                                            "fallbackCount": video_decoder_fallbacks,
                                            "reason": surface_error.as_str(),
                                        })
                                        .to_string(),
                                    );
                                    let event = VideoDecoderEvent {
                                        stage: "open_surface_to_bytebuffer".to_string(),
                                        requested_backend: decoder_config.backend,
                                        previous_backend: Some(decoder_config.backend),
                                        active_backend: decoder.backend(),
                                        fallback_count: video_decoder_fallbacks,
                                        codec: codec.clone(),
                                        pixel_format: None,
                                        line_sizes: None,
                                        reason: Some(surface_error),
                                    };
                                    trace::diagnostic(event.structured_message());
                                    video_decoder_events.push_back(event);
                                    decoder
                                }
                                Err(byte_buffer_error) => {
                                    let byte_buffer_error = byte_buffer_error.to_string();
                                    video_decoder_fallbacks = 2;
                                    let decoder = match Decoder::open_owned_with_config(
                                        parameters,
                                        DecoderConfig::software(),
                                    ) {
                                        Ok(decoder) => decoder,
                                        Err(software_error) => {
                                            return Err(video_decoder_open_unavailable_error(
                                                "open_software_failed",
                                                decoder_config.backend,
                                                stream_index,
                                                codec.as_deref(),
                                                video_decoder_fallbacks,
                                                format!(
                                                    "MediaCodec Surface open failed: {surface_error}; MediaCodec byte-buffer open failed: {byte_buffer_error}; software decoder open failed: {software_error}"
                                                ),
                                            ));
                                        }
                                    };
                                    trace::diagnostic(
                                        serde_json::json!({
                                            "event": "android_mediacodec_fallback",
                                            "stage": "open_bytebuffer_to_software",
                                            "fromMode": "bytebuffer_cpu_upload",
                                            "toMode": "software_decode",
                                            "fallbackCount": video_decoder_fallbacks,
                                            "surfaceReason": surface_error.as_str(),
                                            "reason": byte_buffer_error.as_str(),
                                        })
                                        .to_string(),
                                    );
                                    let event = VideoDecoderEvent {
                                        stage: "open_bytebuffer_to_software".to_string(),
                                        requested_backend: decoder_config.backend,
                                        previous_backend: Some(decoder_config.backend),
                                        active_backend: decoder.backend(),
                                        fallback_count: video_decoder_fallbacks,
                                        codec: codec.clone(),
                                        pixel_format: None,
                                        line_sizes: None,
                                        reason: Some(format!(
                                            "surface mode failed: {surface_error}; MediaCodec byte-buffer mode failed: {byte_buffer_error}"
                                        )),
                                    };
                                    trace::diagnostic(event.structured_message());
                                    video_decoder_events.push_back(event);
                                    decoder
                                }
                            }
                        } else {
                            video_decoder_fallbacks = 1;
                            let decoder = match Decoder::open_owned_with_config(
                                parameters,
                                DecoderConfig::software(),
                            ) {
                                Ok(decoder) => decoder,
                                Err(software_error) => {
                                    let stage =
                                        if decoder_config.backend == DecoderBackend::MediaCodec {
                                            "open_bytebuffer_to_software_failed"
                                        } else {
                                            "open_software_fallback_failed"
                                        };
                                    return Err(video_decoder_open_unavailable_error(
                                        stage,
                                        decoder_config.backend,
                                        stream_index,
                                        codec.as_deref(),
                                        video_decoder_fallbacks,
                                        format!(
                                            "{} decoder open failed: {surface_error}; software decoder open failed: {software_error}",
                                            decoder_config.backend.as_str(),
                                        ),
                                    ));
                                }
                            };
                            let stage = if decoder_config.backend == DecoderBackend::MediaCodec {
                                #[cfg(target_os = "android")]
                                trace::diagnostic(
                                    serde_json::json!({
                                        "event": "android_mediacodec_fallback",
                                        "stage": "open_bytebuffer_to_software",
                                        "fromMode": "bytebuffer_cpu_upload",
                                        "toMode": "software_decode",
                                        "surfaceZeroCopyDisabled": true,
                                        "fallbackCount": video_decoder_fallbacks,
                                        "reason": surface_error.as_str(),
                                    })
                                    .to_string(),
                                );
                                "open_bytebuffer_to_software"
                            } else if decoder_config.backend == DecoderBackend::AvCodec {
                                #[cfg(target_env = "ohos")]
                                trace::diagnostic(
                                    serde_json::json!({
                                        "event": "ohos_avcodec_fallback",
                                        "stage": "open_avcodec_to_software",
                                        "fromMode": "buffer_nv12_direct_frame_copy",
                                        "toMode": "software_decode",
                                        "fallbackCount": video_decoder_fallbacks,
                                        "reason": surface_error.as_str(),
                                    })
                                    .to_string(),
                                );
                                "open_avcodec_to_software"
                            } else {
                                "open"
                            };
                            let event = VideoDecoderEvent {
                                stage: stage.to_string(),
                                requested_backend: decoder_config.backend,
                                previous_backend: Some(decoder_config.backend),
                                active_backend: decoder.backend(),
                                fallback_count: video_decoder_fallbacks,
                                codec: codec.clone(),
                                pixel_format: None,
                                line_sizes: None,
                                reason: Some(surface_error),
                            };
                            trace::diagnostic(event.structured_message());
                            video_decoder_events.push_back(event);
                            decoder
                        }
                    }
                    Err(error) => {
                        return Err(video_decoder_open_unavailable_error(
                            video_decoder_open_stage(decoder_config),
                            decoder_config.backend,
                            stream_index,
                            codec.as_deref(),
                            video_decoder_fallbacks,
                            format!(
                                "{} decoder open failed: {error}",
                                decoder_config.backend.as_str(),
                            ),
                        ));
                    }
                },
            );
        }
        let (selected_audio_track, audio_decoder) = match first_decodable_audio_track(
            audio_track_candidates.iter().copied(),
            |stream_index| {
                let parameters = codec_parameters_for(&codec_parameters, stream_index)?;
                Decoder::open_owned(parameters).map_err(PlaybackError::from)
            },
        ) {
            Ok(Some((stream_index, decoder, failures))) => {
                for (failed_stream_index, error) in failures {
                    let codec = codec_parameters_for(&codec_parameters, failed_stream_index)
                        .ok()
                        .and_then(OwnedCodecParameters::codec_name)
                        .unwrap_or_else(|| "unknown".to_string());
                    trace::diagnostic(
                        serde_json::json!({
                            "event": "audio_decoder_fallback",
                            "stage": "open_failed",
                            "trackId": failed_stream_index,
                            "codec": codec,
                            "error": error.to_string(),
                            "selectedTrackId": stream_index,
                        })
                        .to_string(),
                    );
                }
                (Some(stream_index), Some(decoder))
            }
            Ok(None) => (None, None),
            Err(failures) => {
                let reason = failures
                    .iter()
                    .map(|(stream_index, error)| {
                        let codec = codec_parameters_for(&codec_parameters, *stream_index)
                            .ok()
                            .and_then(OwnedCodecParameters::codec_name)
                            .unwrap_or_else(|| "unknown".to_string());
                        format!("track {stream_index} ({codec}): {error}")
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                trace::diagnostic(
                    serde_json::json!({
                        "event": "audio_decoder_fallback",
                        "stage": "all_tracks_failed",
                        "trackCount": failures.len(),
                        "reason": reason.as_str(),
                    })
                    .to_string(),
                );
                return Err(PlaybackError::AudioDecoderUnavailable { reason });
            }
        };
        if let Some(stream_index) = selected_audio_track {
            selected_streams.push(stream_index);
        }
        let mut subtitle_decoder = None;
        let mut opened_subtitle_track = None;
        if let Some(stream_index) = selected_subtitle_track {
            match codec_parameters_for(&codec_parameters, stream_index).and_then(|parameters| {
                SubtitleDecoder::open_owned_with_fonts(parameters, subtitle_fonts.clone())
                    .map_err(Into::into)
            }) {
                Ok(decoder) => {
                    selected_streams.push(stream_index);
                    opened_subtitle_track = Some(stream_index);
                    subtitle_decoder = Some(decoder);
                }
                Err(error) => {
                    eprintln!("Erika playback subtitle decoder open failed: {error}");
                }
            }
        }
        // Keep every embedded subtitle stream in the asynchronous demux
        // selection. Subtitle track changes can then swap only the subtitle
        // decoder without resetting the demux worker. Resetting its selection
        // drains the bounded read-ahead queue while the underlying demux cursor
        // stays ahead; that creates an A/V packet discontinuity and breaks the
        // HEVC reference-picture chain until the next random-access frame.
        for track in &probe.subtitles {
            if let SubtitleTrackSource::Embedded { stream_index } = &track.source {
                selected_streams.push(stream_index_i32(
                    *stream_index,
                    TrackKind::Subtitle,
                    track.id,
                )?);
            }
        }
        selected_streams.sort_unstable();
        selected_streams.dedup();
        if !selected_streams.is_empty() {
            demuxer.set_stream_selection(StreamSelection::only(selected_streams))?;
        }

        let video_params = selected_video_track.and_then(|stream_index| {
            probe
                .video
                .iter()
                .find(|video| video.track_id == stream_index as i64)
                .map(|video| video.params.clone())
        });
        mark_selected_tracks(
            &mut probe.tracks,
            selected_video_track.map(i64::from),
            selected_audio_track.map(i64::from),
            opened_subtitle_track.map(i64::from),
        );
        let info = OpenedMediaInfo {
            uri: probe.uri,
            duration: probe.duration,
            tracks: probe.tracks,
            video_params,
            selected_video_track: selected_video_track.map(i64::from),
            selected_audio_track: selected_audio_track.map(i64::from),
            selected_subtitle_track: opened_subtitle_track.map(i64::from),
            subtitle_tracks: probe.subtitles,
            video_decode_backend: video_decoder.as_ref().map(Decoder::backend),
            audio_output: audio_decoder.as_ref().map(|_| config.audio_output),
        };

        let mediacodec_surface_disabled = video_decoder.as_ref().is_some_and(|decoder| {
            decoder.backend() == DecoderBackend::MediaCodec && !decoder.uses_mediacodec_surface()
        });
        Ok(Self {
            demuxer: AsyncDemuxer::spawn(demuxer),
            codec_parameters,
            video_decoder,
            #[cfg(target_env = "ohos")]
            decoder_resources,
            video_decoder_unavailable_reason: None,
            audio_decoder,
            subtitle_decoder,
            subtitle_fonts,
            external_subtitles: Vec::new(),
            audio_resampler: None,
            audio_output: config.audio_output,
            audio_output_active: true,
            video_decode_suspended: false,
            info,
            video_frames: VecDeque::new(),
            audio_frames: VecDeque::new(),
            subtitle_frames: VecDeque::new(),
            pending_video_packets: VecDeque::new(),
            video_fallback_waiting_for_keyframe: false,
            audio_seek_waiting_for_valid_packet: false,
            audio_seek_dropped_packets: 0,
            mediacodec_surface_disabled,
            video_decoder_fallbacks,
            dolby_vision_profile,
            dovi_rpu_diagnostics: DoviRpuDiagnostics::default(),
            video_decoder_events,
            queue_limits,
            buffer_recovery_audio: buffer_recovery_audio_for_request(request),
            buffering_audio_video_scan: BufferingAudioVideoScan::default(),
            buffering_audio_recovery_suspended: false,
            demux_eof: false,
            video_packet_stall_polls: 0,
            video_packet_stall_logged: false,
            video_packet_stall_started_at: None,
            eof_drain_polls: 0,
            eof_drain_pending_logged: false,
            eof_drain_last_progress_at: None,
            eof: false,
        })
    }

    pub fn info(&self) -> &OpenedMediaInfo {
        &self.info
    }

    fn ensure_video_decoder_available(&self, stage: &str) -> Result<()> {
        let Some(reason) = video_decoder_unavailability_reason(
            self.info.selected_video_track,
            self.video_decoder.is_some(),
            self.video_decoder_unavailable_reason.as_deref(),
        ) else {
            return Ok(());
        };
        trace::diagnostic(
            serde_json::json!({
                "event": "video_decoder_unavailable",
                "stage": stage,
                "selectedVideoTrack": self.info.selected_video_track,
                "fallbackCount": self.video_decoder_fallbacks,
                "reason": reason.as_str(),
            })
            .to_string(),
        );
        Err(PlaybackError::VideoDecoderUnavailable { reason })
    }

    fn mark_video_decoder_unavailable(&mut self, reason: String) {
        self.video_decoder_unavailable_reason = Some(reason);
        self.info.video_decode_backend = None;
    }

    fn clear_video_decoder_unavailable(&mut self) {
        self.video_decoder_unavailable_reason = None;
    }

    fn discard_queued_frames_and_packets(&mut self) -> DiscardedPlaybackQueues {
        let discarded = DiscardedPlaybackQueues {
            video_frames: self.video_frames.len(),
            audio_frames: self.audio_frames.len(),
            subtitle_frames: self.subtitle_frames.len(),
            video_packets: self.pending_video_packets.len(),
        };
        self.video_frames.clear();
        self.audio_frames.clear();
        self.subtitle_frames.clear();
        self.pending_video_packets.clear();
        self.reset_buffering_audio_video_scan();
        discarded
    }

    fn discard_video_frames_and_packets(&mut self) -> DiscardedPlaybackQueues {
        let discarded = DiscardedPlaybackQueues {
            video_frames: self.video_frames.len(),
            video_packets: self.pending_video_packets.len(),
            ..DiscardedPlaybackQueues::default()
        };
        self.video_frames.clear();
        self.pending_video_packets.clear();
        self.reset_buffering_audio_video_scan();
        discarded
    }

    fn set_video_decode_suspended(&mut self, suspended: bool) {
        if self.video_decode_suspended == suspended {
            return;
        }
        self.video_decode_suspended = suspended;
        let discarded = self.discard_video_frames_and_packets();
        trace_discarded_playback_queues(
            if suspended {
                "video_decode_suspend"
            } else {
                "video_decode_resume"
            },
            discarded,
            self.video_decoder.is_some(),
        );
        self.reset_video_packet_stall_state();
        if !suspended {
            if let Some(decoder) = &mut self.video_decoder {
                decoder.flush();
            }
            self.video_fallback_waiting_for_keyframe = self.video_decoder.is_some();
            trace::diagnostic(
                serde_json::json!({
                    "event": "video_decoder_recovery_waiting_for_keyframe",
                    "stage": "foreground_resume",
                    "backend": self.active_video_decoder_backend().map(DecoderBackend::as_str),
                })
                .to_string(),
            );
        }
    }

    fn selected_video_codec(&self) -> Option<String> {
        let selected = self.info.selected_video_track?;
        self.info
            .tracks
            .iter()
            .find(|track| track.id == selected)
            .and_then(|track| track.codec.clone())
    }

    fn take_video_decoder_events(&mut self) -> Vec<VideoDecoderEvent> {
        self.video_decoder_events.drain(..).collect()
    }

    pub fn track_selection(&self) -> TrackSelection {
        self.info.track_selection()
    }

    pub fn is_eof(&self) -> bool {
        self.eof
    }

    fn audio_decoder_is_eof(&self) -> bool {
        self.demux_eof
            && self
                .audio_decoder
                .as_ref()
                .is_none_or(Decoder::is_end_of_stream)
    }

    fn has_queued_video_frames(&self) -> bool {
        !self.video_frames.is_empty()
    }

    fn has_queued_audio_frames(&self) -> bool {
        !self.audio_frames.is_empty()
    }

    fn queued_audio_frame_count(&self) -> usize {
        self.audio_frames.len()
    }

    fn discard_queued_audio_frames(&mut self) -> usize {
        let discarded = self.audio_frames.len();
        self.audio_frames.clear();
        discarded
    }

    fn decoder_fallback_requires_replay(&self) -> bool {
        self.demux_eof
            || self.eof
            || matches!(
                self.active_video_decoder_backend(),
                Some(
                    DecoderBackend::VideoToolbox
                        | DecoderBackend::D3d11va
                        | DecoderBackend::Cuda
                        | DecoderBackend::Vaapi
                        | DecoderBackend::AvCodec
                )
            )
    }

    fn recover_from_video_input_stall(&mut self, reason: &str) -> Result<bool> {
        if self.active_video_decoder_backend() == Some(DecoderBackend::AvCodec) {
            self.fallback_video_decoder_to_software(
                "decoder_input_stall_avcodec_to_software",
                reason.to_string(),
                None,
            )?;
            return Ok(true);
        }
        let Some(target) = self.mediacodec_fallback_target() else {
            return Ok(false);
        };
        match target {
            MediaCodecFallbackTarget::ByteBuffer => self
                .fallback_mediacodec_surface_to_byte_buffer(
                    "decoder_input_stall",
                    reason.to_string(),
                    None,
                )?,
            MediaCodecFallbackTarget::Software => self.fallback_video_decoder_to_software(
                "decoder_input_stall_bytebuffer_to_software",
                reason.to_string(),
                None,
            )?,
        }
        Ok(true)
    }

    fn set_audio_output_active(&mut self, active: bool) -> usize {
        if self.audio_output_active == active {
            return 0;
        }
        self.audio_output_active = active;
        if active {
            0
        } else {
            self.discard_queued_audio_frames()
        }
    }

    fn rebase_decoder_progress_watchdogs(&mut self) {
        let now = Instant::now();
        if self.video_packet_stall_started_at.is_some() {
            self.video_packet_stall_started_at = Some(now);
        }
        if self.eof_drain_last_progress_at.is_some() {
            self.eof_drain_last_progress_at = Some(now);
        }
    }

    fn next_video_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
        self.ensure_video_decoder_available("next_video_frame")?;
        if self.video_decoder.is_none() {
            return Ok(None);
        }
        let started = Instant::now();
        let mut pumped_packets = 0usize;
        while self.video_frames.is_empty()
            && !self.eof
            && pumped_packets < VIDEO_PUMP_PACKET_BUDGET
            && started.elapsed() < VIDEO_PUMP_TIME_BUDGET
        {
            if !self.pump_once(PlaybackPumpDemand::Video)? {
                break;
            }
            pumped_packets = pumped_packets.saturating_add(1);
        }
        if trace::enabled()
            && self.video_frames.is_empty()
            && !self.eof
            && (pumped_packets >= VIDEO_PUMP_PACKET_BUDGET
                || started.elapsed() >= VIDEO_PUMP_TIME_BUDGET)
        {
            trace::log(format!(
                "[erika-playback-trace] stage=session_video_pump_budget packets={} packet_budget={} queued_audio={} pending_video={} elapsed_ms={:.3}",
                pumped_packets,
                VIDEO_PUMP_PACKET_BUDGET,
                self.audio_frames.len(),
                self.pending_video_packets.len(),
                started.elapsed().as_secs_f64() * 1000.0,
            ));
        }
        Ok(self.video_frames.pop_front())
    }

    pub fn next_audio_frame(&mut self) -> Result<Option<PcmAudioFrame>> {
        if self.audio_decoder.is_none() {
            return Ok(None);
        }
        while self.audio_frames.is_empty() && !self.eof {
            if !self.pump_once(PlaybackPumpDemand::Audio)? {
                break;
            }
        }
        Ok(self.audio_frames.pop_front())
    }

    pub fn next_audio_frame_bounded(
        &mut self,
        max_packets: usize,
        max_duration: Duration,
    ) -> Result<Option<PcmAudioFrame>> {
        self.next_audio_frame_bounded_where(
            max_packets,
            max_duration,
            PlaybackPumpDemand::Audio,
            |_| true,
        )
    }

    fn next_audio_frame_bounded_where(
        &mut self,
        max_packets: usize,
        max_duration: Duration,
        demand: PlaybackPumpDemand,
        mut keep_frame: impl FnMut(&mut PcmAudioFrame) -> bool,
    ) -> Result<Option<PcmAudioFrame>> {
        if self.audio_decoder.is_none() {
            return Ok(None);
        }

        let started = Instant::now();
        let mut pumped_packets = 0usize;
        let mut filtered_frames = 0usize;
        let mut inspected_frames = 0usize;
        let mut frame = pop_matching_audio_frame(
            &mut self.audio_frames,
            &mut keep_frame,
            &mut filtered_frames,
            &mut inspected_frames,
            started,
            max_duration,
        );
        while frame.is_none()
            && !self.eof
            && pumped_packets < max_packets
            && !(demand == PlaybackPumpDemand::BufferingAudio
                && self.buffering_audio_recovery_suspended)
        {
            if started.elapsed() >= max_duration {
                break;
            }
            if self.pump_once(demand)? {
                pumped_packets = pumped_packets.saturating_add(1);
                frame = pop_matching_audio_frame(
                    &mut self.audio_frames,
                    &mut keep_frame,
                    &mut filtered_frames,
                    &mut inspected_frames,
                    started,
                    max_duration,
                );
            } else {
                break;
            }
        }

        if trace::enabled()
            && (pumped_packets > 0
                || filtered_frames > 0
                || frame.is_none()
                || started.elapsed() >= max_duration)
        {
            trace::log(format!(
                "[erika-playback-trace] stage=session_audio_pump packets={} max_packets={} produced={} filtered={} queued_audio={} ended={} pending_video={} elapsed_ms={:.3}",
                pumped_packets,
                max_packets,
                frame.is_some(),
                filtered_frames,
                self.audio_frames.len(),
                self.eof,
                self.pending_video_packets.len(),
                started.elapsed().as_secs_f64() * 1000.0,
            ));
        }
        if frame.is_some() && demand == PlaybackPumpDemand::BufferingAudio {
            // Packet/byte/time limits watch for progress since the last audio
            // frame. Keep the video-advance budget cumulative for the entire
            // buffering episode. Do not re-enable audio starvation yet: a
            // sparse track may have produced a frame whose PTS is still far
            // ahead of the paused clock. The engine clears suspension only
            // once that frame enters the audio lead window.
            self.reset_buffering_audio_video_scan_progress();
        }
        Ok(frame)
    }

    pub fn next_subtitle_frame(
        &mut self,
        media_time: Duration,
    ) -> Result<Option<DecodedSubtitleFrame>> {
        let Some(selected_track) = self.info.selected_subtitle_track else {
            return Ok(None);
        };
        if self.subtitle_decoder.is_none()
            && self.selected_external_subtitle(selected_track).is_none()
        {
            return Ok(None);
        }
        self.pump_external_subtitles(selected_track, media_time)?;
        Ok(self.pop_ready_subtitle(media_time))
    }

    pub fn add_external_subtitle(
        &mut self,
        config: SubtitleTrackConfig,
        media_time: Duration,
    ) -> Result<(SubtitleTrackConfig, Option<DecodedSubtitleFrame>)> {
        let previous_subtitle = self.info.selected_subtitle_track;
        let mut external = ExternalSubtitleSession::open(config)?;
        external.seek(media_time)?;
        let track = external.track().clone();
        let mut info = TrackInfo::external(track.id, TrackKind::Subtitle);
        info.title = track.title.clone();
        info.language = track.language.clone();
        info.selected = true;
        self.info.tracks.push(info);
        self.info.subtitle_tracks.push(track.clone());
        self.external_subtitles.push(external);
        self.select_subtitle_track_internal(Some(track.id))?;
        Ok((track, clear_subtitle_frame(previous_subtitle, media_time)))
    }

    pub fn remove_subtitle_track(
        &mut self,
        track_id: i64,
        media_time: Duration,
    ) -> Result<Option<DecodedSubtitleFrame>> {
        let was_selected = self.info.selected_subtitle_track == Some(track_id);
        let Some(index) = self
            .external_subtitles
            .iter()
            .position(|track| track.track().id == track_id)
        else {
            let is_embedded = self
                .info
                .subtitle_tracks
                .iter()
                .any(|track| track.id == track_id && !track.can_remove());
            return if is_embedded {
                Err(PlaybackError::SubtitleTrackNotRemovable(track_id))
            } else {
                Ok(None)
            };
        };
        self.external_subtitles.remove(index);
        self.subtitle_frames
            .retain(|frame| frame.track_id != track_id);
        self.info.tracks.retain(|track| track.id != track_id);
        self.info
            .subtitle_tracks
            .retain(|track| track.id != track_id);
        if was_selected {
            self.info.selected_subtitle_track = None;
            mark_selected_tracks(
                &mut self.info.tracks,
                self.info.selected_video_track,
                self.info.selected_audio_track,
                self.info.selected_subtitle_track,
            );
        }
        Ok(clear_subtitle_frame(Some(track_id), media_time))
    }

    pub fn select_audio_track(&mut self, track_id: Option<i64>) -> Result<()> {
        match track_id {
            Some(id) => {
                let stream_index = self.embedded_track_stream_index(id, TrackKind::Audio)?;
                let parameters = self.codec_parameters(stream_index)?;
                let decoder = Decoder::open_owned(parameters)?;
                self.audio_decoder = Some(decoder);
                self.info.selected_audio_track = Some(id);
                self.info.audio_output = Some(self.audio_output);
            }
            None => {
                self.audio_decoder = None;
                self.info.selected_audio_track = None;
                self.info.audio_output = None;
            }
        }
        self.audio_resampler = None;
        self.audio_frames.clear();
        self.update_demux_selection()?;
        self.mark_selected_tracks();
        Ok(())
    }

    pub fn select_subtitle_track(
        &mut self,
        track_id: Option<i64>,
        media_time: Duration,
    ) -> Result<Option<DecodedSubtitleFrame>> {
        let previous = self.info.selected_subtitle_track;
        self.select_subtitle_track_internal(track_id)?;
        Ok(clear_subtitle_frame(previous, media_time))
    }

    fn select_subtitle_track_internal(&mut self, track_id: Option<i64>) -> Result<()> {
        match track_id {
            Some(id) => match self.subtitle_track_source(id)? {
                SubtitleTrackSource::Embedded { stream_index } => {
                    let stream_index = stream_index_i32(stream_index, TrackKind::Subtitle, id)?;
                    let decoder = SubtitleDecoder::open_owned_with_fonts(
                        self.codec_parameters(stream_index)?,
                        self.subtitle_fonts.clone(),
                    )?;
                    self.subtitle_decoder = Some(decoder);
                    self.info.selected_subtitle_track = Some(id);
                }
                SubtitleTrackSource::External { .. } => {
                    self.subtitle_decoder = None;
                    self.info.selected_subtitle_track = Some(id);
                }
            },
            None => {
                self.subtitle_decoder = None;
                self.info.selected_subtitle_track = None;
            }
        }
        self.subtitle_frames.clear();
        // All embedded subtitle streams are selected when the main demuxer is
        // opened (and whenever an audio selection rebuilds its selection).
        // Do not touch the asynchronous demux selection for a subtitle-only
        // change: doing so would discard queued A/V packets without rewinding
        // the demux cursor.
        self.mark_selected_tracks();
        Ok(())
    }

    pub fn seek(&mut self, position: Duration) -> Result<()> {
        self.seek_with_decoder_flush(position, true, true)
    }

    fn seek_for_stop(&mut self, position: Duration) -> Result<()> {
        self.seek_with_decoder_flush_inner(position, true, true, false)?;
        self.video_frames.shrink_to_fit();
        self.audio_frames.shrink_to_fit();
        self.subtitle_frames.shrink_to_fit();
        self.pending_video_packets.shrink_to_fit();
        for external in &mut self.external_subtitles {
            external.frames.shrink_to_fit();
            external.demuxer.release_source_buffer();
        }
        self.demuxer.release_buffers()
    }

    fn seek_with_decoder_flush(
        &mut self,
        position: Duration,
        flush_audio_decoder: bool,
        flush_subtitle_decoder: bool,
    ) -> Result<()> {
        self.seek_with_decoder_flush_inner(
            position,
            flush_audio_decoder,
            flush_subtitle_decoder,
            true,
        )
    }

    fn seek_with_decoder_flush_inner(
        &mut self,
        position: Duration,
        flush_audio_decoder: bool,
        flush_subtitle_decoder: bool,
        require_video_decoder: bool,
    ) -> Result<()> {
        if require_video_decoder {
            self.ensure_video_decoder_available("seek")?;
        }
        self.demuxer.seek(position)?;
        // Release every queued frame before replacing a MediaCodec decoder.
        // Surface frames retain AImage/AHardwareBuffer state tied to the old
        // codec and `delay_flush=1`; keeping one alive across the replacement
        // can stall the new route or exhaust its ImageReader immediately.
        let decoder_alive = self.video_decoder.is_some();
        let discarded = self.discard_queued_frames_and_packets();
        trace_discarded_playback_queues("seek_before_decoder_transition", discarded, decoder_alive);
        let bypass_seek_keyframe_gate = cfg!(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos"
        )) && self.active_video_decoder_backend()
            == Some(DecoderBackend::VideoToolbox)
            && self.active_video_codec_is_av1();
        self.video_fallback_waiting_for_keyframe =
            self.video_decoder.is_some() && !bypass_seek_keyframe_gate;
        if self.video_fallback_waiting_for_keyframe {
            trace::diagnostic(
                serde_json::json!({
                    "event": "video_seek_preroll",
                    "stage": "waiting_for_keyframe",
                    "targetSeconds": position.as_secs_f64(),
                    "videoBackend": self.active_video_decoder_backend().map(DecoderBackend::as_str),
                })
                .to_string(),
            );
        }
        self.audio_seek_waiting_for_valid_packet = self.audio_decoder.is_some();
        self.audio_seek_dropped_packets = 0;

        #[cfg(target_os = "android")]
        let video_decoder_reopened = self.reopen_mediacodec_video_decoder_for_seek(position)?;
        #[cfg(target_env = "ohos")]
        let video_decoder_reopened = self.reopen_avcodec_video_decoder_for_seek(position)?;
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        let video_decoder_reopened = false;

        if !video_decoder_reopened {
            // Non-MediaCodec backends retain their active route across seeks.
            // Android MediaCodec is reopened above because CCodec flush is not
            // reliable for Surface output (notably AV1 with delay_flush=1).
            if let Some(decoder) = &mut self.video_decoder {
                decoder.flush();
            }
        }
        if flush_audio_decoder {
            if let Some(decoder) = &mut self.audio_decoder {
                decoder.flush();
            }
        }
        if flush_subtitle_decoder {
            if let Some(decoder) = &mut self.subtitle_decoder {
                decoder.flush();
            }
        }
        for external in &mut self.external_subtitles {
            external.seek(position)?;
        }
        self.audio_resampler = None;
        self.reset_eof_drain_state();
        Ok(())
    }

    #[cfg(target_os = "android")]
    fn reopen_mediacodec_video_decoder_for_seek(&mut self, position: Duration) -> Result<bool> {
        let Some(route) = mediacodec_seek_route(
            self.active_video_decoder_backend(),
            self.video_decoder
                .as_ref()
                .is_some_and(Decoder::uses_mediacodec_surface),
        ) else {
            return Ok(false);
        };
        let stream_index = self
            .video_decoder
            .as_ref()
            .expect("MediaCodec decoder exists")
            .stream_index();
        let codec = codec_parameters_for(&self.codec_parameters, stream_index)?.codec_name();
        self.mark_video_decoder_unavailable(format!(
            "MediaCodec {} decoder is being reopened for seek to {:.3}s",
            route.as_str(),
            position.as_secs_f64(),
        ));
        let previous_decoder = self
            .video_decoder
            .take()
            .expect("MediaCodec decoder exists");
        // Destroy CCodec, its Surface, and the old AImageReader before opening
        // the replacement. Keeping the previous decoder alive during open can
        // itself exhaust scarce hardware codec instances and force a false
        // fallback.
        drop(previous_decoder);
        let parameters = codec_parameters_for(&self.codec_parameters, stream_index)?;
        trace::diagnostic(
            serde_json::json!({
                "event": "android_mediacodec_seek_reopen",
                "stage": "begin",
                "mode": route.as_str(),
                "codec": codec.as_deref(),
                "targetSeconds": position.as_secs_f64(),
            })
            .to_string(),
        );

        match Decoder::open_owned_with_config(parameters, route.decoder_config()) {
            Ok(decoder) => {
                self.video_decoder = Some(decoder);
                self.info.video_decode_backend = Some(DecoderBackend::MediaCodec);
                self.clear_video_decoder_unavailable();
                self.mediacodec_surface_disabled = route == MediaCodecSeekRoute::ByteBuffer;
                let event = VideoDecoderEvent {
                    stage: match route {
                        MediaCodecSeekRoute::Surface => "seek_reopen_surface",
                        MediaCodecSeekRoute::ByteBuffer => "seek_reopen_bytebuffer",
                    }
                    .to_string(),
                    requested_backend: DecoderBackend::MediaCodec,
                    previous_backend: Some(DecoderBackend::MediaCodec),
                    active_backend: DecoderBackend::MediaCodec,
                    fallback_count: self.video_decoder_fallbacks,
                    codec: codec.clone(),
                    pixel_format: None,
                    line_sizes: None,
                    reason: None,
                };
                trace::diagnostic(event.structured_message());
                self.video_decoder_events.push_back(event);
                trace::diagnostic(
                    serde_json::json!({
                        "event": "android_mediacodec_seek_reopen",
                        "stage": "ready",
                        "mode": route.as_str(),
                        "codec": codec.as_deref(),
                        "targetSeconds": position.as_secs_f64(),
                    })
                    .to_string(),
                );
            }
            Err(reopen_error) => {
                let reopen_reason = reopen_error.to_string();
                trace::diagnostic(
                    serde_json::json!({
                        "event": "android_mediacodec_seek_reopen",
                        "stage": "route_open_failed",
                        "mode": route.as_str(),
                        "codec": codec.as_deref(),
                        "targetSeconds": position.as_secs_f64(),
                        "reason": reopen_reason.as_str(),
                    })
                    .to_string(),
                );
                let recovery = match route {
                    MediaCodecSeekRoute::Surface => self
                        .fallback_mediacodec_surface_to_byte_buffer(
                            "seek_reopen",
                            reopen_reason.clone(),
                            None,
                        ),
                    MediaCodecSeekRoute::ByteBuffer => self.fallback_video_decoder_to_software(
                        "seek_reopen_bytebuffer_to_software",
                        reopen_reason.clone(),
                        None,
                    ),
                };
                if let Err(recovery_error) = recovery {
                    trace::diagnostic(
                        serde_json::json!({
                            "event": "android_mediacodec_seek_reopen",
                            "stage": "fallback_failed",
                            "mode": route.as_str(),
                            "codec": codec.as_deref(),
                            "targetSeconds": position.as_secs_f64(),
                            "routeReason": reopen_reason.as_str(),
                            "reason": recovery_error.to_string(),
                        })
                        .to_string(),
                    );
                    return Err(recovery_error);
                }
                trace::diagnostic(
                    serde_json::json!({
                        "event": "android_mediacodec_seek_reopen",
                        "stage": "fallback_ready",
                        "requestedMode": route.as_str(),
                        "activeBackend": self.active_video_decoder_backend().map(DecoderBackend::as_str),
                        "activeMode": self.video_decoder.as_ref().and_then(|decoder| {
                            (decoder.backend() == DecoderBackend::MediaCodec).then_some(
                                if decoder.uses_mediacodec_surface() {
                                    "surface_ahardwarebuffer"
                                } else {
                                    "bytebuffer_cpu_upload"
                                },
                            )
                        }),
                        "codec": codec.as_deref(),
                        "targetSeconds": position.as_secs_f64(),
                    })
                    .to_string(),
                );
            }
        }
        Ok(true)
    }

    #[cfg(target_env = "ohos")]
    fn reopen_avcodec_video_decoder_for_seek(&mut self, position: Duration) -> Result<bool> {
        let Some(previous_decoder) = self.video_decoder.as_ref() else {
            return Ok(false);
        };
        if previous_decoder.backend() != DecoderBackend::AvCodec
            || !previous_decoder.uses_avcodec_surface()
        {
            return Ok(false);
        }
        let stream_index = previous_decoder.stream_index();
        let codec = codec_parameters_for(&self.codec_parameters, stream_index)?.codec_name();
        self.mark_video_decoder_unavailable(format!(
            "AVCodec Surface decoder is being reopened for seek to {:.3}s",
            position.as_secs_f64(),
        ));
        let previous_decoder = self
            .video_decoder
            .take()
            .expect("AVCodec Surface decoder exists");
        drop(previous_decoder);
        let parameters = codec_parameters_for(&self.codec_parameters, stream_index)?;
        trace::diagnostic(
            serde_json::json!({
                "event": "ohos_avcodec_seek_reopen",
                "stage": "begin",
                "mode": "surface_native_buffer",
                "codec": codec.as_deref(),
                "targetSeconds": position.as_secs_f64(),
            })
            .to_string(),
        );
        match open_video_decoder(
            parameters,
            DecoderConfig::avcodec(),
            &self.decoder_resources,
        ) {
            Ok(decoder) => {
                self.video_decoder = Some(decoder);
                self.info.video_decode_backend = Some(DecoderBackend::AvCodec);
                self.clear_video_decoder_unavailable();
                let event = VideoDecoderEvent {
                    stage: "seek_reopen_avcodec_surface".to_string(),
                    requested_backend: DecoderBackend::AvCodec,
                    previous_backend: Some(DecoderBackend::AvCodec),
                    active_backend: DecoderBackend::AvCodec,
                    fallback_count: self.video_decoder_fallbacks,
                    codec: codec.clone(),
                    pixel_format: None,
                    line_sizes: None,
                    reason: None,
                };
                trace::diagnostic(event.structured_message());
                self.video_decoder_events.push_back(event);
                trace::diagnostic(
                    serde_json::json!({
                        "event": "ohos_avcodec_seek_reopen",
                        "stage": "ready",
                        "mode": "surface_native_buffer",
                        "codec": codec.as_deref(),
                        "targetSeconds": position.as_secs_f64(),
                    })
                    .to_string(),
                );
            }
            Err(error) => {
                let reason = error.to_string();
                self.fallback_video_decoder_to_software(
                    "seek_reopen_avcodec_to_software",
                    reason,
                    None,
                )?;
            }
        }
        Ok(true)
    }

    fn pump_external_subtitles(&mut self, track_id: i64, media_time: Duration) -> Result<()> {
        if let Some(external) = self
            .external_subtitles
            .iter_mut()
            .find(|track| track.track().id == track_id)
        {
            external.pump_until(media_time)?;
        }
        Ok(())
    }

    fn pop_ready_subtitle(&mut self, media_time: Duration) -> Option<DecodedSubtitleFrame> {
        let embedded_start = self.subtitle_frames.front().map(decoded_subtitle_start);
        let external_starts = self
            .external_subtitles
            .iter()
            .enumerate()
            .filter(|(_, external)| Some(external.track().id) == self.info.selected_subtitle_track)
            .filter_map(|(index, external)| external.peek_start().map(|start| (index, start)));
        let candidate = select_ready_subtitle(embedded_start, external_starts, media_time)?;

        match candidate {
            SubtitleQueueCandidate::Embedded { .. } => self.subtitle_frames.pop_front(),
            SubtitleQueueCandidate::External { index, .. } => {
                self.external_subtitles[index].pop_front()
            }
        }
    }

    fn selected_external_subtitle(&self, track_id: i64) -> Option<&ExternalSubtitleSession> {
        self.external_subtitles
            .iter()
            .find(|track| track.track().id == track_id)
    }

    fn embedded_track_stream_index(&self, track_id: i64, kind: TrackKind) -> Result<i32> {
        let Some(track) = self
            .info
            .tracks
            .iter()
            .find(|track| track.id == track_id && track.kind == kind)
        else {
            return Err(PlaybackError::TrackNotFound { kind, track_id });
        };
        stream_index_i32(track.id, kind, track_id)
    }

    fn codec_parameters(&self, stream_index: i32) -> Result<&OwnedCodecParameters> {
        codec_parameters_for(&self.codec_parameters, stream_index)
    }

    fn subtitle_track_source(&self, track_id: i64) -> Result<SubtitleTrackSource> {
        self.info
            .subtitle_tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.source.clone())
            .ok_or(PlaybackError::TrackNotFound {
                kind: TrackKind::Subtitle,
                track_id,
            })
    }

    fn update_demux_selection(&mut self) -> Result<()> {
        let mut streams = Vec::new();
        if let Some(track_id) = self.info.selected_video_track {
            streams.push(self.embedded_track_stream_index(track_id, TrackKind::Video)?);
        }
        if let Some(track_id) = self.info.selected_audio_track {
            streams.push(self.embedded_track_stream_index(track_id, TrackKind::Audio)?);
        }
        for track in &self.info.subtitle_tracks {
            if let SubtitleTrackSource::Embedded { stream_index } = &track.source {
                streams.push(stream_index_i32(
                    *stream_index,
                    TrackKind::Subtitle,
                    track.id,
                )?);
            }
        }
        streams.sort_unstable();
        streams.dedup();
        if streams.is_empty() {
            self.demuxer.set_stream_selection(StreamSelection::all())?;
        } else {
            self.demuxer
                .set_stream_selection(StreamSelection::only(streams))?;
        }
        self.reset_eof_drain_state();
        Ok(())
    }

    fn mark_selected_tracks(&mut self) {
        mark_selected_tracks(
            &mut self.info.tracks,
            self.info.selected_video_track,
            self.info.selected_audio_track,
            self.info.selected_subtitle_track,
        );
    }

    fn reset_buffering_audio_video_scan(&mut self) {
        self.buffering_audio_video_scan = BufferingAudioVideoScan::default();
        self.buffering_audio_recovery_suspended = false;
    }

    fn begin_buffering_audio_video_scan(&mut self, media_time: Duration) {
        self.buffering_audio_video_scan = BufferingAudioVideoScan {
            baseline_media_time: Some(media_time),
            ..BufferingAudioVideoScan::default()
        };
    }

    fn finish_buffering_audio_video_scan(&mut self) {
        // Keep `buffering_audio_recovery_suspended` until a real audio frame,
        // seek, or stream reset. This prevents a legitimate video-only tail
        // from entering the same futile buffering scan over and over.
        self.buffering_audio_video_scan = BufferingAudioVideoScan::default();
    }

    fn reset_buffering_audio_video_scan_progress(&mut self) {
        let scan = &mut self.buffering_audio_video_scan;
        scan.active_time = Duration::ZERO;
        scan.packets = 0;
        scan.bytes = 0;
    }

    fn buffering_video_advance_allowed(&self, pts: Option<Duration>) -> bool {
        let scan = &self.buffering_audio_video_scan;
        if scan.discarded_frames >= BUFFERING_VIDEO_MAX_DRAINED_FRAMES {
            return false;
        }
        !scan
            .baseline_media_time
            .zip(pts)
            .is_some_and(|(baseline, pts)| {
                pts.saturating_sub(baseline) > BUFFERING_VIDEO_MAX_ADVANCE
            })
    }

    fn record_buffering_video_advance(&mut self) {
        self.buffering_audio_video_scan.discarded_frames = self
            .buffering_audio_video_scan
            .discarded_frames
            .saturating_add(1);
    }

    fn resume_buffering_audio_recovery(&mut self) {
        self.buffering_audio_recovery_suspended = false;
    }

    fn suspend_buffering_audio_recovery(&mut self, stage: &'static str, reason: String) {
        if self.buffering_audio_recovery_suspended {
            return;
        }
        self.buffering_audio_recovery_suspended = true;
        let scan = &self.buffering_audio_video_scan;
        trace::diagnostic(
            serde_json::json!({
                "event": "buffering_audio_recovery",
                "stage": stage,
                "videoPackets": scan.packets,
                "videoBytes": scan.bytes,
                "activeSeconds": scan.active_time.as_secs_f64(),
                "pendingVideoPackets": self.pending_video_packets.len(),
                "discardedVideoFrames": scan.discarded_frames,
                "reason": reason,
            })
            .to_string(),
        );
    }

    fn observe_buffering_audio_video_packet(
        &mut self,
        packet_bytes: usize,
        active_time: Duration,
    ) -> Result<()> {
        let scan = &mut self.buffering_audio_video_scan;
        scan.active_time = scan.active_time.saturating_add(active_time);
        scan.packets = scan.packets.saturating_add(1);
        scan.bytes = scan.bytes.saturating_add(packet_bytes);
        if scan.packets <= BUFFERING_AUDIO_VIDEO_SCAN_PACKET_LIMIT
            && scan.bytes <= BUFFERING_AUDIO_VIDEO_SCAN_BYTE_LIMIT
            && scan.active_time <= BUFFERING_AUDIO_VIDEO_SCAN_TIMEOUT
        {
            return Ok(());
        }

        let reason = format!(
            "audio recovery scanned {} video packets / {} bytes for {:.3}s without producing audio (pending_video_packets={}, discarded_video_frames={})",
            scan.packets,
            scan.bytes,
            scan.active_time.as_secs_f64(),
            self.pending_video_packets.len(),
            scan.discarded_frames,
        );
        // Reaching this watchdog means audio recovery should stop doing
        // speculative work, not that the media is invalid. The selected audio
        // track may legitimately end before a long video tail. Video pumping
        // continues and a later audio frame can re-enable audio recovery.
        self.suspend_buffering_audio_recovery("scan_limit", reason);
        Ok(())
    }

    fn release_video_frame_for_buffering_audio_scan(&mut self) -> bool {
        if self.pending_video_packets.is_empty()
            || self.video_frames.len() < self.active_video_frame_queue_limit()
        {
            return false;
        }
        let frame_pts = self
            .video_frames
            .front()
            .and_then(DecodedVideoFrame::pts)
            .and_then(|pts| pts.as_duration());
        if !self.buffering_video_advance_allowed(frame_pts) {
            let reason = format!(
                "audio recovery reached the shared video advance budget (frames={}, max_frames={}, baseline_seconds={:?}, next_pts_seconds={:?}, max_advance_seconds={:.3})",
                self.buffering_audio_video_scan.discarded_frames,
                BUFFERING_VIDEO_MAX_DRAINED_FRAMES,
                self.buffering_audio_video_scan
                    .baseline_media_time
                    .map(|value| value.as_secs_f64()),
                frame_pts.map(|value| value.as_secs_f64()),
                BUFFERING_VIDEO_MAX_ADVANCE.as_secs_f64(),
            );
            self.suspend_buffering_audio_recovery("video_advance_limit", reason);
            return false;
        }
        if self.video_frames.pop_front().is_none() {
            return false;
        }
        self.record_buffering_video_advance();
        let scan = &self.buffering_audio_video_scan;
        if scan.discarded_frames == 1 || scan.discarded_frames.is_power_of_two() {
            trace::diagnostic(
                serde_json::json!({
                    "event": "buffering_audio_recovery",
                    "stage": "release_video_backpressure",
                    "discardedVideoFrames": scan.discarded_frames,
                    "scannedVideoPackets": scan.packets,
                    "pendingVideoPackets": self.pending_video_packets.len(),
                    "queuedVideoFrames": self.video_frames.len(),
                })
                .to_string(),
            );
        }
        true
    }

    fn pump_once(&mut self, demand: PlaybackPumpDemand) -> Result<bool> {
        if demand == PlaybackPumpDemand::BufferingAudio {
            // Once both decoded and compressed video queues reach their caps,
            // release one already-decoded frame. This preserves compressed
            // packet order while guaranteeing that the next audio recovery
            // poll can continue toward a later interleaved audio packet.
            let _ = self.release_video_frame_for_buffering_audio_scan();
        }
        if !self.video_decode_suspended && self.route_pending_video_packets(demand)? {
            return Ok(true);
        }
        if !self.video_decode_suspended
            && !self.pending_video_packets.is_empty()
            && !audio_demand_may_scan_past_pending_video(demand, self.pending_video_packets.len())
        {
            return Ok(false);
        }
        if audio_queue_blocks_demux(
            demand,
            self.audio_output_active,
            self.audio_frames.len(),
            self.queue_limits.audio_frames,
        ) {
            return Ok(false);
        }
        if self.demux_eof {
            return self.finish_decoders(demand);
        }
        match self.demuxer.poll()? {
            PumpInput::Packet(packet) => {
                self.route_packet(packet, demand)?;
                Ok(true)
            }
            PumpInput::Eof => {
                self.demux_eof = true;
                let _ = self.finish_decoders(demand)?;
                Ok(true)
            }
            PumpInput::Empty => Ok(false),
        }
    }

    fn route_packet(&mut self, packet: ffmpeg::Packet, demand: PlaybackPumpDemand) -> Result<()> {
        if self
            .video_decoder
            .as_ref()
            .is_some_and(|decoder| packet.stream_index() == decoder.stream_index())
        {
            if self.video_decode_suspended {
                return Ok(());
            }
            let packet_bytes = packet.size();
            let scan_started = (demand == PlaybackPumpDemand::BufferingAudio).then(Instant::now);
            // Once a packet is pending, preserve decode order while an audio
            // pump scans across the interleaved stream. Normal audio demand
            // stops at the compressed queue cap; buffering recovery may free
            // a decoded frame to keep moving, but its packet/byte/time watchdog
            // turns a stream with no later audio into an explicit error.
            if !self.pending_video_packets.is_empty()
                || self.should_defer_video_packet()
                || video_queue_blocks_audio_driven_decode(
                    demand,
                    self.video_frames.len(),
                    self.active_video_frame_queue_limit(),
                )
            {
                self.pending_video_packets.push_back(packet);
                if let Some(started) = scan_started {
                    self.observe_buffering_audio_video_packet(packet_bytes, started.elapsed())?;
                }
                return Ok(());
            }
            let _ = self.route_video_packet(packet, demand)?;
            if let Some(started) = scan_started {
                self.observe_buffering_audio_video_packet(packet_bytes, started.elapsed())?;
            }
            return Ok(());
        }

        if self
            .audio_decoder
            .as_ref()
            .is_some_and(|decoder| packet.stream_index() == decoder.stream_index())
        {
            self.route_audio_packet(packet)?;
            return Ok(());
        }

        if self
            .subtitle_decoder
            .as_ref()
            .is_some_and(|decoder| packet.stream_index() == decoder.stream_index())
        {
            let decoder = self
                .subtitle_decoder
                .as_mut()
                .expect("subtitle decoder exists");
            if let Some(frame) = decoder.decode_packet(&packet)? {
                self.subtitle_frames.push_back(frame);
                trim_subtitle_queue(&mut self.subtitle_frames, self.queue_limits.subtitle_frames);
            }
        }
        Ok(())
    }

    fn route_audio_packet(&mut self, packet: ffmpeg::Packet) -> Result<()> {
        let mut send_result = self
            .audio_decoder
            .as_mut()
            .expect("audio decoder exists")
            .send_packet(&packet);
        if matches!(&send_result, Err(error) if error.is_again()) {
            self.drain_audio_frames()?;
            send_result = self
                .audio_decoder
                .as_mut()
                .expect("audio decoder exists")
                .send_packet(&packet);
        }

        match send_result {
            Ok(()) => {
                if self.audio_seek_dropped_packets > 0 {
                    trace::diagnostic(
                        serde_json::json!({
                            "event": "audio_decoder_recovery",
                            "stage": "seek_preroll_ready",
                            "stream": packet.stream_index(),
                            "droppedPackets": self.audio_seek_dropped_packets,
                            "packetPtsSeconds": packet
                                .pts()
                                .map(ffmpeg::PacketTimestamp::seconds),
                        })
                        .to_string(),
                    );
                }
                self.audio_seek_waiting_for_valid_packet = false;
                self.audio_seek_dropped_packets = 0;
                self.drain_audio_frames()?;
                Ok(())
            }
            Err(error)
                if self.audio_seek_waiting_for_valid_packet
                    && self.audio_seek_dropped_packets < AUDIO_SEEK_INVALID_PACKET_LIMIT
                    && error.is_invalid_argument() =>
            {
                self.audio_seek_dropped_packets = self.audio_seek_dropped_packets.saturating_add(1);
                trace::diagnostic(
                    serde_json::json!({
                        "event": "audio_decoder_recovery",
                        "stage": "seek_preroll_packet_dropped",
                        "stream": packet.stream_index(),
                        "droppedPackets": self.audio_seek_dropped_packets,
                        "dropLimit": AUDIO_SEEK_INVALID_PACKET_LIMIT,
                        "packetPtsSeconds": packet.pts().map(ffmpeg::PacketTimestamp::seconds),
                        "packetDtsSeconds": packet.dts().map(ffmpeg::PacketTimestamp::seconds),
                        "packetDurationSeconds": packet.duration_seconds(),
                        "packetBytes": packet.size(),
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
                Ok(())
            }
            Err(error) => {
                trace::diagnostic(
                    serde_json::json!({
                        "event": "decoder_packet_failure",
                        "kind": "audio",
                        "stream": packet.stream_index(),
                        "seekPreroll": self.audio_seek_waiting_for_valid_packet,
                        "seekPrerollDroppedPackets": self.audio_seek_dropped_packets,
                        "packetPtsSeconds": packet.pts().map(ffmpeg::PacketTimestamp::seconds),
                        "packetDtsSeconds": packet.dts().map(ffmpeg::PacketTimestamp::seconds),
                        "packetDurationSeconds": packet.duration_seconds(),
                        "packetBytes": packet.size(),
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
                Err(error.into())
            }
        }
    }

    fn route_pending_video_packets(&mut self, demand: PlaybackPumpDemand) -> Result<bool> {
        let mut routed_any = false;
        while !self.should_defer_video_packet()
            && !video_queue_blocks_audio_driven_decode(
                demand,
                self.video_frames.len(),
                self.active_video_frame_queue_limit(),
            )
        {
            let Some(packet) = self.pending_video_packets.pop_front() else {
                self.reset_video_packet_stall_state();
                return Ok(routed_any);
            };
            if !self.route_video_packet(packet, demand)? {
                self.observe_video_packet_stall()?;
                return Ok(routed_any);
            }
            self.reset_video_packet_stall_state();
            routed_any = true;
        }
        // Hardware surface decoders deliberately hold packets while their
        // bounded frame queue is full. That is consumer backpressure, not a
        // decoder send/receive deadlock, so a previous codec-stall deadline
        // must not leak into it.
        self.reset_video_packet_stall_state();
        Ok(routed_any)
    }

    fn observe_video_packet_stall(&mut self) -> Result<()> {
        self.video_packet_stall_polls = self.video_packet_stall_polls.saturating_add(1);
        let now = Instant::now();
        let started_at = *self.video_packet_stall_started_at.get_or_insert(now);
        let stalled_for = now.saturating_duration_since(started_at);
        if !self.video_packet_stall_logged {
            self.video_packet_stall_logged = true;
            trace::diagnostic(
                serde_json::json!({
                    "event": "decoder_input_stall",
                    "stage": "pending",
                    "polls": self.video_packet_stall_polls,
                    "pendingVideoPackets": self.pending_video_packets.len(),
                    "demuxEof": self.demux_eof,
                    "videoBackend": self.active_video_decoder_backend().map(DecoderBackend::as_str),
                    "retryOwner": "playback_pump",
                })
                .to_string(),
            );
        }
        if stalled_for < DECODER_INPUT_STALL_TIMEOUT {
            return Ok(());
        }

        let packet = self.pending_video_packets.front();
        let packet_pts = packet.and_then(ffmpeg::Packet::pts);
        let packet_dts = packet.and_then(ffmpeg::Packet::dts);
        let reason = format!(
            "video packet remained rejected for {:.3}s after {} polls (pending_packets={}, demux_ended={}, video_backend={}, packet_pts={:?}, packet_dts={:?}, packet_key={}, packet_bytes={})",
            stalled_for.as_secs_f64(),
            self.video_packet_stall_polls,
            self.pending_video_packets.len(),
            self.demux_eof,
            self.active_video_decoder_backend()
                .map(DecoderBackend::as_str)
                .unwrap_or("none"),
            packet_pts.map(ffmpeg::PacketTimestamp::seconds),
            packet_dts.map(ffmpeg::PacketTimestamp::seconds),
            packet.is_some_and(ffmpeg::Packet::is_key),
            packet.map_or(0, ffmpeg::Packet::size),
        );
        trace::diagnostic(
            serde_json::json!({
                "event": "decoder_input_stall",
                "stage": "timeout",
                "polls": self.video_packet_stall_polls,
                "stalledSeconds": stalled_for.as_secs_f64(),
                "pendingVideoPackets": self.pending_video_packets.len(),
                "demuxEof": self.demux_eof,
                "videoBackend": self.active_video_decoder_backend().map(DecoderBackend::as_str),
                "packetPtsSeconds": packet_pts.map(ffmpeg::PacketTimestamp::seconds),
                "packetDtsSeconds": packet_dts.map(ffmpeg::PacketTimestamp::seconds),
                "packetKey": packet.is_some_and(ffmpeg::Packet::is_key),
                "packetBytes": packet.map_or(0, ffmpeg::Packet::size),
                "reason": reason.as_str(),
            })
            .to_string(),
        );
        Err(PlaybackError::DecoderInputStall { reason })
    }

    fn reset_video_packet_stall_state(&mut self) {
        self.video_packet_stall_polls = 0;
        self.video_packet_stall_logged = false;
        self.video_packet_stall_started_at = None;
    }

    fn route_video_packet(
        &mut self,
        packet: ffmpeg::Packet,
        demand: PlaybackPumpDemand,
    ) -> Result<bool> {
        if self.video_fallback_waiting_for_keyframe {
            if !packet.is_key() {
                return Ok(true);
            }
            self.video_fallback_waiting_for_keyframe = false;
            let active_backend = self
                .active_video_decoder_backend()
                .unwrap_or(DecoderBackend::Software);
            trace::diagnostic(
                serde_json::json!({
                    "event": "video_decoder_recovery_keyframe",
                    "backend": active_backend.as_str(),
                    "mediaCodecMode": self.video_decoder.as_ref().and_then(|decoder| {
                        (decoder.backend() == DecoderBackend::MediaCodec).then_some(
                            if decoder.uses_mediacodec_surface() {
                                "surface_ahardwarebuffer"
                            } else {
                                "bytebuffer_cpu_upload"
                            },
                        )
                    }),
                    "stream": packet.stream_index(),
                })
                .to_string(),
            );
        }

        let before_frames = self.video_frames.len();
        match self.route_video_packet_with_active_decoder(&packet, demand) {
            Ok(Some(progress)) => Ok(progress),
            Ok(None) => {
                self.pending_video_packets.push_front(packet);
                Ok(self.video_frames.len() > before_frames)
            }
            Err(error)
                if self.active_video_decoder_backend() == Some(DecoderBackend::MediaCodec) =>
            {
                let reason = error.to_string();
                match self.mediacodec_fallback_target() {
                    Some(MediaCodecFallbackTarget::ByteBuffer) => self
                        .fallback_mediacodec_surface_to_byte_buffer(
                            "decode",
                            reason.clone(),
                            None,
                        )?,
                    Some(MediaCodecFallbackTarget::Software) => self
                        .fallback_video_decoder_to_software(
                            "decode_bytebuffer_to_software",
                            reason.clone(),
                            None,
                        )?,
                    None => return Err(error),
                }
                if !packet.is_key() {
                    self.video_fallback_waiting_for_keyframe = true;
                    let active_backend = self
                        .active_video_decoder_backend()
                        .unwrap_or(DecoderBackend::Software);
                    trace::diagnostic(
                        serde_json::json!({
                            "event": "video_decoder_recovery_waiting_for_keyframe",
                            "backend": active_backend.as_str(),
                            "stream": packet.stream_index(),
                            "reason": reason,
                        })
                        .to_string(),
                    );
                    return Ok(true);
                }
                match self.route_video_packet_with_active_decoder(&packet, demand)? {
                    Some(progress) => Ok(progress),
                    None => {
                        self.pending_video_packets.push_front(packet);
                        Ok(self.video_frames.len() > before_frames)
                    }
                }
            }
            Err(error)
                if self
                    .active_video_decoder_backend()
                    .is_some_and(should_fallback_video_decoder_error) =>
            {
                let backend = self.active_video_decoder_backend().expect("active decoder");
                let reason = error.to_string();
                self.fallback_video_decoder_to_software(
                    &format!("decode_{}_to_software", backend.as_str()),
                    reason.clone(),
                    None,
                )?;
                if !packet.is_key() {
                    self.video_fallback_waiting_for_keyframe = true;
                    trace::diagnostic(
                        serde_json::json!({
                            "event": "video_decoder_recovery_waiting_for_keyframe",
                            "backend": DecoderBackend::Software.as_str(),
                            "stream": packet.stream_index(),
                            "reason": reason,
                        })
                        .to_string(),
                    );
                    return Ok(true);
                }
                match self.route_video_packet_with_active_decoder(&packet, demand)? {
                    Some(progress) => Ok(progress),
                    None => {
                        self.pending_video_packets.push_front(packet);
                        Ok(self.video_frames.len() > before_frames)
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    fn route_video_packet_with_active_decoder(
        &mut self,
        packet: &ffmpeg::Packet,
        demand: PlaybackPumpDemand,
    ) -> Result<Option<bool>> {
        let send_result = self
            .video_decoder
            .as_mut()
            .expect("video decoder exists")
            .send_packet(packet);
        match send_result {
            Ok(()) => {
                let _ = self.drain_and_trim_video_frames(demand)?;
                Ok(Some(true))
            }
            Err(error) if error.is_again() => {
                let _ = self.drain_and_trim_video_frames(demand)?;
                let retry_result = self
                    .video_decoder
                    .as_mut()
                    .expect("video decoder exists")
                    .send_packet(packet);
                match retry_result {
                    Ok(()) => {
                        let _ = self.drain_and_trim_video_frames(demand)?;
                        Ok(Some(true))
                    }
                    Err(error) if error.is_again() => Ok(None),
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn drain_and_trim_video_frames(
        &mut self,
        demand: PlaybackPumpDemand,
    ) -> Result<DecoderDrainStatus> {
        let status = drain_video_frames(
            self.video_decoder.as_mut().expect("video decoder exists"),
            &mut self.video_frames,
            self.dolby_vision_profile,
            &mut self.dovi_rpu_diagnostics,
        )?;
        let video_frame_limit = self.active_video_frame_queue_limit();
        if demand != PlaybackPumpDemand::BufferingAudio {
            trim_video_queue(&mut self.video_frames, video_frame_limit);
            return Ok(status);
        }

        let excess = self.video_frames.len().saturating_sub(video_frame_limit);
        let droppable = buffering_video_trim_droppable(
            self.video_frames
                .iter()
                .take(excess)
                .map(|frame| frame.pts().and_then(|timestamp| timestamp.as_duration())),
            self.buffering_audio_video_scan.discarded_frames,
            self.buffering_audio_video_scan.baseline_media_time,
        );
        for _ in 0..droppable {
            let _ = self.video_frames.pop_front();
            self.record_buffering_video_advance();
        }
        if droppable < excess {
            let next_pts = self
                .video_frames
                .front()
                .and_then(DecodedVideoFrame::pts)
                .and_then(|timestamp| timestamp.as_duration());
            let reason = format!(
                "audio recovery decoder output reached the shared video advance budget (frames={}, max_frames={}, baseline_seconds={:?}, next_pts_seconds={:?}, max_advance_seconds={:.3})",
                self.buffering_audio_video_scan.discarded_frames,
                BUFFERING_VIDEO_MAX_DRAINED_FRAMES,
                self.buffering_audio_video_scan
                    .baseline_media_time
                    .map(|value| value.as_secs_f64()),
                next_pts.map(|value| value.as_secs_f64()),
                BUFFERING_VIDEO_MAX_ADVANCE.as_secs_f64(),
            );
            self.suspend_buffering_audio_recovery("video_decode_trim_limit", reason);
        }
        Ok(status)
    }

    fn active_video_decoder_backend(&self) -> Option<DecoderBackend> {
        self.video_decoder.as_ref().map(Decoder::backend)
    }

    fn active_video_codec_is_av1(&self) -> bool {
        self.video_decoder.as_ref().is_some_and(|decoder| {
            codec_parameters_for(&self.codec_parameters, decoder.stream_index())
                .ok()
                .and_then(|parameters| parameters.codec_name())
                .is_some_and(|codec| codec.eq_ignore_ascii_case("av1"))
        })
    }

    fn active_or_selected_video_stream_index(&self) -> Result<i32> {
        if let Some(decoder) = &self.video_decoder {
            return Ok(decoder.stream_index());
        }
        let track_id = self
            .info
            .selected_video_track
            .ok_or(PlaybackError::NoVideoTrack)?;
        stream_index_i32(track_id, TrackKind::Video, track_id)
    }

    fn mediacodec_fallback_target(&self) -> Option<MediaCodecFallbackTarget> {
        mediacodec_fallback_target(
            self.active_video_decoder_backend(),
            self.video_decoder
                .as_ref()
                .is_some_and(Decoder::uses_mediacodec_surface),
            self.mediacodec_surface_disabled,
        )
    }

    fn fallback_mediacodec_surface_to_byte_buffer(
        &mut self,
        stage: &str,
        reason: String,
        import_failure: Option<&VideoFrameImportFailure>,
    ) -> Result<()> {
        let stream_index = self.active_or_selected_video_stream_index()?;
        let codec = codec_parameters_for(&self.codec_parameters, stream_index)?.codec_name();
        self.mark_video_decoder_unavailable(format!(
            "{stage}: MediaCodec Surface decoder retired before byte-buffer fallback: {reason}"
        ));
        self.mediacodec_surface_disabled = true;
        let discarded = self.discard_video_frames_and_packets();
        let previous_decoder = self.video_decoder.take();
        let previous_backend = previous_decoder.as_ref().map(Decoder::backend);
        let previous_surface = previous_decoder
            .as_ref()
            .is_some_and(Decoder::uses_mediacodec_surface);
        drop(previous_decoder);
        trace::diagnostic(
            serde_json::json!({
                "event": "video_decoder_retired",
                "stage": format!("{stage}_surface_to_bytebuffer"),
                "previousBackend": previous_backend.map(DecoderBackend::as_str),
                "previousMediaCodecSurface": previous_surface,
                "discardedVideoFrames": discarded.video_frames,
                "discardedAudioFrames": discarded.audio_frames,
                "discardedSubtitleFrames": discarded.subtitle_frames,
                "discardedVideoPackets": discarded.video_packets,
                "fallbackGeneration": import_failure.map(|failure| failure.generation),
                "reason": reason.as_str(),
            })
            .to_string(),
        );
        let parameters = codec_parameters_for(&self.codec_parameters, stream_index)?;
        match Decoder::open_owned_with_config(parameters, DecoderConfig::mediacodec_byte_buffer()) {
            Ok(decoder) => {
                self.video_decoder = Some(decoder);
                self.info.video_decode_backend = Some(DecoderBackend::MediaCodec);
                self.clear_video_decoder_unavailable();
                self.video_decoder_fallbacks = self.video_decoder_fallbacks.saturating_add(1);
                let transition_stage = format!("{stage}_surface_to_bytebuffer");
                trace::diagnostic(
                    serde_json::json!({
                        "event": "android_mediacodec_fallback",
                        "stage": transition_stage.as_str(),
                        "fromMode": "surface_ahardwarebuffer",
                        "toMode": "bytebuffer_cpu_upload",
                        "surfaceZeroCopyDisabled": true,
                        "fallbackCount": self.video_decoder_fallbacks,
                        "reason": reason.as_str(),
                    })
                    .to_string(),
                );
                let event = VideoDecoderEvent {
                    stage: transition_stage,
                    requested_backend: DecoderBackend::MediaCodec,
                    previous_backend: Some(DecoderBackend::MediaCodec),
                    active_backend: DecoderBackend::MediaCodec,
                    fallback_count: self.video_decoder_fallbacks,
                    codec: import_failure
                        .and_then(|failure| failure.codec.clone())
                        .or(codec.clone()),
                    pixel_format: import_failure.and_then(|failure| failure.pixel_format.clone()),
                    line_sizes: import_failure.map(|failure| failure.line_sizes),
                    reason: Some(reason),
                };
                trace::diagnostic(event.structured_message());
                self.video_decoder_events.push_back(event);
                Ok(())
            }
            Err(byte_buffer_error) => {
                self.video_decoder_fallbacks = self.video_decoder_fallbacks.saturating_add(1);
                let byte_buffer_error = byte_buffer_error.to_string();
                trace::diagnostic(
                    serde_json::json!({
                        "event": "android_mediacodec_fallback",
                        "stage": format!("{stage}_bytebuffer_open_failed"),
                        "fromMode": "surface_ahardwarebuffer",
                        "attemptedMode": "bytebuffer_cpu_upload",
                        "toMode": "software_decode",
                        "surfaceZeroCopyDisabled": true,
                        "fallbackCount": self.video_decoder_fallbacks,
                        "surfaceReason": reason.as_str(),
                        "reason": byte_buffer_error.as_str(),
                    })
                    .to_string(),
                );
                self.fallback_video_decoder_to_software(
                    &format!("{stage}_bytebuffer_to_software"),
                    format!(
                        "MediaCodec surface mode failed: {reason}; MediaCodec byte-buffer mode failed: {byte_buffer_error}"
                    ),
                    import_failure,
                )
            }
        }
    }

    fn fallback_video_decoder_to_software(
        &mut self,
        stage: &str,
        reason: String,
        import_failure: Option<&VideoFrameImportFailure>,
    ) -> Result<()> {
        if require_hardware_decode() {
            return Err(PlaybackError::VideoDecoderUnavailable {
                reason: format!("hardware decoding is required; {stage}: {reason}"),
            });
        }
        let stream_index = self.active_or_selected_video_stream_index()?;
        let codec = codec_parameters_for(&self.codec_parameters, stream_index)?.codec_name();
        let previous_backend = self
            .video_decoder
            .as_ref()
            .map(Decoder::backend)
            .unwrap_or(DecoderBackend::MediaCodec);
        let previous_surface = self
            .video_decoder
            .as_ref()
            .is_some_and(Decoder::uses_mediacodec_surface);
        self.mark_video_decoder_unavailable(format!(
            "{stage}: {} decoder retired before software fallback: {reason}",
            previous_backend.as_str(),
        ));
        let discarded = self.discard_video_frames_and_packets();
        let previous_decoder = self.video_decoder.take();
        drop(previous_decoder);
        trace::diagnostic(
            serde_json::json!({
                "event": "video_decoder_retired",
                "stage": stage,
                "previousBackend": previous_backend.as_str(),
                "previousMediaCodecSurface": previous_surface,
                "discardedVideoFrames": discarded.video_frames,
                "discardedAudioFrames": discarded.audio_frames,
                "discardedSubtitleFrames": discarded.subtitle_frames,
                "discardedVideoPackets": discarded.video_packets,
                "fallbackGeneration": import_failure.map(|failure| failure.generation),
                "reason": reason.as_str(),
            })
            .to_string(),
        );
        let parameters = codec_parameters_for(&self.codec_parameters, stream_index)?;
        self.video_decoder_fallbacks = self.video_decoder_fallbacks.saturating_add(1);
        let decoder = match Decoder::open_owned_with_config(parameters, DecoderConfig::software()) {
            Ok(decoder) => decoder,
            Err(error) => {
                let decoder_error = error.to_string();
                let unavailable_reason =
                    format!("{stage}: {reason}; software decoder open failed: {decoder_error}");
                self.mark_video_decoder_unavailable(unavailable_reason.clone());
                trace::diagnostic(
                    serde_json::json!({
                        "event": "video_decoder_unavailable",
                        "stage": stage,
                        "requestedBackend": previous_backend.as_str(),
                        "previousBackend": previous_backend.as_str(),
                        "previousMediaCodecSurface": previous_surface,
                        "selectedVideoTrack": self.info.selected_video_track,
                        "codec": codec.as_deref(),
                        "fallbackGeneration": import_failure.map(|failure| failure.generation),
                        "fallbackCount": self.video_decoder_fallbacks,
                        "reason": unavailable_reason.as_str(),
                    })
                    .to_string(),
                );
                return Err(PlaybackError::VideoDecoderUnavailable {
                    reason: unavailable_reason,
                });
            }
        };
        self.video_decoder = Some(decoder);
        self.info.video_decode_backend = Some(DecoderBackend::Software);
        self.clear_video_decoder_unavailable();
        let event = VideoDecoderEvent {
            stage: stage.to_string(),
            requested_backend: previous_backend,
            previous_backend: Some(previous_backend),
            active_backend: DecoderBackend::Software,
            fallback_count: self.video_decoder_fallbacks,
            codec: import_failure
                .and_then(|failure| failure.codec.clone())
                .or(codec),
            pixel_format: import_failure.and_then(|failure| failure.pixel_format.clone()),
            line_sizes: import_failure.map(|failure| failure.line_sizes),
            reason: Some(reason),
        };
        trace::diagnostic(event.structured_message());
        self.video_decoder_events.push_back(event);
        Ok(())
    }

    fn fallback_after_video_import_failure(
        &mut self,
        stage: &str,
        failure: &VideoFrameImportFailure,
    ) -> Result<bool> {
        if !should_fallback_video_decoder_error(failure.decode_backend) {
            return Ok(false);
        }
        let active_backend = self.active_video_decoder_backend();
        let expected_backend = failure.decode_backend;
        if active_backend != Some(expected_backend) {
            trace::diagnostic(
                serde_json::json!({
                    "event": "video_frame_import_failure",
                    "stage": "stale_backend_feedback_ignored",
                    "failedBackend": failure.decode_backend.as_str(),
                    "activeBackend": active_backend.map(DecoderBackend::as_str),
                    "generation": failure.generation,
                    "reason": failure.reason.as_str(),
                })
                .to_string(),
            );
            return Ok(false);
        }
        if expected_backend != DecoderBackend::MediaCodec {
            let fallback_stage = format!("{stage}_{}_to_software", expected_backend.as_str());
            self.fallback_video_decoder_to_software(
                &fallback_stage,
                failure.reason.clone(),
                Some(failure),
            )?;
            return Ok(true);
        }
        let active_surface = self
            .video_decoder
            .as_ref()
            .is_some_and(Decoder::uses_mediacodec_surface);
        if failure.mediacodec_surface != active_surface {
            trace::diagnostic(
                serde_json::json!({
                    "event": "video_frame_import_failure",
                    "stage": "stale_route_feedback_ignored",
                    "failedMediaCodecSurface": failure.mediacodec_surface,
                    "activeMediaCodecSurface": active_surface,
                    "generation": failure.generation,
                    "reason": failure.reason.as_str(),
                })
                .to_string(),
            );
            return Ok(false);
        }
        match self.mediacodec_fallback_target() {
            Some(MediaCodecFallbackTarget::ByteBuffer) => self
                .fallback_mediacodec_surface_to_byte_buffer(
                    stage,
                    failure.reason.clone(),
                    Some(failure),
                )?,
            Some(MediaCodecFallbackTarget::Software) => self.fallback_video_decoder_to_software(
                &format!("{stage}_bytebuffer_to_software"),
                failure.reason.clone(),
                Some(failure),
            )?,
            None => return Ok(false),
        }
        self.video_fallback_waiting_for_keyframe = true;
        let active_backend = self
            .active_video_decoder_backend()
            .unwrap_or(DecoderBackend::Software);
        trace::diagnostic(
            serde_json::json!({
                "event": "video_decoder_recovery_waiting_for_keyframe",
                "backend": active_backend.as_str(),
                "mediaCodecMode": self.video_decoder.as_ref().and_then(|decoder| {
                    (decoder.backend() == DecoderBackend::MediaCodec).then_some(
                        if decoder.uses_mediacodec_surface() {
                            "surface_ahardwarebuffer"
                        } else {
                            "bytebuffer_cpu_upload"
                        },
                    )
                }),
                "codec": failure.codec.as_deref(),
                "pixelFormat": failure.pixel_format.as_deref(),
                "lineSizes": failure.line_sizes,
                "reason": failure.reason.as_str(),
            })
            .to_string(),
        );
        Ok(true)
    }

    fn video_import_failure_matches_active_decoder(
        &self,
        failure: &VideoFrameImportFailure,
    ) -> bool {
        failure.decode_backend
            == self
                .active_video_decoder_backend()
                .unwrap_or(DecoderBackend::Software)
            && (failure.decode_backend != DecoderBackend::MediaCodec
                || failure.mediacodec_surface
                    == self
                        .video_decoder
                        .as_ref()
                        .is_some_and(Decoder::uses_mediacodec_surface))
    }

    fn should_defer_video_packet(&self) -> bool {
        self.video_decoder.as_ref().is_some_and(|decoder| {
            (decoder.backend() == DecoderBackend::D3d11va
                && self.video_frames.len() >= D3D11VA_VIDEO_FRAME_QUEUE_LIMIT)
                || (decoder.uses_avcodec_surface()
                    && self.video_frames.len() >= AVCODEC_SURFACE_VIDEO_FRAME_QUEUE_LIMIT)
        })
    }

    fn active_video_frame_queue_limit(&self) -> usize {
        match self.video_decoder.as_ref() {
            Some(decoder) if decoder.backend() == DecoderBackend::D3d11va => {
                D3D11VA_VIDEO_FRAME_QUEUE_LIMIT
            }
            Some(decoder) if decoder.uses_avcodec_surface() => {
                AVCODEC_SURFACE_VIDEO_FRAME_QUEUE_LIMIT
            }
            _ => self.queue_limits.video_frames,
        }
    }

    fn finish_decoders(&mut self, demand: PlaybackPumpDemand) -> Result<bool> {
        if self.eof {
            return Ok(false);
        }

        let mut made_progress = false;
        if !self.video_decode_suspended {
            while self.route_pending_video_packets(demand)? {
                made_progress = true;
            }
        }
        let video_packets_pending = !self.pending_video_packets.is_empty();

        self.eof_drain_polls = self.eof_drain_polls.saturating_add(1);
        let now = Instant::now();
        self.eof_drain_last_progress_at.get_or_insert(now);

        if self.video_decoder.as_ref().is_some_and(|decoder| {
            !self.video_decode_suspended && !video_packets_pending && !decoder.is_end_of_stream()
        }) {
            if !self
                .video_decoder
                .as_ref()
                .expect("video decoder exists")
                .eof_sent()
            {
                match self
                    .video_decoder
                    .as_mut()
                    .expect("video decoder exists")
                    .send_eof()
                {
                    Ok(()) => made_progress = true,
                    Err(error) if error.is_again() => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let status = self.drain_and_trim_video_frames(demand)?;
            made_progress |= status.made_progress();
        }
        if self
            .audio_decoder
            .as_ref()
            .is_some_and(|decoder| !decoder.is_end_of_stream())
        {
            let eof_sent = self.audio_decoder.as_ref().is_some_and(Decoder::eof_sent);
            if !eof_sent {
                let decoder = self.audio_decoder.as_mut().expect("audio decoder exists");
                match decoder.send_eof() {
                    Ok(()) => made_progress = true,
                    Err(error) if error.is_again() => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let status = self.drain_audio_frames()?;
            made_progress |= status.made_progress();
        }
        if made_progress {
            self.eof_drain_last_progress_at = Some(now);
        }

        let video_complete = self.video_decode_suspended
            || (!video_packets_pending
                && self
                    .video_decoder
                    .as_ref()
                    .is_none_or(Decoder::is_end_of_stream));
        let audio_complete = self
            .audio_decoder
            .as_ref()
            .is_none_or(Decoder::is_end_of_stream);
        let complete = video_complete && audio_complete;
        if complete {
            trace::diagnostic(
                serde_json::json!({
                    "event": "decoder_eof_drain",
                    "stage": "complete",
                    "polls": self.eof_drain_polls,
                    "videoComplete": video_complete,
                    "audioComplete": audio_complete,
                    "videoBackend": self.active_video_decoder_backend().map(DecoderBackend::as_str),
                })
                .to_string(),
            );
            self.eof = true;
            return Ok(true);
        }

        if !self.eof_drain_pending_logged {
            self.eof_drain_pending_logged = true;
            trace::diagnostic(
                serde_json::json!({
                    "event": "decoder_eof_drain",
                    "stage": "pending",
                    "polls": self.eof_drain_polls,
                    "videoEofSubmitted": self.video_decoder.as_ref().is_none_or(Decoder::eof_sent),
                    "videoComplete": video_complete,
                    "audioEofSubmitted": self.audio_decoder.as_ref().is_none_or(Decoder::eof_sent),
                    "audioComplete": audio_complete,
                    "videoBackend": self.active_video_decoder_backend().map(DecoderBackend::as_str),
                    "retryOwner": "playback_pump",
                })
                .to_string(),
            );
        }

        let stalled_for = now.saturating_duration_since(
            self.eof_drain_last_progress_at
                .expect("EOF drain progress timestamp initialized"),
        );
        if stalled_for >= DECODER_EOF_DRAIN_STALL_TIMEOUT {
            let reason = format!(
                "no decoder drain progress for {:.3}s after {} polls (video_complete={video_complete}, audio_complete={audio_complete}, video_backend={})",
                stalled_for.as_secs_f64(),
                self.eof_drain_polls,
                self.active_video_decoder_backend()
                    .map(DecoderBackend::as_str)
                    .unwrap_or("none"),
            );
            trace::diagnostic(
                serde_json::json!({
                    "event": "decoder_eof_drain",
                    "stage": "timeout",
                    "polls": self.eof_drain_polls,
                    "stalledSeconds": stalled_for.as_secs_f64(),
                    "videoComplete": video_complete,
                    "audioComplete": audio_complete,
                    "videoBackend": self.active_video_decoder_backend().map(DecoderBackend::as_str),
                    "reason": reason.as_str(),
                })
                .to_string(),
            );
            return Err(PlaybackError::DecoderDrainTimeout { reason });
        }
        Ok(made_progress)
    }

    fn drain_audio_frames(&mut self) -> Result<DecoderDrainStatus> {
        let mut status = DecoderDrainStatus::default();
        loop {
            let output = {
                let decoder = self.audio_decoder.as_mut().expect("audio decoder exists");
                decoder.receive_frame()?
            };
            match output {
                DecoderOutputFrame::Frame(frame) => {
                    let pcm = self.convert_audio_frame(frame)?;
                    if self.audio_output_active {
                        self.audio_frames.push_back(pcm);
                    }
                    status.frames = status.frames.saturating_add(1);
                }
                DecoderOutputFrame::NeedMoreInput => return Ok(status),
                DecoderOutputFrame::EndOfStream => {
                    status.frames = status.frames.saturating_add(self.drain_audio_resampler()?);
                    status.end_of_stream = true;
                    return Ok(status);
                }
            }
        }
    }

    fn reset_eof_drain_state(&mut self) {
        self.demux_eof = false;
        self.reset_video_packet_stall_state();
        self.eof_drain_polls = 0;
        self.eof_drain_pending_logged = false;
        self.eof_drain_last_progress_at = None;
        self.eof = false;
    }

    fn convert_audio_frame(&mut self, frame: Frame) -> Result<PcmAudioFrame> {
        if self.audio_resampler.is_none() {
            self.audio_resampler = Some(AudioResampler::new_from_frame(&frame, self.audio_output)?);
        }
        self.audio_resampler
            .as_mut()
            .expect("audio resampler exists")
            .convert(&frame)
            .map_err(PlaybackError::from)
    }

    fn drain_audio_resampler(&mut self) -> Result<usize> {
        let Some(resampler) = self.audio_resampler.as_mut() else {
            return Ok(0);
        };
        let mut output_frames = 0usize;
        while let Some(frame) = resampler.drain()? {
            if self.audio_output_active {
                self.audio_frames.push_back(frame);
            }
            output_frames = output_frames.saturating_add(1);
        }
        if output_frames > 0 {
            trace::diagnostic(
                serde_json::json!({
                    "event": "audio_resampler_drain",
                    "stage": "complete",
                    "outputFrames": output_frames,
                    "audioOutputActive": self.audio_output_active,
                    "queuedAudioFrames": self.audio_frames.len(),
                })
                .to_string(),
            );
        }
        Ok(output_frames)
    }
}

fn audio_demand_may_scan_past_pending_video(
    demand: PlaybackPumpDemand,
    pending_video_packets: usize,
) -> bool {
    demand.is_audio() && pending_video_packets < AUDIO_DEMAND_PENDING_VIDEO_PACKET_LIMIT
}

fn video_queue_blocks_audio_driven_decode(
    demand: PlaybackPumpDemand,
    queued_video_frames: usize,
    video_frame_limit: usize,
) -> bool {
    demand.is_audio() && queued_video_frames >= video_frame_limit
}

fn audio_queue_blocks_demux(
    demand: PlaybackPumpDemand,
    audio_output_active: bool,
    queued_audio_frames: usize,
    audio_frame_limit: usize,
) -> bool {
    // Audio and video packets share the same demux stream. A full decoded
    // audio queue may stop audio prefetch, but it must not prevent a video
    // request from scanning past interleaved audio packets to reach the next
    // video packet. Otherwise the 8-frame local video queue drains, then
    // stalls behind the 16-frame audio limit in a visible ~300 ms cycle.
    //
    // Video demand still needs a ceiling: a container whose audio is
    // interleaved far ahead of video, or a video stream that stops yielding
    // packets, would otherwise grow the decoded audio queue without bound. The
    // slack is wide enough to cross any sane interleave distance and only
    // stops pathological streams.
    if !audio_output_active {
        return false;
    }
    let limit = match demand {
        PlaybackPumpDemand::Audio | PlaybackPumpDemand::BufferingAudio => audio_frame_limit,
        PlaybackPumpDemand::Video => audio_frame_limit.max(VIDEO_DEMAND_AUDIO_FRAME_CEILING),
    };
    queued_audio_frames >= limit
}

enum SubtitleQueueCandidate {
    Embedded { start: Duration },
    External { index: usize, start: Duration },
}

impl SubtitleQueueCandidate {
    fn start(&self) -> Duration {
        match self {
            Self::Embedded { start } | Self::External { start, .. } => *start,
        }
    }
}

fn select_ready_subtitle(
    embedded_start: Option<Duration>,
    external_starts: impl IntoIterator<Item = (usize, Duration)>,
    media_time: Duration,
) -> Option<SubtitleQueueCandidate> {
    let embedded = embedded_start
        .filter(|start| *start <= media_time)
        .map(|start| SubtitleQueueCandidate::Embedded { start });
    external_starts
        .into_iter()
        .filter_map(|(index, start)| {
            (start <= media_time).then_some(SubtitleQueueCandidate::External { index, start })
        })
        .chain(embedded)
        .min_by_key(SubtitleQueueCandidate::start)
}

fn decoded_subtitle_start(frame: &DecodedSubtitleFrame) -> Duration {
    frame.start.unwrap_or(Duration::ZERO)
}

struct ExternalSubtitleSession {
    demuxer: Demuxer,
    decoder: SubtitleDecoder,
    track: SubtitleTrackConfig,
    frames: VecDeque<DecodedSubtitleFrame>,
    eof: bool,
}

impl ExternalSubtitleSession {
    fn open(mut config: SubtitleTrackConfig) -> Result<Self> {
        let uri = match &config.source {
            crate::subtitle::SubtitleTrackSource::External { uri } => uri.clone(),
            crate::subtitle::SubtitleTrackSource::Embedded { stream_index } => {
                return Err(PlaybackError::SubtitleTrackNotRemovable(*stream_index));
            }
        };
        let source = match external_subtitle_source(&uri) {
            Some(source) => source,
            None => source_from_uri_with_hint(&uri, crate::core::MediaSourceHint::Auto)?,
        };
        let mut demuxer = Demuxer::open_source(source)?;
        let stream_index = demuxer
            .probe()
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Subtitle)
            .map(|track| track.id as i32)
            .ok_or(PlaybackError::NoSubtitleTrack)?;
        let decoder = demuxer.open_subtitle_decoder(stream_index)?;
        demuxer.set_stream_selection(StreamSelection::only([stream_index]))?;

        if let Some(probed) = demuxer
            .probe()
            .subtitles
            .iter()
            .find(|track| track.source.is_embedded())
        {
            config.language = config.language.or_else(|| probed.language.clone());
            config.title = config.title.or_else(|| probed.title.clone());
        }
        if config.title.is_none() {
            config.title = external_subtitle_title(&uri);
        }

        Ok(Self {
            demuxer,
            decoder,
            track: config,
            frames: VecDeque::new(),
            eof: false,
        })
    }

    fn track(&self) -> &SubtitleTrackConfig {
        &self.track
    }

    fn pump_until(&mut self, media_time: Duration) -> Result<()> {
        let lookahead = media_time.saturating_add(EXTERNAL_SUBTITLE_LOOKAHEAD);
        while self.frames.len() < SUBTITLE_FRAME_QUEUE_LIMIT && !self.eof {
            if self
                .frames
                .back()
                .and_then(|frame| frame.start)
                .is_some_and(|start| start > lookahead)
            {
                break;
            }

            match self.demuxer.read_packet()? {
                Some(packet) => {
                    if let Some(frame) = self.decoder.decode_packet(&packet)? {
                        self.frames.push_back(frame.with_track_id(self.track.id));
                    }
                }
                None => self.eof = true,
            }
        }
        Ok(())
    }

    fn peek_start(&self) -> Option<Duration> {
        self.frames
            .front()
            .and_then(|frame| frame.start)
            .or_else(|| self.frames.front().map(|_| Duration::ZERO))
    }

    fn pop_front(&mut self) -> Option<DecodedSubtitleFrame> {
        self.frames.pop_front()
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        self.demuxer.seek(position)?;
        self.decoder.flush();
        self.frames.clear();
        self.eof = false;
        Ok(())
    }
}

/// Whether charset inspection should take over opening `uri`.
///
/// True for the text subtitle formats FFmpeg parses as UTF-8, and also for a
/// URI whose last path segment carries no extension at all -- Android hands us
/// `fd://<n>?offset=...&length=...` for a content:// pick, which names no file
/// yet is exactly where GBK/Big5/Shift-JIS sidecars turn up. A known extension
/// that is not a text format (PGS `.sup`, VobSub `.idx`/`.sub`) is left to the
/// regular source path so its bytes are never buffered.
fn external_subtitle_needs_charset_inspection(uri: &str) -> bool {
    let path = crate::subtitle::uri_path_component(uri);
    match crate::subtitle::subtitle_path_extension(path) {
        Some(_) => crate::subtitle::SubtitleFileFormat::from_path(path).is_some(),
        None => true,
    }
}

/// Opens an external text subtitle, transcoding it to UTF-8 when its bytes are
/// not already valid UTF-8.
///
/// Returns `None` only when the URI was never taken over (see
/// [`external_subtitle_needs_charset_inspection`]) or could not be read, in
/// which case the caller opens it through the regular source path. Once the
/// bytes have been read this **always** yields a source holding them, even for
/// passthrough: the URI must not be opened a second time. Android content
/// descriptors are one-shot and a reopen fails outright, and an HTTP sidecar
/// would otherwise be downloaded twice for the common already-UTF-8 case.
fn external_subtitle_source(uri: &str) -> Option<Box<dyn source::MediaSource>> {
    if !external_subtitle_needs_charset_inspection(uri) {
        return None;
    }
    let bytes = match source::read_uri_to_end(uri) {
        Ok(bytes) => bytes,
        Err(error) => {
            trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_charset",
                    "uri": uri,
                    "detected": "unread",
                    "transcoded": false,
                    "error": error.to_string(),
                })
                .to_string(),
            );
            return None;
        }
    };
    let inspection = crate::subtitle_charset::inspect(&bytes);
    let transcoded = inspection.utf8.is_some();
    trace::diagnostic(
        serde_json::json!({
            "event": "subtitle_charset",
            "uri": uri,
            "detected": inspection.detected,
            "transcoded": transcoded,
        })
        .to_string(),
    );
    Some(Box::new(
        crate::subtitle_charset::TranscodedMemorySource::new(
            uri.to_string(),
            inspection.utf8.unwrap_or(bytes),
        ),
    ))
}

fn external_subtitle_title(uri: &str) -> Option<String> {
    let leaf = uri
        .rsplit_once('/')
        .map(|(_, leaf)| leaf)
        .unwrap_or(uri)
        .trim();
    (!leaf.is_empty()).then_some(leaf.to_string())
}

fn codec_parameters_for(
    parameters: &[OwnedCodecParameters],
    stream_index: i32,
) -> Result<&OwnedCodecParameters> {
    parameters
        .iter()
        .find(|parameters| parameters.stream_index() == stream_index)
        .ok_or(PlaybackError::TrackNotFound {
            kind: TrackKind::Video,
            track_id: i64::from(stream_index),
        })
}

fn first_decodable_audio_track<T, E>(
    track_ids: impl IntoIterator<Item = i32>,
    mut open: impl FnMut(i32) -> std::result::Result<T, E>,
) -> std::result::Result<Option<(i32, T, Vec<(i32, E)>)>, Vec<(i32, E)>> {
    let mut failures = Vec::new();
    let mut found_track = false;
    for track_id in track_ids {
        found_track = true;
        match open(track_id) {
            Ok(decoder) => return Ok(Some((track_id, decoder, failures))),
            Err(error) => failures.push((track_id, error)),
        }
    }
    if found_track { Err(failures) } else { Ok(None) }
}

fn mark_selected_tracks(
    tracks: &mut [TrackInfo],
    selected_video: Option<i64>,
    selected_audio: Option<i64>,
    selected_subtitle: Option<i64>,
) {
    for track in tracks {
        track.selected = match track.kind {
            TrackKind::Video => selected_video == Some(track.id),
            TrackKind::Audio => selected_audio == Some(track.id),
            TrackKind::Subtitle => selected_subtitle == Some(track.id),
        };
    }
}

fn clear_subtitle_frame(
    track_id: Option<i64>,
    media_time: Duration,
) -> Option<DecodedSubtitleFrame> {
    track_id.map(|track_id| DecodedSubtitleFrame::new(track_id, Some(media_time), None))
}

fn stream_index_i32(stream_index: i64, kind: TrackKind, track_id: i64) -> Result<i32> {
    i32::try_from(stream_index).map_err(|_| PlaybackError::TrackNotFound { kind, track_id })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackRunState {
    Paused,
    Playing,
    Stopped,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackClockMode {
    Wall,
    AudioMaster,
}

impl Default for PlaybackClockMode {
    fn default() -> Self {
        Self::AudioMaster
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSyncConfig {
    pub enabled: bool,
    pub deadband: Duration,
    pub max_correction_per_frame: Duration,
    pub snap_threshold: Duration,
}

impl Default for AudioSyncConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            deadband: Duration::from_millis(5),
            max_correction_per_frame: Duration::from_millis(5),
            snap_threshold: Duration::from_millis(250),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackTimingConfig {
    pub clock_mode: PlaybackClockMode,
    pub video_scheduler: VideoFrameScheduler,
    pub audio_lead_time: Duration,
    pub audio_sync: AudioSyncConfig,
}

impl Default for PlaybackTimingConfig {
    fn default() -> Self {
        Self {
            clock_mode: PlaybackClockMode::default(),
            video_scheduler: VideoFrameScheduler::default(),
            audio_lead_time: DEFAULT_AUDIO_LEAD_TIME,
            audio_sync: AudioSyncConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackClockSource {
    Wall,
    Audio,
    Display,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockCorrectionDirection {
    None,
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockCorrection {
    pub source: PlaybackClockSource,
    pub direction: ClockCorrectionDirection,
    pub drift: Duration,
    pub applied: Duration,
    pub snapped: bool,
}

impl ClockCorrection {
    pub const fn none(source: PlaybackClockSource) -> Self {
        Self {
            source,
            direction: ClockCorrectionDirection::None,
            drift: Duration::ZERO,
            applied: Duration::ZERO,
            snapped: false,
        }
    }
}

impl Default for PlaybackClockSource {
    fn default() -> Self {
        Self::Wall
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaybackClockSnapshot {
    pub media_time: Duration,
    pub source: PlaybackClockSource,
    pub rate: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackClock {
    base_media_time: Duration,
    anchor: Option<Instant>,
    rate: f64,
    source: PlaybackClockSource,
}

impl PlaybackClock {
    pub fn paused_at(media_time: Duration) -> Self {
        Self {
            base_media_time: media_time,
            anchor: None,
            rate: 1.0,
            source: PlaybackClockSource::Wall,
        }
    }

    pub fn running_at(media_time: Duration, now: Instant) -> Self {
        let mut clock = Self::paused_at(media_time);
        clock.anchor = Some(now);
        clock
    }

    pub fn media_time_at(&self, now: Instant) -> Duration {
        let Some(anchor) = self.anchor else {
            return self.base_media_time;
        };
        self.base_media_time
            .saturating_add(scale_duration(elapsed_since(anchor, now), self.rate))
    }

    pub fn snapshot_at(&self, now: Instant) -> PlaybackClockSnapshot {
        PlaybackClockSnapshot {
            media_time: self.media_time_at(now),
            source: self.source,
            rate: self.rate,
        }
    }

    pub fn is_running(&self) -> bool {
        self.anchor.is_some()
    }

    pub fn source(&self) -> PlaybackClockSource {
        self.source
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn play(&mut self, now: Instant) {
        if self.anchor.is_none() {
            self.anchor = Some(now);
        }
    }

    pub fn pause(&mut self, now: Instant) {
        self.base_media_time = self.media_time_at(now);
        self.anchor = None;
    }

    pub fn seek(&mut self, media_time: Duration, now: Instant) {
        self.base_media_time = media_time;
        if self.anchor.is_some() {
            self.anchor = Some(now);
        }
    }

    pub fn reset(&mut self, media_time: Duration, running: bool, now: Instant) {
        self.base_media_time = media_time;
        self.anchor = running.then_some(now);
        self.source = PlaybackClockSource::Wall;
    }

    pub fn sync_to(&mut self, media_time: Duration, now: Instant, source: PlaybackClockSource) {
        self.base_media_time = media_time;
        if self.anchor.is_some() {
            self.anchor = Some(now);
        }
        self.source = source;
    }

    pub fn discipline_to(
        &mut self,
        reference_time: Duration,
        now: Instant,
        source: PlaybackClockSource,
        config: AudioSyncConfig,
    ) -> ClockCorrection {
        self.source = source;
        if !config.enabled {
            return ClockCorrection::none(source);
        }

        let current = self.media_time_at(now);
        let drift_nanos = duration_to_nanos(reference_time) - duration_to_nanos(current);
        let drift_abs = nanos_to_duration(drift_nanos.abs());
        let direction = if drift_nanos > 0 {
            ClockCorrectionDirection::Forward
        } else if drift_nanos < 0 {
            ClockCorrectionDirection::Backward
        } else {
            ClockCorrectionDirection::None
        };

        if drift_abs <= config.deadband || direction == ClockCorrectionDirection::None {
            return ClockCorrection {
                source,
                direction,
                drift: drift_abs,
                applied: Duration::ZERO,
                snapped: false,
            };
        }

        if drift_abs >= config.snap_threshold {
            self.sync_to(reference_time, now, source);
            return ClockCorrection {
                source,
                direction,
                drift: drift_abs,
                applied: drift_abs,
                snapped: true,
            };
        }

        let applied = drift_abs.min(config.max_correction_per_frame);
        let correction_nanos = match direction {
            ClockCorrectionDirection::Forward => duration_to_nanos(applied),
            ClockCorrectionDirection::Backward => -duration_to_nanos(applied),
            ClockCorrectionDirection::None => 0,
        };
        self.sync_to(add_signed_duration(current, correction_nanos), now, source);
        ClockCorrection {
            source,
            direction,
            drift: drift_abs,
            applied,
            snapped: false,
        }
    }

    pub fn set_rate(&mut self, rate: f64, now: Instant) {
        self.base_media_time = self.media_time_at(now);
        self.rate = sanitize_playback_rate(rate);
        if self.anchor.is_some() {
            self.anchor = Some(now);
        }
    }
}

impl Default for PlaybackClock {
    fn default() -> Self {
        Self::paused_at(Duration::ZERO)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFrameDecision {
    Present { late_by: Option<Duration> },
    Wait { early_by: Duration },
    Drop { late_by: Duration },
}

impl VideoFrameDecision {
    pub fn late_by(self) -> Option<Duration> {
        match self {
            Self::Present { late_by } => late_by,
            Self::Drop { late_by } => Some(late_by),
            Self::Wait { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrameScheduler {
    pub lead_time: Duration,
    pub drop_tolerance: Duration,
    pub max_consecutive_drops: usize,
}

impl VideoFrameScheduler {
    pub fn new(lead_time: Duration, drop_tolerance: Duration) -> Self {
        Self {
            lead_time,
            drop_tolerance,
            max_consecutive_drops: 5,
        }
    }

    pub fn schedule(
        self,
        pts: Option<Duration>,
        media_time: Duration,
        first_frame: bool,
    ) -> VideoFrameDecision {
        let Some(pts) = pts else {
            return VideoFrameDecision::Present { late_by: None };
        };
        if first_frame {
            return VideoFrameDecision::Present {
                late_by: media_time.checked_sub(pts),
            };
        }
        if media_time.saturating_add(self.lead_time) < pts {
            return VideoFrameDecision::Wait {
                early_by: pts.saturating_sub(media_time.saturating_add(self.lead_time)),
            };
        }
        let late_by = media_time.checked_sub(pts);
        if late_by.is_some_and(|late| late > self.drop_tolerance) {
            return VideoFrameDecision::Drop {
                late_by: late_by.expect("checked above"),
            };
        }
        VideoFrameDecision::Present { late_by }
    }
}

impl Default for VideoFrameScheduler {
    fn default() -> Self {
        Self::new(Duration::from_millis(4), Duration::from_millis(120))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplaySyncConfig {
    pub enabled: bool,
    pub vsync_interval: Duration,
    pub allow_zero_vsyncs: bool,
}

impl DisplaySyncConfig {
    pub fn for_refresh_rate_hz(refresh_rate_hz: f64) -> Self {
        let interval = if refresh_rate_hz.is_finite() && refresh_rate_hz > 0.0 {
            Duration::from_secs_f64(1.0 / refresh_rate_hz)
        } else {
            Duration::from_millis(16)
        };
        Self {
            vsync_interval: interval,
            ..Self::default()
        }
    }
}

impl Default for DisplaySyncConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            vsync_interval: Duration::from_nanos(16_666_667),
            allow_zero_vsyncs: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DisplaySyncState {
    residual_error_nanos: i128,
}

impl DisplaySyncState {
    pub fn reset(&mut self) {
        self.residual_error_nanos = 0;
    }

    pub fn residual_error_nanos(&self) -> i128 {
        self.residual_error_nanos
    }

    pub fn schedule_frame(
        &mut self,
        frame_duration: Duration,
        config: DisplaySyncConfig,
    ) -> DisplayFrameSchedule {
        if !config.enabled || config.vsync_interval.is_zero() {
            return DisplayFrameSchedule {
                vsyncs: 1,
                scheduled_duration: frame_duration,
                residual_error_nanos: self.residual_error_nanos,
            };
        }

        let vsync_nanos = duration_to_nanos(config.vsync_interval).max(1);
        let target_nanos = duration_to_nanos(frame_duration) + self.residual_error_nanos;
        let rounded = if target_nanos <= 0 {
            0
        } else {
            (target_nanos + vsync_nanos / 2) / vsync_nanos
        };
        let min_vsyncs = if config.allow_zero_vsyncs { 0 } else { 1 };
        let vsyncs = rounded.max(min_vsyncs).min(u32::MAX as i128) as u32;
        let scheduled_nanos = vsyncs as i128 * vsync_nanos;
        self.residual_error_nanos = target_nanos - scheduled_nanos;

        DisplayFrameSchedule {
            vsyncs,
            scheduled_duration: nanos_to_duration(scheduled_nanos),
            residual_error_nanos: self.residual_error_nanos,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayFrameSchedule {
    pub vsyncs: u32,
    pub scheduled_duration: Duration,
    pub residual_error_nanos: i128,
}

pub struct TimedVideoFrame {
    pub frame: Frame,
    pub decode_backend: DecoderBackend,
    pub pts: Option<Duration>,
    pub media_time: Duration,
    pub late_by: Option<Duration>,
}

pub struct TimedAudioFrame {
    pub frame: PcmAudioFrame,
    pub pts: Option<Duration>,
    pub media_time: Duration,
    pub late_by: Option<Duration>,
}

pub struct TimedSubtitleFrame {
    pub frame: DecodedSubtitleFrame,
    pub pts: Option<Duration>,
    pub media_time: Duration,
    pub late_by: Option<Duration>,
}

pub struct VideoPlaybackEngine {
    session: PlaybackSession,
    state: PlaybackRunState,
    clock: PlaybackClock,
    timing: PlaybackTimingConfig,
    buffer_recovery_audio: Duration,
    pending_frame: Option<DecodedVideoFrame>,
    pending_audio: Option<PcmAudioFrame>,
    pending_subtitle: Option<DecodedSubtitleFrame>,
    last_audio_output_end: Option<Duration>,
    audio_output_active: bool,
    audio_eof_stall_started_at: Option<Instant>,
    audio_eof_stall_pending_frames: usize,
    audio_eof_stall_logged: bool,
    last_audio_clock_sample: Option<(AudioClockSnapshot, Instant)>,
    audio_clock_output_epoch: Option<u64>,
    audio_clock_floor: Duration,
    last_presented_pts: Option<Duration>,
    eof: bool,
    waiting_for_first_frame: bool,
    buffering: bool,
    buffering_video_pts: Option<Duration>,
    last_buffering_video_drain_at: Option<Instant>,
    buffering_video_drained_frames: usize,
    paused_seek_frame_pending: bool,
    video_seek_floor: Option<Duration>,
    audio_seek_floor: Option<Duration>,
    video_seek_preroll_started_at: Option<Instant>,
    video_seek_preroll_dropped_frames: u64,
    last_video_seek_preroll_log: Option<Instant>,
}

impl VideoPlaybackEngine {
    pub fn set_video_decode_suspended(&mut self, suspended: bool) -> Result<()> {
        let resume_position = (!suspended).then(|| self.media_time_at(Instant::now()));
        if suspended {
            self.pending_frame = None;
        }
        if let Some(position) = resume_position {
            self.session.video_decode_suspended = false;
            self.session.seek_with_decoder_flush(position, true, true)?;
        } else {
            self.session.set_video_decode_suspended(true);
        }
        if !suspended {
            self.pending_frame = None;
            self.pending_audio = None;
            self.pending_subtitle = None;
            self.last_presented_pts = None;
            self.waiting_for_first_frame = true;
            self.video_seek_floor = resume_position;
            // Audio already handed to the output remains queued while video
            // seeks back to a keyframe. Do not enqueue that PCM a second time.
            self.audio_seek_floor = resume_position
                .map(|position| position.max(self.last_audio_output_end.unwrap_or(position)));
            self.reset_video_seek_preroll_budget(Instant::now());
        }
        Ok(())
    }
}

unsafe impl Send for VideoPlaybackEngine {}

impl Drop for VideoPlaybackEngine {
    fn drop(&mut self) {
        // pending_frame is declared after session, so without an explicit
        // teardown it would outlive the MediaCodec decoder owned by session.
        // Drop engine- and session-owned frames first while that decoder and
        // its release callback context are both valid.
        let decoder_alive = self.session.video_decoder.is_some();
        let pending_video = self.pending_frame.take().is_some();
        let pending_audio = self.pending_audio.take().is_some();
        let pending_subtitle = self.pending_subtitle.take().is_some();
        let discarded = self.session.discard_queued_frames_and_packets();
        trace::diagnostic(
            serde_json::json!({
                "event": "playback_engine_teardown",
                "stage": "drop_before_decoder",
                "pendingVideoFrame": pending_video,
                "pendingAudioFrame": pending_audio,
                "pendingSubtitleFrame": pending_subtitle,
                "discardedVideoFrames": discarded.video_frames,
                "discardedAudioFrames": discarded.audio_frames,
                "discardedSubtitleFrames": discarded.subtitle_frames,
                "discardedVideoPackets": discarded.video_packets,
                "decoderAliveDuringCleanup": decoder_alive,
            })
            .to_string(),
        );
    }
}

impl VideoPlaybackEngine {
    pub fn open(request: &MediaRequest, config: PlaybackSessionConfig) -> Result<Self> {
        Self::open_with_decoder_resources(request, config, PlaybackDecoderResources::default())
    }

    pub(crate) fn open_with_decoder_resources(
        request: &MediaRequest,
        config: PlaybackSessionConfig,
        decoder_resources: PlaybackDecoderResources,
    ) -> Result<Self> {
        let timing = playback_timing_for_request(request, config.timing);
        Ok(Self::from_session_with_timing(
            PlaybackSession::open_with_decoder_resources(request, config, decoder_resources)?,
            timing,
        ))
    }

    pub fn from_session(session: PlaybackSession) -> Self {
        Self::from_session_with_timing(session, PlaybackTimingConfig::default())
    }

    pub fn from_session_with_timing(
        session: PlaybackSession,
        timing: PlaybackTimingConfig,
    ) -> Self {
        let buffer_recovery_audio = session.buffer_recovery_audio;
        Self {
            session,
            state: PlaybackRunState::Paused,
            clock: PlaybackClock::default(),
            timing,
            buffer_recovery_audio,
            pending_frame: None,
            pending_audio: None,
            pending_subtitle: None,
            last_audio_output_end: None,
            audio_output_active: true,
            audio_eof_stall_started_at: None,
            audio_eof_stall_pending_frames: 0,
            audio_eof_stall_logged: false,
            last_audio_clock_sample: None,
            audio_clock_output_epoch: None,
            audio_clock_floor: Duration::ZERO,
            last_presented_pts: None,
            eof: false,
            waiting_for_first_frame: false,
            buffering: false,
            buffering_video_pts: None,
            last_buffering_video_drain_at: None,
            buffering_video_drained_frames: 0,
            paused_seek_frame_pending: false,
            video_seek_floor: None,
            audio_seek_floor: None,
            video_seek_preroll_started_at: None,
            video_seek_preroll_dropped_frames: 0,
            last_video_seek_preroll_log: None,
        }
    }

    pub fn info(&self) -> &OpenedMediaInfo {
        self.session.info()
    }

    pub fn take_video_decoder_events(&mut self) -> Vec<VideoDecoderEvent> {
        self.session.take_video_decoder_events()
    }

    pub fn handle_video_frame_import_failure(
        &mut self,
        failure: &VideoFrameImportFailure,
    ) -> Result<bool> {
        self.handle_video_frame_failure("renderer_import", failure)
    }

    fn handle_video_frame_failure(
        &mut self,
        stage: &str,
        failure: &VideoFrameImportFailure,
    ) -> Result<bool> {
        let fallback_requires_replay = self.session.decoder_fallback_requires_replay();
        let replay_position = self.last_presented_pts.unwrap_or_else(|| self.media_time());
        let resume_after_replay = self.state == PlaybackRunState::Playing;
        if !self
            .session
            .video_import_failure_matches_active_decoder(failure)
        {
            let changed = self
                .session
                .fallback_after_video_import_failure(stage, failure)?;
            if changed {
                self.finish_video_decoder_fallback(
                    stage,
                    fallback_requires_replay,
                    replay_position,
                    resume_after_replay,
                )?;
            }
            return Ok(changed);
        }
        let pending_frame_discarded = self.pending_frame.take().is_some();
        if pending_frame_discarded {
            trace::diagnostic(
                serde_json::json!({
                    "event": "video_decoder_retired",
                    "stage": format!("{stage}_engine_pending_frame"),
                    "discardedEnginePendingFrame": true,
                    "fallbackGeneration": failure.generation,
                    "reason": failure.reason.as_str(),
                })
                .to_string(),
            );
        }
        let changed = self
            .session
            .fallback_after_video_import_failure(stage, failure)?;
        if changed {
            self.finish_video_decoder_fallback(
                stage,
                fallback_requires_replay,
                replay_position,
                resume_after_replay,
            )?;
        }
        Ok(changed)
    }

    #[cfg(target_os = "android")]
    fn prepare_pending_mediacodec_frame(&mut self) -> Result<bool> {
        let preparation = self
            .pending_frame
            .as_ref()
            .expect("pending frame exists")
            .frame
            .prepare_mediacodec_image();
        if let Err(error) = preparation {
            if error.is_android_mediacodec_backpressure() {
                // Keep the already-released frame pending. A later worker tick
                // retries only ImageReader acquisition; it must never release
                // the codec token twice.
                return Ok(false);
            }
            let pending = self.pending_frame.as_ref().expect("pending frame exists");
            let failure = VideoFrameImportFailure {
                decode_backend: pending.decode_backend,
                mediacodec_surface: pending.frame.is_mediacodec(),
                codec: self.session.selected_video_codec(),
                pixel_format: pending.frame.pixel_format(),
                line_sizes: pending.frame.line_sizes(),
                width: pending.frame.width(),
                height: pending.frame.height(),
                generation: 0,
                reason: format!("stage=android_mediacodec_worker_prepare reason={error}"),
            };
            trace::diagnostic(failure.structured_message());
            if self.handle_video_frame_failure("surface_delivery", &failure)? {
                return Ok(false);
            }
            return Err(error.into());
        }
        Ok(true)
    }

    fn finish_video_decoder_fallback(
        &mut self,
        stage: &str,
        requires_replay: bool,
        replay_position: Duration,
        resume_after_replay: bool,
    ) -> Result<()> {
        self.eof = false;
        if !requires_replay {
            return Ok(());
        }
        trace::diagnostic(
            serde_json::json!({
                "event": "video_decoder_recovery_replay",
                "stage": stage,
                "targetSeconds": replay_position.as_secs_f64(),
                "resumeAfterReplay": resume_after_replay,
                "reason": "decoder route changed after demux or decoder EOF",
            })
            .to_string(),
        );
        self.seek_with_playback_intent(replay_position, resume_after_replay)
    }

    pub fn track_selection(&self) -> TrackSelection {
        self.session.track_selection()
    }

    pub fn add_external_subtitle(
        &mut self,
        config: SubtitleTrackConfig,
    ) -> Result<(SubtitleTrackConfig, Option<TimedSubtitleFrame>)> {
        let media_time = self.media_time();
        let (track, clear_frame) = self.session.add_external_subtitle(config, media_time)?;
        self.pending_subtitle = None;
        Ok((
            track,
            clear_frame.map(|frame| TimedSubtitleFrame {
                frame,
                pts: Some(media_time),
                media_time,
                late_by: None,
            }),
        ))
    }

    pub fn remove_subtitle_track(&mut self, track_id: i64) -> Result<Option<TimedSubtitleFrame>> {
        let media_time = self.media_time();
        let Some(frame) = self.session.remove_subtitle_track(track_id, media_time)? else {
            return Ok(None);
        };
        self.pending_subtitle = None;
        Ok(Some(TimedSubtitleFrame {
            frame,
            pts: Some(media_time),
            media_time,
            late_by: None,
        }))
    }

    pub fn select_audio_track(&mut self, track_id: Option<i64>) -> Result<()> {
        self.select_audio_track_with_now(track_id, Instant::now)
    }

    #[cfg(test)]
    fn select_audio_track_at(&mut self, track_id: Option<i64>, now: Instant) -> Result<()> {
        self.select_audio_track_with_now(track_id, || now)
    }

    fn select_audio_track_with_now(
        &mut self,
        track_id: Option<i64>,
        mut now: impl FnMut() -> Instant,
    ) -> Result<()> {
        if self.session.info.selected_audio_track == track_id {
            return Ok(());
        }
        let media_time = self.media_time_at(now());
        self.session.select_audio_track(track_id)?;
        self.reset_streams_at_with_decoder_flush(media_time, false, true, now)?;
        Ok(())
    }

    pub fn select_subtitle_track(
        &mut self,
        track_id: Option<i64>,
    ) -> Result<Option<TimedSubtitleFrame>> {
        self.select_subtitle_track_with_now(track_id, Instant::now)
    }

    fn select_subtitle_track_with_now(
        &mut self,
        track_id: Option<i64>,
        mut now: impl FnMut() -> Instant,
    ) -> Result<Option<TimedSubtitleFrame>> {
        if self.session.info.selected_subtitle_track == track_id {
            return Ok(None);
        }
        let media_time = self.media_time_at(now());
        let frame = self.session.select_subtitle_track(track_id, media_time)?;
        self.reset_streams_at_with_decoder_flush(media_time, true, false, now)?;
        Ok(frame.map(|frame| TimedSubtitleFrame {
            frame,
            pts: Some(media_time),
            media_time,
            late_by: None,
        }))
    }

    fn reset_streams_at_with_decoder_flush(
        &mut self,
        media_time: Duration,
        flush_audio_decoder: bool,
        flush_subtitle_decoder: bool,
        now: impl FnOnce() -> Instant,
    ) -> Result<()> {
        self.discard_pending_frames_for_seek();
        self.session.seek_with_decoder_flush(
            media_time,
            flush_audio_decoder,
            flush_subtitle_decoder,
        )?;
        let now = now();
        let before = self.clock.media_time_at(now);
        // Park the clock on the seek target until the first post-seek frame is
        // actually presented. Letting it run through preroll makes the reported
        // position drift forward over frames nobody has seen yet, which the
        // first-frame `sync_to` then has to pull back — a visible backward step
        // for every clock consumer. `tick_with_now` starts it on that frame.
        self.clock.reset(media_time, false, now);
        self.reset_audio_clock_tracking(media_time);
        trace_clock_reset("reset_streams_at", before, media_time, self.state);
        self.last_presented_pts = None;
        self.eof = false;
        self.buffering = false;
        self.buffering_video_pts = None;
        self.last_buffering_video_drain_at = None;
        self.buffering_video_drained_frames = 0;
        self.waiting_for_first_frame = self.state == PlaybackRunState::Playing;
        self.paused_seek_frame_pending = self.state == PlaybackRunState::Paused;
        self.video_seek_floor = Some(media_time);
        self.audio_seek_floor = Some(media_time);
        self.reset_video_seek_preroll_budget(now);
        Ok(())
    }

    pub fn state(&self) -> PlaybackRunState {
        self.state
    }

    pub(crate) fn is_buffering(&self) -> bool {
        self.buffering
    }

    pub(crate) fn is_waiting_for_first_frame(&self) -> bool {
        self.waiting_for_first_frame
    }

    pub(crate) fn buffering_video_pts(&self) -> Option<Duration> {
        self.buffering_video_pts
    }

    pub(crate) fn has_audio_output(&self) -> bool {
        self.audio_output_active
            && self.info().selected_audio_track.is_some()
            && !self.session.buffering_audio_recovery_suspended
    }

    pub(crate) fn buffering_audio_is_exhausted(&self) -> bool {
        self.buffering
            && (self.session.buffering_audio_recovery_suspended
                || (self.session.audio_decoder_is_eof()
                    && self.pending_audio.is_none()
                    && !self.session.has_queued_audio_frames()))
    }

    pub(crate) fn has_video_output(&self) -> bool {
        self.info().selected_video_track.is_some()
    }

    pub(crate) fn begin_buffering_at(&mut self, now: Instant) -> bool {
        if self.state != PlaybackRunState::Playing || self.buffering {
            return false;
        }
        let media_time = self.clock.media_time_at(now);
        self.clock.pause(now);
        self.session.begin_buffering_audio_video_scan(media_time);
        self.last_audio_clock_sample = None;
        self.buffering = true;
        self.buffering_video_pts = None;
        self.last_buffering_video_drain_at = None;
        self.buffering_video_drained_frames = 0;
        trace::log(format!(
            "[erika-clock-trace] stage=buffering_begin media={}",
            trace::duration_label(Some(media_time)),
        ));
        true
    }

    pub(crate) fn resume_buffering_at(
        &mut self,
        reference_time: Duration,
        source: PlaybackClockSource,
        now: Instant,
    ) -> bool {
        if self.state != PlaybackRunState::Playing || !self.buffering {
            return false;
        }
        let before = self.clock.media_time_at(now);
        self.clock.sync_to(reference_time, now, source);
        self.clock.play(now);
        self.session.finish_buffering_audio_video_scan();
        self.buffering = false;
        self.buffering_video_pts = None;
        self.last_buffering_video_drain_at = None;
        self.buffering_video_drained_frames = 0;
        trace::log(format!(
            "[erika-clock-trace] stage=buffering_resume before={} reference={} after={} source={:?}",
            trace::duration_label(Some(before)),
            trace::duration_label(Some(reference_time)),
            trace::duration_label(Some(self.clock.media_time_at(now))),
            source,
        ));
        true
    }

    pub(crate) fn should_prefill_audio(&self) -> bool {
        self.state == PlaybackRunState::Playing
    }

    pub(crate) fn has_pending_paused_seek_frame(&self) -> bool {
        self.state == PlaybackRunState::Paused && self.paused_seek_frame_pending
    }

    pub(crate) fn set_audio_output_active(&mut self, active: bool) {
        if self.audio_output_active == active {
            return;
        }
        let pending_audio = if active {
            0
        } else {
            usize::from(self.pending_audio.take().is_some())
        };
        let queued_audio = self.session.set_audio_output_active(active);
        self.audio_output_active = active;
        self.last_audio_output_end = None;
        self.last_audio_clock_sample = None;
        self.reset_audio_eof_stall_state();
        trace::diagnostic(
            serde_json::json!({
                "event": "playback_audio_output",
                "stage": if active { "attached" } else { "detached" },
                "discardedPendingAudioFrames": pending_audio,
                "discardedQueuedAudioFrames": queued_audio,
            })
            .to_string(),
        );
    }

    pub(crate) fn rebase_progress_watchdogs(&mut self) {
        self.session.rebase_decoder_progress_watchdogs();
        if self.audio_eof_stall_started_at.is_some() {
            self.audio_eof_stall_started_at = Some(Instant::now());
        }
    }

    pub fn play(&mut self) -> Result<()> {
        self.play_checked().map(|_| ())
    }

    pub(crate) fn play_checked(&mut self) -> Result<bool> {
        self.play_checked_with_now(Instant::now)
    }

    #[cfg(test)]
    fn play_at(&mut self, now: Instant) {
        self.play_checked_at(now).expect("play fixture");
    }

    #[cfg(test)]
    fn play_checked_at(&mut self, now: Instant) -> Result<bool> {
        self.play_checked_with_now(|| now)
    }

    fn play_checked_with_now(&mut self, now: impl FnOnce() -> Instant) -> Result<bool> {
        self.session.ensure_video_decoder_available("play")?;
        if self.state == PlaybackRunState::Playing {
            return Ok(false);
        }
        let state_before = self.state;
        let rewound_from_ended = playback_state_requires_rewind(state_before);
        if rewound_from_ended {
            trace::diagnostic(
                serde_json::json!({
                    "event": "playback_restart",
                    "stage": "begin",
                    "fromState": format!("{state_before:?}"),
                    "targetSeconds": 0.0,
                })
                .to_string(),
            );
        }
        let now = if rewound_from_ended {
            // Drop engine-owned MediaCodec/AImage state before the session
            // reopens its decoder route for replay.
            self.seek_with_playback_intent_with_now(Duration::ZERO, false, now)?
        } else {
            now()
        };
        self.start_playback_at(now);
        if rewound_from_ended {
            trace::diagnostic(
                serde_json::json!({
                    "event": "playback_restart",
                    "stage": "ready",
                    "fromState": format!("{state_before:?}"),
                    "targetSeconds": 0.0,
                    "state": "Playing",
                })
                .to_string(),
            );
        }
        Ok(rewound_from_ended)
    }

    fn start_playback_at(&mut self, now: Instant) {
        let before = self.clock.media_time_at(now);
        let waiting_for_first_frame = self.last_presented_pts.is_none();
        if !waiting_for_first_frame {
            self.clock.play(now);
        }
        trace::log(format!(
            "[erika-clock-trace] stage=engine_play before={} after={} state_before={:?} waiting_for_first_frame={}",
            trace::duration_label(Some(before)),
            trace::duration_label(Some(self.clock.media_time_at(now))),
            self.state,
            waiting_for_first_frame,
        ));
        self.state = PlaybackRunState::Playing;
        self.buffering = false;
        self.buffering_video_pts = None;
        self.last_buffering_video_drain_at = None;
        self.buffering_video_drained_frames = 0;
        self.waiting_for_first_frame = waiting_for_first_frame;
        self.paused_seek_frame_pending = false;
        self.rebase_progress_watchdogs();
    }

    pub fn pause(&mut self) {
        self.pause_with_now(Instant::now);
    }

    #[cfg(test)]
    fn pause_at(&mut self, now: Instant) {
        self.pause_with_now(|| now);
    }

    fn pause_with_now(&mut self, now: impl FnOnce() -> Instant) {
        if self.state != PlaybackRunState::Playing {
            return;
        }
        // Some hosts implement paused seeking by briefly resuming, issuing the
        // seek, and pausing again before the target frame is decoded. Preserve
        // that in-flight seek as a one-frame paused preview instead of letting
        // the pause gate stop video pumping with no updated frame.
        let seek_frame_pending = self.waiting_for_first_frame && self.video_seek_floor.is_some();
        let now = now();
        let before = self.clock.media_time_at(now);
        // The clock is already parked on the seek target while a post-seek
        // frame is pending (see `reset_streams_at`), so pausing never has to
        // rewind it back to the floor here.
        self.clock.pause(now);
        self.last_audio_clock_sample = None;
        trace::log(format!(
            "[erika-clock-trace] stage=engine_pause before={} after={}",
            trace::duration_label(Some(before)),
            trace::duration_label(Some(self.clock.media_time_at(now))),
        ));
        self.state = PlaybackRunState::Paused;
        self.buffering = false;
        self.buffering_video_pts = None;
        self.last_buffering_video_drain_at = None;
        self.buffering_video_drained_frames = 0;
        self.waiting_for_first_frame = false;
        self.paused_seek_frame_pending = seek_frame_pending;
    }

    pub fn stop(&mut self) {
        let _ = self.stop_checked();
    }

    pub(crate) fn stop_checked(&mut self) -> Result<()> {
        self.stop_checked_with_now(Instant::now)
    }

    #[cfg(test)]
    fn stop_checked_at(&mut self, now: Instant) -> Result<()> {
        self.stop_checked_with_now(|| now)
    }

    fn stop_checked_with_now(&mut self, now: impl FnOnce() -> Instant) -> Result<()> {
        self.discard_pending_frames_for_seek();
        // Stop must remain available after a final decoder failure so callers
        // can quiesce playback and release audio even though replay is blocked.
        // The stop-specific session seek resets demux/audio/subtitle state but
        // deliberately preserves the recorded video-decoder failure.
        self.session.seek_for_stop(Duration::ZERO)?;
        self.commit_stopped_at(now(), "stop");
        Ok(())
    }

    fn commit_stopped_at(&mut self, now: Instant, stage: &'static str) {
        let before = self.clock.media_time_at(now);
        self.clock.reset(Duration::ZERO, false, now);
        self.reset_audio_clock_tracking(Duration::ZERO);
        trace_clock_reset(stage, before, Duration::ZERO, self.state);
        self.pending_frame = None;
        self.pending_audio = None;
        self.pending_subtitle = None;
        self.last_presented_pts = None;
        self.state = PlaybackRunState::Stopped;
        self.eof = false;
        self.waiting_for_first_frame = false;
        self.buffering = false;
        self.buffering_video_pts = None;
        self.last_buffering_video_drain_at = None;
        self.buffering_video_drained_frames = 0;
        self.paused_seek_frame_pending = false;
        self.video_seek_floor = Some(Duration::ZERO);
        self.audio_seek_floor = Some(Duration::ZERO);
        self.reset_audio_eof_stall_state();
        self.reset_video_seek_preroll_budget(now);
    }

    pub fn seek(&mut self, position: Duration) -> Result<()> {
        self.seek_with_playback_intent_with_now(
            position,
            self.state == PlaybackRunState::Playing,
            Instant::now,
        )
        .map(|_| ())
    }

    #[cfg(test)]
    fn seek_at(&mut self, position: Duration, now: Instant) -> Result<()> {
        self.seek_with_playback_intent_with_now(
            position,
            self.state == PlaybackRunState::Playing,
            || now,
        )
        .map(|_| ())
    }

    pub fn seek_with_playback_intent(
        &mut self,
        position: Duration,
        resume_after_seek: bool,
    ) -> Result<()> {
        self.seek_with_playback_intent_with_now(position, resume_after_seek, Instant::now)
            .map(|_| ())
    }

    fn seek_with_playback_intent_with_now(
        &mut self,
        position: Duration,
        resume_after_seek: bool,
        now: impl FnOnce() -> Instant,
    ) -> Result<Instant> {
        self.discard_pending_frames_for_seek();
        self.session.seek(position)?;
        let now = now();
        let state_before = self.state;
        let state_after = playback_state_after_seek_intent(state_before, resume_after_seek);
        let before = self.clock.media_time_at(now);
        // Park on the seek target; the first presented frame starts the clock
        // (see `reset_streams_at`). Running it through preroll would report a
        // position no frame has reached yet and then step backward.
        self.clock.reset(position, false, now);
        self.reset_audio_clock_tracking(position);
        trace_clock_reset("seek", before, position, state_after);
        self.last_presented_pts = None;
        self.eof = false;
        self.state = state_after;
        self.buffering = false;
        self.buffering_video_pts = None;
        self.last_buffering_video_drain_at = None;
        self.buffering_video_drained_frames = 0;
        self.waiting_for_first_frame = state_after == PlaybackRunState::Playing;
        self.paused_seek_frame_pending = state_after == PlaybackRunState::Paused;
        self.video_seek_floor = Some(position);
        self.audio_seek_floor = Some(position);
        self.reset_video_seek_preroll_budget(now);
        if state_before != state_after {
            trace::log(format!(
                "[erika-playback-trace] stage=seek_rearm from={state_before:?} to={state_after:?} target={}",
                trace::duration_label(Some(position)),
            ));
        }
        Ok(now)
    }

    fn discard_pending_frames_for_seek(&mut self) {
        // Drop the engine-owned frames before PlaybackSession touches a
        // decoder. In particular, a MediaCodec Surface frame retains the old
        // AImageReader/codec route and must not survive decoder replacement.
        self.pending_frame = None;
        self.pending_audio = None;
        self.pending_subtitle = None;
        self.last_audio_output_end = None;
        self.reset_audio_eof_stall_state();
    }

    fn reset_video_seek_preroll_budget(&mut self, now: Instant) {
        self.video_seek_preroll_started_at = Some(now);
        self.video_seek_preroll_dropped_frames = 0;
        self.last_video_seek_preroll_log = None;
    }

    pub fn media_time(&self) -> Duration {
        self.media_time_at(Instant::now())
    }

    /// The engine's clock itself, for publishing to readers that evaluate it
    /// on their own schedule rather than at the worker's polling rate.
    pub fn clock(&self) -> PlaybackClock {
        self.clock.clone()
    }

    fn media_time_at(&self, now: Instant) -> Duration {
        self.clock.media_time_at(now)
    }

    pub fn clock_snapshot(&self) -> PlaybackClockSnapshot {
        self.clock_snapshot_at(Instant::now())
    }

    fn clock_snapshot_at(&self, now: Instant) -> PlaybackClockSnapshot {
        self.clock.snapshot_at(now)
    }

    pub fn timing_config(&self) -> PlaybackTimingConfig {
        self.timing
    }

    pub(crate) fn buffer_recovery_audio(&self) -> Duration {
        self.buffer_recovery_audio
    }

    pub fn set_timing_config(&mut self, timing: PlaybackTimingConfig) {
        self.timing = timing;
    }

    pub fn set_playback_rate(&mut self, rate: f64) {
        self.set_playback_rate_at(rate, Instant::now());
    }

    fn set_playback_rate_at(&mut self, rate: f64, now: Instant) {
        let before = self.clock.media_time_at(now);
        let before_rate = self.clock.rate();
        self.clock.set_rate(rate, now);
        self.last_audio_clock_sample = None;
        trace::log(format!(
            "[erika-clock-trace] stage=engine_set_rate before={} after={} rate_before={:.3} rate_after={:.3}",
            trace::duration_label(Some(before)),
            trace::duration_label(Some(self.clock.media_time_at(now))),
            before_rate,
            self.clock.rate(),
        ));
    }

    pub fn sync_to_audio_clock(&mut self, snapshot: AudioClockSnapshot) -> Option<ClockCorrection> {
        self.sync_to_audio_clock_with_now(snapshot, Instant::now)
    }

    #[cfg(test)]
    fn sync_to_audio_clock_at(
        &mut self,
        snapshot: AudioClockSnapshot,
        now: Instant,
    ) -> Option<ClockCorrection> {
        self.sync_to_audio_clock_with_now(snapshot, || now)
    }

    fn sync_to_audio_clock_with_now(
        &mut self,
        snapshot: AudioClockSnapshot,
        now: impl FnOnce() -> Instant,
    ) -> Option<ClockCorrection> {
        let now = now();
        self.discipline_audio_observation(snapshot, now, now)
    }

    pub(crate) fn sync_to_audio_clock_observed(
        &mut self,
        snapshot: AudioClockSnapshot,
        captured_at: Instant,
        output_epoch: u64,
    ) -> Option<ClockCorrection> {
        if self.audio_clock_output_epoch != Some(output_epoch) {
            self.last_audio_clock_sample = None;
            self.audio_clock_output_epoch = Some(output_epoch);
        }
        self.discipline_audio_observation(snapshot, captured_at, Instant::now())
    }

    fn reset_audio_clock_tracking(&mut self, floor: Duration) {
        self.last_audio_clock_sample = None;
        self.audio_clock_output_epoch = None;
        self.audio_clock_floor = floor;
    }

    fn discipline_audio_observation(
        &mut self,
        snapshot: AudioClockSnapshot,
        captured_at: Instant,
        now: Instant,
    ) -> Option<ClockCorrection> {
        if self.state != PlaybackRunState::Playing || self.buffering || !self.clock.is_running() {
            return None;
        }
        let elapsed = now.checked_duration_since(captured_at)?;
        // AudioRingBuffer keeps media-time segments rate-aware, including the
        // SoundTouch output ratio. Once a rate transition is committed by the
        // presenter, the audio clock is therefore valid at every playback
        // rate and must remain available to correct the video clock.
        let media_time = snapshot.media_time?;
        if media_time < self.audio_clock_floor
            || self
                .last_audio_clock_sample
                .is_some_and(|(_, last_at)| captured_at <= last_at)
        {
            trace::log(format!(
                "[erika-clock-trace] stage=output_audio_clock_skip reason=before_floor_or_out_of_order media={} floor={}",
                trace::duration_label(Some(media_time)),
                trace::duration_label(Some(self.audio_clock_floor)),
            ));
            return None;
        }
        // Prime each output epoch before trusting it. Read counters can survive
        // a clear, so a nonzero first counter alone does not prove playback.
        // Silence callbacks advance underflow, but not the media timeline.
        let Some((previous, last_captured_at)) = self.last_audio_clock_sample.as_mut() else {
            self.last_audio_clock_sample = Some((snapshot, captured_at));
            return None;
        };
        // Preserve capture ordering even when this observation cannot establish
        // progress. Only the timestamp watermark advances on rejection.
        *last_captured_at = captured_at;
        if snapshot.read_frames <= previous.read_frames
            || previous
                .media_time
                .is_none_or(|previous| media_time <= previous)
        {
            return None;
        }
        // Rejected observations must not lower the progress baseline. A counter
        // or timeline reset establishes a new baseline through the output epoch.
        *previous = snapshot;
        // Account for delivery latency only within PCM known to be queued at
        // capture. Never extrapolate silence past the last media sample.
        let queued_elapsed = elapsed.min(snapshot.queued_duration.unwrap_or_default());
        let reference =
            media_time.saturating_add(scale_duration(queued_elapsed, self.clock.rate()));
        let before = self.clock.media_time_at(now);
        let correction = self.clock.discipline_to(
            reference,
            now,
            PlaybackClockSource::Audio,
            self.timing.audio_sync,
        );
        let after = self.clock.media_time_at(now);
        trace_clock_correction(
            "output_audio_clock",
            before,
            reference,
            after,
            correction,
            Some(snapshot),
        );
        Some(correction)
    }

    pub fn next_audio_frame(&mut self) -> Result<Option<PcmAudioFrame>> {
        self.session.next_audio_frame()
    }

    pub fn tick_audio(&mut self) -> Result<Option<TimedAudioFrame>> {
        self.tick_audio_with_now(Instant::now)
    }

    #[cfg(test)]
    fn tick_audio_at(&mut self, now: Instant) -> Result<Option<TimedAudioFrame>> {
        self.tick_audio_with_now(|| now)
    }

    fn tick_audio_with_now(
        &mut self,
        now: impl FnOnce() -> Instant,
    ) -> Result<Option<TimedAudioFrame>> {
        if self.state != PlaybackRunState::Playing {
            return Ok(None);
        }
        self.ensure_pending_audio()?;
        let Some(frame) = self.pending_audio.as_ref() else {
            return Ok(None);
        };

        let pts = frame.pts;
        let now = now();
        let media_time = self.clock.media_time_at(now);
        if pts.is_some_and(|pts| pts > media_time + self.timing.audio_lead_time) {
            if self.buffering || self.session.buffering_audio_recovery_suspended {
                self.session.suspend_buffering_audio_recovery(
                    "future_audio_frame",
                    format!(
                        "next audio frame at {:.3}s is ahead of the {:.3}s playback clock lead window",
                        pts.map_or(0.0, |value| value.as_secs_f64()),
                        media_time.as_secs_f64(),
                    ),
                );
            }
            return Ok(None);
        }

        let frame = self.pending_audio.take().expect("pending audio exists");
        self.session.resume_buffering_audio_recovery();
        let late_by = pts.and_then(|pts| media_time.checked_sub(pts));
        Ok(Some(TimedAudioFrame {
            frame,
            pts,
            media_time,
            late_by,
        }))
    }

    pub fn tick_audio_bounded(
        &mut self,
        max_packets: usize,
        max_decode_duration: Duration,
    ) -> Result<Option<TimedAudioFrame>> {
        if self.state != PlaybackRunState::Playing {
            return Ok(None);
        }
        self.ensure_pending_audio_bounded(max_packets, max_decode_duration)?;
        let Some(frame) = self.pending_audio.as_ref() else {
            return Ok(None);
        };

        let pts = frame.pts;
        let now = Instant::now();
        let media_time = self.clock.media_time_at(now);
        if pts.is_some_and(|pts| pts > media_time + self.timing.audio_lead_time) {
            if self.buffering || self.session.buffering_audio_recovery_suspended {
                self.session.suspend_buffering_audio_recovery(
                    "future_audio_frame",
                    format!(
                        "next audio frame at {:.3}s is ahead of the {:.3}s playback clock lead window",
                        pts.map_or(0.0, |value| value.as_secs_f64()),
                        media_time.as_secs_f64(),
                    ),
                );
            }
            return Ok(None);
        }

        let frame = self.pending_audio.take().expect("pending audio exists");
        self.session.resume_buffering_audio_recovery();
        let late_by = pts.and_then(|pts| media_time.checked_sub(pts));
        Ok(Some(TimedAudioFrame {
            frame,
            pts,
            media_time,
            late_by,
        }))
    }

    pub(crate) fn restore_pending_audio_frame(&mut self, frame: PcmAudioFrame) {
        debug_assert!(self.pending_audio.is_none());
        self.pending_audio = Some(frame);
    }

    /// Records PCM only after a successful handoff to the audio consumer.
    pub(crate) fn record_audio_output_end(&mut self, end: Option<Duration>) {
        self.last_audio_output_end = end;
    }

    pub fn tick_subtitle(&mut self) -> Result<Option<TimedSubtitleFrame>> {
        if self.state != PlaybackRunState::Playing {
            return Ok(None);
        }
        let media_time = self.media_time();
        self.ensure_pending_subtitle(media_time)?;
        let Some(frame) = self.pending_subtitle.as_ref() else {
            return Ok(None);
        };

        let pts = frame.start;
        if pts.is_some_and(|pts| pts > media_time) {
            return Ok(None);
        }

        let frame = self
            .pending_subtitle
            .take()
            .expect("pending subtitle exists");
        let late_by = pts.and_then(|pts| media_time.checked_sub(pts));
        Ok(Some(TimedSubtitleFrame {
            frame,
            pts,
            media_time,
            late_by,
        }))
    }

    pub fn tick(&mut self) -> Result<Option<TimedVideoFrame>> {
        self.tick_with_now(Instant::now)
    }

    #[cfg(test)]
    fn tick_at(&mut self, now: Instant) -> Result<Option<TimedVideoFrame>> {
        self.tick_with_now(|| now)
    }

    fn tick_with_now(
        &mut self,
        mut now: impl FnMut() -> Instant,
    ) -> Result<Option<TimedVideoFrame>> {
        let paused_seek_preview =
            self.state == PlaybackRunState::Paused && self.paused_seek_frame_pending;
        if paused_seek_preview && self.session.video_decode_suspended {
            // Background audio-only playback suspends video decoding, and a
            // suspended decoder can never produce the preview frame: pumping
            // for it would discard every remaining video packet at the 2 ms
            // paused rate -- a full file scan for an HTTP source. Give the
            // preview up; a foreground resume that wants the frame seeks again.
            self.paused_seek_frame_pending = false;
            trace::log(format!(
                "[erika-playback-trace] stage=paused_seek_frame_abandoned reason=video_decode_suspended target={}",
                trace::duration_label(self.video_seek_floor),
            ));
            return Ok(None);
        }
        if self.state != PlaybackRunState::Playing && !paused_seek_preview {
            return Ok(None);
        }
        if self.buffering && self.buffering_video_pts.is_some() {
            // The first buffering frame remains visible in the presenter, but
            // interleaved audio cannot refill unless video demux keeps moving.
            // Pace later drains instead of consuming one frame per 2 ms worker
            // poll. For MediaCodec this still releases output tokens without
            // sending an unprepared Surface frame across the worker boundary.
            let drain_now = now();
            if self.last_buffering_video_drain_at.is_some_and(|last| {
                drain_now.saturating_duration_since(last) < BUFFERING_VIDEO_DRAIN_INTERVAL
            }) {
                return Ok(None);
            }
            self.ensure_pending_frame_with_now(&mut || drain_now)?;
            let Some(frame) = self.pending_frame.as_ref() else {
                return Ok(None);
            };
            let frame_pts = frame.pts().and_then(|pts| pts.as_duration());
            if !self.session.buffering_video_advance_allowed(frame_pts) {
                // Keep this frame parked. Audio demand may continue scanning
                // across a bounded compressed-video queue, but video content
                // must never be discarded without limit when audio is missing
                // or corrupt.
                return Ok(None);
            }
            let _ = self.pending_frame.take();
            self.session.record_buffering_video_advance();
            self.buffering_video_drained_frames =
                self.buffering_video_drained_frames.saturating_add(1);
            self.last_buffering_video_drain_at = Some(drain_now);
            return Ok(None);
        }
        let tick_started = Instant::now();
        let mut consecutive_drops = 0usize;
        let mut seek_preroll_drops = 0usize;
        loop {
            self.ensure_pending_frame_with_now(&mut now)?;
            let Some(frame) = self.pending_frame.as_ref() else {
                // A paused seek past the last frame never produces the preview
                // frame that would clear the flag. Give it up at EOF so the
                // worker can fall back to its idle poll interval instead of
                // spinning at the 2 ms paused-preview rate until playback
                // resumes.
                if paused_seek_preview && self.eof {
                    self.paused_seek_frame_pending = false;
                    trace::log(format!(
                        "[erika-playback-trace] stage=paused_seek_frame_abandoned reason=eof target={}",
                        trace::duration_label(self.video_seek_floor),
                    ));
                }
                return Ok(None);
            };

            let pts = frame.pts().and_then(|pts| pts.as_duration());
            if self.should_drop_video_seek_preroll(pts) {
                let _ = self.pending_frame.take();
                seek_preroll_drops = seek_preroll_drops.saturating_add(1);
                self.video_seek_preroll_dropped_frames =
                    self.video_seek_preroll_dropped_frames.saturating_add(1);
                let elapsed = tick_started.elapsed();
                if seek_preroll_budget_exhausted(
                    seek_preroll_drops,
                    self.timing.video_scheduler.max_consecutive_drops,
                    elapsed,
                ) {
                    self.trace_video_seek_preroll_yield(pts, seek_preroll_drops, elapsed);
                    return Ok(None);
                }
                continue;
            }
            let should_present_first = self.last_presented_pts.is_none();
            if should_present_first && self.waiting_for_first_frame {
                let now = now();
                let before = self.clock.media_time_at(now);
                self.clock.sync_to(
                    pts.unwrap_or(Duration::ZERO),
                    now,
                    PlaybackClockSource::Wall,
                );
                if !self.buffering {
                    self.clock.play(now);
                }
                trace::log(format!(
                    "[erika-clock-trace] stage=first_video_sync pts={} before={} after={} state={:?}",
                    trace::duration_label(pts),
                    trace::duration_label(Some(before)),
                    trace::duration_label(Some(self.clock.media_time_at(now))),
                    self.state,
                ));
                self.waiting_for_first_frame = false;
            }

            if self.buffering {
                #[cfg(target_os = "android")]
                if !self.prepare_pending_mediacodec_frame()? {
                    return Ok(None);
                }
                let frame = self.pending_frame.take().expect("pending frame exists");
                self.last_presented_pts = pts;
                let presented_at = now();
                self.buffering_video_pts = pts.or(Some(self.media_time_at(presented_at)));
                self.last_buffering_video_drain_at = Some(presented_at);
                return Ok(Some(TimedVideoFrame {
                    frame: frame.frame,
                    decode_backend: frame.decode_backend,
                    pts,
                    media_time: self.media_time_at(presented_at),
                    late_by: None,
                }));
            }

            let media_time = self.media_time_at(now());
            match self
                .timing
                .video_scheduler
                .schedule(pts, media_time, should_present_first)
            {
                VideoFrameDecision::Wait { .. } => return Ok(None),
                VideoFrameDecision::Drop { .. }
                    if consecutive_drops < self.timing.video_scheduler.max_consecutive_drops =>
                {
                    let _ = self.pending_frame.take();
                    consecutive_drops += 1;
                }
                decision => {
                    #[cfg(target_os = "android")]
                    if !self.prepare_pending_mediacodec_frame()? {
                        return Ok(None);
                    }
                    let frame = self.pending_frame.take().expect("pending frame exists");
                    self.last_presented_pts = pts;
                    if paused_seek_preview {
                        self.paused_seek_frame_pending = false;
                        trace::log(format!(
                            "[erika-playback-trace] stage=paused_seek_frame pts={} target={}",
                            trace::duration_label(pts),
                            trace::duration_label(Some(media_time)),
                        ));
                    }
                    return Ok(Some(TimedVideoFrame {
                        frame: frame.frame,
                        decode_backend: frame.decode_backend,
                        pts,
                        media_time,
                        late_by: decision.late_by(),
                    }));
                }
            }
        }
    }

    fn should_drop_video_seek_preroll(&mut self, pts: Option<Duration>) -> bool {
        let Some(target) = self.video_seek_floor else {
            return false;
        };
        let Some(pts) = pts else {
            self.video_seek_floor = None;
            self.finish_video_seek_preroll(target, None, "missing_pts");
            return false;
        };
        if pts < target {
            true
        } else {
            self.video_seek_floor = None;
            self.finish_video_seek_preroll(target, Some(pts), "target_reached");
            false
        }
    }

    fn trace_video_seek_preroll_yield(
        &mut self,
        pts: Option<Duration>,
        burst_drops: usize,
        elapsed: Duration,
    ) {
        let now = Instant::now();
        if self
            .last_video_seek_preroll_log
            .is_some_and(|last| now.duration_since(last) < SEEK_PREROLL_LOG_INTERVAL)
        {
            return;
        }
        self.last_video_seek_preroll_log = Some(now);
        trace::diagnostic(
            serde_json::json!({
                "event": "video_seek_preroll",
                "stage": "budget_yield",
                "targetSeconds": self.video_seek_floor.map(|target| target.as_secs_f64()),
                "lastPtsSeconds": pts.map(|pts| pts.as_secs_f64()),
                "burstDroppedFrames": burst_drops,
                "totalDroppedFrames": self.video_seek_preroll_dropped_frames,
                "frameBudget": self.timing.video_scheduler.max_consecutive_drops.max(1),
                "timeBudgetMs": SEEK_PREROLL_DECODE_TIME_BUDGET.as_secs_f64() * 1000.0,
                "burstElapsedMs": elapsed.as_secs_f64() * 1000.0,
            })
            .to_string(),
        );
    }

    fn finish_video_seek_preroll(
        &mut self,
        target: Duration,
        pts: Option<Duration>,
        reason: &'static str,
    ) {
        let dropped_frames = self.video_seek_preroll_dropped_frames;
        let elapsed = self
            .video_seek_preroll_started_at
            .map(|started| started.elapsed())
            .unwrap_or_default();
        trace::diagnostic(
            serde_json::json!({
                "event": "video_seek_preroll",
                "stage": "complete",
                "reason": reason,
                "targetSeconds": target.as_secs_f64(),
                "firstOutputPtsSeconds": pts.map(|pts| pts.as_secs_f64()),
                "totalDroppedFrames": dropped_frames,
                "elapsedMs": elapsed.as_secs_f64() * 1000.0,
            })
            .to_string(),
        );
        self.video_seek_preroll_started_at = None;
        self.video_seek_preroll_dropped_frames = 0;
        self.last_video_seek_preroll_log = None;
    }

    fn ensure_pending_frame_with_now(&mut self, now: &mut impl FnMut() -> Instant) -> Result<()> {
        if self.pending_frame.is_some() || self.eof {
            return Ok(());
        }
        self.pending_frame = match self.session.next_video_frame() {
            Ok(frame) => frame,
            Err(PlaybackError::DecoderInputStall { reason }) => {
                let replay_position = self
                    .video_seek_floor
                    .unwrap_or_else(|| self.media_time_at(now()));
                let resume_after_replay = self.state == PlaybackRunState::Playing;
                if !self.session.recover_from_video_input_stall(&reason)? {
                    return Err(PlaybackError::DecoderInputStall { reason });
                }
                trace::diagnostic(
                    serde_json::json!({
                        "event": "video_decoder_recovery_replay",
                        "stage": "decoder_input_stall",
                        "targetSeconds": replay_position.as_secs_f64(),
                        "resumeAfterReplay": resume_after_replay,
                        "reason": reason.as_str(),
                    })
                    .to_string(),
                );
                self.seek_with_playback_intent_with_now(
                    replay_position,
                    resume_after_replay,
                    || now(),
                )?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if self.pending_frame.is_none() && self.session.is_eof() {
            if !self.audio_output_active {
                let pending_audio = usize::from(self.pending_audio.take().is_some());
                let queued_audio = self.session.discard_queued_audio_frames();
                if pending_audio > 0 || queued_audio > 0 {
                    trace::diagnostic(
                        serde_json::json!({
                            "event": "playback_audio_eof",
                            "stage": "discard_without_output",
                            "pendingEngineAudioFrames": pending_audio,
                            "queuedSessionAudioFrames": queued_audio,
                            "reason": "no active audio frame consumer",
                        })
                        .to_string(),
                    );
                }
            }

            let queued_audio = self.session.queued_audio_frame_count();
            let pending_audio = self.pending_audio.is_some();
            if self.audio_output_active && (pending_audio || queued_audio > 0) {
                self.observe_audio_eof_stall(usize::from(pending_audio) + queued_audio)?;
                return Ok(());
            }
            self.reset_audio_eof_stall_state();

            if !playback_eof_ready(
                self.session.is_eof(),
                self.pending_frame.is_some(),
                self.session.has_queued_video_frames(),
                self.audio_output_active,
                self.pending_audio.is_some(),
                self.session.has_queued_audio_frames(),
            ) {
                return Ok(());
            }
            if self.state == PlaybackRunState::Paused && self.paused_seek_frame_pending {
                // The preview pump ran a paused seek to the end of the media.
                // The user's session is paused, not finished: abandon the
                // preview instead of flipping a paused session into Ended,
                // which would publish a spurious end-of-media and turn the
                // next play() into a rewind-from-ended full reopen.
                self.paused_seek_frame_pending = false;
                trace::log(format!(
                    "[erika-playback-trace] stage=paused_seek_frame_abandoned reason=eof target={}",
                    trace::duration_label(self.video_seek_floor),
                ));
                return Ok(());
            }
            self.eof = true;
            self.state = PlaybackRunState::Ended;
            self.buffering = false;
            self.buffering_video_pts = None;
            self.last_buffering_video_drain_at = None;
            self.buffering_video_drained_frames = 0;
            let eof_now = now();
            let media_time = self
                .info()
                .duration
                .unwrap_or_else(|| self.media_time_at(now()));
            let before = self.clock.media_time_at(eof_now);
            self.clock.reset(media_time, false, eof_now);
            trace_clock_reset("eof", before, media_time, self.state);
        }
        Ok(())
    }

    fn observe_audio_eof_stall(&mut self, pending_frames: usize) -> Result<()> {
        let now = Instant::now();
        if self.audio_eof_stall_started_at.is_none()
            || pending_frames < self.audio_eof_stall_pending_frames
        {
            self.audio_eof_stall_started_at = Some(now);
        }
        self.audio_eof_stall_pending_frames = pending_frames;
        if !self.audio_eof_stall_logged {
            self.audio_eof_stall_logged = true;
            trace::diagnostic(
                serde_json::json!({
                    "event": "playback_audio_eof",
                    "stage": "pending_output",
                    "pendingAudioFrames": pending_frames,
                    "timeoutSeconds": AUDIO_EOF_OUTPUT_STALL_TIMEOUT.as_secs_f64(),
                    "retryOwner": "playback_audio_pump",
                })
                .to_string(),
            );
        }
        let stalled_for = now.saturating_duration_since(
            self.audio_eof_stall_started_at
                .expect("audio EOF stall timestamp initialized"),
        );
        if stalled_for < AUDIO_EOF_OUTPUT_STALL_TIMEOUT {
            return Ok(());
        }

        let reason = format!(
            "{} decoded audio frame(s) remained blocked for {:.3}s after video and decoder EOF",
            pending_frames,
            stalled_for.as_secs_f64(),
        );
        trace::diagnostic(
            serde_json::json!({
                "event": "playback_audio_eof",
                "stage": "output_timeout",
                "pendingAudioFrames": pending_frames,
                "stalledSeconds": stalled_for.as_secs_f64(),
                "reason": reason.as_str(),
            })
            .to_string(),
        );
        Err(PlaybackError::AudioOutputStall { reason })
    }

    fn reset_audio_eof_stall_state(&mut self) {
        self.audio_eof_stall_started_at = None;
        self.audio_eof_stall_pending_frames = 0;
        self.audio_eof_stall_logged = false;
    }

    fn ensure_pending_audio(&mut self) -> Result<()> {
        if self.pending_audio.is_some() || self.eof {
            return Ok(());
        }
        while let Some(mut frame) = self.session.next_audio_frame()? {
            if keep_audio_frame_after_seek_floor(&mut self.audio_seek_floor, &mut frame) {
                self.pending_audio = Some(frame);
                break;
            }
        }
        Ok(())
    }

    fn ensure_pending_audio_bounded(
        &mut self,
        max_packets: usize,
        max_decode_duration: Duration,
    ) -> Result<()> {
        if self.pending_audio.is_some() || self.eof {
            return Ok(());
        }
        let audio_seek_floor = &mut self.audio_seek_floor;
        let demand = if self.buffering {
            PlaybackPumpDemand::BufferingAudio
        } else {
            PlaybackPumpDemand::Audio
        };
        self.pending_audio = self.session.next_audio_frame_bounded_where(
            max_packets,
            max_decode_duration,
            demand,
            |frame| keep_audio_frame_after_seek_floor(audio_seek_floor, frame),
        )?;
        Ok(())
    }

    fn ensure_pending_subtitle(&mut self, media_time: Duration) -> Result<()> {
        if self.pending_subtitle.is_some() || self.eof {
            return Ok(());
        }
        self.pending_subtitle = self.session.next_subtitle_frame(media_time)?;
        Ok(())
    }
}

fn playback_eof_ready(
    session_eof: bool,
    pending_video: bool,
    queued_video: bool,
    audio_output_active: bool,
    pending_audio: bool,
    queued_audio: bool,
) -> bool {
    session_eof
        && !pending_video
        && !queued_video
        && (!audio_output_active || (!pending_audio && !queued_audio))
}

fn playback_state_requires_rewind(state: PlaybackRunState) -> bool {
    state == PlaybackRunState::Ended
}

fn video_decoder_unavailability_reason(
    selected_video_track: Option<i64>,
    decoder_available: bool,
    recorded_reason: Option<&str>,
) -> Option<String> {
    let track_id = selected_video_track.filter(|_| !decoder_available)?;
    Some(recorded_reason.map(str::to_owned).unwrap_or_else(|| {
        format!(
            "selected video track {track_id} has no active decoder; reopen the media before playback"
        )
    }))
}

#[cfg(test)]
fn playback_state_after_seek(state: PlaybackRunState) -> PlaybackRunState {
    playback_state_after_seek_intent(state, state == PlaybackRunState::Playing)
}

fn playback_state_after_seek_intent(
    state_before: PlaybackRunState,
    resume_after_seek: bool,
) -> PlaybackRunState {
    if resume_after_seek {
        PlaybackRunState::Playing
    } else if matches!(
        state_before,
        PlaybackRunState::Stopped | PlaybackRunState::Ended
    ) {
        PlaybackRunState::Stopped
    } else {
        PlaybackRunState::Paused
    }
}

fn seek_preroll_budget_exhausted(
    dropped_frames: usize,
    configured_frame_budget: usize,
    elapsed: Duration,
) -> bool {
    dropped_frames >= configured_frame_budget.max(1) || elapsed >= SEEK_PREROLL_DECODE_TIME_BUDGET
}

fn playback_timing_for_request(
    request: &MediaRequest,
    mut timing: PlaybackTimingConfig,
) -> PlaybackTimingConfig {
    if request_uses_http_source(request) {
        if timing.audio_lead_time == DEFAULT_AUDIO_LEAD_TIME {
            timing.audio_lead_time = STREAMING_AUDIO_LEAD_TIME;
        }
    }
    timing
}

fn buffer_recovery_audio_for_request(request: &MediaRequest) -> Duration {
    if request_uses_http_source(request) {
        STREAMING_BUFFER_RECOVERY_AUDIO
    } else {
        DEFAULT_BUFFER_RECOVERY_AUDIO
    }
}

fn request_uses_http_source(request: &MediaRequest) -> bool {
    match request.source_hint {
        MediaSourceHint::Http => true,
        MediaSourceHint::Auto => is_http_uri(&request.uri),
        MediaSourceHint::LocalFile => false,
    }
}

fn is_http_uri(uri: &str) -> bool {
    uri.starts_with("http://") || uri.starts_with("https://")
}

fn trace_clock_reset(
    stage: &'static str,
    before: Duration,
    target: Duration,
    state: PlaybackRunState,
) {
    trace::log(format!(
        "[erika-clock-trace] stage=clock_reset:{stage} before={} target={} delta={:.3} state={:?} back={}",
        trace::duration_label(Some(before)),
        trace::duration_label(Some(target)),
        trace::duration_diff(before, target).as_secs_f64(),
        state,
        trace::duration_regressed(target, before),
    ));
}

fn trace_clock_correction(
    stage: &'static str,
    before: Duration,
    reference: Duration,
    after: Duration,
    correction: ClockCorrection,
    snapshot: Option<AudioClockSnapshot>,
) {
    let should_log = correction.direction != ClockCorrectionDirection::None
        || correction.snapped
        || trace::duration_regressed(after, before)
        || trace::duration_diff(before, reference) > Duration::from_millis(50);
    if !should_log {
        return;
    }
    let snapshot_suffix = snapshot.map_or_else(String::new, |snapshot| {
        format!(
            " queued={} queued_frames={} read={} written={} underflow={}",
            trace::duration_label(snapshot.queued_duration),
            snapshot.queued_frames,
            snapshot.read_frames,
            snapshot.written_frames,
            snapshot.underflow_frames,
        )
    });
    trace::log(format!(
        "[erika-clock-trace] stage=clock_discipline:{stage} before={} reference={} after={} drift={} applied={} direction={:?} snapped={} back={}{}",
        trace::duration_label(Some(before)),
        trace::duration_label(Some(reference)),
        trace::duration_label(Some(after)),
        trace::duration_label(Some(correction.drift)),
        trace::duration_label(Some(correction.applied)),
        correction.direction,
        correction.snapped,
        trace::duration_regressed(after, before),
        snapshot_suffix,
    ));
}

fn drain_video_frames(
    decoder: &mut Decoder,
    frames: &mut VecDeque<DecodedVideoFrame>,
    dolby_vision_profile: Option<u8>,
    dovi_rpu_diagnostics: &mut DoviRpuDiagnostics,
) -> Result<DecoderDrainStatus> {
    let decode_backend = decoder.backend();
    let mut status = DecoderDrainStatus::default();
    loop {
        match decoder.receive_frame()? {
            DecoderOutputFrame::Frame(frame) => {
                diagnose_dovi_frame(
                    dolby_vision_profile,
                    dovi_rpu_diagnostics,
                    &frame,
                    decode_backend,
                );
                frames.push_back(DecodedVideoFrame {
                    frame,
                    decode_backend,
                });
                status.frames = status.frames.saturating_add(1);
            }
            DecoderOutputFrame::NeedMoreInput => return Ok(status),
            DecoderOutputFrame::EndOfStream => {
                status.end_of_stream = true;
                return Ok(status);
            }
        }
    }
}

/// Per-stream accounting for decoded frames whose Dolby Vision RPU cannot
/// feed the mapping path, and for frames whose RPU asks for an enhancement
/// layer this renderer never decodes.
#[derive(Debug, Default)]
struct DoviRpuDiagnostics {
    unavailable_frames: u64,
    el_frames: u64,
}

/// Re-report interval after the first diagnostic, so a broken stream stays
/// visible over long playback without logging at frame rate.
const DOVI_RPU_DIAGNOSTIC_INTERVAL: u64 = 1024;

impl DoviRpuDiagnostics {
    fn record_unavailable_frame(&mut self) -> u64 {
        self.unavailable_frames = self.unavailable_frames.saturating_add(1);
        self.unavailable_frames
    }

    fn record_el_frame(&mut self) -> u64 {
        self.el_frames = self.el_frames.saturating_add(1);
        self.el_frames
    }

    fn should_report(count: u64) -> bool {
        count == 1 || count % DOVI_RPU_DIAGNOSTIC_INTERVAL == 0
    }
}

/// Decides whether a frame lacking usable RPU metadata warrants a diagnostic.
/// A missing RPU is only fatal on Profile 5, whose base layer is not
/// HDR10-compatible; other profiles either repeat RPUs across frames or fall
/// back to a base layer that displays correctly. A *present but rejected*
/// RPU always warrants one: the stream intended to carry Dolby Vision mapping
/// and the frame will silently display unmapped.
fn dovi_rpu_diagnostic_warranted(profile: Option<u8>, reason: DoviRejectReason) -> bool {
    reason != DoviRejectReason::MissingSideData || profile == Some(5)
}

fn diagnose_dovi_frame(
    profile: Option<u8>,
    diagnostics: &mut DoviRpuDiagnostics,
    frame: &Frame,
    decode_backend: DecoderBackend,
) {
    // One parse: both the reject reason and the EL residual status come from
    // the same `frame_dovi_metadata_result` result.
    let el_status = match frame.dovi_mapping_probe() {
        Ok(status) => status,
        Err(reason) => {
            if !dovi_rpu_diagnostic_warranted(profile, reason) {
                return;
            }
            let count = diagnostics.record_unavailable_frame();
            if !DoviRpuDiagnostics::should_report(count) {
                return;
            }
            trace::diagnostic(
                serde_json::json!({
                    "event": "dovi_rpu_unavailable",
                    "profile": profile,
                    "decodeBackend": decode_backend.as_str(),
                    "reason": reason.label(),
                    "framesAffected": count,
                })
                .to_string(),
            );
            return;
        }
    };
    // The RPU is usable, but it may ask for an enhancement layer this renderer
    // never decodes (Profile 7 FEL/MEL). The base-layer mapping still applies,
    // so report the gap instead of pretending the stream was composed.
    if !el_status.is_active() {
        return;
    }
    let count = diagnostics.record_el_frame();
    if !DoviRpuDiagnostics::should_report(count) {
        return;
    }
    trace::diagnostic(
        serde_json::json!({
            "event": "dovi_el_not_composed",
            "profile": profile,
            "decodeBackend": decode_backend.as_str(),
            "residualRequested": el_status.residual_requested,
            "nlqNontrivial": el_status.nlq_nontrivial,
            "framesAffected": count,
        })
        .to_string(),
    );
}

fn pop_matching_audio_frame(
    frames: &mut VecDeque<PcmAudioFrame>,
    keep_frame: &mut impl FnMut(&mut PcmAudioFrame) -> bool,
    filtered_frames: &mut usize,
    inspected_frames: &mut usize,
    started: Instant,
    max_duration: Duration,
) -> Option<PcmAudioFrame> {
    while !frames.is_empty() {
        if *inspected_frames > 0 && started.elapsed() >= max_duration {
            return None;
        }
        let mut frame = frames.pop_front().expect("audio frame exists");
        *inspected_frames = inspected_frames.saturating_add(1);
        if keep_frame(&mut frame) {
            return Some(frame);
        }
        *filtered_frames = filtered_frames.saturating_add(1);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioSeekFloorAction {
    Drop,
    Emit { trimmed_frames: usize },
}

fn keep_audio_frame_after_seek_floor(
    seek_floor: &mut Option<Duration>,
    frame: &mut PcmAudioFrame,
) -> bool {
    let Some(target) = *seek_floor else {
        return true;
    };
    let original_pts = frame.pts;
    match trim_audio_frame_to_seek_floor(frame, target) {
        AudioSeekFloorAction::Drop => false,
        AudioSeekFloorAction::Emit { trimmed_frames } => {
            *seek_floor = None;
            trace::log(format!(
                "[erika-playback-trace] stage=audio_seek_floor_emit target={} pts_before={} pts_after={} trimmed_frames={} frames={}",
                trace::duration_label(Some(target)),
                trace::duration_label(original_pts),
                trace::duration_label(frame.pts),
                trimmed_frames,
                frame.frames,
            ));
            true
        }
    }
}

fn trim_audio_frame_to_seek_floor(
    frame: &mut PcmAudioFrame,
    target: Duration,
) -> AudioSeekFloorAction {
    let Some(pts) = frame.pts else {
        return AudioSeekFloorAction::Emit { trimmed_frames: 0 };
    };
    if pts >= target {
        return AudioSeekFloorAction::Emit { trimmed_frames: 0 };
    }

    let channels = frame.format.channels as usize;
    let sample_rate = frame.format.sample_rate;
    if channels == 0 || sample_rate == 0 {
        return AudioSeekFloorAction::Emit { trimmed_frames: 0 };
    }

    let available_frames = frame.samples.len() / channels;
    let delta = target.saturating_sub(pts);
    let frames_to_trim = duration_to_audio_frames_ceil(delta, sample_rate);
    if frames_to_trim >= available_frames {
        return AudioSeekFloorAction::Drop;
    }

    frame.samples = frame.samples.split_off(frames_to_trim * channels);
    frame.frames = frame.samples.len() / channels;
    frame.pts = Some(pts.saturating_add(Duration::from_secs_f64(
        frames_to_trim as f64 / sample_rate as f64,
    )));
    AudioSeekFloorAction::Emit {
        trimmed_frames: frames_to_trim,
    }
}

fn duration_to_audio_frames_ceil(duration: Duration, sample_rate: u32) -> usize {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;
    let numerator = duration.as_nanos().saturating_mul(sample_rate as u128);
    let frames = numerator
        .saturating_add(NANOS_PER_SECOND - 1)
        .checked_div(NANOS_PER_SECOND)
        .unwrap_or(0);
    frames.min(usize::MAX as u128) as usize
}

fn trim_video_queue<T>(frames: &mut VecDeque<T>, limit: usize) {
    while frames.len() > limit {
        // Playback is real-time: when audio-driven demux temporarily outruns
        // presentation, retain the newest decoded frames and discard the
        // stalest one. Dropping the newest tail can otherwise make EOF freeze
        // on an older frame even though the decoder produced the final frame.
        let _ = frames.pop_front();
    }
}

fn buffering_video_trim_droppable(
    frame_pts: impl IntoIterator<Item = Option<Duration>>,
    already_discarded: usize,
    baseline_media_time: Option<Duration>,
) -> usize {
    let remaining_frames = BUFFERING_VIDEO_MAX_DRAINED_FRAMES.saturating_sub(already_discarded);
    frame_pts
        .into_iter()
        .take(remaining_frames)
        .take_while(|pts| {
            !baseline_media_time
                .zip(*pts)
                .is_some_and(|(baseline, pts)| {
                    pts.saturating_sub(baseline) > BUFFERING_VIDEO_MAX_ADVANCE
                })
        })
        .count()
}

fn trim_subtitle_queue(frames: &mut VecDeque<DecodedSubtitleFrame>, limit: usize) {
    while frames.len() > limit {
        let _ = frames.pop_front();
    }
}

fn sanitize_playback_rate(rate: f64) -> f64 {
    if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        1.0
    }
}

// A hardware preference must not make software-only codecs unplayable. This
// policy also covers delayed hardware initialization and renderer import errors.
fn should_fallback_video_decoder_error(backend: DecoderBackend) -> bool {
    backend != DecoderBackend::Software && !require_hardware_decode()
}

/// VideoToolbox and D3D11VA hardware decoders retain the Dolby Vision RPU
/// metadata side data on the hardware AVFrame, allowing GPU textures + RPU
/// reshaping via shaders without software decode.
///
/// Backends without GPU shader RPU mapping (e.g. Android MediaCodec with
/// direct surface output, or OpenHarmony AVCodec) fall back to software
/// decoding for Profile 5 streams to preserve color mapping.
fn dolby_vision_decode_fallback(
    profile: Option<u8>,
    requested: DecoderConfig,
) -> (DecoderConfig, Option<String>) {
    let mobile_hardware_fallback = matches!(
        requested.backend,
        DecoderBackend::MediaCodec | DecoderBackend::AvCodec
    );
    if !mobile_hardware_fallback || profile != Some(5) {
        return (requested, None);
    }
    (
        DecoderConfig::software(),
        Some("dolby vision profile 5 on mobile/embedded backend requires software decode for RPU mapping".to_string()),
    )
}

fn video_decoder_open_stage(config: DecoderConfig) -> &'static str {
    match (config.backend, config.mediacodec_surface) {
        (DecoderBackend::MediaCodec, true) => "open_surface",
        (DecoderBackend::MediaCodec, false) => "open_bytebuffer",
        (DecoderBackend::AvCodec, _) => "open_avcodec",
        _ => "open",
    }
}

fn video_decoder_open_unavailable_error(
    stage: &str,
    requested_backend: DecoderBackend,
    selected_video_track: i32,
    codec: Option<&str>,
    fallback_count: u64,
    reason: String,
) -> PlaybackError {
    let reason = format!("{stage}: {reason}");
    trace::diagnostic(
        serde_json::json!({
            "event": "video_decoder_unavailable",
            "stage": stage,
            "requestedBackend": requested_backend.as_str(),
            "selectedVideoTrack": selected_video_track,
            "codec": codec,
            "fallbackCount": fallback_count,
            "reason": reason.as_str(),
        })
        .to_string(),
    );
    PlaybackError::VideoDecoderUnavailable { reason }
}

fn mediacodec_fallback_target(
    active_backend: Option<DecoderBackend>,
    uses_surface: bool,
    surface_disabled: bool,
) -> Option<MediaCodecFallbackTarget> {
    match active_backend {
        Some(DecoderBackend::MediaCodec) if uses_surface && !surface_disabled => {
            Some(MediaCodecFallbackTarget::ByteBuffer)
        }
        Some(DecoderBackend::MediaCodec) => Some(MediaCodecFallbackTarget::Software),
        _ => None,
    }
}

fn mediacodec_seek_route(
    active_backend: Option<DecoderBackend>,
    uses_surface: bool,
) -> Option<MediaCodecSeekRoute> {
    match active_backend {
        Some(DecoderBackend::MediaCodec) if uses_surface => Some(MediaCodecSeekRoute::Surface),
        Some(DecoderBackend::MediaCodec) => Some(MediaCodecSeekRoute::ByteBuffer),
        _ => None,
    }
}

fn elapsed_since(anchor: Instant, now: Instant) -> Duration {
    now.checked_duration_since(anchor).unwrap_or(Duration::ZERO)
}

fn scale_duration(duration: Duration, rate: f64) -> Duration {
    let seconds = duration.as_secs_f64() * sanitize_playback_rate(rate);
    if seconds.is_finite() && seconds > 0.0 {
        Duration::from_secs_f64(seconds)
    } else {
        Duration::ZERO
    }
}

fn duration_to_nanos(duration: Duration) -> i128 {
    duration.as_nanos().min(i128::MAX as u128) as i128
}

fn nanos_to_duration(nanos: i128) -> Duration {
    Duration::from_nanos(nanos.max(0).min(u64::MAX as i128) as u64)
}

fn add_signed_duration(base: Duration, delta_nanos: i128) -> Duration {
    nanos_to_duration(duration_to_nanos(base).saturating_add(delta_nanos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, thread};

    const FIXTURE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn dropping_async_demuxer_waits_for_source_resources() {
        struct TrackedSource {
            source: source::LocalFileSource,
            dropped: std::sync::mpsc::Sender<()>,
        }
        impl source::MediaSource for TrackedSource {
            fn uri(&self) -> &str {
                self.source.uri()
            }
            fn len(&mut self) -> source::Result<Option<u64>> {
                self.source.len()
            }
            fn read_range(&mut self, range: source::ByteRange) -> source::Result<Vec<u8>> {
                self.source.read_range(range)
            }
        }
        impl Drop for TrackedSource {
            fn drop(&mut self) {
                thread::sleep(Duration::from_millis(50));
                let _ = self.dropped.send(());
            }
        }

        let (dropped, receiver) = std::sync::mpsc::channel();
        let demuxer = Demuxer::open_source(Box::new(TrackedSource {
            source: source::LocalFileSource::open(playback_fixture_path()).unwrap(),
            dropped,
        }))
        .unwrap();
        drop(AsyncDemuxer::spawn(demuxer));
        assert!(
            receiver.try_recv().is_ok(),
            "demux drop returned with a live media source"
        );
    }

    #[test]
    fn dropping_async_demuxer_with_full_packet_queue_does_not_deadlock() {
        let demuxer = AsyncDemuxer::spawn(Demuxer::open_path(playback_fixture_path()).unwrap());
        for _ in 0..=DEMUX_PACKET_QUEUE_LIMIT {
            demuxer
                .commands
                .send(DemuxCommand::SetSelection {
                    generation: 1,
                    selection: StreamSelection::only([i32::MAX]),
                })
                .unwrap();
        }
        let (done, receiver) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            drop(demuxer);
            let _ = done.send(());
        });
        receiver
            .recv_timeout(FIXTURE_WAIT_TIMEOUT)
            .expect("demux teardown blocked on a full packet queue");
        worker.join().unwrap();
    }

    fn playback_fixture_path() -> PathBuf {
        std::env::var_os("ERIKA_PLAYBACK_FIXTURE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("testdata/playback/playback-fixture.mkv")
            })
    }

    fn playback_fixture_engine() -> VideoPlaybackEngine {
        let path = playback_fixture_path();
        assert!(path.is_file(), "fixture is missing: {}", path.display());
        let request = MediaRequest {
            uri: path.to_string_lossy().into_owned(),
            source_hint: MediaSourceHint::LocalFile,
            http_headers: Vec::new(),
            http_read_ahead_bytes: None,
            http_back_buffer_bytes: None,
        };
        let config = PlaybackSessionConfig {
            video_decode: VideoDecodePreference::Software,
            ..PlaybackSessionConfig::default()
        };
        let engine = VideoPlaybackEngine::open(&request, config).unwrap();

        assert_eq!(engine.info().duration, Some(Duration::from_secs(8)));
        assert_eq!(engine.info().selected_video_track, Some(0));
        assert_eq!(engine.info().selected_audio_track, Some(1));
        engine
    }

    #[test]
    fn audio_track_open_falls_back_to_the_first_decodable_track() {
        let opened = first_decodable_audio_track([2, 5, 8], |track_id| match track_id {
            2 | 5 => Err(format!("unsupported track {track_id}")),
            _ => Ok(format!("decoder for {track_id}")),
        })
        .unwrap()
        .unwrap();

        assert_eq!(opened.0, 8);
        assert_eq!(opened.1, "decoder for 8");
        assert_eq!(
            opened.2,
            vec![
                (2, "unsupported track 2".to_string()),
                (5, "unsupported track 5".to_string()),
            ]
        );
    }

    #[test]
    fn audio_track_open_reports_every_failure() {
        let failures = first_decodable_audio_track([1, 3], |track_id| {
            Err::<(), _>(format!("unsupported track {track_id}"))
        })
        .unwrap_err();

        assert_eq!(
            failures,
            vec![
                (1, "unsupported track 1".to_string()),
                (3, "unsupported track 3".to_string()),
            ]
        );
    }

    #[test]
    fn audio_track_open_allows_media_without_audio() {
        let opened = first_decodable_audio_track([], |_track_id| {
            Err::<(), String>("unreachable".to_string())
        })
        .unwrap();

        assert!(opened.is_none());
    }

    #[test]
    fn audio_only_mode_discards_video_packets_and_resumes_at_keyframe() {
        let mut engine = playback_fixture_engine();
        let started_at = Instant::now();
        engine.play_at(started_at);
        engine.set_video_decode_suspended(true).unwrap();

        let deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
        let mut decoded_audio_frames = 0;
        while decoded_audio_frames < 80 {
            if engine.next_audio_frame().unwrap().is_some() {
                decoded_audio_frames += 1;
            }
            assert!(
                Instant::now() < deadline,
                "timed out pumping audio-only playback"
            );
            thread::yield_now();
        }

        assert!(engine.session.video_frames.is_empty());
        assert!(engine.session.pending_video_packets.is_empty());
        assert!(engine.session.video_decode_suspended);

        engine.set_video_decode_suspended(false).unwrap();

        assert!(!engine.session.video_decode_suspended);
        assert!(engine.session.video_fallback_waiting_for_keyframe);
        let _ = next_fixture_video_at(&mut engine, started_at);
        assert!(!engine.session.video_fallback_waiting_for_keyframe);
    }

    fn next_fixture_video_at(engine: &mut VideoPlaybackEngine, now: Instant) -> TimedVideoFrame {
        let deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
        loop {
            if let Some(frame) = engine.tick_at(now).unwrap() {
                return frame;
            }
            assert_ne!(
                engine.state(),
                PlaybackRunState::Ended,
                "fixture reached EOF before yielding a video frame"
            );
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a fixture video frame"
            );
            thread::yield_now();
        }
    }

    fn wait_for_buffering_video_to_park(engine: &mut VideoPlaybackEngine, mut now: Instant) {
        // Advancing a synthetic clock does not advance the asynchronous demux
        // worker. Wait for actual decode progress before checking the bound.
        let deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
        loop {
            now += BUFFERING_VIDEO_DRAIN_INTERVAL;
            assert!(engine.tick_at(now).unwrap().is_none());
            assert!(engine.buffering_video_drained_frames <= BUFFERING_VIDEO_MAX_DRAINED_FRAMES);
            if engine.pending_frame.is_some() {
                // A buffering tick parks a frame only when the recovery bound
                // prevents it from being discarded.
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for bounded video recovery"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn next_fixture_audio_at(engine: &mut VideoPlaybackEngine, now: Instant) -> TimedAudioFrame {
        let deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
        loop {
            if let Some(frame) = engine.tick_audio_at(now).unwrap() {
                return frame;
            }
            assert_eq!(engine.state(), PlaybackRunState::Playing);
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a fixture audio frame"
            );
            thread::yield_now();
        }
    }

    fn positive_zero_crossing_indices(frame: &PcmAudioFrame) -> Vec<usize> {
        let channels = frame.format.channels as usize;
        assert!(channels > 0);
        let mut samples = frame
            .samples
            .chunks_exact(channels)
            .map(|samples| samples[0]);
        let Some(mut previous) = samples.next() else {
            return Vec::new();
        };
        let mut crossings = Vec::new();
        for (index, sample) in samples.enumerate() {
            if previous <= 0.0 && sample > 0.0 {
                crossings.push(index + 1);
            }
            previous = sample;
        }
        crossings
    }

    fn estimated_frequency_hz(frame: &PcmAudioFrame) -> f64 {
        let crossings = positive_zero_crossing_indices(frame);
        assert!(crossings.len() >= 2);
        let crossing_span = crossings.last().unwrap() - crossings.first().unwrap();
        (crossings.len() - 1) as f64 * frame.format.sample_rate as f64 / crossing_span as f64
    }

    fn duration_difference(left: Duration, right: Duration) -> Duration {
        if left >= right {
            left - right
        } else {
            right - left
        }
    }

    fn next_fixture_av_at_target(
        engine: &mut VideoPlaybackEngine,
        now: Instant,
        target: Duration,
    ) -> (TimedVideoFrame, TimedAudioFrame) {
        let video = next_fixture_video_at(engine, now);
        let audio = next_fixture_audio_at(engine, now);
        let video_pts = video.pts.expect("fixture video PTS");
        let audio_pts = audio.pts.expect("fixture audio PTS");

        assert!(video_pts >= target, "video preroll escaped: {video_pts:?}");
        assert!(audio_pts >= target, "audio preroll escaped: {audio_pts:?}");
        assert!(video_pts - target <= Duration::from_millis(34));
        assert!(audio_pts - target <= Duration::from_millis(34));
        assert!(duration_difference(video_pts, audio_pts) <= Duration::from_millis(34));
        assert_eq!(video.media_time, video_pts);
        (video, audio)
    }

    fn drive_fixture_to_eof_at(
        engine: &mut VideoPlaybackEngine,
        started_at: Instant,
    ) -> (Instant, Duration) {
        let mut timing = engine.timing_config();
        timing.video_scheduler.drop_tolerance = Duration::from_secs(9);
        engine.set_timing_config(timing);
        engine.play_at(started_at);
        let first = next_fixture_video_at(engine, started_at);
        assert_eq!(first.pts, Some(Duration::ZERO));

        let eof_at = started_at + Duration::from_secs(8);
        let deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
        let mut last_video_pts = first.pts;
        while engine.state() != PlaybackRunState::Ended {
            if let Some(frame) = engine.tick_at(eof_at).unwrap() {
                last_video_pts = frame.pts.or(last_video_pts);
            }
            let _ = engine.tick_audio_at(eof_at).unwrap();
            assert!(
                Instant::now() < deadline,
                "timed out waiting for fixture EOF"
            );
            thread::yield_now();
        }
        (eof_at, last_video_pts.expect("last fixture video PTS"))
    }

    fn assert_decoder_unavailable<T>(result: Result<T>, expected: &str) {
        match result {
            Err(PlaybackError::VideoDecoderUnavailable { reason }) => {
                assert_eq!(reason, expected)
            }
            Err(error) => panic!("expected VideoDecoderUnavailable, got {error}"),
            Ok(_) => panic!("expected VideoDecoderUnavailable"),
        }
    }

    #[cfg(target_os = "android")]
    fn wait_for_android_video_frame(
        engine: &mut VideoPlaybackEngine,
        stage: &str,
    ) -> TimedVideoFrame {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(frame) = engine.tick().unwrap() {
                return frame;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {stage}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn decoder_drain_status_reports_only_frames_or_end_of_stream_as_progress() {
        assert!(!DecoderDrainStatus::default().made_progress());
        assert!(
            DecoderDrainStatus {
                frames: 1,
                end_of_stream: false,
            }
            .made_progress()
        );
        assert!(
            DecoderDrainStatus {
                frames: 0,
                end_of_stream: true,
            }
            .made_progress()
        );
        assert!(
            DecoderDrainStatus {
                frames: 1,
                end_of_stream: true,
            }
            .made_progress()
        );
    }

    #[test]
    fn playback_eof_ready_covers_every_pending_output_combination() {
        for session_eof in [false, true] {
            for pending_video in [false, true] {
                for queued_video in [false, true] {
                    for audio_output_active in [false, true] {
                        for pending_audio in [false, true] {
                            for queued_audio in [false, true] {
                                let expected = session_eof
                                    && !pending_video
                                    && !queued_video
                                    && (!audio_output_active || (!pending_audio && !queued_audio));
                                assert_eq!(
                                    playback_eof_ready(
                                        session_eof,
                                        pending_video,
                                        queued_video,
                                        audio_output_active,
                                        pending_audio,
                                        queued_audio,
                                    ),
                                    expected,
                                    "session_eof={session_eof} pending_video={pending_video} queued_video={queued_video} audio_output_active={audio_output_active} pending_audio={pending_audio} queued_audio={queued_audio}",
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn only_natural_eof_requires_a_zero_rewind_before_play() {
        assert!(!playback_state_requires_rewind(PlaybackRunState::Stopped));
        assert!(playback_state_requires_rewind(PlaybackRunState::Ended));
        assert!(!playback_state_requires_rewind(PlaybackRunState::Paused));
        assert!(!playback_state_requires_rewind(PlaybackRunState::Playing));
    }

    #[test]
    fn selected_video_track_without_decoder_reports_the_recorded_reason() {
        assert_eq!(video_decoder_unavailability_reason(None, false, None), None);
        assert_eq!(
            video_decoder_unavailability_reason(Some(7), true, Some("ignored")),
            None
        );
        assert_eq!(
            video_decoder_unavailability_reason(Some(7), false, Some("software fallback failed")),
            Some("software fallback failed".to_string())
        );
        assert_eq!(
            video_decoder_unavailability_reason(Some(7), false, None),
            Some(
                "selected video track 7 has no active decoder; reopen the media before playback"
                    .to_string()
            )
        );
    }

    #[test]
    fn successful_seek_preserves_terminal_and_paused_states() {
        assert_eq!(
            playback_state_after_seek(PlaybackRunState::Stopped),
            PlaybackRunState::Stopped
        );
        assert_eq!(
            playback_state_after_seek(PlaybackRunState::Ended),
            PlaybackRunState::Stopped
        );
        assert_eq!(
            playback_state_after_seek(PlaybackRunState::Paused),
            PlaybackRunState::Paused
        );
        assert_eq!(
            playback_state_after_seek(PlaybackRunState::Playing),
            PlaybackRunState::Playing
        );
        assert_eq!(
            playback_state_after_seek_intent(PlaybackRunState::Paused, true),
            PlaybackRunState::Playing
        );
        assert_eq!(
            playback_state_after_seek_intent(PlaybackRunState::Paused, false),
            PlaybackRunState::Paused
        );
    }

    #[test]
    fn mediacodec_seek_reopens_the_active_surface_or_bytebuffer_route() {
        assert_eq!(
            mediacodec_seek_route(Some(DecoderBackend::MediaCodec), true),
            Some(MediaCodecSeekRoute::Surface)
        );
        assert_eq!(
            mediacodec_seek_route(Some(DecoderBackend::MediaCodec), false),
            Some(MediaCodecSeekRoute::ByteBuffer)
        );
        assert_eq!(
            mediacodec_seek_route(Some(DecoderBackend::Software), false),
            None
        );
        assert_eq!(
            MediaCodecSeekRoute::Surface.decoder_config(),
            DecoderConfig::mediacodec()
        );
        assert_eq!(
            MediaCodecSeekRoute::ByteBuffer.decoder_config(),
            DecoderConfig::mediacodec_byte_buffer()
        );
    }

    #[test]
    fn seek_preroll_budget_yields_by_frame_count_or_elapsed_time() {
        assert!(!seek_preroll_budget_exhausted(
            2,
            3,
            SEEK_PREROLL_DECODE_TIME_BUDGET.saturating_sub(Duration::from_nanos(1))
        ));
        assert!(seek_preroll_budget_exhausted(3, 3, Duration::ZERO));
        assert!(seek_preroll_budget_exhausted(
            1,
            100,
            SEEK_PREROLL_DECODE_TIME_BUDGET
        ));
        assert!(
            seek_preroll_budget_exhausted(1, 0, Duration::ZERO),
            "a zero scheduler budget must still allow one frame before yielding"
        );
    }

    #[test]
    fn software_only_wmv_plays_and_seeks_with_every_decoder_preference() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/software/wmv2-wmav2.asf");
        for preference in [
            VideoDecodePreference::Software,
            VideoDecodePreference::VideoToolbox,
            VideoDecodePreference::D3d11va,
            VideoDecodePreference::MediaCodec,
            VideoDecodePreference::MediaCodecByteBuffer,
            VideoDecodePreference::AvCodec,
        ] {
            let mut engine = VideoPlaybackEngine::open(
                &MediaRequest::new(path.to_string_lossy()),
                PlaybackSessionConfig {
                    video_decode: preference,
                    ..PlaybackSessionConfig::default()
                },
            )
            .unwrap_or_else(|error| panic!("{preference:?}: {error}"));
            assert_eq!(
                engine.info().video_decode_backend,
                Some(DecoderBackend::Software)
            );
            let events = engine.take_video_decoder_events();
            let event = events.last().expect("decoder selection event");
            assert_eq!(event.requested_backend, preference.decoder_config().backend);
            assert_eq!(event.active_backend, DecoderBackend::Software);
            assert_eq!(event.codec.as_deref(), Some("wmv2"));
            assert_eq!(
                event.fallback_count > 0,
                preference != VideoDecodePreference::Software
            );
            let started = Instant::now();
            engine.play_at(started);
            let video = next_fixture_video_at(&mut engine, started);
            assert_eq!(video.decode_backend, DecoderBackend::Software);
            let planar = video
                .frame
                .to_planar_frame()
                .expect("GPU-uploadable software frame");
            assert_eq!((planar.width, planar.height), (160, 90));
            assert!(planar.luma.iter().any(|value| *value != planar.luma[0]));
            let audio = next_fixture_audio_at(&mut engine, started);
            assert!(!audio.frame.samples.is_empty());

            let target = Duration::from_secs(1);
            let seek_at = started + target;
            engine.seek_at(target, seek_at).unwrap();
            let video = next_fixture_video_at(&mut engine, seek_at);
            assert!(video.pts.unwrap() >= target);
            assert!(video.frame.to_planar_frame().is_some());
            assert_eq!(
                engine.info().video_decode_backend,
                Some(DecoderBackend::Software)
            );

            engine.stop_checked_at(seek_at).unwrap();
            engine.play_at(seek_at);
            let replay = next_fixture_video_at(&mut engine, seek_at);
            assert_eq!(replay.pts, Some(Duration::from_millis(46)));
        }
    }

    #[test]
    fn decoder_open_fallback_is_enabled_for_platform_hardware_backends() {
        for backend in [
            DecoderBackend::VideoToolbox,
            DecoderBackend::D3d11va,
            DecoderBackend::MediaCodec,
            DecoderBackend::AvCodec,
        ] {
            assert!(should_fallback_video_decoder_error(backend));
        }
        assert!(!should_fallback_video_decoder_error(
            DecoderBackend::Software
        ));
    }

    #[test]
    fn mediacodec_open_events_identify_surface_and_bytebuffer_modes() {
        assert_eq!(
            video_decoder_open_stage(DecoderConfig::mediacodec()),
            "open_surface"
        );
        assert_eq!(
            video_decoder_open_stage(DecoderConfig::mediacodec_byte_buffer()),
            "open_bytebuffer"
        );
        assert_eq!(video_decoder_open_stage(DecoderConfig::software()), "open");
    }

    #[test]
    fn mediacodec_surface_failure_steps_down_to_bytebuffer() {
        assert_eq!(
            mediacodec_fallback_target(Some(DecoderBackend::MediaCodec), true, false),
            Some(MediaCodecFallbackTarget::ByteBuffer)
        );
    }

    #[test]
    fn mediacodec_bytebuffer_failure_steps_down_to_software() {
        assert_eq!(
            mediacodec_fallback_target(Some(DecoderBackend::MediaCodec), false, true),
            Some(MediaCodecFallbackTarget::Software)
        );
        assert_eq!(
            mediacodec_fallback_target(Some(DecoderBackend::MediaCodec), true, true),
            Some(MediaCodecFallbackTarget::Software),
            "a disabled Surface path must not be re-enabled by a stale decoder"
        );
    }

    #[test]
    fn mediacodec_fallback_ignores_non_mediacodec_decoders() {
        assert_eq!(
            mediacodec_fallback_target(Some(DecoderBackend::Software), false, true),
            None
        );
    }

    #[test]
    fn decoder_unavailability_survives_stop_and_blocks_replay_when_env_is_set() {
        let Ok(sample) = std::env::var("ERIKA_TEST_SAMPLE") else {
            return;
        };
        let config = PlaybackSessionConfig {
            video_decode: VideoDecodePreference::Software,
            ..PlaybackSessionConfig::default()
        };
        let mut engine = VideoPlaybackEngine::open(&MediaRequest::new(sample), config).unwrap();
        assert!(engine.session.info.selected_video_track.is_some());
        engine.play().unwrap();

        let reason = "forced final software decoder fallback failure".to_string();
        drop(engine.session.video_decoder.take());
        engine
            .session
            .mark_video_decoder_unavailable(reason.clone());
        assert_eq!(engine.info().video_decode_backend, None);
        engine.stop();

        assert_decoder_unavailable(engine.session.next_video_frame(), &reason);
        assert_decoder_unavailable(engine.seek(Duration::from_secs(1)), &reason);
        assert_decoder_unavailable(engine.play(), &reason);
        assert_eq!(engine.state(), PlaybackRunState::Stopped);
        assert_eq!(engine.media_time(), Duration::ZERO);
        assert!(!engine.clock.is_running());
        assert!(!engine.should_prefill_audio());
    }

    #[cfg(target_os = "android")]
    #[test]
    fn repeated_android_seek_and_track_transitions_release_old_mediacodec_routes_when_env_is_set() {
        let Ok(sample) = std::env::var("ERIKA_TEST_SAMPLE") else {
            return;
        };
        let subtitle_path = std::env::temp_dir().join(format!(
            "erika-android-transition-stress-{}.srt",
            std::process::id()
        ));
        fs::write(
            &subtitle_path,
            "1\n00:00:00,000 --> 00:00:10,000\nAndroid transition stress\n",
        )
        .unwrap();

        let config = PlaybackSessionConfig {
            video_decode: VideoDecodePreference::MediaCodec,
            ..PlaybackSessionConfig::default()
        };
        let mut engine = VideoPlaybackEngine::open(&MediaRequest::new(sample), config).unwrap();
        let audio_track = engine
            .info()
            .selected_audio_track
            .expect("ERIKA_TEST_SAMPLE must contain audio");
        let (subtitle_track, _) = engine
            .add_external_subtitle(SubtitleTrackConfig::external(
                1_900_001,
                subtitle_path.to_string_lossy(),
            ))
            .unwrap();
        engine.play().unwrap();

        for round in 0..6u64 {
            let target = Duration::from_millis((round * 733) % 4_500);
            engine.seek(target).unwrap();
            drop(wait_for_android_video_frame(&mut engine, "seek frame"));

            engine.select_audio_track(None).unwrap();
            engine.select_audio_track(Some(audio_track)).unwrap();
            engine.select_subtitle_track(None).unwrap();
            engine
                .select_subtitle_track(Some(subtitle_track.id))
                .unwrap();
            drop(wait_for_android_video_frame(
                &mut engine,
                "post-track-transition frame",
            ));
        }

        assert_eq!(
            engine.session.active_video_decoder_backend(),
            Some(DecoderBackend::MediaCodec)
        );
        assert!(engine.session.video_decoder_unavailable_reason.is_none());
        engine.remove_subtitle_track(subtitle_track.id).unwrap();
        drop(engine);
        let _ = fs::remove_file(subtitle_path);
    }

    #[test]
    fn opened_media_info_keeps_probe_summary() {
        let mut video_track = TrackInfo::embedded(0, TrackKind::Video);
        video_track.codec = Some("hevc".to_string());
        let info = OpenedMediaInfo {
            uri: "file:///tmp/test.mp4".to_string(),
            duration: Some(Duration::from_secs(12)),
            tracks: vec![video_track],
            video_params: Some(VideoParams {
                width: 3840,
                height: 2160,
                primaries: crate::core::ColorPrimaries::Bt2020,
                transfer: crate::core::TransferFunction::Pq,
            }),
            selected_video_track: Some(0),
            selected_audio_track: Some(1),
            selected_subtitle_track: Some(2),
            subtitle_tracks: vec![crate::subtitle::SubtitleTrackConfig::embedded(2, 2)],
            video_decode_backend: Some(DecoderBackend::Software),
            audio_output: Some(PcmFormat::default()),
        };

        assert_eq!(info.duration, Some(Duration::from_secs(12)));
        assert_eq!(info.tracks.len(), 1);
        assert_eq!(
            info.video_params.as_ref().map(|params| params.width),
            Some(3840)
        );
        assert_eq!(info.selected_video_track, Some(0));
        assert_eq!(info.selected_audio_track, Some(1));
        assert_eq!(info.selected_subtitle_track, Some(2));
        assert_eq!(info.subtitle_tracks.len(), 1);
        assert_eq!(info.video_decode_backend, Some(DecoderBackend::Software));
        assert_eq!(info.audio_output, Some(PcmFormat::default()));
    }

    #[test]
    fn playback_fixture_rapid_seek_emits_only_final_target_av() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);

        engine
            .seek_at(Duration::from_millis(1_275), t0 + Duration::from_millis(10))
            .unwrap();
        engine
            .seek_at(Duration::from_millis(6_125), t0 + Duration::from_millis(20))
            .unwrap();
        let target = Duration::from_millis(3_250);
        let settled_at = t0 + Duration::from_millis(30);
        engine.seek_at(target, settled_at).unwrap();

        let video = next_fixture_video_at(&mut engine, settled_at);
        let audio = next_fixture_audio_at(&mut engine, settled_at);
        let video_pts = video.pts.expect("fixture video PTS");
        let audio_pts = audio.pts.expect("fixture audio PTS");

        assert!(video_pts >= target, "video preroll escaped: {video_pts:?}");
        assert!(audio_pts >= target, "audio preroll escaped: {audio_pts:?}");
        assert!(video_pts - target <= Duration::from_millis(34));
        assert!(audio_pts - target <= Duration::from_millis(34));
        assert!(duration_difference(video_pts, audio_pts) <= Duration::from_millis(34));
        assert_eq!(video.media_time, video_pts);
    }

    #[test]
    fn playback_fixture_paused_seek_previews_target_and_keeps_clock_frozen() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);

        let paused_at = t0 + Duration::from_millis(500);
        engine.pause_at(paused_at);
        let target = Duration::from_millis(5_125);
        engine.seek_at(target, paused_at).unwrap();

        assert_eq!(engine.state(), PlaybackRunState::Paused);
        assert_eq!(engine.media_time_at(paused_at), target);
        assert_eq!(
            engine.media_time_at(paused_at + Duration::from_secs(60)),
            target
        );
        let resume_at = paused_at + Duration::from_secs(60);
        let preview = next_fixture_video_at(&mut engine, resume_at);
        let preview_pts = preview.pts.expect("paused seek preview PTS");
        assert!(preview_pts >= target);
        assert!(preview_pts - target <= Duration::from_millis(34));
        assert_eq!(preview.media_time, target);
        assert_eq!(engine.state(), PlaybackRunState::Paused);
        assert_eq!(engine.media_time_at(resume_at), target);
        assert!(
            engine.tick_at(resume_at).unwrap().is_none(),
            "a paused seek must present exactly one target frame"
        );
        assert!(engine.tick_audio_at(resume_at).unwrap().is_none());

        engine.play_at(resume_at);
        let playing_at = resume_at + Duration::from_millis(50);
        let video = next_fixture_video_at(&mut engine, playing_at);
        let audio = next_fixture_audio_at(&mut engine, playing_at);
        let video_pts = video.pts.expect("resumed video PTS");
        let audio_pts = audio.pts.expect("resumed audio PTS");
        assert!(video_pts >= target);
        assert!(audio_pts >= target);
        assert!(audio_pts - target <= Duration::from_millis(34));
    }

    #[test]
    fn playback_fixture_paused_seek_past_the_last_frame_stays_paused() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);

        let paused_at = t0 + Duration::from_millis(500);
        engine.pause_at(paused_at);
        // The fixture is 8 s long and its frames end before the target, so
        // every decoded frame is seek preroll and the preview pump reaches
        // demux EOF without ever presenting a frame. Audio output stays off
        // so the queued tail audio is discarded at EOF instead of stalling it.
        let target = Duration::from_secs(8);
        engine.seek_at(target, paused_at).unwrap();
        engine.set_audio_output_active(false);

        assert_eq!(engine.state(), PlaybackRunState::Paused);
        assert_eq!(engine.media_time_at(paused_at), target);

        let mut ticks = 0;
        while engine.has_pending_paused_seek_frame() {
            let _ = engine
                .tick_at(paused_at + Duration::from_millis(ticks))
                .unwrap();
            ticks += 1;
            assert!(
                ticks < 2000,
                "the preview pump must reach the abandonment path"
            );
            assert_eq!(
                engine.state(),
                PlaybackRunState::Paused,
                "a paused session must not become Ended while only seeking"
            );
            assert_eq!(
                engine.media_time_at(paused_at + Duration::from_millis(ticks)),
                target
            );
        }
        assert!(ticks > 0);
        assert_eq!(engine.state(), PlaybackRunState::Paused);
    }

    #[test]
    fn playback_fixture_suspended_decode_abandons_the_paused_seek_preview() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);

        let paused_at = t0 + Duration::from_millis(500);
        engine.pause_at(paused_at);
        let target = Duration::from_millis(5_125);
        engine.seek_at(target, paused_at).unwrap();
        engine.set_video_decode_suspended(true);

        assert!(engine.has_pending_paused_seek_frame());
        let tick = engine.tick_at(paused_at + Duration::from_secs(60)).unwrap();
        assert!(tick.is_none());
        assert!(
            !engine.has_pending_paused_seek_frame(),
            "a suspended decoder can never produce the preview"
        );
        assert_eq!(engine.state(), PlaybackRunState::Paused);
        assert_eq!(
            engine.media_time_at(paused_at + Duration::from_secs(60)),
            target
        );
    }

    #[test]
    fn playback_fixture_seek_parks_the_clock_until_the_first_frame_is_presented() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);

        let seek_at = t0 + Duration::from_millis(500);
        let target = Duration::from_millis(5_125);
        engine.seek_at(target, seek_at).unwrap();

        // Preroll: no post-seek frame has been presented yet. The clock must
        // stay parked on the target however long decoding takes, so the
        // reported position never runs past frames nobody has seen...
        for elapsed in [0u64, 20, 100, 400, 800] {
            assert_eq!(
                engine.media_time_at(seek_at + Duration::from_millis(elapsed)),
                target,
                "clock advanced during seek preroll after {elapsed} ms"
            );
        }

        // ...and audio pulled during that window is likewise gated on the
        // target rather than an already-advancing clock.
        let audio = next_fixture_audio_at(&mut engine, seek_at + Duration::from_millis(20));
        let audio_pts = audio.pts.expect("post-seek audio PTS");
        assert!(audio_pts >= target);

        // The first presented frame starts the clock; from there it runs.
        let resumed_at = seek_at + Duration::from_millis(900);
        let video = next_fixture_video_at(&mut engine, resumed_at);
        let video_pts = video.pts.expect("post-seek video PTS");
        assert!(video_pts >= target);
        assert!(engine.media_time_at(resumed_at) >= target);
        assert!(
            engine.media_time_at(resumed_at + Duration::from_millis(50))
                > engine.media_time_at(resumed_at),
            "clock must run once the first post-seek frame is presented"
        );
    }

    #[test]
    fn playback_fixture_pause_during_playing_seek_finishes_target_preview() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);

        let seek_at = t0 + Duration::from_millis(500);
        let target = Duration::from_millis(5_125);
        engine.seek_at(target, seek_at).unwrap();
        assert_eq!(engine.state(), PlaybackRunState::Playing);

        let paused_at = seek_at + Duration::from_millis(100);
        engine.pause_at(paused_at);
        assert_eq!(engine.state(), PlaybackRunState::Paused);
        assert!(engine.has_pending_paused_seek_frame());
        assert_eq!(engine.media_time_at(paused_at), target);

        let preview = next_fixture_video_at(&mut engine, paused_at);
        let preview_pts = preview.pts.expect("interrupted seek preview PTS");
        assert!(preview_pts >= target);
        assert!(preview_pts - target <= Duration::from_millis(34));
        assert_eq!(preview.media_time, target);
        assert_eq!(engine.state(), PlaybackRunState::Paused);
        assert!(!engine.has_pending_paused_seek_frame());
        assert!(engine.tick_at(paused_at).unwrap().is_none());
    }

    #[test]
    fn playback_fixture_non_unit_rate_uses_audio_master_correction() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);

        let rate_changed_at = t0 + Duration::from_millis(100);
        engine.set_playback_rate_at(2.0, rate_changed_at);
        let sync_at = rate_changed_at + Duration::from_millis(100);
        // Establish actual output progress; an unread first measurement does
        // not by itself establish an audio master, at any playback rate.
        assert!(
            engine
                .sync_to_audio_clock_at(
                    AudioClockSnapshot {
                        media_time: Some(Duration::from_millis(4998)),
                        queued_duration: Some(Duration::from_millis(250)),
                        queued_frames: 12_000,
                        read_frames: 23_952,
                        written_frames: 35_952,
                        underflow_frames: 0,
                    },
                    sync_at - Duration::from_millis(1),
                )
                .is_none()
        );
        let before = engine.clock_snapshot_at(sync_at);
        let correction = engine.sync_to_audio_clock_at(
            AudioClockSnapshot {
                media_time: Some(Duration::from_secs(5)),
                queued_duration: Some(Duration::from_millis(250)),
                queued_frames: 12_000,
                read_frames: 24_000,
                written_frames: 36_000,
                underflow_frames: 0,
            },
            sync_at,
        );

        assert_eq!(before.media_time, Duration::from_millis(300));
        assert_eq!(before.source, PlaybackClockSource::Wall);
        assert_eq!(before.rate, 2.0);

        let correction = correction.expect("audio clock remains authoritative at 2x");
        assert_eq!(correction.source, PlaybackClockSource::Audio);
        assert!(correction.snapped);
        let after = engine.clock_snapshot_at(sync_at);
        assert_eq!(after.media_time, Duration::from_secs(5));
        assert_eq!(after.source, PlaybackClockSource::Audio);
        assert_eq!(after.rate, 2.0);
    }

    fn clock_test_ring() -> crate::audio::AudioRingBuffer {
        crate::audio::AudioRingBuffer::with_format(
            crate::audio::AudioRingBufferConfig::default(),
            PcmFormat::f32_interleaved(48_000, 1),
        )
        .unwrap()
    }

    fn clock_test_push(ring: &mut crate::audio::AudioRingBuffer, pts_ms: u64, ms: u64) {
        let frames = ms as usize * 48;
        ring.push_frame(PcmAudioFrame {
            format: PcmFormat::f32_interleaved(48_000, 1),
            pts: Some(Duration::from_millis(pts_ms)),
            frames,
            samples: vec![0.25; frames],
        })
        .unwrap();
    }

    fn clock_test_consume(ring: &mut crate::audio::AudioRingBuffer, ms: u64) {
        ring.read_interleaved(&mut vec![0.0; ms as usize * 48])
            .unwrap();
    }

    #[test]
    fn playback_fixture_foreground_resume_does_not_repeat_published_audio() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let mut output_end = Duration::ZERO;
        for _ in 0..2 {
            let frame = next_fixture_audio_at(&mut engine, t0);
            output_end = frame.pts.unwrap()
                + Duration::from_secs_f64(
                    frame.frame.frames as f64 / f64::from(frame.frame.format.sample_rate),
                );
            engine.record_audio_output_end(Some(output_end));
        }
        assert!(output_end > Duration::from_millis(100));
        engine.set_video_decode_suspended(true).unwrap();
        engine.set_video_decode_suspended(false).unwrap();
        // The old output tail is still queued. Video may preroll behind that
        // tail, but audio handed off after recovery must start at its end.
        let now = t0 + output_end;
        let resumed = next_fixture_audio_at(&mut engine, now);
        let resumed_pts = resumed.pts.unwrap();
        assert!(resumed_pts >= output_end);
        assert!(resumed_pts - output_end < Duration::from_millis(1));

        // A real seek discards queued output, so its old tail must not suppress
        // audio on the replacement timeline.
        engine.seek_at(Duration::ZERO, now).unwrap();
        let _ = next_fixture_video_at(&mut engine, now);
        let replay = next_fixture_audio_at(&mut engine, now);
        assert_eq!(replay.pts, Some(Duration::ZERO));
    }

    #[test]
    fn playback_fixture_audio_clock_recovers_after_render_stall() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let mut ring = clock_test_ring();
        clock_test_push(&mut ring, 0, 300);
        engine.sync_to_audio_clock_at(ring.clock_snapshot(), t0);
        clock_test_consume(&mut ring, 100);
        assert!(
            engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_millis(100))
                .is_some()
        );
        // Audio continues consuming while rendering prevents PCM handoff.
        clock_test_consume(&mut ring, 733);
        assert_eq!(ring.stats().underflow_frames, 25_584);
        clock_test_push(&mut ring, 300, 800);
        let mut corrections = 0;
        for step in 0..=600 {
            if step > 0 {
                clock_test_consume(&mut ring, 10);
                clock_test_push(&mut ring, 1100 + (step - 1) * 10, 10);
            }
            let now = t0 + Duration::from_millis(833 + step * 10);
            let correction = engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), now)
                .unwrap();
            if step == 0 {
                assert!(correction.snapped);
            }
            corrections += 1;
            assert!(
                engine
                    .clock_snapshot_at(now)
                    .media_time
                    .abs_diff(ring.clock_snapshot().media_time.unwrap())
                    <= Duration::from_millis(5)
            );
        }
        assert_eq!(corrections, 601);
    }

    #[test]
    fn playback_fixture_frozen_or_silent_audio_does_not_drive_clock() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let mut ring = clock_test_ring();
        clock_test_push(&mut ring, 0, 300);
        assert!(
            engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), t0)
                .is_none()
        );
        assert!(
            engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_millis(10))
                .is_none()
        );
        clock_test_consume(&mut ring, 300);
        assert!(
            engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_millis(300))
                .is_some()
        );
        for step in 1..=600 {
            clock_test_consume(&mut ring, 10);
            assert!(
                engine
                    .sync_to_audio_clock_at(
                        ring.clock_snapshot(),
                        t0 + Duration::from_millis(300 + step * 10)
                    )
                    .is_none()
            );
        }
        assert_eq!(
            engine
                .clock_snapshot_at(t0 + Duration::from_millis(6300))
                .media_time,
            Duration::from_millis(6300)
        );
    }

    #[test]
    fn playback_fixture_rejected_audio_regressions_do_not_lower_progress_baseline() {
        let sample = |media_ms: u64, read_ms: u64| AudioClockSnapshot {
            media_time: Some(Duration::from_millis(media_ms)),
            queued_duration: Some(Duration::from_millis(200)),
            queued_frames: 9_600,
            read_frames: read_ms * 48,
            written_frames: read_ms * 48 + 9_600,
            underflow_frames: 0,
        };
        for (case, regressed, partial_recovery) in [
            ("both", sample(200, 200), sample(300, 300)),
            ("read counter", sample(1010, 200), sample(1020, 300)),
            ("media time", sample(200, 1010), sample(300, 1020)),
        ] {
            let mut engine = playback_fixture_engine();
            let t0 = Instant::now();
            engine.play_at(t0);
            let _ = next_fixture_video_at(&mut engine, t0);
            assert!(
                engine
                    .sync_to_audio_clock_at(sample(900, 900), t0 + Duration::from_millis(900))
                    .is_none()
            );
            assert!(
                engine
                    .sync_to_audio_clock_at(sample(1000, 1000), t0 + Duration::from_millis(1000))
                    .is_some()
            );

            // The second invalid sample advances relative to the rejected one,
            // but still regresses relative to the last trusted observation.
            for (offset, snapshot) in [(1010, regressed), (1020, partial_recovery)] {
                let now = t0 + Duration::from_millis(offset);
                let before = engine.media_time_at(now);
                assert!(
                    engine.sync_to_audio_clock_at(snapshot, now).is_none(),
                    "{case} regression at {offset} ms must not drive the clock"
                );
                assert_eq!(engine.media_time_at(now), before);
            }

            // Rejection must retain capture ordering too: a delayed observation
            // cannot become current just because its counters appear valid.
            let now = t0 + Duration::from_millis(1020);
            let before = engine.media_time_at(now);
            assert!(
                engine
                    .discipline_audio_observation(
                        sample(1015, 1015),
                        t0 + Duration::from_millis(1015),
                        now,
                    )
                    .is_none(),
                "{case}: rejected samples must still preserve capture ordering"
            );
            assert_eq!(engine.media_time_at(now), before);

            // A bad observation must neither lower the progress baseline nor
            // prevent later progress on the original timeline from syncing.
            assert!(
                engine
                    .sync_to_audio_clock_at(sample(1030, 1030), t0 + Duration::from_millis(1030))
                    .is_some(),
                "{case}: genuine progress must recover without an output reset"
            );
        }
    }

    #[test]
    fn playback_fixture_audio_clock_preserves_seek_floor_and_capture_order() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let mut ring = clock_test_ring();
        clock_test_push(&mut ring, 0, 500);
        engine.sync_to_audio_clock_at(ring.clock_snapshot(), t0);
        clock_test_consume(&mut ring, 200);
        let old = ring.clock_snapshot();
        engine.sync_to_audio_clock_at(old, t0 + Duration::from_millis(200));
        engine
            .seek_at(Duration::from_secs(3), t0 + Duration::from_secs(1))
            .unwrap();
        let _ = next_fixture_video_at(&mut engine, t0 + Duration::from_secs(1));
        assert!(
            engine
                .sync_to_audio_clock_at(old, t0 + Duration::from_secs(1))
                .is_none()
        );
        assert_eq!(
            engine
                .clock_snapshot_at(t0 + Duration::from_secs(1))
                .media_time,
            Duration::from_secs(3)
        );

        ring.clear();
        clock_test_push(&mut ring, 3000, 500);
        engine.sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_secs(1));
        clock_test_consume(&mut ring, 10);
        let captured_at = t0 + Duration::from_millis(1010);
        let current = ring.clock_snapshot();
        assert!(
            engine
                .sync_to_audio_clock_at(current, captured_at)
                .is_some()
        );
        clock_test_consume(&mut ring, 10);
        assert!(
            engine
                .discipline_audio_observation(
                    ring.clock_snapshot(),
                    captured_at - Duration::from_millis(1),
                    captured_at
                )
                .is_none()
        );
    }

    #[test]
    fn playback_fixture_audio_clock_cannot_move_parked_first_frame_clock() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let mut ring = clock_test_ring();
        clock_test_push(&mut ring, 0, 300);
        for step in 0..3 {
            clock_test_consume(&mut ring, 50);
            let now = t0 + Duration::from_millis(step * 50);
            assert!(
                engine
                    .sync_to_audio_clock_at(ring.clock_snapshot(), now)
                    .is_none()
            );
            assert_eq!(engine.clock_snapshot_at(now).media_time, Duration::ZERO);
        }
        engine.seek_at(Duration::from_secs(3), t0).unwrap();
        ring.clear();
        clock_test_push(&mut ring, 3000, 300);
        for step in 0..3 {
            clock_test_consume(&mut ring, 50);
            let now = t0 + Duration::from_millis(150 + step * 50);
            assert!(
                engine
                    .sync_to_audio_clock_at(ring.clock_snapshot(), now)
                    .is_none()
            );
            assert_eq!(
                engine.clock_snapshot_at(now).media_time,
                Duration::from_secs(3)
            );
        }
    }

    #[test]
    fn playback_fixture_audio_clock_requires_new_progress_after_pause() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let mut ring = clock_test_ring();
        clock_test_push(&mut ring, 0, 500);
        engine.sync_to_audio_clock_at(ring.clock_snapshot(), t0);
        clock_test_consume(&mut ring, 100);
        engine.sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_millis(100));
        engine.pause_at(t0 + Duration::from_millis(100));
        // A final callback may consume PCM as the host pauses the device.
        clock_test_consume(&mut ring, 20);
        engine.play_at(t0 + Duration::from_secs(1));
        assert!(
            engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_secs(1))
                .is_none()
        );
        assert!(
            engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_millis(1010))
                .is_none()
        );
        clock_test_consume(&mut ring, 10);
        assert!(
            engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_millis(1020))
                .is_some()
        );
    }

    #[test]
    fn playback_fixture_audio_clock_stays_master_while_video_resumes() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let mut ring = clock_test_ring();
        clock_test_push(&mut ring, 0, 500);
        engine.sync_to_audio_clock_at(ring.clock_snapshot(), t0);
        engine.set_video_decode_suspended(true).unwrap();
        clock_test_consume(&mut ring, 100);
        assert!(
            engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_millis(100))
                .is_some()
        );
        engine.set_video_decode_suspended(false).unwrap();
        assert!(engine.is_waiting_for_first_frame());
        assert!(engine.clock().is_running());
        clock_test_consume(&mut ring, 100);
        assert!(
            engine
                .sync_to_audio_clock_at(ring.clock_snapshot(), t0 + Duration::from_millis(200))
                .is_some()
        );
    }

    #[test]
    fn playback_fixture_new_output_epoch_requires_progress_with_retained_or_reset_counters() {
        let mut engine = playback_fixture_engine();
        engine.play().unwrap();
        let _ = next_fixture_video_at(&mut engine, Instant::now());
        let mut ring = clock_test_ring();
        clock_test_push(&mut ring, 0, 500);
        engine.sync_to_audio_clock_observed(ring.clock_snapshot(), Instant::now(), 1);
        clock_test_consume(&mut ring, 100);
        assert!(
            engine
                .sync_to_audio_clock_observed(ring.clock_snapshot(), Instant::now(), 1)
                .is_some()
        );
        ring.clear();
        clock_test_push(&mut ring, 2000, 500);
        assert!(ring.clock_snapshot().read_frames > 0);
        assert!(
            engine
                .sync_to_audio_clock_observed(ring.clock_snapshot(), Instant::now(), 2)
                .is_none()
        );
        assert!(
            engine
                .sync_to_audio_clock_observed(ring.clock_snapshot(), Instant::now(), 2)
                .is_none()
        );
        clock_test_consume(&mut ring, 10);
        assert!(
            engine
                .sync_to_audio_clock_observed(ring.clock_snapshot(), Instant::now(), 2)
                .unwrap()
                .snapped
        );

        // A replacement device may reset both counters and media time. Its new
        // epoch must deliberately establish a new baseline, then accept progress.
        let mut replacement = clock_test_ring();
        clock_test_push(&mut replacement, 0, 500);
        assert!(
            engine
                .sync_to_audio_clock_observed(replacement.clock_snapshot(), Instant::now(), 3)
                .is_none()
        );
        clock_test_consume(&mut replacement, 10);
        assert!(
            engine
                .sync_to_audio_clock_observed(replacement.clock_snapshot(), Instant::now(), 3)
                .is_some()
        );
    }

    #[test]
    fn playback_fixture_audio_clock_accounts_for_bounded_delivery_latency() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let mut ring = clock_test_ring();
        clock_test_push(&mut ring, 0, 300);
        engine.discipline_audio_observation(ring.clock_snapshot(), t0, t0);
        clock_test_consume(&mut ring, 100);
        let received_at = t0 + Duration::from_millis(400);
        let correction = engine
            .discipline_audio_observation(
                ring.clock_snapshot(),
                t0 + Duration::from_millis(100),
                received_at,
            )
            .unwrap();
        // Only 200 ms was queued at capture, so delivery prediction cannot
        // extend the media timestamp past the end of the 300 ms segment.
        assert_eq!(correction.drift, Duration::from_millis(100));
    }

    #[test]
    fn playback_fixture_audio_track_switch_restarts_at_current_media_time() {
        let mut engine = playback_fixture_engine();
        let audio_track_ids = engine
            .info()
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        assert_eq!(audio_track_ids, vec![1, 2]);

        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let first_track_audio = next_fixture_audio_at(&mut engine, t0);

        let switch_at = t0 + Duration::from_secs(2);
        engine
            .select_audio_track_at(Some(audio_track_ids[1]), switch_at)
            .unwrap();
        assert_eq!(engine.track_selection().audio, Some(audio_track_ids[1]));

        let switched_video = next_fixture_video_at(&mut engine, switch_at);
        let switched_audio = next_fixture_audio_at(&mut engine, switch_at);
        let switched_video_pts = switched_video.pts.expect("switched video PTS");
        let switched_audio_pts = switched_audio.pts.expect("switched audio PTS");
        assert!(switched_video_pts >= Duration::from_secs(2));
        assert!(switched_audio_pts >= Duration::from_secs(2));
        assert!(switched_video_pts - Duration::from_secs(2) <= Duration::from_millis(34));
        assert!(switched_audio_pts - Duration::from_secs(2) <= Duration::from_millis(34));

        let first_frequency = estimated_frequency_hz(&first_track_audio.frame);
        let switched_frequency = estimated_frequency_hz(&switched_audio.frame);
        assert!((800.0..=960.0).contains(&first_frequency));
        assert!(
            (1_200.0..=1_440.0).contains(&switched_frequency),
            "expected 1320 Hz track after switch, got {switched_frequency:.1} Hz"
        );
    }

    #[test]
    fn playback_fixture_stop_rewinds_and_replays_from_zero() {
        let mut engine = playback_fixture_engine();
        let second_audio_track = engine
            .info()
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
            .nth(1)
            .map(|track| track.id)
            .expect("second fixture audio track");
        let t0 = Instant::now();
        engine.play_at(t0);

        let midstream_target = Duration::from_millis(3_250);
        let midstream_at = t0 + Duration::from_millis(10);
        engine.seek_at(midstream_target, midstream_at).unwrap();
        let _ = next_fixture_av_at_target(&mut engine, midstream_at, midstream_target);
        engine
            .select_audio_track_at(Some(second_audio_track), midstream_at)
            .unwrap();
        engine.set_playback_rate_at(1.5, midstream_at);

        let stopped_at = midstream_at + Duration::from_millis(10);
        engine.stop_checked_at(stopped_at).unwrap();

        assert_eq!(engine.state(), PlaybackRunState::Stopped);
        assert_eq!(engine.media_time_at(stopped_at), Duration::ZERO);
        assert_eq!(
            engine.media_time_at(stopped_at + Duration::from_secs(60)),
            Duration::ZERO
        );
        assert_eq!(engine.track_selection().audio, Some(second_audio_track));
        assert_eq!(engine.clock_snapshot_at(stopped_at).rate, 1.5);

        let replay_at = stopped_at + Duration::from_secs(60);
        assert!(!engine.play_checked_at(replay_at).unwrap());
        let (_, audio) = next_fixture_av_at_target(&mut engine, replay_at, Duration::ZERO);
        let replay_frequency = estimated_frequency_hz(&audio.frame);
        assert!(
            (1_200.0..=1_440.0).contains(&replay_frequency),
            "expected preserved 1320 Hz track after stop, got {replay_frequency:.1} Hz"
        );
    }

    #[test]
    fn playback_fixture_play_from_eof_auto_rewinds() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        let (eof_at, _) = drive_fixture_to_eof_at(&mut engine, t0);
        assert_eq!(engine.state(), PlaybackRunState::Ended);

        let replay_at = eof_at + Duration::from_secs(60);
        assert!(engine.play_checked_at(replay_at).unwrap());

        assert_eq!(engine.state(), PlaybackRunState::Playing);
        assert_eq!(engine.media_time_at(replay_at), Duration::ZERO);
        let _ = next_fixture_av_at_target(&mut engine, replay_at, Duration::ZERO);
    }

    #[test]
    fn playback_fixture_seek_from_eof_recovers_for_nonzero_and_zero_targets() {
        for target in [Duration::from_millis(3_250), Duration::ZERO] {
            let mut engine = playback_fixture_engine();
            let t0 = Instant::now();
            let (eof_at, _) = drive_fixture_to_eof_at(&mut engine, t0);
            assert_eq!(engine.state(), PlaybackRunState::Ended);

            let seek_at = eof_at + Duration::from_secs(1);
            engine.seek_at(target, seek_at).unwrap();

            assert_eq!(engine.state(), PlaybackRunState::Stopped);
            assert_eq!(engine.media_time_at(seek_at), target);
            assert_eq!(
                engine.media_time_at(seek_at + Duration::from_secs(60)),
                target
            );

            let replay_at = seek_at + Duration::from_secs(60);
            assert!(!engine.play_checked_at(replay_at).unwrap());
            let _ = next_fixture_av_at_target(&mut engine, replay_at, target);
        }
    }

    #[test]
    fn playback_fixture_eof_ends_and_freezes_clock_at_duration() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        let (eof_at, last_video_pts) = drive_fixture_to_eof_at(&mut engine, t0);

        let duration = engine.info().duration.expect("fixture duration");
        assert_eq!(duration, Duration::from_secs(8));
        assert!(last_video_pts <= duration);
        assert!(
            duration - last_video_pts <= Duration::from_millis(34),
            "fixture ended before the final video frame: last={last_video_pts:?} duration={duration:?}",
        );
        assert_eq!(engine.media_time_at(eof_at), duration);
        assert_eq!(
            engine.media_time_at(eof_at + Duration::from_secs(60)),
            duration
        );
        assert_eq!(
            engine.clock_snapshot_at(eof_at + Duration::from_secs(60)),
            PlaybackClockSnapshot {
                media_time: duration,
                source: PlaybackClockSource::Wall,
                rate: 1.0,
            }
        );
        assert!(
            engine
                .tick_at(eof_at + Duration::from_secs(60))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn subtitle_queue_selects_ready_external_before_future_embedded() {
        let candidate = select_ready_subtitle(
            Some(Duration::from_secs(10)),
            [(3, Duration::from_secs(1))],
            Duration::from_secs(2),
        )
        .unwrap();

        assert!(matches!(
            candidate,
            SubtitleQueueCandidate::External {
                index: 3,
                start
            } if start == Duration::from_secs(1)
        ));
    }

    #[test]
    fn subtitle_queue_selects_earliest_ready_candidate() {
        let candidate = select_ready_subtitle(
            Some(Duration::from_secs(1)),
            [(0, Duration::from_secs(2))],
            Duration::from_secs(3),
        )
        .unwrap();

        assert!(matches!(
            candidate,
            SubtitleQueueCandidate::Embedded { start } if start == Duration::from_secs(1)
        ));
    }

    #[test]
    fn subtitle_queue_waits_when_no_candidate_is_ready() {
        assert!(
            select_ready_subtitle(
                Some(Duration::from_secs(5)),
                [(0, Duration::from_secs(6))],
                Duration::from_secs(4),
            )
            .is_none()
        );
    }

    #[test]
    fn external_subtitle_session_decodes_text_frames_with_external_track_id() {
        let path = std::env::temp_dir().join(format!(
            "erika-external-subtitle-{}.srt",
            std::process::id()
        ));
        fs::write(
            &path,
            "1\n00:00:01,000 --> 00:00:03,000\nExternal subtitle\n",
        )
        .unwrap();
        let config = SubtitleTrackConfig::external(1_000_007, path.to_string_lossy());

        let mut external = ExternalSubtitleSession::open(config).unwrap();
        external.pump_until(Duration::from_secs(2)).unwrap();
        let frame = external.pop_front().unwrap();

        assert_eq!(external.track().id, 1_000_007);
        assert_eq!(frame.track_id, 1_000_007);
        assert_eq!(frame.start, Some(Duration::from_secs(1)));
        assert_eq!(frame.end, Some(Duration::from_secs(3)));
        assert_eq!(frame.text.len(), 1);
        assert_eq!(frame.text[0].display_text(), "External subtitle");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn adding_external_subtitle_keeps_main_demux_generation() {
        let path = std::env::temp_dir().join(format!(
            "erika-external-subtitle-demux-continuity-{}.srt",
            std::process::id()
        ));
        fs::write(
            &path,
            "1\n00:00:00,000 --> 00:00:08,000\nExternal subtitle\n",
        )
        .unwrap();
        let mut engine = playback_fixture_engine();
        let demux_generation = engine.session.demuxer.generation;

        let (track, _) = engine
            .add_external_subtitle(SubtitleTrackConfig::external(
                1_000_008,
                path.to_string_lossy(),
            ))
            .unwrap();

        assert_eq!(track.id, 1_000_008);
        assert_eq!(
            engine.session.demuxer.generation, demux_generation,
            "a subtitle-only change must not discard queued A/V packets"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn external_subtitle_inspection_covers_query_uris_and_extensionless_uris() {
        assert!(external_subtitle_needs_charset_inspection("/tmp/movie.srt"));
        // Signed sidecar URLs keep their extension behind a query string.
        assert!(external_subtitle_needs_charset_inspection(
            "https://host/sub.srt?token=abc"
        ));
        // Android content picks arrive as a bare descriptor with no file name.
        assert!(external_subtitle_needs_charset_inspection(
            "fd://42?offset=0&length=4096"
        ));
        // Bitmap subtitle formats stay on the regular source path so their
        // bytes are never buffered for a charset guess.
        assert!(!external_subtitle_needs_charset_inspection(
            "/tmp/movie.sup"
        ));
        assert!(!external_subtitle_needs_charset_inspection(
            "https://host/movie.idx?token=abc"
        ));
    }

    #[test]
    fn external_subtitle_source_serves_utf8_bytes_without_reopening() {
        let path = std::env::temp_dir().join(format!(
            "erika-external-subtitle-utf8-{}.srt",
            std::process::id()
        ));
        let srt = "1\n00:00:01,000 --> 00:00:03,000\nalready utf-8\n";
        fs::write(&path, srt).unwrap();

        // Passthrough must still hand back a source: the bytes were already
        // consumed, and a one-shot descriptor could not be opened again.
        let source = external_subtitle_source(&path.to_string_lossy())
            .expect("passthrough must still yield a source");
        let mut source = source;
        let bytes = source
            .read_range(crate::source::ByteRange::suffix_from(0))
            .unwrap();
        assert_eq!(bytes, srt.as_bytes());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn external_subtitle_session_transcodes_gbk_encoded_file() {
        let path = std::env::temp_dir().join(format!(
            "erika-external-subtitle-gbk-{}.srt",
            std::process::id()
        ));
        let dialogue = "简体中文外挂字幕";
        let srt = format!("1\n00:00:01,000 --> 00:00:03,000\n{dialogue}\n");
        let (encoded, _, had_errors) = encoding_rs::GBK.encode(&srt);
        assert!(!had_errors);
        assert!(std::str::from_utf8(&encoded).is_err());
        fs::write(&path, &encoded).unwrap();
        let config = SubtitleTrackConfig::external(1_000_008, path.to_string_lossy());

        let mut external = ExternalSubtitleSession::open(config).unwrap();
        external.pump_until(Duration::from_secs(2)).unwrap();
        let frame = external.pop_front().unwrap();

        assert_eq!(frame.track_id, 1_000_008);
        assert_eq!(frame.start, Some(Duration::from_secs(1)));
        assert_eq!(frame.text.len(), 1);
        assert_eq!(frame.text[0].display_text(), dialogue);

        let _ = fs::remove_file(path);
    }

    fn pcm_frame(pts: Duration, frames: usize) -> PcmAudioFrame {
        PcmAudioFrame {
            format: PcmFormat::f32_interleaved(10, 2),
            pts: Some(pts),
            frames,
            samples: (0..frames * 2).map(|sample| sample as f32).collect(),
        }
    }

    #[test]
    fn audio_seek_floor_drops_pcm_ending_at_target() {
        let mut frame = pcm_frame(Duration::from_secs(9), 5);

        let action = trim_audio_frame_to_seek_floor(&mut frame, Duration::from_millis(9_500));

        assert_eq!(action, AudioSeekFloorAction::Drop);
    }

    #[test]
    fn audio_seek_floor_trims_overlapping_pcm_to_first_sample_at_or_after_target() {
        let mut frame = pcm_frame(Duration::from_secs(9), 10);

        let action = trim_audio_frame_to_seek_floor(&mut frame, Duration::from_millis(9_450));

        assert_eq!(action, AudioSeekFloorAction::Emit { trimmed_frames: 5 });
        assert_eq!(frame.pts, Some(Duration::from_millis(9_500)));
        assert_eq!(frame.frames, 5);
        assert_eq!(
            frame.samples,
            (10..20).map(|sample| sample as f32).collect::<Vec<_>>()
        );
    }

    #[test]
    fn audio_seek_floor_survives_dropped_frames_and_clears_on_emit() {
        let target = Duration::from_secs(10);
        let mut seek_floor = Some(target);
        let mut preroll = pcm_frame(Duration::from_millis(9_500), 5);
        let mut on_target = pcm_frame(target, 5);

        assert!(!keep_audio_frame_after_seek_floor(
            &mut seek_floor,
            &mut preroll
        ));
        assert_eq!(seek_floor, Some(target));
        assert!(keep_audio_frame_after_seek_floor(
            &mut seek_floor,
            &mut on_target
        ));
        assert_eq!(seek_floor, None);
    }

    #[test]
    fn bounded_audio_filter_preserves_remaining_queue_after_deadline() {
        let mut frames = VecDeque::from([
            pcm_frame(Duration::from_secs(9), 5),
            pcm_frame(Duration::from_millis(9_500), 5),
            pcm_frame(Duration::from_secs(10), 5),
        ]);
        let mut filtered_frames = 0;
        let mut inspected_frames = 0;
        let mut reject_all = |_: &mut PcmAudioFrame| false;

        let frame = pop_matching_audio_frame(
            &mut frames,
            &mut reject_all,
            &mut filtered_frames,
            &mut inspected_frames,
            Instant::now(),
            Duration::ZERO,
        );

        assert!(frame.is_none());
        assert_eq!(filtered_frames, 1);
        assert_eq!(inspected_frames, 1);
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn playback_clock_paused_seek_remains_frozen() {
        let t0 = Instant::now();
        let mut clock = PlaybackClock::paused_at(Duration::from_secs(10));

        clock.seek(Duration::from_secs(2), t0);

        assert!(!clock.is_running());
        assert_eq!(clock.media_time_at(t0), Duration::from_secs(2));
        assert_eq!(
            clock.media_time_at(t0 + Duration::from_secs(60)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn playback_clock_pause_freezes_accumulated_time() {
        let t0 = Instant::now();
        let mut clock = PlaybackClock::paused_at(Duration::from_secs(10));
        let pause_time = t0 + Duration::from_millis(500);

        clock.play(t0);
        clock.pause(pause_time);

        assert!(!clock.is_running());
        assert_eq!(
            clock.media_time_at(pause_time),
            Duration::from_millis(10_500)
        );
        assert_eq!(
            clock.media_time_at(pause_time + Duration::from_secs(60)),
            Duration::from_millis(10_500)
        );
    }

    #[test]
    fn buffering_freezes_clock_and_resumes_from_recovered_media_time() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let stalled_at = t0 + Duration::from_secs(2);

        assert!(engine.begin_buffering_at(stalled_at));
        assert!(engine.is_buffering());
        assert_eq!(engine.media_time_at(stalled_at), Duration::from_secs(2));
        assert_eq!(
            engine.media_time_at(stalled_at + Duration::from_secs(15)),
            Duration::from_secs(2)
        );

        let recovered_at = stalled_at + Duration::from_secs(15);
        assert!(engine.resume_buffering_at(
            Duration::from_millis(2_250),
            PlaybackClockSource::Audio,
            recovered_at,
        ));
        assert!(!engine.is_buffering());
        assert_eq!(
            engine.media_time_at(recovered_at),
            Duration::from_millis(2_250)
        );
        assert_eq!(
            engine.media_time_at(recovered_at + Duration::from_millis(50)),
            Duration::from_millis(2_300)
        );
    }

    #[test]
    fn buffering_drains_only_a_bounded_video_recovery_window() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let stalled_at = t0 + Duration::from_secs(2);
        assert!(engine.begin_buffering_at(stalled_at));

        let _ = next_fixture_video_at(&mut engine, stalled_at);
        let visible_buffering_pts = engine.buffering_video_pts();
        assert!(visible_buffering_pts.is_some());
        assert!(engine.pending_frame.is_none());
        assert_eq!(engine.last_buffering_video_drain_at, Some(stalled_at));

        let too_soon = stalled_at + BUFFERING_VIDEO_DRAIN_INTERVAL - Duration::from_millis(1);
        assert!(engine.tick_at(too_soon).unwrap().is_none());
        assert_eq!(engine.last_buffering_video_drain_at, Some(stalled_at));

        let next_drain = stalled_at + BUFFERING_VIDEO_DRAIN_INTERVAL;
        assert!(engine.tick_at(next_drain).unwrap().is_none());
        assert!(engine.pending_frame.is_none());
        assert_eq!(engine.buffering_video_pts(), visible_buffering_pts);
        assert_eq!(engine.last_buffering_video_drain_at, Some(next_drain));

        wait_for_buffering_video_to_park(&mut engine, next_drain);
        let parked = engine
            .pending_frame
            .as_ref()
            .expect("video must stop advancing once the recovery window is full");
        let parked_pts = parked
            .pts()
            .and_then(|pts| pts.as_duration())
            .expect("fixture video pts");
        assert!(
            engine.buffering_video_drained_frames == BUFFERING_VIDEO_MAX_DRAINED_FRAMES
                || parked_pts > engine.media_time_at(stalled_at) + BUFFERING_VIDEO_MAX_ADVANCE
        );
        assert!(engine.buffering_video_drained_frames <= BUFFERING_VIDEO_MAX_DRAINED_FRAMES);
    }

    #[test]
    fn buffering_video_recovery_stays_bounded_without_audio_progress() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let stalled_at = t0 + Duration::from_secs(2);
        assert!(engine.begin_buffering_at(stalled_at));

        engine.pending_audio = None;
        let _ = engine.session.discard_queued_audio_frames();
        engine.session.audio_decoder = None;

        let _ = next_fixture_video_at(&mut engine, stalled_at);
        assert!(engine.buffering_video_pts().is_some());
        let audio_scan_deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
        while engine.session.pending_video_packets.is_empty()
            && !engine.session.demux_eof
            && Instant::now() < audio_scan_deadline
        {
            let _ = engine.session.pump_once(PlaybackPumpDemand::Audio).unwrap();
            thread::yield_now();
        }
        assert!(
            !engine.session.pending_video_packets.is_empty(),
            "audio recovery scan must park compressed video before EOF (decoded={}, demux_eof={}, eof={})",
            engine.session.video_frames.len(),
            engine.session.demux_eof,
            engine.session.eof,
        );
        assert!(
            engine.session.pending_video_packets.len() <= AUDIO_DEMAND_PENDING_VIDEO_PACKET_LIMIT
        );
        assert!(!engine.session.demux_eof);

        wait_for_buffering_video_to_park(&mut engine, stalled_at);

        assert!(engine.pending_frame.is_some());
        assert!(engine.buffering_video_drained_frames <= BUFFERING_VIDEO_MAX_DRAINED_FRAMES);
        assert_eq!(engine.state(), PlaybackRunState::Playing);
        assert!(engine.is_buffering());
    }

    #[test]
    fn buffering_resume_allows_controlled_backward_reanchor() {
        let mut engine = playback_fixture_engine();
        let t0 = Instant::now();
        engine.play_at(t0);
        let _ = next_fixture_video_at(&mut engine, t0);
        let stalled_at = t0 + Duration::from_secs(10);

        assert!(engine.begin_buffering_at(stalled_at));
        assert!(engine.resume_buffering_at(
            Duration::from_secs(4),
            PlaybackClockSource::Audio,
            stalled_at + Duration::from_secs(1),
        ));
        assert_eq!(
            engine.media_time_at(stalled_at + Duration::from_secs(1)),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn activating_before_queued_pause_preserves_paused_seek_preview() {
        for starts_stopped in [false, true] {
            let mut engine = playback_fixture_engine();
            let t0 = Instant::now();
            if starts_stopped {
                engine.stop_checked_at(t0).unwrap();
                assert_eq!(engine.state(), PlaybackRunState::Stopped);
            }

            engine
                .play_checked_at(t0 + Duration::from_millis(1))
                .unwrap();
            engine.pause_at(t0 + Duration::from_millis(2));
            assert_eq!(engine.state(), PlaybackRunState::Paused);

            engine
                .seek_at(Duration::from_secs(2), t0 + Duration::from_millis(3))
                .unwrap();
            assert_eq!(engine.state(), PlaybackRunState::Paused);
            assert!(engine.has_pending_paused_seek_frame());
        }
    }

    #[test]
    fn playback_clock_paused_seek_then_resume_starts_from_new_target() {
        let t0 = Instant::now();
        let mut clock = PlaybackClock::running_at(Duration::from_secs(10), t0);
        let pause_time = t0 + Duration::from_millis(500);
        let seek_time = pause_time + Duration::from_secs(5);
        let resume_time = seek_time + Duration::from_secs(5);

        clock.pause(pause_time);
        clock.seek(Duration::from_secs(2), seek_time);

        assert!(!clock.is_running());
        assert_eq!(clock.media_time_at(resume_time), Duration::from_secs(2));

        clock.play(resume_time);

        assert_eq!(clock.media_time_at(resume_time), Duration::from_secs(2));
        assert_eq!(
            clock.media_time_at(resume_time + Duration::from_millis(250)),
            Duration::from_millis(2_250)
        );
    }

    #[test]
    fn playback_clock_running_seek_reanchors_elapsed_time() {
        let t0 = Instant::now();
        let mut clock = PlaybackClock::running_at(Duration::from_secs(10), t0);
        let seek_time = t0 + Duration::from_secs(3);

        clock.seek(Duration::from_secs(2), seek_time);

        assert!(clock.is_running());
        assert_eq!(clock.media_time_at(seek_time), Duration::from_secs(2));
        assert_eq!(
            clock.media_time_at(seek_time + Duration::from_millis(750)),
            Duration::from_millis(2_750)
        );
    }

    #[test]
    fn playback_clock_rate_change_is_continuous_and_scales_elapsed_time() {
        let t0 = Instant::now();
        let mut clock = PlaybackClock::running_at(Duration::from_secs(10), t0);
        let rate_change_time = t0 + Duration::from_millis(500);

        clock.set_rate(2.0, rate_change_time);

        assert_eq!(clock.rate(), 2.0);
        assert_eq!(
            clock.media_time_at(rate_change_time),
            Duration::from_millis(10_500)
        );
        assert_eq!(
            clock.media_time_at(rate_change_time + Duration::from_secs(1)),
            Duration::from_millis(12_500)
        );
    }

    #[test]
    fn playback_clock_invalid_rate_falls_back_to_one() {
        let t0 = Instant::now();

        for invalid_rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut clock = PlaybackClock::running_at(Duration::from_secs(10), t0);
            let rate_change_time = t0 + Duration::from_millis(500);

            clock.set_rate(invalid_rate, rate_change_time);

            assert_eq!(clock.rate(), 1.0);
            assert_eq!(
                clock.media_time_at(rate_change_time),
                Duration::from_millis(10_500)
            );
            assert_eq!(
                clock.media_time_at(rate_change_time + Duration::from_secs(1)),
                Duration::from_millis(11_500)
            );
        }
    }

    #[test]
    fn playback_clock_disciplines_toward_audio_master() {
        let t0 = Instant::now();
        let mut clock = PlaybackClock::running_at(Duration::from_secs(10), t0);
        let config = AudioSyncConfig {
            deadband: Duration::from_millis(5),
            max_correction_per_frame: Duration::from_millis(20),
            snap_threshold: Duration::from_millis(250),
            ..AudioSyncConfig::default()
        };

        let correction = clock.discipline_to(
            Duration::from_millis(10_100),
            t0 + Duration::from_millis(50),
            PlaybackClockSource::Audio,
            config,
        );

        assert_eq!(correction.source, PlaybackClockSource::Audio);
        assert_eq!(correction.direction, ClockCorrectionDirection::Forward);
        assert_eq!(correction.drift, Duration::from_millis(50));
        assert_eq!(correction.applied, Duration::from_millis(20));
        assert!(!correction.snapped);
        assert_eq!(clock.source(), PlaybackClockSource::Audio);
        assert_eq!(
            clock.media_time_at(t0 + Duration::from_millis(50)),
            Duration::from_millis(10_070)
        );
    }

    #[test]
    fn playback_clock_snaps_on_large_audio_drift() {
        let t0 = Instant::now();
        let mut clock = PlaybackClock::running_at(Duration::from_secs(10), t0);

        let correction = clock.discipline_to(
            Duration::from_secs(11),
            t0,
            PlaybackClockSource::Audio,
            AudioSyncConfig::default(),
        );

        assert_eq!(correction.direction, ClockCorrectionDirection::Forward);
        assert_eq!(correction.drift, Duration::from_secs(1));
        assert!(correction.snapped);
        assert_eq!(clock.media_time_at(t0), Duration::from_secs(11));
    }

    #[test]
    fn http_playback_uses_deeper_audio_lead() {
        let request = MediaRequest::new("https://example.invalid/video.mkv");
        let timing = playback_timing_for_request(&request, PlaybackTimingConfig::default());

        assert_eq!(timing.audio_lead_time, STREAMING_AUDIO_LEAD_TIME);
        assert_eq!(
            buffer_recovery_audio_for_request(&request),
            STREAMING_BUFFER_RECOVERY_AUDIO,
        );
    }

    #[test]
    fn custom_audio_lead_is_preserved_for_http_playback() {
        let request = MediaRequest::new("https://example.invalid/video.mkv");
        let custom = PlaybackTimingConfig {
            audio_lead_time: Duration::from_millis(750),
            ..PlaybackTimingConfig::default()
        };
        let timing = playback_timing_for_request(&request, custom);

        assert_eq!(timing.audio_lead_time, Duration::from_millis(750));
        assert_eq!(
            buffer_recovery_audio_for_request(&request),
            STREAMING_BUFFER_RECOVERY_AUDIO,
        );
    }

    #[test]
    fn local_playback_keeps_default_audio_lead() {
        let request = MediaRequest::new("/tmp/video.mkv");
        let timing = playback_timing_for_request(&request, PlaybackTimingConfig::default());

        assert_eq!(timing.audio_lead_time, DEFAULT_AUDIO_LEAD_TIME);
        assert_eq!(
            buffer_recovery_audio_for_request(&request),
            DEFAULT_BUFFER_RECOVERY_AUDIO,
        );
    }

    #[test]
    fn playback_session_recovery_threshold_survives_engine_construction() {
        let path = playback_fixture_path();
        let request = MediaRequest {
            uri: path.to_string_lossy().into_owned(),
            source_hint: MediaSourceHint::LocalFile,
            http_headers: Vec::new(),
            http_read_ahead_bytes: None,
            http_back_buffer_bytes: None,
        };
        let config = PlaybackSessionConfig {
            video_decode: VideoDecodePreference::Software,
            ..PlaybackSessionConfig::default()
        };
        let mut session = PlaybackSession::open(&request, config).unwrap();
        session.buffer_recovery_audio = STREAMING_BUFFER_RECOVERY_AUDIO;

        let engine = VideoPlaybackEngine::from_session(session);

        assert_eq!(
            engine.buffer_recovery_audio(),
            STREAMING_BUFFER_RECOVERY_AUDIO
        );
    }

    #[test]
    fn http_requests_get_deeper_playback_queue_limits() {
        let request = MediaRequest::new("https://example.invalid/video.mkv");
        let limits = PlaybackQueueLimits::for_request(&request);

        assert_eq!(limits.video_frames, STREAMING_VIDEO_FRAME_QUEUE_LIMIT);
        assert_eq!(limits.audio_frames, STREAMING_AUDIO_FRAME_QUEUE_LIMIT);
        assert_eq!(limits.subtitle_frames, SUBTITLE_FRAME_QUEUE_LIMIT);
    }

    #[test]
    fn local_requests_keep_default_playback_queue_limits() {
        let request = MediaRequest::new("/tmp/video.mkv");
        let limits = PlaybackQueueLimits::for_request(&request);

        assert_eq!(limits, PlaybackQueueLimits::default());
    }

    #[test]
    fn audio_demand_scans_past_only_a_bounded_pending_video_run() {
        assert!(audio_demand_may_scan_past_pending_video(
            PlaybackPumpDemand::Audio,
            1,
        ));
        assert!(audio_demand_may_scan_past_pending_video(
            PlaybackPumpDemand::Audio,
            AUDIO_DEMAND_PENDING_VIDEO_PACKET_LIMIT - 1,
        ));
        assert!(!audio_demand_may_scan_past_pending_video(
            PlaybackPumpDemand::Audio,
            AUDIO_DEMAND_PENDING_VIDEO_PACKET_LIMIT,
        ));
        assert!(!audio_demand_may_scan_past_pending_video(
            PlaybackPumpDemand::BufferingAudio,
            AUDIO_DEMAND_PENDING_VIDEO_PACKET_LIMIT,
        ));
        assert!(!audio_demand_may_scan_past_pending_video(
            PlaybackPumpDemand::Video,
            1,
        ));
    }

    #[test]
    fn audio_demand_stops_decoding_when_the_video_frame_queue_is_full() {
        assert!(!video_queue_blocks_audio_driven_decode(
            PlaybackPumpDemand::Audio,
            7,
            8,
        ));
        assert!(video_queue_blocks_audio_driven_decode(
            PlaybackPumpDemand::Audio,
            8,
            8,
        ));
        assert!(video_queue_blocks_audio_driven_decode(
            PlaybackPumpDemand::BufferingAudio,
            8,
            8,
        ));
        assert!(!video_queue_blocks_audio_driven_decode(
            PlaybackPumpDemand::Video,
            8,
            8,
        ));
    }

    #[test]
    fn buffering_audio_demand_crosses_a_full_pending_video_packet_run() {
        let mut engine = playback_fixture_engine();
        let deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
        while engine.session.pending_video_packets.len() < AUDIO_DEMAND_PENDING_VIDEO_PACKET_LIMIT
            && !engine.session.demux_eof
            && Instant::now() < deadline
        {
            engine.session.audio_frames.clear();
            let _ = engine.session.pump_once(PlaybackPumpDemand::Audio).unwrap();
            thread::yield_now();
        }
        assert_eq!(
            engine.session.pending_video_packets.len(),
            AUDIO_DEMAND_PENDING_VIDEO_PACKET_LIMIT,
            "fixture must reach the compressed-video cap before EOF"
        );
        assert_eq!(
            engine.session.video_frames.len(),
            engine.session.active_video_frame_queue_limit(),
            "decoded video backpressure must also be active"
        );

        engine.session.audio_frames.clear();
        let recovery_deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
        while engine.session.audio_frames.is_empty()
            && !engine.session.eof
            && Instant::now() < recovery_deadline
        {
            engine
                .session
                .pump_once(PlaybackPumpDemand::BufferingAudio)
                .unwrap();
            thread::yield_now();
        }

        assert!(
            !engine.session.audio_frames.is_empty(),
            "buffering recovery must scan beyond the 64-packet video run"
        );
        assert!(engine.session.buffering_audio_video_scan.discarded_frames > 0);
        assert!(
            engine.session.pending_video_packets.len() <= AUDIO_DEMAND_PENDING_VIDEO_PACKET_LIMIT
        );
    }

    #[test]
    fn buffering_audio_video_scan_becomes_nonfatal_at_its_packet_limit() {
        let mut engine = playback_fixture_engine();
        for _ in 0..BUFFERING_AUDIO_VIDEO_SCAN_PACKET_LIMIT {
            engine
                .session
                .observe_buffering_audio_video_packet(1, Duration::ZERO)
                .unwrap();
        }
        engine
            .session
            .observe_buffering_audio_video_packet(1, Duration::ZERO)
            .expect("a long video-only tail must not become a fatal playback error");
        assert!(engine.session.buffering_audio_recovery_suspended);
        assert!(!engine.has_audio_output());
    }

    #[test]
    fn buffering_audio_video_scan_charges_only_active_work() {
        let mut engine = playback_fixture_engine();
        engine.session.buffering_audio_video_scan.active_time =
            BUFFERING_AUDIO_VIDEO_SCAN_TIMEOUT - Duration::from_millis(1);

        // Arbitrarily long network idle produces no active work sample. The
        // next cheap packet must therefore remain below the watchdog limit.
        engine
            .session
            .observe_buffering_audio_video_packet(1, Duration::ZERO)
            .unwrap();

        engine
            .session
            .observe_buffering_audio_video_packet(1, Duration::from_millis(2))
            .expect("watchdog exhaustion must remain a nonfatal recovery state");
        assert!(engine.session.buffering_audio_recovery_suspended);
    }

    #[test]
    fn buffering_audio_progress_preserves_the_episode_video_advance_budget() {
        let mut engine = playback_fixture_engine();
        engine
            .session
            .begin_buffering_audio_video_scan(Duration::from_secs(10));
        engine.session.buffering_audio_video_scan.packets = 123;
        engine.session.buffering_audio_video_scan.bytes = 456;
        engine.session.buffering_audio_video_scan.active_time = Duration::from_secs(1);
        for _ in 0..7 {
            engine.session.record_buffering_video_advance();
        }

        engine.session.reset_buffering_audio_video_scan_progress();

        assert_eq!(engine.session.buffering_audio_video_scan.packets, 0);
        assert_eq!(engine.session.buffering_audio_video_scan.bytes, 0);
        assert_eq!(
            engine.session.buffering_audio_video_scan.active_time,
            Duration::ZERO
        );
        assert_eq!(
            engine.session.buffering_audio_video_scan.discarded_frames,
            7
        );
        assert_eq!(
            engine
                .session
                .buffering_audio_video_scan
                .baseline_media_time,
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn buffering_session_and_engine_share_one_video_advance_budget() {
        let mut engine = playback_fixture_engine();
        let baseline = Duration::from_secs(10);
        engine.session.begin_buffering_audio_video_scan(baseline);

        for _ in 0..BUFFERING_VIDEO_MAX_DRAINED_FRAMES {
            assert!(
                engine
                    .session
                    .buffering_video_advance_allowed(Some(baseline + Duration::from_millis(100)))
            );
            engine.session.record_buffering_video_advance();
        }
        assert!(
            !engine
                .session
                .buffering_video_advance_allowed(Some(baseline + Duration::from_millis(100)))
        );

        engine.session.begin_buffering_audio_video_scan(baseline);
        assert!(!engine.session.buffering_video_advance_allowed(Some(
            baseline + BUFFERING_VIDEO_MAX_ADVANCE + Duration::from_millis(1)
        )));
    }

    #[test]
    fn buffering_decoder_batch_trim_cannot_cross_the_shared_budget() {
        let baseline = Duration::from_secs(10);
        let frame_pts = [
            Some(baseline + Duration::from_millis(100)),
            Some(baseline + Duration::from_millis(200)),
            Some(baseline + Duration::from_millis(300)),
        ];

        assert_eq!(
            buffering_video_trim_droppable(
                frame_pts,
                BUFFERING_VIDEO_MAX_DRAINED_FRAMES - 1,
                Some(baseline),
            ),
            1,
            "a multi-frame decoder drain may only consume the remaining frame budget"
        );
        assert_eq!(
            buffering_video_trim_droppable(
                [
                    Some(baseline + Duration::from_millis(400)),
                    Some(baseline + BUFFERING_VIDEO_MAX_ADVANCE + Duration::from_millis(1)),
                    Some(baseline + Duration::from_millis(450)),
                ],
                0,
                Some(baseline),
            ),
            1,
            "trimming must stop at the first frame beyond the PTS advance budget"
        );
    }

    #[test]
    fn buffering_treats_a_finished_audio_decoder_as_exhausted() {
        let mut engine = playback_fixture_engine();
        let now = Instant::now();
        engine.play_at(now);
        assert!(engine.begin_buffering_at(now + Duration::from_millis(500)));

        engine.pending_audio = None;
        engine.session.audio_frames.clear();
        engine.session.demux_eof = true;
        engine.session.audio_decoder = None;

        assert!(engine.has_audio_output());
        assert!(engine.buffering_audio_is_exhausted());
    }

    #[test]
    fn nonfatal_audio_recovery_exhaustion_survives_resume_until_audio_returns() {
        let mut engine = playback_fixture_engine();
        let now = Instant::now();
        engine.play_at(now);
        assert!(engine.begin_buffering_at(now + Duration::from_millis(500)));
        engine.session.audio_frames.clear();
        for _ in 0..=BUFFERING_AUDIO_VIDEO_SCAN_PACKET_LIMIT {
            engine
                .session
                .observe_buffering_audio_video_packet(1, Duration::ZERO)
                .unwrap();
        }

        assert!(engine.buffering_audio_is_exhausted());
        assert!(engine.resume_buffering_at(
            Duration::from_secs(1),
            PlaybackClockSource::Wall,
            now + Duration::from_secs(1),
        ));
        assert!(engine.session.buffering_audio_recovery_suspended);
        assert!(!engine.has_audio_output());

        let deadline = Instant::now() + FIXTURE_WAIT_TIMEOUT;
        while engine.session.audio_frames.is_empty()
            && !engine.session.eof
            && Instant::now() < deadline
        {
            let _ = engine.session.pump_once(PlaybackPumpDemand::Audio).unwrap();
            thread::yield_now();
        }
        let recovered = engine
            .session
            .next_audio_frame_bounded(1, Duration::from_millis(10))
            .unwrap()
            .expect("fixture must eventually provide audio again");
        assert!(
            engine.session.buffering_audio_recovery_suspended,
            "decoding a sparse future frame must not re-enable starvation early"
        );
        engine.pending_audio = Some(recovered);
        assert!(
            engine
                .tick_audio_at(now + Duration::from_secs(60))
                .unwrap()
                .is_some(),
            "the recovered frame must become deliverable once its PTS is ready"
        );
        assert!(!engine.session.buffering_audio_recovery_suspended);
        assert!(engine.has_audio_output());
    }

    #[test]
    fn sparse_future_audio_resumes_wall_clock_without_rebuffering() {
        let mut engine = playback_fixture_engine();
        let now = Instant::now();
        engine.play_at(now);
        assert!(engine.begin_buffering_at(now));
        engine.pending_audio = Some(pcm_frame(Duration::from_secs(5), 1));

        assert!(engine.tick_audio_at(now).unwrap().is_none());
        assert!(engine.session.buffering_audio_recovery_suspended);
        assert!(engine.buffering_audio_is_exhausted());
        assert!(engine.resume_buffering_at(Duration::ZERO, PlaybackClockSource::Wall, now,));
        assert!(!engine.has_audio_output());

        assert!(
            engine
                .tick_audio_at(now + Duration::from_secs(5))
                .unwrap()
                .is_some()
        );
        assert!(!engine.session.buffering_audio_recovery_suspended);
        assert!(engine.has_audio_output());
    }

    #[test]
    fn full_audio_queue_only_blocks_audio_driven_demux() {
        assert!(audio_queue_blocks_demux(
            PlaybackPumpDemand::Audio,
            true,
            DEFAULT_AUDIO_FRAME_QUEUE_LIMIT,
            DEFAULT_AUDIO_FRAME_QUEUE_LIMIT,
        ));
        assert!(!audio_queue_blocks_demux(
            PlaybackPumpDemand::Video,
            true,
            DEFAULT_AUDIO_FRAME_QUEUE_LIMIT,
            DEFAULT_AUDIO_FRAME_QUEUE_LIMIT,
        ));
    }

    #[test]
    fn video_demand_audio_backpressure_is_relaxed_but_bounded() {
        // Wide enough to cross a container whose audio is interleaved several
        // seconds ahead of the next video packet...
        assert!(!audio_queue_blocks_demux(
            PlaybackPumpDemand::Video,
            true,
            VIDEO_DEMAND_AUDIO_FRAME_CEILING - 1,
            DEFAULT_AUDIO_FRAME_QUEUE_LIMIT,
        ));
        // ...but still a ceiling, so a stream that never yields another video
        // packet cannot grow the decoded audio queue without bound.
        assert!(audio_queue_blocks_demux(
            PlaybackPumpDemand::Video,
            true,
            VIDEO_DEMAND_AUDIO_FRAME_CEILING,
            DEFAULT_AUDIO_FRAME_QUEUE_LIMIT,
        ));
        // A streaming profile already allows more than the ceiling; keep its
        // own limit rather than tightening it.
        assert!(!audio_queue_blocks_demux(
            PlaybackPumpDemand::Video,
            true,
            VIDEO_DEMAND_AUDIO_FRAME_CEILING,
            VIDEO_DEMAND_AUDIO_FRAME_CEILING * 2,
        ));
        // Inactive audio output never blocks either demand.
        assert!(!audio_queue_blocks_demux(
            PlaybackPumpDemand::Audio,
            false,
            VIDEO_DEMAND_AUDIO_FRAME_CEILING * 4,
            DEFAULT_AUDIO_FRAME_QUEUE_LIMIT,
        ));
    }

    #[test]
    fn video_frame_scheduler_waits_presents_and_drops() {
        let scheduler =
            VideoFrameScheduler::new(Duration::from_millis(4), Duration::from_millis(80));

        assert_eq!(
            scheduler.schedule(
                Some(Duration::from_millis(110)),
                Duration::from_millis(100),
                false
            ),
            VideoFrameDecision::Wait {
                early_by: Duration::from_millis(6)
            }
        );
        assert_eq!(
            scheduler.schedule(
                Some(Duration::from_millis(98)),
                Duration::from_millis(100),
                false
            ),
            VideoFrameDecision::Present {
                late_by: Some(Duration::from_millis(2))
            }
        );
        assert_eq!(
            scheduler.schedule(
                Some(Duration::from_millis(10)),
                Duration::from_millis(100),
                false
            ),
            VideoFrameDecision::Drop {
                late_by: Duration::from_millis(90)
            }
        );
    }

    #[test]
    fn video_frame_scheduler_always_presents_first_frame() {
        let scheduler = VideoFrameScheduler::new(Duration::ZERO, Duration::from_millis(1));

        assert_eq!(
            scheduler.schedule(
                Some(Duration::from_millis(10)),
                Duration::from_millis(100),
                true
            ),
            VideoFrameDecision::Present {
                late_by: Some(Duration::from_millis(90))
            }
        );
    }

    #[test]
    fn display_sync_quantizes_frames_to_vsyncs_and_carries_error() {
        let mut state = DisplaySyncState::default();
        let config = DisplaySyncConfig::for_refresh_rate_hz(60.0);
        let first = state.schedule_frame(Duration::from_secs_f64(1.0 / 24.0), config);
        let second = state.schedule_frame(Duration::from_secs_f64(1.0 / 24.0), config);

        assert_eq!(first.vsyncs + second.vsyncs, 5);
        assert_ne!(first.vsyncs, second.vsyncs);
        assert!(first.residual_error_nanos.signum() != second.residual_error_nanos.signum());
    }

    #[test]
    fn dolby_vision_profile_5_stays_on_hardware_for_videotoolbox_and_d3d11va() {
        let vt_config = DecoderConfig {
            backend: DecoderBackend::VideoToolbox,
            mediacodec_surface: false,
        };
        let (config, reason) = dolby_vision_decode_fallback(Some(5), vt_config);
        assert_eq!(config.backend, DecoderBackend::VideoToolbox);
        assert_eq!(reason, None);

        let d3d11_config = DecoderConfig {
            backend: DecoderBackend::D3d11va,
            mediacodec_surface: false,
        };
        let (config, reason) = dolby_vision_decode_fallback(Some(5), d3d11_config);
        assert_eq!(config.backend, DecoderBackend::D3d11va);
        assert_eq!(reason, None);
    }

    #[test]
    fn dolby_vision_profile_5_falls_back_to_software_decode_on_mobile_backends() {
        let avcodec_config = DecoderConfig {
            backend: DecoderBackend::AvCodec,
            mediacodec_surface: false,
        };
        let (config, reason) = dolby_vision_decode_fallback(Some(5), avcodec_config);
        assert_eq!(config.backend, DecoderBackend::Software);
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("profile 5"));

        let mediacodec_config = DecoderConfig {
            backend: DecoderBackend::MediaCodec,
            mediacodec_surface: true,
        };
        let (config, reason) = dolby_vision_decode_fallback(Some(5), mediacodec_config);
        assert_eq!(config.backend, DecoderBackend::Software);
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("profile 5"));
    }

    #[test]
    fn dolby_vision_profile_8_stays_on_hardware_decode() {
        let hardware_config = DecoderConfig {
            backend: DecoderBackend::VideoToolbox,
            mediacodec_surface: false,
        };
        let (config, reason) = dolby_vision_decode_fallback(Some(8), hardware_config);

        assert_eq!(config.backend, DecoderBackend::VideoToolbox);
        assert_eq!(reason, None);
    }

    #[test]
    fn dolby_vision_software_decode_stays_software() {
        let software_config = DecoderConfig::software();
        let (config, reason) = dolby_vision_decode_fallback(Some(5), software_config);

        assert_eq!(config.backend, DecoderBackend::Software);
        assert_eq!(reason, None);
    }

    #[test]
    fn dovi_rpu_missing_is_diagnostic_only_on_profile_5() {
        assert!(dovi_rpu_diagnostic_warranted(
            Some(5),
            DoviRejectReason::MissingSideData
        ));
        assert!(!dovi_rpu_diagnostic_warranted(
            Some(8),
            DoviRejectReason::MissingSideData
        ));
        assert!(!dovi_rpu_diagnostic_warranted(
            None,
            DoviRejectReason::MissingSideData
        ));
        assert!(dovi_rpu_diagnostic_warranted(
            None,
            DoviRejectReason::UnsupportedCoefDenom
        ));
        assert!(dovi_rpu_diagnostic_warranted(
            Some(8),
            DoviRejectReason::InvalidCurves
        ));
    }

    #[test]
    fn dovi_rpu_diagnostics_report_first_then_spaced() {
        assert!(DoviRpuDiagnostics::should_report(1));
        assert!(!DoviRpuDiagnostics::should_report(2));
        assert!(!DoviRpuDiagnostics::should_report(
            DOVI_RPU_DIAGNOSTIC_INTERVAL - 1
        ));
        assert!(DoviRpuDiagnostics::should_report(
            DOVI_RPU_DIAGNOSTIC_INTERVAL
        ));
        assert!(!DoviRpuDiagnostics::should_report(
            DOVI_RPU_DIAGNOSTIC_INTERVAL + 1
        ));

        let mut diagnostics = DoviRpuDiagnostics::default();
        assert_eq!(diagnostics.record_unavailable_frame(), 1);
        assert_eq!(diagnostics.record_unavailable_frame(), 2);
        // The enhancement-layer counter is independent, so a stream that both
        // loses RPUs and carries FEL residual reports each at its own cadence.
        assert_eq!(diagnostics.record_el_frame(), 1);
        assert_eq!(diagnostics.record_unavailable_frame(), 3);
        assert_eq!(diagnostics.record_el_frame(), 2);
    }
}
