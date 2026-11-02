#![cfg(feature = "wgpu")]

use erika::renderer::{
    pipeline::{LumaUpscalerMode as Mode, UpscalerFamily},
    sr::{
        catalog, fsr1,
        model::{
            self, Activation, ConvInput, ConvLayer, Head, HeadBase, InputRange, InputTransform,
            Padding, SrModel, SubpixelOrder,
        },
        reference::{self, LumaRange},
        wgsl::Options,
    },
    wgpu_sr::{Config, Input, Output, Status, WgpuSr},
};
use std::{sync::Arc, time::Instant};

fn test_adapter_config(a: &wgpu::Adapter) -> Config {
    Config {
        autotune: false,
        ..Config::for_adapter(a)
    }
}

fn gpu() -> Option<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    if std::path::Path::new("/dev/dxg").exists() {
        unsafe {
            for lib in [
                c"/usr/lib/wsl/lib/libd3d12core.so",
                c"/usr/lib/wsl/lib/libdxcore.so",
            ] {
                libc::dlopen(
                    lib.as_ptr(),
                    libc::RTLD_LAZY | libc::RTLD_LOCAL | libc::RTLD_NODELETE,
                );
            }
        }
    }
    let backend = match std::env::var("WGPU_BACKEND").as_deref() {
        Ok("gl") => wgpu::Backends::GL,
        Ok("vulkan") => wgpu::Backends::VULKAN,
        Ok("dx12") => wgpu::Backends::DX12,
        _ => wgpu::Backends::PRIMARY,
    };
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: backend,
        flags: wgpu::InstanceFlags::from_build_config() | wgpu::InstanceFlags::DISCARD_HAL_LABELS,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = if let Ok(name) = std::env::var("ERIKA_SR_ADAPTER") {
        pollster::block_on(instance.enumerate_adapters(backend))
            .into_iter()
            .find(|a| {
                a.get_info()
                    .name
                    .to_lowercase()
                    .contains(&name.to_lowercase())
            })
            .expect("requested adapter name not found")
    } else {
        match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        })) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("No GPU adapter: {e}");
                return None;
            }
        }
    };
    eprintln!(
        "GPU {:?}, features {:?}",
        adapter.get_info(),
        adapter.features()
    );
    eprintln!("COOPERATIVE {:?}", adapter.cooperative_matrix_properties());
    let mut limits = wgpu::Limits::downlevel_defaults()
        .using_resolution(adapter.limits())
        .using_alignment(adapter.limits());
    limits.max_compute_workgroup_storage_size = adapter
        .limits()
        .max_compute_workgroup_storage_size
        .min(32768);
    limits.max_uniform_buffer_binding_size =
        adapter.limits().max_uniform_buffer_binding_size.min(65536);
    let mut features = adapter.features()
        & (wgpu::Features::SHADER_F16
            | wgpu::Features::TIMESTAMP_QUERY
            | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS
            | wgpu::Features::SUBGROUP);
    features |= adapter.features() & wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES;
    if std::env::var_os("ERIKA_SR_NO_TIMESTAMP").is_some() {
        features &=
            !(wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
    }
    if std::env::var("ERIKA_WGPU_SR_COOPERATIVE").as_deref() == Ok("1")
        || (std::env::var_os("ERIKA_WGPU_SR_COOPERATIVE").is_none()
            && Config::vulkan_matrix_supported(&adapter))
    {
        features |= adapter.features() & wgpu::Features::EXPERIMENTAL_COOPERATIVE_MATRIX;
    }
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("SR-test"),
        required_features: features,
        required_limits: limits,
        experimental_features: unsafe { wgpu::ExperimentalFeatures::enabled() },
        ..Default::default()
    }))
    .expect("request GPU device");
    Some((adapter, device, queue))
}

