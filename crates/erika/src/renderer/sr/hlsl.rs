//! HLSL (`cs_5_0`) code generation for the D3D11 network engine.
//!
//! Every convolution layer becomes one compute kernel per output-channel
//! block. The generated code is fully unrolled straight-line code whose
//! weight operands are constant-buffer reads at compile-time indices, so
//! drivers turn them into FMA operands (NVIDIA `c[bank][offset]`, AMD scalar
//! registers) instead of per-thread loads. Inputs with spatial kernels are
//! staged once per thread group in `groupshared` memory, element-wise input
//! transforms (CReLU halves) are applied on read, and bias, residual,
//! activation and the depth-to-space head are fused into the epilogue.
//!
//! Weights are not part of the source: models with the same shapes (for
//! example ArtCNN C4F16, DS and DN) share compiled kernels.

use std::fmt::Write as _;

mod head;

use super::model::{
    Activation, ConvLayer, HeadBase, INPUT_TENSOR, InputTransform, Padding, SrModel, SubpixelOrder,
};

/// Group shared memory budget per thread group. `cs_5_0` allows 32 KiB; a
/// little headroom keeps two groups resident per SM on more hardware.
const LDS_BUDGET_BYTES: usize = 28 * 1024;
/// D3D11 constant buffers hold at most 4096 `float4`.
const MAX_CONSTANTS: usize = 4096;
/// Output channels a single kernel may accumulate. 16 keeps the unrolled
/// kernel (taps x inputs x 16 FMAs) inside the instruction cache; 32-wide
/// blocks were 35% slower for ArtCNN C4F32 on an RTX 5060 Ti.
const MAX_OUTPUT_BLOCK: usize = 16;

/// Execution parameters chosen per kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KernelShape {
    pub group_x: u32,
    pub group_y: u32,
    /// Output pixels per thread along x and y (register blocking).
    pub pixels_x: u32,
    pub pixels_y: u32,
}

impl KernelShape {
    /// Output pixels covered by one thread group.
    pub fn block(self) -> (u32, u32) {
        (self.group_x * self.pixels_x, self.group_y * self.pixels_y)
    }
}

/// One input tensor as the kernel sees it: staged in LDS (`radius > 0`) or
/// read directly at the output pixel (`radius == 0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelSource {
    pub tensor: usize,
    pub slices: usize,
    pub radius: usize,
}

/// A compiled unit: one layer, one block of output channels.
#[derive(Debug, Clone, PartialEq)]
pub struct KernelSpec {
    pub layer: usize,
    pub out_start: usize,
    pub out_count: usize,
    pub dense: Option<DenseContribution>,
    pub partial_head: Option<PartialHead>,
    pub shape: KernelShape,
    /// SRV slot `t{i + 1}` binds `sources[i]`; `t0` is always the luma.
    pub sources: Vec<KernelSource>,
    /// Residual tensor bound at `t{sources.len() + 1}`.
    pub residual: Option<usize>,
    /// Writes the 2x output instead of the layer's tensor.
    pub head: bool,
    pub source_code: String,
    /// Constant buffer `b1` contents for the interior weights.
    pub constants: Vec<[f32; 4]>,
    /// Exact re-evaluation of the one-pixel image ring for layers with
    /// border variants.
    pub ring: Option<RingKernel>,
}

/// A producer's contribution to a running dense 1x1 head sum.
#[derive(Debug, Clone, PartialEq)]
pub struct DenseContribution {
    pub step: usize,
    pub last: bool,
    pub constants: Vec<[f32; 4]>,
}

/// One input-channel block of a spatial head. Only the final block writes D2S.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialHead {
    pub step: usize,
    pub last: bool,
    pub first_slice: usize,
    pub macs: usize,
}

/// One dispatch over the image ring: threads map to perimeter pixels, read
/// their inputs directly, and pick one of the nine border-class weight sets
/// from a buffer (bound at `t15`).
#[derive(Debug, Clone, PartialEq)]
pub struct RingKernel {
    pub source_code: String,
    /// Nine variants of `variant_stride` float4 each.
    pub weights: Vec<[f32; 4]>,
    pub variant_stride: usize,
}

/// Threads per ring-kernel group.
pub const RING_GROUP: u32 = 64;

/// Pixels on the one-pixel ring of a `width x height` image.
pub fn ring_pixels(width: u32, height: u32) -> u32 {
    if width < 2 || height < 2 {
        width * height
    } else {
        2 * width + 2 * (height - 2)
    }
}

/// Everything the D3D11 executor needs for one model.
#[derive(Debug, Clone, PartialEq)]
pub struct D3d11Plan {
    pub halos: Vec<usize>,
    pub max_halo: usize,
    /// Texture slot per tensor (`None` for the input and a fused head).
    pub tensor_slot: Vec<Option<usize>>,
    /// Array slices of each slot texture.
    pub slot_slices: Vec<usize>,
    pub kernels: Vec<KernelSpec>,
    pub border_layers: Vec<bool>,
    /// Measured cache-sized tiling for narrow feature graphs.
    pub tile_budget_bytes: Option<usize>,
}

fn slices(channels: usize) -> usize {
    channels.div_ceil(4)
}

/// Builds the execution plan and kernels for `model`.
pub fn plan(model: &SrModel) -> D3d11Plan {
    plan_for_adapter(model, false)
}

