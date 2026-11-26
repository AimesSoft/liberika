//! Cross-backend RGB post-processing user shaders.
//!
//! Accepts either:
//! - Single-pass mpv-style user shaders (`//!HOOK MAIN` or `//!HOOK OUTPUT`
//!   with `vec4 hook()`, `HOOKED_pos`, `HOOKED_pt`, `HOOKED_size`,
//!   `HOOKED_tex(uv)`, `HOOKED_texOff(offset)`, and `MAIN_*` aliases).
//! - Direct WGSL fragment shaders declaring the standard Erika post-process
//!   bindings (`@group(0) @binding(0)` uniform `ErikaPostParams`,
//!   `@group(0) @binding(1)` `texture_2d<f32>`, `@group(0) @binding(2)`
//!   `sampler`, and `@fragment fn fs_main(@location(0) erika_uv: vec2<f32>) -> @location(0) vec4<f32>`).
//!
//! Shaders are parsed and validated once through `naga` and translated to the
//! active renderer's native shading language (MSL on Apple, HLSL on Windows,
//! WGSL on wgpu).

use std::time::Duration;

use crate::core::{PlayerError, Result};

/// Maximum accepted user shader source size (512 KiB).
pub const MAX_SHADER_BYTES: usize = 512 * 1024;

/// Runtime status of the active post-processing shader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PostProcessStatus {
    #[default]
    Off,
    Building,
    Active,
    Inactive,
}

impl PostProcessStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Building => "building",
            Self::Active => "active",
            Self::Inactive => "inactive",
        }
    }
}

/// Snapshot of post-processing runtime state for UI and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PostProcessRuntimeInfo {
    pub status: PostProcessStatus,
    pub name: Option<String>,
    pub reason: Option<String>,
    pub last_gpu_duration: Duration,
}

/// Shared uniform payload uploaded to post-processing shaders (16-byte aligned).
///
/// Layout matches `ErikaPostParams` in the GLSL/WGSL/MSL/HLSL wrappers:
/// - `input_size`: `[width, height, 1/width, 1/height]` of the video RGB source texture.
/// - `output_size`: `[width, height, 1/width, 1/height]` of the rendered video rect.
/// - `timing`: `[time_seconds, frame_index, 0, 0]`.
/// - `rect`: `[x, y, width, height]` of the destination video rect in pixels.
/// - `viewport`: `[surface_width, surface_height, 0, 0]` of the render target.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "wgpu", derive(bytemuck::Pod, bytemuck::Zeroable))]
pub struct PostProcessUniforms {
    pub input_size: [f32; 4],
    pub output_size: [f32; 4],
    pub timing: [f32; 4],
    pub rect: [f32; 4],
    pub viewport: [f32; 4],
}