fn upload(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    luma: &[f32],
    w: u32,
    h: u32,
    kind: u32,
) -> (wgpu::Texture, Vec<f32>) {
    let mut expected = Vec::new();
    let (bytes, format, bpp) = match kind {
        0 => (
            bytemuck::cast_slice(luma).to_vec(),
            wgpu::TextureFormat::R32Float,
            4,
        ),
        1 => {
            let mut rgba = Vec::new();
            for &v in luma {
                let rgb = [v, v * 0.7 + 0.1, 1.0 - v * 0.6];
                expected.push(rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722);
                rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 1.0]);
            }
            (
                bytemuck::cast_slice(&rgba).to_vec(),
                wgpu::TextureFormat::Rgba32Float,
                16,
            )
        }
        _ => {
            let mut b = Vec::new();
            for &v in luma {
                let word = ((v * 1023.0).round() as u16) << 6;
                expected.push(word as f32 / 65535.0);
                b.extend_from_slice(&word.to_le_bytes());
            }
            (b, wgpu::TextureFormat::Rg8Unorm, 2)
        }
    };
    if kind == 0 {
        expected.extend_from_slice(luma);
    }
    let t = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("SR-input"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
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
        t.as_image_copy(),
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * bpp),
            rows_per_image: Some(h),
        },
        t.size(),
    );
    (t, expected)
}

fn input(view: &wgpu::TextureView, kind: u32) -> Input<'_> {
    if kind == 1 {
        Input::NonlinearRgb {
            view,
            luma_coefficients: [0.2126, 0.7152, 0.0722],
        }
    } else {
        Input::PlanarLuma {
            view,
            p010_le_bytes: kind == 2,
        }
    }
}
fn submit(
    sr: &mut WgpuSr,
    d: &wgpu::Device,
    q: &wgpu::Queue,
    view: &wgpu::TextureView,
    kind: u32,
    w: u32,
    h: u32,
    target: [u32; 2],
    range: LumaRange,
    token: Option<u64>,
) -> Output {
    let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut e = d.create_command_encoder(&Default::default());
    let out = sr
        .encode(d, q, &mut e, input(view, kind), w, h, target, range, token)
        .unwrap()
        .expect("ready SR output");
    if !out.cache_hit {
        q.submit([e.finish()]);
    }
    assert!(
        pollster::block_on(scope.pop()).is_none(),
        "GPU encode validation"
    );
    sr.commit_encoded_output(&out).unwrap();
    out
}
fn map(d: &wgpu::Device, b: &wgpu::Buffer) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    b.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    let deadline = Instant::now() + std::time::Duration::from_secs(120);
    loop {
        match d.poll(wgpu::PollType::wait_indefinitely()) {
            Ok(_) => break,
            Err(wgpu::PollError::Timeout) if Instant::now() < deadline => continue,
            Err(e) => panic!("GPU readback did not complete: {e}"),
        }
    }
    rx.recv().unwrap().unwrap();
    let bytes = b.slice(..).get_mapped_range().to_vec();
    b.unmap();
    bytes
}
fn read(d: &wgpu::Device, q: &wgpu::Queue, o: &Output) -> Vec<f32> {
    let (w, h) = (o.packed_width, o.packed_height);
    let bpp = if o.texture.format() == wgpu::TextureFormat::R16Float {
        2
    } else {
        8
    };
    let pitch = (w * bpp).div_ceil(256) * 256;
    let b = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("SR-readback"),
        size: u64::from(pitch) * u64::from(h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut e = d.create_command_encoder(&Default::default());
    e.copy_texture_to_buffer(
        o.texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &b,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(pitch),
                rows_per_image: Some(h),
            },
        },
        o.texture.size(),
    );
    q.submit([e.finish()]);
    let bytes = map(d, &b);
    if o.packed {
        erika::renderer::wgpu_artcnn::unpack_packed_d2s_rgba16f(&bytes, w, h, pitch).unwrap()
    } else {
        (0..h as usize)
            .flat_map(|y| {
                let b = &bytes;
                (0..w as usize).map(move |x| {
                    let i = y * pitch as usize + x * bpp as usize;
                    model::f16_to_f32(u16::from_le_bytes([b[i], b[i + 1]]))
                })
            })
            .collect()
    }
}
fn compare(label: &str, a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len());
    let mut sum = 0.0f64;
    let mut max = 0.0f32;
    for (&a, &b) in a.iter().zip(b) {
        assert!(a.is_finite(), "{label}: nonfinite");
        let e = (a - b).abs();
        sum += e as f64;
        max = max.max(e);
    }
    let mae = sum / a.len() as f64;
    eprintln!("ORACLE {label} mae={mae:.8} max={max:.8}");
    if mae >= 2e-3 || max >= 3e-2 {
        for (index, (&actual, &expected)) in a.iter().zip(b).enumerate() {
            if (actual - expected).abs() >= 3e-2 {
                eprintln!("MISMATCH index={index} actual={actual:.8} expected={expected:.8}");
            }
        }
    }
    assert!(mae < 2e-3 && max < 3e-2, "{label}: MAE {mae}, max {max}");
}
fn selected(mode: Mode) -> bool {
    std::env::var("ERIKA_SR_TEST_MODE").map_or(true, |v| {
        v.to_lowercase()
            .split(',')
            .any(|p| mode.label().contains(p))
    })
}