/// Intel benefits from loading spatial-head windows one row at a time.
pub(crate) fn plan_for_adapter(model: &SrModel, intel: bool) -> D3d11Plan {
    let head_prefetch = tuning_override("ERIKA_SR_HEAD_PREFETCH").unwrap_or(usize::from(intel));
    let stream = tuning_override("ERIKA_SR_STREAM_HEAD").unwrap_or(1) != 0 && streamable(model);
    let halos = model.tensor_halos();
    let max_halo = halos.iter().copied().max().unwrap_or(0);

    // The head layer writes the 2x output directly when the head tensor has
    // no other reader.
    let head_readers = model
        .layers
        .iter()
        .filter(|layer| {
            layer
                .inputs
                .iter()
                .any(|input| input.tensor == model.head.tensor)
                || layer.residual == Some(model.head.tensor)
        })
        .count();
    let fused_head = head_readers == 0;

    // Last layer index reading each tensor, for slot reuse.
    let mut last_use = vec![0usize; model.tensors.len()];
    for (index, layer) in model.layers.iter().enumerate() {
        if stream && layer.output == model.head.tensor {
            continue;
        }
        for input in &layer.inputs {
            last_use[input.tensor] = index;
        }
        if let Some(residual) = layer.residual {
            last_use[residual] = index;
        }
    }
    let mut tensor_slot = vec![None; model.tensors.len()];
    let mut slot_slices: Vec<usize> = Vec::new();
    let mut slot_free_after: Vec<usize> = Vec::new();
    for (index, layer) in model.layers.iter().enumerate() {
        if fused_head
            && (layer.output == model.head.tensor || (stream && index + 2 == model.layers.len()))
        {
            continue;
        }
        let needed = slices(layer.out_channels);
        // A slot is free once its tensor's last reader ran before this layer.
        let reusable = (0..slot_slices.len()).find(|&slot| slot_free_after[slot] < index);
        let slot = match reusable {
            Some(slot) => {
                slot_slices[slot] = slot_slices[slot].max(needed);
                slot
            }
            None => {
                slot_slices.push(needed);
                slot_free_after.push(0);
                slot_slices.len() - 1
            }
        };
        slot_free_after[slot] = last_use[layer.output].max(index);
        tensor_slot[layer.output] = Some(slot);
    }

    let mut kernels = Vec::new();
    let mut border_layers = Vec::new();
    let mut dense_step = 0;
    for (index, layer) in model.layers.iter().enumerate() {
        if stream && layer.output == model.head.tensor {
            continue;
        }
        let head = fused_head && layer.output == model.head.tensor;
        border_layers.push(layer.border.is_some());
        for (out_start, out_count) in output_blocks(model, layer) {
            let mut kernel = build_kernel(
                model,
                index,
                layer,
                out_start,
                out_count,
                head,
                head_prefetch,
            );
            if stream
                && model.layers.last().is_some_and(|head| {
                    head.inputs.iter().any(|input| input.tensor == layer.output)
                })
            {
                let last = index + 2 == model.layers.len();
                let dense = dense_contribution(model, layer, dense_step, last);
                dense_step += 1;
                kernel.source_code = emit(
                    model,
                    layer,
                    out_count,
                    false,
                    kernel.shape,
                    &kernel.sources,
                    Some(&dense),
                    head_prefetch,
                );
                kernel.head = last;
                kernel.dense = Some(dense);
            }
            kernels.push(kernel);
        }
    }
    let head_block = tuning_override("ERIKA_SR_HEAD_ICB").unwrap_or(16);
    if head_block >= 4 && head_block % 4 == 0 {
        head::split(model, &mut kernels, head_block, head_prefetch);
    }
    let widest = model.tensors.iter().copied().max().unwrap_or(1);
    let tile_budget_bytes = if stream && widest <= 8 {
        Some(24 << 20)
    } else if !stream && model.padding == Padding::Clamp && widest == 8 {
        Some(48 << 20)
    } else {
        None
    };
    D3d11Plan {
        halos,
        max_halo,
        tensor_slot,
        slot_slices,
        kernels,
        border_layers,
        tile_budget_bytes,
    }
}

fn streamable(model: &SrModel) -> bool {
    let Some(head) = model.layers.last() else {
        return false;
    };
    head.output == model.head.tensor
        && model.layers.len() > 1
        && head
            .inputs
            .iter()
            .any(|input| input.tensor == model.layers[model.layers.len() - 2].output)
        && head.activation == Activation::None
        && head.border.is_none()
        && head.residual.is_none()
        && head
            .inputs
            .iter()
            .all(|input| input.kernel == 1 && input.tensor != INPUT_TENSOR)
        && model.layers[..model.layers.len() - 1]
            .iter()
            .enumerate()
            .all(|(index, layer)| {
                layer.border.is_none()
                    && layer.residual.is_none()
                    && matches!(layer.activation, Activation::None | Activation::Relu)
                    && output_blocks(model, layer).len() == 1
                    && layer.inputs.iter().all(|input| {
                        input.tensor
                            == if index == 0 {
                                INPUT_TENSOR
                            } else {
                                model.layers[index - 1].output
                            }
                    })
            })
}

fn dense_contribution(
    model: &SrModel,
    layer: &ConvLayer,
    step: usize,
    last: bool,
) -> DenseContribution {
    let head = model.layers.last().expect("dense head");
    let mut constants = Vec::new();
    for input in head
        .inputs
        .iter()
        .filter(|input| input.tensor == layer.output)
    {
        for c in 0..layer.out_channels {
            constants.push(std::array::from_fn(|o| {
                input.weights[o * layer.out_channels + c]
            }));
        }
    }
    constants.push(std::array::from_fn(|o| head.bias[o]));
    DenseContribution {
        step,
        last,
        constants,
    }
}

/// Splits a layer's outputs into blocks whose weights fit one constant
/// buffer and whose accumulators stay in registers.
fn output_blocks(model: &SrModel, layer: &ConvLayer) -> Vec<(usize, usize)> {
    let per_output: usize = layer
        .inputs
        .iter()
        .map(|input| slices(model.tensors[input.tensor]) * 4 * input.kernel * input.kernel)
        .sum();
    let total = layer.out_channels;
    let default_block = if layer
        .inputs
        .iter()
        .any(|input| model.tensors[input.tensor] >= 48)
    {
        8
    } else {
        MAX_OUTPUT_BLOCK
    };
    let max_block =
        tuning_override("ERIKA_SR_OCB").map_or(default_block, |block| block.clamp(4, 64) / 4 * 4);
    let mut block = total.min(max_block);
    // Constants per block: weights (block/4 float4 per input scalar) + bias +
    // PReLU slopes.
    while block > 4 && per_output * slices(block) + 2 * slices(block) > MAX_CONSTANTS {
        block -= 4;
    }
    // Balance the blocks so no dispatch runs nearly empty.
    let count = total.div_ceil(block);
    let even = total.div_ceil(count).div_ceil(4) * 4;
    let mut blocks = Vec::new();
    let mut start = 0;
    while start < total {
        let size = even.min(total - start);
        blocks.push((start, size));
        start += size;
    }
    blocks
}

