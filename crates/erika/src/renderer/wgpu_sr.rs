//! Portable network and FSR1 executor. Compilation happens on a worker while
//! presentation continues at native resolution. Network scratch is tiled and
//! allocated by tensor lifetime; output retains the packed TL/TR/BL/BR ABI.

pub use super::wgpu_artcnn::{
    WgpuArtCnnCapability as Capability, WgpuArtCnnFailure as Failure,
    WgpuArtCnnFailureStage as FailureStage, WgpuArtCnnInput as Input,
    WgpuArtCnnInputKind as InputKind, WgpuArtCnnStats as Stats, WgpuArtCnnStatus as Status,
};
use super::{
    pipeline::{LumaUpscalerMode, UpscalerFamily},
    sr::{
        catalog,
        model::SrModel,
        reference::{LumaRange, input_normalization},
        wgsl::{self, Options},
    },
};
use std::{
    borrow::Cow,
    collections::HashMap,
    mem,
    num::NonZeroU64,
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};
use wgpu::util::DeviceExt;

#[derive(Debug, Clone, Copy)]
pub enum DeviceProfile {
    Portable,
    Apple,
    IntelVulkan,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub device_profile: DeviceProfile,
    pub tile: [u32; 2],
    pub budget: u64,
    pub kernel: Options,
    pub bounds_checks: bool,
    pub loop_bounds: bool,
    pub zero_workgroup: bool,
    /// Compile and time candidates on a worker after the first input arrives.
    pub autotune: bool,
}

impl Default for Config {
    fn default() -> Self {
        fn env(name: &str, default: u32) -> u32 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }
        Self {
            device_profile: DeviceProfile::Portable,
            autotune: env("ERIKA_WGPU_SR_AUTOTUNE", 1) != 0
                && !std::env::vars().any(|(k, _)| {
                    k.starts_with("ERIKA_WGPU_SR_")
                        && k != "ERIKA_WGPU_SR_AUTOTUNE"
                        && k != "ERIKA_WGPU_SR_DUMP_DIR"
                }),
            bounds_checks: env("ERIKA_WGPU_SR_BOUNDS_CHECKS", 0) != 0,
            loop_bounds: env("ERIKA_WGPU_SR_LOOP_BOUNDING", 0) != 0,
            zero_workgroup: env("ERIKA_WGPU_SR_ZERO_INIT", 0) != 0,
            tile: [
                env("ERIKA_WGPU_SR_TILE_X", 256).clamp(1, 2048),
                env("ERIKA_WGPU_SR_TILE_Y", 128).clamp(1, 2048),
            ],
            budget: u64::from(env("ERIKA_WGPU_SR_BUDGET_MB", 64).clamp(1, 512)) << 20,
            kernel: Options {
                group: [
                    env("ERIKA_WGPU_SR_GROUP_X", 8).clamp(1, 16),
                    env("ERIKA_WGPU_SR_GROUP_Y", 8).clamp(1, 16),
                ],
                fsr_group: [
                    env("ERIKA_WGPU_SR_GROUP_X", 16).clamp(1, 16),
                    env("ERIKA_WGPU_SR_GROUP_Y", 16).clamp(1, 16),
                ],
                pixels: env("ERIKA_WGPU_SR_PIXELS", 1).clamp(1, 4),
                register_blocking: env("ERIKA_WGPU_SR_REGISTER_BLOCK", 0) != 0,
                output_block: env("ERIKA_WGPU_SR_OUTPUT_BLOCK", 8).clamp(4, 16) as usize,
                half: env("ERIKA_WGPU_SR_F16", 1) != 0,
                weights_half: env("ERIKA_WGPU_SR_WEIGHTS_F16", 0) != 0,
                half_products: env("ERIKA_WGPU_SR_HALF_PRODUCTS", 0) != 0,
                narrow_half_products: env("ERIKA_WGPU_SR_NARROW_HALF", 0) != 0,
                half_accum: env("ERIKA_WGPU_SR_HALF_ACCUM", 0) != 0,
                mad: env("ERIKA_WGPU_SR_MAD", 0) != 0,
                compact_taps: env("ERIKA_WGPU_SR_COMPACT_TAPS", 0) != 0,
                subgroup: env("ERIKA_WGPU_SR_SUBGROUP", 0) != 0,
                cooperative: env("ERIKA_WGPU_SR_COOPERATIVE", 0) != 0,
                matrix_rows: env("ERIKA_WGPU_SR_MATRIX_ROWS", 4).clamp(1, 4) as usize,
                r16: env("ERIKA_WGPU_SR_R16", 1) != 0,
                gather: env("ERIKA_WGPU_SR_GATHER", 0) != 0,
                fsr_unroll: env("ERIKA_WGPU_SR_FSR_UNROLL", 1) != 0,
                fsr_half_math: env("ERIKA_WGPU_SR_FSR_HALF_MATH", 0) != 0,
                fusion: env("ERIKA_WGPU_SR_FUSION", 1) != 0,
                staging: env("ERIKA_WGPU_SR_STAGING", 1) != 0,
                unroll: env("ERIKA_WGPU_SR_UNROLL", 0) != 0,
                fusion_depth: env("ERIKA_WGPU_SR_FUSION_DEPTH", 2).clamp(1, 8) as usize,
                fusion_max_channels: env("ERIKA_WGPU_SR_FUSION_CHANNELS", 4).clamp(4, 12) as usize,
                shared_bytes: env("ERIKA_WGPU_SR_SHARED_KB", 28).clamp(1, 32) as usize * 1024,
                ..Options::default()
            },
        }
    }
}

