//! Renders a BT.709 limited-range NV12 color-bar frame through the wgpu video
//! pipeline and writes the result to a PNG. A visual smoke test for the wgpu
//! YCbCr->RGB path: the output PNG should show clean SMPTE-style color bars.

use erika::renderer::pipeline::VideoRenderPipeline;
use erika::renderer::wgpu::{VideoUniforms, WgpuRenderer};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 144;

/// sRGB color bars (white, yellow, cyan, green, magenta, red, blue, black).
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

fn build_color_bars_nv12() -> (Vec<u8>, Vec<u8>) {
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

/// A faithful BT.709 limited-range round-trip: linear in/out, clip tone map,
/// matched nits, identity gamut. The decoded RGB should match the source bars.
fn bt709_limited_uniforms() -> VideoUniforms {
    let pipeline = VideoRenderPipeline::sdr_default();
    VideoUniforms::from_pipeline(&pipeline, false, false)
}

fn main() {
    let (luma, chroma) = build_color_bars_nv12();
    let mut renderer = WgpuRenderer::new().expect("create wgpu renderer");
    println!("wgpu backend: {:?}", renderer.adapter_info().backend);

    let readback = renderer
        .render_nv12_offscreen(WIDTH, HEIGHT, &luma, &chroma, bt709_limited_uniforms())
        .expect("render nv12 frame");

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/erika_wgpu_bars.png".to_string());
    let file = std::fs::File::create(&path).expect("create png file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write png header");
    writer
        .write_image_data(&readback.rgba)
        .expect("write png data");
    println!("wrote {path} ({WIDTH}x{HEIGHT})");
}
