//! Times frame export of evenly spaced pictures: one `export_frame` per
//! picture (each opens the media and seeks) against one `FrameExportSession`
//! taking them in order, and optionally writes the session's pictures as PNG.
//!
//! usage: cargo run --release -p frame_export_bench -- <media> [start-ms] [every-ms] [count] [max-edge] [out-dir]
//!
//! Prints one JSON object per mode with the elapsed time, and for the session
//! its seeks and decoded frames; with an output directory, one line per
//! picture with its luma percentiles (BT.709 weights on the 8-bit values).

use std::env;
use std::path::PathBuf;
use std::process;
use std::time::{Duration, Instant};

use erika::MediaRequest;
use erika::export::{
    FrameExportFormat, FrameExportOptions, FrameExportSession, FrameRequest, export_frame,
};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(uri) = args.first().cloned() else {
        eprintln!(
            "usage: frame_export_bench <media> [start-ms] [every-ms] [count] [max-edge] [out-dir]"
        );
        process::exit(2);
    };
    let number = |index: usize, default: u64| {
        args.get(index)
            .map(|value| value.parse::<u64>().unwrap_or_else(|_| usage(value)))
            .unwrap_or(default)
    };
    let start = number(1, 60_000);
    let every = number(2, 100);
    let count = number(3, 60);
    let edge = number(4, 640) as u32;
    let out = args.get(5).map(PathBuf::from);
    let modes = env::var("FRAME_EXPORT_BENCH_MODES").unwrap_or_else(|_| "single,session".into());
    let positions: Vec<Duration> = (0..count)
        .map(|index| Duration::from_millis(start + index * every))
        .collect();

    if modes.split(',').any(|mode| mode == "single") {
        let started = Instant::now();
        for position in &positions {
            export_frame(&FrameExportOptions {
                max_width: edge,
                max_height: edge,
                format: FrameExportFormat::Rgba,
                ..FrameExportOptions::new(uri.clone(), *position)
            })
            .unwrap_or_else(|error| fail(error));
        }
        println!(
            "{}",
            serde_json::json!({
                "mode": "single",
                "pictures": positions.len(),
                "elapsedMs": started.elapsed().as_millis() as u64,
            })
        );
    }

    if modes.split(',').any(|mode| mode == "session") {
        let started = Instant::now();
        let mut session =
            FrameExportSession::open(&MediaRequest::new(uri.clone())).unwrap_or_else(|e| fail(e));
        let mut pictures = Vec::new();
        for position in &positions {
            let frame = session
                .frame_at(&FrameRequest {
                    max_width: edge,
                    max_height: edge,
                    format: FrameExportFormat::Rgba,
                    ..FrameRequest::new(*position)
                })
                .unwrap_or_else(|error| fail(error));
            pictures.push((*position, frame));
        }
        let stats = session.stats();
        println!(
            "{}",
            serde_json::json!({
                "mode": "session",
                "pictures": stats.pictures,
                "elapsedMs": started.elapsed().as_millis() as u64,
                "seeks": stats.seeks,
                "decodedFrames": stats.decoded_frames,
            })
        );
        if let Some(out) = out {
            std::fs::create_dir_all(&out).unwrap_or_else(|error| fail(error));
            for (position, frame) in pictures {
                let path = out.join(format!("frame_{}ms.png", position.as_millis()));
                write_png(&path, frame.width, frame.height, &frame.data);
                let mut luma: Vec<f64> = frame
                    .data
                    .chunks_exact(4)
                    .map(|p| 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64)
                    .collect();
                luma.sort_by(|a, b| a.total_cmp(b));
                let at = |q: f64| luma[((luma.len() - 1) as f64 * q) as usize];
                println!(
                    "{}",
                    serde_json::json!({
                        "path": path,
                        "timestampMs": frame.timestamp.as_millis() as u64,
                        "size": [frame.width, frame.height],
                        "toneMapped": frame.tone_mapped,
                        "lumaP5": at(0.05), "lumaP50": at(0.5), "lumaP95": at(0.95),
                        "lumaMean": luma.iter().sum::<f64>() / luma.len() as f64,
                    })
                );
            }
        }
    }
}

fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) {
    let rgb: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    let mut file = std::fs::File::create(path).unwrap_or_else(|error| fail(error));
    let mut encoder = png_encoder(&mut file, width, height);
    encoder
        .write_image_data(&rgb)
        .unwrap_or_else(|error| fail(error));
}

fn png_encoder<'a>(
    file: &'a mut std::fs::File,
    width: u32,
    height: u32,
) -> png::Writer<&'a mut std::fs::File> {
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    encoder.write_header().unwrap_or_else(|error| fail(error))
}

fn usage(value: &str) -> ! {
    eprintln!("not a number: {value}");
    process::exit(2);
}

fn fail(error: impl std::fmt::Display) -> ! {
    eprintln!("{error}");
    process::exit(1);
}
