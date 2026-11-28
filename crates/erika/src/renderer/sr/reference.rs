//! Straightforward f32 CPU evaluation of an [`SrModel`].
//!
//! This is the numerical oracle for every GPU backend: it applies padding,
//! border variants, activations and the head exactly as documented on the
//! model types, with no tiling or fusion.

use super::model::{
    Activation, ConvLayer, HeadBase, INPUT_TENSOR, InputRange, InputTransform, Padding, SrModel,
    SubpixelOrder,
};

/// Range of the luma handed to the upscaler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LumaRange {
    Full,
    /// 16..235 (8-bit) and its higher bit depth equivalents, normalized.
    Limited,
}

/// Offset and scale that map raw luma to the network's input domain.
pub fn input_normalization(model: &SrModel, range: LumaRange) -> (f32, f32) {
    match (model.input_range, range) {
        (InputRange::Full, LumaRange::Limited) => (16.0 / 255.0, 255.0 / 219.0),
        _ => (0.0, 1.0),
    }
}

/// Row-major `[channel][y][x]` storage for one tensor.
struct Tensor {
    channels: usize,
    data: Vec<f32>,
}

/// Evaluates `model` on a `width x height` luma plane (row-major) and returns
/// the `2*width x 2*height` output luma, row-major.
pub fn run(
    model: &SrModel,
    luma: &[f32],
    width: usize,
    height: usize,
    range: LumaRange,
) -> Vec<f32> {
    assert_eq!(luma.len(), width * height);
    let (offset, scale) = input_normalization(model, range);
    let plane = width * height;
    let mut tensors: Vec<Option<Tensor>> = (0..model.tensors.len()).map(|_| None).collect();
    tensors[INPUT_TENSOR] = Some(Tensor {
        channels: 1,
        data: luma.iter().map(|&value| (value - offset) * scale).collect(),
    });
    for layer in &model.layers {
        let output = evaluate_layer(model, layer, &tensors, width, height);
        tensors[layer.output] = Some(Tensor {
            channels: layer.out_channels,
            data: output,
        });
    }

    let head = tensors[model.head.tensor]
        .as_ref()
        .expect("head tensor written");
    let mut out = vec![0.0f32; plane * 4];
    for y in 0..height {
        for x in 0..width {
            for sy in 0..2 {
                for sx in 0..2 {
                    let channel = match model.head.order {
                        SubpixelOrder::RowMajor => sy * 2 + sx,
                        SubpixelOrder::ColumnMajor => sx * 2 + sy,
                    };
                    let value = head.data[channel * plane + y * width + x] / scale;
                    let value = match model.head.base {
                        HeadBase::None => value + offset,
                        HeadBase::Bilinear => {
                            value
                                + bilinear_clamped(
                                    luma,
                                    width,
                                    height,
                                    x as f32 - 0.25 + 0.5 * sx as f32,
                                    y as f32 - 0.25 + 0.5 * sy as f32,
                                )
                        }
                    };
                    let value = if model.head.clamp {
                        value.clamp(0.0, 1.0)
                    } else {
                        value
                    };
                    out[(2 * y + sy) * width * 2 + 2 * x + sx] = value;
                }
            }
        }
    }
    out
}

/// Bilinear sample at pixel-space position `(px, py)` (pixel centers at
/// integers) with clamp-to-edge addressing.
pub fn bilinear_clamped(luma: &[f32], width: usize, height: usize, px: f32, py: f32) -> f32 {
    let x0 = px.floor();
    let y0 = py.floor();
    let fx = px - x0;
    let fy = py - y0;
    let at = |x: f32, y: f32| {
        let x = (x as isize).clamp(0, width as isize - 1) as usize;
        let y = (y as isize).clamp(0, height as isize - 1) as usize;
        luma[y * width + x]
    };
    let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1.0, y0) * fx;
    let bottom = at(x0, y0 + 1.0) * (1.0 - fx) + at(x0 + 1.0, y0 + 1.0) * fx;
    top * (1.0 - fy) + bottom * fy
}