struct BenchmarkLock(std::path::PathBuf);
impl BenchmarkLock {
    fn acquire() -> Self {
        let path = std::path::PathBuf::from(if cfg!(target_os = "windows") {
            r"C:\Users\rgliang\erika-sr\_gpu.lock"
        } else if std::path::Path::new("/dev/dxg").exists() {
            "/mnt/c/Users/rgliang/erika-sr/_gpu.lock"
        } else {
            "/tmp/sr/gpu.lock"
        });
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_secs(5))
                }
                Err(e) => panic!("GPU lock {path:?}: {e}"),
            }
        }
        eprintln!("GPU_LOCK acquired {:?} pid={}", path, std::process::id());
        Self(path)
    }
}
impl Drop for BenchmarkLock {
    fn drop(&mut self) {
        std::fs::remove_dir(&self.0).expect("release GPU benchmark lock");
    }
}

#[test]
fn every_network_matches_oracle() {
    let Some((a, d, q)) = gpu() else {
        return;
    };
    for mode in Mode::ALL {
        if mode.family() != UpscalerFamily::Network || !selected(mode) {
            continue;
        }
        let model = if mode == Mode::Custom {
            custom()
        } else {
            catalog::execution_model(mode).unwrap()
        };
        let (w, h) = (67, 45);
        let pattern = reference::test_pattern(w as usize, h as usize, 19);
        // The CPU oracle is independent of GPU precision. Avoid re-running
        // the large residual network for every precision/range combination.
        let fixtures: Vec<_> = (0..3)
            .map(|kind| {
                let (texture, luma) = upload(&d, &q, &pattern, w, h, kind);
                let full = reference::run(&model, &luma, w as usize, h as usize, LumaRange::Full);
                let limited = if reference::input_normalization(&model, LumaRange::Full)
                    == reference::input_normalization(&model, LumaRange::Limited)
                {
                    full.clone()
                } else {
                    reference::run(&model, &luma, w as usize, h as usize, LumaRange::Limited)
                };
                (texture, [full, limited])
            })
            .collect();
        for half in [false, true] {
            if half && !d.features().contains(wgpu::Features::SHADER_F16) {
                continue;
            }
            let config = Config {
                tile: [31, 23],
                kernel: Options {
                    half,
                    ..Config::for_adapter(&a).kernel
                },
                ..test_adapter_config(&a)
            };
            let mut sr = WgpuSr::new_with_config(&a, &d, config);
            if mode == Mode::Custom {
                sr.set_custom_model(&d, Some(model.clone()));
            }
            sr.set_mode(&d, mode).unwrap();
            assert_eq!(sr.status(), Status::Building);
            sr.wait_ready().unwrap();
            for kind in 0..3 {
                let (t, expected) = &fixtures[kind as usize];
                let view = t.create_view(&Default::default());
                for (ri, range) in [LumaRange::Full, LumaRange::Limited]
                    .into_iter()
                    .enumerate()
                {
                    let out = submit(
                        &mut sr,
                        &d,
                        &q,
                        &view,
                        kind,
                        w,
                        h,
                        [w * 2, h * 2],
                        range,
                        None,
                    );
                    let actual = read(&d, &q, &out);
                    compare(
                        &format!("{} half={half} kind={kind} {range:?}", mode.label()),
                        &actual,
                        &expected[ri],
                    );
                }
            }
        }
    }
}

