#![cfg(feature = "wgpu")]

use erika::renderer::pipeline::LumaUpscalerMode;
use erika::renderer::wgpu_artcnn::{
    WgpuArtCnn, WgpuArtCnnConfig, WgpuArtCnnInput, WgpuArtCnnInputKind, WgpuArtCnnOutput,
    WgpuArtCnnStatus, unpack_packed_d2s_rgba16f,
};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;

fn retain_test_graphics_runtime() {
    // These tests construct wgpu directly and bypass liberika's renderer setup.
    // WSL D3D12 registers TLS destructors that outlive Vulkan/EGL teardown.
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    if std::path::Path::new("/dev/dxg").exists() {
        static RETAIN: std::sync::Once = std::sync::Once::new();
        RETAIN.call_once(|| {
            for library in [
                c"/usr/lib/wsl/lib/libd3d12core.so",
                c"/usr/lib/wsl/lib/libdxcore.so",
            ] {
                // SAFETY: known WSL runtime libraries, retained until exit.
                unsafe {
                    libc::dlopen(
                        library.as_ptr(),
                        libc::RTLD_LAZY | libc::RTLD_LOCAL | libc::RTLD_NODELETE,
                    );
                }
            }
        });
    }
}

fn request_device() -> Option<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    retain_test_graphics_runtime();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        // The emulator's ranchu Vulkan debug-utils implementation dereferences
        // a null object while naming headless test resources.
        flags: wgpu::InstanceFlags::from_build_config() | wgpu::InstanceFlags::DISCARD_HAL_LABELS,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .ok()?;
    let adapter_limits = adapter.limits();
    let required_limits = wgpu::Limits::downlevel_defaults()
        .using_resolution(adapter_limits.clone())
        .using_alignment(adapter_limits);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("erika-wgpu-artcnn-test-device"),
        required_features: wgpu::Features::empty(),
        required_limits,
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((adapter, device, queue))
}

fn request_gles_webgl2_device() -> Option<(wgpu::Adapter, wgpu::Device)> {
    retain_test_graphics_runtime();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::GL,
        flags: wgpu::InstanceFlags::from_build_config() | wgpu::InstanceFlags::DISCARD_HAL_LABELS,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .ok()?;
    let adapter_limits = adapter.limits();
    let required_limits = wgpu::Limits::downlevel_webgl2_defaults()
        .using_resolution(adapter_limits.clone())
        .using_alignment(adapter_limits);
    let (device, _) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("erika-wgpu-artcnn-gles-webgl2-test-device"),
        required_features: wgpu::Features::empty(),
        required_limits,
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((adapter, device))
}

fn load_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four bytes")))
        .collect()
}

fn upload_luma(device: &wgpu::Device, queue: &wgpu::Queue, input: &[f32]) -> wgpu::Texture {
    let bytes: Vec<u8> = input
        .iter()
        .map(|value| (value * 255.0).round().clamp(0.0, 255.0) as u8)
        .collect();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("erika-wgpu-artcnn-test-luma"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(WIDTH),
            rows_per_image: Some(HEIGHT),
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );
    texture
}

fn upload_grayscale_rgb(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    input: &[f32],
) -> wgpu::Texture {
    let mut bytes = Vec::with_capacity(input.len() * 4);
    for value in input {
        let value = (value * 255.0).round().clamp(0.0, 255.0) as u8;
        bytes.extend_from_slice(&[value, value, value, 255]);
    }
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("erika-wgpu-artcnn-test-rgb"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(WIDTH * 4),
            rows_per_image: Some(HEIGHT),
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );
    texture
}

fn encode_once(
    upscaler: &mut WgpuArtCnn,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    input: WgpuArtCnnInput<'_>,
    frame_token: u64,
) -> WgpuArtCnnOutput {
    let validation_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("erika-wgpu-artcnn-test-encoder"),
    });
    let output = upscaler
        .encode(
            device,
            queue,
            &mut encoder,
            input,
            WIDTH,
            HEIGHT,
            Some(frame_token),
        )
        .expect("encode model")
        .expect("enabled output");
    let commands = encoder.finish();
    if !output.cache_hit {
        queue.submit(Some(commands));
    }
    let validation = pollster::block_on(validation_scope.pop());
    assert!(
        validation.is_none(),
        "wgpu validation error: {validation:?}"
    );
    upscaler
        .commit_encoded_output(&output)
        .expect("commit validated ArtCNN output");
    output
}