/// Constant buffer layout for one kernel: per input `ConvInput`, per input
/// channel `c`, per tap `t`, per output quad `q` one `float4` holding the
/// weights of outputs `4q..4q+4`; then bias quads; then PReLU slope quads.
struct ConstantLayout {
    input_base: Vec<usize>,
    quads: usize,
    bias: usize,
    alpha: usize,
    total: usize,
}

fn crelu_pair(layer: &ConvLayer, index: usize) -> Option<usize> {
    let input = &layer.inputs[index];
    if input.transform == InputTransform::Identity {
        return None;
    }
    let peers = layer
        .inputs
        .iter()
        .enumerate()
        .filter(|(_, other)| {
            other.tensor == input.tensor
                && other.kernel == input.kernel
                && other.transform != InputTransform::Identity
        })
        .collect::<Vec<_>>();
    if peers.len() != 2 {
        return None;
    }
    peers
        .into_iter()
        .find(|(_, other)| other.transform != input.transform)
        .map(|(index, _)| index)
}

fn constant_layout(model: &SrModel, layer: &ConvLayer, out_count: usize) -> ConstantLayout {
    let quads = slices(out_count);
    let mut input_base = Vec::new();
    let mut cursor = 0;
    for input in &layer.inputs {
        input_base.push(cursor);
        cursor += slices(model.tensors[input.tensor]) * 4 * input.kernel * input.kernel * quads;
    }
    let bias = cursor;
    cursor += quads;
    let alpha = cursor;
    if matches!(layer.activation, Activation::Prelu(_)) {
        cursor += quads;
    }
    ConstantLayout {
        input_base,
        quads,
        bias,
        alpha,
        total: cursor,
    }
}

fn pack_constants(
    model: &SrModel,
    layer: &ConvLayer,
    out_start: usize,
    out_count: usize,
    inputs: &[&Vec<f32>],
    bias: &[f32],
) -> Vec<[f32; 4]> {
    let layout = constant_layout(model, layer, out_count);
    let mut out = vec![[0.0f32; 4]; layout.total];
    let abs = tuning_override("ERIKA_SR_CRELU_ABS").unwrap_or(1) != 0;
    for (index, input) in layer.inputs.iter().enumerate() {
        let pair = if abs { crelu_pair(layer, index) } else { None };
        let channels = model.tensors[input.tensor];
        let taps = input.kernel * input.kernel;
        let weights = inputs[index];
        for c in 0..channels {
            for tap in 0..taps {
                for q in 0..layout.quads {
                    let slot =
                        &mut out[layout.input_base[index] + (c * taps + tap) * layout.quads + q];
                    for lane in 0..4 {
                        let o = q * 4 + lane;
                        if o < out_count {
                            let at = ((out_start + o) * channels + c) * taps + tap;
                            slot[lane] = if let Some(peer) = pair {
                                let (positive, negative) =
                                    if input.transform == InputTransform::Relu {
                                        (weights[at], inputs[peer][at])
                                    } else {
                                        (inputs[peer][at], weights[at])
                                    };
                                0.5 * if input.transform == InputTransform::Relu {
                                    positive - negative
                                } else {
                                    positive + negative
                                }
                            } else {
                                weights[at]
                            };
                        }
                    }
                }
            }
        }
    }
    for q in 0..layout.quads {
        for lane in 0..4 {
            let o = q * 4 + lane;
            if o < out_count {
                out[layout.bias + q][lane] = bias[out_start + o];
                if let Activation::Prelu(alpha) = &layer.activation {
                    out[layout.alpha + q][lane] = alpha[out_start + o];
                }
            }
        }
    }
    out
}

/// Tuning overrides for benchmarking (`ERIKA_SR_PX=2`, `ERIKA_SR_PY=1`,
/// `ERIKA_SR_OCB=16`, `ERIKA_SR_GROUP=16x8`).
fn tuning_override(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.parse().ok()
}

fn choose_shape(sources: &[KernelSource]) -> KernelShape {
    let lds_bytes = |bw: usize, bh: usize| -> usize {
        sources
            .iter()
            .filter(|source| source.radius > 0)
            .map(|source| {
                let w = bw + 2 * source.radius;
                let h = bh + 2 * source.radius;
                // Luma is staged as scalars.
                let lanes = if source.tensor == INPUT_TENSOR {
                    1
                } else {
                    source.slices * 4
                };
                w * h * lanes * 4
            })
            .sum()
    };
    // Register blocking: every constant-buffer weight feeds `px * py`
    // pixels, as long as the accumulators stay within ~64 registers.
    let (px, py) = match (
        tuning_override("ERIKA_SR_PX"),
        tuning_override("ERIKA_SR_PY"),
    ) {
        (Some(px), py) => (px.clamp(1, 4), py.unwrap_or(1).clamp(1, 4)),
        (None, Some(py)) => (1, py.clamp(1, 4)),
        // Measured on an RTX 5060 Ti: blocking several pixels per thread
        // doubles the straight-line code and overflows the instruction
        // cache, which costs far more than the extra constant reads it saves.
        (None, None) => (1, 1),
    };
    let groups: Vec<(usize, usize)> = match std::env::var("ERIKA_SR_GROUP").ok().and_then(|value| {
        let (x, y) = value.split_once('x')?;
        Some((x.parse().ok()?, y.parse().ok()?))
    }) {
        Some(group) => vec![group],
        None => vec![(16, 16), (16, 8), (8, 8), (8, 4), (4, 4)],
    };
    for &(gx, gy) in &groups {
        if lds_bytes(gx * px, gy * py) <= LDS_BUDGET_BYTES {
            return KernelShape {
                group_x: gx as u32,
                group_y: gy as u32,
                pixels_x: px as u32,
                pixels_y: py as u32,
            };
        }
    }
    // Very wide inputs: stage in several phases (see `stage_phases`).
    let (gx, gy) = *groups.last().expect("non-empty");
    KernelShape {
        group_x: gx.min(8) as u32,
        group_y: gy.min(8) as u32,
        pixels_x: 1,
        pixels_y: 1,
    }
}

