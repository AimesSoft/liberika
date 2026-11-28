//! Generic Metal executor for [`SrModel`] networks (ArtCNN, Anime4K,
//! FSRCNNX and user models).
//!
//! # Lowering
//!
//! The model is first lowered to layers whose inputs carry no element-wise
//! transform: a tensor read through `Relu`/`NegRelu` (Anime4K's CReLU) is
//! stored by its producer as a transformed *view* (`max(x, 0)`, `max(-x, 0)`
//! or both halves interleaved as `2C` channels), and the consumer's weights
//! are re-indexed onto that view. A layer reading both halves of a tensor
//! with the same kernel then becomes one convolution over `2C` channels.
//! Raw views are kept for identity inputs, residual adds and an unfused head.
//!
//! # Storage
//!
//! Views live in fp16 device buffers laid out NHWC with `P` border columns on
//! each side (`P` = largest kernel radius). Producers write the border
//! columns themselves (zeros for zero padding, edge copies for clamp
//! padding), so convolution windows never branch on `x`; rows outside the
//! image are handled per row (skipped for zero padding, clamped otherwise).
//! Frames are processed in horizontal bands sized to a memory budget; every
//! tensor of a band is computed with its receptive-field halo
//! ([`SrModel::tensor_halos`]), so banding is exact.
//!
//! # Kernels
//!
//! Each layer is an implicit GEMM `out[px x Cout] += A[px x K] * W[K x Cout]`
//! per input and kernel row: in NHWC the `k` taps of a kernel row over all
//! `C` channels are `k*C` contiguous halves, so the `A` operand is a strided
//! view of the feature map (row stride `C`, overlapping rows) and no im2col
//! copy is needed. Three implementations share that layout:
//!
//! - **Tensor ops** (Metal 4 `mpp::tensor_ops::matmul2d`, Apple10 GPUs such
//!   as M5/A19 with GPU neural accelerators).
//! - **simdgroup_matrix** (`simdgroup_half8x8`, Apple7+).
//! - **Scalar** (one thread per pixel and four output channels) for GPUs
//!   without matrix support. The same kernel re-evaluates the one-pixel image
//!   ring of algebraically folded layers with their exact border weights
//!   after a matrix pass.
//!
//! Bias, residual, activation, view transforms and the 2x head (depth to
//! space, bilinear base, range mapping, clamp) are fused into the epilogue,
//! which writes the upscaled luma texture directly.

use std::ffi::c_void;
use std::fmt::Write as _;
use std::ptr::NonNull;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSRange, NSString};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCompileOptions, MTLComputeCommandEncoder, MTLComputePipelineState, MTLDevice, MTLGPUFamily,
    MTLLanguageVersion, MTLLibrary, MTLPixelFormat, MTLResource, MTLResourceOptions, MTLSize,
    MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use crate::core::{PlayerError, Result};
use crate::renderer::sr::model::{
    Activation, HeadBase, INPUT_TENSOR, InputTransform, Padding, SrModel, SubpixelOrder, f32_to_f16,
};
use crate::renderer::sr::reference::{LumaRange, input_normalization};

/// Kernel family executing the convolutions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// Scalar ALU kernels.
    Scalar,
    /// `simdgroup_half8x8` matrix multiplies.
    Simdgroup,
    /// Metal 4 tensor ops (`matmul2d`), backed by the GPU's matrix units on
    /// Apple10 devices.
    TensorOps,
}

impl EngineKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::Simdgroup => "simdgroup_matrix",
            Self::TensorOps => "metal4_tensor_ops",
        }
    }

    /// Best kernel family the device supports.
    pub fn preferred(device: &ProtocolObject<dyn MTLDevice>) -> Self {
        if device.supportsFamily(MTLGPUFamily::Apple10) {
            Self::TensorOps
        } else if device.supportsFamily(MTLGPUFamily::Apple7) {
            Self::Simdgroup
        } else {
            Self::Scalar
        }
    }
}

/// Simdgroups per threadgroup of the simdgroup kernel (one output row each).
const SIMDS: usize = 4;
/// Largest tensor-op M tile (pixel groups per threadgroup); sizes slack.
const TENSOR_TILE_MAX: usize = 128;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Pixel groups (GEMM rows) per tensor-op threadgroup. One simdgroup per
/// threadgroup running 32 x N tiles measured fastest on Apple M5 Pro
/// (25 TFLOPS at N >= 32 vs 21 with four simdgroups sharing a 128-row op).
fn tensor_tile() -> usize {
    env_usize("ERIKA_SR_TENSOR_TM", 32).clamp(16, TENSOR_TILE_MAX)
}

fn tensor_simds() -> usize {
    env_usize("ERIKA_SR_TENSOR_SG", 1).clamp(1, 4)
}

/// Pixels evaluated per GEMM row on tensor ops: narrow layers widen N to
/// about 32 columns, where the matrix units run at full rate.
fn tensor_group(out_channels: usize) -> usize {
    let cap = env_usize("ERIKA_SR_TENSOR_GMAX", 8).max(1);
    // Measured per width on Apple M5 Pro (1080p layer times).
    let group = match out_channels {
        0..=4 => env_usize("ERIKA_SR_TENSOR_G4", 4),
        5..=8 => 2,
        9..=12 => 1,
        13..=16 => 2,
        _ => 1,
    };
    group.min(cap)
}

/// Pixel grouping of simdgroup kernels for narrow layers (only applied to
/// wide windows, see [`simd_grouped`]).
fn simd_group(out_channels: usize) -> usize {
    if round_up(out_channels, 8) <= 8 {
        16 / round_up(out_channels, 4)
    } else {
        1
    }
}
/// Band feature buffers take at most this share of the device's
/// recommended working set, clamped to the range below.
const BAND_BUDGET_SHARE: u64 = 32;
const MIN_BAND_BUDGET: usize = 64 << 20;
const MAX_BAND_BUDGET: usize = 384 << 20;
const MIN_BAND_ROWS: usize = 32;
/// Layers whose padded output width reaches this run on tensor ops in a
/// tensor-op engine; narrower layers use `simdgroup_matrix` kernels, which
/// measured faster for them (Apple M5 Pro).
const TENSOR_MIN_OUTPUTS: usize = 16;
/// Input views a single layer kernel may bind (`buffer(0..16)`).
const MAX_LAYER_INPUTS: usize = 16;
const BUFFER_RESIDUAL: usize = 16;
const BUFFER_OUT0: usize = 17;
const BUFFER_OUT1: usize = 18;
const BUFFER_WEIGHTS: usize = 19;
const BUFFER_PARAMS: usize = 20;
const BUFFER_ACC: usize = 21;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewKind {
    Raw,
    /// `max(x, 0)`.
    Pos,
    /// `max(-x, 0)`.
    Neg,
    /// `max(x, 0)` in channels `0..C`, `max(-x, 0)` in `C..2C`.
    Crelu,
    /// Input luma only: channel `c` holds row `y + c - k/2` (zero or clamp
    /// padded), a vertical im2col that turns the first `k x k` convolution
    /// into one `k x 1` convolution over `k` channels.
    Stacked,
}

#[derive(Debug, Clone)]
struct View {
    tensor: usize,
    kind: ViewKind,
    /// Stored channels per pixel.
    channels: usize,
    /// Rows held by a stacked view (0 otherwise).
    stack: usize,
    slot: usize,
}

#[derive(Debug, Clone)]
struct PlannedInput {
    view: usize,
    /// Kernel width.
    kernel: usize,
    /// Kernel height (1 for stacked views).
    rows: usize,
    /// Rows of the per-kernel-row weight matrix: `kernel * C` rounded up to 16.
    kpad: usize,
    /// Offset of the input's `[rows][kpad][npad]` weights inside a variant
    /// block.
    offset: usize,
}

#[derive(Debug, Clone)]
struct PlannedLayer {
    output: usize,
    out_channels: usize,
    npad: usize,
    inputs: Vec<PlannedInput>,
    /// Distinct views read, in binding order (`buffer(i)`).
    bindings: Vec<usize>,
    residual: Option<usize>,
    activation: ActivationKind,
    writes: Vec<usize>,
    head: bool,
    variants: usize,
    block_offset: usize,
    block_stride: usize,
    bias_offset: usize,
    alpha_offset: Option<usize>,
    /// Pixel-grouped interior weights for narrow layers (one entry per
    /// group size some kernel family uses).
    grouped: Vec<Grouped>,
}