fn custom() -> Arc<SrModel> {
    Arc::new(SrModel {
        name: "custom-test".into(),
        padding: Padding::Zero,
        input_range: InputRange::Full,
        tensors: vec![1, 3, 3, 4],
        layers: vec![
            ConvLayer {
                output: 1,
                out_channels: 3,
                inputs: vec![ConvInput {
                    tensor: 0,
                    transform: InputTransform::NegRelu,
                    kernel: 5,
                    weights: (0..75).map(|i| ((i as f32) * 0.7).sin() * 0.04).collect(),
                }],
                bias: vec![0.1, -0.2, 0.03],
                residual: None,
                activation: Activation::Prelu(vec![0.1, 0.2, 0.3]),
                border: None,
            },
            ConvLayer {
                output: 2,
                out_channels: 3,
                inputs: vec![ConvInput {
                    tensor: 1,
                    transform: InputTransform::Relu,
                    kernel: 1,
                    weights: vec![0.1; 9],
                }],
                bias: vec![0.02; 3],
                residual: Some(1),
                activation: Activation::Silu,
                border: None,
            },
            ConvLayer {
                output: 3,
                out_channels: 4,
                inputs: vec![
                    ConvInput {
                        tensor: 1,
                        transform: InputTransform::Identity,
                        kernel: 3,
                        weights: vec![0.02; 108],
                    },
                    ConvInput {
                        tensor: 2,
                        transform: InputTransform::NegRelu,
                        kernel: 1,
                        weights: vec![-0.1; 12],
                    },
                ],
                bias: vec![0.03; 4],
                residual: None,
                activation: Activation::None,
                border: None,
            },
        ],
        head: Head {
            tensor: 3,
            order: SubpixelOrder::ColumnMajor,
            base: HeadBase::Bilinear,
            clamp: true,
        },
    })
}

#[test]
fn sparse_fused_custom_head_keeps_its_accumulator() {
    let Some((a, d, q)) = gpu() else {
        return;
    };
    let mut m = SrModel {
        name: "sparse fused head".into(),
        padding: Padding::Clamp,
        input_range: InputRange::Full,
        tensors: vec![1],
        layers: Vec::new(),
        head: Head {
            tensor: 8,
            order: SubpixelOrder::RowMajor,
            base: HeadBase::None,
            clamp: true,
        },
    };
    for t in 1..=7 {
        let cin = if t == 1 { 1 } else { 4 };
        m.tensors.push(4);
        m.layers.push(ConvLayer {
            output: t,
            out_channels: 4,
            inputs: vec![ConvInput {
                tensor: t - 1,
                transform: InputTransform::Identity,
                kernel: 3,
                weights: vec![0.01; 4 * cin * 9],
            }],
            bias: vec![
                if t == 6 {
                    0.5
                } else if t == 4 {
                    0.1
                } else {
                    0.04
                };
                4
            ],
            residual: None,
            activation: Activation::None,
            border: None,
        });
    }
    m.tensors.push(4);
    m.layers.push(ConvLayer {
        output: 8,
        out_channels: 4,
        inputs: [2, 7]
            .into_iter()
            .map(|t| ConvInput {
                tensor: t,
                transform: InputTransform::Identity,
                kernel: 1,
                weights: vec![0.5; 16],
            })
            .collect(),
        bias: vec![0.02; 4],
        residual: None,
        activation: Activation::None,
        border: None,
    });
    m.validate().unwrap();
    let m = Arc::new(m);
    let (w, h) = (267, 195);
    let p = reference::test_pattern(w as usize, h as usize, 11);
    let (t, luma) = upload(&d, &q, &p, w, h, 0);
    let view = t.create_view(&Default::default());
    for half in [false, true] {
        let mut sr = WgpuSr::new_with_config(
            &a,
            &d,
            Config {
                tile: [251, 181],
                kernel: Options {
                    half,
                    fusion: true,
                    fusion_depth: 2,
                    fusion_max_channels: 4,
                    ..Options::default()
                },
                ..test_adapter_config(&a)
            },
        );
        sr.set_custom_model(&d, Some(m.clone()));
        sr.set_mode(&d, Mode::Custom).unwrap();
        sr.wait_ready().unwrap();
        for range in [LumaRange::Full, LumaRange::Limited] {
            let out = submit(&mut sr, &d, &q, &view, 0, w, h, [w * 2, h * 2], range, None);
            compare(
                "sparse fused Custom head",
                &read(&d, &q, &out),
                &reference::run(&m, &luma, w as usize, h as usize, range),
            );
        }
    }
}

