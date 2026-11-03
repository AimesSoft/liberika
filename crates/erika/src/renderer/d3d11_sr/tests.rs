use super::*;
use ::windows::Win32::Foundation::HMODULE;
use ::windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
    D3D_SRV_DIMENSION_TEXTURE2DARRAY,
};
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_TEX2D_ARRAY_SRV, D3D11_USAGE_STAGING, D3D11CreateDevice,
};
use ::windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R32_FLOAT};

use crate::renderer::sr::catalog::source_model;
use crate::renderer::sr::model::f16_to_f32;
use crate::renderer::sr::reference::{run, test_pattern};

fn create_device(driver: D3D_DRIVER_TYPE) -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    use ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use ::windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let adapter = std::env::var("ERIKA_SR_ADAPTER").ok().and_then(|name| {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
        let mut index = 0;
        loop {
            let adapter = unsafe { factory.EnumAdapters1(index) }.ok()?;
            let desc = unsafe { adapter.GetDesc1() }.ok()?;
            let label = String::from_utf16_lossy(&desc.Description);
            eprintln!(
                "DXGI adapter {index}: {} vendor {:04x}",
                label.trim_end_matches('\0'),
                desc.VendorId
            );
            if label.to_lowercase().contains(&name.to_lowercase()) || name == index.to_string() {
                return adapter
                    .cast::<::windows::Win32::Graphics::Dxgi::IDXGIAdapter>()
                    .ok();
            }
            index += 1;
        }
    });
    if std::env::var_os("ERIKA_SR_ADAPTER").is_some() && adapter.is_none() {
        panic!("requested DXGI adapter unavailable");
    }
    let mut device = None;
    let mut context = None;
    let mut selected = D3D_FEATURE_LEVEL_11_0;
    let result = unsafe {
        D3D11CreateDevice(
            adapter.as_ref(),
            if adapter.is_some() {
                D3D_DRIVER_TYPE_UNKNOWN
            } else {
                driver
            },
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut selected),
            Some(&mut context),
        )
    };
    if result.is_err() {
        return None;
    }
    Some((device?, context?))
}

pub(crate) fn test_device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    create_device(D3D_DRIVER_TYPE_HARDWARE).or_else(|| create_device(D3D_DRIVER_TYPE_WARP))
}

/// R32_FLOAT luma (exact values) as a Texture2DArray SRV.
pub(crate) fn upload_luma(
    device: &ID3D11Device,
    luma: &[f32],
    width: u32,
    height: u32,
) -> ID3D11ShaderResourceView {
    upload(
        device,
        luma.as_ptr().cast(),
        width * 4,
        width,
        height,
        DXGI_FORMAT_R32_FLOAT,
    )
}

fn upload(
    device: &ID3D11Device,
    data: *const c_void,
    pitch: u32,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> ID3D11ShaderResourceView {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: data,
        SysMemPitch: pitch,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, Some(&initial), Some(&mut texture))
            .expect("create luma");
    }
    let resource: ID3D11Resource = texture.unwrap().cast().unwrap();
    let view_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: 0,
                ArraySize: 1,
            },
        },
    };
    let mut view = None;
    unsafe {
        device
            .CreateShaderResourceView(&resource, Some(&view_desc), Some(&mut view))
            .expect("luma SRV");
    }
    view.unwrap()
}

pub(crate) fn readback_r16f(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    view: &ID3D11ShaderResourceView,
    width: u32,
    height: u32,
) -> Vec<f32> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .unwrap();
    }
    let staging: ID3D11Resource = staging.unwrap().cast().unwrap();
    let source = unsafe { view.GetResource() }.unwrap();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let mut out = Vec::with_capacity((width * height) as usize);
    unsafe {
        context.CopyResource(&staging, &source);
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .unwrap();
        for y in 0..height as usize {
            let row = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                width as usize * 2,
            );
            for x in 0..width as usize {
                out.push(f16_to_f32(u16::from_le_bytes([row[x * 2], row[x * 2 + 1]])));
            }
        }
        context.Unmap(&staging, 0);
    }
    out
}

