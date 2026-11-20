use std::mem;

use ::windows::Win32::Foundation::{E_INVALIDARG, E_OUTOFMEMORY};
use ::windows::Win32::Graphics::Direct3D::{
    D3D_FEATURE_LEVEL_11_0, Fxc::D3DCOMPILE_OPTIMIZATION_LEVEL3,
};
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_CPU_ACCESS_WRITE,
    D3D11_FEATURE_D3D11_OPTIONS2, D3D11_FEATURE_DATA_D3D11_OPTIONS2,
    D3D11_FEATURE_DATA_FORMAT_SUPPORT2, D3D11_FEATURE_FORMAT_SUPPORT2,
    D3D11_FORMAT_SUPPORT_TYPED_UNORDERED_ACCESS_VIEW, D3D11_FORMAT_SUPPORT2_UAV_TYPED_LOAD,
    D3D11_FORMAT_SUPPORT2_UAV_TYPED_STORE, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEX2D_UAV, D3D11_TEXTURE2D_DESC, D3D11_UAV_DIMENSION_TEXTURE2D,
    D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, ID3D11ComputeShader, ID3D11Device, ID3D11Texture2D,
    ID3D11UnorderedAccessView,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16_UNORM,
    DXGI_FORMAT_R16G16_UNORM, DXGI_SAMPLE_DESC,
};
use ::windows::Win32::Graphics::Dxgi::{DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAS_STILL_DRAWING};

use super::{
    D3d11BorrowedLuma, D3d11DeviceState, D3d11OverlayTexture, create_cpu_texture,
    create_cpu_texture_with_view_from_ptr, d3d_error,
};
use crate::core::{PlayerError, Result};
use crate::ffmpeg::SoftwareP010Upload;
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

pub(super) struct ChromaStaging {
    extent: (u32, u32),
    textures: [Option<ID3D11Texture2D>; 2],
    next: usize,
    disabled: bool,
}

// Two UHD chroma planes require 15.82 MiB before driver allocation padding.
const MAX_STAGING_SLOT_BYTES: u32 = 8 * 1024 * 1024;

impl ChromaStaging {
    fn disable(&mut self) {
        self.textures = [None, None];
        self.disabled = true;
    }
}

fn enable_staging(state: &mut D3d11DeviceState, extent: (u32, u32)) -> bool {
    let plane_bytes = extent
        .0
        .checked_mul(extent.1)
        .and_then(|pixels| pixels.checked_mul(4));
    if plane_bytes.is_none_or(|bytes| bytes > MAX_STAGING_SLOT_BYTES) {
        state.p010_chroma_staging = None;
        return false;
    }
    if state
        .p010_chroma_staging
        .as_ref()
        .is_none_or(|staging| staging.extent != extent)
    {
        state.p010_chroma_staging = Some(ChromaStaging {
            extent,
            textures: [None, None],
            next: 0,
            disabled: false,
        });
    }
    !state
        .p010_chroma_staging
        .as_ref()
        .expect("staging initialized")
        .disabled
}

fn can_fallback(error: &::windows::core::Error) -> bool {
    matches!(
        error.code(),
        E_INVALIDARG | E_OUTOFMEMORY | DXGI_ERROR_UNSUPPORTED
    )
}