fn build_kernel(
    model: &SrModel,
    layer_index: usize,
    layer: &ConvLayer,
    out_start: usize,
    out_count: usize,
    head: bool,
    head_prefetch: usize,
) -> KernelSpec {
    // Distinct tensors read, with the largest radius each needs.
    let mut sources: Vec<KernelSource> = Vec::new();
    for input in &layer.inputs {
        let radius = input.kernel / 2;
        match sources
            .iter_mut()
            .find(|source| source.tensor == input.tensor)
        {
            Some(source) => source.radius = source.radius.max(radius),
            None => sources.push(KernelSource {
                tensor: input.tensor,
                slices: slices(model.tensors[input.tensor]),
                radius,
            }),
        }
    }
    let shape = choose_shape(&sources);
    let interior: Vec<&Vec<f32>> = layer.inputs.iter().map(|input| &input.weights).collect();
    let constants = pack_constants(model, layer, out_start, out_count, &interior, &layer.bias);
    let ring = layer.border.as_ref().map(|variants| {
        let variant_stride = constants.len();
        let weights = variants
            .iter()
            .flat_map(|variant| {
                let weights: Vec<&Vec<f32>> = variant.inputs.iter().collect();
                pack_constants(model, layer, out_start, out_count, &weights, &variant.bias)
            })
            .collect();
        RingKernel {
            source_code: emit_ring(model, layer, out_count, head, &sources, variant_stride),
            weights,
            variant_stride,
        }
    });
    let source_code = emit(
        model,
        layer,
        out_count,
        head,
        shape,
        &sources,
        None,
        head_prefetch,
    );
    KernelSpec {
        layer: layer_index,
        out_start,
        out_count,
        dense: None,
        partial_head: None,
        shape,
        sources,
        residual: layer.residual,
        head,
        source_code,
        constants,
        ring,
    }
}

/// Phases of LDS staging: each phase stages a set of (source, slice range)
/// that fits the budget, then accumulates the inputs reading them.
struct StagePhase {
    /// (source index, first slice, slice count).
    parts: Vec<(usize, usize, usize)>,
}

fn stage_phases(sources: &[KernelSource], block: (usize, usize)) -> Vec<StagePhase> {
    let mut phases = vec![StagePhase { parts: Vec::new() }];
    let mut used = 0usize;
    for (index, source) in sources.iter().enumerate() {
        if source.radius == 0 {
            continue;
        }
        let area = (block.0 + 2 * source.radius) * (block.1 + 2 * source.radius);
        let per_slice = if source.tensor == INPUT_TENSOR {
            area * 4
        } else {
            area * 16
        };
        let mut start = 0;
        while start < source.slices {
            let fit = LDS_BUDGET_BYTES.saturating_sub(used) / per_slice;
            if fit == 0 {
                phases.push(StagePhase { parts: Vec::new() });
                used = 0;
                continue;
            }
            let count = fit.min(source.slices - start);
            phases
                .last_mut()
                .expect("phase exists")
                .parts
                .push((index, start, count));
            used += count * per_slice;
            start += count;
        }
    }
    phases.retain(|phase| !phase.parts.is_empty());
    phases
}

const LANES: [&str; 4] = ["x", "y", "z", "w"];

/// Emits the multiply-accumulates of one `ConvInput` for one slice of its
/// tensor. `cell(wy, wx)` names the value at window position `(wy, wx)`
/// relative to the thread's first pixel minus the input radius.
#[allow(clippy::too_many_arguments)]
fn emit_accumulate(
    w: &mut String,
    model: &SrModel,
    layer: &ConvLayer,
    layout: &ConstantLayout,
    input_index: usize,
    slice: usize,
    pixels: (usize, usize),
    cell: &dyn Fn(usize, usize) -> String,
    weight: &dyn Fn(usize) -> String,
    rows: Option<(usize, usize)>,
) {
    let input = &layer.inputs[input_index];
    let abs = tuning_override("ERIKA_SR_CRELU_ABS").unwrap_or(1) != 0
        && crelu_pair(layer, input_index).is_some();
    let k = input.kernel;
    let taps = k * k;
    let channels = model.tensors[input.tensor];
    let scalar = input.tensor == INPUT_TENSOR;
    let (row_start, row_end) = rows.unwrap_or((0, k));
    for ky in row_start..row_end {
        for kx in 0..k {
            let tap = ky * k + kx;
            // Transformed operand per pixel of the thread's block.
            let mut values = Vec::new();
            for py in 0..pixels.1 {
                for px in 0..pixels.0 {
                    let raw = cell(ky + py, kx + px);
                    values.push(if abs && input.transform == InputTransform::Relu {
                        raw
                    } else if abs {
                        format!("abs({raw})")
                    } else {
                        match input.transform {
                            InputTransform::Identity => raw,
                            InputTransform::Relu => format!("max({raw}, 0.0)"),
                            InputTransform::NegRelu => format!("max(-{raw}, 0.0)"),
                        }
                    });
                }
            }
            let ty = if scalar { "float" } else { "float4" };
            let _ = write!(w, "      {{");
            for (p, value) in values.iter().enumerate() {
                let _ = write!(w, " {ty} v{p} = {value};");
            }
            let _ = writeln!(w);
            for lane in 0..if scalar { 1 } else { 4 } {
                let c = slice * 4 + lane;
                if c >= channels {
                    continue;
                }
                for q in 0..layout.quads {
                    let index =
                        layout.input_base[input_index] + (c * taps + tap) * layout.quads + q;
                    for p in 0..values.len() {
                        let component = if scalar {
                            format!("v{p}")
                        } else {
                            format!("v{p}.{}", LANES[lane])
                        };
                        let _ = writeln!(
                            w,
                            "        a{p}_{q} = mad({}, {component}, a{p}_{q});",
                            weight(index)
                        );
                    }
                }
            }
            let _ = writeln!(w, "      }}");
        }
    }
}

