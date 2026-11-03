//! Backend-neutral super-resolution network description.
//!
//! Every network Erika executes (ArtCNN, Anime4K, user-supplied FSRCNNX, ...)
//! is lowered to this graph of 2D convolutions over a single-channel input
//! (the decoded luma plane) followed by a 2x depth-to-space head. GPU
//! backends generate their kernels from it, and [`super::reference`]
//! evaluates it on the CPU as the numerical oracle for those kernels.
//!
//! Weight layout is ONNX `OIHW` per input: `[out][channel][ky][kx]`.

use crate::core::{PlayerError, Result};

/// Tensor 0 is always the network input: one channel of luma.
pub const INPUT_TENSOR: usize = 0;

const MAGIC: u32 = 0x4D52_5345; // "ESRM"
const VERSION: u32 = 1;
const MAX_CHANNELS: usize = 256;
const MAX_KERNEL: usize = 9;

/// Value of a tensor outside the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Padding {
    /// ONNX/Keras "same" padding: every layer sees zeros outside the image.
    Zero,
    /// mpv hook semantics: every texture is sampled clamp-to-edge.
    Clamp,
}

/// Domain of the luma fed to the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRange {
    /// Raw plane values, as mpv's `LUMA` hook exposes them.
    Raw,
    /// Limited-range video is expanded to full range first, and the output is
    /// mapped back. Used by networks trained on full-range RGB (`MAIN`).
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputTransform {
    Identity,
    /// `max(x, 0)`.
    Relu,
    /// `max(-x, 0)`, the negative half of a CReLU.
    NegRelu,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Activation {
    None,
    Relu,
    /// `x * sigmoid(x)`.
    Silu,
    /// `max(x, 0) + alpha * min(x, 0)` with one slope per output channel.
    Prelu(Vec<f32>),
}

/// One operand of a convolution: a tensor seen through an element-wise
/// transform and convolved with its own kernel. Several operands are summed,
/// which expresses concatenated inputs (Anime4K's CReLU and dense skip
/// connections) without materializing the concatenation.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvInput {
    pub tensor: usize,
    pub transform: InputTransform,
    /// Odd kernel width; taps span `-(k/2)..=k/2`.
    pub kernel: usize,
    /// `[out][channel][ky][kx]`.
    pub weights: Vec<f32>,
}

