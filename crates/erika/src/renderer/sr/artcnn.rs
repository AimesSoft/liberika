//! Lowers the legacy `ACNN` ArtCNN C-series blobs to the generic model graph.

use super::model::{
    Activation, ConvInput, ConvLayer, Head, HeadBase, InputRange, InputTransform, Padding, SrModel,
    SubpixelOrder, f16_to_f32,
};
use crate::core::Result;
use crate::renderer::artcnn::{ArtCnnModel, CONVOLUTION_TAPS, MIDDLE_LAYER_COUNT};

/// conv0 -> 5x (conv, ReLU on the first four) -> +conv0 -> conv6 -> D2S -> clip.
pub fn lower(name: &str, model: ArtCnnModel<'_>) -> Result<SrModel> {
    let features = model.layout.feature_count;
    let slices = features / 4;
    let offsets = model.layout.layer_offsets;
    let half = |index: usize| -> f32 {
        f16_to_f32(u16::from_le_bytes([
            model.payload[index * 2],
            model.payload[index * 2 + 1],
        ]))
    };
    // Offsets are in half4 units.
    let at = |half4: u32, lane: usize| half(half4 as usize * 4 + lane);

    let mut conv0 = vec![0.0f32; features * CONVOLUTION_TAPS];
    for s in 0..slices {
        for tap in 0..CONVOLUTION_TAPS {
            for o in 0..4 {
                conv0[(s * 4 + o) * CONVOLUTION_TAPS + tap] =
                    at(offsets.conv0_w, (s * CONVOLUTION_TAPS + tap) * 4 + o);
            }
        }
    }
    let bias = |start: u32, count: usize| (0..count).map(|i| at(start, i)).collect::<Vec<_>>();

    let mut layers = vec![ConvLayer {
        output: 1,
        out_channels: features,
        inputs: vec![ConvInput {
            tensor: 0,
            transform: InputTransform::Identity,
            kernel: 3,
            weights: conv0,
        }],
        bias: bias(offsets.conv0_b, features),
        residual: None,
        activation: Activation::None,
        border: None,
    }];

    for layer in 0..MIDDLE_LAYER_COUNT {
        let mut weights = vec![0.0f32; features * features * CONVOLUTION_TAPS];
        for s in 0..slices {
            for tap in 0..CONVOLUTION_TAPS {
                for i in 0..slices {
                    // 4x4 column-major block: column j is input channel j.
                    let block = ((s * CONVOLUTION_TAPS + tap) * slices + i) * 16;
                    for j in 0..4 {
                        for o in 0..4 {
                            weights
                                [((s * 4 + o) * features + i * 4 + j) * CONVOLUTION_TAPS + tap] =
                                at(offsets.mid_w[layer], block + j * 4 + o);
                        }
                    }
                }
            }
        }
        let last = layer == MIDDLE_LAYER_COUNT - 1;
        layers.push(ConvLayer {
            output: layer + 2,
            out_channels: features,
            inputs: vec![ConvInput {
                tensor: layer + 1,
                transform: InputTransform::Identity,
                kernel: 3,
                weights,
            }],
            bias: bias(offsets.mid_b[layer], features),
            residual: last.then_some(1),
            activation: if last {
                Activation::None
            } else {
                Activation::Relu
            },
            border: None,
        });
    }

    let mut conv6 = vec![0.0f32; 4 * features * CONVOLUTION_TAPS];
    for tap in 0..CONVOLUTION_TAPS {
        for i in 0..slices {
            let block = (tap * slices + i) * 16;
            for j in 0..4 {
                for o in 0..4 {
                    conv6[(o * features + i * 4 + j) * CONVOLUTION_TAPS + tap] =
                        at(offsets.conv6_w, block + j * 4 + o);
                }
            }
        }
    }
    let head_tensor = MIDDLE_LAYER_COUNT + 2;
    layers.push(ConvLayer {
        output: head_tensor,
        out_channels: 4,
        inputs: vec![ConvInput {
            tensor: MIDDLE_LAYER_COUNT + 1,
            transform: InputTransform::Identity,
            kernel: 3,
            weights: conv6,
        }],
        bias: bias(offsets.conv6_b, 4),
        residual: None,
        activation: Activation::None,
        border: None,
    });

    let mut tensors = vec![1];
    tensors.extend(std::iter::repeat_n(features, MIDDLE_LAYER_COUNT + 1));
    tensors.push(4);
    let lowered = SrModel {
        name: name.to_string(),
        padding: Padding::Zero,
        input_range: InputRange::Raw,
        tensors,
        layers,
        head: Head {
            tensor: head_tensor,
            order: SubpixelOrder::RowMajor,
            base: HeadBase::None,
            clamp: true,
        },
    };
    lowered.validate()?;
    Ok(lowered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::artcnn::model_for_mode;
    use crate::renderer::pipeline::LumaUpscalerMode;
    use crate::renderer::sr::reference::{LumaRange, run};

    pub(crate) fn load_f32(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four bytes")))
            .collect()
    }

    fn check(mode: LumaUpscalerMode, input: &[u8], expected: &[u8]) {
        let model = lower("test", model_for_mode(mode).unwrap()).unwrap();
        let output = run(&model, &load_f32(input), 128, 72, LumaRange::Full);
        let expected = load_f32(expected);
        let (mut sum, mut max) = (0.0f64, 0.0f64);
        for (a, b) in output.iter().zip(&expected) {
            let error = f64::from((a - b).abs());
            sum += error;
            max = max.max(error);
        }
        let mae = sum / output.len() as f64;
        // Weights are f16; the ONNX reference is f32.
        assert!(mae < 1e-3, "{mode:?} mae {mae}");
        assert!(max < 1e-2, "{mode:?} max {max}");
    }

    #[test]
    fn lowered_artcnn_matches_onnx_reference() {
        let input = include_bytes!("../../../tests/data/artcnn/c4f16/input_128x72.f32");
        check(
            LumaUpscalerMode::ArtCnnC4F16,
            input,
            include_bytes!("../../../tests/data/artcnn/c4f16/output_256x144.f32"),
        );
        check(
            LumaUpscalerMode::ArtCnnC4F16Ds,
            input,
            include_bytes!("../../../tests/data/artcnn/c4f16_ds/output_256x144.f32"),
        );
        check(
            LumaUpscalerMode::ArtCnnC4F32,
            include_bytes!("../../../tests/data/artcnn/c4f32/input_128x72.f32"),
            include_bytes!("../../../tests/data/artcnn/c4f32/output_256x144.f32"),
        );
    }
}