/// A narrow layer (`Cout <= 8`) evaluated `group` adjacent pixels at a time:
/// the GEMM's N dimension becomes `group * Cout` columns (pixel-major) and
/// its K dimension the union of the pixels' windows, `(k + group - 1) * C`
/// per kernel row. Matrix units run far below peak at N = 8; this trades
/// some zero taps for full-width tiles.
#[derive(Debug, Clone)]
struct Grouped {
    group: usize,
    npad: usize,
    /// Per input: K rows per kernel row and the weights offset (absolute).
    inputs: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationKind {
    None,
    Relu,
    Silu,
    Prelu,
}

/// Static execution plan for one model.
#[derive(Debug, Clone)]
pub struct Plan {
    pad: usize,
    padding: Padding,
    views: Vec<View>,
    input_views: Vec<usize>,
    halos: Vec<usize>,
    max_halo: usize,
    slot_channels: Vec<usize>,
    layers: Vec<PlannedLayer>,
    head_view: Option<usize>,
    head_order: SubpixelOrder,
    head_base: HeadBase,
    head_clamp: bool,
    weights: Vec<u16>,
    absorption: Option<Absorption>,
}

/// A head layer made only of 1x1 inputs over earlier tensors (Anime4K's
/// dense head over every feature map). On tensor ops each producer adds its
/// tensor's head contribution to a four-channel accumulator right after
/// storing the tile (a small matmul over the tile it just wrote), so the
/// head no longer re-reads every feature map at the end.
#[derive(Debug, Clone)]
struct Absorption {
    head: usize,
    terms: Vec<AbsorbedTerm>,
}

#[derive(Debug, Clone)]
struct AbsorbedTerm {
    producer: usize,
    view: usize,
    kpad: usize,
    /// Offset of the head's `[kpad][npad]` weights for this view.
    weights: usize,
    /// Four-channel view receiving this term's contribution.
    partial: usize,
}

fn round_up(value: usize, multiple: usize) -> usize {
    value.div_ceil(multiple) * multiple
}

fn detect_absorption(
    layers: &[PlannedLayer],
    views: &mut Vec<View>,
    halos: &[usize],
) -> Option<Absorption> {
    if std::env::var_os("ERIKA_SR_NO_ABSORB").is_some() {
        return None;
    }
    let head = layers.iter().position(|layer| layer.head)?;
    let layer = &layers[head];
    if layer.residual.is_some()
        || layer.variants > 1
        || layer.out_channels != 4
        || layer.inputs.len() < 2
        || layer
            .inputs
            .iter()
            .any(|input| input.kernel != 1 || input.rows != 1)
    {
        return None;
    }
    // Narrow views (Anime4K M: 8 channels) read cheaply enough that the
    // per-producer matmul does not pay off (measured).
    if layer
        .inputs
        .iter()
        .any(|input| views[input.view].channels < 16)
    {
        return None;
    }
    let mut terms = Vec::new();
    for input in &layer.inputs {
        let producer = layers[..head]
            .iter()
            .position(|candidate| candidate.writes.contains(&input.view))?;
        terms.push(AbsorbedTerm {
            producer,
            view: input.view,
            kpad: input.kpad,
            weights: layer.block_offset + input.offset,
            partial: usize::MAX,
        });
    }
    // One write-only partial per term, rows following its producer.
    for term in terms.iter_mut() {
        views.push(View {
            tensor: layers[term.producer].output,
            kind: ViewKind::Raw,
            channels: layer.out_channels,
            stack: 0,
            slot: usize::MAX,
        });
        term.partial = views.len() - 1;
    }
    let _ = halos;
    Some(Absorption { head, terms })
}

impl Plan {
    pub fn new(model: &SrModel) -> Result<Self> {
        model.validate()?;
        let tensors = model.tensors.len();
        let fused_head = !model.layers.iter().any(|layer| {
            layer
                .inputs
                .iter()
                .any(|input| input.tensor == model.head.tensor)
                || layer.residual == Some(model.head.tensor)
        });

        // Transforms each tensor is read through.
        let mut uses_raw = vec![false; tensors];
        let mut uses_pos = vec![false; tensors];
        let mut uses_neg = vec![false; tensors];
        for layer in &model.layers {
            for input in &layer.inputs {
                match input.transform {
                    InputTransform::Identity => uses_raw[input.tensor] = true,
                    InputTransform::Relu => uses_pos[input.tensor] = true,
                    InputTransform::NegRelu => uses_neg[input.tensor] = true,
                }
            }
            if let Some(residual) = layer.residual {
                uses_raw[residual] = true;
            }
        }
        if !fused_head {
            uses_raw[model.head.tensor] = true;
        }

        // Vertical im2col of the luma when every reader applies the same
        // k x k kernel without transform.
        let input_readers: Vec<_> = model
            .layers
            .iter()
            .flat_map(|layer| layer.inputs.iter())
            .filter(|input| input.tensor == INPUT_TENSOR)
            .collect();
        let stack_input = input_readers.first().and_then(|first| {
            let uniform = input_readers.iter().all(|input| {
                input.kernel == first.kernel && input.transform == InputTransform::Identity
            });
            let other_use = model
                .layers
                .iter()
                .any(|layer| layer.residual == Some(INPUT_TENSOR))
                || model.head.tensor == INPUT_TENSOR;
            (uniform && !other_use && first.kernel > 1).then_some(first.kernel)
        });

        // Views per tensor and the (view, channel offset) each transform maps to.
        let mut views: Vec<View> = Vec::new();
        let mut tensor_views: Vec<Vec<usize>> = vec![Vec::new(); tensors];
        let mut transform_view = vec![[None::<(usize, usize)>; 3]; tensors];
        for tensor in 0..tensors {
            let channels = model.tensors[tensor];
            let mut add = |kind: ViewKind, stored: usize| {
                views.push(View {
                    tensor,
                    kind,
                    // Padding stacked rows to an aligned channel count
                    // measured slower (more staging traffic).
                    channels: stored,
                    stack: if kind == ViewKind::Stacked { stored } else { 0 },
                    slot: usize::MAX,
                });
                tensor_views[tensor].push(views.len() - 1);
                views.len() - 1
            };
            if tensor == INPUT_TENSOR
                && let Some(k) = stack_input
            {
                let view = add(ViewKind::Stacked, k);
                transform_view[tensor][0] = Some((view, 0));
                continue;
            }
            if uses_raw[tensor] {
                let view = add(ViewKind::Raw, channels);
                transform_view[tensor][0] = Some((view, 0));
            }
            match (uses_pos[tensor], uses_neg[tensor]) {
                (true, true) => {
                    let view = add(ViewKind::Crelu, 2 * channels);
                    transform_view[tensor][1] = Some((view, 0));
                    transform_view[tensor][2] = Some((view, channels));
                }
                (true, false) => {
                    let view = add(ViewKind::Pos, channels);
                    transform_view[tensor][1] = Some((view, 0));
                }
                (false, true) => {
                    let view = add(ViewKind::Neg, channels);
                    transform_view[tensor][2] = Some((view, 0));
                }
                (false, false) => {}
            }
        }

        let pad = model
            .layers
            .iter()
            .flat_map(|layer| layer.inputs.iter().map(|input| input.kernel / 2))
            .max()
            .unwrap_or(0);

        // Lower every layer and pack its weights.
        let mut weights: Vec<u16> = Vec::new();
        let mut layers = Vec::with_capacity(model.layers.len());
        for (layer_index, layer) in model.layers.iter().enumerate() {
            let head = fused_head && layer.output == model.head.tensor;
            let out_channels = layer.out_channels;
            let npad = round_up(out_channels, 8);
            // Merge inputs reading the same view with the same kernel.
            let mut inputs: Vec<PlannedInput> = Vec::new();
            let mut members: Vec<Vec<(usize, usize)>> = Vec::new(); // (source input, channel offset)
            for (source_index, input) in layer.inputs.iter().enumerate() {
                let slot = match input.transform {
                    InputTransform::Identity => 0,
                    InputTransform::Relu => 1,
                    InputTransform::NegRelu => 2,
                };
                let (view, channel_offset) = transform_view[input.tensor][slot]
                    .expect("every consumed transform has a view");
                let existing = inputs
                    .iter()
                    .position(|planned| planned.view == view && planned.kernel == input.kernel);
                let position = existing.unwrap_or_else(|| {
                    let stacked = views[view].kind == ViewKind::Stacked;
                    inputs.push(PlannedInput {
                        view,
                        kernel: input.kernel,
                        rows: if stacked { 1 } else { input.kernel },
                        kpad: round_up(input.kernel * views[view].channels, 16),
                        offset: 0,
                    });
                    members.push(Vec::new());
                    inputs.len() - 1
                });
                members[position].push((source_index, channel_offset));
            }
            let mut bindings: Vec<usize> = Vec::new();
            for input in &inputs {
                if !bindings.contains(&input.view) {
                    bindings.push(input.view);
                }
            }
            if bindings.len() > MAX_LAYER_INPUTS {
                return Err(PlayerError::Renderer(format!(
                    "upscaler model {:?}: layer {layer_index} reads {} tensors (max {MAX_LAYER_INPUTS})",
                    model.name,
                    bindings.len()
                )));
            }
            let mut cursor = 0;
            for input in inputs.iter_mut() {
                input.offset = cursor;
                cursor += input.rows * input.kpad * npad;
            }
            let bias_offset = cursor;
            let block_stride = round_up(cursor + npad, 8);
            let variants = if layer.border.is_some() { 9 } else { 1 };
            let block_offset = round_up(weights.len(), 8);
            weights.resize(block_offset + variants * block_stride, 0);
            let mut block = vec![0.0f32; block_stride];
            let mut interior = Vec::new();
            for variant in 0..variants {
                block.iter_mut().for_each(|value| *value = 0.0);
                let (source_weights, bias): (Vec<&Vec<f32>>, &Vec<f32>) = match &layer.border {
                    Some(border) => (
                        border[variant].inputs.iter().collect(),
                        &border[variant].bias,
                    ),
                    None => (
                        layer.inputs.iter().map(|input| &input.weights).collect(),
                        &layer.bias,
                    ),
                };
                for (planned, members) in inputs.iter().zip(&members) {
                    let view_channels = views[planned.view].channels;
                    let stacked = views[planned.view].kind == ViewKind::Stacked;
                    let k = planned.kernel;
                    for &(source_index, channel_offset) in members {
                        let source = &layer.inputs[source_index];
                        let channels = model.tensors[source.tensor];
                        let w = source_weights[source_index];
                        for o in 0..out_channels {
                            for c in 0..channels {
                                for ky in 0..k {
                                    for kx in 0..k {
                                        let value = w[((o * channels + c) * k + ky) * k + kx];
                                        let (matrix, row) = if stacked {
                                            (0, kx * view_channels + ky)
                                        } else {
                                            (ky, kx * view_channels + channel_offset + c)
                                        };
                                        let index = planned.offset
                                            + (matrix * planned.kpad + row) * npad
                                            + o;
                                        block[index] += value;
                                    }
                                }
                            }
                        }
                    }
                }
                block[bias_offset..bias_offset + out_channels].copy_from_slice(bias);
                if variant == variants / 2 {
                    interior = block.clone();
                }
                let target = &mut weights[block_offset + variant * block_stride..];
                for (slot, &value) in target.iter_mut().zip(&block) {
                    *slot = f32_to_f16(value);
                }
            }
            let (activation, alpha_offset) = match &layer.activation {
                Activation::None => (ActivationKind::None, None),
                Activation::Relu => (ActivationKind::Relu, None),
                Activation::Silu => (ActivationKind::Silu, None),
                Activation::Prelu(alpha) => {
                    let offset = round_up(weights.len(), 8);
                    weights.resize(offset + npad, 0);
                    for (o, &value) in alpha.iter().enumerate() {
                        weights[offset + o] = f32_to_f16(value);
                    }
                    (ActivationKind::Prelu, Some(offset))
                }
            };
            let residual = layer.residual.map(|tensor| {
                transform_view[tensor][0]
                    .expect("residual has a raw view")
                    .0
            });
            let mut groups = vec![tensor_group(out_channels), simd_group(out_channels)];
            groups.sort_unstable();
            groups.dedup();
            let grouped: Vec<Grouped> = groups
                .into_iter()
                .filter(|&group| group > 1)
                .map(|group| {
                    let gnpad = round_up(group * out_channels, 8);
                    let mut grouped_inputs = Vec::new();
                    for input in &inputs {
                        let channels = views[input.view].channels;
                        let width = input.kernel + group - 1;
                        let gkpad = round_up(width * channels, 16);
                        let offset = round_up(weights.len(), 8);
                        weights.resize(offset + input.rows * gkpad * gnpad, 0);
                        for row in 0..input.rows {
                            for kx in 0..input.kernel {
                                for c in 0..channels {
                                    for o in 0..out_channels {
                                        let value = interior[input.offset
                                            + (row * input.kpad + kx * channels + c) * npad
                                            + o];
                                        for pixel in 0..group {
                                            let q = (kx + pixel) * channels + c;
                                            let n = pixel * out_channels + o;
                                            weights[offset + (row * gkpad + q) * gnpad + n] =
                                                f32_to_f16(value);
                                        }
                                    }
                                }
                            }
                        }
                        grouped_inputs.push((gkpad, offset));
                    }
                    Grouped {
                        group,
                        npad: gnpad,
                        inputs: grouped_inputs,
                    }
                })
                .collect();
            layers.push(PlannedLayer {
                output: layer.output,
                out_channels,
                npad,
                inputs,
                bindings,
                residual,
                activation,
                writes: if head {
                    Vec::new()
                } else {
                    tensor_views[layer.output].clone()
                },
                head,
                variants,
                block_offset,
                block_stride,
                bias_offset,
                alpha_offset,
                grouped,
            });
        }
        // Slack for the last output-block group's over-reads.
        weights.resize(round_up(weights.len(), 8) + 1024, 0);

        // Slot assignment: a view's slot is reusable once its last reader ran.
        let mut last_use = vec![-1isize; views.len()];
        for (index, layer) in layers.iter().enumerate() {
            for input in &layer.inputs {
                last_use[input.view] = last_use[input.view].max(index as isize);
            }
            if let Some(residual) = layer.residual {
                last_use[residual] = last_use[residual].max(index as isize);
            }
        }
        let head_view = (!fused_head).then(|| {
            transform_view[model.head.tensor][0]
                .expect("unfused head has a raw view")
                .0
        });
        if let Some(view) = head_view {
            last_use[view] = layers.len() as isize;
        }
        let halos = model.tensor_halos();
        let absorption = detect_absorption(&layers, &mut views, &halos);
        if let Some(absorption) = absorption.as_ref() {
            for _ in &absorption.terms {
                last_use.push(absorption.head as isize);
            }
        }
        let mut slot_channels: Vec<usize> = Vec::new();
        let mut slot_free_after: Vec<isize> = Vec::new();
        let mut assign = |view: usize, producer: isize, views: &mut Vec<View>| {
            let reusable = (0..slot_channels.len()).find(|&slot| slot_free_after[slot] < producer);
            let slot = match reusable {
                Some(slot) => slot,
                None => {
                    slot_channels.push(0);
                    slot_free_after.push(0);
                    slot_channels.len() - 1
                }
            };
            slot_channels[slot] = slot_channels[slot].max(views[view].channels);
            slot_free_after[slot] = last_use[view].max(producer);
            views[view].slot = slot;
        };
        for &view in &tensor_views[INPUT_TENSOR] {
            assign(view, -1, &mut views);
        }
        for (index, layer) in layers.iter().enumerate() {
            for &view in &layer.writes {
                assign(view, index as isize, &mut views);
            }
            if let Some(absorption) = absorption.as_ref() {
                for term in absorption
                    .terms
                    .iter()
                    .filter(|term| term.producer == index)
                {
                    assign(term.partial, index as isize, &mut views);
                }
            }
        }

        let max_halo = halos.iter().copied().max().unwrap_or(0);
        Ok(Self {
            pad,
            padding: model.padding,
            views,
            input_views: tensor_views[INPUT_TENSOR].clone(),
            halos,
            max_halo,
            slot_channels,
            layers,
            head_view,
            head_order: model.head.order,
            head_base: model.head.base,
            head_clamp: model.head.clamp,
            weights,
            absorption,
        })
    }

    /// Rows per band for a `width x height` frame.
    fn band_rows(&self, width: usize, height: usize, budget: usize) -> usize {
        let row_bytes: usize = self
            .slot_channels
            .iter()
            .map(|&channels| (width + 2 * self.pad) * channels * 2)
            .sum::<usize>()
            .max(1);
        let rows = (budget / row_bytes)
            .saturating_sub(2 * self.max_halo)
            .max(MIN_BAND_ROWS);
        rows.min(height).max(1)
    }

    fn slot_halves(&self, slot: usize, width: usize, rows: usize) -> usize {
        let channels = self.slot_channels[slot];
        let row = (width + 2 * self.pad) * channels;
        row * rows + (8 * TENSOR_TILE_MAX + 64 + 2 * self.pad) * channels + 256
    }