#[test]
fn padded_input_and_mip_views_preserve_logical_extent() {
    let Some((a, d, q)) = gpu() else {
        return;
    };
    for mip in [false, true] {
        let (w, h, tw, th) = if mip {
            (9, 6, 18, 12)
        } else {
            (15, 11, 17, 13)
        };
        let p = reference::test_pattern(tw as usize, th as usize, 17);
        let (t, luma) = if mip {
            let t = d.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: tw,
                    height: th,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 2,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Float,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let luma = reference::test_pattern(w as usize, h as usize, 21);
            q.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &t,
                    mip_level: 1,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&luma),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            (t, luma)
        } else {
            let (t, _) = upload(&d, &q, &p, tw, th, 0);
            let luma = (0..h as usize)
                .flat_map(|y| {
                    p[y * tw as usize..y * tw as usize + w as usize]
                        .iter()
                        .copied()
                })
                .collect();
            (t, luma)
        };
        let view = t.create_view(&wgpu::TextureViewDescriptor {
            base_mip_level: u32::from(mip),
            mip_level_count: Some(1),
            ..Default::default()
        });
        for mode in [Mode::Custom, Mode::Fsr1] {
            let mut sr = WgpuSr::new_with_config(&a, &d, test_adapter_config(&a));
            let model = custom();
            sr.set_custom_model(&d, Some(model.clone()));
            sr.set_mode(&d, mode).unwrap();
            sr.wait_ready().unwrap();
            let target = [w * 2 + 1, h * 2 + 1];
            let out = submit(
                &mut sr,
                &d,
                &q,
                &view,
                0,
                w,
                h,
                target,
                LumaRange::Limited,
                None,
            );
            let expected = if mode == Mode::Custom {
                reference::run(&model, &luma, w as usize, h as usize, LumaRange::Limited)
            } else {
                fsr1::rcas(
                    &fsr1::easu(
                        &luma,
                        w as usize,
                        h as usize,
                        target[0] as usize,
                        target[1] as usize,
                    ),
                    target[0] as usize,
                    target[1] as usize,
                    fsr1::rcas_strength(fsr1::DEFAULT_SHARPNESS_STOPS),
                )
            };
            compare(
                &format!("padded/mip input mip={mip} mode={mode:?}"),
                &read(&d, &q, &out),
                &expected,
            );
        }
    }
}

#[test]
fn folded_border_classes_and_unfused_head() {
    let Some((a, d, q)) = gpu() else {
        return;
    };
    let mut m = (*custom()).clone();
    let first = &mut m.layers[0];
    first.border = Some(
        (0..9)
            .map(|v| model::LayerWeights {
                inputs: first
                    .inputs
                    .iter()
                    .map(|i| {
                        i.weights
                            .iter()
                            .map(|&w| w * if v == 4 { 1.0 } else { 0.5 + v as f32 * 0.1 })
                            .collect()
                    })
                    .collect(),
                bias: first
                    .bias
                    .iter()
                    .map(|&b| b + if v == 4 { 0.0 } else { v as f32 * 0.01 })
                    .collect(),
            })
            .collect(),
    );
    m.tensors.push(4);
    m.layers.push(ConvLayer {
        output: 4,
        out_channels: 4,
        inputs: vec![ConvInput {
            tensor: 3,
            transform: InputTransform::Identity,
            kernel: 1,
            weights: vec![0.01; 16],
        }],
        bias: vec![0.0; 4],
        residual: None,
        activation: Activation::None,
        border: None,
    });
    m.validate().unwrap();
    for half in [false, true] {
        let mut sr = WgpuSr::new_with_config(
            &a,
            &d,
            Config {
                tile: [3, 2],
                kernel: Options {
                    half,
                    ..Config::for_adapter(&a).kernel
                },
                ..test_adapter_config(&a)
            },
        );
        sr.set_custom_model(&d, Some(Arc::new(m.clone())));
        sr.set_mode(&d, Mode::Custom).unwrap();
        sr.wait_ready().unwrap();
        for (w, h) in [(1, 1), (1, 7), (9, 1), (7, 5)] {
            let p = reference::test_pattern(w as usize, h as usize, 31);
            let (t, luma) = upload(&d, &q, &p, w, h, 0);
            let view = t.create_view(&Default::default());
            let out = submit(
                &mut sr,
                &d,
                &q,
                &view,
                0,
                w,
                h,
                [w * 2, h * 2],
                LumaRange::Limited,
                None,
            );
            compare(
                &format!("border/unfused {w}x{h} half={half}"),
                &read(&d, &q, &out),
                &reference::run(&m, &luma, w as usize, h as usize, LumaRange::Limited),
            );
        }
    }
}

