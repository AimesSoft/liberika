//! Direct3D 11 RGB post-processing pass driven by a validated
//! [`PostProcessShader`].

use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

use ::windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use ::windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_SAMPLER_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11Resource, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader,
};
use ::windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use ::windows::core::{Interface, PCSTR};

use crate::core::{PlayerError, Result};
use crate::renderer::post_process::{
    PostProcessRuntimeInfo, PostProcessShader, PostProcessStage, PostProcessStatus,
    PostProcessUniforms,
};

const POST_VS_SOURCE: &[u8] = br#"
// Keep user varyings (`LOC0`, ...) before `SV_Position` so the DXBC output
// signature registers (`o0`, `o1`, ...) match naga's HLSL fragment input
// ordering (locations first, built-ins last).
struct PostVsOut {
    float2 uv : LOC0;
    float4 position : SV_Position;
};

PostVsOut erika_post_vs(uint vertex_id : SV_VertexID) {
    float2 corners[4] = {
        float2(0.0, 0.0),
        float2(1.0, 0.0),
        float2(0.0, 1.0),
        float2(1.0, 1.0)
    };
    float2 uv = corners[vertex_id];
    PostVsOut output;
    output.position = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    output.uv = uv;
    return output;
}
"#;

struct SourceTarget {
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    _texture: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
}

struct DeviceResources {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: Option<ID3D11PixelShader>,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    source: Option<SourceTarget>,
}

pub(crate) struct D3d11PostProcess {
    shader: Option<Arc<PostProcessShader>>,
    resources: Option<DeviceResources>,
    generation: u64,
    status: PostProcessStatus,
    reason: Option<String>,
    last_gpu_duration: Duration,
}

impl Default for D3d11PostProcess {
    fn default() -> Self {
        Self {
            shader: None,
            resources: None,
            generation: 0,
            status: PostProcessStatus::Off,
            reason: None,
            last_gpu_duration: Duration::ZERO,
        }
    }
}

impl D3d11PostProcess {
    pub(crate) fn is_enabled(&self) -> bool {
        self.shader.is_some()
    }

