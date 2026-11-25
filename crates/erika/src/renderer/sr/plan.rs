//! Portable WGSL super-resolution planner shared by browser WebGPU and native tests.
//!
//! Produces standard WebGPU-valid WGSL compute kernels, weight buffers, tiled
//! tensor-arena layouts, and dynamic uniform parameter blocks without depending
//! on `wgpu` or OS threads.

use std::sync::Arc;

use super::{
    catalog, fsr1,
    model::SrModel,
    reference::{LumaRange, input_normalization},
    wgsl::{self, Kernel, Options},
};
use crate::core::{PlayerError, Result};
use crate::renderer::pipeline::{LumaUpscalerMode, UpscalerFamily};

/// Byte size of the WGSL `Params` uniform struct shared by every kernel.
///
/// Layout: `size (16) + tile (16) + rect (16) + range (16) + luma (16) + offsets[64] (1024)`.
pub const PARAMS_BYTES: usize = 1104;

/// Default tile dimensions for browser WebGPU scratch budgeting.
pub const DEFAULT_TILE: [u32; 2] = [512, 256];

/// Default upper bound on the tiled feature arena buffer (64 MiB).
pub const DEFAULT_ARENA_BUDGET_BYTES: u64 = 64 << 20;

/// All upscaler modes supported by portable WebGPU compute shaders.
pub const WEB_UPSCALER_MODES: [LumaUpscalerMode; 19] = [
    LumaUpscalerMode::Off,
    LumaUpscalerMode::Fsr1,
    LumaUpscalerMode::ArtCnnC4F16,
    LumaUpscalerMode::ArtCnnC4F16Ds,
    LumaUpscalerMode::ArtCnnC4F16Dn,
    LumaUpscalerMode::ArtCnnC4F32,
    LumaUpscalerMode::ArtCnnC4F32Ds,
    LumaUpscalerMode::ArtCnnC4F32Dn,
    LumaUpscalerMode::Anime4kS,
    LumaUpscalerMode::Anime4kM,
    LumaUpscalerMode::Anime4kL,
    LumaUpscalerMode::Anime4kVl,
    LumaUpscalerMode::Anime4kUl,
    LumaUpscalerMode::Anime4kDenoiseS,
    LumaUpscalerMode::Anime4kDenoiseM,
    LumaUpscalerMode::Anime4kDenoiseL,
    LumaUpscalerMode::Anime4kDenoiseVl,
    LumaUpscalerMode::Anime4kDenoiseUl,
    LumaUpscalerMode::Custom,
];

/// Relevant limits and features queried from a WebGPU `GPUDevice`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceCapabilities {
    pub shader_f16: bool,
    pub max_compute_workgroup_storage_size: u32,
    pub max_uniform_buffer_binding_size: u64,
    pub max_storage_buffer_binding_size: u64,
    pub max_buffer_size: u64,
    pub max_compute_workgroup_size_x: u32,
    pub max_compute_workgroup_size_y: u32,
    pub max_compute_invocations_per_workgroup: u32,
    pub max_texture_dimension_2d: u32,
    pub min_uniform_buffer_offset_alignment: u32,
    pub max_storage_buffers_per_shader_stage: u32,
}

impl Default for DeviceCapabilities {
    /// WebGPU specification baseline limits (guaranteed on every core device).
    fn default() -> Self {
        Self {
            shader_f16: false,
            max_compute_workgroup_storage_size: 16384,
            max_uniform_buffer_binding_size: 65536,
            max_storage_buffer_binding_size: 128 << 20,
            max_buffer_size: 256 << 20,
            max_compute_workgroup_size_x: 256,
            max_compute_workgroup_size_y: 256,
            max_compute_invocations_per_workgroup: 256,
            max_texture_dimension_2d: 8192,
            min_uniform_buffer_offset_alignment: 256,
            max_storage_buffers_per_shader_stage: 8,
        }
    }
}

impl DeviceCapabilities {
    pub fn validate(&self) -> Result<()> {
        if self.max_storage_buffers_per_shader_stage < 2 {
            return Err(PlayerError::Renderer(
                "WebGPU SR requires at least 2 storage buffers per compute stage".into(),
            ));
        }
        if (self.max_uniform_buffer_binding_size as usize) < PARAMS_BYTES {
            return Err(PlayerError::Renderer(format!(
                "WebGPU uniform limit ({}) is smaller than SR parameter block ({PARAMS_BYTES})",
                self.max_uniform_buffer_binding_size
            )));
        }
        if self.min_uniform_buffer_offset_alignment == 0
            || !self.min_uniform_buffer_offset_alignment.is_power_of_two()
        {
            return Err(PlayerError::Renderer(
                "WebGPU minUniformBufferOffsetAlignment must be a power of two".into(),
            ));
        }
        if self.max_compute_workgroup_size_x == 0
            || self.max_compute_workgroup_size_y == 0
            || self.max_compute_invocations_per_workgroup == 0
        {
            return Err(PlayerError::Renderer(
                "WebGPU compute workgroup limits must be non-zero".into(),
            ));
        }
        Ok(())
    }
}

