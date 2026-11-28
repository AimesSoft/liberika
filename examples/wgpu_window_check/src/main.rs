//! Opens a real macOS window backed by a CAMetalLayer, attaches it to the wgpu
//! renderer as a surface, and presents BT.709 color bars for a few seconds. This
//! exercises the wgpu surface/present path (not just offscreen) on real hardware.
//!
//! usage: cargo run -p wgpu_window_check -- [seconds]

use std::ffi::c_void;
use std::process;
use std::time::Duration;

use erika::renderer::pipeline::VideoRenderPipeline;
use erika::renderer::wgpu::{VideoUniforms, WgpuRenderer};
use erika::{
    PlatformSurface, RenderFrameContext, RendererBackend, WgpuSurfaceHandle, WgpuSurfaceKind,
};

unsafe extern "C" {
    fn erika_wgpu_window_create(width: f64, height: f64, scale: f64) -> *mut c_void;
    fn erika_wgpu_window_pump();
    fn erika_wgpu_window_release(layer: *mut c_void);
}

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
    let seconds: f64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(3.0);
    let scale = 2.0;

    let layer = unsafe { erika_wgpu_window_create(f64::from(WIDTH), f64::from(HEIGHT), scale) };
    if layer.is_null() {
        eprintln!("failed to create window/layer");
        process::exit(1);
    }
    let result = run(layer, scale, seconds);
    unsafe { erika_wgpu_window_release(layer) };
    match result {
        Ok(()) => println!("wgpu window check OK"),
        Err(error) => {
            eprintln!("wgpu window check failed: {error}");
            process::exit(1);
        }
    }
}

fn run(layer: *mut c_void, scale: f64, seconds: f64) -> Result<(), String> {
    let mut renderer = WgpuRenderer::new().map_err(|e| e.to_string())?;
    println!("wgpu backend: {:?}", renderer.adapter_info().backend);
    let pixel_width = (f64::from(WIDTH) * scale).round().max(1.0) as u32;
    let pixel_height = (f64::from(HEIGHT) * scale).round().max(1.0) as u32;

    let surface = PlatformSurface::Wgpu(WgpuSurfaceHandle::new(
        WgpuSurfaceKind::MacOsCaMetalLayer,
        layer as u64,
        0,
        pixel_width,
        pixel_height,
        scale,
    ));
    renderer
        .attach_surface(surface)
        .map_err(|e| e.to_string())?;

    let (luma, chroma) = color_bars_nv12();
    renderer
        .upload_nv12(WIDTH, HEIGHT, &luma, &chroma, bars_uniforms())
        .map_err(|e| e.to_string())?;

    let frames = (seconds * 60.0) as u32;
    for _ in 0..frames {
        if !renderer
            .render_current_frame(RenderFrameContext::new(Duration::ZERO, 1))
            .map_err(|e| e.to_string())?
        {
            return Err("render_current_frame returned false (no surface/frame)".to_string());
        }
        unsafe { erika_wgpu_window_pump() };
        std::thread::sleep(Duration::from_millis(16));
    }
    println!(
        "presented {frames} frames, rendered_frames={}",
        renderer.stats().rendered_frames
    );
    Ok(())
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
