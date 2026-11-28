//! Renders BT.709 color bars as the video plane and composites a translucent
//! subtitle bitmap over them through the wgpu overlay pass, writing a PNG. Proves
//! the wgpu overlay/subtitle compositing path (alpha-blended over the video).

use erika::overlay::{OverlayFrame, OverlayViewport};
use erika::renderer::pipeline::VideoRenderPipeline;
use erika::renderer::wgpu::{VideoUniforms, WgpuRenderer};
use erika::subtitle::{SubtitleAlphaBitmap, SubtitleBitmapPlacement, SubtitleBitmapPlane};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

const BARS: [[u8; 3]; 8] = [
    [255, 255, 255],
    [255, 255, 0],
    [0, 255, 255],
    [0, 255, 0],
    [255, 0, 255],
    [255, 0, 0],
    [0, 0, 255],
    [0, 0, 0],
];

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/erika_wgpu_overlay.png".to_string());

    let (luma, chroma) = color_bars_nv12();
    let mut renderer = WgpuRenderer::new().expect("create wgpu renderer");
    println!("wgpu backend: {:?}", renderer.adapter_info().backend);
    renderer
        .upload_nv12(WIDTH, HEIGHT, &luma, &chroma, bars_uniforms())
        .expect("upload video");

    let overlay = OverlayFrame {
        pts: std::time::Duration::ZERO,
        viewport: OverlayViewport::new(WIDTH, HEIGHT),
        subtitle_planes: vec![
            // A translucent white caption bar near the bottom.
            SubtitleBitmapPlane {
                x: 40,
                y: 132,
                width: 240,
                height: 28,
                canvas_width: WIDTH,
                canvas_height: HEIGHT,
                rgba: solid_rgba(240, 28, [255, 255, 255, 200]),
            },
            // A small opaque magenta box top-left to show exact placement.
            SubtitleBitmapPlane {
                x: 12,
                y: 12,
                width: 40,
                height: 24,
                canvas_width: WIDTH,
                canvas_height: HEIGHT,
                rgba: solid_rgba(40, 24, [255, 0, 255, 255]),
            },
        ],
        subtitle_alpha_planes: vec![
            // A libass-style coverage bitmap: cyan, horizontal alpha gradient.
            // Exercises the mode-1 alpha-atlas path (coverage tinted by color).
            alpha_gradient_bitmap(60, 96, 200, 20, 0x00FF_FF00),
        ],
        subtitle_changed: true,
    };

    let readback = renderer
        .render_current_offscreen(Some(&overlay))
        .expect("render with overlay")
        .expect("a frame was uploaded");

    let file = std::fs::File::create(&out).expect("create png file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("write png header")
        .write_image_data(&readback.rgba)
        .expect("write png data");
    println!("wrote {out} ({WIDTH}x{HEIGHT})");
}

fn alpha_gradient_bitmap(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    color_rgba: u32,
) -> SubtitleAlphaBitmap {
    let mut alpha = vec![0u8; width as usize * height as usize];
    for row in 0..height as usize {
        for col in 0..width as usize {
            alpha[row * width as usize + col] = (col * 255 / width.max(1) as usize) as u8;
        }
    }
    SubtitleAlphaBitmap::new(
        SubtitleBitmapPlacement::new(x, y, width, height),
        width as usize,
        color_rgba,
        alpha,
    )
}

fn solid_rgba(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&rgba);
    }
    pixels
}

fn rgb_to_ycbcr_limited(rgb: [u8; 3]) -> (u8, u8, u8) {
    let r = f32::from(rgb[0]) / 255.0;
    let g = f32::from(rgb[1]) / 255.0;
    let b = f32::from(rgb[2]) / 255.0;
    let (kr, kg, kb) = (0.2126_f32, 0.7152_f32, 0.0722_f32);
    let y = kr * r + kg * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));
    let to8 = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    (
        to8(16.0 + 219.0 * y),
        to8(128.0 + 224.0 * cb),
        to8(128.0 + 224.0 * cr),
    )
}

fn bar_index(x: u32) -> usize {
    (x * 8 / WIDTH).min(7) as usize
}

fn color_bars_nv12() -> (Vec<u8>, Vec<u8>) {
    let mut luma = vec![0u8; (WIDTH * HEIGHT) as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let (y8, _, _) = rgb_to_ycbcr_limited(BARS[bar_index(x)]);
            luma[(y * WIDTH + x) as usize] = y8;
        }
    }
    let (cw, ch) = (WIDTH / 2, HEIGHT / 2);
    let mut chroma = vec![0u8; (cw * ch * 2) as usize];
    for y in 0..ch {
        for x in 0..cw {
            let (_, cb8, cr8) = rgb_to_ycbcr_limited(BARS[bar_index(x * 2)]);
            let idx = ((y * cw + x) * 2) as usize;
            chroma[idx] = cb8;
            chroma[idx + 1] = cr8;
        }
    }
    (luma, chroma)
}

fn bars_uniforms() -> VideoUniforms {
    let pipeline = VideoRenderPipeline::sdr_default();
    VideoUniforms::from_pipeline(&pipeline, false, false)
}
