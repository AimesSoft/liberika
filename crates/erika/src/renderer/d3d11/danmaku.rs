//! Comment-level composition without an offscreen texture per comment.
//!
//! Each shared batch instance covers a disjoint strip of one comment. The pixel
//! shader composes all glyph effects locally, then applies comment opacity once.
//! Glyph records use a regular float texture rather than StructuredBuffer so the
//! production path remains available on feature-level 10.0 devices.

use super::*;
use crate::renderer::danmaku_batch::{DanmakuBatch, DanmakuBatchInstance};
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_VERTEX_BUFFER, D3D11_INPUT_PER_INSTANCE_DATA,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32G32_UINT, DXGI_FORMAT_R32G32B32A32_FLOAT,
};

const SHADER: &[u8] = include_bytes!("danmaku.hlsl");

#[repr(C)]
#[derive(Clone, Copy)]
struct DanmakuUniforms {
    viewport: [f32; 2],
    glyph_texture_width: u32,
    reserved: u32,
    ui_nits: [f32; 4],
}

pub(super) struct D3d11DanmakuPipeline {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    constants: ID3D11Buffer,
}

pub(super) struct D3d11DanmakuBatch {
    instances: ID3D11Buffer,
    instance_count: u32,
    glyph_data: D3d11OverlayTexture,
    fill: D3d11OverlayTexture,
    outline: D3d11OverlayTexture,
    uniforms: DanmakuUniforms,
}

#[derive(Default)]
pub(super) struct D3d11DanmakuDraws {
    pub(super) text: Option<D3d11DanmakuBatch>,
    pub(super) pictures: Vec<D3d11OverlayDraw>,
}

impl D3d11DanmakuDraws {
    pub(super) fn is_empty(&self) -> bool {
        self.text.is_none() && self.pictures.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.text
            .as_ref()
            .map_or(0, |text| text.instance_count as usize)
            + self.pictures.len()
    }
}

impl D3d11DanmakuBatch {
    pub(super) fn upload(
        state: &D3d11DeviceState,
        plan: &DanmakuRenderPlan,
        fill: D3d11OverlayTexture,
        outline: D3d11OverlayTexture,
        target: TargetColorState,
    ) -> Result<Option<Self>> {
        let batch = DanmakuBatch::new(&plan.items);
        if batch.instances.is_empty() {
            return Ok(None);
        }
        // FL10 supports 8192; FL11 supports 16384. No texture arrays, structured
        // buffers, shader model 5, or per-comment render targets are required.
        let max_dimension =
            if unsafe { state.device.GetFeatureLevel() }.0 >= D3D_FEATURE_LEVEL_11_0.0 {
                16384
            } else {
                8192
            };
        let (width, height, texels) = batch.glyph_texture(max_dimension).ok_or_else(|| {
            PlayerError::Renderer("d3d11: danmaku glyph texture exceeds device limits".into())
        })?;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                texels.as_ptr().cast::<u8>(),
                mem::size_of_val(texels.as_slice()),
            )
        };
        let glyph_data = create_cpu_texture(
            state,
            width,
            height,
            DXGI_FORMAT_R32G32B32A32_FLOAT,
            bytes,
            width * 16,
        )?;
        let byte_width = u32::try_from(mem::size_of_val(batch.instances.as_slice()))
            .map_err(|_| PlayerError::Renderer("d3d11: danmaku instance buffer overflow".into()))?;
        let instance_count = u32::try_from(batch.instances.len())
            .map_err(|_| PlayerError::Renderer("d3d11: too many danmaku strips".into()))?;
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: byte_width,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: batch.instances.as_ptr().cast(),
            ..Default::default()
        };
        let instances = create_buffer(
            &state.device,
            &desc,
            Some(&data),
            "ID3D11Device::CreateBuffer(danmaku instances)",
        )?;
        Ok(Some(Self {
            instances,
            instance_count,
            glyph_data,
            fill,
            outline,
            uniforms: DanmakuUniforms {
                viewport: [
                    plan.viewport.width.max(1) as f32,
                    plan.viewport.height.max(1) as f32,
                ],
                glyph_texture_width: width,
                reserved: 0,
                ui_nits: overlay_ui_nits(target),
            },
        }))
    }
}