impl Config {
    pub fn vulkan_matrix_supported(adapter: &wgpu::Adapter) -> bool {
        let info = adapter.get_info();
        info.backend == wgpu::Backend::Vulkan
            && info.vendor == 0x10de
            && info.subgroup_min_size == 32
            && info.subgroup_max_size == 32
            && adapter.features().contains(
                wgpu::Features::EXPERIMENTAL_COOPERATIVE_MATRIX
                    | wgpu::Features::SHADER_F16
                    | wgpu::Features::SUBGROUP,
            )
            && adapter.cooperative_matrix_properties().iter().any(|p| {
                p.m_size == 16
                    && p.n_size == 16
                    && p.k_size == 16
                    && p.ab_type == wgpu::CooperativeScalarType::F16
                    && p.cr_type == wgpu::CooperativeScalarType::F32
                    && !p.saturating_accumulation
            })
    }
    /// Measured starting points; each explicitly set environment knob wins.
    pub fn for_adapter(adapter: &wgpu::Adapter) -> Self {
        let mut c = Self::default();
        let info = adapter.get_info();
        c.device_profile = if info.backend == wgpu::Backend::Metal && info.name.starts_with("Apple")
        {
            DeviceProfile::Apple
        } else if info.backend == wgpu::Backend::Vulkan && info.vendor == 0x8086 {
            DeviceProfile::IntelVulkan
        } else {
            DeviceProfile::Portable
        };
        let pin = |name: &str| std::env::var_os(name).is_some();
        if !pin("ERIKA_WGPU_SR_COOPERATIVE") && Self::vulkan_matrix_supported(adapter) {
            c.kernel.cooperative = true;
            c.kernel.matrix_min_channels = 16;
        }
        if !pin("ERIKA_WGPU_SR_TILE_X") {
            c.tile[0] = if info.vendor == 0x10de { 2048 } else { 1024 };
        }
        if !pin("ERIKA_WGPU_SR_TILE_Y") {
            c.tile[1] = if info.vendor == 0x10de { 1024 } else { 512 };
        }
        if !pin("ERIKA_WGPU_SR_OUTPUT_BLOCK") {
            c.kernel.output_block = 16;
        }
        if info.vendor == 0x10de {
            if !pin("ERIKA_WGPU_SR_BUDGET_MB") {
                c.budget = 128 << 20;
            }
            if !pin("ERIKA_WGPU_SR_UNROLL") {
                c.kernel.unroll = true;
            }
            if !pin("ERIKA_WGPU_SR_GROUP_X") {
                c.kernel.group[0] = 16;
            }
        } else if info.backend == wgpu::Backend::Metal {
            if !pin("ERIKA_WGPU_SR_FSR_HALF_MATH") {
                c.kernel.fsr_half_math = true;
            }
            if !pin("ERIKA_WGPU_SR_HALF_PRODUCTS") && !pin("ERIKA_WGPU_SR_NARROW_HALF") {
                c.kernel.narrow_half_products = true;
            }
            if !pin("ERIKA_WGPU_SR_FUSION") {
                c.kernel.fusion = false;
            }
        } else {
            c.kernel.output_block = if pin("ERIKA_WGPU_SR_OUTPUT_BLOCK") {
                c.kernel.output_block
            } else {
                8
            };
            if !pin("ERIKA_WGPU_SR_TILE_X") {
                c.tile[0] = 512;
            }
            if !pin("ERIKA_WGPU_SR_TILE_Y") {
                c.tile[1] = 256;
            }
        }
        if !pin("ERIKA_WGPU_SR_GATHER") {
            // RGB gather failed on Mesa D3D12 GL and Intel DX12. Keep it
            // on the validated Metal path; scalar fetches also won on NVIDIA.
            c.kernel.gather = info.backend == wgpu::Backend::Metal;
        }
        c
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    size: [u32; 4],
    tile: [i32; 4],
    rect: [i32; 4],
    range: [f32; 4],
    luma: [f32; 4],
    offsets: [[u32; 4]; 64],
}

struct Compiled {
    spec: wgsl::Kernel,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    weights: wgpu::Buffer,
}
struct Network {
    fingerprint: u64,
    model: Option<Arc<SrModel>>,
    kernels: Vec<Compiled>,
    halos: Vec<u32>,
    max_halo: u32,
    slots: Vec<usize>,
    slot_slices: Vec<u32>,
    allocated: Vec<bool>,
    options: Options,
    sampler: wgpu::Sampler,
}

fn entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    }
}
fn buffer(uniform: bool, dynamic: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: if uniform {
            wgpu::BufferBindingType::Uniform
        } else {
            wgpu::BufferBindingType::Storage { read_only: true }
        },
        has_dynamic_offset: dynamic,
        min_binding_size: None,
    }
}

