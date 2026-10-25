//! Headless media export jobs: animated GIFs and single frames.
//!
//! This module deliberately does not construct a [`crate::Player`] or attach a
//! presentation surface. It owns a separate demuxer, software decoder and
//! encoder, so an export can run beside normal playback without sharing timing,
//! audio or renderer state.

use std::ffi::{CStr, CString};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use erika_ffmpeg_sys as sys;
use libc::EAGAIN;
use thiserror::Error;

use crate::core::{MediaRequest, TrackKind};
use crate::export_color::SdrRendition;
use crate::ffmpeg::{Decoder, DecoderOutputFrame, Demuxer, Frame, StreamSelection};
use crate::source::source_from_uri_with_options;

const AVERROR_EOF: i32 = -541_478_725;

#[derive(Debug, Clone, PartialEq)]
pub struct GifExportOptions {
    pub input: MediaRequest,
    pub output_path: PathBuf,
    pub start: Duration,
    pub end: Duration,
    pub frames_per_second: u32,
    /// Exact output width in pixels.
    pub output_width: u32,
    /// Exact output height in pixels.
    pub output_height: u32,
    pub quality: GifExportQuality,
    /// GIF loop count. Zero means infinite looping, matching FFmpeg's muxer.
    pub loop_count: i32,
    pub overwrite: bool,
}

impl GifExportOptions {
    pub fn new(
        input_uri: impl Into<String>,
        output_path: impl Into<PathBuf>,
        start: Duration,
        end: Duration,
    ) -> Self {
        Self {
            input: MediaRequest::new(input_uri),
            output_path: output_path.into(),
            start,
            end,
            frames_per_second: 10,
            output_width: 640,
            output_height: 360,
            quality: GifExportQuality::Normal,
            loop_count: 0,
            overwrite: false,
        }
    }
}

/// Scaling quality used before GIF palette encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GifExportQuality {
    /// Faster bilinear scaling, suitable for interactive previews.
    Normal,
    /// Higher quality Lanczos scaling, suitable for the final export.
    High,
}

impl GifExportQuality {
    fn scaler_flags(self) -> i32 {
        match self {
            Self::Normal => sys::ERIKA_SWS_BILINEAR,
            Self::High => sys::ERIKA_SWS_LANCZOS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GifExportResult {
    pub output_path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub frame_count: u64,
    pub file_size: u64,
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("invalid GIF export options: {0}")]
    InvalidOptions(String),
    #[error("output file already exists: {0}")]
    OutputExists(PathBuf),
    #[error("input has no video track")]
    NoVideoTrack,
    #[error("media source error: {0}")]
    Source(String),
    #[error("FFmpeg error: {0}")]
    Ffmpeg(String),
    #[error("I/O error for {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("invalid frame export options: {0}")]
    InvalidFrameOptions(String),
    #[error("position {position:?} is not before the end of the media ({duration:?})")]
    PastEnd {
        position: Duration,
        duration: Duration,
    },
    #[error("the video has no picture at or after the start")]
    NoPicture,
    #[error("PNG encoding failed: {0}")]
    Png(String),
}

pub type Result<T> = std::result::Result<T, ExportError>;

/// Exports a bounded section of a media source to an animated GIF.
///
/// The operation is synchronous. UI integrations must call it on a worker
/// thread or isolate. A temporary sibling file is renamed into place only after
/// the encoder and muxer have completed successfully.
pub fn export_gif(options: &GifExportOptions) -> Result<GifExportResult> {
    validate_options(options)?;
    if options.output_path.exists() && !options.overwrite {
        return Err(ExportError::OutputExists(options.output_path.clone()));
    }
    let parent = output_parent(&options.output_path);
    if !parent.is_dir() {
        return Err(ExportError::Io {
            path: parent.to_path_buf(),
            message: "parent directory does not exist".to_string(),
        });
    }

    let source = source_from_uri_with_options(
        &options.input.uri,
        options.input.source_hint,
        options.input.http_headers.clone(),
        options.input.http_read_ahead_bytes,
        // The export walks one bounded range forward, so it keeps the default
        // rewind budget: `ErikaGifExportOptions` exposes no knob for it.
        None,
    )
    .map_err(|error| ExportError::Source(error.to_string()))?;
    let mut demuxer = Demuxer::open_source(source).map_err(ffmpeg_error)?;
    let video = demuxer
        .probe()
        .video
        .first()
        .cloned()
        .ok_or(ExportError::NoVideoTrack)?;
    let stream_index = i32::try_from(video.track_id)
        .map_err(|_| ExportError::Ffmpeg("video stream index exceeds i32".to_string()))?;
    if !demuxer
        .probe()
        .tracks
        .iter()
        .any(|track| track.id == video.track_id && track.kind == TrackKind::Video)
    {
        return Err(ExportError::NoVideoTrack);
    }
    demuxer
        .set_stream_selection(StreamSelection::only([stream_index]))
        .map_err(ffmpeg_error)?;
    let mut decoder = demuxer.open_decoder(stream_index).map_err(ffmpeg_error)?;
    demuxer.seek(options.start).map_err(ffmpeg_error)?;
    decoder.flush();

    if video.params.width == 0 || video.params.height == 0 {
        return Err(ExportError::Ffmpeg(
            "video stream reported zero dimensions".to_string(),
        ));
    }
    let width = options.output_width;
    let height = options.output_height;
    let temporary_path = temporary_output_path(&options.output_path);
    let mut writer = GifWriter::new(
        &temporary_path,
        width,
        height,
        options.frames_per_second,
        options.loop_count,
        options.quality,
    )?;
    let start_seconds = options.start.as_secs_f64();
    let end_seconds = options.end.as_secs_f64();
    let duration = options.end - options.start;
    let frame_interval = 1.0 / options.frames_per_second as f64;
    let target_frame_count =
        (duration.as_secs_f64() * options.frames_per_second as f64).ceil() as u64;
    let mut next_sample = start_seconds;
    let mut reached_end = false;
    let mut last_frame = None;

    let export_result = (|| {
        while !reached_end {
            drain_decoder(
                &mut decoder,
                &mut writer,
                start_seconds,
                end_seconds,
                frame_interval,
                target_frame_count,
                &mut next_sample,
                &mut reached_end,
                &mut last_frame,
            )?;
            if reached_end {
                break;
            }
            let Some(packet) = demuxer.read_packet().map_err(ffmpeg_error)? else {
                decoder.send_eof().map_err(ffmpeg_error)?;
                drain_decoder(
                    &mut decoder,
                    &mut writer,
                    start_seconds,
                    end_seconds,
                    frame_interval,
                    target_frame_count,
                    &mut next_sample,
                    &mut reached_end,
                    &mut last_frame,
                )?;
                pad_to_target_frame_count(
                    &mut writer,
                    last_frame.as_ref(),
                    target_frame_count,
                    &mut next_sample,
                    frame_interval,
                )?;
                break;
            };
            decoder.send_packet(&packet).map_err(ffmpeg_error)?;
        }
        if writer.frame_count == 0 {
            return Err(ExportError::Ffmpeg(
                "the selected interval produced no video frames".to_string(),
            ));
        }
        writer.finish()?;
        Ok(writer.frame_count)
    })();

    let frame_count = match export_result {
        Ok(frame_count) => frame_count,
        Err(error) => {
            drop(writer);
            let _ = fs::remove_file(&temporary_path);
            return Err(error);
        }
    };
    drop(writer);
    commit_temporary_output(&temporary_path, &options.output_path, options.overwrite)?;
    let file_size = fs::metadata(&options.output_path)
        .map_err(|error| ExportError::Io {
            path: options.output_path.clone(),
            message: error.to_string(),
        })?
        .len();
    Ok(GifExportResult {
        output_path: options.output_path.clone(),
        width,
        height,
        frame_count,
        file_size,
    })
}

/// What [`export_frame`] returns the picture as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameExportFormat {
    /// RGBA8 rows of exactly `width * 4` bytes, top row first; alpha is 255.
    Rgba,
    /// A PNG file (8-bit RGB).
    Png,
}

impl FrameExportFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rgba => "rgba",
            Self::Png => "png",
        }
    }
}

