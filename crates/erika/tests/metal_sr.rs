//! Metal super-resolution engines against the CPU oracles:
//! the generic network engine vs `sr::reference::run` for every bundled
//! network, and FSR1 vs `sr::fsr1`.
//!
//! Run in release (`cargo test --release -p erika --test metal_sr`); the
//! reference evaluation of the large networks is slow in debug builds.
//! Benchmarks: `cargo test --release -p erika --test metal_sr -- --ignored --nocapture bench`.

#![cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion,
    MTLResourceOptions, MTLSize, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use erika::renderer::metal::LumaUpscalerMode;
use erika::renderer::metal::upscaler::network::{EngineKind, NetworkEngine};
use erika::renderer::pipeline::UpscalerFamily;
use erika::renderer::sr::catalog::execution_model;
use erika::renderer::sr::model::{SrModel, f16_to_f32};
use erika::renderer::sr::reference::{self, LumaRange, test_pattern};

type Device = Retained<ProtocolObject<dyn MTLDevice>>;
type Texture = Retained<ProtocolObject<dyn MTLTexture>>;

fn device() -> Device {
    MTLCreateSystemDefaultDevice().expect("Metal device")
}

/// Quantizes to the 8-bit grid so the R8 upload is lossless.
fn quantized_pattern(width: usize, height: usize, seed: u32) -> Vec<f32> {
    test_pattern(width, height, seed)
        .into_iter()
        .map(|value| (value * 255.0).round() / 255.0)
        .collect()
}

fn upload_r8(device: &Device, luma: &[f32], width: usize, height: usize) -> Texture {
    let descriptor = unsafe {
        let descriptor =
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::R8Unorm,
                width,
                height,
                false,
            );
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor
    };
    let texture = device
        .newTextureWithDescriptor(&descriptor)
        .expect("luma texture");
    let bytes: Vec<u8> = luma
        .iter()
        .map(|value| (value * 255.0).round().clamp(0.0, 255.0) as u8)
        .collect();
    unsafe {
        texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
            MTLRegion {
                origin: MTLOrigin { x: 0, y: 0, z: 0 },
                size: MTLSize {
                    width,
                    height,
                    depth: 1,
                },
            },
            0,
            NonNull::new(bytes.as_ptr().cast::<c_void>().cast_mut()).expect("bytes"),
            width,
        );
    }
    texture
}

/// Reads back an `R16Float` texture.
fn read_r16f(
    device: &Device,
    command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    texture: &ProtocolObject<dyn MTLTexture>,
) -> impl FnOnce() -> Vec<f32> {
    let width = texture.width();
    let height = texture.height();
    let row_bytes = width * 2;
    let readback = device
        .newBufferWithLength_options(row_bytes * height, MTLResourceOptions::StorageModeShared)
        .expect("readback buffer");
    let blit = command_buffer.blitCommandEncoder().expect("blit encoder");
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
            texture,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
            MTLSize {
                width,
                height,
                depth: 1,
            },
            &readback,
            0,
            row_bytes,
            row_bytes * height,
        );
    }
    blit.endEncoding();
    move || {
        let contents = readback.contents().as_ptr().cast::<u16>();
        (0..width * height)
            .map(|index| f16_to_f32(unsafe { *contents.add(index) }))
            .collect()
    }
}

struct Stats {
    mae: f64,
    max: f64,
}

fn compare(actual: &[f32], expected: &[f32]) -> Stats {
    assert_eq!(actual.len(), expected.len());
    let mut sum = 0.0f64;
    let mut max = 0.0f64;
    for (a, e) in actual.iter().zip(expected) {
        let diff = (f64::from(*a) - f64::from(*e)).abs();
        assert!(diff.is_finite(), "non-finite output");
        sum += diff;
        max = max.max(diff);
    }
    Stats {
        mae: sum / actual.len() as f64,
        max,
    }
}

fn kinds(device: &Device) -> Vec<EngineKind> {
    let mut kinds = vec![EngineKind::Scalar];
    match EngineKind::preferred(device) {
        EngineKind::TensorOps => kinds.extend([EngineKind::Simdgroup, EngineKind::TensorOps]),
        EngineKind::Simdgroup => kinds.push(EngineKind::Simdgroup),
        EngineKind::Scalar => {}
    }
    kinds
}

fn run_engine(
    device: &Device,
    engine: &mut NetworkEngine,
    luma: &[f32],
    width: usize,
    height: usize,
    range: LumaRange,
) -> Vec<f32> {
    let queue = device.newCommandQueue().expect("queue");
    let input = upload_r8(device, luma, width, height);
    let command_buffer = queue.commandBuffer().expect("command buffer");
    let output = engine
        .encode(
            device,
            &command_buffer,
            &input,
            MTLPixelFormat::R16Float,
            range,
        )
        .expect("encode");
    let read = read_r16f(device, &command_buffer, &output);
    command_buffer.commit();
    command_buffer.waitUntilCompleted();
    assert!(
        command_buffer.error().is_none(),
        "command buffer failed: {:?}",
        command_buffer.error()
    );
    read()
}

fn check_model(
    device: &Device,
    label: &str,
    model: Arc<SrModel>,
    sizes: &[(usize, usize)],
    band_rows: Option<usize>,
) {
    for (width, height) in sizes.iter().copied() {
        let luma = quantized_pattern(width, height, 11);
        for range in [LumaRange::Full, LumaRange::Limited] {
            let expected = reference::run(&model, &luma, width, height, range);
            for kind in kinds(device) {
                let mut engine =
                    NetworkEngine::build(device, model.clone(), kind).expect("build engine");
                assert_eq!(
                    engine.kind(),
                    kind,
                    "{label}: {:?}",
                    engine.downgrade_reason()
                );
                engine.set_max_band_rows(band_rows);
                let actual = run_engine(device, &mut engine, &luma, width, height, range);
                let stats = compare(&actual, &expected);
                eprintln!(
                    "{label:<20} {kind:?} {width}x{height} {range:?} bands={}: mae {:.6} max {:.6}",
                    engine.band_count(width, height),
                    stats.mae,
                    stats.max
                );
                assert!(
                    stats.mae < 2e-3 && stats.max < 3e-2,
                    "{label} {kind:?} {width}x{height} {range:?}: mae {} max {}",
                    stats.mae,
                    stats.max
                );
            }
        }
    }
}

