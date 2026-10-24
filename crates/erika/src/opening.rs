//! The first seconds of a video, measured: a few numbers for each sampled
//! frame, enough to tell a service's or a studio's ident before the episode
//! (a red letter on black, a logo on white) from the episode itself. Like
//! an export, it runs beside playback with a demuxer and software decoder
//! of its own; call it on a worker thread.

use std::ptr;
use std::time::Duration;

use erika_ffmpeg_sys as sys;

use crate::core::TrackKind;
use crate::ffmpeg::{DecoderOutputFrame, Demuxer, Frame, StreamSelection};
use crate::source::source_from_uri;

/// Frames are measured at this size, which is plenty for averages.
const WIDTH: i32 = 64;
const HEIGHT: i32 = 36;

/// One sampled frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct OpeningFrame {
    /// Seconds from the start.
    pub time: f64,
    /// The average color, from 0 to 1.
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub luma: f32,
    /// Shares of the frame, from 0 to 1: nearly black, nearly white, and a
    /// saturated red.
    pub dark: f32,
    pub white: f32,
    pub red_share: f32,
}

/// The frames of the first `seconds` of the video at `uri`, `per_second`
/// of them each second.
pub fn opening_profile(uri: &str, seconds: f64, per_second: f64) -> Result<Vec<OpeningFrame>, String> {
    let seconds = seconds.clamp(0.5, 60.0);
    let interval = 1.0 / per_second.clamp(0.5, 30.0);
    let source = source_from_uri(uri).map_err(|error| error.to_string())?;
    let mut demuxer = Demuxer::open_source(source).map_err(|error| error.to_string())?;
    let video = demuxer
        .probe()
        .video
        .first()
        .cloned()
        .ok_or("the media has no video")?;
    if !demuxer
        .probe()
        .tracks
        .iter()
        .any(|track| track.id == video.track_id && track.kind == TrackKind::Video)
    {
        return Err("the media has no video".into());
    }
    let stream = i32::try_from(video.track_id).map_err(|_| "video stream index exceeds i32")?;
    demuxer
        .set_stream_selection(StreamSelection::only([stream]))
        .map_err(|error| error.to_string())?;
    let mut decoder = demuxer.open_decoder(stream).map_err(|error| error.to_string())?;
    demuxer.seek(Duration::ZERO).map_err(|error| error.to_string())?;
    decoder.flush();

    let mut measure = Measure::new();
    let mut frames = Vec::new();
    let mut next = 0.0;
    let mut first: Option<f64> = None;
    loop {
        let mut ended = false;
        loop {
            match decoder.receive_frame().map_err(|error| error.to_string())? {
                DecoderOutputFrame::Frame(frame) => {
                    let Some(time) = frame.pts().map(|pts| pts.seconds()) else {
                        continue;
                    };
                    // Times count from the first frame, whatever the
                    // container's start time.
                    let start = *first.get_or_insert(time);
                    let time = time - start;
                    if time >= seconds {
                        return Ok(frames);
                    }
                    if time + interval * 0.5 >= next {
                        frames.push(measure.frame(&frame, time)?);
                        next += interval;
                    }
                }
                DecoderOutputFrame::NeedMoreInput => break,
                DecoderOutputFrame::EndOfStream => {
                    ended = true;
                    break;
                }
            }
        }
        if ended {
            return Ok(frames);
        }
        match demuxer.read_packet().map_err(|error| error.to_string())? {
            Some(packet) => decoder.send_packet(&packet).map_err(|error| error.to_string())?,
            None => decoder.send_eof().map_err(|error| error.to_string())?,
        }
    }
}

/// Scales frames down to RGB and averages them.
struct Measure {
    scaler: *mut sys::SwsContext,
    pixels: Vec<u8>,
}

impl Measure {
    fn new() -> Self {
        Measure {
            scaler: ptr::null_mut(),
            pixels: vec![0; (WIDTH * HEIGHT * 3) as usize],
        }
    }

    fn frame(&mut self, frame: &Frame, time: f64) -> Result<OpeningFrame, String> {
        unsafe {
            self.scaler = sys::sws_getCachedContext(
                self.scaler,
                frame.width() as i32,
                frame.height() as i32,
                frame.raw_pixel_format(),
                WIDTH,
                HEIGHT,
                sys::AVPixelFormat_AV_PIX_FMT_RGB24,
                sys::SwsFlags_SWS_AREA as i32,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if self.scaler.is_null() {
                return Err("sws_getCachedContext returned null".into());
            }
            let source = frame.as_ptr();
            let destination = [self.pixels.as_mut_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
            let strides = [WIDTH * 3, 0, 0, 0];
            let rows = sys::sws_scale(
                self.scaler,
                (*source).data.as_ptr() as *const *const u8,
                (*source).linesize.as_ptr(),
                0,
                (*source).height,
                destination.as_ptr(),
                strides.as_ptr(),
            );
            if rows != HEIGHT {
                return Err(format!("sws_scale converted {rows} rows, expected {HEIGHT}"));
            }
        }
        Ok(measure(&self.pixels, time))
    }
}

impl Drop for Measure {
    fn drop(&mut self) {
        unsafe { sys::sws_freeContext(self.scaler) };
    }
}

/// The numbers of one RGB24 frame.
fn measure(pixels: &[u8], time: f64) -> OpeningFrame {
    let count = (pixels.len() / 3).max(1) as f32;
    let mut sums = [0f32; 3];
    let (mut dark, mut white, mut red) = (0f32, 0f32, 0f32);
    for pixel in pixels.chunks_exact(3) {
        let [r, g, b] = [pixel[0], pixel[1], pixel[2]].map(|c| c as f32 / 255.0);
        sums[0] += r;
        sums[1] += g;
        sums[2] += b;
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        if luma < 0.08 {
            dark += 1.0;
        }
        if r > 0.85 && g > 0.85 && b > 0.85 {
            white += 1.0;
        }
        if r > 0.45 && g < 0.25 && b < 0.25 {
            red += 1.0;
        }
    }
    let [r, g, b] = sums.map(|sum| sum / count);
    OpeningFrame {
        time,
        red: r,
        green: g,
        blue: b,
        luma: 0.2126 * r + 0.7152 * g + 0.0722 * b,
        dark: dark / count,
        white: white / count,
        red_share: red / count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_measure_their_shares() {
        // A black frame with a red quarter.
        let mut pixels = vec![0u8; 16 * 3];
        for pixel in pixels.chunks_exact_mut(3).take(4) {
            pixel.copy_from_slice(&[229, 9, 20]);
        }
        let frame = measure(&pixels, 1.0);
        assert_eq!(frame.dark, 0.75);
        assert_eq!(frame.red_share, 0.25);
        assert_eq!(frame.white, 0.0);
        assert!(frame.red > frame.green && frame.red > frame.blue);
    }
}

#[cfg(test)]
mod real {
    /// `ERIKA_OPENING_SAMPLE=/path/video.mkv cargo test -p erika opening::real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn profile_a_real_video() {
        let path = std::env::var("ERIKA_OPENING_SAMPLE").expect("ERIKA_OPENING_SAMPLE");
        let uri = format!("file://{path}");
        for frame in super::opening_profile(&uri, 8.0, 2.0).unwrap() {
            println!(
                "{:5.2}s luma {:.2} rgb {:.2} {:.2} {:.2} dark {:.2} white {:.2} red {:.2}",
                frame.time, frame.luma, frame.red, frame.green, frame.blue, frame.dark, frame.white, frame.red_share
            );
        }
    }
}
