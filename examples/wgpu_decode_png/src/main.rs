//! Decodes a video frame with liberika (software path) and renders it through the
//! wgpu backend's `upload_player_frame` -> `render_current_offscreen` path, writing
//! the result to a PNG. End-to-end proof that wgpu renders real decoded frames.
//!
//! usage: cargo run -p wgpu_decode_png -- <media> [out.png] [frame-index]

use std::env;
use std::process;
use std::time::Duration;

use erika::ffmpeg::{DecoderOutputFrame, Demuxer, Frame, StreamSelection};
use erika::renderer::{VideoFramePayload, wgpu::WgpuRenderer};
use erika::{PlayerVideoFrame, RendererBackend, TrackKind};

fn main() {
    let mut args = env::args().skip(1);
    let uri = args.next().unwrap_or_else(|| {
        eprintln!("usage: wgpu_decode_png <media> [out.png] [frame-index]");
        process::exit(2);
    });
    let out = args
        .next()
        .unwrap_or_else(|| "/tmp/erika_wgpu_decode.png".to_string());
    let target_index: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);

    let mut demuxer = Demuxer::open_uri(&uri).unwrap_or_else(|error| {
        eprintln!("open failed: {error}");
        process::exit(1);
    });
    let stream_index = demuxer
        .probe()
        .tracks
        .iter()
        .find(|track| track.kind == TrackKind::Video)
        .map(|track| track.id as i32)
        .unwrap_or_else(|| {
            eprintln!("no video stream found");
            process::exit(1);
        });
    demuxer
        .set_stream_selection(StreamSelection::only([stream_index]))
        .unwrap();
    let mut decoder = demuxer.open_decoder(stream_index).unwrap_or_else(|error| {
        eprintln!("decoder open failed: {error}");
        process::exit(1);
    });

    let mut renderer = WgpuRenderer::new().expect("create wgpu renderer");
    println!("wgpu backend: {:?}", renderer.adapter_info().backend);

    let mut index = 0usize;
    loop {
        match demuxer.read_packet() {
            Ok(Some(packet)) => {
                decoder.send_packet(&packet).expect("send packet");
                while let Ok(DecoderOutputFrame::Frame(frame)) = decoder.receive_frame() {
                    if index == target_index {
                        render_frame(&mut renderer, frame, &out);
                        return;
                    }
                    index += 1;
                }
            }
            Ok(None) => {
                decoder.send_eof().expect("send eof");
                while let Ok(DecoderOutputFrame::Frame(frame)) = decoder.receive_frame() {
                    if index >= target_index {
                        render_frame(&mut renderer, frame, &out);
                        return;
                    }
                    index += 1;
                }
                eprintln!("stream ended before reaching frame {target_index}");
                process::exit(1);
            }
            Err(error) => {
                eprintln!("read packet failed: {error}");
                process::exit(1);
            }
        }
    }
}

fn render_frame(renderer: &mut WgpuRenderer, frame: Frame, out: &str) {
    let pts = frame
        .pts()
        .map(|timestamp| Duration::from_secs_f64(timestamp.seconds().max(0.0)));
    println!(
        "frame {}x{} pix_fmt={} primaries={:?} transfer={:?}",
        frame.width(),
        frame.height(),
        frame
            .pixel_format()
            .unwrap_or_else(|| "unknown".to_string()),
        frame.color_primaries(),
        frame.transfer_function(),
    );
    let player_frame = PlayerVideoFrame {
        frame: VideoFramePayload::from_decoded(frame).expect("prepare renderer payload"),
        decode_backend: erika::ffmpeg::DecoderBackend::Software,
        pts,
        media_time: pts.unwrap_or_default(),
        late_by: None,
        generation: 1,
        scene_avg_nits: std::env::var("ERIKA_TEST_SCENE_AVG_NITS")
            .ok()
            .and_then(|value| value.parse::<f32>().ok()),
    };
    renderer
        .upload_player_frame(&player_frame)
        .expect("upload decoded frame");
    // The perceptual gamut LUT is generated on a background thread and only
    // bound once ready, so the first render uses the fast path. Repeat the
    // render (ERIKA_TEST_RENDER_PASSES, default 1) to exercise the LUT path
    // the player reaches after the first frames.
    let passes = std::env::var("ERIKA_TEST_RENDER_PASSES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1);
    let mut readback = renderer
        .render_current_offscreen(None)
        .expect("render current frame")
        .expect("a frame was uploaded");
    for _ in 1..passes {
        // TODO: replace fixed-delay waiting with explicit LUT-ready synchronization.
        std::thread::sleep(Duration::from_millis(200));
        readback = renderer
            .render_current_offscreen(None)
            .expect("render current frame")
            .expect("a frame was uploaded");
    }
    write_png(out, readback.width, readback.height, &readback.rgba);
    println!("wrote {out} ({}x{})", readback.width, readback.height);
}

fn write_png(path: &str, width: u32, height: u32, rgba: &[u8]) {
    let file = std::fs::File::create(path).expect("create png file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("write png header")
        .write_image_data(rgba)
        .expect("write png data");
}