fn emit(
    model: &SrModel,
    layer: &ConvLayer,
    out_count: usize,
    head: bool,
    shape: KernelShape,
    sources: &[KernelSource],
    dense: Option<&DenseContribution>,
    head_prefetch: usize,
) -> String {
    let layout = constant_layout(model, layer, out_count);
    let quads = layout.quads;
    let (gx, gy) = (shape.group_x as usize, shape.group_y as usize);
    let (px, py) = (shape.pixels_x as usize, shape.pixels_y as usize);
    let pixel_count = px * py;
    let block = (gx * px, gy * py);
    let clamp = model.padding == Padding::Clamp;
    let phases = stage_phases(sources, block);
    let constant = |index: usize| format!("W[{index}]");
    let mut s = String::new();
    let w = &mut s;
    // The source must not depend on which layer or output block it serves, so
    // kernels of identical shape compile once (slice bases are parameters).
    let _ = writeln!(
        w,
        "#define GX {gx}\n#define GY {gy}\n#define BX {}\n#define BY {}",
        block.0, block.1
    );
    let _ = writeln!(
        w,
        r#"cbuffer Params : register(b0) {{
    int4 region;  // xy: first output pixel (image), zw: size
    int4 tile;    // xy: image coordinate of texel (0, 0) of tensor textures
    int4 image;   // xy: image size
    float4 range; // x: input offset, y: input scale, z: 1 / scale
    uint4 misc;   // x: first output (and residual) array slice
}};
cbuffer Weights : register(b1) {{ float4 W[{}]; }};
Texture2DArray<float> t_luma : register(t0);"#,
        layout.total.max(1)
    );
    for (index, source) in sources.iter().enumerate() {
        if source.tensor != INPUT_TENSOR {
            let _ = writeln!(
                w,
                "Texture2DArray<float4> t_src{index} : register(t{});",
                index + 1
            );
        }
    }
    if layer.residual.is_some() {
        let _ = writeln!(
            w,
            "Texture2DArray<float4> t_residual : register(t{});",
            sources.len() + 1
        );
    }
    if head || dense.is_some_and(|dense| dense.last) {
        let _ = writeln!(w, "RWTexture2D<float> u_out2x : register(u1);");
    } else {
        let _ = writeln!(w, "RWTexture2DArray<float4> u_out : register(u0);");
    }
    if let Some(dense) = dense {
        let _ = writeln!(
            w,
            "cbuffer HeadWeights : register(b2) {{ float4 HW[{}]; }};",
            dense.constants.len()
        );
        if dense.step > 0 {
            let _ = writeln!(w, "Texture2D<float4> t_partial : register(t14);");
        }
        if !dense.last {
            let _ = writeln!(w, "RWTexture2D<float4> u_partial : register(u2);");
        }
    }

    // Padding-aware loads in image coordinates.
    let pad = if clamp {
        "img = clamp(img, int2(0, 0), image.xy - 1);"
    } else {
        "if (any(img < 0) || any(img >= image.xy)) return 0;"
    };
    let _ = writeln!(
        w,
        "float load_luma(int2 img) {{ {pad} return (t_luma.Load(int4(img, 0, 0)) - range.x) * range.y; }}"
    );
    for (index, source) in sources.iter().enumerate() {
        if source.tensor != INPUT_TENSOR {
            let _ = writeln!(
                w,
                "float4 load_src{index}(int2 img, uint slice) {{ {pad} return t_src{index}.Load(int4(img - tile.xy, slice, 0)); }}"
            );
        }
    }

    // One float4 array (tensor parts) and one float array (luma) shared by
    // all phases; parts of a phase are laid out back to back.
    let area_of =
        |source: &KernelSource| (block.0 + 2 * source.radius) * (block.1 + 2 * source.radius);
    let part_offsets: Vec<Vec<usize>> = phases
        .iter()
        .map(|phase| {
            let mut cursor = 0;
            phase
                .parts
                .iter()
                .map(|&(source_index, _, count)| {
                    let source = &sources[source_index];
                    if source.tensor == INPUT_TENSOR {
                        0
                    } else {
                        let offset = cursor;
                        cursor += count * area_of(source);
                        offset
                    }
                })
                .collect()
        })
        .collect();
    let lds_vec4 = phases
        .iter()
        .map(|phase| {
            phase
                .parts
                .iter()
                .filter(|(source_index, _, _)| sources[*source_index].tensor != INPUT_TENSOR)
                .map(|&(source_index, _, count)| count * area_of(&sources[source_index]))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let lds_luma = sources
        .iter()
        .filter(|source| source.tensor == INPUT_TENSOR && source.radius > 0)
        .map(area_of)
        .max()
        .unwrap_or(0);
    if lds_vec4 > 0 {
        let _ = writeln!(w, "groupshared float4 lds[{lds_vec4}];");
    }
    if lds_luma > 0 {
        let _ = writeln!(w, "groupshared float lds_luma[{lds_luma}];");
    }

    let _ = writeln!(
        w,
        r#"[numthreads(GX, GY, 1)]
void main(uint3 gtid : SV_GroupThreadID, uint3 gid : SV_GroupID) {{
    int2 block = region.xy + int2(gid.xy) * int2(BX, BY);
    uint lin = gtid.y * GX + gtid.x;
    int2 base = int2(gtid.x * {px}, gtid.y * {py});
    int2 end = region.xy + region.zw;
    bool active = all(block + base < end);"#
    );
    for p in 0..pixel_count {
        let _ = writeln!(
            w,
            "    int2 pix{p} = block + base + int2({}, {});",
            p % px,
            p / px
        );
        for q in 0..quads {
            let _ = writeln!(w, "    float4 a{p}_{q} = W[{}];", layout.bias + q);
        }
    }

    // Direct (1x1-only) sources: load each slice at every pixel.
    for (source_index, source) in sources.iter().enumerate() {
        if source.radius != 0 {
            continue;
        }
        let is_luma = source.tensor == INPUT_TENSOR;
        let _ = writeln!(w, "    if (active) {{");
        for slice in 0..source.slices {
            for p in 0..pixel_count {
                if is_luma {
                    let _ = writeln!(
                        w,
                        "      float d{source_index}_{slice}_{p} = load_luma(pix{p});"
                    );
                } else {
                    let _ = writeln!(
                        w,
                        "      float4 d{source_index}_{slice}_{p} = load_src{source_index}(pix{p}, {slice});"
                    );
                }
            }
            for (input_index, input) in layer.inputs.iter().enumerate() {
                if input.tensor != source.tensor {
                    continue;
                }
                // Kernel 1: window cell (y, x) is pixel (x, y) of the block.
                let cell =
                    |wy: usize, wx: usize| format!("d{source_index}_{slice}_{}", wy * px + wx);
                emit_accumulate(
                    w,
                    model,
                    layer,
                    &layout,
                    input_index,
                    slice,
                    (px, py),
                    &cell,
                    &constant,
                    None,
                );
            }
        }
        let _ = writeln!(w, "    }}");
    }

    // Staged sources, phase by phase.
    for (phase_index, phase) in phases.iter().enumerate() {
        if phase_index > 0 {
            let _ = writeln!(w, "    GroupMemoryBarrierWithGroupSync();");
        }
        for (part_index, &(source_index, first, count)) in phase.parts.iter().enumerate() {
            let source = &sources[source_index];
            let r = source.radius;
            let lw = block.0 + 2 * r;
            let area = area_of(source);
            let (target, load) = if source.tensor == INPUT_TENSOR {
                ("lds_luma[i]".to_string(), "load_luma(img)".to_string())
            } else {
                (
                    format!("lds[{} + i]", part_offsets[phase_index][part_index]),
                    format!("load_src{source_index}(img, {first} + i / {area})"),
                )
            };
            let _ = writeln!(
                w,
                "    for (uint i = lin; i < {}; i += GX * GY) {{ uint r = i % {area}; int2 img = block - {r} + int2(r % {lw}, r / {lw}); {target} = {load}; }}",
                count * area
            );
        }
        let _ = writeln!(
            w,
            "    GroupMemoryBarrierWithGroupSync();\n    if (active) {{"
        );
        for (part_index, &(source_index, first, count)) in phase.parts.iter().enumerate() {
            let source = &sources[source_index];
            let r = source.radius;
            let lw = block.0 + 2 * r;
            let area = area_of(source);
            let name = if source.tensor == INPUT_TENSOR {
                "lds_luma"
            } else {
                "lds"
            };
            let part_base = part_offsets[phase_index][part_index];
            for slice in first..first + count {
                let slice_base = part_base + (slice - first) * area;
                // Inputs of this source with their kernel offsets.
                let readers: Vec<usize> = layer
                    .inputs
                    .iter()
                    .enumerate()
                    .filter(|(_, input)| input.tensor == source.tensor)
                    .map(|(index, _)| index)
                    .collect();
                if readers.is_empty() {
                    continue;
                }
                // Load the thread's window once per slice into registers.
                let win_w = px + 2 * r;
                let win_h = py + 2 * r;
                let ty = if source.tensor == INPUT_TENSOR {
                    "float"
                } else {
                    "float4"
                };
                let prefetch = if head
                    && pixel_count == 1
                    && source.tensor != INPUT_TENSOR
                    && readers.len() == 1
                    && layer.inputs[readers[0]].transform == InputTransform::Identity
                {
                    head_prefetch
                } else {
                    0
                };
                if prefetch > 0 {
                    let input_index = readers[0];
                    let k = layer.inputs[input_index].kernel;
                    let offset = r - k / 2;
                    for start in (0..k).step_by(prefetch) {
                        let end = (start + prefetch).min(k);
                        for wy in start..end {
                            for wx in 0..k {
                                let _ = writeln!(
                                    w,
                                    "float4 s{source_index}_{slice}_{wy}_{wx}={name}[{slice_base}+(base.y+{})*{lw}+base.x+{}];",
                                    wy + offset,
                                    wx + offset
                                );
                            }
                        }
                        let cell =
                            |wy: usize, wx: usize| format!("s{source_index}_{slice}_{wy}_{wx}");
                        emit_accumulate(
                            w,
                            model,
                            layer,
                            &layout,
                            input_index,
                            slice,
                            (1, 1),
                            &cell,
                            &constant,
                            Some((start, end)),
                        );
                    }
                    continue;
                }
                for wy in 0..win_h {
                    for wx in 0..win_w {
                        let _ = writeln!(
                            w,
                            "      {ty} s{source_index}_{slice}_{wy}_{wx} = {name}[{slice_base} + (base.y + {wy}) * {lw} + base.x + {wx}];"
                        );
                    }
                }
                for input_index in readers {
                    let offset = r - layer.inputs[input_index].kernel / 2;
                    let cell = |wy: usize, wx: usize| {
                        format!("s{source_index}_{slice}_{}_{}", wy + offset, wx + offset)
                    };
                    emit_accumulate(
                        w,
                        model,
                        layer,
                        &layout,
                        input_index,
                        slice,
                        (px, py),
                        &cell,
                        &constant,
                        None,
                    );
                }
            }
        }
        let _ = writeln!(w, "    }}");
    }

    // Epilogue per pixel.
    for p in 0..pixel_count {
        let _ = writeln!(w, "    if (all(pix{p} < end)) {{");
        if dense.is_some_and(|dense| dense.last) {
            if layer.activation == Activation::Relu {
                for q in 0..quads {
                    let _ = writeln!(w, "a{p}_{q} = max(a{p}_{q}, 0.0);");
                }
            }
        } else {
            emit_epilogue(w, model, layer, &layout, head, p, &constant);
        }
        if let Some(dense) = dense {
            emit_dense(w, model, layer, dense, p);
        }
        let _ = writeln!(w, "    }}");
    }
    let _ = writeln!(w, "}}");
    s
}

fn emit_dense(
    w: &mut String,
    model: &SrModel,
    layer: &ConvLayer,
    dense: &DenseContribution,
    p: usize,
) {
    let head = model.layers.last().expect("dense head");
    let count = dense.constants.len() - 1;
    let _ = writeln!(
        w,
        "{{ float4 sum = {};",
        if dense.step == 0 {
            format!("HW[{count}]")
        } else {
            format!("t_partial.Load(int3(pix{p} - tile.xy, 0))")
        }
    );
    let mut index = 0;
    for input in head
        .inputs
        .iter()
        .filter(|input| input.tensor == layer.output)
    {
        for c in 0..layer.out_channels {
            let value = format!("a{p}_{}.{}", c / 4, LANES[c % 4]);
            let value = match input.transform {
                InputTransform::Identity => value,
                InputTransform::Relu => format!("max({value}, 0.0)"),
                InputTransform::NegRelu => format!("max(-{value}, 0.0)"),
            };
            let _ = writeln!(w, "sum = mad(HW[{index}], {value}, sum);");
            index += 1;
        }
    }
    if dense.last {
        let _ = writeln!(w, "{{ float4 a{p}_0 = sum;");
        emit_head(w, model, p);
        let _ = writeln!(w, "}}");
    } else {
        let _ = writeln!(w, "u_partial[pix{p} - tile.xy] = sum;");
    }
    let _ = writeln!(w, "}}");
}

fn emit_ring(
    model: &SrModel,
    layer: &ConvLayer,
    out_count: usize,
    head: bool,
    sources: &[KernelSource],
    variant_stride: usize,
) -> String {
    let layout = constant_layout(model, layer, out_count);
    let quads = layout.quads;
    let clamp = model.padding == Padding::Clamp;
    let weight = |index: usize| format!("Wr[vbase + {index}]");
    let mut s = String::new();
    let w = &mut s;
    let _ = writeln!(
        w,
        r#"cbuffer Params : register(b0) {{
    int4 region;
    int4 tile;
    int4 image;
    float4 range;
    uint4 misc;
}};
Buffer<float4> Wr : register(t15);
Texture2DArray<float> t_luma : register(t0);"#
    );
    for (index, source) in sources.iter().enumerate() {
        if source.tensor != INPUT_TENSOR {
            let _ = writeln!(
                w,
                "Texture2DArray<float4> t_src{index} : register(t{});",
                index + 1
            );
        }
    }
    if layer.residual.is_some() {
        let _ = writeln!(
            w,
            "Texture2DArray<float4> t_residual : register(t{});",
            sources.len() + 1
        );
    }
    if head {
        let _ = writeln!(w, "RWTexture2D<float> u_out2x : register(u1);");
    } else {
        let _ = writeln!(w, "RWTexture2DArray<float4> u_out : register(u0);");
    }
    let pad = if clamp {
        "img = clamp(img, int2(0, 0), image.xy - 1);"
    } else {
        "if (any(img < 0) || any(img >= image.xy)) return 0;"
    };
    let _ = writeln!(
        w,
        "float load_luma(int2 img) {{ {pad} return (t_luma.Load(int4(img, 0, 0)) - range.x) * range.y; }}"
    );
    for (index, source) in sources.iter().enumerate() {
        if source.tensor != INPUT_TENSOR {
            let _ = writeln!(
                w,
                "float4 load_src{index}(int2 img, uint slice) {{ {pad} return t_src{index}.Load(int4(img - tile.xy, slice, 0)); }}"
            );
        }
    }
    let _ = writeln!(
        w,
        r#"uint border_class(int v, int extent) {{ return v == 0 ? 0u : (v == extent - 1 ? 2u : 1u); }}
[numthreads({RING_GROUP}, 1, 1)]
void main(uint3 dtid : SV_DispatchThreadID) {{
    int id = int(dtid.x);
    int width = image.x;
    int height = image.y;
    int2 pix0;
    if (id < width) {{
        pix0 = int2(id, 0);
    }} else if (id < 2 * width) {{
        pix0 = int2(id - width, height - 1);
    }} else if (id < 2 * width + height - 2) {{
        pix0 = int2(0, 1 + id - 2 * width);
    }} else if (id < 2 * width + 2 * (height - 2)) {{
        pix0 = int2(width - 1, 1 + id - 2 * width - (height - 2));
    }} else {{
        return;
    }}
    if (any(pix0 < region.xy) || any(pix0 >= region.xy + region.zw)) {{
        return;
    }}
    uint vbase = (border_class(pix0.y, height) * 3u + border_class(pix0.x, width)) * {variant_stride}u;
    int2 end = region.xy + region.zw;"#
    );
    for q in 0..quads {
        let _ = writeln!(w, "    float4 a0_{q} = {};", weight(layout.bias + q));
    }
    for (source_index, source) in sources.iter().enumerate() {
        let is_luma = source.tensor == INPUT_TENSOR;
        for slice in 0..source.slices {
            for (input_index, input) in layer.inputs.iter().enumerate() {
                if input.tensor != source.tensor {
                    continue;
                }
                let r = (input.kernel / 2) as isize;
                let cell = |wy: usize, wx: usize| {
                    let (dx, dy) = (wx as isize - r, wy as isize - r);
                    if is_luma {
                        format!("load_luma(pix0 + int2({dx}, {dy}))")
                    } else {
                        format!("load_src{source_index}(pix0 + int2({dx}, {dy}), {slice})")
                    }
                };
                emit_accumulate(
                    w,
                    model,
                    layer,
                    &layout,
                    input_index,
                    slice,
                    (1, 1),
                    &cell,
                    &weight,
                    None,
                );
            }
        }
    }
    let _ = writeln!(w, "    {{");
    emit_epilogue(w, model, layer, &layout, head, 0, &weight);
    let _ = writeln!(
        w,
        "    }}
}}"
    );
    s
}

/// Residual, activation and store (or head) for accumulators `a{p}_*` of
/// pixel `pix{p}`.
fn emit_epilogue(
    w: &mut String,
    model: &SrModel,
    layer: &ConvLayer,
    layout: &ConstantLayout,
    head: bool,
    p: usize,
    weight: &dyn Fn(usize) -> String,
) {
    let quads = layout.quads;
    if layer.residual.is_some() {
        for q in 0..quads {
            let _ = writeln!(
                w,
                "      a{p}_{q} += t_residual.Load(int4(pix{p} - tile.xy, misc.x + {q}, 0));"
            );
        }
    }
    for q in 0..quads {
        let line = match &layer.activation {
            Activation::None => None,
            Activation::Relu => Some(format!("a{p}_{q} = max(a{p}_{q}, 0.0);")),
            Activation::Silu => Some(format!("a{p}_{q} = a{p}_{q} / (1.0 + exp(-a{p}_{q}));")),
            Activation::Prelu(_) => Some(format!(
                "a{p}_{q} = max(a{p}_{q}, 0.0) + {} * min(a{p}_{q}, 0.0);",
                weight(layout.alpha + q)
            )),
        };
        if let Some(line) = line {
            let _ = writeln!(w, "      {line}");
        }
    }
    if head {
        emit_head(w, model, p);
    } else {
        for q in 0..quads {
            let _ = writeln!(
                w,
                "      u_out[int3(pix{p} - tile.xy, misc.x + {q})] = a{p}_{q};"
            );
        }
    }
}

fn emit_head(w: &mut String, model: &SrModel, p: usize) {
    // Subpixel (sx, sy) takes the order-dependent lane of the accumulator.
    let lane = |sx: usize, sy: usize| {
        let channel = match model.head.order {
            SubpixelOrder::RowMajor => sy * 2 + sx,
            SubpixelOrder::ColumnMajor => sx * 2 + sy,
        };
        LANES[channel]
    };
    let _ = writeln!(
        w,
        "      float4 sub = float4(a{p}_0.{}, a{p}_0.{}, a{p}_0.{}, a{p}_0.{}) * range.z;",
        lane(0, 0),
        lane(1, 0),
        lane(0, 1),
        lane(1, 1)
    );
    match model.head.base {
        HeadBase::None => {
            let _ = writeln!(w, "      sub += range.x;");
        }
        HeadBase::Bilinear => {
            // Raw luma 3x3 neighborhood, clamp-to-edge; subpixel centers sit
            // at +-0.25 so each is a 9:3:3:1 blend of the center and the
            // neighbors on its side.
            let _ = writeln!(
                w,
                r#"      float n[9];
      [unroll] for (int j = 0; j < 9; ++j) {{
          int2 img = clamp(pix{p} + int2(j % 3 - 1, j / 3 - 1), int2(0, 0), image.xy - 1);
          n[j] = t_luma.Load(int4(img, 0, 0));
      }}
      sub.x += (9.0 * n[4] + 3.0 * n[3] + 3.0 * n[1] + n[0]) / 16.0;
      sub.y += (9.0 * n[4] + 3.0 * n[5] + 3.0 * n[1] + n[2]) / 16.0;
      sub.z += (9.0 * n[4] + 3.0 * n[3] + 3.0 * n[7] + n[6]) / 16.0;
      sub.w += (9.0 * n[4] + 3.0 * n[5] + 3.0 * n[7] + n[8]) / 16.0;"#
            );
        }
    }
    if model.head.clamp {
        let _ = writeln!(w, "      sub = saturate(sub);");
    }
    let _ = writeln!(
        w,
        r#"      int2 o = pix{p} * 2;
      u_out2x[o] = sub.x;
      u_out2x[o + int2(1, 0)] = sub.y;
      u_out2x[o + int2(0, 1)] = sub.z;
      u_out2x[o + int2(1, 1)] = sub.w;"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::pipeline::LumaUpscalerMode;
    use crate::renderer::sr::catalog::execution_model;

    #[test]
    fn plans_every_bundled_network_within_d3d11_limits() {
        for mode in LumaUpscalerMode::ALL {
            let Ok(model) = execution_model(mode) else {
                continue;
            };
            let plan = plan(&model);
            for kernel in &plan.kernels {
                assert!(kernel.constants.len() <= MAX_CONSTANTS, "{}", mode.label());
                let lds: usize = kernel
                    .source_code
                    .lines()
                    .filter_map(|line| line.strip_prefix("groupshared "))
                    .map(|decl| {
                        let size: usize = decl
                            .split('[')
                            .nth(1)
                            .and_then(|tail| tail.split(']').next())
                            .and_then(|n| n.parse().ok())
                            .unwrap();
                        size * if decl.starts_with("float4") { 16 } else { 4 }
                    })
                    .sum();
                assert!(lds <= 32 * 1024, "{} LDS {lds}", mode.label());
            }
            let head_kernels = plan.kernels.iter().filter(|kernel| kernel.head).count();
            assert_eq!(head_kernels, 1, "{}", mode.label());
            eprintln!(
                "{:<22} kernels {:>3} slots {:?} halo {}",
                mode.label(),
                plan.kernels.len(),
                plan.slot_slices,
                plan.max_halo
            );
        }
    }

    #[test]
    #[ignore = "prints generated HLSL"]
    fn dump_kernel_for_inspection() {
        let mode =
            std::env::var("ERIKA_SR_DUMP_MODE").unwrap_or_else(|_| "artcnn_c4f16".to_string());
        let mode = LumaUpscalerMode::ALL
            .into_iter()
            .find(|candidate| candidate.label() == mode)
            .unwrap();
        let index: usize = std::env::var("ERIKA_SR_DUMP_KERNEL")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        let plan = plan(&execution_model(mode).unwrap());
        let kernel = &plan.kernels[index];
        eprintln!(
            "{:?} sources {:?}\n{}",
            kernel.shape, kernel.sources, kernel.source_code
        );
    }

    #[test]
    fn artcnn_plan_uses_two_ping_pong_slots_and_fused_head() {
        let model = execution_model(LumaUpscalerMode::ArtCnnC4F16).unwrap();
        let plan = plan(&model);
        assert_eq!(plan.kernels.len(), 5);
        assert_eq!(plan.slot_slices, vec![4, 4]);
        assert!(plan.kernels.iter().all(|kernel| kernel.out_count <= 16));
        assert!(plan.kernels[4].head);
        assert!(plan.kernels[0].ring.is_some());
        assert!(plan.kernels[4].ring.is_some());
        assert_eq!(ring_pixels(5, 4), 14);
        assert_eq!(plan.max_halo, 7);
    }
}