/// Weights of a layer in one image-border class (see [`ConvLayer::border`]).
#[derive(Debug, Clone, PartialEq)]
pub struct LayerWeights {
    pub inputs: Vec<Vec<f32>>,
    pub bias: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConvLayer {
    pub output: usize,
    pub out_channels: usize,
    pub inputs: Vec<ConvInput>,
    pub bias: Vec<f32>,
    /// Tensor added after the bias and before the activation.
    pub residual: Option<usize>,
    pub activation: Activation,
    /// Exact weights for output pixels on the image border.
    ///
    /// Algebraically folded layers (see [`super::fold`]) are only equivalent
    /// to the original pair away from the border, where the intermediate
    /// tensor's padding no longer applies. Index `row_class * 3 + col_class`
    /// with classes 0 = first row/column, 1 = interior, 2 = last; entry 4 must
    /// equal the main weights. Backends evaluate the interior weights
    /// everywhere and then re-evaluate the one-pixel ring with these.
    pub border: Option<Vec<LayerWeights>>,
}

/// How the head turns its four channels into the 2x output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubpixelOrder {
    /// Channel `dy * 2 + dx` (ONNX DepthToSpace DCR, Anime4K).
    RowMajor,
    /// Channel `dx * 2 + dy` (FSRCNNX aggregation).
    ColumnMajor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadBase {
    /// The head channels are the output luma.
    None,
    /// The head channels are a residual added to the bilinearly upsampled
    /// input luma (clamp-to-edge), as Anime4K does with `MAIN_tex(MAIN_pos)`.
    Bilinear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub tensor: usize,
    pub order: SubpixelOrder,
    pub base: HeadBase,
    /// Clamp the result to `[0, 1]` (ArtCNN's final `Clip`).
    pub clamp: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SrModel {
    pub name: String,
    pub padding: Padding,
    pub input_range: InputRange,
    /// Channel count per tensor. `tensors[0] == 1`.
    pub tensors: Vec<usize>,
    /// Topologically ordered: a layer only reads tensors written earlier.
    pub layers: Vec<ConvLayer>,
    pub head: Head,
}

impl SrModel {
    /// Multiply-accumulates per input pixel, ignoring border re-evaluation.
    pub fn macs_per_pixel(&self) -> u64 {
        self.layers
            .iter()
            .map(|layer| {
                layer
                    .inputs
                    .iter()
                    .map(|input| {
                        (layer.out_channels
                            * self.tensors[input.tensor]
                            * input.kernel
                            * input.kernel) as u64
                    })
                    .sum::<u64>()
            })
            .sum()
    }

    /// Number of stored weights and biases.
    pub fn parameter_count(&self) -> usize {
        self.layers
            .iter()
            .map(|layer| {
                layer.bias.len()
                    + layer
                        .inputs
                        .iter()
                        .map(|input| input.weights.len())
                        .sum::<usize>()
            })
            .sum()
    }

    /// Indices of the layers producing each tensor (`None` for the input).
    pub fn producers(&self) -> Vec<Option<usize>> {
        let mut producers = vec![None; self.tensors.len()];
        for (index, layer) in self.layers.iter().enumerate() {
            producers[layer.output] = Some(index);
        }
        producers
    }

    /// Pixels of halo each tensor must be computed with so the head output is
    /// exact over a tile. The head tensor needs none; every other tensor needs
    /// the largest halo any consumer requires plus that consumer's radius.
    pub fn tensor_halos(&self) -> Vec<usize> {
        let mut halos = vec![0usize; self.tensors.len()];
        let mut needed = vec![false; self.tensors.len()];
        needed[self.head.tensor] = true;
        for layer in self.layers.iter().rev() {
            if !needed[layer.output] {
                continue;
            }
            let out_halo = halos[layer.output];
            for input in &layer.inputs {
                let halo = out_halo + input.kernel / 2;
                halos[input.tensor] = halos[input.tensor].max(halo);
                needed[input.tensor] = true;
            }
            if let Some(residual) = layer.residual {
                halos[residual] = halos[residual].max(out_halo);
                needed[residual] = true;
            }
        }
        halos
    }

    /// Checks shapes and ordering. Every constructor and the deserializer call
    /// this, so backends can index without re-validating.
    pub fn validate(&self) -> Result<()> {
        let fail = |message: String| Err(invalid(&self.name, message));
        if self.tensors.first() != Some(&1) {
            return fail("tensor 0 must be the single-channel input".to_string());
        }
        if self.tensors.iter().any(|&c| c == 0 || c > MAX_CHANNELS) {
            return fail(format!("tensor channel counts must be 1..={MAX_CHANNELS}"));
        }
        let mut written = vec![false; self.tensors.len()];
        written[INPUT_TENSOR] = true;
        for (index, layer) in self.layers.iter().enumerate() {
            let check_read = |tensor: usize| -> Result<()> {
                if tensor >= self.tensors.len() || !written[tensor] {
                    return Err(invalid(
                        &self.name,
                        format!("layer {index} reads tensor {tensor} before it is written"),
                    ));
                }
                Ok(())
            };
            if layer.output == INPUT_TENSOR
                || layer.output >= self.tensors.len()
                || written[layer.output]
            {
                return fail(format!("layer {index} has an invalid output tensor"));
            }
            if self.tensors[layer.output] != layer.out_channels {
                return fail(format!("layer {index} output channel mismatch"));
            }
            if layer.inputs.is_empty() || layer.bias.len() != layer.out_channels {
                return fail(format!(
                    "layer {index} needs inputs and one bias per output"
                ));
            }
            for input in &layer.inputs {
                check_read(input.tensor)?;
                if input.kernel % 2 == 0 || input.kernel > MAX_KERNEL {
                    return fail(format!("layer {index} has kernel {}", input.kernel));
                }
                let expected =
                    layer.out_channels * self.tensors[input.tensor] * input.kernel * input.kernel;
                if input.weights.len() != expected {
                    return fail(format!(
                        "layer {index} input weights are {} values, expected {expected}",
                        input.weights.len()
                    ));
                }
            }
            if let Some(residual) = layer.residual {
                check_read(residual)?;
                if self.tensors[residual] != layer.out_channels {
                    return fail(format!("layer {index} residual channel mismatch"));
                }
            }
            if let Activation::Prelu(alpha) = &layer.activation
                && alpha.len() != layer.out_channels
            {
                return fail(format!("layer {index} PReLU slope count mismatch"));
            }
            if let Some(border) = &layer.border {
                let shapes_match = border.len() == 9
                    && border.iter().all(|variant| {
                        variant.bias.len() == layer.out_channels
                            && variant.inputs.len() == layer.inputs.len()
                            && variant
                                .inputs
                                .iter()
                                .zip(&layer.inputs)
                                .all(|(weights, input)| weights.len() == input.weights.len())
                    });
                if !shapes_match {
                    return fail(format!("layer {index} border variants are malformed"));
                }
            }
            written[layer.output] = true;
        }
        if self.head.tensor >= self.tensors.len() || !written[self.head.tensor] {
            return fail("head tensor is never written".to_string());
        }
        if self.tensors[self.head.tensor] != 4 {
            return fail("head tensor must have four channels".to_string());
        }
        Ok(())
    }

    /// Serializes to the `.esr` format. Weights are stored as IEEE half
    /// precision unless `full_precision` is set.
    pub fn to_bytes(&self, full_precision: bool) -> Vec<u8> {
        let mut out = Writer {
            bytes: Vec::new(),
            half: !full_precision,
        };
        out.u32(MAGIC);
        out.u32(VERSION);
        out.u8(u8::from(!full_precision));
        out.str(&self.name);
        out.u8(match self.padding {
            Padding::Zero => 0,
            Padding::Clamp => 1,
        });
        out.u8(match self.input_range {
            InputRange::Raw => 0,
            InputRange::Full => 1,
        });
        out.u16(self.tensors.len() as u16);
        for &channels in &self.tensors {
            out.u16(channels as u16);
        }
        out.u16(self.layers.len() as u16);
        for layer in &self.layers {
            out.u16(layer.output as u16);
            out.u16(layer.residual.map_or(u16::MAX, |tensor| tensor as u16));
            match &layer.activation {
                Activation::None => out.u8(0),
                Activation::Relu => out.u8(1),
                Activation::Silu => out.u8(2),
                Activation::Prelu(alpha) => {
                    out.u8(3);
                    out.floats(alpha);
                }
            }
            out.u8(layer.inputs.len() as u8);
            for input in &layer.inputs {
                out.u16(input.tensor as u16);
                out.u8(match input.transform {
                    InputTransform::Identity => 0,
                    InputTransform::Relu => 1,
                    InputTransform::NegRelu => 2,
                });
                out.u8(input.kernel as u8);
                out.floats(&input.weights);
            }
            out.floats(&layer.bias);
            match &layer.border {
                None => out.u8(0),
                Some(variants) => {
                    out.u8(1);
                    for variant in variants {
                        for weights in &variant.inputs {
                            out.floats(weights);
                        }
                        out.floats(&variant.bias);
                    }
                }
            }
        }
        out.u16(self.head.tensor as u16);
        out.u8(match self.head.order {
            SubpixelOrder::RowMajor => 0,
            SubpixelOrder::ColumnMajor => 1,
        });
        out.u8(match self.head.base {
            HeadBase::None => 0,
            HeadBase::Bilinear => 1,
        });
        out.u8(u8::from(self.head.clamp));
        out.bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut input = Reader {
            bytes,
            cursor: 0,
            half: false,
        };
        let corrupt = || PlayerError::Renderer("upscaler model file is corrupt".to_string());
        if input.u32().ok_or_else(corrupt)? != MAGIC {
            return Err(PlayerError::Renderer(
                "not an Erika upscaler model (bad magic)".to_string(),
            ));
        }
        let version = input.u32().ok_or_else(corrupt)?;
        if version != VERSION {
            return Err(PlayerError::Renderer(format!(
                "unsupported Erika upscaler model version {version}"
            )));
        }
        let model = (|| -> Option<Self> {
            input.half = input.u8()? != 0;
            let name = input.str()?;
            let padding = match input.u8()? {
                0 => Padding::Zero,
                1 => Padding::Clamp,
                _ => return None,
            };
            let input_range = match input.u8()? {
                0 => InputRange::Raw,
                1 => InputRange::Full,
                _ => return None,
            };
            let tensor_count = input.u16()? as usize;
            let tensors = (0..tensor_count)
                .map(|_| input.u16().map(usize::from))
                .collect::<Option<Vec<_>>>()?;
            let layer_count = input.u16()? as usize;
            let mut layers = Vec::with_capacity(layer_count);
            for _ in 0..layer_count {
                let output = input.u16()? as usize;
                let out_channels = *tensors.get(output)?;
                let residual = match input.u16()? {
                    u16::MAX => None,
                    tensor => Some(tensor as usize),
                };
                let activation = match input.u8()? {
                    0 => Activation::None,
                    1 => Activation::Relu,
                    2 => Activation::Silu,
                    3 => Activation::Prelu(input.floats(out_channels)?),
                    _ => return None,
                };
                let input_count = input.u8()? as usize;
                let mut inputs = Vec::with_capacity(input_count);
                for _ in 0..input_count {
                    let tensor = input.u16()? as usize;
                    let transform = match input.u8()? {
                        0 => InputTransform::Identity,
                        1 => InputTransform::Relu,
                        2 => InputTransform::NegRelu,
                        _ => return None,
                    };
                    let kernel = input.u8()? as usize;
                    if kernel == 0 || kernel > MAX_KERNEL {
                        return None;
                    }
                    let channels = *tensors.get(tensor)?;
                    let weights = input.floats(out_channels * channels * kernel * kernel)?;
                    inputs.push(ConvInput {
                        tensor,
                        transform,
                        kernel,
                        weights,
                    });
                }
                let bias = input.floats(out_channels)?;
                let border = match input.u8()? {
                    0 => None,
                    1 => {
                        let mut variants = Vec::with_capacity(9);
                        for _ in 0..9 {
                            let weights = inputs
                                .iter()
                                .map(|conv| input.floats(conv.weights.len()))
                                .collect::<Option<Vec<_>>>()?;
                            let bias = input.floats(out_channels)?;
                            variants.push(LayerWeights {
                                inputs: weights,
                                bias,
                            });
                        }
                        Some(variants)
                    }
                    _ => return None,
                };
                layers.push(ConvLayer {
                    output,
                    out_channels,
                    inputs,
                    bias,
                    residual,
                    activation,
                    border,
                });
            }
            let head = Head {
                tensor: input.u16()? as usize,
                order: match input.u8()? {
                    0 => SubpixelOrder::RowMajor,
                    1 => SubpixelOrder::ColumnMajor,
                    _ => return None,
                },
                base: match input.u8()? {
                    0 => HeadBase::None,
                    1 => HeadBase::Bilinear,
                    _ => return None,
                },
                clamp: input.u8()? != 0,
            };
            (input.cursor == bytes.len()).then_some(Self {
                name,
                padding,
                input_range,
                tensors,
                layers,
                head,
            })
        })()
        .ok_or_else(corrupt)?;
        model.validate()?;
        Ok(model)
    }
}

fn invalid(name: &str, message: String) -> PlayerError {
    PlayerError::Renderer(format!("upscaler model {name:?} is invalid: {message}"))
}

struct Writer {
    bytes: Vec<u8>,
    half: bool,
}

impl Writer {
    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn str(&mut self, value: &str) {
        let bytes = &value.as_bytes()[..value.len().min(u16::MAX as usize)];
        self.u16(bytes.len() as u16);
        self.bytes.extend_from_slice(bytes);
    }

    fn floats(&mut self, values: &[f32]) {
        for &value in values {
            if self.half {
                self.u16(f32_to_f16(value));
            } else {
                self.bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
    half: bool,
}

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Option<&[u8]> {
        let end = self.cursor.checked_add(len)?;
        let slice = self.bytes.get(self.cursor..end)?;
        self.cursor = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }

    fn str(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }

    fn floats(&mut self, count: usize) -> Option<Vec<f32>> {
        let half = self.half;
        let width = if half { 2 } else { 4 };
        let bytes = self.take(count.checked_mul(width)?)?;
        Some(
            bytes
                .chunks_exact(width)
                .map(|chunk| {
                    if half {
                        f16_to_f32(u16::from_le_bytes([chunk[0], chunk[1]]))
                    } else {
                        f32::from_le_bytes(chunk.try_into().expect("four bytes"))
                    }
                })
                .collect(),
        )
    }
}

/// IEEE 754 binary16 to binary32.
pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = u32::from((bits >> 10) & 0x1f);
    let mantissa = u32::from(bits & 0x03ff);
    let value = match (exponent, mantissa) {
        (0, 0) => sign,
        (0, _) => {
            let shift = mantissa.leading_zeros() - 21;
            let exponent = 127 - 14 - shift;
            let mantissa = (mantissa << shift) & 0x03ff;
            sign | (exponent << 23) | (mantissa << 13)
        }
        (0x1f, 0) => sign | 0x7f80_0000,
        (0x1f, _) => sign | 0x7f80_0000 | (mantissa << 13),
        _ => sign | ((exponent + (127 - 15)) << 23) | (mantissa << 13),
    };
    f32::from_bits(value)
}

/// IEEE 754 binary32 to binary16, round to nearest even.
pub fn f32_to_f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;
    if exponent == 0xff {
        let nan = if mantissa != 0 { 0x0200 } else { 0 };
        return sign | 0x7c00 | nan;
    }
    let half_exponent = exponent - 127 + 15;
    if half_exponent >= 0x1f {
        return sign | 0x7c00;
    }
    if half_exponent <= 0 {
        if half_exponent < -10 {
            return sign;
        }
        let mantissa = mantissa | 0x0080_0000;
        let shift = (14 - half_exponent) as u32;
        let half = mantissa >> shift;
        let remainder = mantissa & ((1 << shift) - 1);
        let midpoint = 1 << (shift - 1);
        let rounded = if remainder > midpoint || (remainder == midpoint && half & 1 == 1) {
            half + 1
        } else {
            half
        };
        return sign | rounded as u16;
    }
    let half = ((half_exponent as u32) << 10) | (mantissa >> 13);
    let remainder = mantissa & 0x1fff;
    let rounded = if remainder > 0x1000 || (remainder == 0x1000 && half & 1 == 1) {
        half + 1
    } else {
        half
    };
    sign | rounded as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn tiny_model() -> SrModel {
        SrModel {
            name: "tiny".to_string(),
            padding: Padding::Zero,
            input_range: InputRange::Raw,
            tensors: vec![1, 4, 4],
            layers: vec![
                ConvLayer {
                    output: 1,
                    out_channels: 4,
                    inputs: vec![ConvInput {
                        tensor: 0,
                        transform: InputTransform::Identity,
                        kernel: 3,
                        weights: (0..36).map(|i| i as f32 * 0.01 - 0.1).collect(),
                    }],
                    bias: vec![0.1, -0.2, 0.3, 0.0],
                    residual: None,
                    activation: Activation::Prelu(vec![0.25, 0.5, 0.0, 1.0]),
                    border: None,
                },
                ConvLayer {
                    output: 2,
                    out_channels: 4,
                    inputs: vec![
                        ConvInput {
                            tensor: 1,
                            transform: InputTransform::Relu,
                            kernel: 1,
                            weights: (0..16).map(|i| i as f32 * -0.02).collect(),
                        },
                        ConvInput {
                            tensor: 1,
                            transform: InputTransform::NegRelu,
                            kernel: 3,
                            weights: (0..144).map(|i| (i % 7) as f32 * 0.03).collect(),
                        },
                    ],
                    bias: vec![0.0; 4],
                    residual: Some(1),
                    activation: Activation::None,
                    border: None,
                },
            ],
            head: Head {
                tensor: 2,
                order: SubpixelOrder::ColumnMajor,
                base: HeadBase::Bilinear,
                clamp: false,
            },
        }
    }

    #[test]
    fn serialization_round_trips_in_full_precision() {
        let model = tiny_model();
        model.validate().unwrap();
        let bytes = model.to_bytes(true);
        assert_eq!(SrModel::from_bytes(&bytes).unwrap(), model);
    }

    #[test]
    fn half_precision_round_trip_is_close() {
        let model = tiny_model();
        let decoded = SrModel::from_bytes(&model.to_bytes(false)).unwrap();
        for (a, b) in model.layers.iter().zip(&decoded.layers) {
            for (x, y) in a.inputs[0].weights.iter().zip(&b.inputs[0].weights) {
                assert!((x - y).abs() <= x.abs() * 1e-3 + 1e-6);
            }
        }
    }

    #[test]
    fn deserializer_rejects_truncation_and_bad_magic() {
        let bytes = tiny_model().to_bytes(false);
        assert!(SrModel::from_bytes(&bytes[..bytes.len() - 1]).is_err());
        let mut bad = bytes.clone();
        bad[0] ^= 1;
        assert!(SrModel::from_bytes(&bad).is_err());
    }

    #[test]
    fn halos_follow_receptive_field() {
        let model = tiny_model();
        // Head tensor 2 needs 0; layer 2 reads tensor 1 with kernel 3 -> 1;
        // layer 1 reads the input with kernel 3 -> 2.
        assert_eq!(model.tensor_halos(), vec![2, 1, 0]);
        assert_eq!(model.macs_per_pixel(), 36 + 16 + 144 * 1);
    }

    #[test]
    fn validation_rejects_read_before_write() {
        let mut model = tiny_model();
        model.layers.swap(0, 1);
        assert!(model.validate().is_err());
    }

    #[test]
    fn half_conversion_matches_known_values() {
        for (value, bits) in [
            (0.0f32, 0x0000u16),
            (1.0, 0x3c00),
            (-2.0, 0xc000),
            (65504.0, 0x7bff),
            (6.103_515_6e-5, 0x0400),
            (5.960_464_5e-8, 0x0001),
        ] {
            assert_eq!(f32_to_f16(value), bits, "{value}");
            assert_eq!(f16_to_f32(bits), value);
        }
        assert_eq!(f32_to_f16(0.333_333_34), 0x3555);
        for bits in [0x0003u16, 0x02ffu16, 0x83ffu16] {
            assert_eq!(f32_to_f16(f16_to_f32(bits)), bits);
        }
        assert_eq!(f16_to_f32(0x0003), 3.0 * 5.960_464_5e-8);
        assert_eq!(f32_to_f16(1e6), 0x7c00);
    }
}
