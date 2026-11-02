//! WGSL lowering of the backend-neutral SR graph. Weights are data, never
//! shader literals. Ordinary kernels use literal uniform/storage indices,
//! cooperative input staging, vector output blocks and fused epilogues.
//! Accumulation normally stays f32. Device profiles can select oracle-checked
//! f16 coefficients, partial products, or accumulators independently.

use std::fmt::Write;

use super::model::{Activation, HeadBase, InputTransform, Padding, SrModel, SubpixelOrder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub group: [u32; 2],
    pub fsr_group: [u32; 2],
    pub pixels: u32,
    pub register_blocking: bool,
    pub output_block: usize,
    pub half: bool,
    pub weights_half: bool,
    /// Round four products in f16 before adding to the f32 accumulator.
    /// Kept as a tuning candidate; direct f32 FMA avoids operand conversions.
    pub half_products: bool,
    pub narrow_half_products: bool,
    pub half_accum: bool,
    pub mad: bool,
    pub compact_taps: bool,
    pub subgroup: bool,
    pub cooperative: bool,
    pub matrix_size: usize,
    pub matrix_rows: usize,
    pub matrix_min_channels: usize,
    pub r16: bool,
    pub gather: bool,
    pub fsr_unroll: bool,
    pub fsr_half_math: bool,
    pub shared_bytes: usize,
    pub uniform_bytes: usize,
    pub fusion: bool,
    pub staging: bool,
    pub unroll: bool,
    pub fusion_depth: usize,
    pub fusion_max_channels: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            group: [8, 8],
            fsr_group: [16, 16],
            pixels: 1,
            register_blocking: false,
            output_block: 8,
            half: false,
            weights_half: false,
            half_products: false,
            narrow_half_products: false,
            half_accum: false,
            mad: false,
            compact_taps: false,
            subgroup: false,
            cooperative: false,
            matrix_size: 8,
            matrix_rows: 4,
            matrix_min_channels: 8,
            r16: false,
            gather: false,
            fsr_unroll: true,
            fsr_half_math: false,
            shared_bytes: 28 * 1024,
            uniform_bytes: 16384,
            fusion: true,
            staging: true,
            unroll: false,
            fusion_depth: 2,
            fusion_max_channels: 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Kernel {
    pub layer: usize,
    pub source: String,
    pub weights: Vec<[f32; 4]>,
    pub uniform: bool,
    pub weights_half: bool,
    pub group: [u32; 2],
    pub pixels: u32,
    pub ring: bool,
    pub head: bool,
    pub fused: bool,
    pub bindings: Vec<usize>,
    pub output_start: usize,
    pub writes: Vec<usize>,
}

fn transform(value: &str, transform: InputTransform) -> String {
    match transform {
        InputTransform::Identity => value.to_owned(),
        InputTransform::Relu => format!("max({value}, F(0))"),
        InputTransform::NegRelu => format!("max(-{value}, F(0))"),
    }
}

/// Common ABI, also used by FSR1. Offsets count vec4 feature elements.
pub fn preamble(half: bool, weights: usize, uniform: bool) -> String {
    let f = if half { "f16" } else { "f32" };
    let enable = if half { "enable f16;" } else { "" };
    let w = if uniform {
        format!(
            "struct Weights {{ data: array<vec4<f32>, {}>, }}; @group(0) @binding(2) var<uniform> weights: Weights;",
            weights.max(1)
        )
    } else {
        "struct Weights { data: array<vec4<f32>>, }; @group(0) @binding(2) var<storage, read> weights: Weights;".to_owned()
    };
    format!(
        r#"{enable}
alias F = {f};
struct Params {{
    size: vec4<u32>,
    tile: vec4<i32>,
    rect: vec4<i32>,
    range: vec4<f32>,
    luma: vec4<f32>,
    offsets: array<vec4<u32>, 64>,
}};
@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read_write> arena: array<vec4<F>>;
{w}
@group(0) @binding(3) var source: texture_2d<f32>;
@group(0) @binding(4) var dest: texture_storage_2d<rgba16float, write>;
fn raw(q: vec2<i32>) -> f32 {{
    let bounds=vec2<i32>(min(p.size.xy,textureDimensions(source,0)));
    let v = textureLoad(source, clamp(q, vec2<i32>(0), bounds-1), 0);
    return dot(v.xyz, p.luma.xyz);
}}
fn addr(t: u32, q: vec2<i32>, slices: u32, c: u32) -> u32 {{
    return p.offsets[t/4u][t%4u] + u32((q.y-p.tile.y)*p.tile.z+q.x-p.tile.x)*slices+c;
}}
fn base(q: vec2<f32>) -> f32 {{
    let a = vec2<i32>(floor(q)); let f = fract(q);
    return mix(mix(raw(a), raw(a+vec2<i32>(1,0)), f.x),
               mix(raw(a+vec2<i32>(0,1)), raw(a+vec2<i32>(1,1)), f.x), f.y);
}}
"#
    )
}

fn network_preamble(options: Options, weights: usize, uniform: bool) -> String {
    let s = preamble(options.half, weights, uniform);
    if options.weights_half {
        s.replace("data: array<vec4<f32>>", "data: array<vec4<f16>>")
    } else {
        s
    }
}

fn fetch(s: &mut String, model: &SrModel, tensor: usize) {
    let slices = model.tensors[tensor].div_ceil(4);
    let _ = writeln!(s, "fn read_{tensor}(q: vec2<i32>, c: u32) -> F {{");
    if model.padding == Padding::Zero {
        s.push_str(
            "if (any(q < vec2<i32>(0)) || any(q >= vec2<i32>(p.size.xy))) { return F(0); }\n",
        );
    }
    s.push_str("let v = clamp(q, vec2<i32>(0), vec2<i32>(p.size.xy)-1);\n");
    if tensor == 0 {
        s.push_str("return F((raw(v)-p.range.x)*p.range.y);\n}\n");
    } else {
        s.push_str("if(any(v<p.tile.xy)||any(v>=p.tile.xy+p.tile.zw)){return F(0);}\n");
        let _ = writeln!(
            s,
            "return arena[addr({tensor}u,v,{slices}u,c/4u)][c%4u];\n}}"
        );
    }
    let _ = writeln!(s, "fn readv_{tensor}(q:vec2<i32>,c:u32)->vec4<F>{{");
    if model.padding == Padding::Zero {
        s.push_str("if(any(q<vec2<i32>(0))||any(q>=vec2<i32>(p.size.xy))){return vec4<F>(0);}\n");
    }
    s.push_str("let v=clamp(q,vec2<i32>(0),vec2<i32>(p.size.xy)-1);\n");
    if tensor == 0 {
        s.push_str("return vec4<F>(F((raw(v)-p.range.x)*p.range.y),F(0),F(0),F(0));}\n");
    } else {
        s.push_str("if(any(v<p.tile.xy)||any(v>=p.tile.xy+p.tile.zw)){return vec4<F>(0);}\n");
        let _ = writeln!(s, "return arena[addr({tensor}u,v,{slices}u,c)];}}");
    }
}

fn head(s: &mut String, model: &SrModel, value: &str, q: &str) {
    let swizzle = if model.head.order == SubpixelOrder::ColumnMajor {
        ".xzyw"
    } else {
        ""
    };
    let _ = writeln!(s, "var result = ({value}){swizzle} * p.range.z;");
    if model.head.base == HeadBase::Bilinear {
        for (i, (x, y)) in [(-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25)]
            .into_iter()
            .enumerate()
        {
            let _ = writeln!(s, "result[{i}] += base(vec2<f32>({q})+vec2<f32>({x},{y}));");
        }
    } else {
        s.push_str("result += vec4<f32>(p.range.x);\n");
    }
    if model.head.clamp {
        s.push_str("result = clamp(result,vec4<f32>(0),vec4<f32>(1));\n");
    }
    let _ = writeln!(s, "textureStore(dest,{q},result);");
}

fn activation(
    s: &mut String,
    model: &SrModel,
    li: usize,
    start: usize,
    v: &str,
    q: &str,
    ring: bool,
) {
    let layer = &model.layers[li];
    if let Some(t) = layer.residual {
        for c in 0..4 {
            if start + c < layer.out_channels {
                let ch = if ring {
                    format!("oc+{c}u")
                } else {
                    format!("{}u", start + c)
                };
                let _ = writeln!(s, "{v}[{c}] += f32(read_{t}({q},{ch}));");
            }
        }
    }
    match &layer.activation {
        Activation::None => (),
        Activation::Relu => {
            let _ = writeln!(s, "{v} = max({v},vec4<f32>(0));");
        }
        Activation::Silu => {
            let _ = writeln!(s, "{v} = {v}/(vec4<f32>(1)+exp(-{v}));");
        }
        Activation::Prelu(alpha) => {
            // PReLU slopes remain f32, even with half operands.
            if ring {
                let _ = write!(s, "let slopes = array<f32,{}>(", alpha.len());
                for a in alpha {
                    let _ = write!(s, "{a:?},");
                }
                s.push_str(");\n");
                for c in 0..4 {
                    let _ = writeln!(
                        s,
                        "if (oc+{c}u < {}u) {{ {v}[{c}] = max({v}[{c}],0.0)+slopes[oc+{c}u]*min({v}[{c}],0.0); }}",
                        alpha.len()
                    );
                }
            } else {
                let a: [f32; 4] =
                    std::array::from_fn(|i| alpha.get(start + i).copied().unwrap_or(0.0));
                let _ = writeln!(
                    s,
                    "{v} = max({v},vec4<f32>(0))+vec4<f32>({:?},{:?},{:?},{:?})*min({v},vec4<f32>(0));",
                    a[0], a[1], a[2], a[3]
                );
            }
        }
    }
}

fn packed_weights(
    model: &SrModel,
    li: usize,
    start: usize,
    count: usize,
    variant: Option<usize>,
) -> Vec<[f32; 4]> {
    let l = &model.layers[li];
    let variant = variant.map(|i| &l.border.as_ref().unwrap()[i]);
    let mut out = Vec::new();
    let bias = variant.map_or(&l.bias, |v| &v.bias);
    for o in (start..start + count).step_by(4) {
        out.push(std::array::from_fn(|i| {
            bias.get(o + i).copied().unwrap_or(0.0)
        }));
    }
    for (ii, input) in l.inputs.iter().enumerate() {
        let weights = variant.map_or(&input.weights, |v| &v.inputs[ii]);
        let k = model.tensors[input.tensor] * input.kernel * input.kernel;
        for tap in 0..k {
            for o in (start..start + count).step_by(4) {
                out.push(std::array::from_fn(|i| {
                    if o + i < l.out_channels {
                        weights[(o + i) * k + tap]
                    } else {
                        0.0
                    }
                }));
            }
        }
    }
    out
}

/// Generates layer kernels. One perimeter kernel handles all border classes
/// and output channels, including degenerate 1xN/Nx1 images.
pub fn network(model: &SrModel, options: Options) -> Vec<Kernel> {
    let mut options = options;
    options.half_products |= options.narrow_half_products && model.tensors.iter().all(|&c| c <= 8);
    debug_assert!(model.validate().is_ok());
    debug_assert!(model.tensors.len() <= 256 && options.pixels > 0);
    debug_assert!(options.group.iter().all(|&v| v > 0));
    if options.fusion
        && let Some(kernels) = fused_chain(model, options)
    {
        return kernels;
    }
    let mut kernels = Vec::new();
    for (li, l) in model.layers.iter().enumerate() {
        let is_head = l.output == model.head.tensor
            && !model.layers.iter().any(|x| {
                x.inputs.iter().any(|i| i.tensor == l.output) || x.residual == Some(l.output)
            });
        let mut tensors: Vec<usize> = l
            .inputs
            .iter()
            .map(|i| i.tensor)
            .chain(l.residual)
            .collect();
        tensors.sort_unstable();
        tensors.dedup();
        let mut options = options;
        options.subgroup &= tensors.contains(&0);
        let mut group = options.group;
        let bytes = |g: [u32; 2]| -> usize {
            tensors
                .iter()
                .map(|&t| {
                    let r = l
                        .inputs
                        .iter()
                        .filter(|i| i.tensor == t)
                        .map(|i| i.kernel / 2)
                        .max()
                        .unwrap_or(0);
                    if r == 0 {
                        0
                    } else {
                        (g[0] as usize * options.pixels as usize + 2 * r)
                            * (g[1] as usize + 2 * r)
                            * model.tensors[t].div_ceil(4)
                            * 4
                            * if options.half { 2 } else { 4 }
                    }
                })
                .sum()
        };
        while options.staging && bytes(group) > options.shared_bytes && group[1] > 1 {
            group[1] /= 2;
        }
        while options.staging && bytes(group) > options.shared_bytes && group[0] > 1 {
            group[0] /= 2;
        }
        let staging = options.staging && bytes(group) <= options.shared_bytes;
        let mt = options.matrix_size;
        let mr = options.matrix_rows.clamp(1, if mt == 8 { 2 } else { 4 });
        let matrix = options.cooperative
            && l.inputs
                .iter()
                .all(|i| model.tensors[i.tensor] >= options.matrix_min_channels.max(8))
            && l.inputs.iter().all(|i| {
                model.tensors[i.tensor].div_ceil(mt)
                    * mt
                    * (mt + i.kernel - 1)
                    * (mr + i.kernel - 1)
                    * (if mt == 16 { 2 } else { 4 })
                    + 128 * mt * mr
                    <= options.shared_bytes
            });
        let block = if matrix {
            32
        } else {
            options.output_block.clamp(4, 16).div_ceil(4) * 4
        };
        for start in (0..l.out_channels).step_by(block) {
            let count = (l.out_channels - start).min(block).div_ceil(4) * 4;
            if matrix && l.out_channels >= 8 && count >= 8 {
                kernels.push(matrix_layer(model, li, start, count, options));
                continue;
            }
            let weights = packed_weights(model, li, start, count, None);
            let uniform = !options.weights_half && weights.len() * 16 <= options.uniform_bytes;
            if options.register_blocking && options.pixels > 1 && !options.compact_taps {
                kernels.push(register_layer(
                    model, li, start, count, group, staging, is_head, options,
                ));
                continue;
            }
            let mut s = network_preamble(options, weights.len(), uniform);
            for &t in &tensors {
                fetch(&mut s, model, t);
            }
            let mut staged = Vec::new();
            for &t in &tensors {
                let r = l
                    .inputs
                    .iter()
                    .filter(|i| i.tensor == t)
                    .map(|i| i.kernel / 2)
                    .max()
                    .unwrap_or(0);
                if r > 0 && staging && !(t == 0 && options.subgroup) {
                    let w = group[0] as usize * options.pixels as usize + 2 * r;
                    let h = group[1] as usize + 2 * r;
                    let slices = model.tensors[t].div_ceil(4);
                    let _ = writeln!(
                        s,
                        "var<workgroup> shared_{t}: array<vec4<F>,{}>;",
                        w * h * slices
                    );
                    staged.push((t, r, w, h, slices));
                }
            }
            let _ = writeln!(
                s,
                "@compute @workgroup_size({},{},1) fn main(@builtin(local_invocation_id) tid: vec3<u32>, @builtin(workgroup_id) gid: vec3<u32>, @builtin(local_invocation_index) lin: u32 {}) {{",
                if options.subgroup {
                    group[0] * group[1]
                } else {
                    group[0]
                },
                if options.subgroup { 1 } else { group[1] },
                if options.subgroup {
                    ",@builtin(subgroup_invocation_id) lane:u32,@builtin(subgroup_size) lanes:u32"
                } else {
                    ""
                }
            );
            if options.subgroup {
                s = s.replace("tid: vec3<u32>", "physical_tid: vec3<u32>");
                let _ = writeln!(
                    s,
                    "let tid=vec3<u32>(lin%{}u,lin/{}u,0u);",
                    group[0], group[0]
                );
            }
            let _ = writeln!(
                s,
                "let origin = p.rect.xy + vec2<i32>(gid.xy)*vec2<i32>({},{});",
                group[0] * options.pixels,
                group[1]
            );
            for &(t, r, w, h, slices) in &staged {
                let _ = writeln!(
                    s,
                    "for (var j=lin; j<{}u; j+={}u) {{ let c=j%{slices}u; let pixel=j/{slices}u; let pos=origin+vec2<i32>(i32(pixel)%{w},i32(pixel)/{w})-vec2<i32>({r}); shared_{t}[c*{}u+pixel]=readv_{t}(pos,c); }}",
                    w * h * slices,
                    group[0] * group[1],
                    w * h
                );
            }
            if !staged.is_empty() {
                s.push_str("workgroupBarrier();\n");
            }
            let _ = writeln!(
                s,
                "for (var px=0u;px<{}u;px++) {{ let local=vec2<i32>(i32(tid.x*{}u+px),i32(tid.y)); let q=origin+local; {}",
                options.pixels,
                options.pixels,
                if options.subgroup {
                    "let sg=f32(read_0(q,0u));"
                } else {
                    "if(any(q>=p.rect.xy+p.rect.zw)){continue;}"
                }
            );
            for o in 0..count / 4 {
                if options.half_accum {
                    let _ = writeln!(s, "var a{o}=vec4<F>(weights.data[{o}]);");
                } else {
                    let _ = writeln!(s, "var a{o}=vec4<f32>(weights.data[{o}]);");
                }
            }
            let mut wi = count / 4;
            for input in &l.inputs {
                let t = input.tensor;
                let k = input.kernel;
                let r = k / 2;
                let channels = model.tensors[t];
                let literal = options.unroll || channels <= 4;
                for cb in 0..if literal { channels.div_ceil(4) } else { 1 } {
                    let c = cb * 4;
                    let slice = if literal {
                        format!("{cb}u")
                    } else {
                        "cb".into()
                    };
                    if !literal {
                        let _ = writeln!(s, "for(var cb=0u;cb<{}u;cb++){{", channels.div_ceil(4));
                    }
                    if options.compact_taps {
                        let _ = writeln!(s, "for(var tap=0u;tap<{}u;tap++){{", k * k);
                    }
                    for ky in 0..if options.compact_taps { 1 } else { k } {
                        for kx in 0..if options.compact_taps { 1 } else { k } {
                            let dx = if options.compact_taps {
                                format!("i32(tap%{k}u)")
                            } else {
                                kx.to_string()
                            };
                            let dy = if options.compact_taps {
                                format!("i32(tap/{k}u)")
                            } else {
                                ky.to_string()
                            };
                            let value = if t == 0 && options.subgroup {
                                let delta = format!("(({dy})-{r})*{}+({dx})-{r}", group[0]);
                                let _ = writeln!(
                                    s,
                                    "{{let desired=q+vec2<i32>({dx}-{r},{dy}-{r});let shuffled=subgroupShuffle(vec3<f32>(sg,vec2<f32>(q)),u32(clamp(i32(lane)+{delta},0,i32(lanes)-1)));var shared_v=F(shuffled.x);if(any(vec2<i32>(shuffled.yz)!=desired)){{shared_v=read_0(desired,0u);}}"
                                );
                                "vec4<F>(shared_v,0,0,0)".into()
                            } else if let Some(&(_, sr, w, h, _)) =
                                staged.iter().find(|&&(a, _, _, _, _)| a == t)
                            {
                                format!(
                                    "shared_{t}[u32((local.y+{dy}+{})*{w}+local.x+{dx}+{})+{slice}*{}u]",
                                    sr - r,
                                    sr - r,
                                    w * h
                                )
                            } else {
                                format!("readv_{t}(q+vec2<i32>({dx}-{r},{dy}-{r}),{slice})")
                            };
                            let _ = writeln!(
                                s,
                                "{{ let v={};",
                                transform(&value, input.transform).replace("F(0)", "vec4<F>(0)")
                            );
                            for o in 0..count / 4 {
                                if options.half_products {
                                    let _ = writeln!(s, "var partial{o}=vec4<F>(0);");
                                }
                                for component in 0..(channels - c).min(4) {
                                    let index = if options.compact_taps {
                                        let ch = if literal {
                                            format!("{}u", c + component)
                                        } else {
                                            format!("cb*4u+{component}u")
                                        };
                                        format!("{wi}u+(({ch})*{}u+tap)*{}u+{o}u", k * k, count / 4)
                                    } else if literal {
                                        (wi + ((c + component) * k * k + ky * k + kx) * (count / 4)
                                            + o)
                                            .to_string()
                                    } else {
                                        format!(
                                            "{wi}u+((cb*4u+{component}u)*{}u+{}u)*{}u+{o}u",
                                            k * k,
                                            ky * k + kx,
                                            count / 4
                                        )
                                    };
                                    if !literal && channels % 4 != 0 {
                                        let _ = writeln!(s, "if(cb*4u+{component}u<{channels}u){{");
                                    }
                                    if options.half_accum {
                                        let _ = writeln!(
                                            s,
                                            "a{o}=fma(vec4<F>(weights.data[{index}]),vec4<F>(v[{component}]),a{o});"
                                        );
                                    } else if options.half_products {
                                        let _ = writeln!(
                                            s,
                                            "partial{o}=fma(vec4<F>(weights.data[{index}]),vec4<F>(v[{component}]),partial{o});"
                                        );
                                    } else if options.mad {
                                        let _ = writeln!(
                                            s,
                                            "a{o}+=vec4<f32>(weights.data[{index}])*f32(v[{component}]);"
                                        );
                                    } else {
                                        let _ = writeln!(
                                            s,
                                            "a{o}=fma(vec4<f32>(weights.data[{index}]),vec4<f32>(f32(v[{component}])),a{o});"
                                        );
                                    }
                                    if !literal && channels % 4 != 0 {
                                        s.push_str("}\n");
                                    }
                                }
                                if options.half_products && !options.half_accum {
                                    let _ = writeln!(s, "a{o} += vec4<f32>(partial{o});");
                                }
                            }
                            s.push_str("}\n");
                            if t == 0 && options.subgroup {
                                s.push_str("}\n");
                            }
                        }
                    }
                    if options.compact_taps {
                        s.push_str("}\n");
                    }
                    if !literal {
                        s.push_str("}\n");
                    }
                }
                wi += channels * k * k * (count / 4);
            }
            debug_assert_eq!(wi, weights.len());
            debug_assert!(staged.iter().all(|&(_, _, w, h, slices)| w
                * h
                * slices
                * 4
                * if options.half { 2 } else { 4 }
                <= options.shared_bytes));
            if options.subgroup {
                s.push_str("if(all(q<p.rect.xy+p.rect.zw)){\n");
            }
            for o in 0..count / 4 {
                let v = if options.half_accum {
                    let _ = writeln!(s, "var fp{o}=vec4<f32>(a{o});");
                    format!("fp{o}")
                } else {
                    format!("a{o}")
                };
                activation(&mut s, model, li, start + 4 * o, &v, "q", false);
                if is_head {
                    head(&mut s, model, &v, "q");
                } else {
                    let _ = writeln!(
                        s,
                        "arena[addr({}u,q,{}u,u32(p.luma.w)/4u+{o}u)] = vec4<F>({v});",
                        l.output,
                        l.out_channels.div_ceil(4)
                    );
                }
            }
            if options.subgroup {
                s.push_str("}\n");
            }
            s.push_str("}\n}\n");
            kernels.push(Kernel {
                layer: li,
                source: s,
                weights,
                uniform,
                weights_half: options.weights_half,
                group,
                pixels: options.pixels,
                ring: false,
                head: is_head,
                fused: false,
                bindings: vec![],
                output_start: start,
                writes: if is_head { vec![] } else { vec![l.output] },
            });
        }
        if l.border.is_some() {
            let slices = l.out_channels.div_ceil(4);
            let mut weights = Vec::new();
            for v in 0..9 {
                weights.extend(packed_weights(model, li, 0, slices * 4, Some(v)));
            }
            let stride = weights.len() / 9;
            let mut s = network_preamble(options, weights.len(), false);
            for &t in &tensors {
                fetch(&mut s, model, t);
            }
            s.push_str("@compute @workgroup_size(64,1,1) fn main(@builtin(global_invocation_id) id: vec3<u32>) {\nlet w=i32(p.size.x); let h=i32(p.size.y); let n=i32(id.x); var q: vec2<i32>;\nif (w==1 || h==1) { if(n>=w*h){return;} q=vec2<i32>(n%w,n/w); } else { if(n>=2*w+2*(h-2)){return;} if(n<w){q=vec2<i32>(n,0);}else if(n<2*w){q=vec2<i32>(n-w,h-1);}else{let j=n-2*w;q=vec2<i32>(select(0,w-1,j%2==1),j/2+1);} }\nif(any(q<p.rect.xy)||any(q>=p.rect.xy+p.rect.zw)){return;}\nlet col=select(select(1u,2u,q.x==w-1),0u,q.x==0); let row=select(select(1u,2u,q.y==h-1),0u,q.y==0);\n");
            let _ = writeln!(
                s,
                "let wb=(row*3u+col)*{stride}u; let oc=id.z*4u; var a=vec4<f32>(weights.data[wb+id.z]);"
            );
            let mut wi = slices;
            for input in &l.inputs {
                let t = input.tensor;
                let k = input.kernel;
                let r = k as i32 / 2;
                // Compact loop keeps very wide custom kernels within the
                // instruction cache. Output-vector index is the z dimension.
                let _ = writeln!(
                    s,
                    "for(var c=0u;c<{}u;c++) {{for(var ky=0u;ky<{k}u;ky++) {{for(var kx=0u;kx<{k}u;kx++) {{ let v={}; a+=vec4<f32>(vec4<F>(weights.data[wb+{wi}u+((c*{k}u+ky)*{k}u+kx)*{slices}u+id.z])*v); }}}}}}",
                    model.tensors[t],
                    transform(
                        &format!("read_{t}(q+vec2<i32>(i32(kx)-{r},i32(ky)-{r}),c)"),
                        input.transform
                    )
                );
                wi += model.tensors[t] * k * k * slices;
            }
            debug_assert_eq!(wi, stride);
            debug_assert_eq!(weights.len(), stride * 9);
            activation(&mut s, model, li, 0, "a", "q", true);
            if is_head {
                head(&mut s, model, "a", "q");
            } else {
                let _ = writeln!(s, "arena[addr({}u,q,{slices}u,id.z)]=vec4<F>(a);", l.output);
            }
            s.push_str("}\n");
            kernels.push(Kernel {
                layer: li,
                source: s,
                weights,
                uniform: false,
                weights_half: options.weights_half,
                group: [64, 1],
                pixels: 1,
                ring: true,
                head: is_head,
                fused: false,
                bindings: vec![],
                output_start: 0,
                writes: if is_head { vec![] } else { vec![l.output] },
            });
        }
    }
    // An unusual custom graph may consume its head tensor in later layers.
    if !kernels.iter().any(|k| k.head) {
        let mut s = preamble(options.half, 1, true);
        fetch(&mut s, model, model.head.tensor);
        s.push_str("@compute @workgroup_size(8,8,1) fn main(@builtin(global_invocation_id) id: vec3<u32>){let q=p.rect.xy+vec2<i32>(id.xy); if(any(q>=p.rect.xy+p.rect.zw)){return;}\n");
        let t = model.head.tensor;
        let _ = writeln!(
            s,
            "let a=vec4<f32>(vec4<F>(read_{t}(q,0u),read_{t}(q,1u),read_{t}(q,2u),read_{t}(q,3u)));"
        );
        head(&mut s, model, "a", "q");
        s.push_str("}\n");
        kernels.push(Kernel {
            layer: model.layers.len(),
            source: s,
            weights: vec![[0.0; 4]],
            uniform: true,
            weights_half: false,
            group: [8, 8],
            pixels: 1,
            ring: false,
            head: true,
            fused: false,
            bindings: vec![],
            output_start: 0,
            writes: vec![],
        });
    }
    for k in &mut kernels {
        canonicalize(model, k);
    }
    kernels
}

/// Independent pixels keep several convolution chains live and reuse each
/// coefficient load across them. This is distinct from a sequential pixel loop.
#[allow(clippy::too_many_arguments)]
fn register_layer(
    model: &SrModel,
    li: usize,
    start: usize,
    count: usize,
    group: [u32; 2],
    staging: bool,
    is_head: bool,
    options: Options,
) -> Kernel {
    let l = &model.layers[li];
    let pixels = options.pixels;
    let weights = packed_weights(model, li, start, count, None);
    let uniform = !options.weights_half && weights.len() * 16 <= options.uniform_bytes;
    let mut s = network_preamble(options, weights.len(), uniform);
    let mut tensors: Vec<_> = l
        .inputs
        .iter()
        .map(|i| i.tensor)
        .chain(l.residual)
        .collect();
    tensors.sort_unstable();
    tensors.dedup();
    let mut staged = Vec::new();
    for t in tensors {
        fetch(&mut s, model, t);
        let r = l
            .inputs
            .iter()
            .filter(|i| i.tensor == t)
            .map(|i| i.kernel / 2)
            .max()
            .unwrap_or(0);
        if staging && r > 0 {
            let w = group[0] as usize * pixels as usize + 2 * r;
            let h = group[1] as usize + 2 * r;
            let slices = model.tensors[t].div_ceil(4);
            let _ = writeln!(
                s,
                "var<workgroup> shared_{t}:array<vec4<F>,{}>;",
                w * h * slices
            );
            staged.push((t, r, w, h, slices));
        }
    }
    let _ = writeln!(
        s,
        "@compute @workgroup_size({},{},1) fn main(@builtin(local_invocation_id) tid:vec3<u32>,@builtin(workgroup_id) gid:vec3<u32>,@builtin(local_invocation_index) lin:u32){{let origin=p.rect.xy+vec2<i32>(gid.xy)*vec2<i32>({},{});",
        group[0],
        group[1],
        group[0] * pixels,
        group[1]
    );
    for &(t, r, w, h, slices) in &staged {
        let _ = writeln!(
            s,
            "for(var j=lin;j<{}u;j+={}u){{let c=j%{slices}u;let pix=j/{slices}u;shared_{t}[c*{}u+pix]=readv_{t}(origin+vec2<i32>(i32(pix%{w}u)-{r},i32(pix/{w}u)-{r}),c);}}",
            w * h * slices,
            group[0] * group[1],
            w * h
        );
    }
    if !staged.is_empty() {
        s.push_str("workgroupBarrier();\n");
    }
    for p in 0..pixels {
        let _ = writeln!(
            s,
            "let local{p}=vec2<i32>(i32(tid.x*{pixels}u+{p}u),i32(tid.y));let q{p}=origin+local{p};"
        );
        for o in 0..count / 4 {
            let ty = if options.half_accum { "F" } else { "f32" };
            let _ = writeln!(s, "var a{o}_{p}=vec4<{ty}>(weights.data[{o}]);");
        }
    }
    let mut wi = count / 4;
    for input in &l.inputs {
        let t = input.tensor;
        let cin = model.tensors[t];
        let k = input.kernel;
        let r = k / 2;
        let literal = options.unroll || cin <= 4;
        for cb in 0..if literal { cin.div_ceil(4) } else { 1 } {
            let slice = if literal {
                format!("{cb}u")
            } else {
                "cb".into()
            };
            if !literal {
                let _ = writeln!(s, "for(var cb=0u;cb<{}u;cb++){{", cin.div_ceil(4));
            }
            for ky in 0..k {
                for kx in 0..k {
                    s.push_str("{\n");
                    for p in 0..pixels {
                        let value = if let Some(&(_, sr, w, h, _)) =
                            staged.iter().find(|&&(a, _, _, _, _)| a == t)
                        {
                            format!(
                                "shared_{t}[{slice}*{}u+u32((local{p}.y+{})*{w}+local{p}.x+{})]",
                                w * h,
                                sr + ky - r,
                                sr + kx - r
                            )
                        } else {
                            format!(
                                "readv_{t}(q{p}+vec2<i32>({},{}),{slice})",
                                kx as i32 - r as i32,
                                ky as i32 - r as i32
                            )
                        };
                        let _ = writeln!(
                            s,
                            "let v{p}={};",
                            transform(&value, input.transform).replace("F(0)", "vec4<F>(0)")
                        );
                        if options.half_products && !options.half_accum {
                            for o in 0..count / 4 {
                                let _ = writeln!(s, "var part{o}_{p}=vec4<F>(0);");
                            }
                        }
                    }
                    for c in 0..(cin - cb * 4).min(4) {
                        if !literal && cin % 4 != 0 {
                            let _ = writeln!(s, "if(cb*4u+{c}u<{cin}u){{");
                        }
                        for o in 0..count / 4 {
                            let index = if literal {
                                (wi + ((cb * 4 + c) * k * k + ky * k + kx) * (count / 4) + o)
                                    .to_string()
                            } else {
                                format!(
                                    "{wi}u+((cb*4u+{c}u)*{}u+{}u)*{}u+{o}u",
                                    k * k,
                                    ky * k + kx,
                                    count / 4
                                )
                            };
                            let _ = writeln!(s, "{{let w=weights.data[{index}];");
                            for p in 0..pixels {
                                if options.half_accum {
                                    let _ = writeln!(
                                        s,
                                        "a{o}_{p}=fma(vec4<F>(w),vec4<F>(v{p}[{c}]),a{o}_{p});"
                                    );
                                } else if options.half_products {
                                    let _ = writeln!(
                                        s,
                                        "part{o}_{p}=fma(vec4<F>(w),vec4<F>(v{p}[{c}]),part{o}_{p});"
                                    );
                                } else if options.mad {
                                    let _ = writeln!(s, "a{o}_{p}+=vec4<f32>(w)*f32(v{p}[{c}]);");
                                } else {
                                    let _ = writeln!(
                                        s,
                                        "a{o}_{p}=fma(vec4<f32>(w),vec4<f32>(f32(v{p}[{c}])),a{o}_{p});"
                                    );
                                }
                            }
                            s.push_str("}\n");
                        }
                        if !literal && cin % 4 != 0 {
                            s.push_str("}\n");
                        }
                    }
                    if options.half_products && !options.half_accum {
                        for p in 0..pixels {
                            for o in 0..count / 4 {
                                let _ = writeln!(s, "a{o}_{p}+=vec4<f32>(part{o}_{p});");
                            }
                        }
                    }
                    s.push_str("}\n");
                }
            }
            if !literal {
                s.push_str("}\n");
            }
        }
        wi += cin * k * k * (count / 4);
    }
    debug_assert_eq!(wi, weights.len());
    for p in 0..pixels {
        let _ = writeln!(s, "if(all(q{p}<p.rect.xy+p.rect.zw)){{");
        for o in 0..count / 4 {
            let _ = writeln!(s, "var v{o}=vec4<f32>(a{o}_{p});");
            activation(
                &mut s,
                model,
                li,
                start + 4 * o,
                &format!("v{o}"),
                &format!("q{p}"),
                false,
            );
            if is_head {
                head(&mut s, model, &format!("v{o}"), &format!("q{p}"));
            } else {
                let _ = writeln!(
                    s,
                    "arena[addr({}u,q{p},{}u,u32(p.luma.w)/4u+{o}u)]=vec4<F>(v{o});",
                    l.output,
                    l.out_channels.div_ceil(4)
                );
            }
        }
        s.push_str("}\n");
    }
    s.push_str("}\n");
    Kernel {
        layer: li,
        source: s,
        weights,
        uniform,
        weights_half: options.weights_half,
        group,
        pixels,
        ring: false,
        head: is_head,
        fused: false,
        bindings: vec![],
        output_start: start,
        writes: if is_head { vec![] } else { vec![l.output] },
    }
}

/// Each subgroup computes eight pixels by eight output channels; up to four
/// subgroups share the staged input. Operands are
/// explicitly initialized before every matrix load; edge lanes participate in
/// every barrier and only the final stores are masked.
fn matrix_layer(
    model: &SrModel,
    li: usize,
    start: usize,
    count: usize,
    options: Options,
) -> Kernel {
    let l = &model.layers[li];
    let tile = options.matrix_size;
    let area = tile * tile;
    let at = if tile == 16 { "f16" } else { "f32" };
    let outs = count.div_ceil(tile) * tile;
    let simds = outs / tile;
    let rows = options.matrix_rows.clamp(1, if tile == 8 { 2 } else { 4 });
    let threads = simds * rows * 32;
    let mut weights = packed_weights(model, li, start, outs, None)[..outs / 4].to_vec();
    for i in &l.inputs {
        let cin = model.tensors[i.tensor];
        let cp = cin.div_ceil(tile) * tile;
        let k = i.kernel;
        for tap in 0..k * k {
            for c in 0..cp {
                for o in (start..start + outs).step_by(4) {
                    weights.push(std::array::from_fn(|j| {
                        if c < cin && o + j < l.out_channels {
                            i.weights[((o + j) * cin + c) * k * k + tap]
                        } else {
                            0.0
                        }
                    }));
                }
            }
        }
    }
    let maxk = l
        .inputs
        .iter()
        .map(|i| {
            model.tensors[i.tensor].div_ceil(tile)
                * tile
                * (tile + i.kernel - 1)
                * (rows + i.kernel - 1)
        })
        .max()
        .unwrap();
    let uniform = false;
    let mut s = format!(
        "enable wgpu_cooperative_matrix;\n{}",
        preamble(options.half, weights.len(), uniform)
    );
    s = s.replace("data: array<vec4<f32>>", &format!("data: array<{at}>"));
    let mut tensors: Vec<_> = l
        .inputs
        .iter()
        .map(|i| i.tensor)
        .chain(l.residual)
        .collect();
    tensors.sort_unstable();
    tensors.dedup();
    for t in tensors {
        fetch(&mut s, model, t);
    }
    let _ = writeln!(
        s,
        "var<workgroup> ma:array<{at},{maxk}>;var<workgroup> mc:array<f32,{}>;",
        rows * simds * area
    );
    let _ = writeln!(
        s,
        "@compute @workgroup_size({threads},1,1) fn main(@builtin(workgroup_id) gid:vec3<u32>,@builtin(local_invocation_index) lin:u32,@builtin(subgroup_id) sid:u32){{let origin=p.rect.xy+vec2<i32>(i32(gid.x)*{tile},i32(gid.y)*{rows});for(var j=lin;j<{}u;j+={threads}u){{mc[j]=0.0;}}workgroupBarrier();var acc=coopLoadT<coop_mat{tile}x{tile}<f32,C>>(&mc[sid*{area}u],{tile}u);",
        rows * simds * area
    );
    let mut base = outs / 4;
    for input in &l.inputs {
        let t = input.tensor;
        let k = input.kernel;
        let r = k / 2;
        let cin = model.tensors[t];
        let cp = cin.div_ceil(tile) * tile;
        let w = tile + k - 1;
        let value = transform(
            &format!("readv_{t}(origin+vec2<i32>(i32(pix%{w}u)-{r},i32(pix/{w}u)-{r}),c/4u)"),
            input.transform,
        )
        .replace("F(0)", "vec4<F>(0)");
        let _ = writeln!(
            s,
            "for(var j=lin;j<{}u;j+={threads}u){{let pix=j/{}u;let c=(j%{}u)*4u;var v=vec4<{at}>(0);if(c<{cin}u){{v=vec4<{at}>({value});}}for(var lane=0u;lane<4u;lane++){{ma[j*4u+lane]=v[lane];}}}}workgroupBarrier();for(var tap=0u;tap<{}u;tap++){{for(var cb=0u;cb<{cp}u;cb+={tile}u){{let a=coopLoadT<coop_mat{tile}x{tile}<{at},A>>(&ma[(((sid/{simds}u)+tap/{k}u)*{w}u+tap%{k}u)*{cp}u+cb],{cp}u);let b=coopLoadT<coop_mat{tile}x{tile}<{at},B>>(&weights.data[{}u+(tap*{cp}u+cb)*{outs}u+(sid%{simds}u)*{tile}u],{outs}u);acc=coopMultiplyAdd(a,b,acc);}}}}workgroupBarrier();",
            w * (rows + k - 1) * cp / 4,
            cp / 4,
            cp / 4,
            k * k,
            base * 4
        );
        base += cp * k * k * outs / 4;
    }
    debug_assert_eq!(base, weights.len());
    let _ = writeln!(
        s,
        "coopStoreT(acc,&mc[sid*{area}u],{tile}u);workgroupBarrier();if(lin<{}u){{let row=lin/{tile}u;let col=lin%{tile}u;let q=origin+vec2<i32>(i32(col),i32(row));if(all(q<p.rect.xy+p.rect.zw)){{",
        rows * tile
    );
    for o in 0..count / 4 {
        let mcbase = (o / (tile / 4)) * area + (o % (tile / 4)) * 4;
        let _ = writeln!(
            s,
            "var a{o}=vec4<f32>(mc[row*{simds}u*{area}u+col*{tile}u+{}u],mc[row*{simds}u*{area}u+col*{tile}u+{}u],mc[row*{simds}u*{area}u+col*{tile}u+{}u],mc[row*{simds}u*{area}u+col*{tile}u+{}u])+vec4<f32>(f32(weights.data[{}]),f32(weights.data[{}]),f32(weights.data[{}]),f32(weights.data[{}]));",
            mcbase,
            mcbase + 1,
            mcbase + 2,
            mcbase + 3,
            o * 4,
            o * 4 + 1,
            o * 4 + 2,
            o * 4 + 3
        );
        activation(
            &mut s,
            model,
            li,
            start + o * 4,
            &format!("a{o}"),
            "q",
            false,
        );
        let _ = writeln!(
            s,
            "arena[addr({}u,q,{}u,{}u)]=vec4<F>(a{o});",
            l.output,
            l.out_channels.div_ceil(4),
            start / 4 + o
        );
    }
    s.push_str("}}}\n");
    // Naga 29's Metal lowering passes the WGSL row-major flag directly to
    // MSL's transpose argument. This experimental path is Metal-only, where
    // the non-T load/store maps to the required row-major native operation.
    if tile == 8 {
        s = s
            .replace("coopLoadT", "coopLoad")
            .replace("coopStoreT", "coopStore");
    }
    Kernel {
        layer: li,
        source: s,
        weights,
        uniform,
        weights_half: tile == 16,
        group: [tile as u32, rows as u32],
        pixels: 1,
        ring: false,
        head: false,
        fused: false,
        bindings: vec![],
        output_start: start,
        writes: vec![l.output],
    }
}

// Tensor numbers and output-block positions are runtime parameters, so equal
// layer shapes share a compiled pipeline (including wide residual networks).
fn canonicalize(model: &SrModel, k: &mut Kernel) {
    let mut tensors = vec![0];
    if let Some(l) = model.layers.get(k.layer) {
        for t in l
            .inputs
            .iter()
            .map(|i| i.tensor)
            .chain(l.residual)
            .chain([l.output])
        {
            if !tensors.contains(&t) {
                tensors.push(t);
            }
        }
    } else {
        tensors.push(model.head.tensor);
    }
    for (local, &actual) in tensors.iter().enumerate() {
        for prefix in ["read_", "readv_", "shared_"] {
            for suffix in ["(", "[", ":"] {
                k.source = k.source.replace(
                    &format!("{prefix}{actual}{suffix}"),
                    &format!("{prefix}T{local}T{suffix}"),
                );
            }
        }
        k.source = k
            .source
            .replace(&format!("addr({actual}u,"), &format!("addr(T{local}Tu,"));
    }
    for local in 0..tensors.len() {
        k.source = k.source.replace(&format!("T{local}T"), &local.to_string());
    }
    k.bindings = tensors;
}

/// Fused EASU + RCAS on luma. RCAS reads a workgroup tile with a one-pixel
/// ring; the EASU intermediate never reaches device memory. Edge analysis and
/// reciprocals remain f32, the shared intermediate may be f16.
/// Based on AMD FidelityFX FSR1, Copyright (c) 2021 Advanced Micro Devices,
/// Inc., MIT license; see `assets/fsr1/LICENSE.FSR1` and [`super::fsr1`].
pub fn fsr(options: Options) -> Kernel {
    let group = options.group;
    let w = group[0] + 2;
    let h = group[1] + 2;
    let mut s = preamble(options.half, 1, true);
    if options.r16 {
        s = s.replace(
            "texture_storage_2d<rgba16float, write>",
            "texture_storage_2d<r16float, write>",
        );
    }
    if options.gather {
        s.push_str(
            r#"
@group(0) @binding(5) var gather_sampler:sampler;
fn quad(q:vec2<i32>)->vec4<f32>{
    if(any(q<=vec2<i32>(0))||any(q>=vec2<i32>(p.size.xy))){
        return vec4<f32>(raw(q+vec2<i32>(-1,0)),raw(q),raw(q+vec2<i32>(0,-1)),raw(q-vec2<i32>(1)));
    }
    let uv=vec2<f32>(q)/vec2<f32>(textureDimensions(source,0));
    var v=textureGather(0,source,gather_sampler,uv)*p.luma.x;
    if(p.luma.y!=0.0){v+=textureGather(1,source,gather_sampler,uv)*p.luma.y;}
    if(p.luma.z!=0.0){v+=textureGather(2,source,gather_sampler,uv)*p.luma.z;}
    return v;
}
"#,
        );
    }
    s.push_str(r#"
fn rcp0(v: f32) -> f32 { if(v>0.0){return 1.0/v;} return 0.0; }
fn edge(w:f32,a:f32,b:f32,c:f32,d:f32,e:f32) -> vec3<f32> {
    let dx=d-b;let dy=e-a;
    let lx=clamp(abs(dx)*rcp0(max(abs(d-c),abs(c-b))),0.0,1.0);
    let ly=clamp(abs(dy)*rcp0(max(abs(e-c),abs(c-a))),0.0,1.0);
    return vec3<f32>(dx*w,dy*w,(lx*lx+ly*ly)*w);
}
fn easu(q:vec2<i32>) -> f32 {
    let scale=p.range.xy;
    let pp=(vec2<f32>(q)+vec2<f32>(0.5))*scale-vec2<f32>(0.5);
    let origin=vec2<i32>(floor(pp));let fpp=fract(pp);
    let b=raw(origin+vec2<i32>(0,-1));let c=raw(origin+vec2<i32>(1,-1));
    let e=raw(origin+vec2<i32>(-1,0));let f=raw(origin);let g=raw(origin+vec2<i32>(1,0));let h=raw(origin+vec2<i32>(2,0));
    let i=raw(origin+vec2<i32>(-1,1));let j=raw(origin+vec2<i32>(0,1));let k=raw(origin+vec2<i32>(1,1));let l=raw(origin+vec2<i32>(2,1));
    let n=raw(origin+vec2<i32>(0,2));let o=raw(origin+vec2<i32>(1,2));
    let ed=edge((1.0-fpp.x)*(1.0-fpp.y),b,e,f,g,j)+edge(fpp.x*(1.0-fpp.y),c,f,g,h,k)+edge((1.0-fpp.x)*fpp.y,f,i,j,k,n)+edge(fpp.x*fpp.y,g,j,k,l,o);
    var dir=ed.xy;let dr=dot(dir,dir);
    if(dr<1.0/32768.0){dir=vec2<f32>(1.0,dir.y);}else{dir*=inverseSqrt(dr);}
    let len=ed.z*ed.z*0.25;
    let stretch=dot(dir,dir)/max(abs(dir.x),abs(dir.y));
    let len2=vec2<f32>(1.0+(stretch-1.0)*len,1.0-0.5*len);
    let lob=0.5+((0.25-0.04)-0.5)*len;let clp=1.0/lob;
    let offsets=array<vec2<f32>,12>(vec2<f32>(0,-1),vec2<f32>(1,-1),vec2<f32>(-1,1),vec2<f32>(0,1),vec2<f32>(0,0),vec2<f32>(-1,0),vec2<f32>(1,1),vec2<f32>(2,1),vec2<f32>(2,0),vec2<f32>(1,0),vec2<f32>(1,2),vec2<f32>(0,2));
    let values=array<f32,12>(b,c,i,j,f,e,k,l,h,g,o,n);
    var acc=0.0;var aw=0.0;
    for(var t=0u;t<12u;t++){
        let off=offsets[t]-fpp;
        let v=vec2<f32>(off.x*dir.x+off.y*dir.y,off.x*(-dir.y)+off.y*dir.x)*len2;
        let d2=min(dot(v,v),clp);let wb=0.4*d2-1.0;let wa=lob*d2-1.0;
        let weight=(25.0/16.0*wb*wb-(25.0/16.0-1.0))*wa*wa;
        acc+=values[t]*weight;aw+=weight;
    }
    return clamp(acc/aw,min(min(f,g),min(j,k)),max(max(f,g),max(j,k)));
}
"#);
    if options.fsr_half_math {
        s = s
            .replace("var acc=0.0;var aw=0.0;", "var acc=F(0);var aw=F(0);")
            .replace(
                "let d2=min(dot(v,v),clp);let wb=0.4*d2-1.0;let wa=lob*d2-1.0;",
                "let d2=F(min(dot(v,v),clp));let wb=F(0.4)*d2-F(1);let wa=F(lob)*d2-F(1);",
            )
            .replace(
                "(25.0/16.0*wb*wb-(25.0/16.0-1.0))*wa*wa",
                "(F(1.5625)*wb*wb-F(0.5625))*wa*wa",
            )
            .replace("acc+=values[t]*weight;", "acc+=F(values[t])*weight;")
            .replace("return clamp(acc/aw,", "return clamp(f32(acc/aw),");
    }
    if options.fsr_unroll {
        let begin = s.find("    for(var t=0u;t<12u;t++){\n").unwrap();
        let end = s[begin..].find("    return clamp(").unwrap() + begin;
        let body = s[begin..end].split_once('\n').unwrap().1;
        let body = body.rsplit_once("    }\n").unwrap().0;
        let mut expanded = String::new();
        for t in 0..12 {
            let _ = writeln!(
                expanded,
                "{{{}\n}}",
                body.replace("offsets[t]", &format!("offsets[{t}] "))
                    .replace("values[t]", &format!("values[{t}]"))
            );
        }
        s.replace_range(begin..end, &expanded);
    }
    if options.gather {
        let begin = s.find("    let b=raw(origin+").unwrap();
        let end = s[begin..].find("    let ed=edge").unwrap() + begin;
        s.replace_range(begin..end,r#"    let q0=quad(origin+vec2<i32>(1,0));let q1=quad(origin+vec2<i32>(0,1));let q2=quad(origin+vec2<i32>(2,1));let q3=quad(origin+vec2<i32>(1,2));
    let b=q0.w;let c=q0.z;let e=q1.w;let f=q0.x;let g=q0.y;let h=q2.z;
    let i=q1.x;let j=q1.y;let k=q2.x;let l=q2.y;let n=q3.x;let o=q3.y;
"#);
    }
    let _ = writeln!(
        s,
        "var<workgroup> lds:array<F,{}>;\n@compute @workgroup_size({},{},1) fn main(@builtin(local_invocation_id) tid:vec3<u32>,@builtin(local_invocation_index) lin:u32,@builtin(workgroup_id) gid:vec3<u32>){{",
        w * h,
        group[0],
        group[1]
    );
    let _ = writeln!(
        s,
        "let origin=vec2<i32>(gid.xy)*vec2<i32>({},{});for(var index=lin;index<{}u;index+={}u){{let q=origin+vec2<i32>(i32(index%{w}u)-1,i32(index/{w}u)-1);lds[index]=F(easu(clamp(q,vec2<i32>(0),vec2<i32>(p.size.zw)-1)));}}workgroupBarrier();",
        group[0],
        group[1],
        w * h,
        group[0] * group[1]
    );
    let _ = writeln!(
        s,
        "let q=origin+vec2<i32>(tid.xy);if(any(q>=vec2<i32>(p.size.zw))){{return;}}let center=(tid.y+1u)*{w}u+tid.x+1u;let e=f32(lds[center]);let b=f32(lds[center-{w}u]);let d=f32(lds[center-1u]);let f=f32(lds[center+1u]);let h=f32(lds[center+{w}u]);"
    );
    s.push_str(
        r#"
    let mn4=min(min(b,d),min(f,h));let mx4=max(max(b,d),max(f,h));
    let hit_min=min(mn4,e)/max(4.0*mx4,1.0/65536.0);
    let hit_max=(1.0-max(mx4,e))/min(4.0*mn4-4.0,-1.0/65536.0);
    let lobe=clamp(max(-hit_min,hit_max),-0.1875,0.0)*p.range.w;
    let v=(lobe*(b+d+f+h)+e)/(4.0*lobe+1.0);
    textureStore(dest,q,vec4<f32>(v));
}
"#,
    );
    Kernel {
        layer: 0,
        source: s,
        weights: vec![[0.0; 4]],
        uniform: true,
        weights_half: false,
        group,
        pixels: 1,
        ring: false,
        head: true,
        fused: false,
        bindings: vec![],
        output_start: 0,
        writes: vec![],
    }
}

/// Narrow clamp-padded chains (Anime4K after sibling folding) stay entirely
/// in two workgroup buffers. Each stage recomputes its shrinking halo. Dense
/// 1x1 head contributions are accumulated as soon as a stage is available,
/// so even dense networks retain only two feature tiles and no device tensors.
fn fused_chain(model: &SrModel, options: Options) -> Option<Vec<Kernel>> {
    if model.padding != Padding::Clamp || model.layers.len() < 2 {
        return None;
    }
    let last = model.layers.last()?;
    if last.output != model.head.tensor
        || last.out_channels != 4
        || last.border.is_some()
        || last.residual.is_some()
    {
        return None;
    }
    let dense = last.activation == Activation::None
        && last.inputs.iter().all(|i| i.kernel == 1 && i.tensor != 0);
    let body = &model.layers[..model.layers.len() - usize::from(dense)];
    let mut previous = 0;
    for layer in body {
        if layer.out_channels > options.fusion_max_channels.min(12)
            || layer.border.is_some()
            || layer.residual.is_some()
            || layer
                .inputs
                .iter()
                .any(|i| i.tensor != previous || i.kernel > 3)
        {
            return None;
        }
        previous = layer.output;
    }
    if dense
        && last
            .inputs
            .iter()
            .any(|i| !body.iter().any(|l| l.output == i.tensor))
    {
        return None;
    }
    let radii: Vec<usize> = body
        .iter()
        .map(|l| l.inputs.iter().map(|i| i.kernel / 2).max().unwrap_or(0))
        .collect();
    let mut kernels = Vec::new();
    for start in (0..body.len()).step_by(options.fusion_depth.clamp(1, 8)) {
        let end = (start + options.fusion_depth.clamp(1, 8)).min(body.len());
        kernels.push(fused_chunk(
            model, options, body, &radii, dense, start, end,
        )?);
    }
    Some(kernels)
}

fn fused_chunk(
    model: &SrModel,
    options: Options,
    body: &[super::model::ConvLayer],
    radii: &[usize],
    dense: bool,
    start: usize,
    end: usize,
) -> Option<Kernel> {
    let last = model.layers.last()?;
    let final_chunk = end == body.len();
    let input_tensor = if start == 0 {
        0
    } else {
        body[start - 1].output
    };
    let halo: usize = radii[start..end].iter().sum();
    let mut group = options.group;
    let extents = |g: [u32; 2]| -> Vec<(usize, usize, usize, usize)> {
        let mut remain = halo;
        let mut v = vec![(
            g[0] as usize * options.pixels as usize + 2 * remain,
            g[1] as usize + 2 * remain,
            model.tensors[input_tensor].div_ceil(4),
            remain,
        )];
        for i in start..end {
            let l = &body[i];
            remain -= radii[i];
            v.push((
                g[0] as usize * options.pixels as usize + 2 * remain,
                g[1] as usize + 2 * remain,
                l.out_channels.div_ceil(4),
                remain,
            ));
        }
        v
    };
    let (extents, capacities) = loop {
        let ex = extents(group);
        let mut caps = [0; 2];
        for (i, &(w, h, c, _)) in ex.iter().enumerate() {
            caps[i % 2] = caps[i % 2].max(w * h * c);
        }
        if caps.iter().sum::<usize>() * if options.half { 8 } else { 16 } <= options.shared_bytes {
            break (ex, caps);
        }
        if group[1] > 1 {
            group[1] /= 2;
        } else if group[0] > 1 {
            group[0] /= 2;
        } else {
            return None;
        }
    };
    let mut weights = Vec::new();
    let mut weight_bases = Vec::new();
    for li in start..end {
        weight_bases.push(weights.len());
        weights.extend(packed_weights(
            model,
            li,
            0,
            body[li].out_channels.div_ceil(4) * 4,
            None,
        ));
    }
    let mut dense_weights = Vec::new();
    if dense {
        for l in &body[start..end] {
            let mut entries = Vec::new();
            for input in last.inputs.iter().filter(|i| i.tensor == l.output) {
                let base = weights.len();
                for c in 0..model.tensors[input.tensor] {
                    weights.push(std::array::from_fn(|o| {
                        input.weights[o * model.tensors[input.tensor] + c]
                    }));
                }
                entries.push((input.transform, base));
            }
            dense_weights.push(entries);
        }
    }
    let uniform = !options.weights_half && weights.len() * 16 <= options.uniform_bytes;
    let mut s = network_preamble(options, weights.len(), uniform);
    fetch(&mut s, model, input_tensor);
    let _ = writeln!(
        s,
        "var<workgroup> stage0:array<vec4<F>,{}>;var<workgroup> stage1:array<vec4<F>,{}>;",
        capacities[0], capacities[1]
    );
    let _ = writeln!(
        s,
        "@compute @workgroup_size({},{},1) fn main(@builtin(local_invocation_id) tid:vec3<u32>,@builtin(local_invocation_index) lin:u32,@builtin(workgroup_id) gid:vec3<u32>){{let origin=p.rect.xy+vec2<i32>(gid.xy)*vec2<i32>({},{});",
        group[0],
        group[1],
        group[0] * options.pixels,
        group[1]
    );
    let (w, h, c, r) = extents[0];
    let _ = writeln!(
        s,
        "for(var index=lin;index<{}u;index+={}u){{let pixel=index/{c}u;let q=origin+vec2<i32>(i32(pixel%{w}u)-{r},i32(pixel/{w}u)-{r});stage0[index]=readv_{input_tensor}(q,index%{c}u);}}workgroupBarrier();",
        w * h * c,
        group[0] * group[1]
    );
    if dense {
        for px in 0..options.pixels {
            if start == 0 {
                let _ = writeln!(
                    s,
                    "var ha{px}=vec4<f32>({:?},{:?},{:?},{:?});",
                    last.bias[0], last.bias[1], last.bias[2], last.bias[3]
                );
            } else {
                let _ = writeln!(
                    s,
                    "var ha{px}=vec4<f32>(0);{{let q=origin+vec2<i32>(i32(tid.x*{}u+{px}u),i32(tid.y));if(all(q<p.rect.xy+p.rect.zw)){{ha{px}=vec4<f32>(arena[addr({}u,q,1u,0u)]);}}}}",
                    options.pixels, model.head.tensor
                );
            }
        }
    }
    for (li, l) in body[start..end].iter().enumerate() {
        let (pw, _, pc, pr) = extents[li];
        let (w, h, channels, r) = extents[li + 1];
        let src = li % 2;
        let dst = (li + 1) % 2;
        let _ = writeln!(
            s,
            "for(var index=lin;index<{}u;index+={}u){{let local=vec2<i32>(i32(index%{w}u)-{r},i32(index/{w}u)-{r});let original=origin+local;let q=clamp(original,vec2<i32>(0),vec2<i32>(p.size.xy)-1);",
            w * h,
            group[0] * group[1]
        );
        for o in 0..channels {
            let _ = writeln!(
                s,
                "var a{o}=vec4<f32>(weights.data[{}]);",
                weight_bases[li] + o
            );
        }
        let mut wi = weight_bases[li] + channels;
        for input in &l.inputs {
            let k = input.kernel;
            let kr = k as i32 / 2;
            let cin = model.tensors[input.tensor];
            let literal = options.unroll || cin <= 4;
            for cb in 0..if literal { cin.div_ceil(4) } else { 1 } {
                let cs = if literal {
                    format!("{cb}u")
                } else {
                    "cb".into()
                };
                if !literal {
                    let _ = writeln!(s, "for(var cb=0u;cb<{}u;cb++){{", cin.div_ceil(4));
                }
                for ky in 0..k {
                    for kx in 0..k {
                        let v = transform(
                            &format!("stage{src}[u32(pos.y*{pw}+pos.x)*{pc}u+{cs}]"),
                            input.transform,
                        )
                        .replace("F(0)", "vec4<F>(0)");
                        let _ = writeln!(
                            s,
                            "{{let pos=clamp(q+vec2<i32>({},{}),vec2<i32>(0),vec2<i32>(p.size.xy)-1)-origin+vec2<i32>({pr});let v={v};",
                            kx as i32 - kr,
                            ky as i32 - kr
                        );
                        for o in 0..channels {
                            if options.half_products {
                                let _ = writeln!(s, "var partial{o}=vec4<F>(0);");
                            }
                            for c in 0..(cin - cb * 4).min(4) {
                                let index = if literal {
                                    (wi + ((cb * 4 + c) * k * k + ky * k + kx) * channels + o)
                                        .to_string()
                                } else {
                                    format!(
                                        "{wi}u+((cb*4u+{c}u)*{}u+{}u)*{channels}u+{o}u",
                                        k * k,
                                        ky * k + kx
                                    )
                                };
                                if !literal && cin % 4 != 0 {
                                    let _ = writeln!(s, "if(cb*4u+{c}u<{cin}u){{");
                                }
                                if options.half_products {
                                    let _ = writeln!(
                                        s,
                                        "partial{o}=fma(vec4<F>(weights.data[{index}]),vec4<F>(v[{c}]),partial{o});"
                                    );
                                } else if options.mad {
                                    let _ = writeln!(
                                        s,
                                        "a{o}+=vec4<f32>(weights.data[{index}])*f32(v[{c}]);"
                                    );
                                } else {
                                    let _ = writeln!(
                                        s,
                                        "a{o}=fma(vec4<f32>(weights.data[{index}]),vec4<f32>(f32(v[{c}])),a{o});"
                                    );
                                }
                                if !literal && cin % 4 != 0 {
                                    s.push_str("}\n");
                                }
                            }
                            if options.half_products {
                                let _ = writeln!(s, "a{o}+=vec4<f32>(partial{o});");
                            }
                        }
                        s.push_str("}\n");
                    }
                }
                if !literal {
                    s.push_str("}\n");
                }
            }
            wi += cin * k * k * channels;
        }
        debug_assert!(wi <= weights.len());
        debug_assert!(w * h * channels <= capacities[dst]);
        for o in 0..channels {
            activation(
                &mut s,
                model,
                start + li,
                o * 4,
                &format!("a{o}"),
                "q",
                false,
            );
            if !dense && start + li + 1 == body.len() {
                s.push_str("if(all(original<p.rect.xy+p.rect.zw)){\n");
                head(&mut s, model, &format!("a{o}"), "original");
                s.push_str("}\n");
            } else {
                let _ = writeln!(s, "stage{dst}[index*{channels}u+{o}u]=vec4<F>(a{o});");
            }
        }
        s.push_str("}\nworkgroupBarrier();\n");
        if dense {
            for px in 0..options.pixels {
                let _ = writeln!(
                    s,
                    "{{let center=vec2<i32>(i32(tid.x*{}u+{px}u)+{r},i32(tid.y)+{r});",
                    options.pixels
                );
                for &(tr, base) in &dense_weights[li] {
                    for c in 0..l.out_channels {
                        let value = transform(
                            &format!(
                                "stage{dst}[u32(center.y*{w}+center.x)*{channels}u+{}u][{}]",
                                c / 4,
                                c % 4
                            ),
                            tr,
                        );
                        let _ = writeln!(
                            s,
                            "ha{px}+=vec4<f32>(vec4<F>(weights.data[{}])*{value});",
                            base + c
                        );
                    }
                }
                s.push_str("}\n");
            }
            // No writer may recycle this buffer before all dense-head reads
            // have completed (workgroups need not execute in lockstep).
            s.push_str("workgroupBarrier();\n");
        }
    }
    if dense || !final_chunk {
        let (w, _, channels, r) = *extents.last()?;
        let dst = (end - start) % 2;
        let output_tensor = body[end - 1].output;
        for px in 0..options.pixels {
            let _ = writeln!(
                s,
                "{{let q=origin+vec2<i32>(i32(tid.x*{}u+{px}u),i32(tid.y));if(all(q<p.rect.xy+p.rect.zw)){{",
                options.pixels
            );
            if dense {
                if final_chunk {
                    head(&mut s, model, &format!("ha{px}"), "q");
                } else {
                    let _ = writeln!(
                        s,
                        "arena[addr({}u,q,1u,0u)]=vec4<F>(ha{px});",
                        model.head.tensor
                    );
                }
            }
            if !final_chunk {
                let _ = writeln!(s, "let local=q-origin+vec2<i32>({r});");
                for c in 0..channels {
                    let _ = writeln!(
                        s,
                        "arena[addr({output_tensor}u,q,{channels}u,{c}u)]=stage{dst}[u32(local.y*{w}+local.x)*{channels}u+{c}u];"
                    );
                }
            }
            s.push_str("}}\n");
        }
    }
    s.push_str("}\n");
    let writes = if final_chunk {
        vec![]
    } else if dense {
        vec![body[end - 1].output, model.head.tensor]
    } else {
        vec![body[end - 1].output]
    };
    Some(Kernel {
        layer: if final_chunk && dense {
            model.layers.len() - 1
        } else {
            end - 1
        },
        source: s,
        weights,
        uniform,
        weights_half: options.weights_half,
        group,
        pixels: options.pixels,
        ring: false,
        head: final_chunk,
        fused: true,
        bindings: (0..model.tensors.len()).collect(),
        output_start: 0,
        writes,
    })
}