    /// Multiply-accumulates per input pixel actually issued by the matrix
    /// kernels (including channel/tap padding), for throughput reporting.
    pub fn padded_macs_per_pixel(&self) -> u64 {
        self.layers
            .iter()
            .map(|layer| {
                layer
                    .inputs
                    .iter()
                    .map(|input| (input.rows * input.kpad * layer.npad) as u64)
                    .sum::<u64>()
            })
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Code generation

fn emit_prelude(plan: &Plan, s: &mut String, tensor_ops: bool, simdgroup: bool) {
    s.push_str("#include <metal_stdlib>\n");
    if simdgroup {
        // MSL 2.3+: only included when used, so the scalar family still
        // compiles on older systems.
        s.push_str("#include <metal_simdgroup_matrix>\n");
    }
    if tensor_ops {
        s.push_str("#include <metal_tensor>\n#include <MetalPerformancePrimitives/MetalPerformancePrimitives.h>\nusing namespace mpp::tensor_ops;\n");
        let acc = if std::env::var_os("ERIKA_SR_TENSOR_FLOAT_ACC").is_some() {
            "float"
        } else {
            "half"
        };
        let _ = writeln!(s, "typedef {acc} ACC_T;");
    }
    let _ = writeln!(
        s,
        r#"using namespace metal;
#define UNROLL _Pragma("clang loop unroll(full)")
constant constexpr int P = {pad};
struct BandParams {{
    int width;
    int height;
    int row_begin;
    int row_end;
    int ring_count;
    int ring_top;
    int ring_bottom;
    int reserved;
    float in_offset;
    float in_scale;
    float inv_scale;
    float reserved1;
    int origin[{views}];
}};
inline int border_class(int v, int extent) {{
    return v == 0 ? 0 : (v == extent - 1 ? 2 : 1);
}}
inline float luma_at(texture2d<float, access::read> luma, int x, int y, int w, int h) {{
    return luma.read(uint2(clamp(x, 0, w - 1), clamp(y, 0, h - 1))).r;
}}
inline void store_view(device half* view, int channels, int x, int width, int ch, float v, bool clamp_pad) {{
    half h = half(v);
    view[(x + P) * channels + ch] = h;
    if (P > 0) {{
        half edge = clamp_pad ? h : half(0.0h);
        if (x == 0) {{
            for (int q = 1; q <= P; ++q) {{
                view[(P - q) * channels + ch] = edge;
            }}
        }}
        if (x == width - 1) {{
            for (int q = 1; q <= P; ++q) {{
                view[(x + P + q) * channels + ch] = edge;
            }}
        }}
    }}
}}"#,
        pad = plan.pad,
        views = plan.views.len().max(1),
    );
    emit_head_pixel(plan, s);
}

/// `head_pixel(x, y, v)`: the 2x2 output block of input pixel `(x, y)` from
/// the four head channels `v` (range mapping, base, clamp, depth to space).
fn emit_head_pixel(plan: &Plan, s: &mut String) {
    s.push_str(
        "inline void head_pixel(int x, int y, float4 v, constant BandParams& p, texture2d<float, access::read> luma, texture2d<float, access::write> dst) {\n    v *= p.inv_scale;\n",
    );
    let bilinear = plan.head_base == HeadBase::Bilinear;
    if bilinear {
        // Subpixel (sx, sy) samples at (x - 0.25 + 0.5 sx, y - 0.25 + 0.5 sy):
        // weights 0.25/0.75 over the 3x3 neighborhood, clamp to edge.
        s.push_str(
            r#"    float h0[3];
    float h1[3];
    for (int r = 0; r < 3; ++r) {
        float l0 = luma_at(luma, x - 1, y - 1 + r, p.width, p.height);
        float l1 = luma_at(luma, x, y - 1 + r, p.width, p.height);
        float l2 = luma_at(luma, x + 1, y - 1 + r, p.width, p.height);
        h0[r] = l0 * 0.25f + l1 * 0.75f;
        h1[r] = l1 * 0.75f + l2 * 0.25f;
    }
"#,
        );
    } else {
        s.push_str("    v += p.in_offset;\n");
    }
    for sy in 0..2 {
        for sx in 0..2 {
            let ch = match plan.head_order {
                SubpixelOrder::RowMajor => sy * 2 + sx,
                SubpixelOrder::ColumnMajor => sx * 2 + sy,
            };
            let mut value = format!("v[{ch}]");
            if bilinear {
                let h = if sx == 0 { "h0" } else { "h1" };
                let (a, b) = if sy == 0 { (0, 1) } else { (1, 2) };
                let (wa, wb) = if sy == 0 {
                    ("0.25f", "0.75f")
                } else {
                    ("0.75f", "0.25f")
                };
                value = format!("({value} + ({h}[{a}] * {wa} + {h}[{b}] * {wb}))");
            }
            if plan.head_clamp {
                value = format!("clamp({value}, 0.0f, 1.0f)");
            }
            let _ = writeln!(
                s,
                "    dst.write(float4({value}), uint2(2 * x + {sx}, 2 * y + {sy}));"
            );
        }
    }
    s.push_str("}\n");
}

fn view_row(plan: &Plan, view: usize, pointer: &str, row: &str) -> String {
    let channels = plan.views[view].channels;
    format!("({pointer} + ({row} - p.origin[{view}]) * ((p.width + 2 * P) * {channels}))")
}

/// `act_L(v, ch, x, y)`: residual add and activation of one output element
/// (bias already added). `store_L` writes it to the layer's views.
fn emit_epilogue(plan: &Plan, index: usize, s: &mut String) {
    let layer = &plan.layers[index];
    let _ = writeln!(
        s,
        "inline float act_{index}(float v, int ch, int x, int y, constant BandParams& p, device const half* res, device const half* wts) {{"
    );
    if let Some(residual) = layer.residual {
        let row = view_row(plan, residual, "res", "y");
        let channels = plan.views[residual].channels;
        let _ = writeln!(s, "    v += float({row}[(x + P) * {channels} + ch]);");
    }
    match layer.activation {
        ActivationKind::None => {}
        ActivationKind::Relu => s.push_str("    v = max(v, 0.0f);\n"),
        ActivationKind::Silu => s.push_str("    v = v / (1.0f + exp(-v));\n"),
        ActivationKind::Prelu => {
            let offset = layer.alpha_offset.expect("prelu slopes");
            let _ = writeln!(
                s,
                "    v = max(v, 0.0f) + float(wts[{offset} + ch]) * min(v, 0.0f);"
            );
        }
    }
    s.push_str("    return v;\n}\n");
    if layer.head {
        return;
    }
    let _ = writeln!(
        s,
        "inline void store_{index}(float v, int ch, int x, int y, constant BandParams& p, device half* out0, device half* out1) {{"
    );
    let clamp_pad = plan.padding == Padding::Clamp;
    for (slot, &view) in layer.writes.iter().enumerate() {
        let stored = plan.views[view].channels;
        let row = view_row(plan, view, &format!("out{slot}"), "y");
        let store = |value: &str, channel: &str| {
            format!(
                "    store_view({row}, {stored}, x, p.width, {channel}, {value}, {clamp_pad});\n"
            )
        };
        match plan.views[view].kind {
            ViewKind::Raw => s.push_str(&store("v", "ch")),
            ViewKind::Pos => s.push_str(&store("max(v, 0.0f)", "ch")),
            ViewKind::Neg => s.push_str(&store("max(-v, 0.0f)", "ch")),
            ViewKind::Crelu => {
                s.push_str(&store("max(v, 0.0f)", "ch"));
                s.push_str(&store(
                    "max(-v, 0.0f)",
                    &format!("ch + {}", layer.out_channels),
                ));
            }
            ViewKind::Stacked => unreachable!("only the input is stacked"),
        }
    }
    s.push_str("}\n");
}

fn emit_signature(plan: &Plan, index: usize, name: &str, s: &mut String) {
    let layer = &plan.layers[index];
    let _ = writeln!(s, "kernel void {name}(");
    for (binding, _) in layer.bindings.iter().enumerate() {
        let _ = writeln!(
            s,
            "    device const half* in{binding} [[buffer({binding})]],"
        );
    }
    if layer.residual.is_some() {
        let _ = writeln!(
            s,
            "    device const half* res [[buffer({BUFFER_RESIDUAL})]],"
        );
    }
    if !layer.writes.is_empty() {
        let _ = writeln!(s, "    device half* out0 [[buffer({BUFFER_OUT0})]],");
    }
    if layer.writes.len() > 1 {
        let _ = writeln!(s, "    device half* out1 [[buffer({BUFFER_OUT1})]],");
    }
    let _ = writeln!(
        s,
        "    device const half* wts [[buffer({BUFFER_WEIGHTS})]],\n    constant BandParams& p [[buffer({BUFFER_PARAMS})]],"
    );
    if layer.head {
        s.push_str(
            "    texture2d<float, access::read> luma [[texture(0)]],\n    texture2d<float, access::write> dst [[texture(1)]],\n",
        );
    }
}

fn act_call(plan: &Plan, index: usize, value: &str) -> String {
    let res = if plan.layers[index].residual.is_some() {
        "res"
    } else {
        "nullptr"
    };
    format!("act_{index}({value}, ch, x, y, p, {res}, wts)")
}

fn store_call(plan: &Plan, index: usize, value: &str) -> String {
    let layer = &plan.layers[index];
    let out0 = if !layer.writes.is_empty() {
        "out0"
    } else {
        "nullptr"
    };
    let out1 = if layer.writes.len() > 1 {
        "out1"
    } else {
        "nullptr"
    };
    format!("store_{index}({value}, ch, x, y, p, {out0}, {out1})")
}

/// Row pointer for kernel row `ky` of input `input`, or `continue` when the
/// row lies outside the image under zero padding.
fn emit_input_row(plan: &Plan, layer: &PlannedLayer, input: &PlannedInput, s: &mut String) {
    let radius = input.rows / 2;
    let binding = layer
        .bindings
        .iter()
        .position(|&view| view == input.view)
        .expect("input is bound");
    let _ = writeln!(s, "        int iy = y + ky - {radius};");
    if input.rows > 1 {
        match plan.padding {
            Padding::Zero => s.push_str("        if (iy < 0 || iy >= p.height) continue;\n"),
            Padding::Clamp => s.push_str("        iy = clamp(iy, 0, p.height - 1);\n"),
        }
    }
    let row = view_row(plan, input.view, &format!("in{binding}"), "iy");
    let _ = writeln!(s, "        device const half* row = {row};");
}

fn variant_base(layer: &PlannedLayer) -> usize {
    layer.block_offset
        + if layer.variants > 1 {
            4 * layer.block_stride
        } else {
            0
        }
}

fn emit_scalar_kernel(plan: &Plan, index: usize, s: &mut String) {
    let layer = &plan.layers[index];
    emit_signature(plan, index, &format!("layer{index}_scalar"), s);
    s.push_str("    uint3 gid [[thread_position_in_grid]])\n{\n");
    s.push_str(
        r#"    const int W = p.width;
    const int H = p.height;
    int x;
    int y;
    if (p.ring_count > 0) {
        int t = int(gid.x);
        if (t >= p.ring_count) {
            return;
        }
        int rows = p.row_end - p.row_begin;
        int inner = max(W - 2, 0);
        if (t < 2 * rows) {
            y = p.row_begin + t / 2;
            x = (t & 1) != 0 ? W - 1 : 0;
        } else {
            t -= 2 * rows;
            if (p.ring_top != 0 && t < inner) {
                y = 0;
            } else {
                if (p.ring_top != 0) {
                    t -= inner;
                }
                y = H - 1;
            }
            x = 1 + t;
        }
    } else {
        x = int(gid.x);
        y = p.row_begin + int(gid.y);
        if (x >= W || y >= p.row_end) {
            return;
        }
    }
    const int g = int(gid.z) * 4;
"#,
    );
    if layer.variants > 1 {
        let _ = writeln!(
            s,
            "    const int variant = border_class(y, H) * 3 + border_class(x, W);\n    device const half* blk = wts + {} + variant * {};",
            layer.block_offset, layer.block_stride
        );
    } else {
        let _ = writeln!(
            s,
            "    device const half* blk = wts + {};",
            layer.block_offset
        );
    }
    let npad = layer.npad;
    let _ = writeln!(
        s,
        "    float4 acc = float4(*(device const half4*)(blk + {} + g));",
        layer.bias_offset
    );
    for input in &layer.inputs {
        let k = input.kernel;
        let radius = k / 2;
        let channels = plan.views[input.view].channels;
        let _ = writeln!(s, "    for (int ky = 0; ky < {}; ++ky) {{", input.rows);
        emit_input_row(plan, layer, input, s);
        let _ = writeln!(
            s,
            "        device const half* wr = blk + {} + ky * {} + g;",
            input.offset,
            input.kpad * npad
        );
        let _ = writeln!(
            s,
            "        for (int kx = 0; kx < {k}; ++kx) {{\n            device const half* px = row + (x + kx - {radius} + P) * {channels};\n            device const half* wq = wr + kx * {};\n            for (int c = 0; c < {channels}; ++c) {{\n                acc += float(px[c]) * float4(*(device const half4*)(wq + c * {npad}));\n            }}\n        }}\n    }}",
            channels * npad
        );
    }
    if layer.head {
        let _ = writeln!(
            s,
            "    float4 v;\n    for (int ch = 0; ch < 4; ++ch) {{\n        v[ch] = {};\n    }}\n    head_pixel(x, y, v, p, luma, dst);\n}}",
            act_call(plan, index, "acc[ch]")
        );
    } else {
        let _ = writeln!(
            s,
            "    for (int e = 0; e < 4; ++e) {{\n        int ch = g + e;\n        if (ch < {}) {{\n            {};\n        }}\n    }}\n}}",
            layer.out_channels,
            store_call(plan, index, &act_call(plan, index, "acc[e]"))
        );
    }
}

/// Pixels per thread of the scalar block kernel.
fn block_pixels() -> usize {
    std::env::var("ERIKA_SR_BLOCK_PX")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| (1..=8).contains(value))
        .unwrap_or(2)
}

/// Output channels per thread of the scalar block kernel.
fn block_channels(layer: &PlannedLayer) -> usize {
    if layer.head {
        return 4;
    }
    let limit = std::env::var("ERIKA_SR_BLOCK_OG")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value: &usize| *value >= 4 && value.is_multiple_of(4))
        .unwrap_or(16);
    let mut og = layer.npad.min(limit);
    while !layer.npad.is_multiple_of(og) {
        og -= 4;
    }
    og
}

/// Main pass for GPUs without matrix units: each thread computes a few
/// horizontally adjacent pixels times up to sixteen output channels, so
/// every loaded input feeds many FMAs and every (uniform) weight vector
/// several pixels. Channels are loaded four at a time when the layout
/// allows. Border rings are fixed afterwards by the per-pixel scalar kernel.
fn emit_block_kernel(plan: &Plan, index: usize, s: &mut String) {
    let layer = &plan.layers[index];
    let npad = layer.npad;
    let og = block_channels(layer);
    let quads = og / 4;
    let pixels = block_pixels();
    // Half accumulation: double-rate FP16 on the GPUs this fallback serves
    // (A11-A13, AMD); the oracle tolerances hold (see tests/metal_sr.rs).
    let (a4, a1) = if std::env::var_os("ERIKA_SR_BLOCK_FLOAT").is_some() {
        ("float4", "float")
    } else {
        ("half4", "half")
    };
    emit_signature(plan, index, &format!("layer{index}_block"), s);
    s.push_str("    uint3 gid [[thread_position_in_grid]])\n{\n");
    let _ = writeln!(
        s,
        r#"    const int W = p.width;
    const int xb = int(gid.x) * {pixels};
    const int y = p.row_begin + int(gid.y);
    if (xb >= W || y >= p.row_end) {{
        return;
    }}
    const int g = int(gid.z) * {og};
    device const half* blk = wts + {base};
    {a4} acc[{pixels}][{quads}];
    UNROLL for (int q = 0; q < {quads}; ++q) {{
        {a4} bias = {a4}(*(device const half4*)(blk + {bias} + g + 4 * q));
        UNROLL for (int px = 0; px < {pixels}; ++px) {{
            acc[px][q] = bias;
        }}
    }}"#,
        base = variant_base(layer),
        bias = layer.bias_offset,
    );
    for input in &layer.inputs {
        let k = input.kernel;
        let radius = k / 2;
        let channels = plan.views[input.view].channels;
        let _ = writeln!(s, "    for (int ky = 0; ky < {}; ++ky) {{", input.rows);
        emit_input_row(plan, layer, input, s);
        let _ = writeln!(
            s,
            "        device const half* wr = blk + {} + ky * {} + g;\n        for (int kx = 0; kx < {k}; ++kx) {{\n            device const half* src = row + (xb + kx - {radius} + P) * {channels};\n            device const half* wq = wr + kx * {};",
            input.offset,
            input.kpad * npad,
            channels * npad,
        );
        if channels.is_multiple_of(4) {
            let _ = writeln!(
                s,
                r#"            for (int c = 0; c < {channels}; c += 4) {{
                {a4} v[{pixels}];
                UNROLL for (int px = 0; px < {pixels}; ++px) {{
                    v[px] = {a4}(*(device const half4*)(src + px * {channels} + c));
                }}
                UNROLL for (int i = 0; i < 4; ++i) {{
                    UNROLL for (int q = 0; q < {quads}; ++q) {{
                        {a4} w = {a4}(*(device const half4*)(wq + (c + i) * {npad} + 4 * q));
                        UNROLL for (int px = 0; px < {pixels}; ++px) {{
                            acc[px][q] += v[px][i] * w;
                        }}
                    }}
                }}
            }}
        }}
    }}"#
            );
        } else {
            let _ = writeln!(
                s,
                r#"            for (int c = 0; c < {channels}; ++c) {{
                {a1} v[{pixels}];
                UNROLL for (int px = 0; px < {pixels}; ++px) {{
                    v[px] = {a1}(src[px * {channels} + c]);
                }}
                UNROLL for (int q = 0; q < {quads}; ++q) {{
                    {a4} w = {a4}(*(device const half4*)(wq + c * {npad} + 4 * q));
                    UNROLL for (int px = 0; px < {pixels}; ++px) {{
                        acc[px][q] += v[px] * w;
                    }}
                }}
            }}
        }}
    }}"#
            );
        }
    }
    if layer.head {
        let _ = writeln!(
            s,
            r#"    for (int px = 0; px < {pixels}; ++px) {{
        const int x = xb + px;
        if (x >= W) {{
            break;
        }}
        float4 v;
        for (int ch = 0; ch < 4; ++ch) {{
            v[ch] = {act};
        }}
        head_pixel(x, y, v, p, luma, dst);
    }}
}}"#,
            act = act_call(plan, index, "float(acc[px][0][ch])"),
        );
    } else {
        let _ = writeln!(
            s,
            r#"    for (int px = 0; px < {pixels}; ++px) {{
        const int x = xb + px;
        if (x >= W) {{
            break;
        }}
        UNROLL for (int e = 0; e < {og}; ++e) {{
            const int ch = g + e;
            if (ch < {out_channels}) {{
                {call};
            }}
        }}
    }}
}}"#,
            out_channels = layer.out_channels,
            call = store_call(
                plan,
                index,
                &act_call(plan, index, "float(acc[px][e / 4][e % 4])")
            ),
        );
    }
}