/// Largest side [`export_frame`] accepts as a bound.
pub const FRAME_EXPORT_MAX_DIMENSION: u32 = 16_384;

#[derive(Debug, Clone, PartialEq)]
pub struct FrameExportOptions {
    pub input: MediaRequest,
    /// The picture shown at this media time is exported.
    pub position: Duration,
    /// Largest output width in pixels; zero leaves the width unbounded.
    pub max_width: u32,
    /// Largest output height in pixels; zero leaves the height unbounded.
    /// The picture keeps the video's display aspect ratio and is never larger
    /// than the video, so two zeros give the video's own size.
    pub max_height: u32,
    pub format: FrameExportFormat,
}

impl FrameExportOptions {
    pub fn new(input_uri: impl Into<String>, position: Duration) -> Self {
        Self {
            input: MediaRequest::new(input_uri),
            position,
            max_width: 0,
            max_height: 0,
            format: FrameExportFormat::Png,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedFrame {
    pub width: u32,
    pub height: u32,
    pub format: FrameExportFormat,
    /// RGBA rows or a PNG file, as `format` says.
    pub data: Vec<u8>,
    /// Presentation time of the picture: the last frame at or before the
    /// requested position (the first frame when the position precedes it).
    pub timestamp: Duration,
    /// The video's decoded size.
    pub source_width: u32,
    pub source_height: u32,
    /// The media's duration when the container reports one.
    pub duration: Option<Duration>,
    /// The source is HDR (PQ, HLG or Dolby Vision) and the picture was tone
    /// mapped to SDR the way Erika renders it on an SDR display.
    pub tone_mapped: bool,
}

/// Decodes the picture shown at `options.position` and returns it scaled to
/// the requested bounds as RGBA or PNG.
///
/// A one-picture [`FrameExportSession`]: it opens its own demuxer and a
/// (multi-threaded) software decoder, seeks to the keyframe before the
/// position and decodes forward, so it never touches a player. The operation
/// is synchronous; UI integrations must call it on a worker thread or isolate.
pub fn export_frame(options: &FrameExportOptions) -> Result<ExportedFrame> {
    validate_frame_options(options)?;
    let mut session = FrameExportSession::open(&options.input)?;
    session.frame_at(&FrameRequest {
        position: options.position,
        max_width: options.max_width,
        max_height: options.max_height,
        format: options.format,
    })
}

/// One picture asked of a [`FrameExportSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRequest {
    /// The picture shown at this media time is exported.
    pub position: Duration,
    /// Bounds as in [`FrameExportOptions`]: zero leaves a side unbounded; the
    /// display aspect ratio is kept and the picture is never enlarged.
    pub max_width: u32,
    pub max_height: u32,
    pub format: FrameExportFormat,
}

impl FrameRequest {
    pub fn new(position: Duration) -> Self {
        Self {
            position,
            max_width: 0,
            max_height: 0,
            format: FrameExportFormat::Png,
        }
    }
}

/// Counters of a [`FrameExportSession`] since it opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameExportStats {
    /// Pictures returned.
    pub pictures: u64,
    /// Seeks to a keyframe, the first picture's included.
    pub seeks: u64,
    /// Video frames the decoder produced.
    pub decoded_frames: u64,
}

/// Without a seek index, a request at most this far past the decoded
/// position is reached by decoding forward. The keyframe interval seen in the
/// stream raises it.
const FORWARD_DECODE_WITHOUT_INDEX: f64 = 2.0;

/// A keyframe at most this far (seconds) past the decoded position is
/// decoded through rather than sought to.
const KEYFRAME_SEEK_MARGIN: f64 = 0.5;

/// Pictures of one media taken one after another with one demuxer and one
/// decoder.
///
/// [`export_frame`] opens the media, seeks to the keyframe before the
/// position and decodes up to it for every picture. A session keeps both
/// open: a request after the previous one continues decoding from where the
/// decoder is when no keyframe lies between that point and the request (the
/// demuxer's seek index tells; without one, when it is close), and seeks
/// otherwise, backwards included. Requests in ascending time order are the
/// cheapest. A session is used by one thread at a time; it is `Send`.
pub struct FrameExportSession {
    demuxer: Demuxer,
    decoder: Decoder,
    duration: Option<Duration>,
    source_width: u32,
    source_height: u32,
    /// The newest decoded frame at or before the previous request.
    shown: Option<Frame>,
    /// The decoded frame after `shown`, when the decoder produced it.
    ahead: Option<Frame>,
    /// The decoder continues from a seek (false when opened or after an
    /// error, so the next request seeks).
    positioned: bool,
    eof_sent: bool,
    finished: bool,
    /// Presentation time of the last keyframe packet read, and the longest
    /// interval between two keyframes seen.
    last_keyframe: Option<f64>,
    keyframe_interval: f64,
    stats: FrameExportStats,
}

impl FrameExportSession {
    /// Opens the media's first video stream with a multi-threaded software
    /// decoder.
    pub fn open(input: &MediaRequest) -> Result<Self> {
        if input.uri.is_empty() {
            return Err(ExportError::InvalidFrameOptions(
                "input URI must not be empty".to_string(),
            ));
        }
        let source = source_from_uri_with_options(
            &input.uri,
            input.source_hint,
            input.http_headers.clone(),
            input.http_read_ahead_bytes,
            None,
        )
        .map_err(|error| ExportError::Source(error.to_string()))?;
        let mut demuxer = Demuxer::open_source(source).map_err(ffmpeg_error)?;
        let duration = demuxer.probe().duration;
        let video = demuxer
            .probe()
            .video
            .first()
            .cloned()
            .ok_or(ExportError::NoVideoTrack)?;
        let stream_index = i32::try_from(video.track_id)
            .map_err(|_| ExportError::Ffmpeg("video stream index exceeds i32".to_string()))?;
        demuxer
            .set_stream_selection(StreamSelection::only([stream_index]))
            .map_err(ffmpeg_error)?;
        let decoder = Decoder::open_software_threaded(
            demuxer
                .codec_parameters(stream_index)
                .map_err(ffmpeg_error)?,
        )
        .map_err(ffmpeg_error)?;
        Ok(Self {
            demuxer,
            decoder,
            duration,
            source_width: video.params.width,
            source_height: video.params.height,
            shown: None,
            ahead: None,
            positioned: false,
            eof_sent: false,
            finished: false,
            last_keyframe: None,
            keyframe_interval: 0.0,
            stats: FrameExportStats::default(),
        })
    }