/// Compiled mode-level WGSL kernels and tensor lifetime schedule.
#[derive(Debug, Clone)]
pub struct SrPlan {
    pub mode: LumaUpscalerMode,
    pub packed: bool,
    pub half: bool,
    pub kernels: Vec<Kernel>,
    model: Option<Arc<SrModel>>,
    halos: Vec<u32>,
    max_halo: u32,
    slots: Vec<usize>,
    slot_slices: Vec<u32>,
    allocated: Vec<bool>,
}

/// Per-frame tiled dispatch schedule and packed dynamic uniform buffer.
#[derive(Debug, Clone)]
pub struct FramePlan {
    pub output_width: u32,
    pub output_height: u32,
    pub virtual_width: u32,
    pub virtual_height: u32,
    pub packed: bool,
    pub arena_bytes: u64,
    pub tile_width: u32,
    pub tile_height: u32,
    pub tiles: u32,
    pub params_stride: u32,
    pub params_bytes: Vec<u8>,
    /// Flat groups of 5 `u32` values: `[kernel_index, params_offset_bytes, gx, gy, gz]`.
    pub dispatches: Vec<u32>,
}

impl SrPlan {
    /// Compiles `mode` into portable WebGPU WGSL compute kernels tailored to `caps`.
    pub fn compile(
        mode: LumaUpscalerMode,
        custom: Option<Arc<SrModel>>,
        caps: DeviceCapabilities,
    ) -> Result<Self> {
        caps.validate()?;
        if mode == LumaUpscalerMode::Off {
            return Err(PlayerError::Renderer("upscaler mode is off".into()));
        }
        if mode.family() != UpscalerFamily::Network && mode != LumaUpscalerMode::Fsr1 {
            return Err(PlayerError::Renderer(format!(
                "upscaler mode {} is platform-specific and unavailable on WebGPU",
                mode.label()
            )));
        }

        let model =
            if mode == LumaUpscalerMode::Fsr1 {
                None
            } else if mode == LumaUpscalerMode::Custom {
                Some(custom.ok_or_else(|| {
                    PlayerError::Renderer("no custom upscaler model loaded".into())
                })?)
            } else {
                Some(catalog::execution_model(mode)?)
            };

        let mut options = Options {
            group: [8, 8],
            fsr_group: [16, 16],
            pixels: 1,
            register_blocking: false,
            output_block: 8,
            half: caps.shader_f16,
            weights_half: false,
            half_products: false,
            narrow_half_products: false,
            half_accum: false,
            mad: false,
            compact_taps: false,
            subgroup: false,
            cooperative: false,
            r16: false,
            gather: false,
            fsr_unroll: true,
            fsr_half_math: false,
            shared_bytes: (caps.max_compute_workgroup_storage_size as usize).min(28 * 1024),
            uniform_bytes: (caps.max_uniform_buffer_binding_size as usize).min(16384),
            fusion: true,
            staging: true,
            unroll: false,
            fusion_depth: 2,
            fusion_max_channels: 4,
            ..Options::default()
        };
        if model.is_none() {
            options.group = options.fsr_group;
        }
        options.group[0] = options.group[0]
            .max(1)
            .min(caps.max_compute_workgroup_size_x);
        options.group[1] = options.group[1]
            .max(1)
            .min(caps.max_compute_workgroup_size_y);
        while options.group[0] * options.group[1] > caps.max_compute_invocations_per_workgroup {
            if options.group[1] > 1 {
                options.group[1] = (options.group[1] / 2).max(1);
            } else {
                options.group[0] = (options.group[0] / 2).max(1);
            }
        }

        let (kernels, halos, max_halo, slots, slot_slices, allocated) = if let Some(m) = &model {
            m.validate()?;
            if m.tensors.len() > 256 {
                return Err(PlayerError::Renderer(
                    "WebGPU SR supports at most 256 tensors".into(),
                ));
            }
            let specs = wgsl::network(m, options);
            let halos: Vec<u32> = m.tensor_halos().into_iter().map(|h| h as u32).collect();
            let single_fused = specs.len() == 1 && specs[0].fused;
            let allocated: Vec<bool> = (0..m.tensors.len())
                .map(|t| specs.iter().any(|k| k.writes.contains(&t)))
                .collect();
            let max_halo = if single_fused {
                0
            } else {
                halos.iter().copied().max().unwrap_or(0)
            };

            let mut last = vec![0usize; m.tensors.len()];
            for (i, l) in m.layers.iter().enumerate() {
                for input in &l.inputs {
                    last[input.tensor] = i;
                }
                if let Some(t) = l.residual {
                    last[t] = i;
                }
            }
            last[m.head.tensor] = m.layers.len();
            if specs.iter().all(|k| k.fused) {
                last.fill(0);
                for pair in specs.windows(2) {
                    if let Some(&written) = pair[0].writes.first() {
                        last[written] = pair[1].layer;
                    }
                }
                last[m.head.tensor] = m.layers.len();
            }

            let mut slots = vec![0usize; m.tensors.len()];
            let mut slot_slices: Vec<u32> = Vec::new();
            let mut free: Vec<usize> = Vec::new();
            let reserve_head = allocated[m.head.tensor]
                && specs
                    .iter()
                    .any(|k| k.fused && k.writes.contains(&m.head.tensor));
            if reserve_head {
                slots[m.head.tensor] = 0;
                free.push(m.layers.len());
                slot_slices.push(m.tensors[m.head.tensor].div_ceil(4) as u32);
            }
            for (i, l) in m.layers.iter().enumerate() {
                if !allocated[l.output] || (reserve_head && l.output == m.head.tensor) {
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

        Ok(Self {
            mode,
            packed: model.is_some(),
            half: options.half,
            kernels,
            model,
            halos,
            max_halo,
            slots,
            slot_slices,
            allocated,
        })
    }

    /// Computes tile sizing, feature-arena allocation, aligned `Params` bytes,
    /// and the ordered dispatch list for one input frame.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_frame(
        &self,
        width: u32,
        height: u32,
        target_width: u32,
        target_height: u32,
        luma: [f32; 3],
        range: LumaRange,
        caps: DeviceCapabilities,
    ) -> Result<FramePlan> {
        caps.validate()?;
        let target = if self.packed {
            [width, height]
        } else {
            [target_width.max(1), target_height.max(1)]
        };
        let max_dim = caps.max_texture_dimension_2d;
        if width == 0
            || height == 0
            || width > max_dim
            || height > max_dim
            || target[0] == 0
            || target[1] == 0
            || target[0] > max_dim
            || target[1] > max_dim
        {
            return Err(PlayerError::Renderer(format!(
                "invalid SR extent {width}x{height} -> {}x{} (limit {max_dim})",
                target[0], target[1]
            )));
        }
        if !luma.iter().all(|v| v.is_finite()) || luma.iter().sum::<f32>() <= 0.0 {
            return Err(PlayerError::Renderer(
                "invalid input luma coefficients".into(),
            ));
        }

        let mut tile = [
            DEFAULT_TILE[0].min(width).max(1),
            DEFAULT_TILE[1].min(height).max(1),
        ];
        let element_bytes = if self.half { 8u64 } else { 16u64 };
        let total_slices = u64::from(self.slot_slices.iter().sum::<u32>());
        let budget = DEFAULT_ARENA_BUDGET_BYTES
            .min(caps.max_storage_buffer_binding_size)
            .min(caps.max_buffer_size);
        let halo = self.max_halo;
        let (pitch, rows, arena_bytes) = loop {
            let pitch = tile[0] + 2 * halo;
            let rows = tile[1] + 2 * halo;
            let size = u64::from(pitch) * u64::from(rows) * total_slices * element_bytes;
            if size <= budget {
                break (pitch, rows, size.max(16));
            }
            if tile[1] > 1 {
                tile[1] = tile[1].div_ceil(2);
            } else if tile[0] > 1 {
                tile[0] = tile[0].div_ceil(2);
            } else {
                return Err(PlayerError::Renderer(format!(
                    "SR tile arena requires {size} bytes, exceeding limit {budget}"
                )));
            }
        };

        let mut pool_offsets = [0u32; 256];
        let mut bases = Vec::with_capacity(self.slot_slices.len());
        let mut base = 0u32;
        for &slices in &self.slot_slices {
            bases.push(base);
            base = base
                .checked_add(
                    pitch
                        .checked_mul(rows)
                        .and_then(|v| v.checked_mul(slices))
                        .ok_or_else(|| PlayerError::Renderer("SR arena offset overflow".into()))?,
                )
                .ok_or_else(|| PlayerError::Renderer("SR arena offset overflow".into()))?;
        }
        if let Some(model) = &self.model {
            for layer in &model.layers {
                if self.allocated[layer.output] {
                    pool_offsets[layer.output] = bases[self.slots[layer.output]];
                }
            }
        }

        let (offset, scale) = self
            .model
            .as_ref()
            .map_or((0.0, 1.0), |model| input_normalization(model, range));

        let align = caps.min_uniform_buffer_offset_alignment as usize;
        let stride = PARAMS_BYTES.div_ceil(align) * align;

        let mut params_bytes = Vec::new();
        let mut dispatches = Vec::new();
        let mut tiles = 0u32;

        let mut add_tile = |cx: u32, cy: u32, cw: u32, ch: u32| -> Result<()> {
            tiles += 1;
            for (ki, k) in self.kernels.iter().enumerate() {
                let h = if k.head {
                    0
                } else {
                    self.model
                        .as_ref()
                        .and_then(|m| m.layers.get(k.layer))
                        .and_then(|l| self.halos.get(l.output))
                        .copied()
                        .unwrap_or(0)
                };
                let x = cx.saturating_sub(h);
                let y = cy.saturating_sub(h);
                let right = (cx + cw + h).min(width);
                let bottom = (cy + ch + h).min(height);
                if k.ring && x > 0 && y > 0 && right < width && bottom < height {
                    continue;
                }
                let rect = if self.packed {
                    [x as i32, y as i32, (right - x) as i32, (bottom - y) as i32]
                } else {
                    [0, 0, target[0] as i32, target[1] as i32]
                };

                let mut offsets = [0u32; 256];
                for (local, &actual) in k.bindings.iter().enumerate() {
                    offsets[local] = pool_offsets[actual];
                }

                let param_offset = params_bytes.len();
                if param_offset + stride > caps.max_buffer_size as usize
                    || param_offset > u32::MAX as usize
                {
                    return Err(PlayerError::Renderer(
                        "SR parameter buffer exceeds device limit".into(),
                    ));
                }
                let range_vec = if self.packed {
                    [offset, scale, 1.0 / scale, 0.0]
                } else {
                    [
                        width as f32 / target[0] as f32,
                        height as f32 / target[1] as f32,
                        0.0,
                        fsr1::rcas_strength(fsr1::DEFAULT_SHARPNESS_STOPS),
                    ]
                };
                encode_params(
                    &mut params_bytes,
                    [width, height, target[0], target[1]],
                    [
                        cx as i32 - self.max_halo as i32,
                        cy as i32 - self.max_halo as i32,
                        pitch as i32,
                        rows as i32,
                    ],
                    rect,
                    range_vec,
                    [luma[0], luma[1], luma[2], k.output_start as f32],
                    &offsets,
                    stride,
                );

                let groups = if k.ring {
                    let perimeter = if width < 2 || height < 2 {
                        width * height
                    } else {
                        2 * width + 2 * (height - 2)
                    };
                    let z = self
                        .model
                        .as_ref()
                        .and_then(|m| m.layers.get(k.layer))
                        .map_or(1, |l| l.out_channels.div_ceil(4) as u32);
                    [perimeter.div_ceil(64), 1, z]
                } else {
                    [
                        (rect[2] as u32).div_ceil(k.group[0] * k.pixels),
                        (rect[3] as u32).div_ceil(k.group[1]),
                        1,
                    ]
                };
                dispatches.extend_from_slice(&[
                    ki as u32,
                    param_offset as u32,
                    groups[0],
                    groups[1],
                    groups[2],
                ]);
            }
            Ok(())
        };

        if self.packed {
            for y in (0..height).step_by(tile[1] as usize) {
                for x in (0..width).step_by(tile[0] as usize) {
                    add_tile(x, y, (width - x).min(tile[0]), (height - y).min(tile[1]))?;
                }
            }
        } else {
            add_tile(0, 0, width, height)?;
        }

        Ok(FramePlan {
            output_width: target[0],
            output_height: target[1],
            virtual_width: if self.packed { width * 2 } else { target[0] },
            virtual_height: if self.packed { height * 2 } else { target[1] },
            packed: self.packed,
            arena_bytes,
            tile_width: tile[0],
            tile_height: tile[1],
            tiles,
            params_stride: stride as u32,
            params_bytes,
            dispatches,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_params(
    out: &mut Vec<u8>,
    size: [u32; 4],
    tile: [i32; 4],
    rect: [i32; 4],
    range: [f32; 4],
    luma: [f32; 4],
    offsets: &[u32; 256],
    stride: usize,
) {
    let start = out.len();
    out.reserve(stride);
    for v in size {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in tile {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in rect {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in range {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in luma {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for &v in offsets {
        out.extend_from_slice(&v.to_le_bytes());
    }
    debug_assert_eq!(out.len() - start, PARAMS_BYTES);
    out.resize(start + stride, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::sr::{
        fsr1::{DEFAULT_SHARPNESS_STOPS, easu, rcas, rcas_strength},
        model::{
            Activation, ConvInput, ConvLayer, Head, HeadBase, InputRange, InputTransform, Padding,
            SubpixelOrder, f16_to_f32,
        },
        reference::{run as ref_run, test_pattern},
    };

    #[test]
    fn compiles_and_plans_every_web_mode_for_f32_and_f16() {
        let baseline = DeviceCapabilities::default();
        let f16_caps = DeviceCapabilities {
            shader_f16: true,
            ..baseline
        };
        for caps in [baseline, f16_caps] {
            for mode in WEB_UPSCALER_MODES {
                if matches!(mode, LumaUpscalerMode::Off | LumaUpscalerMode::Custom) {
                    assert!(SrPlan::compile(mode, None, caps).is_err());
                    continue;
                }
                let plan = SrPlan::compile(mode, None, caps)
                    .unwrap_or_else(|e| panic!("failed to compile {}: {e}", mode.label()));
                assert_eq!(plan.half, caps.shader_f16);
                assert!(!plan.kernels.is_empty());
                for kernel in &plan.kernels {
                    assert!(!kernel.weights_half);
                    assert!(
                        !kernel.source.contains("wgpu_cooperative_matrix"),
                        "web kernel must remain portable WGSL"
                    );
                    assert!(
                        !kernel.source.contains("texture_storage_2d<r16float"),
                        "web kernel must target rgba16float storage textures"
                    );
                    assert_eq!(kernel.source.contains("enable f16;"), caps.shader_f16);
                }
                let frame = plan
                    .plan_frame(
                        640,
                        360,
                        1280,
                        720,
                        [0.2126, 0.7152, 0.0722],
                        LumaRange::Full,
                        caps,
                    )
                    .unwrap_or_else(|e| panic!("failed to plan {}: {e}", mode.label()));
                assert!(frame.arena_bytes >= 16);
                assert_eq!(frame.params_stride as usize % 256, 0);
                assert_eq!(frame.dispatches.len() % 5, 0);
                assert_eq!(
                    frame.params_bytes.len(),
                    (frame.dispatches.len() / 5) * frame.params_stride as usize
                );
                if plan.packed {
                    assert_eq!((frame.output_width, frame.output_height), (640, 360));
                    assert_eq!((frame.virtual_width, frame.virtual_height), (1280, 720));
                } else {
                    assert_eq!((frame.output_width, frame.output_height), (1280, 720));
                    assert_eq!((frame.virtual_width, frame.virtual_height), (1280, 720));
                }
            }
        }
    }

    #[test]
    fn rejects_platform_specific_modes_and_parses_labels_and_codes() {
        let caps = DeviceCapabilities::default();
        assert!(SrPlan::compile(LumaUpscalerMode::PlatformVideo, None, caps).is_err());
        assert!(SrPlan::compile(LumaUpscalerMode::MetalFxSpatial, None, caps).is_err());
        assert_eq!(
            LumaUpscalerMode::parse("artcnn_c4f16"),
            Some(LumaUpscalerMode::ArtCnnC4F16)
        );
        assert_eq!(
            LumaUpscalerMode::parse("ANIME4K_M"),
            Some(LumaUpscalerMode::Anime4kM)
        );
        assert_eq!(LumaUpscalerMode::parse("32"), Some(LumaUpscalerMode::Fsr1));
        assert_eq!(LumaUpscalerMode::parse("0"), Some(LumaUpscalerMode::Off));
        assert_eq!(LumaUpscalerMode::parse("unknown"), None);
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn web_plan_executes_on_gpu_and_matches_cpu_oracle() {
        let instance = wgpu::Instance::default();
        let Some(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            }))
            .ok()
        else {
            return;
        };
        let has_f16 = adapter.features().contains(wgpu::Features::SHADER_F16);
        let required_features = if has_f16 {
            wgpu::Features::SHADER_F16
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("web-sr-plan-test"),
                required_features,
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits())
                    .using_alignment(adapter.limits()),
                ..Default::default()
            }),
        )
        .unwrap();

        let limits = device.limits();
        // Exercise strict WebGPU 16 KiB workgroup storage limits on both f32 and f16.
        let mut cap_variants = vec![DeviceCapabilities {
            shader_f16: false,
            max_compute_workgroup_storage_size: limits
                .max_compute_workgroup_storage_size
                .min(16384),
            max_uniform_buffer_binding_size: limits.max_uniform_buffer_binding_size,
            max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size as u64,
            max_buffer_size: limits.max_buffer_size,
            max_compute_workgroup_size_x: limits.max_compute_workgroup_size_x,
            max_compute_workgroup_size_y: limits.max_compute_workgroup_size_y,
            max_compute_invocations_per_workgroup: limits.max_compute_invocations_per_workgroup,
            max_texture_dimension_2d: limits.max_texture_dimension_2d,
            min_uniform_buffer_offset_alignment: limits.min_uniform_buffer_offset_alignment,
            max_storage_buffers_per_shader_stage: limits.max_storage_buffers_per_shader_stage,
        }];
        if has_f16 {
            cap_variants.push(DeviceCapabilities {
                shader_f16: true,
                ..cap_variants[0]
            });
        }

        let width = 32u32;
        let height = 24u32;
        let target = [64u32, 48u32];
        let luma_in = test_pattern(width as usize, height as usize, 11);
        let mut rgba_bytes = Vec::with_capacity((width * height * 4) as usize);
        let mut quantized_luma = Vec::with_capacity((width * height) as usize);
        for &v in &luma_in {
            let b = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            rgba_bytes.extend_from_slice(&[b, b, b, 255]);
            quantized_luma.push(f32::from(b) / 255.0);
        }
        let source_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("web-sr-test-source"),
            size: wgpu::Extent3d {
                width,
                height,
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
            source_tex.as_image_copy(),
            &rgba_bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            source_tex.size(),
        );
        let source_view = source_tex.create_view(&Default::default());

        let custom = Arc::new(SrModel {
            name: "custom-identity".into(),
            padding: Padding::Clamp,
            input_range: InputRange::Full,
            tensors: vec![1, 4],
            layers: vec![ConvLayer {
                output: 1,
                out_channels: 4,
                inputs: vec![ConvInput {
                    tensor: 0,
                    transform: InputTransform::Identity,
                    kernel: 1,
                    weights: vec![0.0; 4],
                }],
                bias: vec![0.0; 4],
                residual: None,
                activation: Activation::None,
                border: None,
            }],
            head: Head {
                tensor: 1,
                order: SubpixelOrder::RowMajor,
                base: HeadBase::Bilinear,
                clamp: true,
            },
        });

        let modes = [
            LumaUpscalerMode::Fsr1,
            LumaUpscalerMode::ArtCnnC4F16,
            LumaUpscalerMode::ArtCnnC4F32,
            LumaUpscalerMode::Anime4kS,
            LumaUpscalerMode::Anime4kL,
            LumaUpscalerMode::Anime4kDenoiseS,
            LumaUpscalerMode::Custom,
        ];

        for caps in cap_variants {
            for mode in modes {
                let plan = SrPlan::compile(
                    mode,
                    (mode == LumaUpscalerMode::Custom).then(|| custom.clone()),
                    caps,
                )
                .unwrap();
                let frame = plan
                    .plan_frame(
                        width,
                        height,
                        target[0],
                        target[1],
                        [0.2126, 0.7152, 0.0722],
                        LumaRange::Full,
                        caps,
                    )
                    .unwrap();

                let arena = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("web-sr-arena"),
                    size: frame.arena_bytes,
                    usage: wgpu::BufferUsages::STORAGE,
                    mapped_at_creation: false,
                });
                let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("web-sr-params"),
                    size: frame.params_bytes.len() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                queue.write_buffer(&params_buf, 0, &frame.params_bytes);

                let dest_tex = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("web-sr-dest"),
                    size: wgpu::Extent3d {
                        width: frame.output_width,
                        height: frame.output_height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba16Float,
                    usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let dest_view = dest_tex.create_view(&Default::default());

                let mut pipelines = Vec::new();
                let mut bind_groups = Vec::new();
                for k in &plan.kernels {
                    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: None,
                        entries: &[
                            wgpu::BindGroupLayoutEntry {
                                binding: 0,
                                visibility: wgpu::ShaderStages::COMPUTE,
                                ty: wgpu::BindingType::Buffer {
                                    ty: wgpu::BufferBindingType::Uniform,
                                    has_dynamic_offset: true,
                                    min_binding_size: std::num::NonZeroU64::new(
                                        PARAMS_BYTES as u64,
                                    ),
                                },
                                count: None,
                            },
                            wgpu::BindGroupLayoutEntry {
                                binding: 1,
                                visibility: wgpu::ShaderStages::COMPUTE,
                                ty: wgpu::BindingType::Buffer {
                                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                                    has_dynamic_offset: false,
                                    min_binding_size: None,
                                },
                                count: None,
                            },
                            wgpu::BindGroupLayoutEntry {
                                binding: 2,
                                visibility: wgpu::ShaderStages::COMPUTE,
                                ty: wgpu::BindingType::Buffer {
                                    ty: if k.uniform {
                                        wgpu::BufferBindingType::Uniform
                                    } else {
                                        wgpu::BufferBindingType::Storage { read_only: true }
                                    },
                                    has_dynamic_offset: false,
                                    min_binding_size: None,
                                },
                                count: None,
                            },
                            wgpu::BindGroupLayoutEntry {
                                binding: 3,
                                visibility: wgpu::ShaderStages::COMPUTE,
                                ty: wgpu::BindingType::Texture {
                                    sample_type: wgpu::TextureSampleType::Float {
                                        filterable: false,
                                    },
                                    view_dimension: wgpu::TextureViewDimension::D2,
                                    multisampled: false,
                                },
                                count: None,
                            },
                            wgpu::BindGroupLayoutEntry {
                                binding: 4,
                                visibility: wgpu::ShaderStages::COMPUTE,
                                ty: wgpu::BindingType::StorageTexture {
                                    access: wgpu::StorageTextureAccess::WriteOnly,
                                    format: wgpu::TextureFormat::Rgba16Float,
                                    view_dimension: wgpu::TextureViewDimension::D2,
                                },
                                count: None,
                            },
                        ],
                    });
                    let mut weight_bytes = Vec::with_capacity(k.weights.len().max(1) * 16);
                    if k.weights.is_empty() {
                        weight_bytes.resize(16, 0);
                    } else {
                        for v in &k.weights {
                            for f in v {
                                weight_bytes.extend_from_slice(&f.to_le_bytes());
                            }
                        }
                    }
                    let weight_buf = device.create_buffer(&wgpu::BufferDescriptor {
                        label: None,
                        size: weight_bytes.len() as u64,
                        usage: if k.uniform {
                            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST
                        } else {
                            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST
                        },
                        mapped_at_creation: false,
                    });
                    queue.write_buffer(&weight_buf, 0, &weight_bytes);

                    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("web-sr-test-shader"),
                        source: wgpu::ShaderSource::Wgsl(k.source.as_str().into()),
                    });
                    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: None,
                        bind_group_layouts: &[Some(&bgl)],
                        immediate_size: 0,
                    });
                    let pipe = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        label: None,
                        layout: Some(&pl),
                        module: &module,
                        entry_point: Some("main"),
                        compilation_options: Default::default(),
                        cache: None,
                    });
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: None,
                        layout: &bgl,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer: &params_buf,
                                    offset: 0,
                                    size: std::num::NonZeroU64::new(PARAMS_BYTES as u64),
                                }),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: arena.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: weight_buf.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::TextureView(&source_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: wgpu::BindingResource::TextureView(&dest_view),
                            },
                        ],
                    });
                    pipelines.push(pipe);
                    bind_groups.push(bg);
                }

                let mut encoder = device.create_command_encoder(&Default::default());
                {
                    let mut pass = encoder.begin_compute_pass(&Default::default());
                    for d in frame.dispatches.chunks_exact(5) {
                        let ki = d[0] as usize;
                        pass.set_pipeline(&pipelines[ki]);
                        pass.set_bind_group(0, &bind_groups[ki], &[d[1]]);
                        pass.dispatch_workgroups(d[2], d[3], d[4]);
                    }
                }
                let row_bytes = (frame.output_width * 8).div_ceil(256) * 256;
                let readback = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: u64::from(row_bytes) * u64::from(frame.output_height),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                encoder.copy_texture_to_buffer(
                    dest_tex.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &readback,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(row_bytes),
                            rows_per_image: Some(frame.output_height),
                        },
                    },
                    dest_tex.size(),
                );
                queue.submit([encoder.finish()]);

                let (tx, rx) = std::sync::mpsc::channel();
                readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                    let _ = tx.send(r);
                });
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
                rx.recv().unwrap().unwrap();
                let mapped = readback.slice(..).get_mapped_range();
                let mut got = vec![0.0f32; (frame.virtual_width * frame.virtual_height) as usize];
                for y in 0..frame.output_height as usize {
                    let row = &mapped[y * row_bytes as usize..];
                    for x in 0..frame.output_width as usize {
                        let base = x * 8;
                        let px = |c: usize| {
                            f16_to_f32(u16::from_le_bytes([
                                row[base + c * 2],
                                row[base + c * 2 + 1],
                            ]))
                        };
                        if frame.packed {
                            let vw = frame.virtual_width as usize;
                            got[(2 * y) * vw + 2 * x] = px(0);
                            got[(2 * y) * vw + 2 * x + 1] = px(1);
                            got[(2 * y + 1) * vw + 2 * x] = px(2);
                            got[(2 * y + 1) * vw + 2 * x + 1] = px(3);
                        } else {
                            got[y * frame.output_width as usize + x] = px(0);
                        }
                    }
                }
                drop(mapped);
                readback.unmap();

                let expected = if mode == LumaUpscalerMode::Fsr1 {
                    let e = easu(
                        &quantized_luma,
                        width as usize,
                        height as usize,
                        target[0] as usize,
                        target[1] as usize,
                    );
                    rcas(
                        &e,
                        target[0] as usize,
                        target[1] as usize,
                        rcas_strength(DEFAULT_SHARPNESS_STOPS),
                    )
                } else {
                    let model = if mode == LumaUpscalerMode::Custom {
                        custom.clone()
                    } else {
                        catalog::source_model(mode).unwrap()
                    };
                    ref_run(
                        &model,
                        &quantized_luma,
                        width as usize,
                        height as usize,
                        LumaRange::Full,
                    )
                };

                let max_err = got
                    .iter()
                    .zip(&expected)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0f32, f32::max);
                let tol = if caps.shader_f16 { 5e-3 } else { 1.5e-3 };
                assert!(
                    max_err < tol,
                    "mode={} f16={} max_err={max_err} >= {tol}",
                    mode.label(),
                    caps.shader_f16
                );
            }
        }
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn webgpu_renderer_presentation_wgsl_compiles_on_gpu() {
        let instance = wgpu::Instance::default();
        let Some(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            }))
            .ok()
        else {
            return;
        };
        let (device, _queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("webgpu-presentation-wgsl-test"),
                ..Default::default()
            }))
            .unwrap();

        let js = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../packages/erika_web/renderer_webgpu.js"),
        )
        .expect("read renderer_webgpu.js");

        let extract = |name: &str| -> String {
            let marker = format!("const {name} = /* wgsl */ `");
            let start = js
                .find(&marker)
                .unwrap_or_else(|| panic!("missing {name} in renderer_webgpu.js"))
                + marker.len();
            let end = js[start..]
                .find("`;")
                .unwrap_or_else(|| panic!("unterminated {name}"))
                + start;
            js[start..end].to_string()
        };

        let format = wgpu::TextureFormat::Bgra8Unorm;
        let frame_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);

        // 1. VIDEO_WGSL
        {
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("VIDEO_WGSL"),
                source: wgpu::ShaderSource::Wgsl(extract("VIDEO_WGSL").into()),
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let _ = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("VIDEO_WGSL"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            });
        }

        // 2. SUBTITLE_WGSL
        {
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("SUBTITLE_WGSL"),
                source: wgpu::ShaderSource::Wgsl(extract("SUBTITLE_WGSL").into()),
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&frame_bgl), Some(&bgl)],
                immediate_size: 0,
            });
            let _ = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("SUBTITLE_WGSL"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            });
        }

        // 3. DANMAKU_WGSL
        {
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("DANMAKU_WGSL"),
                source: wgpu::ShaderSource::Wgsl(extract("DANMAKU_WGSL").into()),
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&frame_bgl), Some(&bgl)],
                immediate_size: 0,
            });
            let _ = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("DANMAKU_WGSL"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: 80,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![
                            0 => Float32x4,
                            1 => Float32x4,
                            2 => Float32x4,
                            3 => Float32x4,
                            4 => Uint32x2
                        ],
                    }],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            });
        }

        // 4. PICTURE_WGSL
        {
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("PICTURE_WGSL"),
                source: wgpu::ShaderSource::Wgsl(extract("PICTURE_WGSL").into()),
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&frame_bgl), Some(&bgl)],
                immediate_size: 0,
            });
            let _ = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("PICTURE_WGSL"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: 36,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![
                            0 => Float32x4,
                            1 => Float32x4,
                            2 => Float32
                        ],
                    }],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            });
        }

        let err = pollster::block_on(scope.pop());
        assert!(err.is_none(), "WebGPU presentation WGSL error: {err:?}");
    }
}