#[test]
fn custom_degenerate_borders_and_cache() {
    let Some((a, d, q)) = gpu() else {
        return;
    };
    let m = custom();
    for half in [false, true] {
        let mut sr = WgpuSr::new_with_config(
            &a,
            &d,
            Config {
                tile: [3, 2],
                kernel: Options {
                    half,
                    ..Config::for_adapter(&a).kernel
                },
                ..test_adapter_config(&a)
            },
        );
        sr.set_custom_model(&d, Some(m.clone()));
        sr.set_mode(&d, Mode::Custom).unwrap();
        sr.wait_ready().unwrap();
        for (w, h) in [(1, 1), (1, 7), (9, 1), (7, 5)] {
            let p = reference::test_pattern(w as usize, h as usize, 3);
            let (t, luma) = upload(&d, &q, &p, w, h, 0);
            let view = t.create_view(&Default::default());
            let out = submit(
                &mut sr,
                &d,
                &q,
                &view,
                0,
                w,
                h,
                [w * 2, h * 2],
                LumaRange::Limited,
                Some(1),
            );
            compare(
                &format!("custom {w}x{h} {half}"),
                &read(&d, &q, &out),
                &reference::run(&m, &luma, w as usize, h as usize, LumaRange::Limited),
            );
            let hit = submit(
                &mut sr,
                &d,
                &q,
                &view,
                0,
                w,
                h,
                [w * 2, h * 2],
                LumaRange::Limited,
                Some(1),
            );
            assert!(hit.cache_hit);
            let miss = submit(
                &mut sr,
                &d,
                &q,
                &view,
                0,
                w,
                h,
                [w * 2, h * 2],
                LumaRange::Full,
                Some(1),
            );
            assert!(!miss.cache_hit);
        }
        let view = d
            .create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        let mut encoder = d.create_command_encoder(&Default::default());
        let out = sr
            .encode(
                &d,
                &q,
                &mut encoder,
                input(&view, 0),
                1,
                1,
                [2, 2],
                LumaRange::Full,
                Some(5),
            )
            .unwrap()
            .unwrap();
        assert!(!out.cache_hit);
        sr.handle_deferred_encode_failure(out.input_kind, "test discarded submission");
        assert_eq!(sr.stats().fallback_count, 1);
        sr.set_mode(&d, Mode::Custom).unwrap();
        sr.wait_ready().unwrap();
        assert_eq!(sr.status(), Status::Scalar);
    }
}