    /// The media's duration when the container reports one.
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// The video stream's coded size as the container reports it.
    pub fn video_size(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    pub fn stats(&self) -> FrameExportStats {
        self.stats
    }

    /// The picture shown at `request.position`, scaled and encoded as asked.
    pub fn frame_at(&mut self, request: &FrameRequest) -> Result<ExportedFrame> {
        if request.max_width > FRAME_EXPORT_MAX_DIMENSION
            || request.max_height > FRAME_EXPORT_MAX_DIMENSION
        {
            return Err(ExportError::InvalidFrameOptions(format!(
                "width and height must not exceed {FRAME_EXPORT_MAX_DIMENSION}"
            )));
        }
        if let Some(duration) = self.duration
            && request.position >= duration
        {
            return Err(ExportError::PastEnd {
                position: request.position,
                duration,
            });
        }
        // Half a millisecond absorbs time-base rounding of a frame exactly at
        // the position.
        let target = request.position.as_secs_f64() + 0.000_5;
        let result = (|| {
            if !self.continues_to(request.position, target) {
                self.seek(request.position)?;
            }
            self.advance_to(target)?;
            let picture = self.shown.as_ref().ok_or(ExportError::NoPicture)?;
            render_picture(picture, request, self.duration)
        })();
        match result {
            Ok(frame) => {
                self.stats.pictures += 1;
                Ok(frame)
            }
            Err(error) => {
                // Start over from a seek next time.
                self.positioned = false;
                self.shown = None;
                self.ahead = None;
                Err(error)
            }
        }
    }

    /// Whether decoding on from the current state reaches `target` at least
    /// as cheaply as a seek.
    fn continues_to(&self, position: Duration, target: f64) -> bool {
        if !self.positioned {
            return false;
        }
        let seconds = |frame: &Frame| frame.pts().map(|pts| pts.seconds());
        if self
            .shown
            .as_ref()
            .and_then(seconds)
            .is_some_and(|shown| target < shown)
        {
            // Behind the picture already passed.
            return false;
        }
        let Some(newest) = self
            .ahead
            .as_ref()
            .or(self.shown.as_ref())
            .and_then(seconds)
        else {
            return false;
        };
        if self.finished || target <= newest {
            return true;
        }
        match self.demuxer.keyframe_at_or_before(position) {
            // A keyframe well after the decoded position: seeking to it skips
            // decoding everything before it. Just ahead, decoding the few
            // frames up to it costs less than restarting the decoder.
            Some(keyframe) => keyframe.as_secs_f64() <= newest + KEYFRAME_SEEK_MARGIN,
            None => {
                target - newest <= FORWARD_DECODE_WITHOUT_INDEX.max(self.keyframe_interval / 2.0)
            }
        }
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        self.positioned = false;
        self.shown = None;
        self.ahead = None;
        self.demuxer.seek(position).map_err(ffmpeg_error)?;
        self.decoder.flush();
        self.eof_sent = false;
        self.finished = false;
        self.last_keyframe = None;
        self.positioned = true;
        self.stats.seeks += 1;
        Ok(())
    }

    /// Decodes until `shown` is the last frame at or before `target` (or the
    /// first frame, when `target` precedes it).
    fn advance_to(&mut self, target: f64) -> Result<()> {
        let seconds = |frame: &Frame| frame.pts().map(|pts| pts.seconds());
        loop {
            if let Some(ahead_seconds) = self.ahead.as_ref().and_then(seconds) {
                if ahead_seconds <= target {
                    self.shown = self.ahead.take();
                    continue;
                }
                if self.shown.is_none() {
                    // The position precedes the first picture.
                    self.shown = self.ahead.take();
                }
                return Ok(());
            }
            if self.finished {
                return if self.shown.is_some() {
                    Ok(())
                } else {
                    Err(ExportError::NoPicture)
                };
            }
            match self.decoder.receive_frame().map_err(ffmpeg_error)? {
                DecoderOutputFrame::Frame(frame) => {
                    self.stats.decoded_frames += 1;
                    if frame.pts().is_some() {
                        self.ahead = Some(frame);
                    }
                }
                DecoderOutputFrame::NeedMoreInput if self.eof_sent => self.finished = true,
                DecoderOutputFrame::NeedMoreInput => {
                    match self.demuxer.read_packet().map_err(ffmpeg_error)? {
                        Some(packet) => {
                            if packet.is_key()
                                && let Some(pts) = packet.pts().map(|pts| pts.seconds())
                            {
                                if let Some(previous) = self.last_keyframe
                                    && pts > previous
                                {
                                    self.keyframe_interval =
                                        self.keyframe_interval.max(pts - previous);
                                }
                                self.last_keyframe = Some(pts);
                            }
                            self.decoder.send_packet(&packet).map_err(ffmpeg_error)?;
                        }
                        None => {
                            self.decoder.send_eof().map_err(ffmpeg_error)?;
                            self.eof_sent = true;
                        }
                    }
                }
                DecoderOutputFrame::EndOfStream => self.finished = true,
            }
        }
    }
}

/// Scales (and for HDR sources tone maps) a decoded picture and encodes it.
fn render_picture(
    picture: &Frame,
    request: &FrameRequest,
    duration: Option<Duration>,
) -> Result<ExportedFrame> {
    let (width, height) = frame_output_size(picture, request.max_width, request.max_height)?;
    let rendition = SdrRendition::for_frame(picture);
    let rgba = match &rendition {
        Some(rendition) => rendition
            .render(picture, width, height)
            .map_err(ExportError::Ffmpeg)?,
        None => scale_frame_to_rgba(picture, width, height)?,
    };
    let data = match request.format {
        FrameExportFormat::Rgba => rgba,
        FrameExportFormat::Png => encode_png_rgb(&rgba, width, height)?,
    };
    Ok(ExportedFrame {
        width,
        height,
        format: request.format,
        data,
        timestamp: picture
            .pts()
            .and_then(|pts| pts.as_duration())
            .unwrap_or_default(),
        source_width: picture.width(),
        source_height: picture.height(),
        duration,
        tone_mapped: rendition.is_some(),
    })
}

fn validate_frame_options(options: &FrameExportOptions) -> Result<()> {
    if options.input.uri.is_empty() {
        return Err(ExportError::InvalidFrameOptions(
            "input URI must not be empty".to_string(),
        ));
    }
    if options.max_width > FRAME_EXPORT_MAX_DIMENSION
        || options.max_height > FRAME_EXPORT_MAX_DIMENSION
    {
        return Err(ExportError::InvalidFrameOptions(format!(
            "width and height must not exceed {FRAME_EXPORT_MAX_DIMENSION}"
        )));
    }
    Ok(())
}

/// The output size: the frame's display size (sample aspect ratio applied)
/// fitted into the bounds, never enlarged.
fn frame_output_size(frame: &Frame, max_width: u32, max_height: u32) -> Result<(u32, u32)> {
    let (coded_width, coded_height) = (frame.width(), frame.height());
    if coded_width == 0 || coded_height == 0 {
        return Err(ExportError::Ffmpeg(
            "decoded frame reported zero dimensions".to_string(),
        ));
    }
    let aspect = unsafe { (*frame.as_ptr()).sample_aspect_ratio };
    let display_width = if aspect.num > 0 && aspect.den > 0 && aspect.num != aspect.den {
        (coded_width as f64 * aspect.num as f64 / aspect.den as f64).round()
    } else {
        coded_width as f64
    };
    Ok(fit_within(
        display_width,
        coded_height as f64,
        max_width,
        max_height,
    ))
}

fn fit_within(width: f64, height: f64, max_width: u32, max_height: u32) -> (u32, u32) {
    let mut scale = 1.0_f64;
    if max_width > 0 {
        scale = scale.min(max_width as f64 / width);
    }
    if max_height > 0 {
        scale = scale.min(max_height as f64 / height);
    }
    (
        ((width * scale).round() as u32).max(1),
        ((height * scale).round() as u32).max(1),
    )
}

/// Converts with swscale's frame API, which reads the source's colour space
/// and range from the frame. An unlabelled YUV source is taken as BT.709
/// when it is HD or larger and BT.601 otherwise, as players do.
fn scale_frame_to_rgba(frame: &Frame, width: u32, height: u32) -> Result<Vec<u8>> {
    let source = OwnedAvFrame::alloc()?;
    let output = OwnedAvFrame::alloc()?;
    let scaler = OwnedSwsContext::alloc()?;
    unsafe {
        check(
            sys::av_frame_ref(source.0, frame.as_ptr()),
            "av_frame_ref(frame export)",
        )?;
        let descriptor = sys::av_pix_fmt_desc_get((*source.0).format);
        let rgb_source =
            !descriptor.is_null() && (*descriptor).flags & sys::AV_PIX_FMT_FLAG_RGB as u64 != 0;
        if !rgb_source {
            if (*source.0).colorspace == sys::AVColorSpace_AVCOL_SPC_UNSPECIFIED {
                (*source.0).colorspace = if (*source.0).width >= 1280 || (*source.0).height > 576 {
                    sys::AVColorSpace_AVCOL_SPC_BT709
                } else {
                    sys::AVColorSpace_AVCOL_SPC_SMPTE170M
                };
            }
            if (*source.0).color_range == sys::AVColorRange_AVCOL_RANGE_UNSPECIFIED {
                (*source.0).color_range = sys::AVColorRange_AVCOL_RANGE_MPEG;
            }
        }
        (*output.0).format = sys::AVPixelFormat_AV_PIX_FMT_RGBA;
        (*output.0).width = width as i32;
        (*output.0).height = height as i32;
        (*output.0).color_range = sys::AVColorRange_AVCOL_RANGE_JPEG;
        check(
            sys::av_frame_get_buffer(output.0, 0),
            "av_frame_get_buffer(frame export)",
        )?;
        (*scaler.0).flags = (sys::SwsFlags_SWS_BICUBIC
            | sys::SwsFlags_SWS_ACCURATE_RND
            | sys::SwsFlags_SWS_FULL_CHR_H_INT) as _;
        // 0: one thread per core.
        (*scaler.0).threads = 0;
        check(
            sys::sws_scale_frame(scaler.0, output.0, source.0),
            "sws_scale_frame(frame export)",
        )?;
        let row_bytes = width as usize * 4;
        let stride = (*output.0).linesize[0];
        if stride < row_bytes as i32 {
            return Err(ExportError::Ffmpeg(format!(
                "RGBA output stride {stride} is shorter than a row of {row_bytes} bytes"
            )));
        }
        let mut rgba = Vec::with_capacity(row_bytes * height as usize);
        let base = (*output.0).data[0];
        for row in 0..height as usize {
            rgba.extend_from_slice(std::slice::from_raw_parts(
                base.add(row * stride as usize),
                row_bytes,
            ));
        }
        Ok(rgba)
    }
}

fn encode_png_rgb(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let rgb: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    let mut encoded = Vec::new();
    let mut encoder = png::Encoder::new(&mut encoded, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut writer = encoder
        .write_header()
        .map_err(|error| ExportError::Png(error.to_string()))?;
    writer
        .write_image_data(&rgb)
        .map_err(|error| ExportError::Png(error.to_string()))?;
    writer
        .finish()
        .map_err(|error| ExportError::Png(error.to_string()))?;
    Ok(encoded)
}

struct OwnedAvFrame(*mut sys::AVFrame);

impl OwnedAvFrame {
    fn alloc() -> Result<Self> {
        let frame = unsafe { sys::av_frame_alloc() };
        if frame.is_null() {
            return Err(ExportError::Ffmpeg(
                "av_frame_alloc returned null".to_string(),
            ));
        }
        Ok(Self(frame))
    }
}

impl Drop for OwnedAvFrame {
    fn drop(&mut self) {
        unsafe { sys::av_frame_free(&mut self.0) };
    }
}

struct OwnedSwsContext(*mut sys::SwsContext);

impl OwnedSwsContext {
    fn alloc() -> Result<Self> {
        let context = unsafe { sys::sws_alloc_context() };
        if context.is_null() {
            return Err(ExportError::Ffmpeg(
                "sws_alloc_context returned null".to_string(),
            ));
        }
        Ok(Self(context))
    }
}

impl Drop for OwnedSwsContext {
    fn drop(&mut self) {
        unsafe { sys::sws_free_context(&mut self.0) };
    }
}

fn validate_options(options: &GifExportOptions) -> Result<()> {
    if options.input.uri.is_empty() {
        return Err(ExportError::InvalidOptions(
            "input URI must not be empty".to_string(),
        ));
    }
    if options.output_path.as_os_str().is_empty() {
        return Err(ExportError::InvalidOptions(
            "output path must not be empty".to_string(),
        ));
    }
    if options.end <= options.start {
        return Err(ExportError::InvalidOptions(
            "end must be greater than start".to_string(),
        ));
    }
    if !(1..=60).contains(&options.frames_per_second) {
        return Err(ExportError::InvalidOptions(
            "frames_per_second must be in 1..=60".to_string(),
        ));
    }
    if options.output_width == 0 || options.output_height == 0 {
        return Err(ExportError::InvalidOptions(
            "output dimensions must be non-zero".to_string(),
        ));
    }
    if options.output_width > 8192 || options.output_height > 8192 {
        return Err(ExportError::InvalidOptions(
            "output dimensions must not exceed 8192".to_string(),
        ));
    }
    if options.loop_count < -1 || options.loop_count > u16::MAX as i32 {
        return Err(ExportError::InvalidOptions(
            "loop_count must be -1, 0, or a positive 16-bit value".to_string(),
        ));
    }
    Ok(())
}

fn temporary_output_path(output: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let name = output
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export.gif".to_string());
    output.with_file_name(format!(".{name}.erika-{}-{stamp}.part", std::process::id()))
}

fn output_parent(output: &Path) -> &Path {
    match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn commit_temporary_output(temporary: &Path, output: &Path, overwrite: bool) -> Result<()> {
    if overwrite {
        return replace_file(temporary, output);
    }
    fs::hard_link(temporary, output).map_err(|error| {
        if error.kind() == ErrorKind::AlreadyExists {
            ExportError::OutputExists(output.to_path_buf())
        } else {
            ExportError::Io {
                path: output.to_path_buf(),
                message: error.to_string(),
            }
        }
    })?;
    // The output link is already committed atomically. Failure to remove the
    // private sibling name must not turn a successful export into a failure.
    let _ = fs::remove_file(temporary);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn replace_file(temporary: &Path, output: &Path) -> Result<()> {
    fs::rename(temporary, output).map_err(|error| ExportError::Io {
        path: output.to_path_buf(),
        message: error.to_string(),
    })
}

#[cfg(target_os = "windows")]
fn replace_file(temporary: &Path, output: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = output
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| ExportError::Io {
        path: output.to_path_buf(),
        message: error.to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
fn drain_decoder(
    decoder: &mut crate::ffmpeg::Decoder,
    writer: &mut GifWriter,
    start_seconds: f64,
    end_seconds: f64,
    frame_interval: f64,
    target_frame_count: u64,
    next_sample: &mut f64,
    reached_end: &mut bool,
    last_frame: &mut Option<Frame>,
) -> Result<()> {
    loop {
        match decoder.receive_frame().map_err(ffmpeg_error)? {
            DecoderOutputFrame::Frame(frame) => {
                let Some(timestamp) = frame.pts().map(|pts| pts.seconds()) else {
                    continue;
                };
                if timestamp < start_seconds {
                    continue;
                }
                if timestamp >= end_seconds {
                    pad_to_target_frame_count(
                        writer,
                        last_frame.as_ref(),
                        target_frame_count,
                        next_sample,
                        frame_interval,
                    )?;
                    *reached_end = true;
                    return Ok(());
                }
                *last_frame = Some(frame);
                while writer.frame_count < target_frame_count
                    && timestamp + frame_interval * 0.5 >= *next_sample
                {
                    writer.write_frame(last_frame.as_ref().expect("frame was just stored"))?;
                    *next_sample += frame_interval;
                }
                if writer.frame_count >= target_frame_count {
                    *reached_end = true;
                    return Ok(());
                }
            }
            DecoderOutputFrame::NeedMoreInput | DecoderOutputFrame::EndOfStream => return Ok(()),
        }
    }
}

fn pad_to_target_frame_count(
    writer: &mut GifWriter,
    last_frame: Option<&Frame>,
    target_frame_count: u64,
    next_sample: &mut f64,
    frame_interval: f64,
) -> Result<()> {
    let Some(frame) = last_frame else {
        return Ok(());
    };
    while writer.frame_count < target_frame_count {
        writer.write_frame(frame)?;
        *next_sample += frame_interval;
    }
    Ok(())
}

struct GifWriter {
    format: *mut sys::AVFormatContext,
    codec: *mut sys::AVCodecContext,
    frame: *mut sys::AVFrame,
    packet: *mut sys::AVPacket,
    scaler: *mut sys::SwsContext,
    stream_index: i32,
    stream_time_base: sys::AVRational,
    width: i32,
    height: i32,
    frame_count: u64,
    packet_count: u64,
    scaler_flags: i32,
    finished: bool,
}

impl GifWriter {
    fn new(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        loop_count: i32,
        quality: GifExportQuality,
    ) -> Result<Self> {
        let path = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| {
            ExportError::InvalidOptions("output path contains an interior NUL".to_string())
        })?;
        let mut writer = Self {
            format: ptr::null_mut(),
            codec: ptr::null_mut(),
            frame: ptr::null_mut(),
            packet: ptr::null_mut(),
            scaler: ptr::null_mut(),
            stream_index: 0,
            stream_time_base: sys::AVRational {
                num: 1,
                den: fps as i32,
            },
            width: width as i32,
            height: height as i32,
            frame_count: 0,
            packet_count: 0,
            scaler_flags: quality.scaler_flags(),
            finished: false,
        };
        unsafe {
            check(
                sys::avformat_alloc_output_context2(
                    &mut writer.format,
                    ptr::null_mut(),
                    c"gif".as_ptr(),
                    path.as_ptr(),
                ),
                "avformat_alloc_output_context2",
            )?;
            if writer.format.is_null() {
                return Err(ExportError::Ffmpeg(
                    "avformat_alloc_output_context2 returned null".to_string(),
                ));
            }
            let encoder = sys::avcodec_find_encoder(sys::AVCodecID_AV_CODEC_ID_GIF);
            if encoder.is_null() {
                return Err(ExportError::Ffmpeg(
                    "GIF encoder is unavailable; rebuild Erika native dependencies".to_string(),
                ));
            }
            let stream = sys::avformat_new_stream(writer.format, ptr::null());
            if stream.is_null() {
                return Err(ExportError::Ffmpeg(
                    "avformat_new_stream returned null".to_string(),
                ));
            }
            writer.stream_index = (*stream).index;
            writer.codec = sys::avcodec_alloc_context3(encoder);
            if writer.codec.is_null() {
                return Err(ExportError::Ffmpeg(
                    "avcodec_alloc_context3 returned null".to_string(),
                ));
            }
            (*writer.codec).codec_id = sys::AVCodecID_AV_CODEC_ID_GIF;
            (*writer.codec).codec_type = sys::AVMediaType_AVMEDIA_TYPE_VIDEO;
            (*writer.codec).width = writer.width;
            (*writer.codec).height = writer.height;
            (*writer.codec).pix_fmt = sys::AVPixelFormat_AV_PIX_FMT_RGB8;
            (*writer.codec).time_base = writer.stream_time_base;
            (*writer.codec).framerate = sys::AVRational {
                num: fps as i32,
                den: 1,
            };
            if !(*writer.format).oformat.is_null()
                && (*(*writer.format).oformat).flags & sys::AVFMT_GLOBALHEADER as i32 != 0
            {
                (*writer.codec).flags |= sys::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            }
            check(
                sys::avcodec_open2(writer.codec, encoder, ptr::null_mut()),
                "avcodec_open2(gif)",
            )?;
            check(
                sys::avcodec_parameters_from_context((*stream).codecpar, writer.codec),
                "avcodec_parameters_from_context(gif)",
            )?;
            (*stream).time_base = writer.stream_time_base;
            if (*(*writer.format).oformat).flags & sys::AVFMT_NOFILE as i32 == 0 {
                check(
                    sys::avio_open(
                        &mut (*writer.format).pb,
                        path.as_ptr(),
                        sys::AVIO_FLAG_WRITE as i32,
                    ),
                    "avio_open(gif)",
                )?;
            }
            let mut muxer_options = ptr::null_mut();
            let loop_value = CString::new(loop_count.to_string()).expect("integer contains no NUL");
            check(
                sys::av_dict_set(&mut muxer_options, c"loop".as_ptr(), loop_value.as_ptr(), 0),
                "av_dict_set(loop)",
            )?;
            let final_delay =
                CString::new((100_u32.div_ceil(fps)).to_string()).expect("integer contains no NUL");
            check(
                sys::av_dict_set(
                    &mut muxer_options,
                    c"final_delay".as_ptr(),
                    final_delay.as_ptr(),
                    0,
                ),
                "av_dict_set(final_delay)",
            )?;
            let header_result = sys::avformat_write_header(writer.format, &mut muxer_options);
            sys::av_dict_free(&mut muxer_options);
            check(header_result, "avformat_write_header(gif)")?;
            // The GIF muxer normalizes its stream to a 1/100 second time base
            // in gif_init(). Packet timestamps must be rescaled to that final
            // value, not the encoder's 1/fps time base cached before the header.
            writer.stream_time_base = (*stream).time_base;

            writer.frame = sys::av_frame_alloc();
            if writer.frame.is_null() {
                return Err(ExportError::Ffmpeg(
                    "av_frame_alloc returned null".to_string(),
                ));
            }
            (*writer.frame).format = sys::AVPixelFormat_AV_PIX_FMT_RGB8 as i32;
            (*writer.frame).width = writer.width;
            (*writer.frame).height = writer.height;
            check(
                sys::av_frame_get_buffer(writer.frame, 32),
                "av_frame_get_buffer(gif)",
            )?;
            writer.packet = sys::av_packet_alloc();
            if writer.packet.is_null() {
                return Err(ExportError::Ffmpeg(
                    "av_packet_alloc returned null".to_string(),
                ));
            }
        }
        Ok(writer)
    }

    fn write_frame(&mut self, source: &Frame) -> Result<()> {
        unsafe {
            check(
                sys::av_frame_make_writable(self.frame),
                "av_frame_make_writable(gif)",
            )?;
            self.scaler = sys::sws_getCachedContext(
                self.scaler,
                source.width() as i32,
                source.height() as i32,
                source.raw_pixel_format(),
                self.width,
                self.height,
                sys::AVPixelFormat_AV_PIX_FMT_RGB8,
                self.scaler_flags,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if self.scaler.is_null() {
                return Err(ExportError::Ffmpeg(
                    "sws_getCachedContext returned null".to_string(),
                ));
            }
            let source = source.as_ptr();
            let scaled = sys::sws_scale(
                self.scaler,
                (*source).data.as_ptr() as *const *const u8,
                (*source).linesize.as_ptr(),
                0,
                (*source).height,
                (*self.frame).data.as_mut_ptr(),
                (*self.frame).linesize.as_mut_ptr(),
            );
            if scaled != self.height {
                return Err(ExportError::Ffmpeg(format!(
                    "sws_scale converted {scaled} rows, expected {}",
                    self.height
                )));
            }
            (*self.frame).pts = self.frame_count as i64;
            check(
                sys::avcodec_send_frame(self.codec, self.frame),
                "avcodec_send_frame(gif)",
            )?;
            self.drain_packets()?;
            self.frame_count += 1;
        }
        Ok(())
    }

    fn drain_packets(&mut self) -> Result<()> {
        unsafe {
            loop {
                let code = sys::avcodec_receive_packet(self.codec, self.packet);
                if code == av_error(EAGAIN) || code == AVERROR_EOF {
                    return Ok(());
                }
                check(code, "avcodec_receive_packet(gif)")?;
                sys::av_packet_rescale_ts(
                    self.packet,
                    (*self.codec).time_base,
                    self.stream_time_base,
                );
                (*self.packet).stream_index = self.stream_index;
                self.packet_count += 1;
                let write_result = sys::av_interleaved_write_frame(self.format, self.packet);
                sys::av_packet_unref(self.packet);
                check(write_result, "av_interleaved_write_frame(gif)")?;
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        unsafe {
            check(
                sys::avcodec_send_frame(self.codec, ptr::null()),
                "avcodec_send_frame_eof(gif)",
            )?;
            self.drain_packets()?;
            let trailer = sys::av_write_trailer(self.format);
            if trailer < 0 {
                return Err(ExportError::Ffmpeg(format!(
                    "av_write_trailer(gif) failed after {} input frames and {} encoded packets: {}",
                    self.frame_count,
                    self.packet_count,
                    ffmpeg_error_message(trailer),
                )));
            }
        }
        self.finished = true;
        Ok(())
    }
}

impl Drop for GifWriter {
    fn drop(&mut self) {
        unsafe {
            if !self.packet.is_null() {
                sys::av_packet_free(&mut self.packet);
            }
            if !self.frame.is_null() {
                sys::av_frame_free(&mut self.frame);
            }
            if !self.scaler.is_null() {
                sys::sws_freeContext(self.scaler);
                self.scaler = ptr::null_mut();
            }
            if !self.codec.is_null() {
                sys::avcodec_free_context(&mut self.codec);
            }
            if !self.format.is_null() {
                if !(*self.format).pb.is_null()
                    && !(*self.format).oformat.is_null()
                    && (*(*self.format).oformat).flags & sys::AVFMT_NOFILE as i32 == 0
                {
                    let _ = sys::avio_closep(&mut (*self.format).pb);
                }
                sys::avformat_free_context(self.format);
                self.format = ptr::null_mut();
            }
        }
    }
}

fn av_error(error: i32) -> i32 {
    -error
}

fn check(code: i32, operation: &'static str) -> Result<()> {
    if code >= 0 {
        return Ok(());
    }
    let message = ffmpeg_error_message(code);
    Err(ExportError::Ffmpeg(format!(
        "{operation}: {message} ({code})"
    )))
}

fn ffmpeg_error_message(code: i32) -> String {
    // `c_char` is signed on most supported targets but unsigned on OpenHarmony.
    // Keep the buffer's element type aligned with the target ABI rather than
    // assuming `i8` so both FFmpeg and `CStr` receive compatible pointers.
    let mut buffer = [0 as std::ffi::c_char; 256];
    unsafe {
        if sys::av_strerror(code, buffer.as_mut_ptr(), buffer.len()) >= 0 {
            CStr::from_ptr(buffer.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            format!("unknown FFmpeg error ({code})")
        }
    }
}

fn ffmpeg_error(error: crate::ffmpeg::FfmpegError) -> ExportError {
    ExportError::Ffmpeg(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should follow the Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "erika-export-{name}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("test directory should be created");
        directory
    }

    fn playback_fixture() -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/playback/playback-fixture.mkv")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn frame_bounds_keep_the_aspect_ratio_and_never_enlarge() {
        assert_eq!(fit_within(4096.0, 1744.0, 640, 640), (640, 273));
        assert_eq!(fit_within(4096.0, 1744.0, 0, 360), (846, 360));
        assert_eq!(fit_within(4096.0, 1744.0, 0, 0), (4096, 1744));
        assert_eq!(fit_within(160.0, 90.0, 3840, 0), (160, 90));
        assert_eq!(fit_within(90.0, 160.0, 64, 64), (36, 64));
    }

    #[test]
    fn exports_the_frame_shown_at_a_position_as_png() {
        let mut options = FrameExportOptions::new(playback_fixture(), Duration::from_millis(2_500));
        options.max_width = 80;
        let frame = export_frame(&options).unwrap();
        assert_eq!((frame.width, frame.height), (80, 45));
        assert_eq!((frame.source_width, frame.source_height), (160, 90));
        assert_eq!(frame.format, FrameExportFormat::Png);
        assert_eq!(frame.duration, Some(Duration::from_secs(8)));
        // The last frame at or before 2.5 s (30 fps).
        assert!(
            frame.timestamp <= Duration::from_millis(2_500),
            "{:?}",
            frame.timestamp
        );
        assert!(
            frame.timestamp > Duration::from_millis(2_450),
            "{:?}",
            frame.timestamp
        );
        let decoder = png::Decoder::new(std::io::Cursor::new(&frame.data));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (80, 45));
        assert_eq!(reader.info().color_type, png::ColorType::Rgb);
    }

    #[test]
    fn exports_rgba_at_the_video_size_and_differs_between_times() {
        let start = export_frame(&FrameExportOptions {
            format: FrameExportFormat::Rgba,
            ..FrameExportOptions::new(playback_fixture(), Duration::ZERO)
        })
        .unwrap();
        assert_eq!((start.width, start.height), (160, 90));
        assert_eq!(start.data.len(), 160 * 90 * 4);
        assert!(start.data.chunks_exact(4).all(|pixel| pixel[3] == 255));
        let later = export_frame(&FrameExportOptions {
            format: FrameExportFormat::Rgba,
            ..FrameExportOptions::new(playback_fixture(), Duration::from_secs(6))
        })
        .unwrap();
        assert!(later.timestamp > start.timestamp);
        assert_ne!(start.data, later.data);
    }

    #[test]
    fn frame_export_refuses_positions_past_the_end_and_oversized_bounds() {
        let error = export_frame(&FrameExportOptions::new(
            playback_fixture(),
            Duration::from_secs(8),
        ))
        .unwrap_err();
        assert!(matches!(error, ExportError::PastEnd { .. }), "{error}");
        let mut options = FrameExportOptions::new(playback_fixture(), Duration::ZERO);
        options.max_width = FRAME_EXPORT_MAX_DIMENSION + 1;
        assert!(matches!(
            export_frame(&options).unwrap_err(),
            ExportError::InvalidFrameOptions(_)
        ));
    }

    fn single(position: Duration, format: FrameExportFormat) -> ExportedFrame {
        export_frame(&FrameExportOptions {
            format,
            max_width: 64,
            ..FrameExportOptions::new(playback_fixture(), position)
        })
        .unwrap()
    }

    #[test]
    fn a_session_decodes_forward_inside_a_keyframe_interval() {
        // The fixture has a keyframe every second at 30 fps.
        let mut session = FrameExportSession::open(&MediaRequest::new(playback_fixture())).unwrap();
        assert_eq!(session.duration(), Some(Duration::from_secs(8)));
        assert_eq!(session.video_size(), (160, 90));
        for millis in (100..=900).step_by(100) {
            let position = Duration::from_millis(millis);
            let frame = session
                .frame_at(&FrameRequest {
                    max_width: 64,
                    format: FrameExportFormat::Rgba,
                    ..FrameRequest::new(position)
                })
                .unwrap();
            let expected = single(position, FrameExportFormat::Rgba);
            assert_eq!(frame.timestamp, expected.timestamp, "{millis} ms");
            assert_eq!(frame.data, expected.data, "{millis} ms");
            assert!(!frame.tone_mapped);
        }
        let stats = session.stats();
        assert_eq!(stats.pictures, 9);
        assert_eq!(stats.seeks, 1, "{stats:?}");
        assert!(stats.decoded_frames < 9 * 10, "{stats:?}");
    }

    #[test]
    fn a_session_seeks_past_keyframes_and_backwards_and_survives_errors() {
        let mut session = FrameExportSession::open(&MediaRequest::new(playback_fixture())).unwrap();
        let mut take = |millis: u64| {
            session
                .frame_at(&FrameRequest {
                    max_width: 64,
                    format: FrameExportFormat::Rgba,
                    ..FrameRequest::new(Duration::from_millis(millis))
                })
                .map(|frame| (frame.timestamp, frame.data))
        };
        for millis in [500, 3_500, 3_600, 1_200, 1_200, 0] {
            let expected = single(Duration::from_millis(millis), FrameExportFormat::Rgba);
            assert_eq!(
                take(millis).unwrap(),
                (expected.timestamp, expected.data),
                "{millis} ms"
            );
        }
        assert!(matches!(take(9_000), Err(ExportError::PastEnd { .. })));
        let expected = single(Duration::from_millis(7_990), FrameExportFormat::Rgba);
        assert_eq!(take(7_990).unwrap(), (expected.timestamp, expected.data));
        // 0.5 s; 3.5 s (keyframe at 3 s); 3.6 s forward; 1.2 s backwards;
        // 1.2 s again; 0 backwards; 7.99 s (keyframe at 7 s).
        assert_eq!(session.stats().seeks, 5, "{:?}", session.stats());
        assert_eq!(session.stats().pictures, 7);
    }

    #[test]
    fn decode_thread_override_parses_counts_and_auto() {
        assert_eq!(crate::ffmpeg::software_video_decode_threads_from(None), 0);
        assert_eq!(
            crate::ffmpeg::software_video_decode_threads_from(Some("auto")),
            0
        );
        assert_eq!(
            crate::ffmpeg::software_video_decode_threads_from(Some(" 1 ")),
            1
        );
        assert_eq!(
            crate::ffmpeg::software_video_decode_threads_from(Some("8")),
            8
        );
        assert_eq!(
            crate::ffmpeg::software_video_decode_threads_from(Some("-3")),
            0
        );
        assert_eq!(
            crate::ffmpeg::software_video_decode_threads_from(Some("lots")),
            0
        );
    }

    #[test]
    fn defaults_map_to_ui_export_controls() {
        let options = GifExportOptions::new(
            "/tmp/input.mp4",
            "/tmp/output.gif",
            Duration::from_secs(1),
            Duration::from_secs(5),
        );
        assert_eq!(options.frames_per_second, 10);
        assert_eq!((options.output_width, options.output_height), (640, 360));
        assert_eq!(options.quality, GifExportQuality::Normal);
        assert!(!options.overwrite);
        assert_eq!(options.loop_count, 0);
    }

    #[test]
    fn quality_selects_scaler_algorithm() {
        assert_eq!(
            GifExportQuality::Normal.scaler_flags(),
            sys::ERIKA_SWS_BILINEAR
        );
        assert_eq!(
            GifExportQuality::High.scaler_flags(),
            sys::ERIKA_SWS_LANCZOS
        );
    }

    #[test]
    fn options_reject_invalid_interval_and_unbounded_frame_rate() {
        let mut options = GifExportOptions::new(
            "/tmp/input.mp4",
            "/tmp/output.gif",
            Duration::ZERO,
            Duration::ZERO,
        );
        assert!(validate_options(&options).is_err());
        options.end = Duration::from_secs(1);
        options.frames_per_second = 61;
        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn relative_output_uses_the_current_directory() {
        assert_eq!(output_parent(Path::new("output.gif")), Path::new("."));
        assert_eq!(
            output_parent(Path::new("exports/output.gif")),
            Path::new("exports")
        );
    }

    #[test]
    fn no_clobber_commit_preserves_a_racing_destination() {
        let directory = test_directory("no-clobber");
        let temporary = directory.join("temporary.gif");
        let output = directory.join("output.gif");
        fs::write(&temporary, b"new").expect("temporary output should be written");
        fs::write(&output, b"existing").expect("racing output should be written");

        let error = commit_temporary_output(&temporary, &output, false)
            .expect_err("no-clobber commit must reject an existing output");
        assert!(matches!(error, ExportError::OutputExists(path) if path == output));
        assert_eq!(fs::read(&output).unwrap(), b"existing");
        assert_eq!(fs::read(&temporary).unwrap(), b"new");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn overwrite_commit_replaces_the_destination() {
        let directory = test_directory("overwrite");
        let temporary = directory.join("temporary.gif");
        let output = directory.join("output.gif");
        fs::write(&temporary, b"new").expect("temporary output should be written");
        fs::write(&output, b"existing").expect("existing output should be written");

        commit_temporary_output(&temporary, &output, true)
            .expect("overwrite commit should replace the destination");
        assert_eq!(fs::read(&output).unwrap(), b"new");
        assert!(!temporary.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