fn upload_mapped_chroma(
    state: &mut D3d11DeviceState,
    source: &SoftwareP010Upload<'_>,
    extent: (u32, u32),
) -> ::windows::core::Result<Option<D3d11OverlayTexture>> {
    let (width, height) = extent;
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R16G16_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let staging = state.p010_chroma_staging.as_mut().expect("staging enabled");
    let mut ready = None;
    for slot in [staging.next, 1 - staging.next] {
        let texture = &mut staging.textures[slot];
        if texture.is_none() {
            unsafe { state.device.CreateTexture2D(&desc, None, Some(texture)) }?;
        }
        let texture = texture.as_ref().ok_or_else(|| {
            ::windows::core::Error::new(E_INVALIDARG, "null P010 chroma staging texture")
        })?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // A copy from a previous frame can still use the staging texture.
        // Never wait here; try the other slot before using initial data.
        match unsafe {
            state.context.Map(
                texture,
                0,
                D3D11_MAP_WRITE,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        } {
            Ok(()) => {
                staging.next = 1 - slot;
                ready = Some((texture.clone(), mapped));
                break;
            }
            Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {}
            Err(error) => return Err(error),
        }
    }
    let Some((staging_texture, mapped)) = ready else {
        return Ok(None);
    };
    let pitch = mapped.RowPitch as usize;
    let row_bytes = width as usize * 4;
    let len = pitch
        .checked_mul(height as usize - 1)
        .and_then(|offset| offset.checked_add(row_bytes));
    let written = len
        .filter(|len| {
            row_bytes <= pitch && *len <= MAX_STAGING_SLOT_BYTES as usize && !mapped.pData.is_null()
        })
        .and_then(|len| {
            // Cover visible texels through the final row, without assuming
            // trailing padding after the last texel. The writer only stores.
            let output = unsafe { std::slice::from_raw_parts_mut(mapped.pData.cast(), len) };
            source.write_mapped_d3d11_chroma(output, pitch)
        });
    unsafe { state.context.Unmap(&staging_texture, 0) };
    written.ok_or_else(|| {
        ::windows::core::Error::new(E_INVALIDARG, "invalid P010 chroma staging layout")
    })?;

    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
    desc.CPUAccessFlags = 0;
    let mut texture = None;
    unsafe {
        state
            .device
            .CreateTexture2D(&desc, None, Some(&mut texture))
    }?;
    let texture = texture.ok_or_else(|| {
        ::windows::core::Error::new(E_INVALIDARG, "null P010 chroma sampling texture")
    })?;
    let mut view = None;
    unsafe {
        state
            .device
            .CreateShaderResourceView(&texture, None, Some(&mut view))
    }?;
    let view = view.ok_or_else(|| {
        ::windows::core::Error::new(E_INVALIDARG, "null P010 chroma shader-resource view")
    })?;
    unsafe { state.context.CopyResource(&texture, &staging_texture) };
    Ok(Some(D3d11OverlayTexture {
        _texture: texture,
        view,
    }))
}

pub(super) fn upload(
    state: &mut D3d11DeviceState,
    raw: D3d11BorrowedLuma<'_>,
    source: &SoftwareP010Upload<'_>,
    scratch: &mut Vec<u8>,
) -> Result<(D3d11OverlayTexture, D3d11OverlayTexture)> {
    let extent = (raw.width.div_ceil(2), raw.height.div_ceil(2));
    let stage = enable_staging(state, extent);
    let mut pack_chroma = || {
        source.write_d3d11_chroma(scratch).ok_or_else(|| {
            PlayerError::Renderer("d3d11: invalid software P010 chroma layout".into())
        })
    };

    if !stage {
        pack_chroma()?;
    }
    let (luma, uav) = prepare_luma(state, &raw)?;
    let dimensions = (raw.width, raw.height);
    if stage {
        // Queue independent luma work before the CPU interleaves chroma.
        convert_luma(state, &uav, dimensions);
        match upload_mapped_chroma(state, source, extent) {
            Ok(Some(chroma)) => return Ok((luma, chroma)),
            Ok(None) => {}
            Err(error) if can_fallback(&error) => {
                state
                    .p010_chroma_staging
                    .as_mut()
                    .expect("staging enabled")
                    .disable();
            }
            Err(error) => return Err(d3d_error("Upload P010 staging chroma", error)),
        }
        pack_chroma()?;
    }
    let (cw, ch) = extent;
    let pitch = cw
        .checked_mul(4)
        .ok_or_else(|| PlayerError::Renderer("d3d11: chroma row pitch overflow".into()))?;
    let chroma = create_cpu_texture(state, cw, ch, DXGI_FORMAT_R16G16_UNORM, scratch, pitch)?;
    if !stage {
        convert_luma(state, &uav, dimensions);
    }
    Ok((luma, chroma))
}

fn prepare_luma(
    state: &D3d11DeviceState,
    raw: &D3d11BorrowedLuma<'_>,
) -> Result<(D3d11OverlayTexture, ID3D11UnorderedAccessView)> {
    let (width, height) = (raw.width, raw.height);
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
    Ok((luma, uav))
}

fn convert_luma(
    state: &D3d11DeviceState,
    uav: &ID3D11UnorderedAccessView,
    (width, height): (u32, u32),
) {
    let shader = state
        .p010_upload_shader
        .as_ref()
        .and_then(Option::as_ref)
        .expect("P010 upload shader initialized");
    // The shader reads and writes only its own luma texel. The immediate
    // context orders the dispatch before consumers of the unbound texture.
    unsafe {
        state.context.CSSetShader(shader, None);
        state
            .context
            .CSSetUnorderedAccessViews(0, 1, Some([Some(uav.clone())].as_ptr()), None);
        state
            .context
            .Dispatch(width.div_ceil(16), height.div_ceil(8), 1);
        state
            .context
            .CSSetUnorderedAccessViews(0, 1, Some([None].as_ptr()), None);
        state.context.CSSetShader(None, None);
    }
}