/// Pixel grouping of a simdgroup kernel: only worth it for wide windows
/// (measured: ArtCNN's 5x5 head gains, 3x3 and 1x1 narrow layers lose).
fn simd_grouped(layer: &PlannedLayer) -> Option<&Grouped> {
    let group = simd_group(layer.out_channels);
    layer
        .grouped
        .iter()
        .find(|grouped| grouped.group == group)
        .filter(|_| layer.inputs.iter().all(|input| input.kernel >= 5))
}

fn tensor_grouped(layer: &PlannedLayer) -> Option<&Grouped> {
    let group = tensor_group(layer.out_channels);
    layer.grouped.iter().find(|grouped| grouped.group == group)
}

/// Pixel fragments per simdgroup and output blocks per kernel.
fn simdgroup_shape(layer: &PlannedLayer) -> (usize, usize) {
    let npad = simd_grouped(layer).map_or(layer.npad, |grouped| grouped.npad);
    let blocks = (npad / 8).min(8);
    let pxf = if blocks <= 4 { 8 } else { 4 };
    (pxf, blocks)
}

/// Output pixels covered by one simdgroup of `layer`.
fn simdgroup_span(layer: &PlannedLayer) -> usize {
    let (pxf, _) = simdgroup_shape(layer);
    pxf * 8 * simd_grouped(layer).map_or(1, |grouped| grouped.group)
}

fn emit_simdgroup_kernel(plan: &Plan, index: usize, absorb: bool, s: &mut String) {
    let layer = &plan.layers[index];
    let (pxf, blocks) = simdgroup_shape(layer);
    let grouped = simd_grouped(layer);
    let (group, npad) = grouped.map_or((1, layer.npad), |grouped| (grouped.group, grouped.npad));
    let span = simdgroup_span(layer);
    let terms: Vec<(usize, &AbsorbedTerm)> = plan
        .absorption
        .as_ref()
        .filter(|_| absorb)
        .map(|absorption| {
            absorption
                .terms
                .iter()
                .filter(|term| term.producer == index)
                .enumerate()
                .collect()
        })
        .unwrap_or_default();
    emit_signature(plan, index, &format!("layer{index}_simd"), s);
    for (slot, _) in &terms {
        let _ = writeln!(
            s,
            "    device half* hpart{slot} [[buffer({})]],",
            BUFFER_ACC + slot
        );
    }
    s.push_str(
        "    uint3 tg [[threadgroup_position_in_grid]],\n    ushort sid [[simdgroup_index_in_threadgroup]],\n    ushort lane [[thread_index_in_simdgroup]])\n{\n",
    );
    if layer.head {
        let _ = writeln!(s, "    threadgroup float head_tile[{SIMDS}][{}];", span * 4);
    }
    // Per-simdgroup staging of absorbed head terms (zeroed: K over-reads
    // meet zero weights but must be finite).
    for (slot, term) in &terms {
        let channels = plan.views[term.view].channels;
        let len = span * channels + 16;
        let _ = writeln!(
            s,
            "    threadgroup half hstage_all{slot}[{SIMDS}][{len}];\n    threadgroup half* hstage{slot} = hstage_all{slot}[sid];\n    for (int i = int(lane); i < {len}; i += 32) {{\n        hstage{slot}[i] = 0.0h;\n    }}"
        );
    }
    if !terms.is_empty() {
        s.push_str("    simdgroup_barrier(mem_flags::mem_threadgroup);\n");
    }
    let _ = writeln!(
        s,
        r#"    const int W = p.width;
    const int y = p.row_begin + int(tg.y) * {SIMDS} + int(sid);
    if (y >= p.row_end) {{
        return;
    }}
    const int x0 = int(tg.x) * {span};
    const int ob0 = int(tg.z) * {blocks};
    simdgroup_half8x8 acc[{blocks}][{pxf}];
    UNROLL for (int b = 0; b < {blocks}; ++b) {{
        UNROLL for (int f = 0; f < {pxf}; ++f) {{
            acc[b][f] = make_filled_simdgroup_matrix<half, 8, 8>(0.0h);
        }}
    }}"#,
    );
    for (input_index, input) in layer.inputs.iter().enumerate() {
        let k = input.kernel;
        let radius = k / 2;
        let channels = plan.views[input.view].channels;
        let (kpad, base) = match grouped {
            Some(grouped) => grouped.inputs[input_index],
            None => (input.kpad, variant_base(layer) + input.offset),
        };
        let fragments = ((k + group - 1) * channels).div_ceil(8);
        let _ = writeln!(s, "    for (int ky = 0; ky < {}; ++ky) {{", input.rows);
        emit_input_row(plan, layer, input, s);
        let _ = writeln!(
            s,
            r#"        device const half* a0 = row + (x0 - {radius} + P) * {channels};
        device const half* wr = wts + {base} + ky * {row_stride} + ob0 * 8;
        UNROLL for (int kf = 0; kf < {fragments}; ++kf) {{
            simdgroup_half8x8 w[{blocks}];
            UNROLL for (int b = 0; b < {blocks}; ++b) {{
                simdgroup_load(w[b], wr + kf * {kstep} + b * 8, {npad});
            }}
            UNROLL for (int f = 0; f < {pxf}; ++f) {{
                simdgroup_half8x8 a;
                simdgroup_load(a, a0 + f * {fstep} + kf * 8, {astride});
                UNROLL for (int b = 0; b < {blocks}; ++b) {{
                    simdgroup_multiply_accumulate(acc[b][f], a, w[b], acc[b][f]);
                }}
            }}
        }}
    }}"#,
            row_stride = kpad * npad,
            kstep = 8 * npad,
            fstep = 8 * group * channels,
            astride = group * channels,
        );
    }
    let bias = variant_base(layer) + layer.bias_offset;
    let out_channels = layer.out_channels;
    let value = act_call(
        plan,
        index,
        &format!("float(e[q]) + float(wts[{bias} + ch])"),
    );
    let stage_writes = absorbed_stage_writes(plan, index, &terms).replace("(x - xs)", "(x - x0)");
    let sink = if layer.head {
        format!("head_tile[sid][(x - x0) * 4 + ch] = {value}")
    } else if stage_writes.is_empty() {
        store_call(plan, index, &value)
    } else {
        format!(
            "float v = {value};\n                    {};\n{stage_writes}",
            store_call(plan, index, "v")
        )
    };
    let decode = if group > 1 {
        format!(
            "const int n = (ob0 + b) * 8 + fn + q;\n                const int ch = n % {out_channels};\n                const int x = x0 + (f * 8 + fm) * {group} + n / {out_channels};\n                if (n < {} && x < W) {{",
            group * out_channels
        )
    } else {
        format!(
            "const int ch = (ob0 + b) * 8 + fn + q;\n                const int x = x0 + f * 8 + fm;\n                if (ch < {out_channels} && x < W) {{"
        )
    };
    let _ = writeln!(
        s,
        r#"    const int qid = lane / 4;
    const int fm = (qid & 4) + ((lane / 2) % 4);
    const int fn = (qid & 2) * 2 + (lane % 2) * 2;
    UNROLL for (int f = 0; f < {pxf}; ++f) {{
        UNROLL for (int b = 0; b < {blocks}; ++b) {{
            thread auto& e = acc[b][f].thread_elements();
            UNROLL for (int q = 0; q < 2; ++q) {{
                {decode}
                    {sink};
                }}
            }}
        }}
    }}"#,
    );
    if !terms.is_empty() {
        emit_absorbed_terms_simd(plan, &terms, span, s);
    }
    if layer.head {
        let _ = writeln!(
            s,
            r#"    simdgroup_barrier(mem_flags::mem_threadgroup);
    for (int t = lane; t < {span}; t += 32) {{
        const int x = x0 + t;
        if (x < W) {{
            threadgroup const float* v = &head_tile[sid][t * 4];
            head_pixel(x, y, float4(v[0], v[1], v[2], v[3]), p, luma, dst);
        }}
    }}"#
        );
    }
    s.push_str("}\n");
}