fn wait_until_built(engine: &mut D3d11SrEngine, device: &ID3D11Device) -> Duration {
    let started = Instant::now();
    while !engine.is_active() {
        engine.poll(device);
        if let Some(reason) = engine.last_failure() {
            panic!("SR build failed: {reason}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(600),
            "SR build timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    started.elapsed()
}

fn network_modes() -> Vec<LumaUpscalerMode> {
    LumaUpscalerMode::ALL
        .into_iter()
        .filter(|mode| {
            mode.family() == UpscalerFamily::Network && *mode != LumaUpscalerMode::Custom
        })
        .collect()
}

fn benchmark_modes() -> Vec<LumaUpscalerMode> {
    let mut modes = network_modes();
    if std::env::var_os("ERIKA_SR_BENCH_CUSTOM").is_some() {
        modes.push(LumaUpscalerMode::Custom);
    }
    modes
}

fn benchmark_model(mode: LumaUpscalerMode) -> (Arc<SrModel>, u64) {
    if mode == LumaUpscalerMode::Custom {
        let path = std::env::var_os("ERIKA_SR_BENCH_CUSTOM").expect("custom model path");
        let bytes = std::fs::read(&path).expect("read supplied custom model");
        let source = crate::renderer::sr::catalog::load_custom("benchmark custom", &bytes).unwrap();
        let macs = source.macs_per_pixel();
        (Arc::new(crate::renderer::sr::fold::optimize(&source)), macs)
    } else {
        (
            execution_model(mode).unwrap(),
            source_model(mode).unwrap().macs_per_pixel(),
        )
    }
}

pub(crate) fn compare(actual: &[f32], expected: &[f32]) -> (f64, f64) {
    let (mut sum, mut max) = (0.0f64, 0.0f64);
    for (a, b) in actual.iter().zip(expected) {
        let error = f64::from((a - b).abs());
        sum += error;
        max = max.max(error);
    }
    (sum / actual.len() as f64, max)
}

#[test]
fn kernels_compile_for_every_bundled_network() {
    for mode in network_modes() {
        let model = execution_model(mode).unwrap();
        let plan = plan(&model);
        let started = Instant::now();
        let mut unique = std::collections::HashSet::new();
        for kernel in &plan.kernels {
            if unique.insert(kernel.source_code.clone()) {
                if let Err(error) = compile_cached(&kernel.source_code) {
                    let numbered: String = kernel
                        .source_code
                        .lines()
                        .enumerate()
                        .take(80)
                        .map(|(index, line)| format!("{:4} {line}\n", index + 1))
                        .collect();
                    panic!("{}: {error}\n{numbered}", mode.label());
                }
            }
        }
        eprintln!(
            "{:<22} {} unique kernels compiled in {:.2}s",
            mode.label(),
            unique.len(),
            started.elapsed().as_secs_f64()
        );
    }
}

fn check_mode(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    mode: LumaUpscalerMode,
    width: u32,
    height: u32,
    range: LumaRange,
) {
    check_mode_with_budget(device, context, mode, width, height, range, None);
}

fn check_mode_with_budget(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    mode: LumaUpscalerMode,
    width: u32,
    height: u32,
    range: LumaRange,
    budget: Option<usize>,
) {
    let luma = test_pattern(width as usize, height as usize, 11);
    let view = upload_luma(device, &luma, width, height);
    let mut engine = D3d11SrEngine {
        tensor_budget: budget,
        ..Default::default()
    };
    engine.set_mode(device, mode).unwrap();
    let build = wait_until_built(&mut engine, device);
    let output = engine
        .encode(device, context, &view, width, height, range, Some(1))
        .unwrap()
        .expect("upscaled output");
    assert_eq!((output.width, output.height), (width * 2, height * 2));
    let actual = readback_r16f(device, context, &output.view, output.width, output.height);
    let expected = run(
        &source_model(mode).unwrap(),
        &luma,
        width as usize,
        height as usize,
        range,
    );
    let (mae, max) = compare(&actual, &expected);
    eprintln!(
        "{:<22} {width}x{height} {range:?}: mae {mae:.6} max {max:.6} (build {:.2}s)",
        mode.label(),
        build.as_secs_f64()
    );
    assert!(
        mae < 2e-3 && max < 3e-2,
        "{} mae {mae} max {max}",
        mode.label()
    );

    // A repeated token reuses the cached output.
    let frames = engine.upscaled_frames();
    engine
        .encode(device, context, &view, width, height, range, Some(1))
        .unwrap()
        .unwrap();
    assert_eq!(engine.upscaled_frames(), frames);
}

#[test]
fn every_bundled_network_matches_the_reference() {
    let Some((device, context)) = test_device() else {
        eprintln!("skipping: no D3D11 device");
        return;
    };
    for mode in network_modes() {
        let (width, height) = if mode == LumaUpscalerMode::ArtCnnR8F64 {
            (23, 17)
        } else {
            (67, 45)
        };
        for range in [LumaRange::Full, LumaRange::Limited] {
            check_mode(&device, &context, mode, width, height, range);
        }
    }
    check_mode(
        &device,
        &context,
        LumaUpscalerMode::Anime4kUl,
        33,
        21,
        LumaRange::Limited,
    );
    check_mode(
        &device,
        &context,
        LumaUpscalerMode::ArtCnnC4F16,
        2,
        2,
        LumaRange::Full,
    );
}

#[test]
fn tiled_execution_matches_the_reference() {
    let Some((device, context)) = test_device() else {
        eprintln!("skipping: no D3D11 device");
        return;
    };
    // A 1 MiB budget splits 375x250 into many tiles.
    let budget = 1024 * 1024;
    let (tx, ty) = tile_grid(375, 250, 7, 128, budget);
    assert!(tx * ty > 4, "{tx}x{ty}");
    for mode in [LumaUpscalerMode::ArtCnnC4F32, LumaUpscalerMode::Anime4kVl] {
        check_mode_with_budget(
            &device,
            &context,
            mode,
            375,
            250,
            LumaRange::Full,
            Some(budget),
        );
    }
    for mode in network_modes() {
        for range in [LumaRange::Full, LumaRange::Limited] {
            check_mode_with_budget(&device, &context, mode, 43, 29, range, Some(16 * 1024));
        }
    }
}

#[test]
fn custom_prelu_residual_graph_matches_the_reference() {
    use crate::renderer::sr::model::{Activation, HeadBase, InputRange, SubpixelOrder};
    let Some((device, context)) = test_device() else {
        return;
    };
    let mut model = execution_model(LumaUpscalerMode::Anime4kS)
        .unwrap()
        .as_ref()
        .clone();
    model.name = "custom PReLU residual".to_string();
    model.input_range = InputRange::Raw;
    model.layers[1].activation = Activation::Prelu(vec![0.2; model.layers[1].out_channels]);
    model.layers[1].residual = Some(model.layers[0].output);
    model.head.order = SubpixelOrder::ColumnMajor;
    model.head.base = HeadBase::None;
    model.validate().unwrap();
    let model = Arc::new(model);
    let (width, height) = (71u32, 47u32);
    let luma = test_pattern(width as usize, height as usize, 13);
    let view = upload_luma(&device, &luma, width, height);
    let mut engine = D3d11SrEngine {
        tensor_budget: Some(16 * 1024),
        ..Default::default()
    };
    engine.set_custom_model(&device, Some(model.clone()));
    engine.set_mode(&device, LumaUpscalerMode::Custom).unwrap();
    wait_until_built(&mut engine, &device);
    for range in [LumaRange::Full, LumaRange::Limited] {
        let output = engine
            .encode(&device, &context, &view, width, height, range, None)
            .unwrap()
            .unwrap();
        let actual = readback_r16f(&device, &context, &output.view, width * 2, height * 2);
        let expected = run(&model, &luma, width as usize, height as usize, range);
        let (mae, max) = compare(&actual, &expected);
        eprintln!("custom {range:?}: mae {mae:.6} max {max:.6}");
        assert!(mae < 2e-3 && max < 3e-2, "custom mae {mae} max {max}");
    }
}

fn timed<T>(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    work: impl FnOnce() -> T,
) -> (T, f64) {
    let make = |kind| {
        let desc = D3D11_QUERY_DESC {
            Query: kind,
            MiscFlags: 0,
        };
        let mut query = None;
        unsafe { device.CreateQuery(&desc, Some(&mut query)) }.unwrap();
        query.unwrap()
    };
    let disjoint = make(D3D11_QUERY_TIMESTAMP_DISJOINT);
    let start = make(D3D11_QUERY_TIMESTAMP);
    let end = make(D3D11_QUERY_TIMESTAMP);
    unsafe {
        context.Begin(&disjoint);
        context.End(&start);
    }
    let value = work();
    unsafe {
        context.End(&end);
        context.End(&disjoint);
        context.Flush();
    }
    let timeout = std::env::var("ERIKA_SR_QUERY_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120)
        .max(1);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let last_check = std::cell::Cell::new(Instant::now());
    let check = || {
        if last_check.get().elapsed() >= Duration::from_secs(1) {
            let reason = unsafe { device.GetDeviceRemovedReason() };
            assert!(reason.is_ok(), "GPU timestamp device failure: {reason:?}");
            last_check.set(Instant::now());
        }
        assert!(
            Instant::now() < deadline,
            "GPU timestamp timeout after {timeout}s"
        );
    };
    let wait = |query: &ID3D11Query| loop {
        if let Some(value) = read_query::<u64>(context, query, |value| *value != 0) {
            return value;
        }
        check();
        std::hint::spin_loop();
    };
    let frequency = loop {
        if let Some(data) =
            read_query::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>(context, &disjoint, |data| {
                data.Frequency != 0
            })
        {
            assert!(!data.Disjoint.as_bool());
            break data.Frequency;
        }
        check();
    };
    let (t0, t1) = (wait(&start), wait(&end));
    (value, (t1 - t0) as f64 / frequency as f64 * 1000.0)
}

/// `cargo test -p erika --lib --release d3d11_sr_gpu_benchmark -- --ignored --nocapture`
/// Optional `ERIKA_SR_BENCH_MODES=artcnn_c4f16,anime4k_ul`.
#[test]
#[ignore = "GPU benchmark"]
fn d3d11_sr_gpu_benchmark() {
    let Some((device, context)) = create_device(D3D_DRIVER_TYPE_HARDWARE) else {
        eprintln!("skipping: no hardware D3D11 device");
        return;
    };
    let filter = std::env::var("ERIKA_SR_BENCH_MODES").ok();
    let sizes = std::env::var("ERIKA_SR_BENCH_SIZES")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(|size| {
                    let (w, h) = size.split_once('x').expect("WIDTHxHEIGHT");
                    (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![(1280u32, 720u32), (1920, 1080), (3840, 2160)]);
    let samples = std::env::var("ERIKA_SR_BENCH_SAMPLES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20)
        .max(1);
    let warmup = std::env::var("ERIKA_SR_BENCH_WARMUP")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(3);
    eprintln!(
        "\n{:<22} {:<9} {:>9} {:>9} {:>9} {:>8} {:>9}",
        "mode", "input", "gpu_med", "gpu_min", "encode_ms", "TFLOPS", "src-eq"
    );
    for mode in benchmark_modes() {
        if let Some(filter) = filter.as_deref() {
            if !filter.split(',').any(|label| label == mode.label()) {
                continue;
            }
        }
        let (model, original) = benchmark_model(mode);
        let executed = model.macs_per_pixel() as f64;
        let original = original as f64;
        let mut engine = D3d11SrEngine {
            custom: (mode == LumaUpscalerMode::Custom).then_some(model),
            ..Default::default()
        };
        engine.set_mode(&device, mode).unwrap();
        let build = wait_until_built(&mut engine, &device);
        eprintln!("BUILD {} {:.3}s", mode.label(), build.as_secs_f64());
        for &(width, height) in &sizes {
            if mode == LumaUpscalerMode::ArtCnnR8F64
                && width > 1280
                && std::env::var_os("ERIKA_SR_BENCH_R8_FULL").is_none()
            {
                continue;
            }
            let bytes: Vec<u8> = test_pattern(width as usize, height as usize, 3)
                .iter()
                .map(|value| (value * 255.0).round() as u8)
                .collect();
            let view = upload(
                &device,
                bytes.as_ptr().cast(),
                width,
                width,
                height,
                DXGI_FORMAT_R8_UNORM,
            );
            let mut gpu = Vec::new();
            let mut cpu = Vec::new();
            for token in 0..samples + warmup {
                let (encode_ms, ms) = timed(&device, &context, || {
                    let started = Instant::now();
                    engine
                        .encode(
                            &device,
                            &context,
                            &view,
                            width,
                            height,
                            LumaRange::Limited,
                            Some(token),
                        )
                        .unwrap()
                        .unwrap();
                    started.elapsed().as_secs_f64() * 1000.0
                });
                if token >= warmup {
                    gpu.push(ms);
                    cpu.push(encode_ms);
                }
            }
            gpu.sort_by(f64::total_cmp);
            cpu.sort_by(f64::total_cmp);
            let median = gpu[gpu.len() / 2];
            let pixels = f64::from(width) * f64::from(height);
            eprintln!(
                "{:<22} {:>4}x{:<4} {:>8.3}ms {:>8.3}ms {:>8.3}ms {:>8.2} {:>9.2}",
                mode.label(),
                width,
                height,
                median,
                gpu[0],
                cpu[cpu.len() / 2],
                2.0 * executed * pixels / (median / 1000.0) / 1e12,
                2.0 * original * pixels / (median / 1000.0) / 1e12,
            );
        }
    }
}

/// Compile time of the largest kernel per model under each FXC flag set.
#[test]
#[ignore = "compiler timing"]
fn fxc_compile_time_by_flags() {
    use ::windows::Win32::Graphics::Direct3D::Fxc::{
        D3DCOMPILE_OPTIMIZATION_LEVEL0, D3DCOMPILE_OPTIMIZATION_LEVEL1,
        D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCOMPILE_SKIP_OPTIMIZATION,
    };
    for mode in [
        LumaUpscalerMode::ArtCnnC4F16,
        LumaUpscalerMode::ArtCnnC4F32,
        LumaUpscalerMode::Anime4kUl,
        LumaUpscalerMode::ArtCnnR8F64,
    ] {
        let model = execution_model(mode).unwrap();
        let plan = plan(&model);
        let kernel = plan
            .kernels
            .iter()
            .max_by_key(|kernel| kernel.source_code.len())
            .unwrap();
        for (label, flags) in [
            ("skip", D3DCOMPILE_SKIP_OPTIMIZATION),
            ("O0", D3DCOMPILE_OPTIMIZATION_LEVEL0),
            ("O1", D3DCOMPILE_OPTIMIZATION_LEVEL1),
            ("O3", D3DCOMPILE_OPTIMIZATION_LEVEL3),
        ] {
            let started = Instant::now();
            let result = compile_with_flags(&kernel.source_code, flags);
            eprintln!(
                "{:<16} {:>7} lines {label:>4}: {:.2}s {}",
                mode.label(),
                kernel.source_code.lines().count(),
                started.elapsed().as_secs_f64(),
                result
                    .map(|blob| format!("{} bytes", blob.len()))
                    .unwrap_or_else(|e| e)
            );
            if started.elapsed() > Duration::from_secs(120) {
                break;
            }
        }
    }
}

/// Uses the production background pool/deduplication/cache. Disable the
/// shader cache to measure cold FXC wall time, independently of GPU timing.
#[test]
#[ignore = "compiler timing and DXBC inspection"]
fn d3d11_sr_compile_benchmark() {
    use ::windows::Win32::Graphics::Direct3D::Fxc::D3DDisassemble;
    let filter = std::env::var("ERIKA_SR_BENCH_MODES").ok();
    for mode in benchmark_modes() {
        if filter
            .as_ref()
            .is_some_and(|filter| !filter.split(',').any(|name| name == mode.label()))
        {
            continue;
        }
        let (model, _) = benchmark_model(mode);
        let started = Instant::now();
        let plan = plan(&model);
        let blobs = compile_plan(&plan).expect("compile plan");
        eprintln!(
            "COLD {} {:.3}s kernels {} blobs {}",
            mode.label(),
            started.elapsed().as_secs_f64(),
            plan.kernels.len(),
            blobs.len()
        );
        for (index, blob) in blobs.iter().enumerate() {
            let listing =
                unsafe { D3DDisassemble(blob.as_ptr().cast(), blob.len(), 0, PCSTR::null()) }
                    .unwrap();
            let listing = String::from_utf8_lossy(blob_bytes(&listing));
            let temps = listing
                .lines()
                .find(|line| line.contains("dcl_temps"))
                .unwrap_or("no temps");
            let arrays = listing
                .lines()
                .filter(|line| line.contains("dcl_indexableTemp"))
                .count();
            eprintln!(
                "DXBC {} k{index} {} bytes {temps} indexable {arrays}",
                mode.label(),
                blob.len()
            );
            if let Ok(path) = std::env::var("ERIKA_SR_DXBC_DIR") {
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(
                    std::path::Path::new(&path).join(format!("{}_k{index}.txt", mode.label())),
                    listing.as_bytes(),
                )
                .unwrap();
            }
        }
    }
    let (source, _, _) = D3d11Fsr1::shader_source();
    let started = Instant::now();
    let blob = compile_cached(&source).expect("compile FSR1");
    eprintln!(
        "COLD fsr1 {:.3}s kernels 1 blobs 1",
        started.elapsed().as_secs_f64()
    );
    let listing =
        unsafe { D3DDisassemble(blob.as_ptr().cast(), blob.len(), 0, PCSTR::null()) }.unwrap();
    let listing = String::from_utf8_lossy(blob_bytes(&listing));
    let temps = listing
        .lines()
        .find(|line| line.contains("dcl_temps"))
        .unwrap_or("no temps");
    let arrays = listing
        .lines()
        .filter(|line| line.contains("dcl_indexableTemp"))
        .count();
    eprintln!(
        "DXBC fsr1 k0 {} bytes {temps} indexable {arrays}",
        blob.len()
    );
    assert_eq!(arrays, 0, "FSR1 register arrays");
    if let Ok(path) = std::env::var("ERIKA_SR_DXBC_DIR") {
        std::fs::write(
            std::path::Path::new(&path).join("fsr1_k0.txt"),
            listing.as_bytes(),
        )
        .unwrap();
    }
}

#[test]
fn supplied_custom_graph_matches_the_reference() {
    let Some(path) = std::env::var_os("ERIKA_SR_BENCH_CUSTOM") else {
        return;
    };
    let bytes = std::fs::read(path).unwrap();
    let source = crate::renderer::sr::catalog::load_custom("supplied custom", &bytes).unwrap();
    let model = Arc::new(crate::renderer::sr::fold::optimize(&source));
    let Some((device, context)) = test_device() else {
        return;
    };
    let (width, height) = (43u32, 29u32);
    let luma = test_pattern(width as usize, height as usize, 19);
    let view = upload_luma(&device, &luma, width, height);
    for budget in [None, Some(16 * 1024)] {
        let mut engine = D3d11SrEngine {
            custom: Some(model.clone()),
            tensor_budget: budget,
            ..Default::default()
        };
        engine.set_mode(&device, LumaUpscalerMode::Custom).unwrap();
        wait_until_built(&mut engine, &device);
        for range in [LumaRange::Full, LumaRange::Limited] {
            let output = engine
                .encode(&device, &context, &view, width, height, range, None)
                .unwrap()
                .unwrap();
            let actual = readback_r16f(&device, &context, &output.view, width * 2, height * 2);
            let expected = run(&source, &luma, width as usize, height as usize, range);
            let (mae, max) = compare(&actual, &expected);
            eprintln!("supplied custom {range:?} budget {budget:?}: mae {mae:.6} max {max:.6}");
            assert!(mae < 2e-3 && max < 3e-2, "custom mae {mae} max {max}");
        }
    }
}

#[test]
fn fsr1_large_coordinates_match_the_reference() {
    use crate::renderer::sr::fsr1::{DEFAULT_SHARPNESS_STOPS, easu, rcas, rcas_strength};
    let Some((device, context)) = test_device() else {
        return;
    };
    let source = (1031u32, 577u32);
    let target = (4097u32, 2305u32);
    let luma = test_pattern(source.0 as usize, source.1 as usize, 23);
    let view = upload_luma(&device, &luma, source.0, source.1);
    let mut fsr = D3d11Fsr1::default();
    let started = Instant::now();
    let output = loop {
        if let Some(output) = fsr
            .encode(&device, &context, &view, source, target, None)
            .unwrap()
        {
            break output;
        }
        assert!(fsr.last_failure().is_none(), "{:?}", fsr.last_failure());
        assert!(started.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(5));
    };
    let actual = readback_r16f(&device, &context, &output.view, target.0, target.1);
    let upscaled = easu(
        &luma,
        source.0 as usize,
        source.1 as usize,
        target.0 as usize,
        target.1 as usize,
    );
    let expected = rcas(
        &upscaled,
        target.0 as usize,
        target.1 as usize,
        rcas_strength(DEFAULT_SHARPNESS_STOPS),
    );
    let (mae, max) = compare(&actual, &expected);
    eprintln!("fsr1 large {source:?}->{target:?}: mae {mae:.6} max {max:.6}");
    assert!(mae < 1e-3 && max < 1e-2, "large FSR1 mae {mae} max {max}");
}

#[test]
fn fsr1_matches_the_cpu_reference() {
    use crate::renderer::sr::fsr1::{DEFAULT_SHARPNESS_STOPS, easu, rcas, rcas_strength};
    let Some((device, context)) = test_device() else {
        eprintln!("skipping: no D3D11 device");
        return;
    };
    let (width, height) = (67u32, 45u32);
    let luma = test_pattern(width as usize, height as usize, 5);
    let view = upload_luma(&device, &luma, width, height);
    let mut fsr = D3d11Fsr1::default();
    for target in [(150u32, 101u32), (134, 90), (201, 135)] {
        let started = Instant::now();
        let output = loop {
            if let Some(output) = fsr
                .encode(
                    &device,
                    &context,
                    &view,
                    (width, height),
                    target,
                    Some(u64::from(target.0)),
                )
                .unwrap()
            {
                break output;
            }
            assert!(fsr.last_failure().is_none(), "{:?}", fsr.last_failure());
            assert!(started.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(5));
        };
        let actual = readback_r16f(&device, &context, &output.view, target.0, target.1);
        let upscaled = easu(
            &luma,
            width as usize,
            height as usize,
            target.0 as usize,
            target.1 as usize,
        );
        let expected = rcas(
            &upscaled,
            target.0 as usize,
            target.1 as usize,
            rcas_strength(DEFAULT_SHARPNESS_STOPS),
        );
        let (mae, max) = compare(&actual, &expected);
        eprintln!("fsr1 {width}x{height} -> {target:?}: mae {mae:.6} max {max:.6}");
        assert!(mae < 1e-3 && max < 1e-2, "mae {mae} max {max}");
    }
}

/// `cargo test -p erika --lib --release d3d11_fsr1_gpu_benchmark -- --ignored --nocapture`
#[test]
#[ignore = "GPU benchmark"]
fn d3d11_fsr1_gpu_benchmark() {
    let Some((device, context)) = create_device(D3D_DRIVER_TYPE_HARDWARE) else {
        eprintln!("skipping: no hardware D3D11 device");
        return;
    };
    for (source, target) in [
        ((1280u32, 720u32), (2560u32, 1440u32)),
        ((1920, 1080), (3840, 2160)),
        ((1280, 720), (3840, 2160)),
        ((3840, 2160), (7680, 4320)),
    ] {
        let bytes: Vec<u8> = test_pattern(source.0 as usize, source.1 as usize, 3)
            .iter()
            .map(|value| (value * 255.0).round() as u8)
            .collect();
        let view = upload(
            &device,
            bytes.as_ptr().cast(),
            source.0,
            source.0,
            source.1,
            DXGI_FORMAT_R8_UNORM,
        );
        let mut fsr = D3d11Fsr1::default();
        let started = Instant::now();
        while fsr
            .encode(&device, &context, &view, source, target, None)
            .unwrap()
            .is_none()
        {
            assert!(started.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut gpu = Vec::new();
        for token in 0..23u64 {
            let (_, ms) = timed(&device, &context, || {
                fsr.encode(&device, &context, &view, source, target, Some(token))
                    .unwrap()
                    .unwrap()
            });
            if token >= 3 {
                gpu.push(ms);
            }
        }
        gpu.sort_by(f64::total_cmp);
        eprintln!(
            "fsr1 {source:?} -> {target:?}: median {:.3} ms, min {:.3} ms",
            gpu[gpu.len() / 2],
            gpu[0]
        );
    }
}