fn readback_packed(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
) -> Vec<f32> {
    let tight_row = WIDTH * 8;
    let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_row = tight_row.div_ceil(alignment) * alignment;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("erika-wgpu-artcnn-test-readback"),
        size: u64::from(padded_row * HEIGHT),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("erika-wgpu-artcnn-test-readback-encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(HEIGHT),
            },
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));
    let slice = buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        sender.send(result).expect("map receiver alive");
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll readback");
    receiver
        .recv()
        .expect("map callback")
        .expect("map succeeds");
    let mapped = slice.get_mapped_range();
    let output =
        unpack_packed_d2s_rgba16f(&mapped, WIDTH, HEIGHT, padded_row).expect("unpack packed D2S");
    drop(mapped);
    buffer.unmap();
    output
}

struct ErrorStats {
    mae: f64,
    max: f64,
}

fn compare(actual: &[f32], expected: &[f32]) -> ErrorStats {
    assert_eq!(actual.len(), expected.len());
    let mut sum = 0.0;
    let mut max = 0.0f64;
    for (&actual, &expected) in actual.iter().zip(expected) {
        let error = f64::from(actual - expected).abs();
        sum += error;
        max = max.max(error);
    }
    ErrorStats {
        mae: sum / actual.len() as f64,
        max,
    }
}

fn check_model(mode: LumaUpscalerMode, input_bytes: &[u8], expected_bytes: &[u8]) {
    let Some((adapter, device, queue)) = request_device() else {
        eprintln!("skipping {mode:?}: no native wgpu adapter");
        return;
    };
    let input = load_f32(input_bytes);
    let expected = load_f32(expected_bytes);
    let luma = upload_luma(&device, &queue, &input);
    let luma_view = luma.create_view(&wgpu::TextureViewDescriptor::default());
    let mut upscaler = WgpuArtCnn::new_with_config(
        &adapter,
        &device,
        WgpuArtCnnConfig {
            tile_width: 47,
            tile_height: 29,
        },
    );
    if !upscaler.capability().supported {
        eprintln!(
            "skipping {mode:?}: {}",
            upscaler.capability().reasons.join("; ")
        );
        return;
    }
    upscaler.set_mode(&device, mode).expect("build model");
    assert_eq!(upscaler.status(), WgpuArtCnnStatus::Scalar);

    let output = encode_once(
        &mut upscaler,
        &device,
        &queue,
        WgpuArtCnnInput::PlanarLuma {
            view: &luma_view,
            p010_le_bytes: false,
        },
        1,
    );

    let actual = readback_packed(&device, &queue, &output.texture);
    let stats = compare(&actual, &expected);
    eprintln!(
        "{mode:?}/wgpu tiled scalar: mae={:.6} max={:.6}",
        stats.mae, stats.max
    );
    assert!(stats.mae < 1.5e-3, "MAE too high: {}", stats.mae);
    assert!(stats.max < 2.0e-2, "max error too high: {}", stats.max);

    let cached = encode_once(
        &mut upscaler,
        &device,
        &queue,
        WgpuArtCnnInput::PlanarLuma {
            view: &luma_view,
            p010_le_bytes: false,
        },
        1,
    );
    assert!(cached.cache_hit);
    assert_eq!(upscaler.stats().upscaled_frames, 1);
    assert_eq!(upscaler.stats().cache_hits, 1);
}

#[test]
fn c4f16_wgpu_matches_onnx_across_tile_seams() {
    check_model(
        LumaUpscalerMode::ArtCnnC4F16,
        include_bytes!("data/artcnn/c4f16/input_128x72.f32"),
        include_bytes!("data/artcnn/c4f16/output_256x144.f32"),
    );
}

#[test]
fn c4f16_ds_wgpu_matches_onnx_across_tile_seams() {
    check_model(
        LumaUpscalerMode::ArtCnnC4F16Ds,
        include_bytes!("data/artcnn/c4f16/input_128x72.f32"),
        include_bytes!("data/artcnn/c4f16_ds/output_256x144.f32"),
    );
}

#[test]
fn c4f32_wgpu_matches_onnx_across_tile_seams() {
    check_model(
        LumaUpscalerMode::ArtCnnC4F32,
        include_bytes!("data/artcnn/c4f32/input_128x72.f32"),
        include_bytes!("data/artcnn/c4f32/output_256x144.f32"),
    );
}