impl D3d11DanmakuPipeline {
    pub(super) fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex_blob = compile_shader_source(SHADER, "danmaku_vs_main", "vs_4_0")?;
        let pixel_blob = compile_shader_source(SHADER, "danmaku_ps_main", "ps_4_0")?;
        let mut vertex_shader = None;
        let mut pixel_shader = None;
        unsafe {
            device
                .CreateVertexShader(blob_bytes(&vertex_blob), None, Some(&mut vertex_shader))
                .map_err(|error| d3d_error("ID3D11Device::CreateVertexShader(danmaku)", error))?;
            device
                .CreatePixelShader(blob_bytes(&pixel_blob), None, Some(&mut pixel_shader))
                .map_err(|error| d3d_error("ID3D11Device::CreatePixelShader(danmaku)", error))?;
        }
        let element = |name: &'static [u8], format, offset| D3D11_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(name.as_ptr()),
            SemanticIndex: 0,
            Format: format,
            InputSlot: 0,
            AlignedByteOffset: offset,
            InputSlotClass: D3D11_INPUT_PER_INSTANCE_DATA,
            InstanceDataStepRate: 1,
        };
        let elements = [
            element(b"RECT\0", DXGI_FORMAT_R32G32B32A32_FLOAT, 0),
            element(b"GLYPHS\0", DXGI_FORMAT_R32G32_UINT, 16),
            element(b"OPACITY\0", DXGI_FORMAT_R32_FLOAT, 24),
        ];
        let mut input_layout = None;
        unsafe {
            device
                .CreateInputLayout(&elements, blob_bytes(&vertex_blob), Some(&mut input_layout))
                .map_err(|error| d3d_error("ID3D11Device::CreateInputLayout(danmaku)", error))?;
        }
        let constants = create_buffer(
            device,
            &D3D11_BUFFER_DESC {
                ByteWidth: mem::size_of::<DanmakuUniforms>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            },
            None,
            "ID3D11Device::CreateBuffer(danmaku constants)",
        )?;
        Ok(Self {
            vertex_shader: vertex_shader
                .ok_or_else(|| PlayerError::Renderer("d3d11: null danmaku vertex shader".into()))?,
            pixel_shader: pixel_shader
                .ok_or_else(|| PlayerError::Renderer("d3d11: null danmaku pixel shader".into()))?,
            input_layout: input_layout
                .ok_or_else(|| PlayerError::Renderer("d3d11: null danmaku input layout".into()))?,
            constants,
        })
    }

    pub(super) fn draw(
        &self,
        state: &D3d11DeviceState,
        batch: &D3d11DanmakuBatch,
        render_target: &ID3D11RenderTargetView,
        target_rect: D3d11DrawRect,
    ) {
        let context = &state.context;
        let viewport = D3D11_VIEWPORT {
            TopLeftX: target_rect.x,
            TopLeftY: target_rect.y,
            Width: target_rect.width.max(1.0),
            Height: target_rect.height.max(1.0),
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let stride = mem::size_of::<DanmakuBatchInstance>() as u32;
        let offset = 0;
        unsafe {
            context.UpdateSubresource(
                &self.constants,
                0,
                None,
                &batch.uniforms as *const _ as *const c_void,
                0,
                0,
            );
            context.RSSetViewports(Some(&[viewport]));
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.IASetInputLayout(&self.input_layout);
            context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(batch.instances.clone())),
                Some(&stride),
                Some(&offset),
            );
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(&self.pixel_shader, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetSamplers(0, Some(&[Some(state.overlay_sampler.clone())]));
            context.PSSetShaderResources(
                0,
                Some(&[
                    Some(batch.fill.view.clone()),
                    Some(batch.outline.view.clone()),
                    Some(batch.glyph_data.view.clone()),
                ]),
            );
            context.OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            context.OMSetBlendState(&state.overlay_blend, None, u32::MAX);
            // D3D preserves primitive order across instances, so different
            // comments still use the plan's painter order at intersections.
            context.DrawInstanced(6, batch.instance_count, 0, 0);
            context.PSSetShaderResources(0, Some(&[None, None, None]));
            context.IASetVertexBuffers(0, 1, Some(&None), Some(&stride), Some(&offset));
            context.OMSetBlendState(None, None, u32::MAX);
        }
    }
}

#[cfg(test)]
#[path = "danmaku_tests.rs"]
mod tests;