fn network_modes() -> Vec<LumaUpscalerMode> {
    LumaUpscalerMode::ALL
        .into_iter()
        .filter(|mode| {
            mode.family() == UpscalerFamily::Network && *mode != LumaUpscalerMode::Custom
        })
        .collect()
}

#[test]
fn every_bundled_network_matches_reference() {
    let device = device();
    for mode in network_modes() {
        let model = execution_model(mode).expect("model");
        let sizes: &[(usize, usize)] = if mode == LumaUpscalerMode::ArtCnnR8F64 {
            // The CPU reference of R8F64 (0.86 MMAC/px) is slow unoptimized.
            if cfg!(debug_assertions) {
                &[(19, 13)]
            } else {
                &[(67, 45)]
            }
        } else {
            &[(67, 45), (5, 3)]
        };
        check_model(&device, mode.label(), model, sizes, None);
    }
}

#[test]
fn banded_execution_is_exact() {
    let device = device();
    for mode in [
        LumaUpscalerMode::ArtCnnC4F16,
        LumaUpscalerMode::ArtCnnC4F32Ds,
        LumaUpscalerMode::Anime4kUl,
        LumaUpscalerMode::Anime4kDenoiseM,
    ] {
        let model = execution_model(mode).expect("model");
        check_model(&device, mode.label(), model, &[(67, 45)], Some(7));
    }
}