#[test]
fn fsr_matches_cpu_at_presented_size() {
    let Some((a, d, q)) = gpu() else {
        return;
    };
    for half in [false, true] {
        let mut sr = WgpuSr::new_with_config(
            &a,
            &d,
            Config {
                kernel: Options {
                    half,
                    ..Config::for_adapter(&a).kernel
                },
                ..test_adapter_config(&a)
            },
        );
        sr.set_mode(&d, Mode::Fsr1).unwrap();
        sr.wait_ready().unwrap();
        for kind in 0..3 {
            for (w, h, ow, oh) in [
                (67, 45, 153, 101),
                (1, 5, 3, 9),
                (7, 1, 11, 1),
                (3, 3, 2, 2),
            ] {
                let p = reference::test_pattern(w as usize, h as usize, 29);
                let (t, luma) = upload(&d, &q, &p, w, h, kind);
                let view = t.create_view(&Default::default());
                for range in [LumaRange::Full, LumaRange::Limited] {
                    let out = submit(&mut sr, &d, &q, &view, kind, w, h, [ow, oh], range, None);
                    let expected = fsr1::rcas(
                        &fsr1::easu(&luma, w as usize, h as usize, ow as usize, oh as usize),
                        ow as usize,
                        oh as usize,
                        fsr1::rcas_strength(fsr1::DEFAULT_SHARPNESS_STOPS),
                    );
                    compare(
                        &format!("FSR {w}x{h}->{ow}x{oh} half={half} kind={kind} {range:?}"),
                        &read(&d, &q, &out),
                        &expected,
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "GPU tuner timing; run alone with --release --ignored --test-threads=1"]
fn autotuner_installs_between_submissions_and_reuses_cache() {
    let Some((a, d, q)) = gpu() else {
        return;
    };
    let path = if cfg!(target_os = "windows") {
        r"C:\Users\rgliang\erika-sr\_gpu.lock"
    } else if std::path::Path::new("/dev/dxg").exists() {
        "/mnt/c/Users/rgliang/erika-sr/_gpu.lock"
    } else {
        "/tmp/sr/gpu.lock"
    };
    unsafe {
        std::env::set_var("ERIKA_WGPU_SR_TUNE_LOCK", path);
    }
    let m = custom();
    let p = reference::test_pattern(17, 11, 31);
    let (t, luma) = upload(&d, &q, &p, 17, 11, 0);
    let view = t.create_view(&Default::default());
    let expected = reference::run(&m, &luma, 17, 11, LumaRange::Limited);
    for reuse in [false, true] {
        let mut sr = WgpuSr::new_with_config(
            &a,
            &d,
            Config {
                autotune: true,
                ..test_adapter_config(&a)
            },
        );
        sr.set_custom_model(&d, Some(m.clone()));
        sr.set_mode(&d, Mode::Custom).unwrap();
        sr.wait_ready().unwrap();
        let out = submit(
            &mut sr,
            &d,
            &q,
            &view,
            0,
            17,
            11,
            [34, 22],
            LumaRange::Limited,
            None,
        );
        assert!(sr.is_tuning());
        compare("tuner baseline", &read(&d, &q, &out), &expected);
        let deadline = Instant::now() + std::time::Duration::from_secs(120);
        while sr.is_tuning() {
            sr.poll_build();
            assert!(Instant::now() < deadline, "tuning deadline");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(sr.status(), Status::Scalar);
        let out = submit(
            &mut sr,
            &d,
            &q,
            &view,
            0,
            17,
            11,
            [34, 22],
            LumaRange::Limited,
            None,
        );
        compare(
            &format!("tuner result reuse={reuse}"),
            &read(&d, &q, &out),
            &expected,
        );
        assert!(!sr.is_tuning());
    }
    unsafe {
        std::env::remove_var("ERIKA_WGPU_SR_TUNE_LOCK");
    }
}

#[test]
#[ignore = "GPU performance sweep; run with --release --ignored --nocapture"]
fn benchmark() {
    let Some((a, d, q)) = gpu() else {
        panic!("benchmark requires an adapter");
    };
    let iterations = std::env::var("ERIKA_SR_BENCH_ITERATIONS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(5);
    let sizes = if std::env::var("ERIKA_SR_BENCH_RES").as_deref() == Ok("1080") {
        vec![(1920, 1080)]
    } else if std::env::var("ERIKA_SR_BENCH_RES").as_deref() == Ok("720") {
        vec![(1280, 720)]
    } else if std::env::var("ERIKA_SR_BENCH_SMALL").is_ok() {
        vec![(320, 180)]
    } else {
        vec![(1280, 720), (1920, 1080)]
    };
    let timestamp = d.features().contains(wgpu::Features::TIMESTAMP_QUERY);
    let mut modes = Mode::ALL.to_vec();
    // Avoid preceding every small-model measurement with R8F64's long load.
    modes.sort_by_key(|m| m.label() == "artcnn_r8f64");
    for mode in modes {
        if !selected(mode)
            || (mode.family() != UpscalerFamily::Network && mode != Mode::Fsr1)
            || mode == Mode::Custom
        {
            continue;
        }
        let cfg = Config {
            autotune: false,
            ..Config::for_adapter(&a)
        };
        let mut sr = WgpuSr::new_with_config(&a, &d, cfg);
        let start = Instant::now();
        sr.set_mode(&d, mode).unwrap();
        sr.wait_ready().unwrap();
        eprintln!(
            "BUILD {} {:.3}s config={cfg:?}",
            mode.label(),
            start.elapsed().as_secs_f64()
        );
        eprintln!("ACTIVE {} {:?}", mode.label(), sr.active_options());
        let mut old = if mode.is_legacy_artcnn() && std::env::var("ERIKA_SR_BENCH_OLD").is_ok() {
            let mut old = erika::renderer::wgpu_artcnn::WgpuArtCnn::new(&a, &d);
            old.set_mode(&d, mode).unwrap();
            Some(old)
        } else {
            None
        };
        for &(w, h) in &sizes {
            let p = reference::test_pattern(w as usize, h as usize, 3);
            let (t, _) = upload(&d, &q, &p, w, h, 0);
            let view = t.create_view(&Default::default());
            let _gpu_lock = BenchmarkLock::acquire();
            let out = submit(
                &mut sr,
                &d,
                &q,
                &view,
                0,
                w,
                h,
                [w * 2, h * 2],
                LumaRange::Full,
                None,
            );
            let _ = read(&d, &q, &out);
            let warm_ms = std::env::var("ERIKA_SR_BENCH_WARMUP_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(100);
            let warm_start = Instant::now();
            while warm_start.elapsed().as_millis() < u128::from(warm_ms) {
                let _ = submit(
                    &mut sr,
                    &d,
                    &q,
                    &view,
                    0,
                    w,
                    h,
                    [w * 2, h * 2],
                    LumaRange::Full,
                    None,
                );
                d.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            }
            let queries = timestamp.then(|| {
                d.create_query_set(&wgpu::QuerySetDescriptor {
                    label: None,
                    ty: wgpu::QueryType::Timestamp,
                    count: 2,
                })
            });
            let resolve = d.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: 256,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let staging = d.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: 16,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            sr.set_timestamp_queries(queries.clone());
            let mut times = Vec::new();
            for _ in 0..iterations {
                let mut e = d.create_command_encoder(&Default::default());
                let start = Instant::now();
                let out = sr
                    .encode(
                        &d,
                        &q,
                        &mut e,
                        input(&view, 0),
                        w,
                        h,
                        [w * 2, h * 2],
                        LumaRange::Full,
                        None,
                    )
                    .unwrap()
                    .unwrap();
                if let Some(qs) = &queries {
                    e.resolve_query_set(qs, 0..2, &resolve, 0);
                    e.copy_buffer_to_buffer(&resolve, 0, &staging, 0, 16);
                }
                q.submit([e.finish()]);
                sr.commit_encoded_output(&out).unwrap();
                let ms = if timestamp {
                    let b = map(&d, &staging);
                    let values: Vec<_> = b
                        .chunks_exact(8)
                        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                        .collect();
                    (values[1] - values[0]) as f64 * q.get_timestamp_period() as f64 / 1e6
                } else {
                    d.poll(wgpu::PollType::wait_indefinitely()).unwrap();
                    start.elapsed().as_secs_f64() * 1000.0
                };
                times.push(ms);
            }
            times.sort_by(f64::total_cmp);
            let ms = times[times.len() / 2];
            let flops = if mode == Mode::Fsr1 {
                0.0
            } else {
                catalog::execution_model(mode).unwrap().macs_per_pixel() as f64
                    * 2.0
                    * w as f64
                    * h as f64
            };
            eprintln!(
                "BENCH,{},{w},{h},{ms:.5},{:.5},half={},scratch={},timestamp={timestamp}",
                mode.label(),
                flops / ms / 1e9,
                sr.half(),
                sr.scratch_bytes()
            );
            if let Some(old) = old.as_mut() {
                old.set_timestamp_queries(queries.clone());
                let mut times = Vec::new();
                for i in 0..iterations + 1 {
                    let mut e = d.create_command_encoder(&Default::default());
                    let start = Instant::now();
                    let out = old
                        .encode(&d, &q, &mut e, input(&view, 0), w, h, None)
                        .unwrap()
                        .unwrap();
                    if let Some(qs) = &queries {
                        e.resolve_query_set(qs, 0..2, &resolve, 0);
                        e.copy_buffer_to_buffer(&resolve, 0, &staging, 0, 16);
                    }
                    q.submit([e.finish()]);
                    old.commit_encoded_output(&out).unwrap();
                    let ms = if timestamp {
                        let b = map(&d, &staging);
                        let v: Vec<_> = b
                            .chunks_exact(8)
                            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                            .collect();
                        (v[1] - v[0]) as f64 * q.get_timestamp_period() as f64 / 1e6
                    } else {
                        d.poll(wgpu::PollType::wait_indefinitely()).unwrap();
                        start.elapsed().as_secs_f64() * 1000.0
                    };
                    if i > 0 {
                        times.push(ms);
                    }
                }
                times.sort_by(f64::total_cmp);
                let ms = times[times.len() / 2];
                eprintln!(
                    "OLD,{},{w},{h},{ms:.5},{:.5}",
                    mode.label(),
                    flops / ms / 1e9
                );
            }
        }
    }
}
