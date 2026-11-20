use std::mem;

use ::windows::Win32::Foundation::E_INVALIDARG;
use ::windows::Win32::Graphics::Direct3D::{
    D3D_FEATURE_LEVEL_11_0, Fxc::D3DCOMPILE_OPTIMIZATION_LEVEL3,
};
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_FEATURE_D3D11_OPTIONS2,
    D3D11_FEATURE_DATA_D3D11_OPTIONS2, D3D11_FEATURE_DATA_FORMAT_SUPPORT2,
    D3D11_FEATURE_FORMAT_SUPPORT2, D3D11_FORMAT_SUPPORT_TYPED_UNORDERED_ACCESS_VIEW,
    D3D11_FORMAT_SUPPORT2_UAV_TYPED_LOAD, D3D11_FORMAT_SUPPORT2_UAV_TYPED_STORE, D3D11_TEX2D_UAV,
    D3D11_UAV_DIMENSION_TEXTURE2D, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, ID3D11ComputeShader, ID3D11Device,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM,
};
use ::windows::Win32::Graphics::Dxgi::DXGI_ERROR_UNSUPPORTED;

use super::{
    D3d11BorrowedLuma, D3d11DeviceState, D3d11OverlayTexture, create_cpu_texture,
    create_cpu_texture_with_view_from_ptr, d3d_error,
};
use crate::core::{PlayerError, Result};
use crate::renderer::d3d11_sr::compile_with_flags;

const SHADER: &str = include_str!("p010_upload.hlsl");

fn supports_in_place_luma(device: &ID3D11Device) -> ::windows::core::Result<bool> {
    if unsafe { device.GetFeatureLevel() }.0 < D3D_FEATURE_LEVEL_11_0.0 {
        return Ok(false);
    }
    let support = unsafe { device.CheckFormatSupport(DXGI_FORMAT_R16_UINT) }?;
    if support & D3D11_FORMAT_SUPPORT_TYPED_UNORDERED_ACCESS_VIEW.0 as u32 == 0 {
        return Ok(false);
    }
    let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS2::default();
    unsafe {
        device.CheckFeatureSupport(
            D3D11_FEATURE_D3D11_OPTIONS2,
            (&mut options as *mut D3D11_FEATURE_DATA_D3D11_OPTIONS2).cast(),
            mem::size_of_val(&options) as u32,
        )?;
    }
    if !options.TypedUAVLoadAdditionalFormats.as_bool() {
        return Ok(false);
    }

    let mut support = D3D11_FEATURE_DATA_FORMAT_SUPPORT2 {
        InFormat: DXGI_FORMAT_R16_UINT,
        OutFormatSupport2: 0,
    };
    unsafe {
        device.CheckFeatureSupport(
            D3D11_FEATURE_FORMAT_SUPPORT2,
            (&mut support as *mut D3D11_FEATURE_DATA_FORMAT_SUPPORT2).cast(),
            mem::size_of_val(&support) as u32,
        )?;
    }
    let required =
        (D3D11_FORMAT_SUPPORT2_UAV_TYPED_LOAD.0 | D3D11_FORMAT_SUPPORT2_UAV_TYPED_STORE.0) as u32;
    Ok(support.OutFormatSupport2 & required == required)
}

pub(super) fn create_shader(device: &ID3D11Device) -> Result<Option<ID3D11ComputeShader>> {
    match supports_in_place_luma(device) {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(error) if error.code() == E_INVALIDARG || error.code() == DXGI_ERROR_UNSUPPORTED => {
            return Ok(None);
        }
        Err(error) => return Err(d3d_error("Query P010 luma UAV support", error)),
    }
    let bytecode = compile_with_flags(SHADER, D3DCOMPILE_OPTIMIZATION_LEVEL3).map_err(|error| {
        PlayerError::Renderer(format!("d3d11: software P010 luma shader: {error}"))
    })?;
    let mut shader = None;
    unsafe { device.CreateComputeShader(&bytecode, None, Some(&mut shader)) }
        .map_err(|error| d3d_error("CreateComputeShader(P010 in-place luma)", error))?;
    shader
        .map(Some)
        .ok_or_else(|| PlayerError::Renderer("d3d11: software P010 luma shader was null".into()))
}

pub(super) fn upload(
    state: &D3d11DeviceState,
    shader: &ID3D11ComputeShader,
    raw: D3d11BorrowedLuma<'_>,
    chroma: &[u8],
) -> Result<(D3d11OverlayTexture, D3d11OverlayTexture)> {
    let (width, height) = (raw.width, raw.height);
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let uv_pitch = cw
        .checked_mul(4)
        .ok_or_else(|| PlayerError::Renderer("d3d11: chroma row pitch overflow".into()))?;
    // SAFETY: the raw luma borrow covers the checked 16-bit initial data
    // until CreateTexture2D returns. Both views use the R16 format family.
    let luma = unsafe {
        create_cpu_texture_with_view_from_ptr(
            state,
            (width, height),
            (DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16_UNORM),
            (raw.data, raw.pitch),
            (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_UNORDERED_ACCESS.0) as u32,
        )
    }?;
    let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R16_UINT,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut uav = None;
    unsafe {
        state
            .device
            .CreateUnorderedAccessView(&luma._texture, Some(&desc), Some(&mut uav))
    }
    .map_err(|error| d3d_error("CreateUnorderedAccessView(P010 in-place luma)", error))?;
    let uav =
        uav.ok_or_else(|| PlayerError::Renderer("d3d11: software P010 luma UAV was null".into()))?;
    let chroma = create_cpu_texture(state, cw, ch, DXGI_FORMAT_R16G16_UNORM, chroma, uv_pitch)?;

    // The shader reads and writes only its own luma texel. The immediate
    // context orders the dispatch before consumers of the unbound texture.
    unsafe {
        state.context.CSSetShader(shader, None);
        state
            .context
            .CSSetUnorderedAccessViews(0, 1, Some([Some(uav)].as_ptr()), None);
        state
            .context
            .Dispatch(width.div_ceil(16), height.div_ceil(8), 1);
        state
            .context
            .CSSetUnorderedAccessViews(0, 1, Some([None].as_ptr()), None);
        state.context.CSSetShader(None, None);
    }
    Ok((luma, chroma))
}