impl Network {
    fn build(
        device: &wgpu::Device,
        model: Option<Arc<SrModel>>,
        config: Config,
    ) -> Result<Self, String> {
        let mut options = config.kernel;
        if let Some(model) = &model {
            let width = model.tensors.iter().copied().max().unwrap_or(0);
            match config.device_profile {
                DeviceProfile::Apple => {
                    if width <= 4
                        && model.layers.len() >= 6
                        && std::env::var_os("ERIKA_WGPU_SR_PIXELS").is_none()
                        && std::env::var_os("ERIKA_WGPU_SR_REGISTER_BLOCK").is_none()
                    {
                        options.pixels = 2;
                        options.register_blocking = true;
                    }
                    if std::env::var_os("ERIKA_WGPU_SR_WEIGHTS_F16").is_none() {
                        options.weights_half = width >= 8;
                    }
                    if std::env::var_os("ERIKA_WGPU_SR_HALF_ACCUM").is_none() {
                        options.half_accum = width == 12;
                    }
                }
                DeviceProfile::IntelVulkan
                    if width >= 32 && std::env::var_os("ERIKA_WGPU_SR_COMPACT_TAPS").is_none() =>
                {
                    options.compact_taps = true
                }
                _ => (),
            }
        }
        let limits = device.limits();
        options.shared_bytes = options
            .shared_bytes
            .min(limits.max_compute_workgroup_storage_size as usize);
        options.uniform_bytes = limits.max_uniform_buffer_binding_size as usize;
        options.half &= device.features().contains(wgpu::Features::SHADER_F16);
        options.weights_half &= options.half;
        options.matrix_size = if options.matrix_size == 16 { 16 } else { 8 };
        if options.matrix_size == 16 {
            options.cooperative &= options.half;
        }
        options.subgroup &=
            device.features().contains(wgpu::Features::SUBGROUP) && options.pixels == 1;
        options.cooperative &= device
            .features()
            .contains(wgpu::Features::EXPERIMENTAL_COOPERATIVE_MATRIX);
        options.r16 &= device
            .features()
            .contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES);
        if model.is_none() {
            options.group = options.fsr_group;
        }
        options.pixels = options.pixels.clamp(1, 4);
        options.group[0] = options.group[0]
            .max(1)
            .min(limits.max_compute_workgroup_size_x);
        options.group[1] = options.group[1]
            .max(1)
            .min(limits.max_compute_workgroup_size_y);
        while options.group[0] * options.group[1] > limits.max_compute_invocations_per_workgroup {
            options.group[1] = (options.group[1] / 2).max(1);
        }
        let (specs, halos, max_halo, slots, slot_slices, allocated) = if let Some(model) = &model {
            model.validate().map_err(|e| e.to_string())?;
            if model.tensors.len() > 256 {
                return Err("wgpu SR supports at most 256 tensors".into());
            }
            let specs = wgsl::network(model, options);
            let halos: Vec<u32> = model.tensor_halos().into_iter().map(|h| h as u32).collect();
            let fused = specs.len() == 1 && specs[0].fused;
            let allocated: Vec<bool> = (0..model.tensors.len())
                .map(|t| specs.iter().any(|k| k.writes.contains(&t)))
                .collect();
            let max_halo = if fused {
                0
            } else {
                halos.iter().copied().max().unwrap_or(0)
            };
            let mut last = vec![0; model.tensors.len()];
            for (i, l) in model.layers.iter().enumerate() {
                for input in &l.inputs {
                    last[input.tensor] = i;
                }
                if let Some(t) = l.residual {
                    last[t] = i;
                }
            }
            last[model.head.tensor] = model.layers.len();
            if specs.iter().all(|k| k.fused) {
                // A source must stay live until its entire fused dispatch has
                // completed. IR-layer lifetimes allow a later layer in that
                // same dispatch to alias it across workgroups. Dense-head
                // operands have already been absorbed into the accumulator.
                last.fill(0);
                for pair in specs.windows(2) {
                    last[pair[0].writes[0]] = pair[1].layer;
                }
                last[model.head.tensor] = model.layers.len();
            }
            let mut slots = vec![0; model.tensors.len()];
            let mut slot_slices: Vec<u32> = Vec::new();
            let mut free: Vec<usize> = Vec::new();
            // A fused dense head is written before the IR's final layer.
            // Reserve its slot from the beginning, including sparse skips
            // where ordinary tensor lifetimes would otherwise recycle it.
            let reserve_head = allocated[model.head.tensor]
                && specs
                    .iter()
                    .any(|k| k.fused && k.writes.contains(&model.head.tensor));
            if reserve_head {
                slots[model.head.tensor] = 0;
                free.push(model.layers.len());
                slot_slices.push(model.tensors[model.head.tensor].div_ceil(4) as u32);
            }
            for (i, l) in model.layers.iter().enumerate() {
                if !allocated[l.output] || (reserve_head && l.output == model.head.tensor) {
                    continue;
                }
                let slot = free.iter().position(|&v| v < i).unwrap_or(free.len());
                let slices = l.out_channels.div_ceil(4) as u32;
                if slot == free.len() {
                    free.push(0);
                    slot_slices.push(slices);
                } else {
                    slot_slices[slot] = slot_slices[slot].max(slices);
                }
                free[slot] = last[l.output].max(i);
                slots[l.output] = slot;
            }
            (specs, halos, max_halo, slots, slot_slices, allocated)
        } else {
            (vec![wgsl::fsr(options)], vec![], 0, vec![], vec![], vec![])
        };
        let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut cache: HashMap<String, wgpu::ComputePipeline> = HashMap::new();
        let mut layouts: HashMap<bool, wgpu::BindGroupLayout> = HashMap::new();
        let mut kernels = Vec::new();
        for spec in specs {
            if let Ok(dir) = std::env::var("ERIKA_WGPU_SR_DUMP_DIR") {
                use std::hash::{Hash, Hasher};
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                spec.source.hash(&mut hash);
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(
                    std::path::Path::new(&dir).join(format!("{:016x}.wgsl", hash.finish())),
                    &spec.source,
                );
            }
            let layout = layouts
                .entry(spec.uniform)
                .or_insert_with(|| {
                    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some("erika-sr-layout"),
                        entries: &[
                            entry(0, buffer(true, true)),
                            entry(
                                1,
                                wgpu::BindingType::Buffer {
                                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                                    has_dynamic_offset: false,
                                    min_binding_size: None,
                                },
                            ),
                            entry(2, buffer(spec.uniform, false)),
                            entry(
                                3,
                                wgpu::BindingType::Texture {
                                    sample_type: wgpu::TextureSampleType::Float {
                                        filterable: false,
                                    },
                                    view_dimension: wgpu::TextureViewDimension::D2,
                                    multisampled: false,
                                },
                            ),
                            entry(
                                4,
                                wgpu::BindingType::StorageTexture {
                                    access: wgpu::StorageTextureAccess::WriteOnly,
                                    format: if model.is_none() && options.r16 {
                                        wgpu::TextureFormat::R16Float
                                    } else {
                                        wgpu::TextureFormat::Rgba16Float
                                    },
                                    view_dimension: wgpu::TextureViewDimension::D2,
                                },
                            ),
                            entry(
                                5,
                                wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                            ),
                        ],
                    })
                })
                .clone();
            let half_weights: Vec<u16> = if spec.weights_half {
                spec.weights
                    .iter()
                    .flatten()
                    .copied()
                    .map(super::sr::model::f32_to_f16)
                    .collect()
            } else {
                Vec::new()
            };
            let weights = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("erika-sr-weights"),
                contents: if spec.weights_half {
                    bytemuck::cast_slice(&half_weights)
                } else {
                    bytemuck::cast_slice(&spec.weights)
                },
                usage: if spec.uniform {
                    wgpu::BufferUsages::UNIFORM
                } else {
                    wgpu::BufferUsages::STORAGE
                },
            });
            let pipeline = cache
                .entry(spec.source.clone())
                .or_insert_with(|| {
                    let descriptor = wgpu::ShaderModuleDescriptor {
                        label: Some("erika-sr-generated"),
                        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(&spec.source)),
                    };
                    let mut checks = wgpu::ShaderRuntimeChecks::unchecked();
                    checks.bounds_checks = config.bounds_checks;
                    checks.force_loop_bounding = config.loop_bounds;
                    // SAFETY: this is generated WGSL, never arbitrary user shader
                    // source. Model shapes and tile allocations are validated;
                    // staging guards rounded dispatches, all shared cells are
                    // written before a uniform barrier, and all loops have finite
                    // generator/device-bounded trip counts.
                    let shader = unsafe { device.create_shader_module_trusted(descriptor, checks) };
                    let pipeline_layout =
                        device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                            label: Some("erika-sr-pipeline-layout"),
                            bind_group_layouts: &[Some(&layout)],
                            immediate_size: 0,
                        });
                    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        label: Some("erika-sr-pipeline"),
                        layout: Some(&pipeline_layout),
                        module: &shader,
                        entry_point: Some("main"),
                        compilation_options: wgpu::PipelineCompilationOptions {
                            zero_initialize_workgroup_memory: config.zero_workgroup,
                            ..Default::default()
                        },
                        cache: None,
                    })
                })
                .clone();
            kernels.push(Compiled {
                spec,
                pipeline,
                layout,
                weights,
            });
        }
        let error = pollster::block_on(validation.pop())
            .or(pollster::block_on(internal.pop()))
            .or(pollster::block_on(oom.pop()));
        if let Some(e) = error {
            return Err(e.to_string());
        }
        Ok(Self {
            fingerprint: {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                if let Some(m) = &model {
                    m.to_bytes(true).hash(&mut h);
                }
                h.finish()
            },
            model,
            kernels,
            halos,
            max_halo,
            slots,
            slot_slices,
            allocated,
            options,
            sampler: device.create_sampler(&wgpu::SamplerDescriptor::default()),
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Key {
    width: u32,
    height: u32,
    target: [u32; 2],
    token: Option<u64>,
    range: LumaRange,
    kind: InputKind,
    luma: [f32; 3],
}
struct Pool {
    width: u32,
    height: u32,
    target: [u32; 2],
    tile: [u32; 2],
    pitch: u32,
    rows: u32,
    arena: wgpu::Buffer,
    offsets: [[u32; 4]; 64],
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    cached: Option<Key>,
}

impl Pool {
    fn build(
        device: &wgpu::Device,
        n: &Network,
        c: Config,
        width: u32,
        height: u32,
        target: [u32; 2],
    ) -> Result<Self, String> {
        let max = device.limits().max_texture_dimension_2d;
        if width == 0
            || height == 0
            || target.contains(&0)
            || width > max
            || height > max
            || target.iter().any(|&x| x > max)
        {
            return Err(format!(
                "invalid SR extent {width}x{height} -> {target:?} (limit {max})"
            ));
        }
        let mut tile = [c.tile[0].min(width).max(1), c.tile[1].min(height).max(1)];
        let element = if n.options.half { 8u64 } else { 16 };
        let slices = u64::from(n.slot_slices.iter().sum::<u32>());
        let budget = c
            .budget
            .min(device.limits().max_storage_buffer_binding_size as u64)
            .min(device.limits().max_buffer_size);
        let halo = n.max_halo;
        let (pitch, rows, size) = loop {
            let pitch = tile[0] + 2 * halo;
            let rows = tile[1] + 2 * halo;
            let size = u64::from(pitch) * u64::from(rows) * slices * element;
            if size <= budget {
                break (pitch, rows, size.max(16));
            }
            if tile[1] > 1 {
                tile[1] = tile[1].div_ceil(2);
            } else if tile[0] > 1 {
                tile[0] = tile[0].div_ceil(2);
            } else {
                return Err(format!("SR halo requires {size} bytes, budget {budget}"));
            }
        };
        let mut offsets = [[0u32; 4]; 64];
        let mut bases = Vec::new();
        let mut base = 0;
        for &s in &n.slot_slices {
            bases.push(base);
            base += pitch * rows * s;
        }
        if let Some(model) = &n.model {
            for l in &model.layers {
                if n.allocated[l.output] {
                    offsets[l.output / 4][l.output % 4] = bases[n.slots[l.output]];
                }
            }
        }
        let arena = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("erika-sr-tile-arena"),
            size,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("erika-sr-output"),
            size: wgpu::Extent3d {
                width: target[0],
                height: target[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: if n.model.is_none() && n.options.r16 {
                wgpu::TextureFormat::R16Float
            } else {
                wgpu::TextureFormat::Rgba16Float
            },
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Ok(Self {
            width,
            height,
            target,
            tile,
            pitch,
            rows,
            arena,
            offsets,
            texture,
            view,
            cached: None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Output {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub packed_width: u32,
    pub packed_height: u32,
    pub virtual_width: u32,
    pub virtual_height: u32,
    pub input_kind: InputKind,
    pub frame_token: Option<u64>,
    pub cache_hit: bool,
    pub packed: bool,
    serial: u64,
}
struct Pending {
    key: Key,
    serial: u64,
    tiles: u64,
    dispatches: u64,
    duration: Duration,
}
type BuildResult = Result<Arc<Network>, String>;
type TuneResult = (String, Config, Arc<Network>);
static TUNED: OnceLock<Mutex<HashMap<String, Config>>> = OnceLock::new();

pub struct WgpuSr {
    capability: Capability,
    config: Config,
    base_config: Config,
    mode: LumaUpscalerMode,
    status: Status,
    custom: Option<Arc<SrModel>>,
    network: Option<Arc<Network>>,
    job: Option<mpsc::Receiver<BuildResult>>,
    cache: Vec<(LumaUpscalerMode, Arc<Network>)>,
    pool: Option<Pool>,
    stats: Stats,
    failure: Option<Failure>,
    pending: Option<Pending>,
    serial: u64,
    timestamps: Option<wgpu::QuerySet>,
    adapter: wgpu::Adapter,
    tuning: Option<mpsc::Receiver<TuneResult>>,
    tune_key: Option<String>,
}

impl WgpuSr {
    pub fn new(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Self {
        Self::new_with_config(adapter, device, Config::for_adapter(adapter))
    }
    pub fn new_with_config(
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        mut config: Config,
    ) -> Self {
        config.kernel.r16 &= adapter
            .get_texture_format_features(wgpu::TextureFormat::R16Float)
            .allowed_usages
            .contains(wgpu::TextureUsages::STORAGE_BINDING);
        if adapter.get_info().backend == wgpu::Backend::Vulkan {
            config.kernel.matrix_size = 16;
            config.kernel.cooperative &= adapter.cooperative_matrix_properties().iter().any(|p| {
                p.m_size == 16
                    && p.n_size == 16
                    && p.k_size == 16
                    && p.ab_type == wgpu::CooperativeScalarType::F16
                    && p.cr_type == wgpu::CooperativeScalarType::F32
                    && !p.saturating_accumulation
            });
            let info = adapter.get_info();
            config.kernel.cooperative &=
                info.subgroup_min_size == 32 && info.subgroup_max_size == 32;
        } else {
            config.kernel.matrix_size = 8;
            config.kernel.cooperative &= adapter.get_info().backend == wgpu::Backend::Metal;
        }
        let mut capability = Capability::inspect(adapter, device);
        if device.limits().max_storage_buffers_per_shader_stage < 2 {
            capability
                .reasons
                .push("SR requires two storage-buffer bindings".into());
        }
        if device.limits().max_uniform_buffer_binding_size < mem::size_of::<Params>() as u64 {
            capability
                .reasons
                .push("SR parameter block exceeds uniform limit".into());
        }
        capability.supported = capability.reasons.is_empty();
        Self {
            capability,
            config,
            base_config: config,
            mode: LumaUpscalerMode::Off,
            status: Status::Off,
            custom: None,
            network: None,
            job: None,
            cache: Vec::new(),
            pool: None,
            stats: Stats::default(),
            failure: None,
            pending: None,
            serial: 0,
            timestamps: None,
            adapter: adapter.clone(),
            tuning: None,
            tune_key: None,
        }
    }
    pub fn mode(&self) -> LumaUpscalerMode {
        self.mode
    }
    pub fn status(&self) -> Status {
        self.status
    }
    pub fn stats(&self) -> Stats {
        self.stats
    }
    pub fn capability(&self) -> &Capability {
        &self.capability
    }
    pub fn last_failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }
    pub fn half(&self) -> bool {
        self.network.as_ref().is_some_and(|n| n.options.half)
    }
    pub fn active_options(&self) -> Option<Options> {
        self.network.as_ref().map(|n| n.options)
    }
    pub fn scratch_bytes(&self) -> u64 {
        self.pool.as_ref().map_or(0, |p| p.arena.size())
    }
    /// Optional GPU timestamps around the complete network, for profiling.
    pub fn set_timestamp_queries(&mut self, queries: Option<wgpu::QuerySet>) {
        self.timestamps = queries;
    }
    pub fn set_custom_model(&mut self, device: &wgpu::Device, model: Option<Arc<SrModel>>) {
        self.custom = model;
        self.cache.retain(|(m, _)| *m != LumaUpscalerMode::Custom);
        if self.mode == LumaUpscalerMode::Custom {
            self.status = Status::Inactive;
            let _ = self.set_mode(device, self.mode);
        }
    }
    fn fail(&mut self, stage: FailureStage, reason: impl Into<String>) -> Failure {
        let f = Failure::new(stage, self.mode, None, reason);
        self.status = Status::Inactive;
        self.pending = None;
        self.stats.fallback_count += 1;
        if let Some(p) = &mut self.pool {
            p.cached = None;
        }
        self.failure = Some(f.clone());
        f
    }
    pub fn set_mode(
        &mut self,
        device: &wgpu::Device,
        mode: LumaUpscalerMode,
    ) -> Result<(), Failure> {
        if self.mode == mode
            && matches!(self.status, Status::Off | Status::Building | Status::Scalar)
        {
            return Ok(());
        }
        self.config = self.base_config;
        self.mode = mode;
        self.pool = None;
        self.pending = None;
        self.job = None;
        self.tuning = None;
        self.tune_key = None;
        self.network = None;
        self.failure = None;
        if mode == LumaUpscalerMode::Off {
            self.status = Status::Off;
            return Ok(());
        }
        if !self.capability.supported {
            return Err(self.fail(FailureStage::Capability, self.capability.reasons.join("; ")));
        }
        if mode.family() != UpscalerFamily::Network && mode != LumaUpscalerMode::Fsr1 {
            return Err(self.fail(
                FailureStage::Capability,
                "this platform-specific upscaler is unavailable on wgpu",
            ));
        }
        if let Some((_, n)) = self.cache.iter().find(|(m, _)| *m == mode) {
            self.network = Some(n.clone());
            self.status = Status::Scalar;
            return Ok(());
        }
        let custom = self.custom.clone();
        let config = self.config;
        let device = device.clone();
        let (tx, rx) = mpsc::channel();
        self.status = Status::Building;
        self.job = Some(rx);
        if let Err(e) = std::thread::Builder::new()
            .name("erika-wgpu-sr-build".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let model = if mode == LumaUpscalerMode::Fsr1 {
                        None
                    } else if mode == LumaUpscalerMode::Custom {
                        Some(custom.ok_or("no custom upscaler model loaded")?)
                    } else {
                        Some(catalog::execution_model(mode).map_err(|e| e.to_string())?)
                    };
                    Network::build(&device, model, config).map(Arc::new)
                }))
                .unwrap_or_else(|_| Err("wgpu pipeline compilation panicked".into()));
                let _ = tx.send(result);
            })
        {
            return Err(self.fail(FailureStage::Pipeline, e.to_string()));
        }
        Ok(())
    }
    pub fn poll_build(&mut self) {
        if self.pending.is_none()
            && let Some(rx) = &self.tuning
        {
            match rx.try_recv() {
                Ok((key, config, network)) => {
                    // encode polls before creating a tentative output; an old
                    // output's commit must never be redirected to a new pool.
                    if self.pending.is_none() {
                        self.pool = None;
                        self.config = config;
                        self.cache.retain(|(m, _)| *m != self.mode);
                        if self.cache.len() >= 4 {
                            self.cache.remove(0);
                        }
                        self.cache.push((self.mode, network.clone()));
                        self.network = Some(network);
                        self.tune_key = Some(key);
                        self.tuning = None;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.tuning = None,
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        let Some(rx) = &self.job else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(n)) => {
                if self.cache.len() >= 4 {
                    self.cache.remove(0);
                }
                self.cache.push((self.mode, n.clone()));
                self.network = Some(n);
                self.status = Status::Scalar;
                self.job = None;
            }
            Ok(Err(e)) => {
                self.job = None;
                self.fail(FailureStage::Pipeline, e);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                self.fail(FailureStage::Pipeline, "compilation worker disconnected");
            }
            Err(mpsc::TryRecvError::Empty) => (),
        }
    }
    /// Tuning keeps the current pipeline usable. Only a completed worker result
    /// is installed, between submissions, without waiting for the GPU here.
    pub fn is_tuning(&self) -> bool {
        self.tuning.is_some()
    }
    pub fn selected_config(&self) -> Config {
        self.config
    }

    fn start_tuning(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        input: Input<'_>,
        width: u32,
        height: u32,
        target: [u32; 2],
        range: LumaRange,
    ) {
        let Some(network) = self.network.clone() else {
            return;
        };
        let key = format!(
            "{:?}/{:x}/{:?}/{}x{}/{}x{}",
            self.adapter.get_info(),
            network.fingerprint,
            input.kind(),
            width.div_ceil(512),
            height.div_ceil(512),
            target[0].div_ceil(512),
            target[1].div_ceil(512)
        );
        if self.tune_key.as_ref() == Some(&key) || self.tuning.is_some() {
            return;
        }
        self.tune_key = Some(key.clone());
        let cached = TUNED
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .get(&key)
            .copied();
        let config = Config {
            kernel: network.options,
            device_profile: DeviceProfile::Portable,
            ..self.config
        };
        let adapter = self.adapter.clone();
        let device = device.clone();
        let queue = queue.clone();
        let mode = self.mode;
        let kind = input.kind();
        let luma = input.luma_coefficients();
        let (tx, rx) = mpsc::channel();
        self.tuning = Some(rx);
        let _ = std::thread::Builder::new()
            .name("erika-wgpu-sr-tune".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut candidates = vec![config];
                    if let Some(c) = cached {
                        candidates = vec![c];
                    } else {
                        let mut c = config;
                        c.kernel.output_block = 16;
                        c.kernel.unroll = !c.kernel.unroll;
                        if c.kernel.register_blocking {
                            c.kernel.register_blocking = false;
                            c.kernel.pixels = 1;
                        }
                        candidates.push(c);
                        let mut c = config;
                        c.kernel.half = !c.kernel.half;
                        c.kernel.half_products = !c.kernel.half_products;
                        c.kernel.fusion = false;
                        candidates.push(c);
                        let mut c = config;
                        c.kernel.fusion_max_channels = 8;
                        c.kernel.fusion_depth = 2;
                        c.kernel.group = [16, 8];
                        candidates.push(c);
                        let mut c = config;
                        c.kernel.weights_half = !c.kernel.weights_half;
                        candidates.push(c);
                        if network
                            .model
                            .as_ref()
                            .is_some_and(|m| m.tensors.iter().all(|&c| c <= 4))
                        {
                            let mut c = config;
                            c.kernel.fusion = false;
                            c.kernel.pixels = 2;
                            c.kernel.register_blocking = true;
                            candidates.push(c);
                        }
                        if config.kernel.cooperative {
                            let mut c = config;
                            c.kernel.cooperative = false;
                            candidates.push(c);
                        }
                        if network.model.is_none() {
                            candidates.truncate(1);
                            let mut c = config;
                            c.kernel.fsr_group = [8, 8];
                            candidates.push(c);
                        }
                    }
                    // All pipeline creation precedes timed work. Errors in an
                    // optional candidate preserve the already working baseline.
                    let mut built = Vec::new();
                    for mut c in candidates {
                        c.autotune = false;
                        let n = if c.kernel == config.kernel {
                            Ok(network.clone())
                        } else {
                            Network::build(&device, network.model.clone(), c).map(Arc::new)
                        };
                        if let Ok(n) = n {
                            built.push((c, n));
                        }
                    }
                    if cached.is_some() {
                        if let Some((mut c, n)) = built.pop() {
                            c.autotune = true;
                            let _ = tx.send((key, c, n));
                        }
                        return;
                    }
                    // External Android/OHOS images can outlive neither their
                    // producer lease nor the frame submission. Tune on an
                    // owned representative image of the first frame's size,
                    // never retain or re-read an imported video texture.
                    let p010 = kind == InputKind::PlanarLuma && luma[1] > 0.0;
                    let (format, bpp) = if kind == InputKind::NonlinearRgb {
                        (wgpu::TextureFormat::Rgba8Unorm, 4)
                    } else if p010 {
                        (wgpu::TextureFormat::Rg8Unorm, 2)
                    } else {
                        (wgpu::TextureFormat::R8Unorm, 1)
                    };
                    let Some(nbytes) =
                        usize::try_from(u64::from(width) * u64::from(height) * u64::from(bpp))
                            .ok()
                            .filter(|&n| n <= 128 << 20)
                    else {
                        return;
                    };
                    let mut bytes = Vec::new();
                    if bytes.try_reserve_exact(nbytes).is_err() {
                        return;
                    }
                    bytes.resize(nbytes, 0u8);
                    for (i, pixel) in bytes.chunks_exact_mut(bpp as usize).enumerate() {
                        let v = ((i ^ (i / (width as usize)).wrapping_mul(37)) & 255) as u8;
                        pixel.fill(v);
                        if bpp == 4 {
                            pixel[3] = 255;
                        }
                    }
                    let source = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("SR tuning input"),
                        size: wgpu::Extent3d {
                            width,
                            height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    });
                    queue.write_texture(
                        source.as_image_copy(),
                        &bytes,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(width * bpp),
                            rows_per_image: Some(height),
                        },
                        source.size(),
                    );
                    let view = source.create_view(&Default::default());
                    // A test harness may coordinate this worker with other GPU
                    // benchmarks. Production has no filesystem lock dependency.
                    let lock =
                        std::env::var_os("ERIKA_WGPU_SR_TUNE_LOCK").map(std::path::PathBuf::from);
                    if let Some(p) = &lock {
                        while std::fs::create_dir(p).is_err() {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                    struct Unlock(Option<std::path::PathBuf>);
                    impl Drop for Unlock {
                        fn drop(&mut self) {
                            if let Some(p) = &self.0 {
                                let _ = std::fs::remove_dir(p);
                            }
                        }
                    }
                    let _lock = Unlock(lock);
                    let mut best = None;
                    let mut best_ms = f64::INFINITY;
                    for (c, n) in built {
                        let mut sr = WgpuSr::new_with_config(&adapter, &device, c);
                        sr.mode = mode;
                        sr.status = Status::Scalar;
                        sr.network = Some(n.clone());
                        let sample = || {
                            if kind == InputKind::NonlinearRgb {
                                Input::NonlinearRgb {
                                    view: &view,
                                    luma_coefficients: luma,
                                }
                            } else {
                                Input::PlanarLuma {
                                    view: &view,
                                    p010_le_bytes: luma[1] > 0.0,
                                }
                            }
                        };
                        let timestamp = device.features().contains(wgpu::Features::TIMESTAMP_QUERY);
                        let qs = timestamp.then(|| {
                            device.create_query_set(&wgpu::QuerySetDescriptor {
                                label: Some("SR tune timestamps"),
                                ty: wgpu::QueryType::Timestamp,
                                count: 2,
                            })
                        });
                        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
                            label: None,
                            size: 256,
                            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                            mapped_at_creation: false,
                        });
                        let read = device.create_buffer(&wgpu::BufferDescriptor {
                            label: None,
                            size: 16,
                            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                            mapped_at_creation: false,
                        });
                        let mut times = Vec::new();
                        for frame in 0..4 {
                            sr.timestamps = qs.clone();
                            let mut e = device.create_command_encoder(&Default::default());
                            let start = Instant::now();
                            let Ok(Some(out)) = sr.encode(
                                &device,
                                &queue,
                                &mut e,
                                sample(),
                                width,
                                height,
                                target,
                                range,
                                None,
                            ) else {
                                break;
                            };
                            if let Some(qs) = &qs {
                                e.resolve_query_set(qs, 0..2, &resolve, 0);
                                e.copy_buffer_to_buffer(&resolve, 0, &read, 0, 16);
                            }
                            queue.submit([e.finish()]);
                            if sr.commit_encoded_output(&out).is_err() {
                                break;
                            }
                            let ms = if timestamp {
                                let (tx, rx) = mpsc::channel();
                                read.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                                    let _ = tx.send(r);
                                });
                                if device.poll(wgpu::PollType::wait_indefinitely()).is_err()
                                    || !matches!(rx.recv(), Ok(Ok(())))
                                {
                                    break;
                                }
                                let b = read.slice(..).get_mapped_range();
                                let t0 = u64::from_le_bytes(b[..8].try_into().unwrap());
                                let t1 = u64::from_le_bytes(b[8..16].try_into().unwrap());
                                drop(b);
                                read.unmap();
                                (t1.saturating_sub(t0)) as f64 * queue.get_timestamp_period() as f64
                                    / 1e6
                            } else {
                                if device.poll(wgpu::PollType::wait_indefinitely()).is_err() {
                                    break;
                                }
                                start.elapsed().as_secs_f64() * 1e3
                            };
                            if frame > 0 {
                                times.push(ms);
                            }
                        }
                        if times.len() == 3 {
                            times.sort_by(f64::total_cmp);
                            if times[1] < best_ms {
                                best_ms = times[1];
                                best = Some((c, n));
                            }
                        }
                    }
                    if let Some((mut c, n)) = best {
                        c.autotune = true;
                        let mut cache = TUNED.get_or_init(Default::default).lock().unwrap();
                        if cache.len() >= 64 {
                            cache.clear();
                        }
                        cache.insert(key.clone(), c);
                        drop(cache);
                        let _ = tx.send((key, c, n));
                    }
                }));
            });
    }
    /// Test/tool helper. Renderers use `poll_build` and never wait.
    pub fn wait_ready(&mut self) -> Result<(), Failure> {
        if let Some(rx) = self.job.take() {
            match rx.recv() {
                Ok(Ok(n)) => {
                    if self.cache.len() >= 4 {
                        self.cache.remove(0);
                    }
                    self.cache.push((self.mode, n.clone()));
                    self.network = Some(n);
                    self.status = Status::Scalar;
                }
                Ok(Err(e)) => return Err(self.fail(FailureStage::Pipeline, e)),
                Err(e) => return Err(self.fail(FailureStage::Pipeline, e.to_string())),
            }
        }
        if let Some(f) = &self.failure {
            return Err(f.clone());
        }
        Ok(())
    }
    pub fn handle_deferred_encode_failure(
        &mut self,
        kind: InputKind,
        reason: impl Into<String>,
    ) -> Failure {
        let mut f = self.fail(FailureStage::Encode, reason);
        f.input_kind = Some(kind);
        self.failure = Some(f.clone());
        f
    }
    pub fn commit_encoded_output(&mut self, o: &Output) -> Result<(), Failure> {
        if o.cache_hit {
            return Ok(());
        }
        let valid = self.pending.as_ref().is_some_and(|p| {
            p.serial == o.serial && p.key.token == o.frame_token && p.key.kind == o.input_kind
        }) && self
            .pool
            .as_ref()
            .is_some_and(|p| p.target == [o.packed_width, o.packed_height]);
        if !valid {
            return Err(self.fail(
                FailureStage::Encode,
                "output does not match the pending encode",
            ));
        }
        let p = self.pending.take().unwrap();
        self.pool.as_mut().unwrap().cached = Some(p.key);
        self.stats.upscaled_frames += 1;
        self.stats.encoded_tiles += p.tiles;
        self.stats.compute_dispatches += p.dispatches;
        self.stats.last_encode_duration = p.duration;
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        input: Input<'_>,
        width: u32,
        height: u32,
        target: [u32; 2],
        range: LumaRange,
        token: Option<u64>,
    ) -> Result<Option<Output>, Failure> {
        self.poll_build();
        if matches!(self.status, Status::Off | Status::Building) {
            return Ok(None);
        }
        if self.status == Status::Inactive {
            return Err(self.failure.clone().unwrap_or_else(|| {
                Failure::new(FailureStage::Encode, self.mode, None, "upscaler inactive")
            }));
        }
        if self.pending.is_some() {
            return Err(self.fail(
                FailureStage::Encode,
                "previous tentative output must be committed or failed",
            ));
        }
        let luma = input.luma_coefficients();
        let input_texture = match input {
            Input::PlanarLuma { view, .. } | Input::NonlinearRgb { view, .. } => view.texture(),
        };
        if width == 0
            || height == 0
            || input_texture.width() < width
            || input_texture.height() < height
        {
            return Err(self.fail(
                FailureStage::Frame,
                "input texture is smaller than the declared frame",
            ));
        }
        if !luma.iter().all(|x| x.is_finite()) || luma.iter().sum::<f32>() <= 0.0 {
            return Err(self.fail(FailureStage::Frame, "invalid input luma coefficients"));
        }
        if self.config.autotune && self.pending.is_none() {
            self.start_tuning(device, queue, input, width, height, target, range);
        }
        let started = Instant::now();
        let network = self.network.as_ref().unwrap().clone();
        let packed = network.model.is_some();
        let target = if packed { [width, height] } else { target };
        let key = Key {
            width,
            height,
            target,
            token,
            range,
            kind: input.kind(),
            luma,
        };
        if self
            .pool
            .as_ref()
            .is_none_or(|p| p.width != width || p.height != height || p.target != target)
        {
            match Pool::build(device, &network, self.config, width, height, target) {
                Ok(p) => self.pool = Some(p),
                Err(e) => return Err(self.fail(FailureStage::Resource, e)),
            }
        }
        let pool = self.pool.as_ref().unwrap();
        let cached = token.is_some() && pool.cached == Some(key);
        self.serial = self.serial.wrapping_add(1);
        let output = Output {
            texture: pool.texture.clone(),
            view: pool.view.clone(),
            packed_width: target[0],
            packed_height: target[1],
            virtual_width: if packed { width * 2 } else { target[0] },
            virtual_height: if packed { height * 2 } else { target[1] },
            input_kind: input.kind(),
            frame_token: token,
            cache_hit: cached,
            packed,
            serial: self.serial,
        };
        if cached {
            self.stats.cache_hits += 1;
            return Ok(Some(output));
        }
        let (offset, scale) = network
            .model
            .as_ref()
            .map_or((0.0, 1.0), |m| input_normalization(m, range));
        let mut params = Vec::new();
        let mut dispatches = Vec::new();
        let profile_ring = std::env::var("ERIKA_WGPU_SR_PROFILE_RING").unwrap_or_default();
        let mut tiles = 0;
        let mut add_tile = |cx: u32, cy: u32, cw: u32, ch: u32| {
            tiles += 1;
            for (ki, k) in network.kernels.iter().enumerate() {
                if (profile_ring == "only" && !k.spec.ring)
                    || (profile_ring == "skip" && k.spec.ring)
                {
                    continue;
                }
                let h = if k.spec.head {
                    0
                } else {
                    network
                        .halos
                        .get(network.model.as_ref().unwrap().layers[k.spec.layer].output)
                        .copied()
                        .unwrap_or(0)
                };
                let x = cx.saturating_sub(h);
                let y = cy.saturating_sub(h);
                let right = (cx + cw + h).min(width);
                let bottom = (cy + ch + h).min(height);
                if k.spec.ring && x > 0 && y > 0 && right < width && bottom < height {
                    continue;
                }
                let rect = if packed {
                    [x as i32, y as i32, (right - x) as i32, (bottom - y) as i32]
                } else {
                    [0, 0, target[0] as i32, target[1] as i32]
                };
                let mut offsets = [[0; 4]; 64];
                for (local, &actual) in k.spec.bindings.iter().enumerate() {
                    offsets[local / 4][local % 4] = pool.offsets[actual / 4][actual % 4];
                }
                params.push(Params {
                    size: [width, height, target[0], target[1]],
                    tile: [
                        cx as i32 - network.max_halo as i32,
                        cy as i32 - network.max_halo as i32,
                        pool.pitch as i32,
                        pool.rows as i32,
                    ],
                    rect,
                    range: if packed {
                        [offset, scale, 1.0 / scale, 0.0]
                    } else {
                        // Match the CPU oracle's correctly rounded ratios.
                        // Shader division can use an approximate reciprocal,
                        // shifting an integer EASU footprint by one texel.
                        [
                            width as f32 / target[0] as f32,
                            height as f32 / target[1] as f32,
                            0.0,
                            super::sr::fsr1::rcas_strength(
                                super::sr::fsr1::DEFAULT_SHARPNESS_STOPS,
                            ),
                        ]
                    },
                    luma: [luma[0], luma[1], luma[2], k.spec.output_start as f32],
                    offsets,
                });
                let groups = if k.spec.ring {
                    let n = if width < 2 || height < 2 {
                        width * height
                    } else {
                        2 * width + 2 * (height - 2)
                    };
                    [
                        n.div_ceil(64),
                        1,
                        network.model.as_ref().unwrap().layers[k.spec.layer]
                            .out_channels
                            .div_ceil(4) as u32,
                    ]
                } else {
                    [
                        (rect[2] as u32).div_ceil(k.spec.group[0] * k.spec.pixels),
                        (rect[3] as u32).div_ceil(k.spec.group[1]),
                        1,
                    ]
                };
                dispatches.push((ki, groups));
            }
        };
        if packed {
            for y in (0..height).step_by(pool.tile[1] as usize) {
                for x in (0..width).step_by(pool.tile[0] as usize) {
                    add_tile(
                        x,
                        y,
                        (width - x).min(pool.tile[0]),
                        (height - y).min(pool.tile[1]),
                    );
                }
            }
        } else {
            add_tile(0, 0, width, height);
        }
        let stride = (mem::size_of::<Params>() as u64).div_ceil(u64::from(
            device.limits().min_uniform_buffer_offset_alignment,
        )) * u64::from(device.limits().min_uniform_buffer_offset_alignment);
        let total = stride * params.len() as u64;
        if total > device.limits().max_buffer_size || total > u32::MAX as u64 {
            return Err(self.fail(
                FailureStage::Resource,
                "SR parameter buffer exceeds device limit",
            ));
        }
        let mut bytes = vec![0u8; total as usize];
        for (i, p) in params.iter().enumerate() {
            let start = i * stride as usize;
            bytes[start..start + mem::size_of::<Params>()].copy_from_slice(bytemuck::bytes_of(p));
        }
        let parameters = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("erika-sr-frame-params"),
            contents: &bytes,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let view = match input {
            Input::PlanarLuma { view, .. } | Input::NonlinearRgb { view, .. } => view,
        };
        let groups: Vec<_> = network
            .kernels
            .iter()
            .map(|k| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("erika-sr-frame-bindings"),
                    layout: &k.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &parameters,
                                offset: 0,
                                size: NonZeroU64::new(mem::size_of::<Params>() as u64),
                            }),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: pool.arena.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: k.weights.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::TextureView(&pool.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: wgpu::BindingResource::Sampler(&network.sampler),
                        },
                    ],
                })
            })
            .collect();
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("erika-sr"),
            timestamp_writes: self.timestamps.as_ref().map(|query_set| {
                wgpu::ComputePassTimestampWrites {
                    query_set,
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }
            }),
        });
        for (i, (ki, g)) in dispatches.iter().enumerate() {
            pass.set_pipeline(&network.kernels[*ki].pipeline);
            pass.set_bind_group(0, &groups[*ki], &[(i as u64 * stride) as u32]);
            pass.dispatch_workgroups(g[0], g[1], g[2]);
        }
        drop(pass);
        self.pending = Some(Pending {
            key,
            serial: self.serial,
            tiles,
            dispatches: dispatches.len() as u64,
            duration: started.elapsed(),
        });
        Ok(Some(output))
    }
}
