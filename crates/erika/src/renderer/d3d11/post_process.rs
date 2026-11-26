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
    PostProcessRuntimeInfo, PostProcessShader, PostProcessStatus, PostProcessUniforms,
};

const POST_VS_SOURCE: &[u8] = br#"
struct PostVsOut {
    float4 position : SV_Position;
    float2 uv : LOC0;
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