impl PostProcessUniforms {
    pub fn new(
        input: (u32, u32),
        rect: [f32; 4],
        surface: (u32, u32),
        time_seconds: f64,
        frame_index: u64,
    ) -> Self {
        let iw = input.0.max(1) as f32;
        let ih = input.1.max(1) as f32;
        let ow = rect[2].max(1.0);
        let oh = rect[3].max(1.0);
        let sw = surface.0.max(1) as f32;
        let sh = surface.1.max(1) as f32;
        Self {
            input_size: [iw, ih, 1.0 / iw, 1.0 / ih],
            output_size: [ow, oh, 1.0 / ow, 1.0 / oh],
            timing: [
                time_seconds as f32,
                (frame_index & 0x00ff_ffff) as f32,
                0.0,
                0.0,
            ],
            rect,
            viewport: [sw, sh, 0.0, 0.0],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderSourceKind {
    MpvGlsl,
    Wgsl,
}

/// A parsed and validated user post-processing shader ready for backend codegen.
#[derive(Debug)]
pub struct PostProcessShader {
    pub name: String,
    pub description: Option<String>,
    pub kind: ShaderSourceKind,
    #[cfg(not(target_arch = "wasm32"))]
    module: naga::Module,
    #[cfg(not(target_arch = "wasm32"))]
    info: naga::valid::ModuleInfo,
}

// `naga::Module` and `ModuleInfo` are pure immutable data trees once parsed.
unsafe impl Send for PostProcessShader {}
unsafe impl Sync for PostProcessShader {}

#[cfg(not(target_arch = "wasm32"))]
const GLSL_POST_HEADER: &str = r#"#version 450
layout(location = 0) in vec2 erika_uv;
layout(location = 0) out vec4 erika_frag_color;

layout(set = 0, binding = 0) uniform ErikaPostParams {
    vec4 input_size;
    vec4 output_size;
    vec4 timing;
    vec4 rect;
    vec4 viewport;
} erika_params;

layout(set = 0, binding = 1) uniform texture2D erika_src_tex;
layout(set = 0, binding = 2) uniform sampler erika_src_sampler;

#define HOOKED_pos erika_uv
#define HOOKED_size erika_params.input_size.xy
#define HOOKED_pt erika_params.input_size.zw
#define HOOKED_tex(pos) texture(sampler2D(erika_src_tex, erika_src_sampler), (pos))
#define HOOKED_texOff(off) texture(sampler2D(erika_src_tex, erika_src_sampler), erika_uv + (vec2(off) * erika_params.input_size.zw))
#define MAIN_pos HOOKED_pos
#define MAIN_size HOOKED_size
#define MAIN_pt HOOKED_pt
#define MAIN_tex(pos) HOOKED_tex(pos)
#define MAIN_texOff(off) HOOKED_texOff(off)
#define OUTPUT_pos HOOKED_pos
#define OUTPUT_size erika_params.output_size.xy
#define OUTPUT_pt erika_params.output_size.zw
#define OUTPUT_tex(pos) HOOKED_tex(pos)
#define OUTPUT_texOff(off) HOOKED_texOff(off)
#define random erika_params.timing.x
#define frame int(erika_params.timing.y)
"#;

#[cfg(not(target_arch = "wasm32"))]
const GLSL_POST_FOOTER: &str = r#"
void main() {
    erika_frag_color = hook();
}
"#;

#[cfg(not(target_arch = "wasm32"))]
struct ParsedMpvPost {
    description: Option<String>,
    body: String,
}

#[cfg(not(target_arch = "wasm32"))]
fn preprocess_mpv_post_glsl(name: &str, source: &str) -> Result<ParsedMpvPost> {
    let mut description: Option<String> = None;
    let mut hooks: Vec<String> = Vec::new();
    let mut body = String::with_capacity(source.len());

    for raw in source.lines() {
        let trimmed = raw.trim();
        if let Some(directive) = trimmed.strip_prefix("//!") {
            let (key, value) = directive.split_once(' ').unwrap_or((directive, ""));
            let value = value.trim();
            match key {
                "DESC" if !value.is_empty() && description.is_none() => {
                    description = Some(value.to_string());
                }
                "HOOK" => {
                    hooks.push(value.to_string());
                }
                "SAVE" => {
                    return Err(PlayerError::Renderer(format!(
                        "post-process shader {name:?} uses multi-pass //!SAVE {value}, which is not yet supported"
                    )));
                }
                "BIND" | "WHEN" | "WIDTH" | "HEIGHT" | "COMPONENTS" => {}
                _ => {}
            }
            continue;
        }
        if trimmed.starts_with("#version ") {
            continue;
        }
        body.push_str(raw);
        body.push('\n');
    }

    if hooks.len() > 1 {
        return Err(PlayerError::Renderer(format!(
            "post-process shader {name:?} defines {} //!HOOK passes; only single-pass post-process shaders are supported",
            hooks.len()
        )));
    }
    if let Some(hook) = hooks.first()
        && !matches!(hook.as_str(), "MAIN" | "OUTPUT" | "POSTKERNEL" | "SCALED")
    {
        return Err(PlayerError::Renderer(format!(
            "post-process shader {name:?} targets unsupported hook //!HOOK {hook} (expected MAIN or OUTPUT)"
        )));
    }
    if !body.contains("hook") {
        return Err(PlayerError::Renderer(format!(
            "post-process shader {name:?} does not define vec4 hook()"
        )));
    }

    Ok(ParsedMpvPost { description, body })
}

impl PostProcessShader {
    /// Parses and validates a post-processing shader from UTF-8 source bytes.
    pub fn parse(name: &str, bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Err(PlayerError::Renderer(format!(
                "post-process shader {name:?} is empty"
            )));
        }
        if bytes.len() > MAX_SHADER_BYTES {
            return Err(PlayerError::Renderer(format!(
                "post-process shader {name:?} exceeds the {MAX_SHADER_BYTES}-byte limit"
            )));
        }
        let source = std::str::from_utf8(bytes).map_err(|_| {
            PlayerError::Renderer(format!("post-process shader {name:?} is not valid UTF-8"))
        })?;
        Self::parse_str(name, source)
    }

    /// Parses and validates a post-processing shader from UTF-8 source text.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn parse_str(name: &str, source: &str) -> Result<Self> {
        let is_wgsl = source.contains("@fragment") && !source.contains("//!HOOK");
        let (kind, description, module) = if is_wgsl {
            let module = naga::front::wgsl::parse_str(source).map_err(|err| {
                PlayerError::Renderer(format!(
                    "post-process WGSL parse failed for {name:?}: {}",
                    err.emit_to_string(source)
                ))
            })?;
            (ShaderSourceKind::Wgsl, None, module)
        } else {
            let parsed = preprocess_mpv_post_glsl(name, source)?;
            let wrapped = format!("{GLSL_POST_HEADER}\n{}\n{GLSL_POST_FOOTER}", parsed.body);
            let mut frontend = naga::front::glsl::Frontend::default();
            let options = naga::front::glsl::Options::from(naga::ShaderStage::Fragment);
            let module = frontend.parse(&options, &wrapped).map_err(|err| {
                PlayerError::Renderer(format!(
                    "post-process GLSL parse failed for {name:?}: {err}"
                ))
            })?;
            (ShaderSourceKind::MpvGlsl, parsed.description, module)
        };

        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        );
        let info = validator.validate(&module).map_err(|err| {
            PlayerError::Renderer(format!(
                "post-process shader validation failed for {name:?}: {err}"
            ))
        })?;

        let has_fragment = module
            .entry_points
            .iter()
            .any(|ep| ep.stage == naga::ShaderStage::Fragment);
        if !has_fragment {
            return Err(PlayerError::Renderer(format!(
                "post-process shader {name:?} has no fragment entry point"
            )));
        }

        Ok(Self {
            name: name.to_string(),
            description,
            kind,
            module,
            info,
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub fn parse_str(name: &str, _source: &str) -> Result<Self> {
        Err(PlayerError::Renderer(format!(
            "post-process shader {name:?} is not supported on wasm32"
        )))
    }

    /// Name of the fragment entry point inside the validated module.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn fragment_entry_name(&self) -> &str {
        self.module
            .entry_points
            .iter()
            .find(|ep| ep.stage == naga::ShaderStage::Fragment)
            .map(|ep| ep.name.as_str())
            .unwrap_or("main")
    }

    /// Emits a complete WGSL module containing both the standard full-quad
    /// vertex stage (`erika_post_vs`) and the translated fragment stage.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn to_wgsl(&self) -> Result<(String, String)> {
        let body = naga::back::wgsl::write_string(
            &self.module,
            &self.info,
            naga::back::wgsl::WriterFlags::empty(),
        )
        .map_err(|err| {
            PlayerError::Renderer(format!(
                "post-process WGSL emit failed for {:?}: {err}",
                self.name
            ))
        })?;
        let entry = self.fragment_entry_name().to_string();
        let combined = format!(
            r#"struct ErikaPostVsOut {{
    @builtin(position) position: vec4<f32>,
    @location(0) erika_uv: vec2<f32>,
}};

@vertex
fn erika_post_vs(@builtin(vertex_index) vid: u32) -> ErikaPostVsOut {{
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var uv = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var out: ErikaPostVsOut;
    out.position = vec4<f32>(pos[vid], 0.0, 1.0);
    out.erika_uv = uv[vid];
    return out;
}}

{body}
"#
        );
        Ok((combined, entry))
    }

    /// Emits Metal Shading Language source containing both the fixed vertex
    /// shader (`erika_post_vs`) and the translated fragment shader, returning
    /// `(msl_source, fragment_entry_point_name)`.
    ///
    /// Bindings:
    /// - Vertex & fragment uniform buffer: `buffer(0)` (`PostProcessUniforms`)
    /// - Source video RGB texture: `texture(0)`
    /// - Source sampler: `sampler(0)`
    #[cfg(any(target_vendor = "apple", test))]
    pub fn to_msl(&self) -> Result<(String, String)> {
        let mut resources = naga::back::msl::BindingMap::default();
        resources.insert(
            naga::ResourceBinding {
                group: 0,
                binding: 0,
            },
            naga::back::msl::BindTarget {
                buffer: Some(0),
                ..Default::default()
            },
        );
        resources.insert(
            naga::ResourceBinding {
                group: 0,
                binding: 1,
            },
            naga::back::msl::BindTarget {
                texture: Some(0),
                ..Default::default()
            },
        );
        resources.insert(
            naga::ResourceBinding {
                group: 0,
                binding: 2,
            },
            naga::back::msl::BindTarget {
                sampler: Some(naga::back::msl::BindSamplerTarget::Resource(0)),
                ..Default::default()
            },
        );
        let options = naga::back::msl::Options {
            lang_version: (2, 1),
            per_entry_point_map: {
                let mut map = naga::back::msl::EntryPointResourceMap::default();
                map.insert(
                    self.fragment_entry_name().to_string(),
                    naga::back::msl::EntryPointResources {
                        resources,
                        ..Default::default()
                    },
                );
                map
            },
            fake_missing_bindings: false,
            ..Default::default()
        };
        let pipeline_options = naga::back::msl::PipelineOptions::default();
        let (frag_source, translation) =
            naga::back::msl::write_string(&self.module, &self.info, &options, &pipeline_options)
                .map_err(|err| {
                    PlayerError::Renderer(format!(
                        "post-process MSL emit failed for {:?}: {err}",
                        self.name
                    ))
                })?;

        let frag_index = self
            .module
            .entry_points
            .iter()
            .position(|ep| ep.stage == naga::ShaderStage::Fragment)
            .unwrap_or(0);
        let msl_entry = translation
            .entry_point_names
            .get(frag_index)
            .and_then(|res| res.as_ref().ok())
            .cloned()
            .unwrap_or_else(|| "main_".to_string());

        let combined = format!(
            r#"{frag_source}

struct ErikaPostVsUniforms {{
    metal::float4 input_size;
    metal::float4 output_size;
    metal::float4 timing;
    metal::float4 rect;
    metal::float4 viewport;
}};

struct ErikaPostVsOut {{
    metal::float4 position [[position]];
    metal::float2 uv [[user(loc0)]];
}};

vertex ErikaPostVsOut erika_post_vs(
    uint vertex_id [[vertex_id]],
    constant ErikaPostVsUniforms& u [[buffer(0)]]
) {{
    constexpr metal::float2 corners[4] = {{
        metal::float2(0.0, 0.0),
        metal::float2(1.0, 0.0),
        metal::float2(0.0, 1.0),
        metal::float2(1.0, 1.0),
    }};
    metal::float2 uv = corners[vertex_id];
    metal::float2 px = u.rect.xy + uv * u.rect.zw;
    metal::float2 vp = metal::max(u.viewport.xy, metal::float2(1.0, 1.0));
    metal::float2 ndc = metal::float2(
        (px.x / vp.x) * 2.0 - 1.0,
        1.0 - (px.y / vp.y) * 2.0
    );
    ErikaPostVsOut out;
    out.position = metal::float4(ndc, 0.0, 1.0);
    out.uv = uv;
    return out;
}}
"#
        );
        Ok((combined, msl_entry))
    }

    /// Emits HLSL Shader Model 5.0 pixel shader source, returning
    /// `(hlsl_pixel_source, pixel_entry_point_name)`.
    ///
    /// Bindings:
    /// - Constant buffer: `register(b0)` (`PostProcessUniforms`)
    /// - Source video RGB texture: `register(t0)`
    /// - Source sampler: `register(s0)`
    /// - Vertex-to-pixel input: `float2 uv : LOC0;`
    #[cfg(any(target_os = "windows", test))]
    pub fn to_hlsl(&self) -> Result<(String, String)> {
        let mut binding_map = naga::back::hlsl::BindingMap::default();
        binding_map.insert(
            naga::ResourceBinding {
                group: 0,
                binding: 0,
            },
            naga::back::hlsl::BindTarget {
                space: 0,
                register: 0,
                binding_array_size: None,
                dynamic_storage_buffer_offsets_index: None,
                restrict_indexing: false,
            },
        );
        binding_map.insert(
            naga::ResourceBinding {
                group: 0,
                binding: 1,
            },
            naga::back::hlsl::BindTarget {
                space: 0,
                register: 0,
                binding_array_size: None,
                dynamic_storage_buffer_offsets_index: None,
                restrict_indexing: false,
            },
        );
        binding_map.insert(
            naga::ResourceBinding {
                group: 0,
                binding: 2,
            },
            naga::back::hlsl::BindTarget {
                space: 0,
                register: 0,
                binding_array_size: None,
                dynamic_storage_buffer_offsets_index: None,
                restrict_indexing: false,
            },
        );
        let mut sampler_buffer_binding_map = naga::back::hlsl::SamplerIndexBufferBindingMap::default();
        sampler_buffer_binding_map.insert(
            naga::back::hlsl::SamplerIndexBufferKey { group: 0 },
            naga::back::hlsl::BindTarget {
                space: 0,
                register: 7,
                binding_array_size: None,
                dynamic_storage_buffer_offsets_index: None,
                restrict_indexing: false,
            },
        );
        let options = naga::back::hlsl::Options {
            shader_model: naga::back::hlsl::ShaderModel::V5_0,
            binding_map,
            sampler_buffer_binding_map,
            fake_missing_bindings: false,
            ..Default::default()
        };
        let pipeline_options = naga::back::hlsl::PipelineOptions::default();
        let mut raw = String::new();
        let mut writer = naga::back::hlsl::Writer::new(&mut raw, &options, &pipeline_options);
        let reflection = writer.write(&self.module, &self.info, None).map_err(|err| {
            PlayerError::Renderer(format!(
                "post-process HLSL emit failed for {:?}: {err}",
                self.name
            ))
        })?;
        let out = lower_hlsl_sampler_heap_to_d3d11(&raw);
        let frag_index = self
            .module
            .entry_points
            .iter()
            .position(|ep| ep.stage == naga::ShaderStage::Fragment)
            .unwrap_or(0);
        let entry = reflection
            .entry_point_names
            .get(frag_index)
            .and_then(|res| res.as_ref().ok())
            .cloned()
            .unwrap_or_else(|| "main".to_string());
        Ok((out, entry))
    }
}

