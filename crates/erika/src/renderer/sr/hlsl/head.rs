//! Input-channel blocking for instruction-cache-limited spatial heads.
use super::*;

pub(super) fn split(model: &SrModel, kernels: &mut Vec<KernelSpec>, block: usize, prefetch: usize) {
    let Some(spec) = kernels.last() else {
        return;
    };
    let layer = &model.layers[spec.layer];
    if !spec.head
        || spec.dense.is_some()
        || layer.residual.is_some()
        || layer.activation != Activation::None
    {
        return;
    }
    let features = layer
        .inputs
        .iter()
        .filter(|i| i.tensor != INPUT_TENSOR)
        .map(|i| i.tensor)
        .collect::<std::collections::HashSet<_>>();
    if features.len() != 1 {
        return;
    }
    let tensor = *features.iter().next().unwrap();
    let channels = model.tensors[tensor];
    if channels <= block || channels % block != 0 {
        return;
    }
    let index = spec.layer;
    kernels.pop();
    for (step, first) in (0..channels).step_by(block).enumerate() {
        let last = first + block == channels;
        let retained = layer
            .inputs
            .iter()
            .enumerate()
            .filter(|(_, i)| i.tensor == tensor || step == 0)
            .map(|(n, _)| n)
            .collect::<Vec<_>>();
        let part_weights = |input_index: usize, weights: &[f32]| {
            let input = &layer.inputs[input_index];
            if input.tensor != tensor {
                return weights.to_vec();
            }
            let taps = input.kernel * input.kernel;
            (0..layer.out_channels)
                .flat_map(|o| {
                    weights[(o * channels + first) * taps..(o * channels + first + block) * taps]
                        .iter()
                        .copied()
                })
                .collect::<Vec<_>>()
        };
        let mut part = layer.clone();
        part.inputs = retained
            .iter()
            .map(|&n| {
                let mut input = layer.inputs[n].clone();
                input.weights = part_weights(n, &input.weights);
                input
            })
            .collect();
        if step > 0 {
            part.bias.fill(0.0);
        }
        part.border = layer.border.as_ref().map(|variants| {
            variants
                .iter()
                .map(|v| super::super::model::LayerWeights {
                    inputs: retained
                        .iter()
                        .map(|&n| part_weights(n, &v.inputs[n]))
                        .collect(),
                    bias: if step == 0 {
                        v.bias.clone()
                    } else {
                        vec![0.0; layer.out_channels]
                    },
                })
                .collect()
        });
        let mut tensors = model.tensors.clone();
        tensors[tensor] = block;
        let local = SrModel {
            name: model.name.clone(),
            padding: model.padding,
            input_range: model.input_range,
            tensors,
            layers: vec![part.clone()],
            head: model.head,
        };
        let mut kernel = build_kernel(&local, index, &part, 0, 4, true, prefetch);
        let adapt = |source: &mut String| {
            *source = source.replace("slice, 0)", "slice + misc.z, 0)");
            if !last {
                *source = source.replace(
                    "RWTexture2D<float> u_out2x : register(u1);",
                    "RWTexture2D<float4> u_partial : register(u2);",
                );
            }
            if step > 0 {
                source.insert_str(0, "Texture2D<float4> t_partial : register(t14);\n");
            }
            let pixels = kernel.shape.pixels_x as usize * kernel.shape.pixels_y as usize;
            for p in 0..pixels {
                let mut original = String::new();
                emit_head(&mut original, &local, p);
                let previous = if step > 0 {
                    format!("a{p}_0 += t_partial.Load(int3(pix{p}-tile.xy,0));\n")
                } else {
                    String::new()
                };
                let replacement = if !last {
                    format!("{previous}u_partial[pix{p}-tile.xy]=a{p}_0;\n")
                } else {
                    format!("{previous}{original}")
                };
                if source.contains(&original) {
                    *source = source.replace(&original, &replacement);
                }
            }
        };
        adapt(&mut kernel.source_code);
        if let Some(ring) = kernel.ring.as_mut() {
            adapt(&mut ring.source_code);
        }
        kernel.head = last;
        let macs = part
            .inputs
            .iter()
            .map(|i| local.tensors[i.tensor] * i.kernel * i.kernel * part.out_channels)
            .sum();
        kernel.partial_head = Some(PartialHead {
            step,
            last,
            first_slice: first / 4,
            macs,
        });
        kernels.push(kernel);
    }
}