    pub(crate) fn stage(&self) -> PostProcessStage {
        self.shader
            .as_ref()
            .map_or(PostProcessStage::Main, |shader| shader.stage)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn reset_device(&mut self) {
        self.resources = None;
        if self.shader.is_some() {
            self.status = PostProcessStatus::Building;
            self.reason = None;
        }
    }

    pub(crate) fn set_shader(&mut self, shader: Option<Arc<PostProcessShader>>) {
        let same = match (&self.shader, &shader) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.shader = shader;
        self.generation = self.generation.wrapping_add(1);
        self.reason = None;
        self.last_gpu_duration = Duration::ZERO;
        if let Some(res) = self.resources.as_mut() {
            res.pixel_shader = None;
            if self.shader.is_none() {
                res.source = None;
            }
        }
        self.status = if self.shader.is_some() {
            PostProcessStatus::Building
        } else {
            PostProcessStatus::Off
        };
    }

    pub(crate) fn status_info(&self) -> PostProcessRuntimeInfo {
        PostProcessRuntimeInfo {
            status: self.status,
            name: self.shader.as_ref().map(|shader| shader.name.clone()),
            reason: self.reason.clone(),
            last_gpu_duration: self.last_gpu_duration,
        }
    }

    /// Prepares the offscreen RGB render target and compiled shaders.
    /// Returns the intermediate RTV that `draw_video` should render into, or
    /// `None` if post-processing is disabled or failed to compile.
    pub(crate) fn prepare(
        &mut self,
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Option<ID3D11RenderTargetView> {
        let shader = self.shader.clone()?;
        if self.resources.is_none() {
            match create_device_resources(device) {
                Ok(res) => self.resources = Some(res),
                Err(error) => {
                    self.status = PostProcessStatus::Inactive;
                    self.reason = Some(error.to_string());
                    return None;
                }
            }
        }
        let res = self.resources.as_mut().expect("resources created");
        if res.pixel_shader.is_none() {
            match compile_pixel_shader(device, &shader) {
                Ok(ps) => {
                    res.pixel_shader = Some(ps);
                    self.reason = None;
                }
                Err(error) => {
                    self.status = PostProcessStatus::Inactive;
                    self.reason = Some(error.to_string());
                    return None;
                }
            }
        }
        let width = width.max(1);
        let height = height.max(1);
        let needs_target = res
            .source
            .as_ref()
            .is_none_or(|src| src.width != width || src.height != height || src.format != format);
        if needs_target {
            match create_source_target(device, width, height, format) {
                Ok(target) => res.source = Some(target),
                Err(error) => {
                    self.status = PostProcessStatus::Inactive;
                    self.reason = Some(error.to_string());
                    return None;
                }
            }
        }
        self.status = PostProcessStatus::Active;
        self.reason = None;
        res.source.as_ref().map(|src| src.rtv.clone())
    }

    /// Runs the compiled post-processing pixel shader from the intermediate
    /// source texture into `render_target` over `viewport`.
    pub(crate) fn draw(
        &self,
        context: &ID3D11DeviceContext,
        render_target: &ID3D11RenderTargetView,
        viewport: D3D11_VIEWPORT,
        uniforms: &PostProcessUniforms,
    ) {
        let Some(res) = self.resources.as_ref() else {
            return;
        };
        let (Some(pixel_shader), Some(source)) = (res.pixel_shader.as_ref(), res.source.as_ref())
        else {
            return;
        };
        unsafe {
            context.UpdateSubresource(
                &res.constants,
                0,
                None,
                uniforms as *const PostProcessUniforms as *const c_void,
                0,
                0,
            );
            context.OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            context.OMSetBlendState(None, None, u32::MAX);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.VSSetShader(&res.vertex_shader, None);
            context.PSSetShader(pixel_shader, None);
            context.PSSetConstantBuffers(0, Some(&[Some(res.constants.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(source.srv.clone())]));
            context.PSSetSamplers(0, Some(&[Some(res.sampler.clone())]));
            context.Draw(4, 0);
            context.PSSetShaderResources(0, Some(&[None]));
        }
    }
}

fn compile_hlsl(source: &[u8], entry: &str, target: &str) -> Result<Vec<u8>> {
    let mut entry_nul = entry.as_bytes().to_vec();
    entry_nul.push(0);
    let mut target_nul = target.as_bytes().to_vec();
    target_nul.push(0);
    let mut code = None;
    let mut errors = None;
    unsafe {
        let result = D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry_nul.as_ptr()),
            PCSTR(target_nul.as_ptr()),
            0,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(error) = result {
            let detail = errors
                .as_ref()
                .map(|blob| {
                    let bytes = std::slice::from_raw_parts(
                        blob.GetBufferPointer() as *const u8,
                        blob.GetBufferSize(),
                    );
                    String::from_utf8_lossy(bytes).trim().to_string()
                })
                .filter(|msg| !msg.is_empty())
                .unwrap_or_else(|| error.to_string());
            return Err(PlayerError::Renderer(format!(
                "d3d11 post-process D3DCompile({entry}, {target}) failed: {detail}"
            )));
        }
        let blob = code.ok_or_else(|| {
            PlayerError::Renderer("d3d11 post-process D3DCompile returned null blob".into())
        })?;
        Ok(
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
                .to_vec(),
        )
    }
}

fn create_device_resources(device: &ID3D11Device) -> Result<DeviceResources> {
    let vs_bytes = compile_hlsl(POST_VS_SOURCE, "erika_post_vs", "vs_5_0")?;
    let mut vertex_shader = None;
    unsafe {
        device
            .CreateVertexShader(&vs_bytes, None, Some(&mut vertex_shader))
            .map_err(|error| {
                PlayerError::Renderer(format!("d3d11 CreateVertexShader(post): {error}"))
            })?;
    }
    let vertex_shader = vertex_shader
        .ok_or_else(|| PlayerError::Renderer("d3d11 post vertex shader was null".into()))?;

    let cbuf_desc = D3D11_BUFFER_DESC {
        ByteWidth: mem::size_of::<PostProcessUniforms>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut constants = None;
    unsafe {
        device
            .CreateBuffer(&cbuf_desc, None, Some(&mut constants))
            .map_err(|error| PlayerError::Renderer(format!("d3d11 CreateBuffer(post): {error}")))?;
    }
    let constants =
        constants.ok_or_else(|| PlayerError::Renderer("d3d11 post cbuffer was null".into()))?;

    let sampler_desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };
    let mut sampler = None;
    unsafe {
        device
            .CreateSamplerState(&sampler_desc, Some(&mut sampler))
            .map_err(|error| {
                PlayerError::Renderer(format!("d3d11 CreateSamplerState(post): {error}"))
            })?;
    }
    let sampler =
        sampler.ok_or_else(|| PlayerError::Renderer("d3d11 post sampler was null".into()))?;

    Ok(DeviceResources {
        vertex_shader,
        pixel_shader: None,
        constants,
        sampler,
        source: None,
    })
}

fn compile_pixel_shader(
    device: &ID3D11Device,
    shader: &PostProcessShader,
) -> Result<ID3D11PixelShader> {
    let (hlsl, entry) = shader.to_hlsl()?;
    let ps_bytes = compile_hlsl(hlsl.as_bytes(), &entry, "ps_5_0")?;
    let mut pixel_shader = None;
    unsafe {
        device
            .CreatePixelShader(&ps_bytes, None, Some(&mut pixel_shader))
            .map_err(|error| {
                PlayerError::Renderer(format!(
                    "d3d11 CreatePixelShader(post {:?}): {error}",
                    shader.name
                ))
            })?;
    }
    pixel_shader.ok_or_else(|| PlayerError::Renderer("d3d11 post pixel shader was null".into()))
}

fn create_source_target(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<SourceTarget> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| {
                PlayerError::Renderer(format!("d3d11 CreateTexture2D(post src): {error}"))
            })?;
    }
    let texture = texture
        .ok_or_else(|| PlayerError::Renderer("d3d11 post source texture was null".into()))?;
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| PlayerError::Renderer(format!("d3d11 post cast resource: {error}")))?;
    let mut rtv = None;
    unsafe {
        device
            .CreateRenderTargetView(&resource, None, Some(&mut rtv))
            .map_err(|error| {
                PlayerError::Renderer(format!("d3d11 CreateRTV(post src): {error}"))
            })?;
    }
    let rtv = rtv.ok_or_else(|| PlayerError::Renderer("d3d11 post RTV was null".into()))?;
    let mut srv = None;
    unsafe {
        device
            .CreateShaderResourceView(&resource, None, Some(&mut srv))
            .map_err(|error| {
                PlayerError::Renderer(format!("d3d11 CreateSRV(post src): {error}"))
            })?;
    }
    let srv = srv.ok_or_else(|| PlayerError::Renderer("d3d11 post SRV was null".into()))?;
    let _ = ptr::null::<c_void>();
    Ok(SourceTarget {
        width,
        height,
        format,
        _texture: texture,
        rtv,
        srv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        ColorPrimaries, FlutterTextureHandle, FlutterTextureKind, PlatformSurface, TransferFunction,
    };
    use crate::ffmpeg::{PlanarFrame, PlanarPixelFormat};
    use crate::renderer::d3d11::tests::{gpu_test_renderer, read_bgra_pixels};
    use crate::renderer::d3d11::{
        D3d11Renderer, SDR_SWAPCHAIN_FORMAT, create_flutter_texture_target,
    };
    use crate::renderer::pipeline::SourceColorState;
    use crate::{RenderFrameContext, RendererBackend};
    use ::windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    };

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

    const CRT_STANDARD: &str = r#"//!DESC CRT Standard