#[test]
fn nonlinear_rgb_conv0_matches_planar_luma_and_deferred_failure_invalidates_cache() {
    let Some((adapter, device, queue)) = request_device() else {
        eprintln!("skipping RGB ArtCNN test: no native wgpu adapter");
        return;
    };
    let input = load_f32(include_bytes!("data/artcnn/c4f16/input_128x72.f32"));
    let luma = upload_luma(&device, &queue, &input);
    let rgb = upload_grayscale_rgb(&device, &queue, &input);
    let luma_view = luma.create_view(&wgpu::TextureViewDescriptor::default());
    let rgb_view = rgb.create_view(&wgpu::TextureViewDescriptor::default());
    let mut upscaler = WgpuArtCnn::new_with_config(
        &adapter,
        &device,
        WgpuArtCnnConfig {
            tile_width: 47,
            tile_height: 29,
        },
    );
    if !upscaler.capability().supported {
        eprintln!(
            "skipping RGB ArtCNN test: {}",
            upscaler.capability().reasons.join("; ")
        );
        return;
    }
    upscaler
        .set_mode(&device, LumaUpscalerMode::ArtCnnC4F16)
        .expect("build C4F16");

    let planar = encode_once(
        &mut upscaler,
        &device,
        &queue,
        WgpuArtCnnInput::PlanarLuma {
            view: &luma_view,
            p010_le_bytes: false,
        },
        10,
    );
    let planar = readback_packed(&device, &queue, &planar.texture);
    let rgb = encode_once(
        &mut upscaler,
        &device,
        &queue,
        WgpuArtCnnInput::NonlinearRgb {
            view: &rgb_view,
            luma_coefficients: [0.2126, 0.7152, 0.0722],
        },
        11,
    );
    assert_eq!(rgb.input_kind, WgpuArtCnnInputKind::NonlinearRgb);
    let rgb_values = readback_packed(&device, &queue, &rgb.texture);
    let stats = compare(&rgb_values, &planar);
    eprintln!(
        "nonlinear RGB vs planar luma: mae={:.6} max={:.6}",
        stats.mae, stats.max
    );
    assert!(stats.mae < 2.0e-4, "RGB/planar MAE too high: {}", stats.mae);
    assert!(stats.max < 2.0e-3, "RGB/planar max too high: {}", stats.max);

    let committed_stats = upscaler.stats();
    let validation_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut deferred_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("erika-wgpu-artcnn-synthetic-deferred-failure-encoder"),
    });
    let tentative = upscaler
        .encode(
            &device,
            &queue,
            &mut deferred_encoder,
            WgpuArtCnnInput::NonlinearRgb {
                view: &rgb_view,
                luma_coefficients: [0.2126, 0.7152, 0.0722],
            },
            WIDTH,
            HEIGHT,
            Some(12),
        )
        .expect("record tentative encode")
        .expect("enabled tentative output");
    assert!(!tentative.cache_hit);
    let commands = deferred_encoder.finish();
    let tentative_stats = upscaler.stats();
    assert_eq!(
        tentative_stats.upscaled_frames,
        committed_stats.upscaled_frames
    );
    assert_eq!(tentative_stats.encoded_tiles, committed_stats.encoded_tiles);
    assert_eq!(
        tentative_stats.compute_dispatches,
        committed_stats.compute_dispatches
    );
    assert_eq!(
        tentative_stats.last_encode_duration,
        committed_stats.last_encode_duration
    );

    tentative.texture.destroy();
    queue.submit(Some(commands));
    let deferred_validation = pollster::block_on(validation_scope.pop())
        .expect("destroyed tentative output must fail validation during submission");
    let failure = upscaler.handle_deferred_encode_failure(
        WgpuArtCnnInputKind::NonlinearRgb,
        format!("synthetic deferred validation failure: {deferred_validation}"),
    );
    assert_eq!(failure.input_kind, Some(WgpuArtCnnInputKind::NonlinearRgb));
    assert_eq!(upscaler.status(), WgpuArtCnnStatus::Inactive);
    assert_eq!(upscaler.mode(), LumaUpscalerMode::ArtCnnC4F16);
    let failed_stats = upscaler.stats();
    assert_eq!(
        failed_stats.fallback_count,
        committed_stats.fallback_count + 1
    );
    assert_eq!(
        failed_stats.upscaled_frames,
        committed_stats.upscaled_frames
    );
    assert_eq!(failed_stats.encoded_tiles, committed_stats.encoded_tiles);
    assert_eq!(
        failed_stats.compute_dispatches,
        committed_stats.compute_dispatches
    );
    assert_eq!(
        failed_stats.last_encode_duration,
        committed_stats.last_encode_duration
    );
    upscaler
        .set_mode(&device, LumaUpscalerMode::ArtCnnC4F16)
        .expect("same requested mode rebuilds after deferred failure");
    let rebuilt = encode_once(
        &mut upscaler,
        &device,
        &queue,
        WgpuArtCnnInput::NonlinearRgb {
            view: &rgb_view,
            luma_coefficients: [0.2126, 0.7152, 0.0722],
        },
        12,
    );
    assert!(!rebuilt.cache_hit, "invalidated token must be recomputed");
}

