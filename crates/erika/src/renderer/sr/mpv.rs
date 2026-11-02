//! Imports convolutional networks published as mpv user shaders.
//!
//! Two dialects are understood, which covers Anime4K's v3.2 CNN family and
//! igv's FSRCNNX:
//!
//! - Anime4K (`//!HOOK MAIN`): `#define go_N(x_off, y_off) (...)` /
//!   `#define g_N (...)` operand macros (optionally CReLU halves), statements
//!   `result += mat4(...) * go_N(dx, dy);`, `result += vec4(bias);` and a
//!   Depth-to-Space pass adding the residual to `MAIN_tex(MAIN_pos)`.
//! - FSRCNNX (`//!HOOK LUMA`): `res += mat4(...) * TEX_texOff(vec2(dx, dy));`,
//!   `res += vec4(...) * float(LUMA_texOff(vec2(dx, dy)));`, residual adds
//!   `res += TEX_texOff(0);`, PReLU and an aggregation pass.
//!
//! Only the weights and graph are extracted; nothing is interpreted at render
//! time. Each mpv texture becomes a four-channel tensor and each pass a layer;
//! [`super::fold::optimize`] later merges sibling passes.
//!
//! RGB hooks (`MAIN`) are reduced to luma: the network sees `R = G = B = Y`
//! (so input weights are summed over RGB, and the constant alpha folds into
//! the bias) and RGB residual heads are combined with BT.709 coefficients.

use std::collections::HashMap;

use super::model::{
    Activation, ConvInput, ConvLayer, Head, HeadBase, INPUT_TENSOR, InputRange, InputTransform,
    Padding, SrModel, SubpixelOrder,
};
use crate::core::{PlayerError, Result};

/// BT.709 luma coefficients used to fold RGB residual heads.
const LUMA_RGB: [f32; 3] = [0.2126, 0.7152, 0.0722];