/// Cooperative-tensor index components (channel, pixel) of a transposed op
/// (verified with a probe: index 0 runs along N, index 1 along M).
fn transposed_index() -> (usize, usize) {
    (1, 0)
}

/// Output pixels covered by one tensor-op threadgroup of `layer`.
fn tensor_span(layer: &PlannedLayer, transposed: bool) -> usize {
    let group = if transposed {
        1
    } else {
        tensor_grouped(layer).map_or(1, |grouped| grouped.group)
    };
    tensor_tile() * tensor_simds() * group
}

fn emit_tensor_kernel(plan: &Plan, index: usize, absorb: bool, transposed: bool, s: &mut String) {
    let layer = &plan.layers[index];
    let grouped = if transposed {
        None
    } else {
        tensor_grouped(layer)
    };
    let (group, npad) = grouped.map_or((1, layer.npad), |grouped| (grouped.group, grouped.npad));
    let span = tensor_span(layer, transposed);
    let tile = tensor_tile();
    let simds = tensor_simds();
    let terms: Vec<(usize, &AbsorbedTerm)> = plan
        .absorption
        .as_ref()
        .filter(|_| absorb)
        .map(|absorption| {
            absorption
                .terms
                .iter()
                .filter(|term| term.producer == index)
                .enumerate()
                .collect()
        })
        .unwrap_or_default();
    emit_signature(plan, index, &format!("layer{index}_tensor"), s);
    for (slot, _) in &terms {
        let _ = writeln!(
            s,
            "    device half* hpart{slot} [[buffer({})]],",
            BUFFER_ACC + slot
        );
    }
    s.push_str(
        "    uint3 tg [[threadgroup_position_in_grid]],\n    ushort tid [[thread_index_in_threadgroup]],\n    ushort sid [[simdgroup_index_in_threadgroup]])\n{\n",
    );
    if layer.head {
        let _ = writeln!(s, "    threadgroup float head_tile[{}];", span * 4);
    }
    for (slot, term) in &terms {
        // Padded so K over-reads past the last pixel stay inside; zeroed so
        // they are finite (they meet zero weights).
        let channels = plan.views[term.view].channels;
        let len = span * channels + term.kpad;
        let _ = writeln!(
            s,
            "    threadgroup half hstage{slot}[{len}];\n    for (int i = int(tid); i < {len}; i += {threads}) {{\n        hstage{slot}[i] = 0.0h;\n    }}",
            threads = 32 * simds,
        );
    }
    if !terms.is_empty() {
        s.push_str("    threadgroup_barrier(mem_flags::mem_threadgroup);\n");
    }
    // Each simdgroup runs its own `tile x N` op on consecutive pixels.
    let _ = writeln!(
        s,
        r#"    const int W = p.width;
    const int y = p.row_begin + int(tg.y);
    const int x0 = int(tg.x) * {span};
    const int xs = x0 + int(sid) * {sg_span};
    using TA = tensor<device half, dextents<int32_t, 2>, tensor_inline>;
    constexpr auto desc = {descriptor};
    matmul2d<desc, execution_simdgroups<1>> op;
    auto acc = op.get_destination_cooperative_tensor<TA, TA, ACC_T>();
    UNROLL for (uint16_t i = 0; i < acc.get_capacity(); ++i) {{
        if (acc.is_valid_element(i)) {{
            acc[i] = 0.0f;
        }}
    }}"#,
        sg_span = tile * group,
        descriptor = if transposed {
            format!(
                "matmul2d_descriptor({npad}, {tile}, dynamic_length_v<int>, true, true, false, matmul2d_descriptor::mode::multiply_accumulate)"
            )
        } else {
            format!(
                "matmul2d_descriptor({tile}, {npad}, dynamic_length_v<int>, false, false, false, matmul2d_descriptor::mode::multiply_accumulate)"
            )
        },
    );
    for (input_index, input) in layer.inputs.iter().enumerate() {
        let radius = input.kernel / 2;
        let channels = plan.views[input.view].channels;
        let (kpad, base) = match grouped {
            Some(grouped) => grouped.inputs[input_index],
            None => (input.kpad, variant_base(layer) + input.offset),
        };
        let _ = writeln!(s, "    for (int ky = 0; ky < {}; ++ky) {{", input.rows);
        emit_input_row(plan, layer, input, s);
        let _ = writeln!(
            s,
            r#"        TA a((device half*)(row + (xs - {radius} + P) * {channels}), dextents<int32_t, 2>({kpad}, {tile}), array<int, 2>{{1, {stride}}});
        TA w((device half*)(wts + {base} + ky * {row_stride}), dextents<int32_t, 2>({npad}, {kpad}), array<int, 2>{{1, {npad}}});
        {run}
    }}"#,
            run = if transposed {
                "op.run(w, a, acc);"
            } else {
                "op.run(a, w, acc);"
            },
            stride = group * channels,
            row_stride = kpad * npad,
        );
    }
    let bias = variant_base(layer) + layer.bias_offset;
    let out_channels = layer.out_channels;
    s.push_str("    bool stored = false;\n");
    let stage_writes = absorbed_stage_writes(plan, index, &terms);
    if let Some(fast) =
        tensor_fast_epilogue(plan, index, group, tile, bias, &stage_writes, transposed)
    {
        s.push_str(&fast);
    }
    let value = act_call(
        plan,
        index,
        &format!("float(acc[i]) + float(wts[{bias} + ch])"),
    );
    let sink = if layer.head {
        format!("head_tile[(x - x0) * 4 + ch] = {value}")
    } else if stage_writes.is_empty() {
        store_call(plan, index, &value)
    } else {
        format!(
            "float v = {value};\n            {};\n{stage_writes}",
            store_call(plan, index, "v")
        )
    };
    let (tc, tx) = transposed_index();
    let decode = if transposed {
        format!(
            "const int ch = idx[{tc}];\n        const int x = xs + idx[{tx}];\n        if (x < W && ch < {out_channels}) {{"
        )
    } else if group > 1 {
        format!(
            "const int n = idx[0];\n        const int ch = n % {out_channels};\n        const int x = xs + idx[1] * {group} + n / {out_channels};\n        if (n < {} && x < W) {{",
            group * out_channels
        )
    } else {
        format!(
            "const int ch = idx[0];\n        const int x = xs + idx[1];\n        if (x < W && ch < {out_channels}) {{"
        )
    };
    let _ = writeln!(
        s,
        r#"    if (!stored) {{
    UNROLL for (uint16_t i = 0; i < acc.get_capacity(); ++i) {{
        if (!acc.is_valid_element(i)) {{
            continue;
        }}
        auto idx = acc.get_multidimensional_index(i);
        {decode}
            {sink};
        }}
    }}
    }}"#,
    );
    if !terms.is_empty() {
        emit_absorbed_terms(plan, index, &terms, tile * group, group, s);
    }
    if layer.head {
        let _ = writeln!(
            s,
            r#"    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (int t = tid; t < {span}; t += {threads}) {{
        const int x = x0 + t;
        if (x < W) {{
            threadgroup const float* v = &head_tile[t * 4];
            head_pixel(x, y, float4(v[0], v[1], v[2], v[3]), p, luma, dst);
        }}
    }}"#,
            threads = 32 * simds,
        );
    }
    s.push_str("}\n");
}

/// Epilogue for tiles away from the left/right image edges: activation in
/// registers, then cooperative (vectorized) stores of whole tiles into the
/// output views; residuals arrive through a cooperative load. Edge tiles
/// keep the element-wise path, which also writes the border columns.
fn tensor_fast_epilogue(
    plan: &Plan,
    index: usize,
    group: usize,
    tile: usize,
    bias: usize,
    stage_writes: &str,
    transposed: bool,
) -> Option<String> {
    let layer = &plan.layers[index];
    // Cooperative stores into a destination whose innermost dimension is
    // strided (the transposed layout) produced wrong results: transposed
    // layers keep the element-wise epilogue.
    if layer.head
        || layer.writes.is_empty()
        || transposed
        || std::env::var_os("ERIKA_SR_NO_FAST").is_some()
    {
        return None;
    }
    let crelu = layer
        .writes
        .iter()
        .any(|&view| plan.views[view].kind == ViewKind::Crelu);
    if crelu && group > 1 {
        return None;
    }
    let out_channels = layer.out_channels;
    let columns = group * out_channels;
    let sg_span = tile * group;
    // Tensor geometry of a stored view tile with `stride` halves per pixel.
    let geometry = |stride: usize| {
        if transposed {
            format!("dextents<int32_t, 2>({tile}, {columns}), array<int, 2>{{{stride}, 1}}")
        } else {
            format!(
                "dextents<int32_t, 2>({columns}, {tile}), array<int, 2>{{1, {}}}",
                group * stride
            )
        }
    };
    let mut s = String::new();
    let _ = writeln!(
        s,
        "    if (xs >= 1 && xs + {sg_span} <= W - 1) {{\n        using TO = tensor<device half, dextents<int32_t, 2>, tensor_inline>;"
    );
    if let Some(residual) = layer.residual {
        let channels = plan.views[residual].channels;
        let row = view_row(plan, residual, "res", "y");
        let _ = writeln!(
            s,
            "        auto r = op.get_destination_cooperative_tensor<TA, TA, ACC_T>();\n        TO rt((device half*)({row} + (xs + P) * {channels}), {});\n        r.load(rt);",
            geometry(channels)
        );
    }
    // One cooperative tensor per stored transform.
    let mut stores: Vec<(String, String)> = Vec::new(); // (tensor name, value expression)
    let mut copies = String::new();
    for (slot, &view) in layer.writes.iter().enumerate() {
        let kind = plan.views[view].kind;
        let parts: Vec<(&str, usize)> = match kind {
            ViewKind::Raw => vec![("v", 0)],
            ViewKind::Pos => vec![("max(v, 0.0f)", 0)],
            ViewKind::Neg => vec![("max(-v, 0.0f)", 0)],
            ViewKind::Crelu => vec![("max(v, 0.0f)", 0), ("max(-v, 0.0f)", out_channels)],
            ViewKind::Stacked => return None,
        };
        let stored = plan.views[view].channels;
        for (part, (value, offset)) in parts.into_iter().enumerate() {
            let name = format!("o{slot}_{part}");
            let _ = writeln!(
                copies,
                "        auto {name} = op.get_destination_cooperative_tensor<TA, TA, ACC_T>();"
            );
            let row = view_row(plan, view, &format!("out{slot}"), "y");
            stores.push((
                name.clone(),
                format!(
                    "{{\n            TO ot((device half*)({row} + (xs + P) * {stored} + {offset}), {});\n            {name}.store(ot);\n        }}",
                    geometry(stored)
                ),
            ));
            let _ = writeln!(copies, "        #define VALUE_{name} ({value})");
        }
    }
    s.push_str(&copies);
    let residual = if layer.residual.is_some() {
        " + float(r[i])"
    } else {
        ""
    };
    let activation = match layer.activation {
        ActivationKind::None => String::new(),
        ActivationKind::Relu => "v = max(v, 0.0f);".to_string(),
        ActivationKind::Silu => "v = v / (1.0f + exp(-v));".to_string(),
        ActivationKind::Prelu => format!(
            "v = max(v, 0.0f) + float(wts[{} + ch]) * min(v, 0.0f);",
            layer.alpha_offset.expect("prelu slopes")
        ),
    };
    let assigns: String = stores
        .iter()
        .map(|(name, _)| format!("            {name}[i] = ACC_T(VALUE_{name});\n"))
        .collect();
    let _ = writeln!(
        s,
        r#"        UNROLL for (uint16_t i = 0; i < acc.get_capacity(); ++i) {{
            if (!acc.is_valid_element(i)) {{
                continue;
            }}
            auto idx = acc.get_multidimensional_index(i);
            const int ch = {ch_expr};
            float v = float(acc[i]) + float(wts[{bias} + ch]){residual};
            {activation}
{assigns}{stage}        }}"#,
        ch_expr = if transposed {
            format!("idx[{}]", transposed_index().0)
        } else {
            format!("idx[0] % {out_channels}")
        },
        stage = if stage_writes.is_empty() {
            String::new()
        } else if transposed {
            format!(
                "            const int x = xs + idx[{tx}];\n            if (idx[{tc}] < {out_channels}) {{\n{stage_writes}            }}\n",
                tc = transposed_index().0,
                tx = transposed_index().1
            )
        } else {
            format!(
                "            const int x = xs + idx[1] * {group} + idx[0] / {out_channels};\n            if (idx[0] < {columns}) {{\n{stage_writes}            }}\n"
            )
        },
    );
    for (name, store) in &stores {
        let _ = writeln!(s, "        {store}\n        #undef VALUE_{name}");
    }
    s.push_str("        stored = true;\n    }\n");
    Some(s)
}