//!HOOK MAIN
//!BIND HOOKED

const float PI = 3.141592653589793;

vec4 hook() {
    vec2 uv = HOOKED_pos;
    vec2 px = HOOKED_pt;

    vec2 centered = uv * 2.0 - 1.0;
    float r2 = dot(centered, centered);
    float curvature = 0.12;
    vec2 distorted = centered * (1.0 + curvature * r2);
    vec2 duv = distorted * 0.5 + 0.5;

    float inside = step(0.0, duv.x) * step(duv.x, 1.0) *
        step(0.0, duv.y) * step(duv.y, 1.0);
    vec4 color = HOOKED_tex(clamp(duv, 0.0, 1.0));
    color.rgb *= inside;

    float y = duv.y / px.y;
    float line = mod(floor(y), 2.0);
    float scan = mix(1.0, 0.85, line);
    color.rgb *= scan;

    float x = duv.x / px.x;
    float tri = x * (2.0 * PI / 3.0);
    vec3 mask = vec3(
        sin(tri) * 0.5 + 0.5,
        sin(tri + 2.094395102393195) * 0.5 + 0.5,
        sin(tri + 4.1887902047863905) * 0.5 + 0.5
    );
    const float maskStrength = 0.35;
    color.rgb *= mix(vec3(1.0 - maskStrength), vec3(1.0), mask);

    float vignette = 1.0 - smoothstep(0.5, 1.1, length(distorted));
    color.rgb *= mix(1.0, vignette, 0.45);

    color.rgb *= 1.08;
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

    const FRAGCOORD_ONLY: &str = r#"//!DESC FragCoord Only
//!HOOK MAIN
//!BIND HOOKED

vec4 hook() {
    vec2 p = clamp(gl_FragCoord.xy / OUTPUT_size, 0.0, 1.0);
    return vec4(p.x, p.y, 0.0, 1.0);
}
"#;

    const FRAGCOORD_AND_UV: &str = r#"//!DESC FragCoord And UV
//!HOOK MAIN
//!BIND HOOKED

vec4 hook() {
    vec2 p = clamp(gl_FragCoord.xy / OUTPUT_size, 0.0, 1.0);
    vec4 tex = HOOKED_tex(HOOKED_pos);
    return vec4(tex.r, p.x, p.y, 1.0);
}
"#;

    const DIRECT_WGSL: &str = r#"
struct ErikaPostParams {
    input_size: vec4<f32>,
    output_size: vec4<f32>,
    timing: vec4<f32>,
    rect: vec4<f32>,
    viewport: vec4<f32>,
};

@group(0) @binding(0) var<uniform> erika_params: ErikaPostParams;
@group(0) @binding(1) var erika_src_tex: texture_2d<f32>;
@group(0) @binding(2) var erika_src_sampler: sampler;

@fragment
fn fs_main(@location(0) erika_uv: vec2<f32>) -> @location(0) vec4<f32> {
    let _size = erika_params.input_size;
    return textureSample(erika_src_tex, erika_src_sampler, erika_uv);
}
"#;

    fn test_driver() -> ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE {
        match std::env::var("ERIKA_D3D11_POST_TEST_DRIVER").as_deref() {
            Ok("hardware") => D3D_DRIVER_TYPE_HARDWARE,
            Ok("warp") | Err(std::env::VarError::NotPresent) => D3D_DRIVER_TYPE_WARP,
            other => {
                panic!("expected ERIKA_D3D11_POST_TEST_DRIVER=hardware or warp, got {other:?}")
            }
        }
    }

    fn run_shader_on_quadrants(
        driver: ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE,
        name: &str,
        source: &str,
    ) -> Vec<[u8; 4]> {
        let renderer = gpu_test_renderer(driver, D3D_FEATURE_LEVEL_11_0);
        let state = renderer.state.as_ref().unwrap();
        let width = 64u32;
        let height = 64u32;

        let shader = Arc::new(PostProcessShader::parse_str(name, source).unwrap());
        let mut post = D3d11PostProcess::default();
        post.set_shader(Some(shader));
        let _rtv = post
            .prepare(&state.device, width, height, SDR_SWAPCHAIN_FORMAT)
            .unwrap_or_else(|| panic!("d3d11 post prepare failed: {:?}", post.status_info()));
        assert_eq!(post.status_info().status, PostProcessStatus::Active);

        // Upload a 4-quadrant BGRA texture into the post-process source texture:
        // TL = red, TR = green, BL = yellow, BR = blue.
        let mut src_bgra = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let bgra = match (x < width / 2, y < height / 2) {
                    (true, true) => [20, 20, 220, 255],   // Red
                    (false, true) => [20, 220, 20, 255],  // Green
                    (true, false) => [20, 220, 220, 255], // Yellow
                    (false, false) => [220, 20, 20, 255], // Blue
                };
                let offset = ((y * width + x) * 4) as usize;
                src_bgra[offset..offset + 4].copy_from_slice(&bgra);
            }
        }
        let src_tex = &post
            .resources
            .as_ref()
            .unwrap()
            .source
            .as_ref()
            .unwrap()
            ._texture;
        unsafe {
            state.context.UpdateSubresource(
                src_tex,
                0,
                None,
                src_bgra.as_ptr().cast(),
                width * 4,
                0,
            );
        }

        let (dst_tex, dst_rtv) =
            create_flutter_texture_target(&state.device, width, height).unwrap();
        unsafe {
            state
                .context
                .ClearRenderTargetView(&dst_rtv, &[0.0, 0.0, 0.0, 1.0]);
        }
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let uniforms = PostProcessUniforms::new(
            (width, height),
            [0.0, 0.0, width as f32, height as f32],
            (width, height),
            0.0,
            0,
        );
        post.draw(&state.context, &dst_rtv, viewport, &uniforms);
        read_bgra_pixels(state, &dst_tex)
    }

    #[test]
    fn d3d11_executes_crt_post_process_shaders() {
        let driver = test_driver();
        let px = |pixels: &[[u8; 4]], x: u32, y: u32| pixels[(y * 64 + x) as usize];

        for (name, src, has_curvature) in [
            ("crt_lite", CRT_LITE, false),
            ("crt_standard", CRT_STANDARD, true),
            ("crt_high", CRT_HIGH, true),
            ("direct_wgsl", DIRECT_WGSL, false),
        ] {
            let pixels = run_shader_on_quadrants(driver, name, src);
            let tl = px(&pixels, 16, 16); // BGRA -> Red (R=tl[2])
            let tr = px(&pixels, 48, 16); // BGRA -> Green (G=tr[1])
            let bl = px(&pixels, 16, 48); // BGRA -> Yellow (R & G)
            let br = px(&pixels, 48, 48); // BGRA -> Blue (B=br[0])
            let corner = px(&pixels, 0, 0);

            assert!(
                tl[2] > 100 && tl[0] < 60 && tl[1] < 60,
                "{name} TL not red: {tl:?}"
            );
            assert!(
                tr[1] > 100 && tr[0] < 60 && tr[2] < 60,
                "{name} TR not green: {tr:?}"
            );
            assert!(
                bl[2] > 100 && bl[1] > 100 && bl[0] < 60,
                "{name} BL not yellow: {bl:?}"
            );
            assert!(
                br[0] > 100 && br[1] < 60 && br[2] < 60,
                "{name} BR not blue: {br:?}"
            );
            if has_curvature {
                assert_eq!(
                    [corner[0], corner[1], corner[2]],
                    [0, 0, 0],
                    "{name} corner should clip to black: {corner:?}"
                );
            }
            if name == "crt_lite" {
                let even_row = px(&pixels, 16, 16)[2];
                let odd_row = px(&pixels, 16, 17)[2];
                assert!(
                    even_row > odd_row + 15,
                    "crt_lite scanline modulation missing: even={even_row} odd={odd_row}"
                );
            }
        }

        // Verify gl_FragCoord (SV_Position) both alongside HOOKED_pos (LOC0) and alone.
        let fc_both = run_shader_on_quadrants(driver, "fc_both", FRAGCOORD_AND_UV);
        let p_tl = px(&fc_both, 16, 16);
        let p_br = px(&fc_both, 48, 48);
        assert!(
            p_tl[2] > 150 && p_tl[1] < 90 && p_tl[0] < 90,
            "fc_both TL: {p_tl:?}"
        );
        assert!(
            p_br[2] < 50 && p_br[1] > 150 && p_br[0] > 150,
            "fc_both BR: {p_br:?}"
        );

        let fc_only = run_shader_on_quadrants(driver, "fc_only", FRAGCOORD_ONLY);
        let o_tl = px(&fc_only, 16, 16);
        let o_br = px(&fc_only, 48, 48);
        assert!(o_tl[2] < 90 && o_tl[1] < 90, "fc_only TL: {o_tl:?}");
        assert!(o_br[2] > 150 && o_br[1] > 150, "fc_only BR: {o_br:?}");
    }

    #[test]
    fn d3d11_renderer_applies_crt_shaders_with_letterbox_viewport() {
        let mut renderer = gpu_test_renderer(test_driver(), D3D_FEATURE_LEVEL_11_0);
        // 64x32 video letterboxed inside a 64x64 surface (TopLeftY = 16).
        renderer
            .attach_surface(PlatformSurface::FlutterTexture(FlutterTextureHandle::new(
                FlutterTextureKind::WindowsTextureRegistrar,
                1,
                64,
                64,
                1.0,
            )))
            .unwrap();

        let width = 64u32;
        let height = 32u32;
        let mut luma = vec![128u8; (width * height) as usize];
        let mut chroma = vec![128u8; ((width / 2) * (height / 2) * 2) as usize];
        // Left half red (Cb=64, Cr=210), right half blue (Cb=210, Cr=64).
        for cy in 0..(height / 2) {
            for cx in 0..(width / 2) {
                let offset = ((cy * (width / 2) + cx) * 2) as usize;
                if cx < width / 4 {
                    chroma[offset] = 64;
                    chroma[offset + 1] = 210;
                } else {
                    chroma[offset] = 210;
                    chroma[offset + 1] = 64;
                }
            }
        }
        luma.fill(110);
        renderer
            .upload_planar_frame(
                &PlanarFrame {
                    format: PlanarPixelFormat::Nv12,
                    width,
                    height,
                    luma,
                    chroma,
                },
                SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Bt1886),
            )
            .unwrap();

        for (idx, (name, src)) in [
            ("crt_lite", CRT_LITE),
            ("crt_standard", CRT_STANDARD),
            ("crt_high", CRT_HIGH),
        ]
        .into_iter()
        .enumerate()
        {
            let shader = Arc::new(PostProcessShader::parse_str(name, src).unwrap());
            renderer.set_post_process_shader(Some(shader));
            assert!(
                renderer
                    .render_current_frame(RenderFrameContext::new(Duration::ZERO, idx as u64 + 1))
                    .unwrap()
            );
            assert_eq!(
                renderer.post_process_status().status,
                PostProcessStatus::Active,
                "{name} status: {:?}",
                renderer.post_process_status()
            );
            let state = renderer.state.as_ref().unwrap();
            let image = renderer
                .surface
                .as_ref()
                .unwrap()
                .published_texture
                .as_ref()
                .unwrap();
            let pixels = read_bgra_pixels(state, image);
            let px = |x: u32, y: u32| pixels[(y * 64 + x) as usize];

            // Letterbox bars outside [16, 48) remain black.
            assert_eq!(px(32, 4), [0, 0, 0, 255], "{name} top letterbox");
            assert_eq!(px(32, 60), [0, 0, 0, 255], "{name} bottom letterbox");
            // Left half inside video rect is red, right half is blue.
            let left = px(16, 32);
            let right = px(48, 32);
            assert!(
                left[2] > 100 && left[0] < 60,
                "{name} left half should be red: {left:?}"
            );
            assert!(
                right[0] > 100 && right[2] < 60,
                "{name} right half should be blue: {right:?}"
            );
        }
    }

    #[test]
    fn d3d11_output_stage_applies_post_process_to_danmaku_while_main_stage_skips_it() {
        use crate::danmaku::{
            DanmakuFrameStats, DanmakuGlyphAtlas, DanmakuGlyphInstance, DanmakuRenderPlan,
            DanmakuViewport,
        };

        let mut renderer = gpu_test_renderer(test_driver(), D3D_FEATURE_LEVEL_11_0);
        // 64x32 dark video letterboxed inside a 64x64 surface (video occupies y=16..48).
        renderer
            .attach_surface(PlatformSurface::FlutterTexture(FlutterTextureHandle::new(
                FlutterTextureKind::WindowsTextureRegistrar,
                1,
                64,
                64,
                1.0,
            )))
            .unwrap();
        renderer
            .upload_planar_frame(
                &PlanarFrame {
                    format: PlanarPixelFormat::Nv12,
                    width: 64,
                    height: 32,
                    luma: vec![32u8; 64 * 32],
                    chroma: vec![128u8; 32 * 16 * 2],
                },
                SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Bt1886),
            )
            .unwrap();

        let solid = vec![255u8; 8 * 8];
        let plan = DanmakuRenderPlan {
            media_time: Duration::ZERO,
            generation: 1,
            viewport: DanmakuViewport::new(64, 64),
            atlas: Some(Arc::new(DanmakuGlyphAtlas {
                width: 8,
                height: 8,
                stride: 8,
                fill_alpha: solid,
                outline_alpha: vec![0u8; 8 * 8],
                version: 1,
                update: None,
            })),
            items: vec![
                // Solid white danmaku block inside the top letterbox bar.
                DanmakuGlyphInstance {
                    item_id: 1,
                    rect: [28.0, 4.0, 8.0, 8.0],
                    tex_rect: [0.0, 0.0, 1.0, 1.0],
                    color_rgba: [1.0, 1.0, 1.0, 1.0],
                    outline_rgba: [0.0, 0.0, 0.0, 0.0],
                    shadow_rgba: [0.0, 0.0, 0.0, 0.0],
                    shadow_offset: [0.0, 0.0],
                },
                // Solid white danmaku block centered over the video.
                DanmakuGlyphInstance {
                    item_id: 2,
                    rect: [28.0, 28.0, 8.0, 8.0],
                    tex_rect: [0.0, 0.0, 1.0, 1.0],
                    color_rgba: [1.0, 1.0, 1.0, 1.0],
                    outline_rgba: [0.0, 0.0, 0.0, 0.0],
                    shadow_rgba: [0.0, 0.0, 0.0, 0.0],
                    shadow_offset: [0.0, 0.0],
                },
            ],
            color_atlas: None,
            color_glyphs: Vec::new(),
            frame_stats: DanmakuFrameStats::default(),
        };

        let render_with = |renderer: &mut D3d11Renderer, name: &str, src: &str, generation: u64| {
            let shader = Arc::new(PostProcessShader::parse_str(name, src).unwrap());
            renderer.set_post_process_shader(Some(shader));
            assert!(
                renderer
                    .render_current_frame(
                        RenderFrameContext::new(Duration::ZERO, generation).danmaku(Some(&plan)),
                    )
                    .unwrap()
            );
            let state = renderer.state.as_ref().unwrap();
            let image = renderer
                .surface
                .as_ref()
                .unwrap()
                .published_texture
                .as_ref()
                .unwrap();
            read_bgra_pixels(state, image)
        };
        let px = |pixels: &[[u8; 4]], x: u32, y: u32| pixels[(y * 64 + x) as usize];

        let main_red = r#"//!DESC Main Red