/// GPU milliseconds per frame: (median of committed-and-waited frames, best
/// sustained frame: batches of five frames back to back, best batch).
fn gpu_ms(
    device: &Device,
    frames: usize,
    mut encode: impl FnMut(&ProtocolObject<dyn MTLCommandBuffer>),
) -> (f64, f64) {
    let queue = device.newCommandQueue().expect("queue");
    let mut times = Vec::new();
    for frame in 0..frames + 3 {
        let command_buffer = queue.commandBuffer().expect("command buffer");
        encode(&command_buffer);
        command_buffer.commit();
        command_buffer.waitUntilCompleted();
        assert!(
            command_buffer.error().is_none(),
            "{:?}",
            command_buffer.error()
        );
        if frame >= 3 {
            times.push((command_buffer.GPUEndTime() - command_buffer.GPUStartTime()) * 1e3);
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let batch = if frames <= 3 { 1 } else { 5 };
    let mut sustained = f64::MAX;
    for _ in 0..3 {
        let command_buffer = queue.commandBuffer().expect("command buffer");
        for _ in 0..batch {
            encode(&command_buffer);
        }
        command_buffer.commit();
        command_buffer.waitUntilCompleted();
        sustained = sustained.min(
            (command_buffer.GPUEndTime() - command_buffer.GPUStartTime()) * 1e3 / batch as f64,
        );
    }
    (times[times.len() / 2], sustained.min(times[0]))
}

fn bench_frames(macs: u64, width: usize, height: usize) -> usize {
    let flops = 2.0 * macs as f64 * (width * height) as f64;
    if flops > 1e12 {
        3
    } else if flops > 1e11 {
        8
    } else {
        20
    }
}

/// GPU time per frame for every network and kernel family at 720p/1080p
/// (median and best of N frames), plus the legacy ArtCNN kernels.
#[test]
#[ignore]
fn bench_networks() {
    use erika::renderer::metal::upscaler::{LumaUpscaler, UpscalerBackend};
    let device = device();
    let sizes = [(1280usize, 720usize), (1920, 1080)];
    let modes: Vec<LumaUpscalerMode> = match std::env::var("ERIKA_SR_BENCH_MODES") {
        Ok(list) => network_modes()
            .into_iter()
            .filter(|mode| list.split(',').any(|item| item == mode.label()))
            .collect(),
        Err(_) => network_modes(),
    };
    let kinds: Vec<EngineKind> = kinds(&device)
        .into_iter()
        .filter(|kind| {
            std::env::var("ERIKA_SR_BENCH_SCALAR").is_ok() || *kind != EngineKind::Scalar
        })
        .collect();
    eprintln!(
        "| model | MAC/px | engine | size | GPU ms (median) | best sustained ms | TFLOPS (best) | bands | MiB |"
    );
    eprintln!("|---|---|---|---|---|---|---|---|---|");
    for mode in modes {
        let model = execution_model(mode).expect("model");
        let macs = model.macs_per_pixel();
        for kind in &kinds {
            let mut engine = NetworkEngine::build(&device, model.clone(), *kind).expect("build");
            for (width, height) in sizes {
                let luma = upload_r8(&device, &quantized_pattern(width, height, 3), width, height);
                let (median, best) = gpu_ms(&device, bench_frames(macs, width, height), |cb| {
                    engine
                        .encode(
                            &device,
                            cb,
                            &luma,
                            MTLPixelFormat::R8Unorm,
                            LumaRange::Limited,
                        )
                        .expect("encode");
                });
                let tflops = 2.0 * macs as f64 * (width * height) as f64 / (best * 1e-3) / 1e12;
                eprintln!(
                    "| {} | {macs} | {} | {width}x{height} | {median:.3} | {best:.3} | {tflops:.2} | {} | {:.0} |",
                    mode.label(),
                    kind.label(),
                    engine.band_count(width, height),
                    engine.allocated_bytes() as f64 / (1u64 << 20) as f64,
                );
            }
        }
        if mode.is_legacy_artcnn() {
            // The hand-written kernels evaluate the unfolded network.
            let source_macs = erika::renderer::sr::catalog::source_model(mode)
                .expect("source")
                .macs_per_pixel();
            for backend in [
                UpscalerBackend::LegacyScalar,
                UpscalerBackend::LegacySimdgroupMatrix,
            ] {
                let mut upscaler = LumaUpscaler::default();
                upscaler.set_mode(mode);
                upscaler.set_backend(backend);
                upscaler.prepare_blocking(&device).expect("prepare");
                for (width, height) in sizes {
                    let luma =
                        upload_r8(&device, &quantized_pattern(width, height, 3), width, height);
                    let (median, best) = gpu_ms(&device, 20, |cb| {
                        upscaler
                            .encode(&device, cb, &luma, MTLPixelFormat::R8Unorm)
                            .expect("encode")
                            .expect("enabled");
                    });
                    let tflops =
                        2.0 * source_macs as f64 * (width * height) as f64 / (best * 1e-3) / 1e12;
                    eprintln!(
                        "| {} (unfolded) | {source_macs} | legacy {backend:?} | {width}x{height} | {median:.3} | {best:.3} | {tflops:.2} | 1 | {:.0} |",
                        mode.label(),
                        upscaler.allocated_bytes() as f64 / (1u64 << 20) as f64,
                    );
                }
            }
        }
    }
}

/// Per-dispatch GPU time of one 1080p frame (each dispatch in its own
/// command buffer). `ERIKA_SR_BENCH_MODES` selects networks.
#[test]
#[ignore]
fn bench_profile_layers() {
    let device = device();
    let queue = device.newCommandQueue().expect("queue");
    let (width, height) = (1920usize, 1080usize);
    let modes: Vec<LumaUpscalerMode> = match std::env::var("ERIKA_SR_BENCH_MODES") {
        Ok(list) => network_modes()
            .into_iter()
            .filter(|mode| list.split(',').any(|item| item == mode.label()))
            .collect(),
        Err(_) => vec![LumaUpscalerMode::ArtCnnC4F16, LumaUpscalerMode::ArtCnnC4F32],
    };
    let luma = upload_r8(&device, &quantized_pattern(width, height, 3), width, height);
    for mode in modes {
        let model = execution_model(mode).expect("model");
        for kind in kinds(&device)
            .into_iter()
            .filter(|kind| *kind != EngineKind::Scalar)
        {
            let mut engine = NetworkEngine::build(&device, model.clone(), kind).expect("build");
            let profile = engine
                .profile(
                    &device,
                    &queue,
                    &luma,
                    MTLPixelFormat::R8Unorm,
                    LumaRange::Limited,
                )
                .expect("profile");
            let total: f64 = profile.iter().map(|(_, ms)| ms).sum();
            let parts: Vec<String> = profile
                .iter()
                .map(|(label, ms)| format!("{label}={ms:.3}"))
                .collect();
            eprintln!(
                "{} {kind:?} total {total:.3} ms: {}",
                mode.label(),
                parts.join(" ")
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Front-end modes: FSR1, MetalFX, VideoToolbox, custom models.

use erika::LumaUpscalerBackendStatus;
use erika::renderer::metal::upscaler::{LumaUpscaler, UpscaleRequest, UpscaledFrame};
use erika::renderer::sr::fsr1;

/// Reads back an 8-bit unorm texture with `components` channels.
fn read_unorm8(
    device: &Device,
    command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    texture: &ProtocolObject<dyn MTLTexture>,
    components: usize,
) -> impl FnOnce() -> Vec<f32> {
    let width = texture.width();
    let height = texture.height();
    let row_bytes = width * components;
    let readback = device
        .newBufferWithLength_options(row_bytes * height, MTLResourceOptions::StorageModeShared)
        .expect("readback buffer");
    let blit = command_buffer.blitCommandEncoder().expect("blit encoder");
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
            texture,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
            MTLSize {
                width,
                height,
                depth: 1,
            },
            &readback,
            0,
            row_bytes,
            row_bytes * height,
        );
    }
    blit.endEncoding();
    move || {
        let contents = readback.contents().as_ptr().cast::<u8>();
        (0..row_bytes * height)
            .map(|index| f32::from(unsafe { *contents.add(index) }) / 255.0)
            .collect()
    }
}

fn upload_unorm8(
    device: &Device,
    values: &[f32],
    width: usize,
    height: usize,
    format: MTLPixelFormat,
    components: usize,
) -> Texture {
    let descriptor = unsafe {
        let descriptor =
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                format, width, height, false,
            );
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor
    };
    let texture = device
        .newTextureWithDescriptor(&descriptor)
        .expect("texture");
    let bytes: Vec<u8> = values
        .iter()
        .map(|value| (value * 255.0).round().clamp(0.0, 255.0) as u8)
        .collect();
    unsafe {
        texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
            MTLRegion {
                origin: MTLOrigin { x: 0, y: 0, z: 0 },
                size: MTLSize {
                    width,
                    height,
                    depth: 1,
                },
            },
            0,
            NonNull::new(bytes.as_ptr().cast::<c_void>().cast_mut()).expect("bytes"),
            width * components,
        );
    }
    texture
}

/// Encodes one frame through the front-end, polling background builds.
fn upscale_frame(
    device: &Device,
    upscaler: &mut LumaUpscaler,
    luma: &ProtocolObject<dyn MTLTexture>,
    chroma: Option<&ProtocolObject<dyn MTLTexture>>,
    output_format: MTLPixelFormat,
    presented: (usize, usize),
) -> Option<(
    UpscaledFrame,
    Retained<ProtocolObject<dyn MTLCommandBuffer>>,
)> {
    let queue = device.newCommandQueue().expect("queue");
    for _ in 0..600 {
        let command_buffer = queue.commandBuffer().expect("command buffer");
        let request = UpscaleRequest {
            luma,
            chroma,
            output_format,
            range: LumaRange::Limited,
            presented,
            frame_token: None,
        };
        let frame = upscaler
            .encode_frame(device, &command_buffer, &request)
            .expect("encode");
        if let Some(frame) = frame {
            return Some((frame, command_buffer));
        }
        command_buffer.commit();
        if upscaler.active_backend() != LumaUpscalerBackendStatus::Building {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("upscaler never finished building");
}

fn bilinear_resize(
    input: &[f32],
    width: usize,
    height: usize,
    out_w: usize,
    out_h: usize,
) -> Vec<f32> {
    let mut out = Vec::with_capacity(out_w * out_h);
    for y in 0..out_h {
        for x in 0..out_w {
            let px = (x as f32 + 0.5) * width as f32 / out_w as f32 - 0.5;
            let py = (y as f32 + 0.5) * height as f32 / out_h as f32 - 0.5;
            out.push(reference::bilinear_clamped(input, width, height, px, py));
        }
    }
    out
}

#[test]
fn fsr1_matches_cpu_reference() {
    let device = device();
    let mut upscaler = LumaUpscaler::default();
    upscaler.set_mode(LumaUpscalerMode::Fsr1);
    upscaler.prepare_blocking(&device).expect("prepare fsr1");
    for ((width, height), (out_w, out_h)) in [
        ((67, 45), (134, 90)),
        ((67, 45), (113, 77)),
        ((64, 36), (160, 90)),
        ((31, 17), (100, 41)),
    ] {
        let luma = quantized_pattern(width, height, 5);
        let input = upload_r8(&device, &luma, width, height);
        let (frame, command_buffer) = upscale_frame(
            &device,
            &mut upscaler,
            &input,
            None,
            MTLPixelFormat::R16Float,
            (out_w, out_h),
        )
        .expect("fsr1 output");
        assert_eq!((frame.luma.width(), frame.luma.height()), (out_w, out_h));
        assert!(frame.chroma.is_none());
        let read = read_r16f(&device, &command_buffer, &frame.luma);
        command_buffer.commit();
        command_buffer.waitUntilCompleted();
        let actual = read();
        let expected = fsr1::rcas(
            &fsr1::easu(&luma, width, height, out_w, out_h),
            out_w,
            out_h,
            fsr1::rcas_strength(fsr1::DEFAULT_SHARPNESS_STOPS),
        );
        let stats = compare(&actual, &expected);
        eprintln!(
            "fsr1 {width}x{height} -> {out_w}x{out_h}: mae {:.6} max {:.6}",
            stats.mae, stats.max
        );
        assert!(
            stats.mae < 1e-3 && stats.max < 1e-2,
            "mae {} max {}",
            stats.mae,
            stats.max
        );
        assert_eq!(upscaler.active_backend(), LumaUpscalerBackendStatus::Scalar);
    }
    // Not shown above the source size: native sampling with a reason.
    let luma = quantized_pattern(32, 32, 1);
    let input = upload_r8(&device, &luma, 32, 32);
    assert!(
        upscale_frame(
            &device,
            &mut upscaler,
            &input,
            None,
            MTLPixelFormat::R8Unorm,
            (32, 32)
        )
        .is_none()
    );
    assert_eq!(
        upscaler.active_backend(),
        LumaUpscalerBackendStatus::Inactive
    );
    assert!(upscaler.status_reason().is_some());
}

#[test]
fn metalfx_spatial_upscales_luma() {
    let device = device();
    let mut upscaler = LumaUpscaler::default();
    upscaler.set_mode(LumaUpscalerMode::MetalFxSpatial);
    let (width, height, out_w, out_h) = (320usize, 180usize, 800usize, 450usize);
    // Smooth content: MetalFX output must stay close to a bilinear resize.
    let luma: Vec<f32> = (0..width * height)
        .map(|i| {
            let (x, y) = ((i % width) as f32, (i / width) as f32);
            (0.5 + 0.3 * (x * 0.05).sin() * (y * 0.07).cos()).clamp(0.0, 1.0)
        })
        .map(|value| (value * 255.0).round() / 255.0)
        .collect();
    let input = upload_r8(&device, &luma, width, height);
    let Some((frame, command_buffer)) = upscale_frame(
        &device,
        &mut upscaler,
        &input,
        None,
        MTLPixelFormat::R8Unorm,
        (out_w, out_h),
    ) else {
        eprintln!("MetalFX unavailable: {:?}", upscaler.status_reason());
        assert_eq!(
            upscaler.active_backend(),
            LumaUpscalerBackendStatus::Inactive
        );
        return;
    };
    assert_eq!(
        upscaler.active_backend(),
        LumaUpscalerBackendStatus::MetalFx
    );
    assert_eq!((frame.luma.width(), frame.luma.height()), (out_w, out_h));
    assert_eq!(frame.luma.pixelFormat(), MTLPixelFormat::R8Unorm);
    let read = read_unorm8(&device, &command_buffer, &frame.luma, 1);
    command_buffer.commit();
    command_buffer.waitUntilCompleted();
    assert!(
        command_buffer.error().is_none(),
        "{:?}",
        command_buffer.error()
    );
    let actual = read();
    let expected = bilinear_resize(&luma, width, height, out_w, out_h);
    let stats = compare(&actual, &expected);
    eprintln!(
        "metalfx {width}x{height} -> {out_w}x{out_h}: mae {:.6} max {:.6}",
        stats.mae, stats.max
    );
    assert!(
        stats.mae < 0.01,
        "MetalFX output diverges from the source: {}",
        stats.mae
    );
}

#[test]
fn video_toolbox_super_resolution_doubles_frames() {
    let device = device();
    let mut upscaler = LumaUpscaler::default();
    upscaler.set_mode(LumaUpscalerMode::PlatformVideo);
    let (width, height) = (640usize, 360usize);
    let luma: Vec<f32> = (0..width * height)
        .map(|i| {
            let (x, y) = ((i % width) as f32, (i / width) as f32);
            (0.45 + 0.3 * (x * 0.04).sin() * (y * 0.05).cos()).clamp(16.0 / 255.0, 235.0 / 255.0)
        })
        .map(|value| (value * 255.0).round() / 255.0)
        .collect();
    let chroma: Vec<f32> = (0..width * height / 2)
        .map(|i| if i % 2 == 0 { 0.45 } else { 0.55 })
        .collect();
    let luma_texture = upload_unorm8(&device, &luma, width, height, MTLPixelFormat::R8Unorm, 1);
    let chroma_texture = upload_unorm8(
        &device,
        &chroma,
        width / 2,
        height / 2,
        MTLPixelFormat::RG8Unorm,
        2,
    );
    let Some((frame, command_buffer)) = upscale_frame(
        &device,
        &mut upscaler,
        &luma_texture,
        Some(&chroma_texture),
        MTLPixelFormat::R8Unorm,
        (width * 3, height * 3),
    ) else {
        eprintln!(
            "VideoToolbox super resolution unavailable: {:?}",
            upscaler.status_reason()
        );
        assert_eq!(
            upscaler.active_backend(),
            LumaUpscalerBackendStatus::Inactive
        );
        assert!(upscaler.status_reason().is_some());
        return;
    };
    assert_eq!(
        upscaler.active_backend(),
        LumaUpscalerBackendStatus::VideoToolbox
    );
    let chroma_out = frame.chroma.clone().expect("VT replaces chroma");
    assert_eq!(
        (frame.luma.width(), frame.luma.height()),
        (width * 2, height * 2)
    );
    assert_eq!((chroma_out.width(), chroma_out.height()), (width, height));
    let read_luma = read_unorm8(&device, &command_buffer, &frame.luma, 1);
    let read_chroma = read_unorm8(&device, &command_buffer, &chroma_out, 2);
    command_buffer.commit();
    command_buffer.waitUntilCompleted();
    assert!(
        command_buffer.error().is_none(),
        "{:?}",
        command_buffer.error()
    );
    let actual = read_luma();
    let expected = bilinear_resize(&luma, width, height, width * 2, height * 2);
    let stats = compare(&actual, &expected);
    if std::env::var_os("ERIKA_SR_VT_ROWS").is_some() {
        for y in (0..height * 2).step_by(1) {
            let row = &actual[y * width * 2..(y + 1) * width * 2];
            let reference = &expected[y * width * 2..(y + 1) * width * 2];
            let max = row
                .iter()
                .zip(reference)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            if max > 0.03 {
                eprintln!("vt row {y}: max {max:.4}");
            }
        }
    }
    let chroma_actual = read_chroma();
    let chroma_mean: f32 = chroma_actual.iter().sum::<f32>() / chroma_actual.len() as f32;
    eprintln!(
        "vt {width}x{height}: luma mae {:.6} max {:.6}, chroma mean {chroma_mean:.4}",
        stats.mae, stats.max
    );
    assert!(stats.mae < 0.02, "VT luma diverges: {}", stats.mae);
    assert!(
        (chroma_mean - 0.5).abs() < 0.02,
        "VT chroma diverges: {chroma_mean}"
    );

    // 720p sources exceed the low-latency scaler's limits.
    let big_luma = upload_unorm8(
        &device,
        &vec![0.5; 1280 * 720],
        1280,
        720,
        MTLPixelFormat::R8Unorm,
        1,
    );
    let big_chroma = upload_unorm8(
        &device,
        &vec![0.5; 640 * 360 * 2],
        640,
        360,
        MTLPixelFormat::RG8Unorm,
        2,
    );
    assert!(
        upscale_frame(
            &device,
            &mut upscaler,
            &big_luma,
            Some(&big_chroma),
            MTLPixelFormat::R8Unorm,
            (2560, 1440)
        )
        .is_none()
    );
    assert_eq!(
        upscaler.active_backend(),
        LumaUpscalerBackendStatus::Inactive
    );
    eprintln!("vt 1280x720: {:?}", upscaler.status_reason());
    assert!(upscaler.status_reason().is_some());
}

#[test]
fn custom_model_runs_through_the_front_end() {
    let device = device();
    let (width, height) = (67usize, 45usize);
    let luma = quantized_pattern(width, height, 9);
    let input = upload_r8(&device, &luma, width, height);
    let model = execution_model(LumaUpscalerMode::Anime4kS).expect("model");
    let mut upscaler = LumaUpscaler::default();
    upscaler.set_mode(LumaUpscalerMode::Custom);
    // No model loaded yet: inactive with a reason.
    assert!(
        upscale_frame(
            &device,
            &mut upscaler,
            &input,
            None,
            MTLPixelFormat::R16Float,
            (134, 90)
        )
        .is_none()
    );
    assert_eq!(
        upscaler.active_backend(),
        LumaUpscalerBackendStatus::Inactive
    );
    assert!(upscaler.status_reason().is_some());
    upscaler.set_custom_model(Some(Arc::new((*model).clone())));
    let (frame, command_buffer) = upscale_frame(
        &device,
        &mut upscaler,
        &input,
        None,
        MTLPixelFormat::R16Float,
        (134, 90),
    )
    .expect("custom output");
    let read = read_r16f(&device, &command_buffer, &frame.luma);
    command_buffer.commit();
    command_buffer.waitUntilCompleted();
    let expected = reference::run(&model, &luma, width, height, LumaRange::Limited);
    let stats = compare(&read(), &expected);
    eprintln!(
        "custom anime4k_s: mae {:.6} max {:.6}",
        stats.mae, stats.max
    );
    assert!(stats.mae < 2e-3 && stats.max < 3e-2);
    assert!(matches!(
        upscaler.active_backend(),
        LumaUpscalerBackendStatus::TensorOps
            | LumaUpscalerBackendStatus::SimdgroupMatrix
            | LumaUpscalerBackendStatus::Scalar
    ));
}

/// GPU time of FSR1, MetalFX spatial and VideoToolbox super resolution
/// (wall time of commit + wait in parentheses; VideoToolbox may run part of
/// its work outside the GPU timeline).
#[test]
#[ignore]
fn bench_spatial_and_platform() {
    let device = device();
    let queue = device.newCommandQueue().expect("queue");
    eprintln!(
        "| mode | source | output | GPU ms (median) | wall ms (median) | CPU encode ms (median) |"
    );
    eprintln!("|---|---|---|---|---|---|");
    let cases: [(LumaUpscalerMode, (usize, usize), (usize, usize)); 9] = [
        (LumaUpscalerMode::Fsr1, (1280, 720), (2560, 1440)),
        (LumaUpscalerMode::Fsr1, (1920, 1080), (2560, 1440)),
        (LumaUpscalerMode::Fsr1, (1920, 1080), (3840, 2160)),
        (LumaUpscalerMode::MetalFxSpatial, (1280, 720), (2560, 1440)),
        (LumaUpscalerMode::MetalFxSpatial, (1920, 1080), (2560, 1440)),
        (LumaUpscalerMode::MetalFxSpatial, (1920, 1080), (3840, 2160)),
        (LumaUpscalerMode::PlatformVideo, (640, 360), (1280, 720)),
        (LumaUpscalerMode::PlatformVideo, (854, 480), (1708, 960)),
        (LumaUpscalerMode::PlatformVideo, (960, 540), (1920, 1080)),
    ];
    for (mode, (width, height), presented) in cases {
        let mut upscaler = LumaUpscaler::default();
        upscaler.set_mode(mode);
        let luma = upload_unorm8(
            &device,
            &quantized_pattern(width, height, 4),
            width,
            height,
            MTLPixelFormat::R8Unorm,
            1,
        );
        let chroma = upload_unorm8(
            &device,
            &vec![0.5; width * height / 2],
            width / 2,
            height / 2,
            MTLPixelFormat::RG8Unorm,
            2,
        );
        // Warm up (background build / session start).
        if upscale_frame(
            &device,
            &mut upscaler,
            &luma,
            Some(&chroma),
            MTLPixelFormat::R8Unorm,
            presented,
        )
        .is_none()
        {
            eprintln!(
                "| {} | {width}x{height} | unavailable: {:?} | | |",
                mode.label(),
                upscaler.status_reason()
            );
            continue;
        }
        let mut gpu = Vec::new();
        let mut wall = Vec::new();
        let mut cpu = Vec::new();
        for frame in 0..23 {
            let command_buffer = queue.commandBuffer().expect("command buffer");
            let request = UpscaleRequest {
                luma: &luma,
                chroma: Some(&chroma),
                output_format: MTLPixelFormat::R8Unorm,
                range: LumaRange::Limited,
                presented,
                frame_token: None,
            };
            let started = std::time::Instant::now();
            upscaler
                .encode_frame(&device, &command_buffer, &request)
                .expect("encode")
                .expect("output");
            let encoded = started.elapsed().as_secs_f64() * 1e3;
            command_buffer.commit();
            command_buffer.waitUntilCompleted();
            if frame >= 3 {
                cpu.push(encoded);
                wall.push(started.elapsed().as_secs_f64() * 1e3);
                gpu.push((command_buffer.GPUEndTime() - command_buffer.GPUStartTime()) * 1e3);
            }
        }
        // Sustained: ten frames back to back per command buffer.
        let mut sustained = f64::MAX;
        for _ in 0..3 {
            let command_buffer = queue.commandBuffer().expect("command buffer");
            let request = UpscaleRequest {
                luma: &luma,
                chroma: Some(&chroma),
                output_format: MTLPixelFormat::R8Unorm,
                range: LumaRange::Limited,
                presented,
                frame_token: None,
            };
            for _ in 0..10 {
                upscaler
                    .encode_frame(&device, &command_buffer, &request)
                    .expect("encode")
                    .expect("output");
            }
            command_buffer.commit();
            command_buffer.waitUntilCompleted();
            sustained = sustained
                .min((command_buffer.GPUEndTime() - command_buffer.GPUStartTime()) * 1e3 / 10.0);
        }
        eprintln!("  sustained {}: {sustained:.3} ms/frame", mode.label());
        gpu.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        wall.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        cpu.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        eprintln!(
            "| {} | {width}x{height} | {}x{} | {:.3} | {:.3} | {:.3} |",
            mode.label(),
            presented.0,
            presented.1,
            gpu[gpu.len() / 2],
            wall[wall.len() / 2],
            cpu[cpu.len() / 2]
        );
    }
}

/// Wall time of planning + Metal compilation per network and kernel family
/// (first build in the process; Metal's shader cache may make repeat runs of
/// the test faster).
#[test]
#[ignore]
fn bench_build_times() {
    let device = device();
    for mode in network_modes() {
        let model = execution_model(mode).expect("model");
        let mut parts = Vec::new();
        for kind in kinds(&device) {
            let started = std::time::Instant::now();
            let engine = NetworkEngine::build(&device, model.clone(), kind).expect("build");
            parts.push(format!(
                "{}={:.0}ms",
                engine.kind().label(),
                started.elapsed().as_secs_f64() * 1e3
            ));
        }
        eprintln!("{:<20} {}", mode.label(), parts.join(" "));
    }
}

// ---------------------------------------------------------------------------
// End to end through MetalRenderer (Flutter texture target, VideoToolbox-style
// CVPixelBuffer frames).

mod renderer_e2e {
    use super::*;
    use erika::renderer::metal::{
        MetalRenderer, MetalRendererConfig, VideoFrameTextureSource, VideoRenderFrame,
    };
    use erika::{FlutterTextureHandle, FlutterTextureKind, PlatformSurface, RendererBackend};
    use objc2::runtime::AnyObject;
    use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
        kCVPixelBufferMetalCompatibilityKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        kCVReturnSuccess,
    };
    use objc2_foundation::{NSDictionary, NSNumber, NSString};

    fn nv12_pixel_buffer(width: usize, height: usize) -> CFRetained<CVPixelBuffer> {
        let ns_key = |key: &CFString| -> &NSString { unsafe { &*(key as *const CFString).cast() } };
        let surface: objc2::rc::Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
        let metal = NSNumber::new_bool(true);
        let attributes: objc2::rc::Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(
                &[
                    ns_key(unsafe { kCVPixelBufferIOSurfacePropertiesKey }),
                    ns_key(unsafe { kCVPixelBufferMetalCompatibilityKey }),
                ],
                &[&**surface as &AnyObject, &**metal as &AnyObject],
            );
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width,
                height,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                Some(&*(Retained::as_ptr(&attributes).cast::<CFDictionary>())),
                NonNull::new(&mut raw).expect("ptr"),
            )
        };
        assert_eq!(status, kCVReturnSuccess);
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(raw).expect("buffer")) };
        let luma = quantized_pattern(width, height, 2);
        unsafe {
            CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
            let y = CVPixelBufferGetBaseAddressOfPlane(&buffer, 0).cast::<u8>();
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 0);
            for row in 0..height {
                for col in 0..width {
                    let value = 16.0 + luma[row * width + col] * 219.0;
                    *y.add(row * y_stride + col) = value.round() as u8;
                }
            }
            let uv = CVPixelBufferGetBaseAddressOfPlane(&buffer, 1).cast::<u8>();
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 1);
            for row in 0..height / 2 {
                for col in 0..width {
                    *uv.add(row * uv_stride + col) = 128;
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
        }
        buffer
    }

    fn flutter_target(device: &Device, width: usize, height: usize) -> Texture {
        let descriptor = unsafe {
            let descriptor =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    width,
                    height,
                    false,
                );
            descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            descriptor
        };
        device
            .newTextureWithDescriptor(&descriptor)
            .expect("target")
    }

    #[test]
    fn renderer_presents_every_mode() {
        let device = device();
        let (width, height) = (640usize, 360usize);
        let (out_w, out_h) = (1600u32, 900u32);
        let buffer = nv12_pixel_buffer(width, height);
        let target = flutter_target(&device, out_w as usize, out_h as usize);
        for (mode, expected) in [
            (LumaUpscalerMode::ArtCnnC4F16, None),
            (LumaUpscalerMode::Anime4kUl, None),
            (
                LumaUpscalerMode::Fsr1,
                Some(LumaUpscalerBackendStatus::Scalar),
            ),
            (
                LumaUpscalerMode::MetalFxSpatial,
                Some(LumaUpscalerBackendStatus::MetalFx),
            ),
            (
                LumaUpscalerMode::PlatformVideo,
                Some(LumaUpscalerBackendStatus::VideoToolbox),
            ),
        ] {
            let mut renderer = MetalRenderer::with_config(MetalRendererConfig {
                luma_upscaler: mode,
                ..MetalRendererConfig::default()
            })
            .expect("renderer");
            renderer
                .attach_surface(PlatformSurface::FlutterTexture(FlutterTextureHandle::new(
                    FlutterTextureKind::MacOsTextureRegistrar,
                    1,
                    out_w,
                    out_h,
                    1.0,
                )))
                .expect("attach");
            renderer
                .set_flutter_texture_buffer(Retained::as_ptr(&target) as u64, out_w, out_h)
                .expect("flutter texture");
            let frame = unsafe {
                renderer.import_video_frame_textures(VideoFrameTextureSource::new(
                    CFRetained::as_ptr(&buffer).as_ptr().cast(),
                    width as u32,
                    height as u32,
                ))
            }
            .expect("import");
            let mut token = 0;
            while renderer.stats().upscaled_frames == 0 && token < 300 {
                token += 1;
                renderer
                    .render_video_frame(VideoRenderFrame::new(&frame).frame_token(token))
                    .expect("render");
                if renderer.stats().upscaled_frames == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            // A repeated token reuses the cached upscale.
            renderer
                .render_video_frame(VideoRenderFrame::new(&frame).frame_token(token))
                .expect("render");
            let stats = renderer.stats();
            let (reason, engine) = renderer.upscaler_status_detail();
            eprintln!(
                "{}: status {:?} engine {engine:?} reason {reason:?} upscaled {} last encode {:?}",
                mode.label(),
                stats.upscaler_backend,
                stats.upscaled_frames,
                stats.last_upscaler_encode_duration
            );
            assert!(
                stats.upscaled_frames >= 2,
                "{} never upscaled: {reason:?}",
                mode.label()
            );
            // Neutral chroma must stay gray and the picture must be present.
            let queue = device.newCommandQueue().expect("queue");
            let command_buffer = queue.commandBuffer().expect("command buffer");
            let read = read_unorm8(&device, &command_buffer, &target, 4);
            command_buffer.commit();
            command_buffer.waitUntilCompleted();
            let bgra = read();
            let pixels = bgra.len() / 4;
            let mut green = 0.0f64;
            let mut tint = 0.0f64;
            for px in bgra.chunks_exact(4) {
                green += f64::from(px[1]);
                tint += f64::from((px[0] - px[1]).abs() + (px[2] - px[1]).abs());
            }
            if std::env::var_os("ERIKA_SR_TINT_ROWS").is_some() {
                let row_px = out_w as usize;
                for band in 0..10 {
                    let rows = band * out_h as usize / 10..(band + 1) * out_h as usize / 10;
                    let mut band_tint = 0.0f64;
                    let mut count = 0usize;
                    for y in rows {
                        for px in bgra[y * row_px * 4..(y + 1) * row_px * 4].chunks_exact(4) {
                            band_tint += f64::from((px[0] - px[1]).abs() + (px[2] - px[1]).abs());
                            count += 1;
                        }
                    }
                    eprintln!("  band {band}: tint {:.4}", band_tint / count as f64);
                }
            }
            let (green, tint) = (green / pixels as f64, tint / pixels as f64);
            eprintln!("{}: mean green {green:.3} tint {tint:.4}", mode.label());
            assert!(
                (0.2..0.8).contains(&green),
                "{}: picture missing",
                mode.label()
            );
            // VideoToolbox's scaler returns neutral chroma about one code
            // value off (measured tint 0.019); everything else is exact.
            let limit = if mode == LumaUpscalerMode::PlatformVideo {
                0.03
            } else {
                0.005
            };
            assert!(tint < limit, "{}: chroma shifted ({tint})", mode.label());
            match expected {
                Some(status) => assert_eq!(stats.upscaler_backend, status),
                None => assert!(matches!(
                    stats.upscaler_backend,
                    LumaUpscalerBackendStatus::TensorOps
                        | LumaUpscalerBackendStatus::SimdgroupMatrix
                        | LumaUpscalerBackendStatus::Scalar
                )),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Synthetic graphs covering what the bundled networks do not: clamp padding
// with folded border variants, PReLU/SiLU, residual adds, odd channel counts,
// kernels from 1 to 7, mixed transforms on one tensor, column-major heads.

mod synthetic {
    use super::*;
    use erika::renderer::sr::fold;
    use erika::renderer::sr::model::{
        Activation, ConvInput, ConvLayer, Head, HeadBase, InputRange, InputTransform, Padding,
        SubpixelOrder,
    };

    struct Rng(u32);

    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((self.0 >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
        }
    }

    fn input(
        rng: &mut Rng,
        tensor: usize,
        channels: usize,
        out: usize,
        transform: InputTransform,
        kernel: usize,
    ) -> ConvInput {
        let fan_in = (channels * kernel * kernel) as f32;
        ConvInput {
            tensor,
            transform,
            kernel,
            weights: (0..out * channels * kernel * kernel)
                .map(|_| rng.next() * 1.2 / fan_in.sqrt())
                .collect(),
        }
    }

    fn layer(
        rng: &mut Rng,
        output: usize,
        out_channels: usize,
        inputs: Vec<ConvInput>,
        residual: Option<usize>,
        activation: Activation,
    ) -> ConvLayer {
        ConvLayer {
            output,
            out_channels,
            inputs,
            bias: (0..out_channels).map(|_| rng.next() * 0.1).collect(),
            residual,
            activation,
            border: None,
        }
    }

    fn model(padding: Padding, input_range: InputRange, head: Head, seed: u32) -> SrModel {
        let mut rng = Rng(seed);
        let tensors = vec![1, 6, 5, 7, 4, 4];
        let layers = vec![
            // Linear 3x3: folded into both consumers with border variants.
            {
                let inputs = vec![input(&mut rng, 0, 1, 6, InputTransform::Identity, 3)];
                layer(&mut rng, 1, 6, inputs, None, Activation::None)
            },
            {
                let inputs = vec![input(&mut rng, 1, 6, 5, InputTransform::Identity, 3)];
                let alpha = (0..5).map(|_| rng.next() * 0.3).collect();
                layer(&mut rng, 2, 5, inputs, None, Activation::Prelu(alpha))
            },
            {
                let inputs = vec![
                    input(&mut rng, 2, 5, 7, InputTransform::Relu, 5),
                    input(&mut rng, 2, 5, 7, InputTransform::NegRelu, 5),
                    input(&mut rng, 1, 6, 7, InputTransform::Identity, 1),
                ];
                layer(&mut rng, 3, 7, inputs, None, Activation::Silu)
            },
            {
                let inputs = vec![input(&mut rng, 3, 7, 4, InputTransform::Identity, 3)];
                layer(&mut rng, 5, 4, inputs, None, Activation::Relu)
            },
            {
                let inputs = vec![
                    input(&mut rng, 3, 7, 4, InputTransform::Relu, 7),
                    input(&mut rng, 2, 5, 4, InputTransform::NegRelu, 1),
                ];
                layer(&mut rng, 4, 4, inputs, Some(5), Activation::None)
            },
        ];
        // Tensor 5 (the residual) is written by layer 3, before layer 4
        // reads it; tensor ids need not follow layer order.
        let model = SrModel {
            name: "synthetic".to_string(),
            padding,
            input_range,
            tensors,
            layers,
            head,
        };
        model.validate().expect("valid synthetic model");
        model
    }

    #[test]
    fn synthetic_graphs_match_reference() {
        let device = device();
        let variants = [
            model(
                Padding::Clamp,
                InputRange::Full,
                Head {
                    tensor: 4,
                    order: SubpixelOrder::ColumnMajor,
                    base: HeadBase::Bilinear,
                    clamp: false,
                },
                3,
            ),
            model(
                Padding::Zero,
                InputRange::Raw,
                Head {
                    tensor: 4,
                    order: SubpixelOrder::RowMajor,
                    base: HeadBase::None,
                    clamp: true,
                },
                5,
            ),
        ];
        for (index, source) in variants.into_iter().enumerate() {
            let optimized = fold::optimize(&source);
            let borders = optimized
                .layers
                .iter()
                .filter(|layer| layer.border.is_some())
                .count();
            eprintln!(
                "synthetic {index}: {} -> {} layers, {borders} with border variants",
                source.layers.len(),
                optimized.layers.len()
            );
            assert!(borders > 0, "the synthetic graph should fold");
            for (label, model) in [("source", source.clone()), ("folded", optimized)] {
                check_model(
                    &device,
                    &format!("synthetic{index}/{label}"),
                    Arc::new(model.clone()),
                    &[(37, 23), (3, 2)],
                    None,
                );
                check_model(
                    &device,
                    &format!("synthetic{index}/{label}/banded"),
                    Arc::new(model),
                    &[(37, 23)],
                    Some(5),
                );
            }
        }
    }
}

/// Prints the executed graph of every bundled network.
#[test]
#[ignore]
fn dump_graphs() {
    for mode in network_modes() {
        let model = execution_model(mode).expect("model");
        eprintln!(
            "{} padding {:?} range {:?} tensors {:?} head {:?}",
            mode.label(),
            model.padding,
            model.input_range,
            model.tensors,
            model.head
        );
        for (index, layer) in model.layers.iter().enumerate() {
            let inputs: Vec<String> = layer
                .inputs
                .iter()
                .map(|input| format!("t{}:{:?}:k{}", input.tensor, input.transform, input.kernel))
                .collect();
            eprintln!(
                "  L{index}: -> t{} ({}ch) inputs [{}] residual {:?} act {} border {}",
                layer.output,
                layer.out_channels,
                inputs.join(", "),
                layer.residual,
                match &layer.activation {
                    erika::renderer::sr::model::Activation::None => "none",
                    erika::renderer::sr::model::Activation::Relu => "relu",
                    erika::renderer::sr::model::Activation::Silu => "silu",
                    erika::renderer::sr::model::Activation::Prelu(_) => "prelu",
                },
                layer.border.is_some()
            );
        }
    }
}