struct Pass {
    desc: String,
    hook: String,
    binds: Vec<String>,
    save: Option<String>,
    body: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct OperandKey {
    tensor: usize,
    transform: InputTransform,
}

/// An operand reference such as `go_0(-1.0, 1.0)` resolved to a tensor tap.
#[derive(Clone, Copy)]
struct Operand {
    key: OperandKey,
    dx: i32,
    dy: i32,
    /// Channels supplied by the reference (4, or 1 for `float(...)`).
    channels: usize,
}

pub fn parse(name: &str, source: &str) -> Result<SrModel> {
    let passes = split_passes(source)?;
    let hook = passes
        .first()
        .map(|pass| pass.hook.clone())
        .ok_or_else(|| error(name, "no passes"))?;
    let rgb_input = match hook.as_str() {
        "MAIN" => true,
        "LUMA" => false,
        other => return Err(error(name, &format!("unsupported hook {other}"))),
    };

    let mut tensors = vec![1usize];
    let mut names: HashMap<String, usize> = HashMap::new();
    names.insert(hook.clone(), INPUT_TENSOR);
    let mut layers: Vec<ConvLayer> = Vec::new();
    let mut head: Option<Head> = None;
    let mut head_sources: Vec<usize> = Vec::new();

    for pass in &passes {
        if pass.hook != hook {
            return Err(error(name, "passes hook different textures"));
        }
        if head.is_some() {
            return Err(error(name, "passes follow the output pass"));
        }
        let writes_hooked = pass.save.as_deref().is_none_or(|save| save == hook);
        if writes_hooked {
            let (parsed, sources) = parse_head(name, pass, &hook, &names)?;
            head = Some(parsed);
            head_sources = sources;
            continue;
        }
        let output = tensors.len();
        tensors.push(4);
        let layer = parse_conv(name, pass, &hook, rgb_input, &names, &tensors, output)?;
        layers.push(layer);
        names.insert(pass.save.clone().expect("checked above"), output);
    }

    let mut head = head.ok_or_else(|| error(name, "no output pass"))?;
    match head_sources.len() {
        1 => head.tensor = head_sources[0],
        3 => {
            let output = tensors.len();
            tensors.push(4);
            let folded = fold_rgb_layers(name, &layers, &head_sources, output)?;
            layers.push(folded);
            head.tensor = output;
        }
        count => {
            return Err(error(
                name,
                &format!("output pass reads {count} textures; expected 1 or 3"),
            ));
        }
    }

    let model = SrModel {
        name: name.to_string(),
        padding: Padding::Clamp,
        input_range: if rgb_input {
            InputRange::Full
        } else {
            InputRange::Raw
        },
        tensors,
        layers,
        head,
    };
    let mut model = model;
    model.prune();
    model.validate()?;
    Ok(model)
}

fn split_passes(source: &str) -> Result<Vec<Pass>> {
    let mut passes = Vec::new();
    let mut current: Option<Pass> = None;
    let mut in_header = false;
    for raw in source.lines() {
        let line = raw.trim();
        if let Some(directive) = line.strip_prefix("//!") {
            if !in_header {
                passes.extend(current.take());
                current = Some(Pass {
                    desc: String::new(),
                    hook: String::new(),
                    binds: Vec::new(),
                    save: None,
                    body: Vec::new(),
                });
                in_header = true;
            }
            let pass = current.as_mut().expect("pass started");
            let (key, value) = directive.split_once(' ').unwrap_or((directive, ""));
            let value = value.trim().to_string();
            match key {
                "DESC" => pass.desc = value,
                "HOOK" => pass.hook = value,
                "BIND" => pass.binds.push(value),
                "SAVE" => pass.save = Some(value),
                _ => {}
            }
            continue;
        }
        in_header = false;
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(pass) = current.as_mut() {
            pass.body.push(line.to_string());
        }
    }
    passes.extend(current);
    if passes.iter().any(|pass| pass.hook.is_empty()) {
        return Err(PlayerError::Renderer(
            "mpv shader pass without //!HOOK".to_string(),
        ));
    }
    Ok(passes)
}

fn parse_conv(
    name: &str,
    pass: &Pass,
    hook: &str,
    rgb_input: bool,
    names: &HashMap<String, usize>,
    tensors: &[usize],
    output: usize,
) -> Result<ConvLayer> {
    let fail = |message: &str| error(name, &format!("pass {:?}: {message}", pass.desc));
    let resolve = |texture: &str| -> Result<usize> {
        names
            .get(texture)
            .copied()
            .ok_or_else(|| fail(&format!("unknown texture {texture}")))
    };

    // Operand macros.
    let mut macros: HashMap<String, (OperandKey, bool)> = HashMap::new();
    for line in &pass.body {
        let Some(rest) = line.strip_prefix("#define ") else {
            continue;
        };
        let (macro_name, expansion) = rest
            .split_once(' ')
            .ok_or_else(|| fail("malformed #define"))?;
        let macro_name = macro_name
            .split('(')
            .next()
            .unwrap_or(macro_name)
            .to_string();
        let transform = if expansion.contains("max(-") {
            InputTransform::NegRelu
        } else if expansion.contains("max(") {
            InputTransform::Relu
        } else {
            InputTransform::Identity
        };
        let texture = texture_in(expansion).ok_or_else(|| fail("macro without texture"))?;
        let tensor = resolve(&texture)?;
        let one_by_one = !expansion.contains("texOff");
        macros.insert(macro_name, (OperandKey { tensor, transform }, one_by_one));
    }

    let mut bias = [0.0f32; 4];
    // (operand, dx, dy) -> 4 x channels weights, out-major.
    let mut taps: HashMap<(OperandKey, i32, i32), Vec<f32>> = HashMap::new();
    let mut order: Vec<OperandKey> = Vec::new();
    let mut residual = None;
    let mut activation = Activation::None;

    for line in &pass.body {
        if line.starts_with("#define")
            || line.starts_with("vec4 hook")
            || line == "{"
            || line == "}"
        {
            continue;
        }
        if line.starts_with("return") {
            continue;
        }
        if line.contains("max(res, vec4(0.0))") || line.contains("max(result, vec4(0.0))") {
            let alpha = vec4_literals(line)
                .into_iter()
                .find(|values| values.len() == 4)
                .ok_or_else(|| fail("malformed PReLU"))?;
            activation = Activation::Prelu(alpha);
            continue;
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            return Err(fail(&format!("unrecognized statement {line}")));
        };
        let accumulate = lhs.trim_end().ends_with('+');
        let rhs = rhs.trim().trim_end_matches(';').trim();
        let declares = lhs.contains("vec4 result") || lhs.contains("vec4 res");
        if !accumulate && !declares {
            return Err(fail(&format!("unrecognized statement {line}")));
        }
        if let Some((matrix, operand)) = split_product(rhs) {
            let coefficients = parse_floats_call(matrix).ok_or_else(|| fail("bad matrix"))?;
            let operand = resolve_operand(operand, &macros, names, hook)
                .ok_or_else(|| fail(&format!("bad operand {operand}")))?;
            let weights = match (coefficients.len(), operand.channels) {
                // GLSL matrices are column-major: column j multiplies input j.
                (16, 4) => (0..4)
                    .flat_map(|o| (0..4).map(move |j| (o, j)))
                    .map(|(o, j)| coefficients[j * 4 + o])
                    .collect::<Vec<_>>(),
                (4, 1) => coefficients.clone(),
                _ => return Err(fail("operand and coefficient shapes disagree")),
            };
            let key = operand.key;
            let (weights, alpha_bias) = if key.tensor == INPUT_TENSOR && rgb_input {
                // R = G = B = Y; alpha is 1 everywhere.
                let luma = (0..4)
                    .map(|o| weights[o * 4] + weights[o * 4 + 1] + weights[o * 4 + 2])
                    .collect::<Vec<_>>();
                let alpha = (0..4).map(|o| weights[o * 4 + 3]).collect::<Vec<_>>();
                (luma, Some(alpha))
            } else {
                (weights, None)
            };
            if let Some(alpha) = alpha_bias {
                for o in 0..4 {
                    bias[o] += alpha[o];
                }
            }
            if !order.contains(&key) {
                order.push(key);
            }
            let entry = taps
                .entry((key, operand.dx, operand.dy))
                .or_insert_with(|| vec![0.0; weights.len()]);
            for (slot, value) in entry.iter_mut().zip(&weights) {
                *slot += value;
            }
            continue;
        }
        if let Some(values) = parse_floats_call(rhs).filter(|values| values.len() == 4) {
            for o in 0..4 {
                bias[o] += values[o];
            }
            continue;
        }
        if let Some(texture) = rhs.strip_suffix("_texOff(0)") {
            residual = Some(resolve(texture)?);
            continue;
        }
        return Err(fail(&format!("unrecognized statement {line}")));
    }

    let mut inputs = Vec::new();
    for key in order {
        let channels = tensors[key.tensor];
        let radius = taps
            .keys()
            .filter(|(tap_key, _, _)| *tap_key == key)
            .map(|(_, dx, dy)| dx.abs().max(dy.abs()))
            .max()
            .unwrap_or(0) as usize;
        let kernel = radius * 2 + 1;
        let mut weights = vec![0.0f32; 4 * channels * kernel * kernel];
        for ((tap_key, dx, dy), values) in &taps {
            if *tap_key != key {
                continue;
            }
            let kx = (dx + radius as i32) as usize;
            let ky = (dy + radius as i32) as usize;
            let tap_channels = values.len() / 4;
            if tap_channels != channels {
                return Err(fail("operand channel count changed between taps"));
            }
            for o in 0..4 {
                for c in 0..channels {
                    weights[((o * channels + c) * kernel + ky) * kernel + kx] =
                        values[o * channels + c];
                }
            }
        }
        inputs.push(ConvInput {
            tensor: key.tensor,
            transform: key.transform,
            kernel,
            weights,
        });
    }
    if inputs.is_empty() {
        return Err(fail("pass has no convolution terms"));
    }
    Ok(ConvLayer {
        output,
        out_channels: 4,
        inputs,
        bias: bias.to_vec(),
        residual,
        activation,
        border: None,
    })
}

fn parse_head(
    name: &str,
    pass: &Pass,
    hook: &str,
    names: &HashMap<String, usize>,
) -> Result<(Head, Vec<usize>)> {
    let body = pass.body.join("\n");
    let fail = |message: &str| error(name, &format!("output pass {:?}: {message}", pass.desc));
    let sources = pass
        .binds
        .iter()
        .filter(|bind| bind.as_str() != hook)
        .map(|bind| {
            names
                .get(bind)
                .copied()
                .ok_or_else(|| fail(&format!("unknown texture {bind}")))
        })
        .collect::<Result<Vec<_>>>()?;
    let order = if body.contains("index.x * 2 + index.y") {
        SubpixelOrder::ColumnMajor
    } else if body.contains(".y * 2 + i0.x") || body.contains("index.y * 2 + index.x") {
        SubpixelOrder::RowMajor
    } else {
        return Err(fail("unrecognized depth-to-space indexing"));
    };
    let base = if body.contains(&format!("+ {hook}_tex({hook}_pos)")) {
        HeadBase::Bilinear
    } else {
        HeadBase::None
    };
    Ok((
        Head {
            tensor: 0,
            order,
            base,
            clamp: false,
        },
        sources,
    ))
}

/// Combines the R, G and B residual layers into one luma residual layer.
fn fold_rgb_layers(
    name: &str,
    layers: &[ConvLayer],
    sources: &[usize],
    output: usize,
) -> Result<ConvLayer> {
    let producers = sources
        .iter()
        .map(|tensor| {
            layers
                .iter()
                .find(|layer| layer.output == *tensor)
                .ok_or_else(|| error(name, "RGB head source is not a layer"))
        })
        .collect::<Result<Vec<_>>>()?;
    let first = producers[0];
    let compatible = producers.iter().all(|layer| {
        layer.activation == Activation::None
            && layer.residual.is_none()
            && layer.inputs.len() == first.inputs.len()
            && layer.inputs.iter().zip(&first.inputs).all(|(a, b)| {
                a.tensor == b.tensor && a.transform == b.transform && a.kernel == b.kernel
            })
    });
    if !compatible {
        return Err(error(name, "RGB head layers cannot be combined"));
    }
    let mut folded = ConvLayer {
        output,
        out_channels: 4,
        inputs: first
            .inputs
            .iter()
            .map(|input| ConvInput {
                weights: vec![0.0; input.weights.len()],
                ..input.clone()
            })
            .collect(),
        bias: vec![0.0; 4],
        residual: None,
        activation: Activation::None,
        border: None,
    };
    for (layer, coefficient) in producers.iter().zip(LUMA_RGB) {
        for (target, source) in folded.inputs.iter_mut().zip(&layer.inputs) {
            for (a, b) in target.weights.iter_mut().zip(&source.weights) {
                *a += coefficient * b;
            }
        }
        for (a, b) in folded.bias.iter_mut().zip(&layer.bias) {
            *a += coefficient * b;
        }
    }
    Ok(folded)
}

/// `mat4(...) * operand` -> (`mat4(...)`, `operand`), splitting at the `*`
/// that follows the matrix's closing parenthesis.
fn split_product(expression: &str) -> Option<(&str, &str)> {
    if !(expression.starts_with("mat4") || expression.starts_with("vec4(")) {
        return None;
    }
    let close = matching_paren(expression, expression.find('(')?)?;
    let rest = expression[close + 1..].trim_start();
    let operand = rest.strip_prefix('*')?.trim();
    Some((&expression[..=close], operand))
}

fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, ch) in text.char_indices().skip_while(|(index, _)| *index < open) {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Parses the numeric arguments of `mat4(...)`, `mat4x4(...)` or `vec4(...)`.
fn parse_floats_call(call: &str) -> Option<Vec<f32>> {
    let open = call.find('(')?;
    let head = &call[..open];
    if !matches!(head, "mat4" | "mat4x4" | "vec4") {
        return None;
    }
    let close = matching_paren(call, open)?;
    if close != call.len() - 1 {
        return None;
    }
    call[open + 1..close]
        .split(',')
        .map(|value| value.trim().parse::<f32>().ok())
        .collect()
}

/// Every `vec4(...)` literal on a line.
fn vec4_literals(line: &str) -> Vec<Vec<f32>> {
    let mut literals = Vec::new();
    let mut search = 0;
    while let Some(found) = line[search..].find("vec4(") {
        let start = search + found;
        let open = start + 4;
        let Some(close) = matching_paren(line, open) else {
            break;
        };
        if let Some(values) = parse_floats_call(&line[start..=close]) {
            literals.push(values);
        }
        search = open + 1;
    }
    literals
}

/// Texture name referenced by `NAME_texOff(` or `NAME_tex(`.
fn texture_in(text: &str) -> Option<String> {
    let index = text.find("_texOff(").or_else(|| text.find("_tex("))?;
    let start = text[..index]
        .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .map_or(0, |position| position + 1);
    Some(text[start..index].to_string())
}

fn resolve_operand(
    operand: &str,
    macros: &HashMap<String, (OperandKey, bool)>,
    names: &HashMap<String, usize>,
    hook: &str,
) -> Option<Operand> {
    let operand = operand.trim();
    // Macro forms: go_N(dx, dy) or g_N.
    let macro_name = operand.split('(').next()?.trim();
    if let Some(&(key, one_by_one)) = macros.get(macro_name) {
        let (dx, dy) = if one_by_one {
            (0, 0)
        } else {
            offsets(operand)?
        };
        return Some(Operand {
            key,
            dx,
            dy,
            channels: 4,
        });
    }
    // Direct forms: float(LUMA_texOff(vec2(dx,dy))) or TEX_texOff(vec2(dx,dy)).
    let (inner, channels) = match operand.strip_prefix("float(") {
        Some(inner) => (inner.strip_suffix(')')?, 1),
        None => (operand, 4),
    };
    let texture = texture_in(inner)?;
    let tensor = *names.get(&texture)?;
    if channels == 1 && texture != hook {
        return None;
    }
    let (dx, dy) = offsets(inner)?;
    Some(Operand {
        key: OperandKey {
            tensor,
            transform: InputTransform::Identity,
        },
        dx,
        dy,
        channels,
    })
}

/// Extracts the integer tap offsets from `...(vec2(dx, dy))` or `go_N(dx, dy)`.
fn offsets(text: &str) -> Option<(i32, i32)> {
    let open = text.rfind('(')?;
    let close = text[open..].find(')')? + open;
    let mut values = text[open + 1..close]
        .split(',')
        .map(|value| value.trim().parse::<f32>().ok());
    let dx = values.next()??;
    let dy = values.next()??;
    if values.next().is_some() || dx.fract() != 0.0 || dy.fract() != 0.0 {
        return None;
    }
    Some((dx as i32, dy as i32))
}

fn error(name: &str, message: &str) -> PlayerError {
    PlayerError::Renderer(format!("cannot import mpv shader {name:?}: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::sr::reference::{LumaRange, run, test_pattern};

    const ANIME4K_LIKE: &str = r#"
// MIT License
//!DESC Example-Conv-4x3x3x3
//!HOOK MAIN
//!BIND MAIN
//!SAVE conv2d_tf
//!WIDTH MAIN.w
//!HEIGHT MAIN.h
//!COMPONENTS 4
#define go_0(x_off, y_off) (MAIN_texOff(vec2(x_off, y_off)))
vec4 hook() {
    vec4 result = mat4(0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.2, 0.0, 0.0, 0.0, 0.0) * go_0(-1.0, -1.0);
    result += mat4(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0) * go_0(1.0, 1.0);
    result += vec4(0.01, 0.02, 0.03, 0.04);
    return result;
}
//!DESC Example-Conv-4x1x1x8
//!HOOK MAIN
//!BIND conv2d_tf
//!SAVE conv2d_last_tf
//!COMPONENTS 4
#define g_0 (max((conv2d_tf_tex(conv2d_tf_pos)), 0.0))
#define g_1 (max(-(conv2d_tf_tex(conv2d_tf_pos)), 0.0))
vec4 hook() {
    vec4 result = mat4(1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0) * g_0;
    result += mat4(-1.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, -1.0) * g_1;
    return result;
}
//!DESC Example-Depth-to-Space
//!HOOK MAIN
//!BIND MAIN
//!BIND conv2d_last_tf
//!SAVE MAIN
vec4 hook() {
    vec2 f0 = fract(conv2d_last_tf_pos * conv2d_last_tf_size);
    ivec2 i0 = ivec2(f0 * vec2(2.0));
    float c0 = conv2d_last_tf_tex((vec2(0.5) - f0) * conv2d_last_tf_pt + conv2d_last_tf_pos)[i0.y * 2 + i0.x];
    float c1 = c0;
    float c2 = c1;
    float c3 = c2;
    return vec4(c0, c1, c2, c3) + MAIN_tex(MAIN_pos);
}
"#;

    #[test]
    fn imports_anime4k_dialect() {
        let model = parse("example", ANIME4K_LIKE).unwrap();
        assert_eq!(model.padding, Padding::Clamp);
        assert_eq!(model.input_range, InputRange::Full);
        assert_eq!(model.tensors, vec![1, 4, 4]);
        assert_eq!(model.head.base, HeadBase::Bilinear);
        assert_eq!(model.head.order, SubpixelOrder::RowMajor);
        let conv0 = &model.layers[0];
        assert_eq!(conv0.inputs[0].kernel, 3);
        // Output 0 at tap (-1, -1) = R + G + B weights = 0.1 + 0.5 + 0.9.
        assert!((conv0.inputs[0].weights[0] - 1.5).abs() < 1e-6);
        // Output 1 at the same tap: 0.2 + 0.6 + 1.0.
        assert!((conv0.inputs[0].weights[9] - 1.8).abs() < 1e-6);
        assert_eq!(conv0.bias, vec![0.01, 0.02, 0.03, 0.04]);
        let head = &model.layers[1];
        assert_eq!(head.inputs.len(), 2);
        assert_eq!(head.inputs[1].transform, InputTransform::NegRelu);
        assert_eq!(head.inputs[1].kernel, 1);
        // relu(x) - relu(-x) = x: the head reproduces conv0.
        let luma = test_pattern(9, 7, 1);
        let out = run(&model, &luma, 9, 7, LumaRange::Full);
        assert_eq!(out.len(), 9 * 7 * 4);
    }

    const FSRCNNX_LIKE: &str = r#"
//!HOOK LUMA
//!WHEN OUTPUT.w LUMA.w / 1.300 > OUTPUT.h LUMA.h / 1.300 > *
//!DESC feature map 1
//!BIND LUMA
//!SAVE FEATURE1
//!COMPONENTS 4
vec4 hook()
{
vec4 res = vec4(0.1,0.2,0.3,0.4);
res += vec4(1.0,0.0,0.0,0.0) * float(LUMA_texOff(vec2(-2,-2)));
res += vec4(0.0,1.0,0.0,0.0) * float(LUMA_texOff(vec2(2,2)));
return res;
}
//!HOOK LUMA
//!DESC sub-band residuals 1
//!BIND FEATURE1
//!SAVE RES1
//!COMPONENTS 4
vec4 hook()
{
vec4 res = vec4(0.0,0.0,0.0,0.0);
res += mat4(1.0,0.0,0.0,0.0,0.0,1.0,0.0,0.0,0.0,0.0,1.0,0.0,0.0,0.0,0.0,1.0) * FEATURE1_texOff(vec2(-1,0));
res += FEATURE1_texOff(0);
res = max(res, vec4(0.0)) + vec4(0.5,0.25,0.125,1.0) * min(res, vec4(0.0));
return res;
}
//!HOOK LUMA
//!DESC aggregation
//!BIND RES1
vec4 hook()
{
vec2 fcoord = fract(RES1_pos * RES1_size);
vec2 base = RES1_pos + (vec2(0.5) - fcoord) * RES1_pt;
ivec2 index = ivec2(fcoord * vec2(2));
vec4 res = RES1_tex(base);
return vec4(res[index.x * 2 + index.y], 0, 0, 1);
}
"#;

    #[test]
    fn imports_fsrcnnx_dialect() {
        let model = parse("fsrcnnx-like", FSRCNNX_LIKE).unwrap();
        assert_eq!(model.input_range, InputRange::Raw);
        assert_eq!(model.head.order, SubpixelOrder::ColumnMajor);
        assert_eq!(model.head.base, HeadBase::None);
        let feature = &model.layers[0];
        assert_eq!(feature.inputs[0].kernel, 5);
        // Output 0 at (dx, dy) = (-2, -2) -> ky = kx = 0.
        assert_eq!(feature.inputs[0].weights[0], 1.0);
        // Output 1 at (2, 2) -> ky = kx = 4.
        assert_eq!(feature.inputs[0].weights[25 + 24], 1.0);
        let residual = &model.layers[1];
        assert_eq!(residual.residual, Some(1));
        assert_eq!(
            residual.activation,
            Activation::Prelu(vec![0.5, 0.25, 0.125, 1.0])
        );
        assert_eq!(residual.inputs[0].kernel, 3);
        // Identity at tap (-1, 0): ky = 1, kx = 0.
        assert_eq!(residual.inputs[0].weights[3], 1.0);
    }

    /// Converts the Anime4K shaders in `ERIKA_ANIME4K_GLSL_DIR` (a checkout of
    /// bloc97/Anime4K's `glsl/` directory) into the bundled `.esr` assets.
    #[test]
    #[ignore = "regenerates bundled assets"]
    fn regenerate_anime4k_assets() {
        let Some(directory) = std::env::var_os("ERIKA_ANIME4K_GLSL_DIR") else {
            eprintln!("set ERIKA_ANIME4K_GLSL_DIR");
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/anime4k");
        std::fs::create_dir_all(&assets).unwrap();
        for (file, asset) in super::super::catalog::ANIME4K_SOURCES {
            let source = std::fs::read_to_string(directory.join(file)).unwrap();
            let model = parse(asset.trim_end_matches(".esr"), &source).unwrap();
            let bytes = model.to_bytes(false);
            eprintln!(
                "{asset}: {} layers, {} params, {} MAC/px, {} bytes",
                model.layers.len(),
                model.parameter_count(),
                model.macs_per_pixel(),
                bytes.len()
            );
            std::fs::write(assets.join(asset), bytes).unwrap();
        }
    }
}