//!HOOK MAIN
//!BIND HOOKED
vec4 hook() {
    vec4 c = HOOKED_tex(HOOKED_pos);
    return vec4(c.r, 0.0, 0.0, c.a);
}
"#;
        let output_red = main_red.replace("//!HOOK MAIN", "//!HOOK OUTPUT");

        // `//!HOOK MAIN` keeps danmaku white (unfiltered) over both video and letterbox.
        let main_pixels = render_with(&mut renderer, "main_red", main_red, 1);
        for (label, x, y) in [("letterbox", 32, 8), ("video", 32, 32)] {
            let p = px(&main_pixels, x, y);
            assert!(
                p[0] > 220 && p[1] > 220 && p[2] > 220,
                "MAIN stage should leave {label} danmaku white: {p:?}"
            );
        }

        // `//!HOOK OUTPUT` applies the shader to danmaku in both video and letterbox regions.
        let output_pixels = render_with(&mut renderer, "output_red", &output_red, 2);
        for (label, x, y) in [("letterbox", 32, 8), ("video", 32, 32)] {
            let p = px(&output_pixels, x, y);
            assert!(
                p[2] > 220 && p[1] < 20 && p[0] < 20,
                "OUTPUT stage should tint {label} danmaku red: {p:?}"
            );
        }

        // Real CRT Lite with `//!HOOK OUTPUT` modulates scanlines across danmaku rows.
        let crt_lite_output = CRT_LITE.replace("//!HOOK MAIN", "//!HOOK OUTPUT");
        let crt_pixels = render_with(&mut renderer, "crt_lite_output", &crt_lite_output, 3);
        let even_row = px(&crt_pixels, 32, 30)[2];
        let odd_row = px(&crt_pixels, 32, 31)[2];
        assert!(
            even_row > odd_row + 15,
            "CRT OUTPUT should apply scanlines to danmaku: even={even_row} odd={odd_row}"
        );
    }
}