fn border_class(position: usize, extent: usize) -> usize {
    if position == 0 {
        0
    } else if position + 1 == extent {
        2
    } else {
        1
    }
}

fn evaluate_layer(
    model: &SrModel,
    layer: &ConvLayer,
    tensors: &[Option<Tensor>],
    width: usize,
    height: usize,
) -> Vec<f32> {
    let plane = width * height;
    let mut out = vec![0.0f32; layer.out_channels * plane];
    let fetch = |tensor: &Tensor, channel: usize, x: isize, y: isize| -> f32 {
        let inside = x >= 0 && y >= 0 && x < width as isize && y < height as isize;
        let (x, y) = match model.padding {
            Padding::Zero if !inside => return 0.0,
            Padding::Zero => (x as usize, y as usize),
            Padding::Clamp => (
                x.clamp(0, width as isize - 1) as usize,
                y.clamp(0, height as isize - 1) as usize,
            ),
        };
        tensor.data[channel * plane + y * width + x]
    };
    let mut acc = vec![0.0f32; layer.out_channels];
    for y in 0..height {
        for x in 0..width {
            let variant = layer
                .border
                .as_ref()
                .map(|variants| &variants[border_class(y, height) * 3 + border_class(x, width)]);
            let bias = variant.map_or(&layer.bias, |variant| &variant.bias);
            acc.copy_from_slice(bias);
            for (index, input) in layer.inputs.iter().enumerate() {
                let weights = variant.map_or(&input.weights, |variant| &variant.inputs[index]);
                let tensor = tensors[input.tensor]
                    .as_ref()
                    .expect("input tensor written");
                let k = input.kernel;
                let radius = (k / 2) as isize;
                for c in 0..tensor.channels {
                    for ky in 0..k {
                        for kx in 0..k {
                            let value = fetch(
                                tensor,
                                c,
                                x as isize + kx as isize - radius,
                                y as isize + ky as isize - radius,
                            );
                            let value = match input.transform {
                                InputTransform::Identity => value,
                                InputTransform::Relu => value.max(0.0),
                                InputTransform::NegRelu => (-value).max(0.0),
                            };
                            if value == 0.0 {
                                continue;
                            }
                            let base = (c * k + ky) * k + kx;
                            let stride = tensor.channels * k * k;
                            for (o, acc) in acc.iter_mut().enumerate() {
                                *acc += weights[o * stride + base] * value;
                            }
                        }
                    }
                }
            }
            if let Some(residual) = layer.residual {
                let tensor = tensors[residual].as_ref().expect("residual tensor written");
                for (o, acc) in acc.iter_mut().enumerate() {
                    *acc += tensor.data[o * plane + y * width + x];
                }
            }
            for (o, &value) in acc.iter().enumerate() {
                out[o * plane + y * width + x] = activate(&layer.activation, o, value);
            }
        }
    }
    out
}

pub fn activate(activation: &Activation, channel: usize, value: f32) -> f32 {
    match activation {
        Activation::None => value,
        Activation::Relu => value.max(0.0),
        Activation::Silu => value / (1.0 + (-value).exp()),
        Activation::Prelu(alpha) => value.max(0.0) + alpha[channel] * value.min(0.0),
    }
}

/// Deterministic smooth-plus-edges test pattern in `[0, 1]`.
pub fn test_pattern(width: usize, height: usize, seed: u32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let mut noise = || {
        state = state.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
        ((state >> 9) as f32 / (1u32 << 23) as f32) - 0.5
    };
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let (fx, fy) = (x as f32, y as f32);
            let smooth = 0.5 + 0.25 * ((fx * 0.21).sin() * (fy * 0.17).cos());
            let ring = ((fx - width as f32 * 0.4).hypot(fy - height as f32 * 0.6) * 0.35).sin();
            let line = if (x + 2 * y) % 11 < 2 { -0.3 } else { 0.0 };
            out.push((smooth + 0.15 * ring.signum() + line + 0.05 * noise()).clamp(0.0, 1.0));
        }
    }
    out
}