#[test]
fn p010_byte_pair_luma_matches_float_input() {
    let Some((adapter, device, queue)) = request_device() else {
        return;
    };
    let mut upscaler = WgpuArtCnn::new(&adapter, &device);
    if !upscaler.capability().supported {
        return;
    }
    upscaler
        .set_mode(&device, LumaUpscalerMode::ArtCnnC4F16)
        .unwrap();
    let words: Vec<u16> = (0..WIDTH * HEIGHT)
        .map(|i| ((64 + i * 13 % 877) as u16) << 6)
        .collect();
    let packed: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    let reference: Vec<u8> = words
        .iter()
        .flat_map(|word| (*word as f32 / 65535.0).to_le_bytes())
        .collect();
    let make = |format, data: &[u8], stride| {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("P010 ArtCNN regression input"),
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(HEIGHT),
            },
            texture.size(),
        );
        texture
    };
    let float_texture = make(wgpu::TextureFormat::R32Float, &reference, WIDTH * 4);
    let bytes_texture = make(wgpu::TextureFormat::Rg8Unorm, &packed, WIDTH * 2);
    let float_view = float_texture.create_view(&Default::default());
    let bytes_view = bytes_texture.create_view(&Default::default());
    let expected = encode_once(
        &mut upscaler,
        &device,
        &queue,
        WgpuArtCnnInput::PlanarLuma {
            view: &float_view,
            p010_le_bytes: false,
        },
        90,
    );
    let expected = readback_packed(&device, &queue, &expected.texture);
    let actual = encode_once(
        &mut upscaler,
        &device,
        &queue,
        WgpuArtCnnInput::PlanarLuma {
            view: &bytes_view,
            p010_le_bytes: true,
        },
        91,
    );
    let actual = readback_packed(&device, &queue, &actual.texture);
    let error = compare(&actual, &expected);
    assert!(
        error.max < 0.002,
        "P010 byte reconstruction changed ArtCNN output: {}",
        error.max
    );
}

#[test]
fn gles_webgl2_limits_report_explicit_sr_fallback_without_building_compute() {
    let Some((adapter, device)) = request_gles_webgl2_device() else {
        eprintln!("skipping GLES fallback test: no headless GLES adapter");
        return;
    };
    let mut upscaler = WgpuArtCnn::new(&adapter, &device);
    assert!(!upscaler.capability().supported);
    let failure = upscaler
        .set_mode(&device, LumaUpscalerMode::ArtCnnC4F16)
        .expect_err("WebGL2/GLES3.0 limits must not build an ArtCNN compute pipeline");
    assert_eq!(upscaler.status(), WgpuArtCnnStatus::Inactive);
    assert_eq!(failure.fallback, "native_luma_sampling");
    assert_eq!(upscaler.stats().fallback_count, 1);
    let diagnostic = failure.diagnostic_json();
    assert_eq!(diagnostic["stage"], "capability");
    assert_eq!(diagnostic["fallback"], "native_luma_sampling");
    let reason = failure.reason;
    assert!(
        reason.contains("max_storage_buffers_per_shader_stage=0")
            || reason.contains("max_storage_textures_per_shader_stage=0")
            || reason.contains("max_compute_invocations_per_workgroup=0"),
        "unexpected GLES fallback reason: {reason}"
    );
}