/// Lines staging an element's activated value `v` (channel `ch`, pixel `x`)
/// into the threadgroup tiles of the producer's absorbed head terms.
fn absorbed_stage_writes(plan: &Plan, index: usize, terms: &[(usize, &AbsorbedTerm)]) -> String {
    let out_channels = plan.layers[index].out_channels;
    let mut writes = String::new();
    for (slot, term) in terms {
        let channels = plan.views[term.view].channels;
        let base = format!("hstage{slot}[(x - xs) * {channels}");
        match plan.views[term.view].kind {
            ViewKind::Raw => {
                let _ = writeln!(writes, "            {base} + ch] = half(v);");
            }
            ViewKind::Pos => {
                let _ = writeln!(writes, "            {base} + ch] = half(max(v, 0.0f));");
            }
            ViewKind::Neg => {
                let _ = writeln!(writes, "            {base} + ch] = half(max(-v, 0.0f));");
            }
            ViewKind::Crelu => {
                let _ = writeln!(
                    writes,
                    "            {base} + ch] = half(max(v, 0.0f));\n            {base} + {out_channels} + ch] = half(max(-v, 0.0f));"
                );
            }
            ViewKind::Stacked => unreachable!(),
        }
    }
    writes
}

/// Head contributions of the tile a producer just computed: the stored
/// views are staged in threadgroup memory and multiplied with the head's
/// 1x1 weights in 32-pixel chunks; each term writes its own partial.
fn emit_absorbed_terms(
    plan: &Plan,
    index: usize,
    terms: &[(usize, &AbsorbedTerm)],
    span: usize,
    group: usize,
    s: &mut String,
) {
    let absorption = plan.absorption.as_ref().expect("absorbing");
    let head = &plan.layers[absorption.head];
    let layer = &plan.layers[index];
    let npad = head.npad;
    let _ = (layer, group);
    s.push_str("    threadgroup_barrier(mem_flags::mem_threadgroup);\n");
    for (slot, term) in terms {
        let channels = plan.views[term.view].channels;
        let partial_row = view_row(plan, term.partial, &format!("hpart{slot}"), "y");
        let _ = writeln!(
            s,
            r#"    for (int chunk = 0; chunk < {chunks}; ++chunk) {{
        const int cx = xs + chunk * 32;
        const int valid = min(32, W - cx);
        if (valid <= 0) {{
            break;
        }}
        constexpr auto hdesc = matmul2d_descriptor(32, {npad}, dynamic_length_v<int>, false, false, false, matmul2d_descriptor::mode::multiply);
        matmul2d<hdesc, execution_simdgroups<1>> hop;
        using TT = tensor<threadgroup half, dextents<int32_t, 2>, tensor_inline>;
        using TO = tensor<device half, dextents<int32_t, 2>, tensor_inline>;
        auto hsum = hop.get_destination_cooperative_tensor<TT, TA, ACC_T>();
        TT ha((threadgroup half*)(hstage{slot} + chunk * 32 * {channels}), dextents<int32_t, 2>({kpad}, 32), array<int, 2>{{1, {channels}}});
        TA hw((device half*)(wts + {weights}), dextents<int32_t, 2>({npad}, {kpad}), array<int, 2>{{1, {npad}}});
        hop.run(ha, hw, hsum);
        TO pt((device half*)({partial_row} + (cx + P) * 4), dextents<int32_t, 2>(4, valid), array<int, 2>{{1, 4}});
        hsum.store(pt);
    }}"#,
            chunks = span.div_ceil(32),
            kpad = term.kpad,
            weights = term.weights,
        );
    }
}

/// `simdgroup_matrix` version of [`emit_absorbed_terms`]: 8-pixel fragments
/// of the staged tile times the head weights; channels below four written.
fn emit_absorbed_terms_simd(
    plan: &Plan,
    terms: &[(usize, &AbsorbedTerm)],
    span: usize,
    s: &mut String,
) {
    let absorption = plan.absorption.as_ref().expect("absorbing");
    let npad = plan.layers[absorption.head].npad;
    s.push_str("    simdgroup_barrier(mem_flags::mem_threadgroup);\n");
    for (slot, term) in terms {
        let channels = plan.views[term.view].channels;
        let partial_row = view_row(plan, term.partial, &format!("hpart{slot}"), "y");
        let _ = writeln!(
            s,
            r#"    UNROLL for (int f = 0; f < {fragments}; ++f) {{
        simdgroup_half8x8 hsum = make_filled_simdgroup_matrix<half, 8, 8>(0.0h);
        UNROLL for (int kf = 0; kf < {kfrags}; ++kf) {{
            simdgroup_half8x8 a;
            simdgroup_half8x8 w;
            simdgroup_load(a, hstage{slot} + f * {fstep} + kf * 8, {channels});
            simdgroup_load(w, wts + {weights} + kf * {kstep}, {npad});
            simdgroup_multiply_accumulate(hsum, a, w, hsum);
        }}
        thread auto& e = hsum.thread_elements();
        const int x = x0 + f * 8 + fm;
        UNROLL for (int q = 0; q < 2; ++q) {{
            const int ch = fn + q;
            if (ch < 4 && x < W) {{
                ({partial_row})[(x + P) * 4 + ch] = e[q];
            }}
        }}
    }}"#,
            fragments = span / 8,
            kfrags = channels.div_ceil(8),
            fstep = 8 * channels,
            kstep = 8 * npad,
            weights = term.weights,
        );
    }
}

/// The absorbed head: sum of the partials + bias, activation, 2x output.
fn emit_absorbed_head(plan: &Plan, index: usize, s: &mut String) {
    let absorption = plan.absorption.as_ref().expect("absorbing");
    let layer = &plan.layers[index];
    let bias = variant_base(layer) + layer.bias_offset;
    let _ = writeln!(s, "kernel void layer{index}_absorbed(");
    for (slot, _) in absorption.terms.iter().enumerate() {
        let _ = writeln!(s, "    device const half* hpart{slot} [[buffer({slot})]],");
    }
    let mut sum = String::new();
    for (slot, term) in absorption.terms.iter().enumerate() {
        let row = view_row(plan, term.partial, &format!("hpart{slot}"), "y");
        let _ = writeln!(
            sum,
            "    v += float4(*(device const half4*)({row} + (x + P) * 4));"
        );
    }
    let _ = writeln!(
        s,
        r#"    device const half* wts [[buffer({BUFFER_WEIGHTS})]],
    constant BandParams& p [[buffer({BUFFER_PARAMS})]],
    texture2d<float, access::read> luma [[texture(0)]],
    texture2d<float, access::write> dst [[texture(1)]],
    uint2 gid [[thread_position_in_grid]])
{{
    const int x = int(gid.x);
    const int y = p.row_begin + int(gid.y);
    if (x >= p.width || y >= p.row_end) {{
        return;
    }}
    float4 v = float4(*(device const half4*)(wts + {bias}));
{sum}    for (int ch = 0; ch < 4; ++ch) {{
        v[ch] = {act};
    }}
    head_pixel(x, y, v, p, luma, dst);
}}"#,
        act = act_call(plan, index, "v[ch]"),
    );
}

fn emit_input_kernel(plan: &Plan, s: &mut String) {
    s.push_str("kernel void stage_input(\n");
    for (slot, _) in plan.input_views.iter().enumerate() {
        let _ = writeln!(
            s,
            "    device half* out{slot} [[buffer({})]],",
            BUFFER_OUT0 + slot
        );
    }
    let _ = writeln!(
        s,
        "    constant BandParams& p [[buffer({BUFFER_PARAMS})]],\n    texture2d<float, access::read> luma [[texture(0)]],\n    uint2 gid [[thread_position_in_grid]])\n{{"
    );
    let zero = plan.padding == Padding::Zero;
    let _ = writeln!(
        s,
        r#"    const int x = int(gid.x) - P;
    const int y = p.row_begin + int(gid.y);
    if (x >= p.width + P || y >= p.row_end) {{
        return;
    }}
    const bool outside_x = x < 0 || x >= p.width;
    const int sx = clamp(x, 0, p.width - 1);"#
    );
    for (slot, &view) in plan.input_views.iter().enumerate() {
        let row = view_row(plan, view, &format!("out{slot}"), "y");
        let stride = plan.views[view].channels;
        if plan.views[view].kind == ViewKind::Stacked {
            let _ = writeln!(
                s,
                r#"    for (int c = 0; c < {stride}; ++c) {{
        int sy = y + c - {radius};
        bool outside = outside_x || sy < 0 || sy >= p.height;
        float v = (luma.read(uint2(sx, clamp(sy, 0, p.height - 1))).r - p.in_offset) * p.in_scale;
        if (({zero} && outside) || c >= {rows}) {{
            v = 0.0f;
        }}
        {row}[(x + P) * {stride} + c] = half(v);
    }}"#,
                rows = plan.views[view].stack,
                radius = plan.views[view].stack / 2,
            );
            continue;
        }
        let _ = writeln!(
            s,
            "    {{\n        float v = (luma.read(uint2(sx, y)).r - p.in_offset) * p.in_scale;\n        if ({zero} && outside_x) {{\n            v = 0.0f;\n        }}"
        );
        match plan.views[view].kind {
            ViewKind::Raw => {
                let _ = writeln!(s, "        {row}[(x + P) * {stride}] = half(v);");
            }
            ViewKind::Pos => {
                let _ = writeln!(s, "        {row}[(x + P) * {stride}] = half(max(v, 0.0f));");
            }
            ViewKind::Neg => {
                let _ = writeln!(
                    s,
                    "        {row}[(x + P) * {stride}] = half(max(-v, 0.0f));"
                );
            }
            ViewKind::Crelu => {
                let _ = writeln!(
                    s,
                    "        {row}[(x + P) * 2] = half(max(v, 0.0f));\n        {row}[(x + P) * 2 + 1] = half(max(-v, 0.0f));"
                );
            }
            ViewKind::Stacked => unreachable!(),
        }
        s.push_str("    }\n");
    }
    s.push_str("}\n");
}

fn emit_head_kernel(plan: &Plan, s: &mut String) {
    let Some(view) = plan.head_view else {
        return;
    };
    let channels = plan.views[view].channels;
    let _ = writeln!(
        s,
        "kernel void head_out(\n    device const half* in0 [[buffer(0)]],\n    constant BandParams& p [[buffer({BUFFER_PARAMS})]],\n    texture2d<float, access::read> luma [[texture(0)]],\n    texture2d<float, access::write> dst [[texture(1)]],\n    uint2 gid [[thread_position_in_grid]])\n{{\n    const int x = int(gid.x);\n    const int y = p.row_begin + int(gid.y);\n    if (x >= p.width || y >= p.row_end) {{\n        return;\n    }}\n    device const half* px = {} + (x + P) * {channels};\n    head_pixel(x, y, float4(px[0], px[1], px[2], px[3]), p, luma, dst);\n}}",
        view_row(plan, view, "in0", "y")
    );
}

/// Kernel family of each layer's main pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerFamily {
    Scalar,
    Simdgroup,
    Tensor,
    /// Tensor ops with channels as the GEMM rows and pixels as columns
    /// (`W^T x A^T`): narrow ungrouped layers (9-12 outputs padded to 16)
    /// waste less of the matrix units this way.
    TensorTransposed,
}

