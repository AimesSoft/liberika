//! Headless media export jobs.
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
use crate::ffmpeg::{DecoderOutputFrame, Demuxer, Frame, StreamSelection};
use crate::source::source_from_uri_with_options;

const AVERROR_EOF: i32 = -541_478_725;

/// Upper bound on the frames one GIF export may produce (ten minutes at the
/// 60 fps maximum). The export is synchronous, so an absurd start/end section
/// would otherwise encode silently for hours.
const GIF_EXPORT_MAX_FRAME_COUNT: u64 = 36_000;

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
    #[error(
        "GIF export would produce {target_frame_count} frames, above the limit of {max_frame_count}"
    )]
    TooManyFrames {
        target_frame_count: u64,
        max_frame_count: u64,
    },
    #[error("I/O error for {path}: {message}")]
    Io { path: PathBuf, message: String },
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
    // fps and dimensions are already bounded, so the remaining runaway is the
    // export length: an unbounded section would encode frames for hours
    // before its synchronous caller noticed.
    let target_frame_count = ((options.end - options.start).as_secs_f64()
        * options.frames_per_second as f64)
        .ceil() as u64;
    if target_frame_count > GIF_EXPORT_MAX_FRAME_COUNT {
        return Err(ExportError::TooManyFrames {
            target_frame_count,
            max_frame_count: GIF_EXPORT_MAX_FRAME_COUNT,
        });
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
    fn options_reject_a_section_above_the_frame_count_limit() {
        let mut options = GifExportOptions::new(
            "/tmp/input.mp4",
            "/tmp/output.gif",
            Duration::ZERO,
            Duration::from_secs(601),
        );
        options.frames_per_second = 60;
        let error = validate_options(&options)
            .expect_err("a ten-minute-plus section must not reach the encoder");
        assert!(matches!(
            error,
            ExportError::TooManyFrames {
                target_frame_count: 36_060,
                max_frame_count: 36_000
            }
        ));
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