/// Adapts naga's D3D12-style sampler-heap bindings into plain Direct3D 11
/// Shader Model 5.0 `register(s0)` declarations accepted by FXC `D3DCompile`.
#[cfg(any(target_os = "windows", test))]
fn lower_hlsl_sampler_heap_to_d3d11(hlsl: &str) -> String {
    let mut out = String::with_capacity(hlsl.len());
    for line in hlsl.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("SamplerState nagaSamplerHeap")
            || trimmed.starts_with("SamplerComparisonState nagaComparisonSamplerHeap")
            || trimmed.starts_with("StructuredBuffer<uint> nagaGroup")
        {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("static const SamplerState ")
            && let Some((name, _)) = rest.split_once(" = nagaSamplerHeap[")
        {
            out.push_str("SamplerState ");
            out.push_str(name.trim());
            out.push_str(" : register(s0);\n");
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("static const SamplerComparisonState ")
            && let Some((name, _)) = rest.split_once(" = nagaComparisonSamplerHeap[")
        {
            out.push_str("SamplerComparisonState ");
            out.push_str(name.trim());
            out.push_str(" : register(s0);\n");
            continue;
        }
        out.push_str(&line.replace(", space0)", ")"));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRT_LITE: &str = r#"//!DESC CRT Lite
//!HOOK MAIN
//!BIND HOOKED

vec4 hook() {
    vec2 uv = HOOKED_pos;
    vec2 px = HOOKED_pt;

    vec4 color = HOOKED_tex(uv);

    float y = uv.y / px.y;
    float line = mod(floor(y), 2.0);
    float scan = mix(1.0, 0.88, line);
    color.rgb *= scan;

    vec2 centered = uv * 2.0 - 1.0;
    float vignette = 1.0 - smoothstep(0.6, 1.2, length(centered));
    color.rgb *= mix(1.0, vignette, 0.35);

    color.rgb *= 1.05;
    return vec4(clamp(color.rgb, 0.0, 1.0), color.a);
}
"#;

    const CRT_HIGH: &str = r#"//!DESC CRT High
//!HOOK MAIN
//!BIND HOOKED

const float PI = 3.141592653589793;

float rand(vec2 co) {
    return fract(sin(dot(co.xy, vec2(12.9898, 78.233))) * 43758.5453);
}

vec4 hook() {
    vec2 uv = HOOKED_pos;
    vec2 px = HOOKED_pt;

    vec2 centered = uv * 2.0 - 1.0;
    float r2 = dot(centered, centered);
    float curvature = 0.18;
    vec2 distorted = centered * (1.0 + curvature * r2);
    vec2 duv = distorted * 0.5 + 0.5;

    float inside = step(0.0, duv.x) * step(duv.x, 1.0) *
        step(0.0, duv.y) * step(duv.y, 1.0);
    vec2 clamped = clamp(duv, 0.0, 1.0);

    vec2 caOffset = vec2(px.x * 0.6, 0.0);
    float r = HOOKED_tex(clamped + caOffset).r;
    float g = HOOKED_tex(clamped).g;
    float b = HOOKED_tex(clamped - caOffset).b;
    vec3 color = vec3(r, g, b);

    vec3 bloom = vec3(0.0);
    bloom += HOOKED_tex(clamped + vec2(px.x, 0.0)).rgb;
    bloom += HOOKED_tex(clamped - vec2(px.x, 0.0)).rgb;
    bloom += HOOKED_tex(clamped + vec2(0.0, px.y)).rgb;
    bloom += HOOKED_tex(clamped - vec2(0.0, px.y)).rgb;
    bloom *= 0.25;
    color = mix(color, bloom, 0.25);

    float y = clamped.y / px.y;
    float line = mod(floor(y), 2.0);
    float scan = mix(1.0, 0.78, line);
    color *= scan;

    float x = clamped.x / px.x;
    float tri = x * (2.0 * PI / 3.0);
    vec3 mask = vec3(
        sin(tri) * 0.5 + 0.5,
        sin(tri + 2.094395102393195) * 0.5 + 0.5,
        sin(tri + 4.1887902047863905) * 0.5 + 0.5
    );
    const float maskStrength = 0.45;
    color *= mix(vec3(1.0 - maskStrength), vec3(1.0), mask);

    float vignette = 1.0 - smoothstep(0.45, 1.05, length(distorted));
    color *= mix(1.0, vignette, 0.55);

    float noise = rand(clamped * vec2(900.0, 600.0));
    color *= 1.0 + (noise - 0.5) * 0.03;

    color *= 1.12;
    color *= inside;
    return vec4(clamp(color, 0.0, 1.0), 1.0);
}
"#;

    #[test]
    fn crt_mpv_shaders_translate_to_wgsl_msl_and_hlsl() {
        for (name, src) in [("crt_lite", CRT_LITE), ("crt_high", CRT_HIGH)] {
            let shader = PostProcessShader::parse_str(name, src).expect("parse crt");
            assert_eq!(shader.kind, ShaderSourceKind::MpvGlsl);
            let (wgsl, wgsl_ep) = shader.to_wgsl().expect("wgsl");
            assert!(wgsl.contains("erika_post_vs"));
            assert!(!wgsl_ep.is_empty());

            let (msl, msl_ep) = shader.to_msl().expect("msl");
            assert!(msl.contains("erika_post_vs"));
            assert!(msl.contains("[[user(loc0)]]"), "msl:\n{msl}");
            assert!(!msl_ep.is_empty());

            let (hlsl, hlsl_ep) = shader.to_hlsl().expect("hlsl");
            assert!(hlsl.contains("LOC0"), "hlsl:\n{hlsl}");
            assert!(hlsl.contains("register(b0)"), "hlsl:\n{hlsl}");
            assert!(hlsl.contains("register(t0)"), "hlsl:\n{hlsl}");
            assert!(hlsl.contains("register(s0)"), "hlsl:\n{hlsl}");
            assert!(!hlsl.contains("nagaSamplerHeap"), "hlsl:\n{hlsl}");
            assert!(!hlsl.contains("space0"), "hlsl:\n{hlsl}");
            assert!(!hlsl_ep.is_empty());
        }
    }

    #[test]
    fn rejects_invalid_or_multipass_post_process_shaders() {
        assert!(PostProcessShader::parse("empty", b"").is_err());
        assert!(PostProcessShader::parse("bad_utf8", &[0xff, 0xfe]).is_err());
        assert!(PostProcessShader::parse_str("no_hook", "void foo() {}").is_err());
        let multipass = "//!HOOK MAIN\n//!SAVE TMP\nvec4 hook(){return vec4(1.0);}";
        assert!(PostProcessShader::parse_str("multi", multipass).is_err());
    }

    #[test]
    fn default_upscaler_probe_returns_consistent_descriptors() {
        let caps = crate::renderer::probe_default_upscalers();
        assert!(!caps.modes.is_empty());
        assert!(caps.supports_custom_network);
        assert!(caps.supports_post_process);
        for desc in &caps.modes {
            assert_eq!(
                crate::renderer::pipeline::LumaUpscalerMode::from_label(desc.id),
                Some(desc.mode)
            );
            assert!(!desc.group_id.is_empty());
            assert!(!desc.group_name.is_empty());
            assert!(!desc.name.is_empty());
        }
    }
}