fn layer_families(plan: &Plan, kind: EngineKind) -> Vec<LayerFamily> {
    let min_outputs = std::env::var("ERIKA_SR_TENSOR_MIN_OUTPUTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(TENSOR_MIN_OUTPUTS);
    plan.layers
        .iter()
        .map(|layer| match kind {
            EngineKind::Scalar => LayerFamily::Scalar,
            EngineKind::Simdgroup => LayerFamily::Simdgroup,
            // Transposed ops need element-wise epilogues (cooperative stores
            // into pixel-major destinations are not supported), which only
            // pays off for the luma layer (measured: Anime4K UL layer 0
            // 0.60 -> 0.44 ms, inner layers 1.25 -> 1.40 ms).
            EngineKind::TensorOps
                if tensor_grouped(layer).is_none()
                    && layer.npad == 16
                    && layer.out_channels <= 12
                    && !layer.head
                    && layer
                        .inputs
                        .iter()
                        .all(|input| plan.views[input.view].kind == ViewKind::Stacked)
                    && std::env::var_os("ERIKA_SR_NO_TRANSPOSE").is_none() =>
            {
                LayerFamily::TensorTransposed
            }
            EngineKind::TensorOps
                if layer.npad >= min_outputs || tensor_grouped(layer).is_some() =>
            {
                LayerFamily::Tensor
            }
            EngineKind::TensorOps => LayerFamily::Simdgroup,
        })
        .collect()
}

/// Complete Metal source for `plan` with the given main-pass families.
fn generate_source(plan: &Plan, families: &[LayerFamily], absorb: bool) -> String {
    let tensor_ops = families
        .iter()
        .any(|family| matches!(family, LayerFamily::Tensor | LayerFamily::TensorTransposed));
    let simdgroup = families.contains(&LayerFamily::Simdgroup);
    let mut s = String::new();
    emit_prelude(plan, &mut s, tensor_ops, simdgroup);
    emit_input_kernel(plan, &mut s);
    emit_head_kernel(plan, &mut s);
    for (index, family) in families.iter().enumerate() {
        emit_epilogue(plan, index, &mut s);
        emit_scalar_kernel(plan, index, &mut s);
        match family {
            LayerFamily::Scalar => emit_block_kernel(plan, index, &mut s),
            LayerFamily::Simdgroup
                if absorb && plan.absorption.as_ref().is_some_and(|a| a.head == index) =>
            {
                emit_absorbed_head(plan, index, &mut s)
            }
            LayerFamily::Simdgroup => emit_simdgroup_kernel(plan, index, absorb, &mut s),
            LayerFamily::Tensor
                if absorb && plan.absorption.as_ref().is_some_and(|a| a.head == index) =>
            {
                emit_absorbed_head(plan, index, &mut s)
            }
            LayerFamily::Tensor => emit_tensor_kernel(plan, index, absorb, false, &mut s),
            LayerFamily::TensorTransposed => emit_tensor_kernel(plan, index, absorb, true, &mut s),
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Resources and encoding

#[repr(C)]
#[derive(Clone)]
struct BandParams {
    width: i32,
    height: i32,
    row_begin: i32,
    row_end: i32,
    ring_count: i32,
    ring_top: i32,
    ring_bottom: i32,
    reserved: i32,
    in_offset: f32,
    in_scale: f32,
    inv_scale: f32,
    reserved1: f32,
    origin: Vec<i32>,
}

impl BandParams {
    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(48 + 4 * self.origin.len());
        for value in [
            self.width,
            self.height,
            self.row_begin,
            self.row_end,
            self.ring_count,
            self.ring_top,
            self.ring_bottom,
            self.reserved,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in [
            self.in_offset,
            self.in_scale,
            self.inv_scale,
            self.reserved1,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in &self.origin {
            out.extend_from_slice(&value.to_le_bytes());
        }
        if self.origin.is_empty() {
            out.extend_from_slice(&0i32.to_le_bytes());
        }
        out
    }
}

type Pipeline = Retained<ProtocolObject<dyn MTLComputePipelineState>>;

struct LayerPipelines {
    family: LayerFamily,
    scalar: Pipeline,
    matrix: Option<Pipeline>,
}

struct Pool {
    width: usize,
    height: usize,
    output_format: MTLPixelFormat,
    slots: Vec<Retained<ProtocolObject<dyn MTLBuffer>>>,
    output: Retained<ProtocolObject<dyn MTLTexture>>,
    cleared: bool,
}

/// A compiled network ready to encode.
pub struct NetworkEngine {
    model: Arc<SrModel>,
    kind: EngineKind,
    plan: Plan,
    weights: Retained<ProtocolObject<dyn MTLBuffer>>,
    input_pipeline: Pipeline,
    head_pipeline: Option<Pipeline>,
    layers: Vec<LayerPipelines>,
    pool: Option<Pool>,
    /// Why the preferred kernel family was not used, if it was not.
    downgrade: Option<String>,
    max_band_rows: Option<usize>,
    band_budget: usize,
    /// Whether the dense head is accumulated by its producers.
    absorb: bool,
}

// SAFETY: Metal devices, libraries, pipelines and buffers are thread-safe;
// the engine is built on a worker and then only used by the render thread.
unsafe impl Send for NetworkEngine {}

fn compile(
    device: &ProtocolObject<dyn MTLDevice>,
    source: &str,
    tensor_ops: bool,
) -> Result<Retained<ProtocolObject<dyn objc2_metal::MTLLibrary>>> {
    let options = MTLCompileOptions::new();
    if tensor_ops {
        options.setLanguageVersion(MTLLanguageVersion::Version4_0);
    }
    device
        .newLibraryWithSource_options_error(&NSString::from_str(source), Some(&options))
        .map_err(|error| {
            PlayerError::Renderer(format!(
                "Metal network shader compile failed: {}",
                error.localizedDescription()
            ))
        })
}

fn pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn objc2_metal::MTLLibrary>,
    name: &str,
) -> Result<Pipeline> {
    let function = library
        .newFunctionWithName(&NSString::from_str(name))
        .ok_or_else(|| PlayerError::Renderer(format!("Metal shader missing {name}")))?;
    device
        .newComputePipelineStateWithFunction_error(&function)
        .map_err(|error| PlayerError::Renderer(format!("compute pipeline {name} failed: {error}")))
}

impl NetworkEngine {
    /// Plans and compiles `model` for `kind`, falling back to the next
    /// simpler kernel family when compilation fails.
    pub fn build(
        device: &ProtocolObject<dyn MTLDevice>,
        model: Arc<SrModel>,
        kind: EngineKind,
    ) -> Result<Self> {
        let plan = Plan::new(&model)?;
        let mut downgrade = None;
        let mut attempt = kind;
        let (library, kind, families, absorb) = loop {
            let families = layer_families(&plan, attempt);
            let tensor = |family: LayerFamily| {
                matches!(family, LayerFamily::Tensor | LayerFamily::TensorTransposed)
            };
            let simd_ok = |producer: usize| {
                families[producer] == LayerFamily::Simdgroup
                    && simd_grouped(&plan.layers[producer]).is_none()
                    && plan.layers[producer].npad <= 64
            };
            let absorb = plan.absorption.as_ref().is_some_and(|absorption| {
                (families[absorption.head] == LayerFamily::Tensor
                    && absorption
                        .terms
                        .iter()
                        .all(|term| tensor(families[term.producer])))
                    || (families[absorption.head] == LayerFamily::Simdgroup
                        && absorption.terms.iter().all(|term| simd_ok(term.producer)))
            });
            let mut source = generate_source(&plan, &families, absorb);
            if let Ok(nonce) = std::env::var("ERIKA_SR_SHADER_NONCE") {
                // Defeats Metal's shader cache when measuring compile time.
                let _ = writeln!(source, "// nonce {nonce}");
            }
            if std::env::var_os("ERIKA_SR_DUMP_SHADER").is_some() {
                let _ = std::fs::write(
                    std::env::temp_dir().join(format!("erika_sr_{}.metal", attempt.label())),
                    &source,
                );
            }
            let tensor_ops = families.iter().any(|family| {
                matches!(family, LayerFamily::Tensor | LayerFamily::TensorTransposed)
            });
            match compile(device, &source, tensor_ops) {
                Ok(library) => break (library, attempt, families, absorb),
                Err(error) => {
                    let next = match attempt {
                        EngineKind::TensorOps => EngineKind::Simdgroup,
                        EngineKind::Simdgroup => EngineKind::Scalar,
                        EngineKind::Scalar => return Err(error),
                    };
                    crate::trace::diagnostic_at(
                        crate::trace::LogLevel::Warn,
                        format!(
                            "Erika network upscaler: {} kernels unavailable ({error}); using {}",
                            attempt.label(),
                            next.label()
                        ),
                    );
                    downgrade = Some(format!("{} unavailable: {error}", attempt.label()));
                    attempt = next;
                }
            }
        };
        let mut layers = Vec::with_capacity(plan.layers.len());
        for (index, &family) in families.iter().enumerate() {
            let scalar = pipeline(device, &library, &format!("layer{index}_scalar"))?;
            let matrix = match family {
                LayerFamily::Scalar => {
                    Some(pipeline(device, &library, &format!("layer{index}_block"))?)
                }
                LayerFamily::Simdgroup
                    if absorb && plan.absorption.as_ref().is_some_and(|a| a.head == index) =>
                {
                    Some(pipeline(
                        device,
                        &library,
                        &format!("layer{index}_absorbed"),
                    )?)
                }
                LayerFamily::Simdgroup => {
                    Some(pipeline(device, &library, &format!("layer{index}_simd"))?)
                }
                LayerFamily::Tensor
                    if absorb && plan.absorption.as_ref().is_some_and(|a| a.head == index) =>
                {
                    Some(pipeline(
                        device,
                        &library,
                        &format!("layer{index}_absorbed"),
                    )?)
                }
                LayerFamily::Tensor | LayerFamily::TensorTransposed => {
                    Some(pipeline(device, &library, &format!("layer{index}_tensor"))?)
                }
            };
            layers.push(LayerPipelines {
                family,
                scalar,
                matrix,
            });
        }
        // `recommendedMaxWorkingSetSize` is iOS/tvOS 16+.
        use objc2::runtime::NSObjectProtocol;
        let working_set = if device.respondsToSelector(objc2::sel!(recommendedMaxWorkingSetSize)) {
            device.recommendedMaxWorkingSetSize()
        } else {
            0
        };
        let band_budget = match env_usize("ERIKA_SR_BAND_BUDGET_MB", 0) {
            0 => usize::try_from(working_set / BAND_BUDGET_SHARE)
                .unwrap_or(usize::MAX)
                .clamp(MIN_BAND_BUDGET, MAX_BAND_BUDGET),
            megabytes => megabytes << 20,
        };
        let input_pipeline = pipeline(device, &library, "stage_input")?;
        let head_pipeline = plan
            .head_view
            .is_some()
            .then(|| pipeline(device, &library, "head_out"))
            .transpose()?;
        let weights = unsafe {
            device.newBufferWithBytes_length_options(
                NonNull::new(plan.weights.as_ptr().cast::<c_void>().cast_mut())
                    .expect("weights pointer is non-null"),
                plan.weights.len() * 2,
                MTLResourceOptions::StorageModeShared,
            )
        }
        .ok_or_else(|| PlayerError::Renderer("network weight buffer alloc failed".to_string()))?;
        Ok(Self {
            model,
            kind,
            plan,
            weights,
            input_pipeline,
            head_pipeline,
            layers,
            pool: None,
            downgrade,
            max_band_rows: None,
            band_budget,
            absorb,
        })
    }

    pub fn kind(&self) -> EngineKind {
        self.kind
    }

    pub fn model(&self) -> &Arc<SrModel> {
        &self.model
    }

    pub fn downgrade_reason(&self) -> Option<&str> {
        self.downgrade.as_deref()
    }

    pub fn padded_macs_per_pixel(&self) -> u64 {
        self.plan.padded_macs_per_pixel()
    }

    pub fn allocated_bytes(&self) -> u64 {
        let pool = self.pool.as_ref().map_or(0u64, |pool| {
            pool.slots
                .iter()
                .fold(pool.output.allocatedSize() as u64, |total, buffer| {
                    total.saturating_add(buffer.allocatedSize() as u64)
                })
        });
        (self.weights.allocatedSize() as u64).saturating_add(pool)
    }

    /// Caps the rows per band (tests exercise banding on small frames).
    pub fn set_max_band_rows(&mut self, rows: Option<usize>) {
        if self.max_band_rows != rows {
            self.max_band_rows = rows;
            self.pool = None;
        }
    }

    fn band_rows(&self, width: usize, height: usize) -> usize {
        let rows = self.plan.band_rows(width, height, self.band_budget);
        let cap = self.max_band_rows.or_else(|| {
            env_usize("ERIKA_SR_BAND_ROWS", 0)
                .checked_sub(0)
                .filter(|&v| v > 0)
        });
        cap.map_or(rows, |cap| rows.min(cap.max(1)))
    }

    /// Number of bands a `width x height` frame is processed in.
    pub fn band_count(&self, width: usize, height: usize) -> usize {
        height.div_ceil(self.band_rows(width, height))
    }

    fn ensure_pool(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        width: usize,
        height: usize,
        output_format: MTLPixelFormat,
    ) -> Result<()> {
        if let Some(pool) = self.pool.as_ref()
            && pool.width == width
            && pool.height == height
            && pool.output_format == output_format
        {
            return Ok(());
        }
        self.pool = None;
        let band_rows = self.band_rows(width, height);
        let rows = (band_rows + 2 * self.plan.max_halo).min(height);
        let mut slots = Vec::with_capacity(self.plan.slot_channels.len());
        for slot in 0..self.plan.slot_channels.len() {
            let bytes = self.plan.slot_halves(slot, width, rows) * 2;
            slots.push(
                device
                    .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModePrivate)
                    .ok_or_else(|| {
                        PlayerError::Renderer("network feature buffer alloc failed".to_string())
                    })?,
            );
        }
        let descriptor = unsafe {
            let descriptor =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    output_format,
                    width * 2,
                    height * 2,
                    false,
                );
            descriptor.setStorageMode(MTLStorageMode::Private);
            descriptor.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
            descriptor
        };
        let output = device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| {
                PlayerError::Renderer("upscaled luma texture alloc failed".to_string())
            })?;
        self.pool = Some(Pool {
            width,
            height,
            output_format,
            slots,
            output,
            cleared: false,
        });
        Ok(())
    }

    /// Encodes the 2x upscale of `luma` and returns the output texture
    /// (`2w x 2h`, `output_format`).
    pub fn encode(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        luma: &ProtocolObject<dyn MTLTexture>,
        output_format: MTLPixelFormat,
        range: LumaRange,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        let width = luma.width();
        let height = luma.height();
        self.ensure_pool(device, width, height, output_format)?;
        let pool = self.pool.as_mut().expect("pool built above");
        if !pool.cleared {
            // Over-reads past the written region (K padding, partial tiles)
            // must see finite values; they are multiplied by zero weights.
            let Some(blit) = command_buffer.blitCommandEncoder() else {
                return Err(PlayerError::Renderer(
                    "blitCommandEncoder returned nil".to_string(),
                ));
            };
            for buffer in &pool.slots {
                blit.fillBuffer_range_value(buffer, NSRange::new(0, buffer.length()), 0);
            }
            blit.endEncoding();
            pool.cleared = true;
        }
        let steps = self.steps(width, height, range);
        let pool = self.pool.as_ref().expect("pool built above");
        let Some(encoder) = command_buffer.computeCommandEncoder() else {
            return Err(PlayerError::Renderer(
                "computeCommandEncoder returned nil".to_string(),
            ));
        };
        encoder.setLabel(Some(&NSString::from_str("erika_sr_network")));
        self.bind_common(&encoder, luma, pool);
        for step in &steps {
            self.run_step(&encoder, pool, step);
        }
        encoder.endEncoding();
        Ok(pool.output.clone())
    }

    /// Runs every dispatch of one frame in its own command buffer and returns
    /// `(label, GPU milliseconds)` per dispatch. Benchmarking aid.
    pub fn profile(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        queue: &ProtocolObject<dyn MTLCommandQueue>,
        luma: &ProtocolObject<dyn MTLTexture>,
        output_format: MTLPixelFormat,
        range: LumaRange,
    ) -> Result<Vec<(String, f64)>> {
        // Warm up and clear the pool.
        let command_buffer = queue
            .commandBuffer()
            .ok_or_else(|| PlayerError::Renderer("commandBuffer returned nil".into()))?;
        self.encode(device, &command_buffer, luma, output_format, range)?;
        command_buffer.commit();
        command_buffer.waitUntilCompleted();
        let steps = self.steps(luma.width(), luma.height(), range);
        let pool = self.pool.as_ref().expect("pool built by encode");
        let mut out: Vec<(String, f64)> = Vec::new();
        for step in &steps {
            // Best of three batches of back-to-back repeats (steps are
            // idempotent): sustained clocks, robust to other GPU clients.
            const REPEAT: usize = 10;
            let mut ms = f64::MAX;
            for _ in 0..3 {
                let command_buffer = queue
                    .commandBuffer()
                    .ok_or_else(|| PlayerError::Renderer("commandBuffer returned nil".into()))?;
                let encoder = command_buffer
                    .computeCommandEncoder()
                    .ok_or_else(|| PlayerError::Renderer("computeCommandEncoder nil".into()))?;
                self.bind_common(&encoder, luma, pool);
                for _ in 0..REPEAT {
                    self.run_step(&encoder, pool, step);
                }
                encoder.endEncoding();
                command_buffer.commit();
                command_buffer.waitUntilCompleted();
                ms = ms.min(
                    (command_buffer.GPUEndTime() - command_buffer.GPUStartTime()) * 1e3
                        / REPEAT as f64,
                );
            }
            if std::env::var_os("ERIKA_SR_PROFILE_STEPS").is_some() {
                eprintln!(
                    "  step {} rows {}..{} grid {:?}: {ms:.3} ms",
                    step.label,
                    step.params.row_begin,
                    step.params.row_end,
                    match step.grid {
                        Grid::Threads(w, h, d) => (w, h, d),
                        Grid::Groups(g, _) => (g.width, g.height, g.depth),
                    }
                );
            }
            match out.iter_mut().find(|(label, _)| *label == step.label) {
                Some((_, total)) => *total += ms,
                None => out.push((step.label.clone(), ms)),
            }
        }
        Ok(out)
    }

    fn bind_common(
        &self,
        encoder: &ProtocolObject<dyn MTLComputeCommandEncoder>,
        luma: &ProtocolObject<dyn MTLTexture>,
        pool: &Pool,
    ) {
        unsafe {
            encoder.setBuffer_offset_atIndex(Some(&self.weights), 0, BUFFER_WEIGHTS);
            encoder.setTexture_atIndex(Some(luma), 0);
            encoder.setTexture_atIndex(Some(&pool.output), 1);
        }
    }

    fn run_step(
        &self,
        encoder: &ProtocolObject<dyn MTLComputeCommandEncoder>,
        pool: &Pool,
        step: &Step,
    ) {
        let pipeline = match step.pipeline {
            StepPipeline::Input => &self.input_pipeline,
            StepPipeline::Head => self.head_pipeline.as_ref().expect("head pipeline"),
            StepPipeline::Scalar(index) => &self.layers[index].scalar,
            StepPipeline::Matrix(index) => {
                self.layers[index].matrix.as_ref().expect("matrix pipeline")
            }
        };
        encoder.setComputePipelineState(pipeline);
        for &(index, slot) in &step.buffers {
            unsafe { encoder.setBuffer_offset_atIndex(Some(&pool.slots[slot]), 0, index) };
        }
        set_params(encoder, &step.params);
        match step.grid {
            Grid::Threads(width, height, depth) => dispatch_threads(encoder, width, height, depth),
            Grid::Groups(groups, threads) => {
                encoder.dispatchThreadgroups_threadsPerThreadgroup(groups, threads)
            }
        }
    }

    /// The dispatches of one frame, in order.
    fn steps(&self, width: usize, height: usize, range: LumaRange) -> Vec<Step> {
        let plan = &self.plan;
        let band_rows = self.band_rows(width, height);
        let (offset, scale) = input_normalization(&self.model, range);
        let rows_for = |tensor: usize, y0: usize, y1: usize| -> (usize, usize) {
            let halo = plan.halos[tensor];
            (y0.saturating_sub(halo), (y1 + halo).min(height))
        };
        let slot_of = |view: usize| plan.views[view].slot;
        let mut steps = Vec::new();
        let mut y0 = 0;
        while y0 < height {
            let y1 = (y0 + band_rows).min(height);
            let origin: Vec<i32> = plan
                .views
                .iter()
                .map(|view| rows_for(view.tensor, y0, y1).0 as i32)
                .collect();
            let base = BandParams {
                width: width as i32,
                height: height as i32,
                row_begin: 0,
                row_end: 0,
                ring_count: 0,
                ring_top: 0,
                ring_bottom: 0,
                reserved: 0,
                in_offset: offset,
                in_scale: scale,
                inv_scale: 1.0 / scale,
                reserved1: 0.0,
                origin,
            };

            let (a, b) = rows_for(INPUT_TENSOR, y0, y1);
            steps.push(Step {
                label: "input".to_string(),
                pipeline: StepPipeline::Input,
                buffers: plan
                    .input_views
                    .iter()
                    .enumerate()
                    .map(|(slot, &view)| (BUFFER_OUT0 + slot, slot_of(view)))
                    .collect(),
                params: BandParams {
                    row_begin: a as i32,
                    row_end: b as i32,
                    ..base.clone()
                },
                grid: Grid::Threads(width + 2 * plan.pad, b - a, 1),
            });

            for (index, layer) in plan.layers.iter().enumerate() {
                let (a, b) = if layer.head {
                    (y0, y1)
                } else {
                    rows_for(layer.output, y0, y1)
                };
                if a >= b {
                    continue;
                }
                let mut buffers: Vec<(usize, usize)> = layer
                    .bindings
                    .iter()
                    .enumerate()
                    .map(|(binding, &view)| (binding, slot_of(view)))
                    .collect();
                if let Some(view) = layer.residual {
                    buffers.push((BUFFER_RESIDUAL, slot_of(view)));
                }
                for (slot, &view) in layer.writes.iter().enumerate() {
                    buffers.push((BUFFER_OUT0 + slot, slot_of(view)));
                }
                let params = BandParams {
                    row_begin: a as i32,
                    row_end: b as i32,
                    ..base.clone()
                };
                if let Some(absorption) = plan.absorption.as_ref().filter(|_| self.absorb) {
                    if absorption.head == index {
                        steps.push(Step {
                            label: format!("layer{index}"),
                            pipeline: StepPipeline::Matrix(index),
                            buffers: absorption
                                .terms
                                .iter()
                                .enumerate()
                                .map(|(slot, term)| (slot, slot_of(term.partial)))
                                .collect(),
                            params,
                            grid: Grid::Threads(width, b - a, 1),
                        });
                        continue;
                    }
                    for (slot, term) in absorption
                        .terms
                        .iter()
                        .filter(|term| term.producer == index)
                        .enumerate()
                    {
                        buffers.push((BUFFER_ACC + slot, slot_of(term.partial)));
                    }
                }
                // The head's four channels form one group (one 2x2 block).
                let groups = if layer.head {
                    1
                } else {
                    layer.npad.div_ceil(4)
                };
                let label = format!("layer{index}");
                let grid = match self.layers[index].family {
                    LayerFamily::Simdgroup => {
                        let (_, blocks) = simdgroup_shape(layer);
                        let npad = simd_grouped(layer).map_or(layer.npad, |grouped| grouped.npad);
                        Some(Grid::Groups(
                            MTLSize {
                                width: width.div_ceil(simdgroup_span(layer)),
                                height: (b - a).div_ceil(SIMDS),
                                depth: (npad / 8).div_ceil(blocks),
                            },
                            MTLSize {
                                width: 32 * SIMDS,
                                height: 1,
                                depth: 1,
                            },
                        ))
                    }
                    LayerFamily::Tensor | LayerFamily::TensorTransposed => Some(Grid::Groups(
                        MTLSize {
                            width: width.div_ceil(tensor_span(
                                layer,
                                self.layers[index].family == LayerFamily::TensorTransposed,
                            )),
                            height: b - a,
                            depth: 1,
                        },
                        MTLSize {
                            width: 32 * tensor_simds(),
                            height: 1,
                            depth: 1,
                        },
                    )),
                    LayerFamily::Scalar => Some(Grid::Threads(
                        width.div_ceil(block_pixels()),
                        b - a,
                        if layer.head {
                            1
                        } else {
                            layer.npad / block_channels(layer)
                        },
                    )),
                };
                let Some(grid) = grid else {
                    steps.push(Step {
                        label,
                        pipeline: StepPipeline::Scalar(index),
                        buffers,
                        params,
                        grid: Grid::Threads(width, b - a, groups),
                    });
                    continue;
                };
                steps.push(Step {
                    label,
                    pipeline: StepPipeline::Matrix(index),
                    buffers: buffers.clone(),
                    params: params.clone(),
                    grid,
                });
                if layer.variants > 1 {
                    // Exact weights on the one-pixel image ring.
                    let top = a == 0;
                    let bottom = b == height && height > 1;
                    let inner = width.saturating_sub(2);
                    let count =
                        2 * (b - a) + if top { inner } else { 0 } + if bottom { inner } else { 0 };
                    steps.push(Step {
                        label: format!("layer{index}_ring"),
                        pipeline: StepPipeline::Scalar(index),
                        buffers,
                        params: BandParams {
                            ring_count: count as i32,
                            ring_top: i32::from(top),
                            ring_bottom: i32::from(bottom),
                            ..params
                        },
                        grid: Grid::Threads(count, 1, groups),
                    });
                }
            }

            if let Some(view) = plan.head_view {
                steps.push(Step {
                    label: "head".to_string(),
                    pipeline: StepPipeline::Head,
                    buffers: vec![(0, slot_of(view))],
                    params: BandParams {
                        row_begin: y0 as i32,
                        row_end: y1 as i32,
                        ..base.clone()
                    },
                    grid: Grid::Threads(width, y1 - y0, 1),
                });
            }
            y0 = y1;
        }
        steps
    }
}

#[derive(Clone, Copy)]
enum StepPipeline {
    Input,
    Head,
    Scalar(usize),
    Matrix(usize),
}

#[derive(Clone, Copy)]
enum Grid {
    Threads(usize, usize, usize),
    Groups(MTLSize, MTLSize),
}

struct Step {
    label: String,
    pipeline: StepPipeline,
    buffers: Vec<(usize, usize)>,
    params: BandParams,
    grid: Grid,
}

fn set_params(encoder: &ProtocolObject<dyn MTLComputeCommandEncoder>, params: &BandParams) {
    let bytes = params.bytes();
    unsafe {
        encoder.setBytes_length_atIndex(
            NonNull::new(bytes.as_ptr().cast::<c_void>().cast_mut()).expect("params non-null"),
            bytes.len(),
            BUFFER_PARAMS,
        );
    }
}

fn dispatch_threads(
    encoder: &ProtocolObject<dyn MTLComputeCommandEncoder>,
    width: usize,
    height: usize,
    depth: usize,
) {
    if width == 0 || height == 0 || depth == 0 {
        return;
    }
    // Uniform threadgroups (every kernel bounds-checks): non-uniform
    // dispatch is unavailable on Apple3 GPUs, which Erika still deploys to.
    let tg_width = if height == 1 { 64 } else { 16 };
    let tg_height = if height == 1 { 1 } else { 8 };
    encoder.dispatchThreadgroups_threadsPerThreadgroup(
        MTLSize {
            width: width.div_ceil(tg_width),
            height: height.div_ceil(tg_height),
            depth,
        },
        MTLSize {
            width: tg_width,
            height: tg_height,
            depth: 1,
        },
    );
}
