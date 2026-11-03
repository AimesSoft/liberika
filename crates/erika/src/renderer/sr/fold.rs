//! Exact graph optimizations applied before code generation.
//!
//! 1. **Sibling merging.** mpv shaders emit four channels per pass, so one
//!    logical layer is often several passes with identical inputs. Merging
//!    them lets a backend load the input tile once and run one dispatch.
//! 2. **Linear folding.** A layer without activation feeding a convolution is
//!    folded into it: `W_c * (W_p * x + b_p) = (W_c * W_p) * x + W_c * b_p`.
//!    For ArtCNN this turns conv0 into a 5x5 luma filter inside conv1 and
//!    conv5 into a 5x5 filter inside conv6, cutting 26-33% of the MACs and two
//!    feature-map round trips. The intermediate tensor's padding is not
//!    linear in the input, so pixels on the one-pixel image ring get
//!    per-border-class weights ([`ConvLayer::border`]); the result is exact.

use super::model::{
    Activation, ConvInput, ConvLayer, INPUT_TENSOR, InputTransform, LayerWeights, Padding, SrModel,
};

/// Runs every optimization until none applies. The result is validated; if a
/// pass ever produced an invalid graph, the input is returned unchanged.
pub fn optimize(model: &SrModel) -> SrModel {
    let mut optimized = merge_siblings(model);
    while let Some(folded) = fold_one_linear(&optimized) {
        optimized = folded;
    }
    optimized.prune();
    match optimized.validate() {
        Ok(()) => optimized,
        Err(error) => {
            debug_assert!(
                model.validate().is_err(),
                "optimizing {} broke the graph: {error}",
                model.name
            );
            model.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// Sibling merging

#[derive(Clone, Copy, PartialEq)]
struct Part {
    tensor: usize,
    offset: usize,
}

/// Merges sibling layers. A residual reads a whole tensor, so a layer whose
/// output a residual reads may only merge when its readers merge the same
/// way; when they don't, that layer is kept apart and the pass reruns.
fn merge_siblings(model: &SrModel) -> SrModel {
    let mut apart = vec![false; model.tensors.len()];
    loop {
        match merge_siblings_except(model, &apart) {
            Ok(merged) => return merged,
            Err(split) => {
                let mut progressed = false;
                for tensor in split {
                    progressed |= !std::mem::replace(&mut apart[tensor], true);
                }
                if !progressed {
                    return model.clone();
                }
            }
        }
    }
}

/// One merging pass that never merges layers whose outputs are `apart`.
/// Fails with the residual tensors that ended up as slices of a wider tensor.
fn merge_siblings_except(model: &SrModel, apart: &[bool]) -> Result<SrModel, Vec<usize>> {
    // Old tensor -> (new tensor, channel offset).
    let mut map: Vec<Part> = vec![
        Part {
            tensor: usize::MAX,
            offset: 0
        };
        model.tensors.len()
    ];
    map[INPUT_TENSOR] = Part {
        tensor: INPUT_TENSOR,
        offset: 0,
    };
    let mut tensors = vec![1usize];
    let mut layers: Vec<ConvLayer> = Vec::new();
    let mut split = Vec::new();

    let mut index = 0;
    while index < model.layers.len() {
        // Grow a group of consecutive layers with identical remapped inputs.
        let signature = |layer: &ConvLayer| {
            layer
                .inputs
                .iter()
                .map(|input| (map[input.tensor], input.transform, input.kernel))
                .collect::<Vec<_>>()
        };
        let first = &model.layers[index];
        let first_signature = signature(first);
        let mut end = index + 1;
        while end < model.layers.len() && !apart[first.output] {
            let candidate = &model.layers[end];
            let same_inputs = signature(candidate) == first_signature;
            let same_activation = std::mem::discriminant(&candidate.activation)
                == std::mem::discriminant(&first.activation);
            let same_residual_kind = candidate.residual.is_some() == first.residual.is_some();
            let independent = model.layers[index..end].iter().all(|member| {
                candidate
                    .inputs
                    .iter()
                    .all(|input| input.tensor != member.output)
                    && candidate.residual != Some(member.output)
            });
            if !(same_inputs
                && same_activation
                && same_residual_kind
                && independent
                && !apart[candidate.output]
                && first.border.is_none()
                && candidate.border.is_none())
            {
                break;
            }
            end += 1;
        }
        let mut group = &model.layers[index..end];
        // Residuals must line up with one previously merged tensor.
        if group.len() > 1 && first.residual.is_some() {
            let mut offset = 0;
            let target = map[first.residual.expect("checked")].tensor;
            let aligned = group.iter().all(|layer| {
                let part = map[layer.residual.expect("same kind")];
                let ok = part.tensor == target && part.offset == offset;
                offset += layer.out_channels;
                ok
            }) && tensors[target] == offset;
            if !aligned {
                group = &model.layers[index..index + 1];
            }
        }

        let out_channels: usize = group.iter().map(|layer| layer.out_channels).sum();
        if let [layer] = group
            && let Some(residual) = layer.residual
        {
            let part = map[residual];
            if part.offset != 0 || tensors[part.tensor] != out_channels {
                split.push(residual);
            }
        }
        let output = tensors.len();
        tensors.push(out_channels);
        let mut merged = ConvLayer {
            output,
            out_channels,
            inputs: Vec::new(),
            bias: group.iter().flat_map(|layer| layer.bias.clone()).collect(),
            residual: group[0].residual.map(|tensor| map[tensor].tensor),
            activation: match &group[0].activation {
                Activation::Prelu(_) => Activation::Prelu(
                    group
                        .iter()
                        .flat_map(|layer| match &layer.activation {
                            Activation::Prelu(alpha) => alpha.clone(),
                            _ => unreachable!("same activation kind"),
                        })
                        .collect(),
                ),
                other => other.clone(),
            },
            border: None,
        };
        // Remap inputs; inputs reading parts of one merged tensor with the
        // same transform and kernel become one wide input.
        let mut placements = Vec::new();
        let mut out_base = 0;
        for layer in group {
            for input in &layer.inputs {
                let part = map[input.tensor];
                let full = tensors[part.tensor];
                let slot = merged.inputs.iter().position(|existing| {
                    existing.tensor == part.tensor
                        && existing.transform == input.transform
                        && existing.kernel == input.kernel
                });
                let slot = slot.unwrap_or_else(|| {
                    merged.inputs.push(ConvInput {
                        tensor: part.tensor,
                        transform: input.transform,
                        kernel: input.kernel,
                        weights: vec![0.0; out_channels * full * input.kernel * input.kernel],
                    });
                    merged.inputs.len() - 1
                });
                let placement = Placement {
                    slot,
                    out_base,
                    out_count: layer.out_channels,
                    channels: model.tensors[input.tensor],
                    full,
                    offset: part.offset,
                    taps: input.kernel * input.kernel,
                };
                placement.scatter(&mut merged.inputs[slot].weights, &input.weights);
                placements.push(placement);
            }
            out_base += layer.out_channels;
        }
        // Only a lone layer can carry border variants; they move with its
        // inputs.
        if let [layer] = group
            && let Some(variants) = &layer.border
        {
            merged.border = Some(
                variants
                    .iter()
                    .map(|variant| {
                        let mut inputs: Vec<Vec<f32>> = merged
                            .inputs
                            .iter()
                            .map(|input| vec![0.0; input.weights.len()])
                            .collect();
                        for (placement, weights) in placements.iter().zip(&variant.inputs) {
                            placement.scatter(&mut inputs[placement.slot], weights);
                        }
                        LayerWeights {
                            inputs,
                            bias: variant.bias.clone(),
                        }
                    })
                    .collect(),
            );
        }
        let mut offset = 0;
        for layer in group {
            map[layer.output] = Part {
                tensor: output,
                offset,
            };
            offset += layer.out_channels;
        }
        layers.push(merged);
        index += group.len();
    }
    if !split.is_empty() {
        return Err(split);
    }

    let head_part = map[model.head.tensor];
    let mut merged = SrModel {
        name: model.name.clone(),
        padding: model.padding,
        input_range: model.input_range,
        tensors,
        layers,
        head: model.head,
    };
    if head_part.offset == 0 && merged.tensors[head_part.tensor] == 4 {
        merged.head.tensor = head_part.tensor;
        Ok(merged)
    } else {
        // The head was merged into a wider tensor; keep the original graph.
        Ok(model.clone())
    }
}

/// Where one layer input's weights land in a merged, wider input.
struct Placement {
    slot: usize,
    out_base: usize,
    out_count: usize,
    /// Channels of the original input tensor and of the merged one it is a
    /// slice of, starting at `offset`.
    channels: usize,
    full: usize,
    offset: usize,
    taps: usize,
}

impl Placement {
    fn scatter(&self, target: &mut [f32], weights: &[f32]) {
        for o in 0..self.out_count {
            for c in 0..self.channels {
                for tap in 0..self.taps {
                    target
                        [((self.out_base + o) * self.full + self.offset + c) * self.taps + tap] +=
                        weights[(o * self.channels + c) * self.taps + tap];
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Linear folding

fn consumers(model: &SrModel, tensor: usize) -> (Vec<(usize, usize)>, Vec<usize>, bool) {
    let mut conv = Vec::new();
    let mut residual = Vec::new();
    let mut other = false;
    for (layer_index, layer) in model.layers.iter().enumerate() {
        for (input_index, input) in layer.inputs.iter().enumerate() {
            if input.tensor == tensor {
                if input.transform == InputTransform::Identity && input.kernel <= 3 {
                    conv.push((layer_index, input_index));
                } else {
                    other = true;
                }
            }
        }
        if layer.residual == Some(tensor) {
            residual.push(layer_index);
        }
    }
    if model.head.tensor == tensor {
        other = true;
    }
    (conv, residual, other)
}

/// Folds the first profitable linear producer into its consumers.
fn fold_one_linear(model: &SrModel) -> Option<SrModel> {
    for (producer_index, producer) in model.layers.iter().enumerate() {
        if producer.activation != Activation::None || producer.border.is_some() {
            continue;
        }
        let tensor = producer.output;
        let (conv, residual, other) = consumers(model, tensor);
        if other || (conv.is_empty() && residual.is_empty()) {
            continue;
        }
        // Each folding consumer may read the tensor once and must not have
        // border variants of its own.
        let mut seen = Vec::new();
        let foldable = conv.iter().all(|&(layer, _)| {
            let fresh = !seen.contains(&layer);
            seen.push(layer);
            fresh && model.layers[layer].border.is_none()
        });
        if !foldable {
            continue;
        }

        let mut candidate = model.clone();
        for &(layer_index, input_index) in &conv {
            let consumer = &mut candidate.layers[layer_index];
            compose_into(model, producer, consumer, input_index);
        }
        for &layer_index in &residual {
            let consumer = &mut candidate.layers[layer_index];
            consumer.residual = producer.residual;
            add_bias(consumer, &producer.bias);
            for input in &producer.inputs {
                add_operand(consumer, input);
            }
        }
        candidate.layers.remove(producer_index);
        candidate.prune();
        if candidate.macs_per_pixel() < model.macs_per_pixel() && candidate.validate().is_ok() {
            return Some(candidate);
        }
    }
    None
}

/// Adds a position-independent bias to a layer and all its border variants.
fn add_bias(layer: &mut ConvLayer, bias: &[f32]) {
    for (a, b) in layer.bias.iter_mut().zip(bias) {
        *a += b;
    }
    for variant in layer.border.iter_mut().flatten() {
        for (a, b) in variant.bias.iter_mut().zip(bias) {
            *a += b;
        }
    }
}

/// Adds a position-independent operand, merging with an identical one and
/// keeping border variants aligned with `inputs`.
fn add_operand(layer: &mut ConvLayer, input: &ConvInput) {
    let slot = layer.inputs.iter().position(|existing| {
        existing.tensor == input.tensor
            && existing.transform == input.transform
            && existing.kernel == input.kernel
    });
    match slot {
        Some(slot) => {
            for (a, b) in layer.inputs[slot].weights.iter_mut().zip(&input.weights) {
                *a += b;
            }
            for variant in layer.border.iter_mut().flatten() {
                for (a, b) in variant.inputs[slot].iter_mut().zip(&input.weights) {
                    *a += b;
                }
            }
        }
        None => {
            layer.inputs.push(input.clone());
            for variant in layer.border.iter_mut().flatten() {
                variant.inputs.push(input.weights.clone());
            }
        }
    }
}

/// Replaces `consumer.inputs[input_index]` (an identity read of the
/// producer's output) with the producer's inputs convolved by the consumer's
/// kernel, adding per-border-class variants.
fn compose_into(
    model: &SrModel,
    producer: &ConvLayer,
    consumer: &mut ConvLayer,
    input_index: usize,
) {
    let read = consumer.inputs.remove(input_index);
    let mid = producer.out_channels;
    let out = consumer.out_channels;
    let kc = read.kernel;
    let rc = (kc / 2) as isize;

    // Effective consumer tap (ty, tx) for a border class, or None when the
    // tap reads zero padding.
    let effective_tap = |class: (usize, usize), ty: isize, tx: isize| -> Option<(isize, isize)> {
        let adjust = |class: usize, t: isize| -> Option<isize> {
            let outside = (class == 0 && t < 0) || (class == 2 && t > 0);
            match (outside, model.padding) {
                (false, _) => Some(t),
                (true, Padding::Zero) => None,
                (true, Padding::Clamp) => Some(0),
            }
        };
        Some((adjust(class.0, ty)?, adjust(class.1, tx)?))
    };

    // Builds composed weights for every producer operand (and the producer's
    // residual, which the consumer kernel convolves directly) plus the bias.
    let compose = |class: (usize, usize)| -> (Vec<ConvInput>, Vec<f32>) {
        let mut inputs = Vec::new();
        for operand in &producer.inputs {
            let channels = model.tensors[operand.tensor];
            let kp = operand.kernel;
            let rp = (kp / 2) as isize;
            let k = kp + kc - 1;
            let r = (k / 2) as isize;
            let mut weights = vec![0.0f32; out * channels * k * k];
            for ty in -rc..=rc {
                for tx in -rc..=rc {
                    let Some((ey, ex)) = effective_tap(class, ty, tx) else {
                        continue;
                    };
                    let consumer_tap = ((ty + rc) as usize) * kc + (tx + rc) as usize;
                    for py in -rp..=rp {
                        for px in -rp..=rp {
                            let producer_tap = ((py + rp) as usize) * kp + (px + rp) as usize;
                            let dy = (ey + py + r) as usize;
                            let dx = (ex + px + r) as usize;
                            for o in 0..out {
                                for m in 0..mid {
                                    let wc = read.weights[(o * mid + m) * kc * kc + consumer_tap];
                                    if wc == 0.0 {
                                        continue;
                                    }
                                    for c in 0..channels {
                                        let wp = operand.weights
                                            [(m * channels + c) * kp * kp + producer_tap];
                                        weights[((o * channels + c) * k + dy) * k + dx] += wc * wp;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            inputs.push(ConvInput {
                tensor: operand.tensor,
                transform: operand.transform,
                kernel: k,
                weights,
            });
        }
        if let Some(residual) = producer.residual {
            // consumer_kernel * residual, with the same tap mapping.
            let mut weights = vec![0.0f32; out * mid * kc * kc];
            for ty in -rc..=rc {
                for tx in -rc..=rc {
                    let Some((ey, ex)) = effective_tap(class, ty, tx) else {
                        continue;
                    };
                    let source = ((ty + rc) as usize) * kc + (tx + rc) as usize;
                    let target = ((ey + rc) as usize) * kc + (ex + rc) as usize;
                    for o in 0..out {
                        for m in 0..mid {
                            weights[(o * mid + m) * kc * kc + target] +=
                                read.weights[(o * mid + m) * kc * kc + source];
                        }
                    }
                }
            }
            inputs.push(ConvInput {
                tensor: residual,
                transform: InputTransform::Identity,
                kernel: kc,
                weights,
            });
        }
        let mut bias = vec![0.0f32; out];
        for ty in -rc..=rc {
            for tx in -rc..=rc {
                if effective_tap(class, ty, tx).is_none() {
                    continue;
                }
                let tap = ((ty + rc) as usize) * kc + (tx + rc) as usize;
                for (o, bias) in bias.iter_mut().enumerate() {
                    for m in 0..mid {
                        *bias += read.weights[(o * mid + m) * kc * kc + tap] * producer.bias[m];
                    }
                }
            }
        }
        (inputs, bias)
    };

    let needs_border = kc > 1;
    let (interior_inputs, interior_bias) = compose((1, 1));
    let base_inputs = consumer.inputs.clone();
    let base_bias = consumer.bias.clone();
    for (o, bias) in consumer.bias.iter_mut().enumerate() {
        *bias += interior_bias[o];
    }
    consumer.inputs.extend(interior_inputs);
    if needs_border {
        let variants = (0..9)
            .map(|index| {
                let (inputs, bias) = compose((index / 3, index % 3));
                LayerWeights {
                    inputs: base_inputs
                        .iter()
                        .map(|input| input.weights.clone())
                        .chain(inputs.into_iter().map(|input| input.weights))
                        .collect(),
                    bias: base_bias.iter().zip(&bias).map(|(a, b)| a + b).collect(),
                }
            })
            .collect::<Vec<_>>();
        debug_assert_eq!(variants[4].inputs.len(), consumer.inputs.len());
        consumer.border = Some(variants);
    }
}

impl SrModel {
    /// Drops layers whose outputs are never read and renumbers tensors in
    /// write order.
    pub fn prune(&mut self) {
        loop {
            let mut used = vec![false; self.tensors.len()];
            used[self.head.tensor] = true;
            for layer in &self.layers {
                for input in &layer.inputs {
                    used[input.tensor] = true;
                }
                if let Some(residual) = layer.residual {
                    used[residual] = true;
                }
            }
            let before = self.layers.len();
            self.layers.retain(|layer| used[layer.output]);
            if self.layers.len() == before {
                break;
            }
        }
        let mut remap = vec![usize::MAX; self.tensors.len()];
        remap[INPUT_TENSOR] = INPUT_TENSOR;
        let mut tensors = vec![1];
        for layer in &self.layers {
            remap[layer.output] = tensors.len();
            tensors.push(self.tensors[layer.output]);
        }
        for layer in &mut self.layers {
            layer.output = remap[layer.output];
            for input in &mut layer.inputs {
                input.tensor = remap[input.tensor];
            }
            if let Some(residual) = layer.residual.as_mut() {
                *residual = remap[*residual];
            }
        }
        self.head.tensor = remap[self.head.tensor];
        self.tensors = tensors;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::artcnn::model_for_mode;
    use crate::renderer::pipeline::LumaUpscalerMode;
    use crate::renderer::sr::reference::{LumaRange, run, test_pattern};

    #[test]
    fn artcnn_folds_into_five_layers_and_stays_exact() {
        let source = crate::renderer::sr::artcnn::lower(
            "c4f16",
            model_for_mode(LumaUpscalerMode::ArtCnnC4F16).unwrap(),
        )
        .unwrap();
        let folded = optimize(&source);
        assert_eq!(folded.layers.len(), 5);
        assert_eq!(source.macs_per_pixel(), 12_240);
        assert_eq!(folded.macs_per_pixel(), 9_012);
        // Same receptive field: 7 pixels.
        assert_eq!(folded.tensor_halos()[0], 7);
        for (width, height) in [(19, 11), (2, 2), (3, 7)] {
            let luma = test_pattern(width, height, 3);
            let a = run(&source, &luma, width, height, LumaRange::Full);
            let b = run(&folded, &luma, width, height, LumaRange::Full);
            let max = a
                .iter()
                .zip(&b)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(max < 1e-4, "{width}x{height}: {max}");
        }
    }

    fn max_difference(a: &SrModel, b: &SrModel, width: usize, height: usize) -> f32 {
        let luma = test_pattern(width, height, 3);
        let a = run(a, &luma, width, height, LumaRange::Full);
        let b = run(b, &luma, width, height, LumaRange::Full);
        a.iter()
            .zip(&b)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max)
    }

    fn weights(count: usize, seed: usize) -> Vec<f32> {
        (0..count)
            .map(|i| (((i * 7 + seed * 13) % 11) as f32 - 5.0) * 0.05)
            .collect()
    }

    fn conv(output: usize, out_channels: usize, input: ConvInput) -> ConvLayer {
        ConvLayer {
            output,
            out_channels,
            bias: weights(out_channels, output),
            inputs: vec![input],
            residual: None,
            activation: Activation::Relu,
            border: None,
        }
    }

    fn read(tensor: usize, channels: usize, out: usize, kernel: usize, seed: usize) -> ConvInput {
        ConvInput {
            tensor,
            transform: InputTransform::Identity,
            kernel,
            weights: weights(out * channels * kernel * kernel, seed),
        }
    }

    #[test]
    fn merged_siblings_keep_the_residual_slice() {
        // Layers 1 and 2 are siblings; layer 3 adds layer 2's output, which
        // merging would turn into the second half of a 4-channel tensor.
        let mut residual = conv(3, 2, read(1, 2, 2, 1, 3));
        residual.residual = Some(2);
        let mut last = conv(4, 4, read(3, 2, 4, 3, 4));
        last.activation = Activation::None;
        let source = SrModel {
            name: "residual-slice".to_string(),
            padding: Padding::Zero,
            input_range: crate::renderer::sr::model::InputRange::Raw,
            tensors: vec![1, 2, 2, 2, 4],
            layers: vec![
                conv(1, 2, read(0, 1, 2, 3, 1)),
                conv(2, 2, read(0, 1, 2, 3, 2)),
                residual,
                last,
            ],
            head: crate::renderer::sr::model::Head {
                tensor: 4,
                order: crate::renderer::sr::model::SubpixelOrder::RowMajor,
                base: crate::renderer::sr::model::HeadBase::None,
                clamp: false,
            },
        };
        source.validate().unwrap();
        let optimized = optimize(&source);
        optimized.validate().unwrap();
        for (width, height) in [(7, 5), (2, 3)] {
            let max = max_difference(&source, &optimized, width, height);
            assert!(max < 1e-5, "{width}x{height}: {max}");
        }
    }

    #[test]
    fn reoptimizing_keeps_border_variants() {
        // A pre-optimized network saved as .esr and loaded as a custom model.
        let source = crate::renderer::sr::artcnn::lower(
            "c4f16",
            model_for_mode(LumaUpscalerMode::ArtCnnC4F16).unwrap(),
        )
        .unwrap();
        let folded = optimize(&source);
        let bordered = |model: &SrModel| {
            model
                .layers
                .iter()
                .filter(|layer| layer.border.is_some())
                .count()
        };
        assert_eq!(bordered(&folded), 2);
        let loaded = SrModel::from_bytes(&folded.to_bytes(true)).unwrap();
        let again = optimize(&loaded);
        assert_eq!(bordered(&again), 2);
        assert_eq!(again.macs_per_pixel(), folded.macs_per_pixel());
        for (width, height) in [(7, 5), (19, 11), (2, 2)] {
            let max = max_difference(&source, &again, width, height);
            assert!(max < 1e-4, "{width}x{height}: {max}");
        }
    }

    #[test]
    fn clamp_padding_folds_exactly() {
        let mut source = crate::renderer::sr::artcnn::lower(
            "c4f16",
            model_for_mode(LumaUpscalerMode::ArtCnnC4F16).unwrap(),
        )
        .unwrap();
        source.padding = Padding::Clamp;
        let folded = optimize(&source);
        assert_eq!(folded.layers.len(), 5);
        let luma = test_pattern(13, 9, 5);
        let a = run(&source, &luma, 13, 9, LumaRange::Full);
        let b = run(&folded, &luma, 13, 9, LumaRange::Full);
        let max = a
            .iter()
            .zip(&b)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max < 1e-4, "{max}");
    }
}
