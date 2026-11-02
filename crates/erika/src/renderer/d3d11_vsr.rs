//! D3D11 video-processor super resolution (NVIDIA RTX Video / Intel VSR).
//!
//! This module drives the fixed-function `ID3D11VideoProcessor` of the decoder
//! device and switches the vendor scaler on through
//! `ID3D11VideoContext::VideoProcessorSetStreamExtension` (the same mechanism
//! mpv's `d3d11vpp scaling-mode=nvidia|intel`, Chromium and VLC use).
//!
//! # API shape
//!
//! ```ignore
//! let mut vsr = D3d11VideoSuperResolution::new(&device)?;   // Err => unsupported GPU
//! let out = vsr.process(&context, &decoded_nv12, array_index,
//!                       (visible_w, visible_h), (target_w, target_h), color)?;
//! // out.luma / out.chroma are R8 / R8G8 (NV12) or R16 / R16G16 (P010) SRVs
//! // of a `target`-sized texture in the same format family as the input.
//! ```
//!
//! `process` only records GPU work (one `VideoProcessorBlt`); it never waits
//! on the GPU. The output texture is reused by every call, so the caller must
//! finish sampling it in the same command stream before the next `process`
//! (D3D11 immediate-context ordering guarantees this for a single renderer).
//!
//! # How the vendors are enabled
//!
//! * NVIDIA: GUID `{D43CE1B3-1F4B-48AC-BAEE-C3C25375E6F7}`, payload
//!   `{ version = 1, method = 2, enable = 1 }` on stream 0, applied once right
//!   after the video processor is created (mpv does the same; the setting
//!   lives in the processor, it is not re-applied per frame).
//! * Intel: VPE GUID `{EDD1D4B9-8659-4CBC-A4D6-9831A2163AC3}` (taken from mpv;
//!   note the `4CBC`/`9831A2163AC3` tail). Payload is `{ u32 function; const
//!   void *param }`. Sequence after processor creation: output extension
//!   `fn 0x1` (version) = 3, output extension `fn 0x20` (mode) = 1 (preproc),
//!   stream-0 extension `fn 0x37` (scaling) = 2 (super resolution).
//!
//! Color spaces are set on both stream and output to the *same* YCbCr space
//! (`ID3D11VideoContext1` when available) so the blt performs no color
//! conversion; the existing YCbCr shader path can sample the result unchanged.
//! Auto processing is disabled, rects are explicit, frame format is
//! progressive. SDR only: PQ/HLG content is tagged with the matching
//! gamma-2.2 YCbCr space, which is just a label for a YCbCr to YCbCr blt.
//!
//! # Resize policy
//!
//! The enumerator, processor and output texture are rebuilt whenever the
//! input format, input texture size or (even-rounded) target size changes.
//! The caller is expected to quantize or debounce the target size while the
//! window is being dragged; rebuilding costs a few hundred microseconds plus
//! one texture allocation.

use std::ffi::c_void;
use std::mem::{ManuallyDrop, size_of};

use ::windows::Win32::Foundation::{RECT, TRUE};
use ::windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2D;
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_TYPE, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020, DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM,
    DXGI_SAMPLE_DESC,
};
use ::windows::Win32::Graphics::Dxgi::IDXGIDevice;
use ::windows::core::{GUID, Interface};

use crate::core::{PlayerError, Result};

const VENDOR_ID_NVIDIA: u32 = 0x10DE;
const VENDOR_ID_INTEL: u32 = 0x8086;

/// `{D43CE1B3-1F4B-48AC-BAEE-C3C25375E6F7}`
const NVIDIA_PPE_INTERFACE_GUID: GUID = GUID::from_u128(0xd43ce1b3_1f4b_48ac_baee_c3c25375e6f7);
/// `{EDD1D4B9-8659-4CBC-A4D6-9831A2163AC3}`
const INTEL_VPE_INTERFACE_GUID: GUID = GUID::from_u128(0xedd1d4b9_8659_4cbc_a4d6_9831a2163ac3);

const INTEL_VPE_FN_VERSION: u32 = 0x1;
const INTEL_VPE_VERSION: u32 = 0x3;
const INTEL_VPE_FN_SCALING: u32 = 0x37;
const INTEL_VPE_SCALING_VSR: u32 = 0x2;
const INTEL_VPE_FN_MODE: u32 = 0x20;
const INTEL_VPE_MODE_PREPROC: u32 = 0x1;

/// Largest input-view cache; decoder pools are normally far smaller.
const INPUT_VIEW_CACHE_LIMIT: usize = 48;

/// GPU vendor whose scaler is driven through the video processor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VsrVendor {
    Nvidia,
    Intel,
}

/// YCbCr matrix of the decoded stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VsrMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// Color description handed to the video processor; stream and output use the
/// same value so no conversion happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VsrColorSpace {
    pub matrix: VsrMatrix,
    pub full_range: bool,
}

impl VsrColorSpace {
    fn dxgi(self) -> DXGI_COLOR_SPACE_TYPE {
        match (self.matrix, self.full_range) {
            (VsrMatrix::Bt601, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
            (VsrMatrix::Bt601, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
            (VsrMatrix::Bt709, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            (VsrMatrix::Bt709, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
            (VsrMatrix::Bt2020, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
            (VsrMatrix::Bt2020, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
        }
    }

    /// Legacy `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` (no BT.2020; falls back to
    /// the 709 flag). Bit layout: Usage:1, RGB_Range:1, YCbCr_Matrix:1,
    /// YCbCr_xvYCC:1, Nominal_Range:2.
    fn legacy(self) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
        let matrix: u32 = if self.matrix == VsrMatrix::Bt601 {
            0
        } else {
            1
        };
        let nominal: u32 = if self.full_range { 2 } else { 1 };
        D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: (matrix << 2) | (nominal << 4),
        }
    }
}

/// Upscaled frame; planes are views of `texture`.
pub(crate) struct VsrOutput {
    #[cfg_attr(not(test), allow(dead_code))]
    pub texture: ID3D11Texture2D,
    pub luma: ID3D11ShaderResourceView,
    pub chroma: ID3D11ShaderResourceView,
    /// Size of the planes; presentation samples them with normalized
    /// coordinates.
    #[cfg_attr(not(test), allow(dead_code))]
    pub width: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub height: u32,
}

struct OutputResources {
    width: u32,
    height: u32,
    texture: ID3D11Texture2D,
    view: ID3D11VideoProcessorOutputView,
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
}

struct ProcessorKey {
    format: DXGI_FORMAT,
    input: (u32, u32),
    output: (u32, u32),
}

struct ProcessorState {
    key: ProcessorKey,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output: OutputResources,
    /// Stream/output state and vendor extensions still have to be recorded.
    needs_setup: bool,
    configured: Option<(VsrColorSpace, (u32, u32))>,
    input_views: Vec<(ID3D11Texture2D, u32, ID3D11VideoProcessorInputView)>,
}

pub(crate) struct D3d11VideoSuperResolution {
    device: ID3D11Device,
    video_device: ID3D11VideoDevice,
    vendor: VsrVendor,
    state: Option<ProcessorState>,
    #[cfg(test)]
    extension_enabled: bool,
}

fn renderer_error(what: &str, error: impl std::fmt::Display) -> PlayerError {
    PlayerError::Renderer(format!("d3d11 video super resolution {what}: {error}"))
}

fn even_up(value: u32) -> u32 {
    (value + 1) & !1
}

impl D3d11VideoSuperResolution {
    /// Detects the adapter vendor and video device support. The device must
    /// have been created with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`.
    pub(crate) fn new(device: &ID3D11Device) -> Result<Self> {
        let dxgi: IDXGIDevice = device
            .cast()
            .map_err(|error| renderer_error("adapter query failed", error))?;
        let adapter = unsafe { dxgi.GetAdapter() }
            .map_err(|error| renderer_error("adapter query failed", error))?;
        let desc = unsafe { adapter.GetDesc() }
            .map_err(|error| renderer_error("adapter description failed", error))?;
        let vendor = match desc.VendorId {
            VENDOR_ID_NVIDIA => VsrVendor::Nvidia,
            VENDOR_ID_INTEL => VsrVendor::Intel,
            other => {
                return Err(PlayerError::Renderer(format!(
                    "d3d11 video super resolution is unsupported on adapter vendor {other:#06x}"
                )));
            }
        };
        let video_device: ID3D11VideoDevice = device.cast().map_err(|error| {
            renderer_error(
                "requires ID3D11VideoDevice (create the device with VIDEO_SUPPORT)",
                error,
            )
        })?;
        Ok(Self {
            device: device.clone(),
            video_device,
            vendor,
            state: None,
            #[cfg(test)]
            extension_enabled: true,
        })
    }

    pub(crate) fn vendor(&self) -> VsrVendor {
        self.vendor
    }

    /// Upscales one decoded frame (see module docs). `source` is the visible
    /// size inside `texture`; `target` is rounded up to even dimensions and
    /// must be at least `source` in both directions.
    pub(crate) fn process(
        &mut self,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        array_index: u32,
        source: (u32, u32),
        target: (u32, u32),
        color: VsrColorSpace,
    ) -> Result<VsrOutput> {
        if source.0 == 0 || source.1 == 0 {
            return Err(renderer_error("source size", "must be non-zero"));
        }
        let target = (
            even_up(target.0.max(source.0)),
            even_up(target.1.max(source.1)),
        );
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 && desc.Format != DXGI_FORMAT_P010 {
            return Err(renderer_error(
                "input format",
                "only NV12 and P010 are supported",
            ));
        }
        if source.0 > desc.Width || source.1 > desc.Height || array_index >= desc.ArraySize {
            return Err(renderer_error(
                "input",
                "source rect or array index out of range",
            ));
        }
        let key = ProcessorKey {
            format: desc.Format,
            input: (desc.Width, desc.Height),
            output: target,
        };
        let rebuild = self.state.as_ref().is_none_or(|state| {
            state.key.format != key.format
                || state.key.input != key.input
                || state.key.output != key.output
        });
        if rebuild {
            self.state = None;
            self.state = Some(self.build(key)?);
        }
        let vendor = self.vendor;
        #[cfg(test)]
        let extension_enabled = self.extension_enabled;
        #[cfg(not(test))]
        let extension_enabled = true;
        let video_device = self.video_device.clone();
        let state = self.state.as_mut().expect("state built above");

        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|error| renderer_error("video context query failed", error))?;
        if state.needs_setup {
            // Auto processing off: stops drivers from adding their own filters.
            unsafe {
                video_context.VideoProcessorSetStreamAutoProcessingMode(&state.processor, 0, false);
                video_context.VideoProcessorSetStreamFrameFormat(
                    &state.processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                video_context.VideoProcessorSetStreamOutputRate(
                    &state.processor,
                    0,
                    D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL,
                    false,
                    None,
                );
                let dest = RECT {
                    left: 0,
                    top: 0,
                    right: target.0 as i32,
                    bottom: target.1 as i32,
                };
                video_context.VideoProcessorSetStreamDestRect(
                    &state.processor,
                    0,
                    true,
                    Some(&dest),
                );
                video_context.VideoProcessorSetOutputTargetRect(
                    &state.processor,
                    true,
                    Some(&dest),
                );
            }
            state.needs_setup = false;
            state.configured = None;
        }
        if state.configured != Some((color, source)) {
            let src = RECT {
                left: 0,
                top: 0,
                right: source.0 as i32,
                bottom: source.1 as i32,
            };
            unsafe {
                video_context.VideoProcessorSetStreamSourceRect(
                    &state.processor,
                    0,
                    true,
                    Some(&src),
                );
            }
            set_color_spaces(&video_context, &state.processor, color);
            state.configured = Some((color, source));
        }

        // Chromium re-applies the extension before every blt; do the same.
        if extension_enabled {
            enable_vendor_extension(&video_context, &state.processor, vendor)?;
        }
        let input_view = state.input_view(&video_device, texture, array_index)?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let result = unsafe {
            video_context.VideoProcessorBlt(
                &state.processor,
                &state.output.view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // The stream holds one reference to the input view; release it.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.map_err(|error| renderer_error("VideoProcessorBlt failed", error))?;
        Ok(VsrOutput {
            texture: state.output.texture.clone(),
            luma: state.output.luma.clone(),
            chroma: state.output.chroma.clone(),
            width: state.output.width,
            height: state.output.height,
        })
    }

    fn build(&self, key: ProcessorKey) -> Result<ProcessorState> {
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputWidth: key.input.0,
            InputHeight: key.input.1,
            OutputWidth: key.output.0,
            OutputHeight: key.output.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            ..Default::default()
        };
        let enumerator = unsafe { self.video_device.CreateVideoProcessorEnumerator(&content) }
            .map_err(|error| renderer_error("enumerator creation failed", error))?;
        let support = |format: DXGI_FORMAT| unsafe {
            enumerator
                .CheckVideoProcessorFormat(format)
                .map_err(|error| renderer_error("format query failed", error))
        };
        let format_support = support(key.format)?;
        if format_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
            || format_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
        {
            return Err(renderer_error(
                "format",
                format!(
                    "video processor does not support {:?} in and out",
                    key.format
                ),
            ));
        }
        let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|error| renderer_error("processor creation failed", error))?;
        let output = self.build_output(&enumerator, key.format, key.output)?;
        Ok(ProcessorState {
            key,
            enumerator,
            processor,
            output,
            needs_setup: true,
            configured: None,
            input_views: Vec::new(),
        })
    }

    fn build_output(
        &self,
        enumerator: &ID3D11VideoProcessorEnumerator,
        format: DXGI_FORMAT,
        size: (u32, u32),
    ) -> Result<OutputResources> {
        let (luma_format, chroma_format) = if format == DXGI_FORMAT_P010 {
            (DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM)
        } else {
            (DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM)
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|error| renderer_error("output texture creation failed", error))?;
        let texture = texture.ok_or_else(|| renderer_error("output texture", "missing"))?;
        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view = None;
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                &texture,
                enumerator,
                &view_desc,
                Some(&mut view),
            )
        }
        .map_err(|error| renderer_error("output view creation failed", error))?;
        let view = view.ok_or_else(|| renderer_error("output view", "missing"))?;
        let plane_view = |plane_format: DXGI_FORMAT| -> Result<ID3D11ShaderResourceView> {
            let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: plane_format,
                ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                    },
                },
            };
            let mut srv = None;
            unsafe {
                self.device
                    .CreateShaderResourceView(&texture, Some(&srv_desc), Some(&mut srv))
            }
            .map_err(|error| renderer_error("plane view creation failed", error))?;
            srv.ok_or_else(|| renderer_error("plane view", "missing"))
        };
        Ok(OutputResources {
            width: size.0,
            height: size.1,
            luma: plane_view(luma_format)?,
            chroma: plane_view(chroma_format)?,
            texture,
            view,
        })
    }
}

impl ProcessorState {
    /// Input views are tied to the enumerator and are cached per decoder
    /// surface (the cache keeps the texture alive so the pointer key is
    /// never reused for another resource).
    fn input_view(
        &mut self,
        video_device: &ID3D11VideoDevice,
        texture: &ID3D11Texture2D,
        array_index: u32,
    ) -> Result<ID3D11VideoProcessorInputView> {
        if let Some((_, _, view)) = self
            .input_views
            .iter()
            .find(|(cached, index, _)| cached.as_raw() == texture.as_raw() && *index == array_index)
        {
            return Ok(view.clone());
        }
        let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: array_index,
                },
            },
        };
        let mut view = None;
        unsafe {
            video_device.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &view_desc,
                Some(&mut view),
            )
        }
        .map_err(|error| renderer_error("input view creation failed", error))?;
        let view = view.ok_or_else(|| renderer_error("input view", "missing"))?;
        if self.input_views.len() >= INPUT_VIEW_CACHE_LIMIT {
            self.input_views.remove(0);
        }
        self.input_views
            .push((texture.clone(), array_index, view.clone()));
        Ok(view)
    }
}

fn set_color_spaces(
    video_context: &ID3D11VideoContext,
    processor: &ID3D11VideoProcessor,
    color: VsrColorSpace,
) {
    if let Ok(context1) = video_context.cast::<ID3D11VideoContext1>() {
        unsafe {
            context1.VideoProcessorSetStreamColorSpace1(processor, 0, color.dxgi());
            context1.VideoProcessorSetOutputColorSpace1(processor, color.dxgi());
        }
    } else {
        let legacy = color.legacy();
        unsafe {
            video_context.VideoProcessorSetStreamColorSpace(processor, 0, &legacy);
            video_context.VideoProcessorSetOutputColorSpace(processor, &legacy);
        }
    }
}

/// The extension entry points return a raw `HRESULT` value.
trait RawHresult {
    fn pipe_hr(self) -> ::windows::core::Result<()>;
}

impl RawHresult for i32 {
    fn pipe_hr(self) -> ::windows::core::Result<()> {
        ::windows::core::HRESULT(self).ok()
    }
}

#[repr(C)]
struct NvidiaExtension {
    version: u32,
    method: u32,
    enable: u32,
}

#[repr(C)]
struct IntelExtension {
    function: u32,
    param: *const c_void,
}

fn enable_vendor_extension(
    video_context: &ID3D11VideoContext,
    processor: &ID3D11VideoProcessor,
    vendor: VsrVendor,
) -> Result<()> {
    match vendor {
        VsrVendor::Nvidia => {
            let mut ext = NvidiaExtension {
                version: 1,
                method: 2,
                enable: 1,
            };
            unsafe {
                video_context.VideoProcessorSetStreamExtension(
                    processor,
                    0,
                    &NVIDIA_PPE_INTERFACE_GUID,
                    size_of::<NvidiaExtension>() as u32,
                    &mut ext as *mut _ as *mut c_void,
                )
            }
            .pipe_hr()
            .map_err(|error| renderer_error("NVIDIA RTX extension rejected", error))
        }
        VsrVendor::Intel => {
            let version = INTEL_VPE_VERSION;
            let mode = INTEL_VPE_MODE_PREPROC;
            let scaling = INTEL_VPE_SCALING_VSR;
            let size = size_of::<IntelExtension>() as u32;
            let mut ext = IntelExtension {
                function: INTEL_VPE_FN_VERSION,
                param: &version as *const u32 as *const c_void,
            };
            unsafe {
                video_context.VideoProcessorSetOutputExtension(
                    processor,
                    &INTEL_VPE_INTERFACE_GUID,
                    size,
                    &mut ext as *mut _ as *mut c_void,
                )
            }
            .pipe_hr()
            .map_err(|error| renderer_error("Intel VPE version rejected", error))?;
            ext = IntelExtension {
                function: INTEL_VPE_FN_MODE,
                param: &mode as *const u32 as *const c_void,
            };
            unsafe {
                video_context.VideoProcessorSetOutputExtension(
                    processor,
                    &INTEL_VPE_INTERFACE_GUID,
                    size,
                    &mut ext as *mut _ as *mut c_void,
                )
            }
            .pipe_hr()
            .map_err(|error| renderer_error("Intel VPE mode rejected", error))?;
            ext = IntelExtension {
                function: INTEL_VPE_FN_SCALING,
                param: &scaling as *const u32 as *const c_void,
            };
            unsafe {
                video_context.VideoProcessorSetStreamExtension(
                    processor,
                    0,
                    &INTEL_VPE_INTERFACE_GUID,
                    size,
                    &mut ext as *mut _ as *mut c_void,
                )
            }
            .pipe_hr()
            .map_err(|error| renderer_error("Intel VSR scaling rejected", error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::windows::Win32::Foundation::HMODULE;
    use ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use ::windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC,
        D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_SDK_VERSION,
        D3D11_SUBRESOURCE_DATA, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Query,
    };

    const BT709: VsrColorSpace = VsrColorSpace {
        matrix: VsrMatrix::Bt709,
        full_range: false,
    };

    fn create_device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .ok()?;
        Some((device?, context?))
    }

    /// 16-bit-expanded luma plane (`wide`) with sharp edges, thin strokes and
    /// diagonals, followed by a mid-grey chroma plane. Returns raw bytes.
    fn pattern(width: u32, height: u32, wide: bool) -> Vec<u8> {
        let bytes = if wide { 2 } else { 1 };
        let mut data = Vec::new();
        let put = |data: &mut Vec<u8>, value: u8| {
            if wide {
                data.extend_from_slice(&(u16::from(value) << 8).to_le_bytes());
            } else {
                data.push(value);
            }
        };
        for y in 0..height {
            for x in 0..width {
                let mut v = 40u8;
                // Checker blocks, thin vertical/horizontal strokes, diagonal.
                if ((x / 32) + (y / 32)) % 2 == 0 {
                    v = 200;
                }
                if x % 12 == 3 && (y / 40) % 2 == 0 {
                    v = 235;
                }
                if y % 10 == 4 && (x / 50) % 2 == 1 {
                    v = 16;
                }
                if (x + y) % 23 < 2 {
                    v = 235;
                }
                put(&mut data, v);
            }
        }
        for _ in 0..(height / 2) * (width / 2) {
            put(&mut data, 128);
            put(&mut data, 128);
        }
        assert_eq!(data.len() as u32, width * (height + height / 2) * bytes);
        data
    }

    fn make_input(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> ID3D11Texture2D {
        let wide = format == DXGI_FORMAT_P010;
        let data = pattern(width, height, wide);
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
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr() as *const c_void,
            SysMemPitch: width * if wide { 2 } else { 1 },
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }
            .expect("create input texture");
        texture.unwrap()
    }

    /// Reads back the luma plane as 8-bit values.
    fn read_luma(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
    ) -> (Vec<u8>, u32, u32) {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging = None;
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
            .expect("create staging");
        let staging = staging.unwrap();
        unsafe { context.CopyResource(&staging, texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .expect("map staging");
        let wide = desc.Format == DXGI_FORMAT_P010;
        let mut out = Vec::with_capacity((desc.Width * desc.Height) as usize);
        for y in 0..desc.Height {
            let row = unsafe {
                std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add((y * mapped.RowPitch) as usize),
                    (desc.Width * if wide { 2 } else { 1 }) as usize,
                )
            };
            if wide {
                out.extend(row.chunks_exact(2).map(|c| c[1]));
            } else {
                out.extend_from_slice(row);
            }
        }
        unsafe { context.Unmap(&staging, 0) };
        (out, desc.Width, desc.Height)
    }

    /// Mean absolute horizontal+vertical gradient (edge energy).
    fn gradient_energy(luma: &[u8], w: u32, h: u32) -> f64 {
        let (w, h) = (w as usize, h as usize);
        let mut sum = 0u64;
        for y in 0..h - 1 {
            for x in 0..w - 1 {
                let p = luma[y * w + x] as i32;
                sum += (p - luma[y * w + x + 1] as i32).unsigned_abs() as u64;
                sum += (p - luma[(y + 1) * w + x] as i32).unsigned_abs() as u64;
            }
        }
        sum as f64 / (w * h) as f64
    }

    fn run_case(format: DXGI_FORMAT, src: (u32, u32), dst: (u32, u32)) {
        let Some((device, context)) = create_device() else {
            eprintln!("skipping VSR test: no hardware D3D11 device");
            return;
        };
        let mut vsr = match D3d11VideoSuperResolution::new(&device) {
            Ok(vsr) => vsr,
            Err(error) => {
                eprintln!("skipping VSR test: {error}");
                return;
            }
        };
        eprintln!("vendor {:?}", vsr.vendor());
        let input = make_input(&device, src.0, src.1, format);

        let out = vsr
            .process(&context, &input, 0, src, dst, BT709)
            .expect("process with extension");
        assert_eq!((out.width, out.height), dst);
        let (vsr_luma, w, h) = read_luma(&device, &context, &out.texture);

        vsr.extension_enabled = false;
        vsr.state = None;
        let out = vsr
            .process(&context, &input, 0, src, dst, BT709)
            .expect("process without extension");
        let (base_luma, _, _) = read_luma(&device, &context, &out.texture);

        let mad = vsr_luma
            .iter()
            .zip(&base_luma)
            .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as f64)
            .sum::<f64>()
            / vsr_luma.len() as f64;
        let ge_vsr = gradient_energy(&vsr_luma, w, h);
        let ge_base = gradient_energy(&base_luma, w, h);
        eprintln!(
            "{format:?} {src:?}->{dst:?}: mean abs diff vs fixed-function scaler {mad:.3}, \
             gradient energy VSR {ge_vsr:.3} vs baseline {ge_base:.3} (ratio {:.3})",
            ge_vsr / ge_base
        );
        if mad == 0.0 {
            // The driver acknowledges the extension with S_OK even for unknown
            // GUIDs, so "engaged" can only be judged from the pixels. The
            // NVIDIA App / Control Panel "Super Resolution" setting must be on.
            eprintln!(
                "NOTE: output identical to the fixed-function scaler; the vendor VSR did not \
                 engage (driver setting off, or unsupported content)"
            );
        }
    }

    #[test]
    fn d3d11_vsr_nv12_2x() {
        run_case(DXGI_FORMAT_NV12, (640, 360), (1280, 720));
    }

    #[test]
    fn d3d11_vsr_nv12_3x() {
        run_case(DXGI_FORMAT_NV12, (640, 360), (1920, 1080));
    }

    #[test]
    fn d3d11_vsr_p010_2x() {
        run_case(DXGI_FORMAT_P010, (640, 360), (1280, 720));
    }

    #[test]
    fn d3d11_vsr_size_changes_do_not_crash() {
        let Some((device, context)) = create_device() else {
            return;
        };
        let Ok(mut vsr) = D3d11VideoSuperResolution::new(&device) else {
            return;
        };
        let a = make_input(&device, 640, 360, DXGI_FORMAT_NV12);
        let b = make_input(&device, 320, 180, DXGI_FORMAT_NV12);
        for (input, src, dst) in [
            (&a, (640, 360), (1280, 720)),
            (&a, (638, 358), (1277, 717)),
            (&a, (640, 360), (1280, 720)),
            (&b, (320, 180), (960, 540)),
            (&a, (640, 360), (2560, 1440)),
            (&a, (640, 360), (700, 400)),
        ] {
            let out = vsr
                .process(&context, input, 0, src, dst, BT709)
                .unwrap_or_else(|e| panic!("{src:?}->{dst:?}: {e}"));
            assert_eq!(out.width, even_up(dst.0));
            assert_eq!(out.height, even_up(dst.1));
        }
        // Frame-to-frame reuse keeps the output texture.
        let first = vsr
            .process(&context, &a, 0, (640, 360), (1280, 720), BT709)
            .unwrap();
        let second = vsr
            .process(&context, &a, 0, (640, 360), (1280, 720), BT709)
            .unwrap();
        assert_eq!(first.texture.as_raw(), second.texture.as_raw());
        unsafe { context.Flush() };
    }

    fn gpu_ms(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        vsr: &mut D3d11VideoSuperResolution,
        input: &ID3D11Texture2D,
        src: (u32, u32),
        dst: (u32, u32),
    ) -> f64 {
        let query = |kind| {
            let desc = D3D11_QUERY_DESC {
                Query: kind,
                MiscFlags: 0,
            };
            let mut q: Option<ID3D11Query> = None;
            unsafe { device.CreateQuery(&desc, Some(&mut q)) }.unwrap();
            q.unwrap()
        };
        let disjoint = query(D3D11_QUERY_TIMESTAMP_DISJOINT);
        let begin = query(D3D11_QUERY_TIMESTAMP);
        let end = query(D3D11_QUERY_TIMESTAMP);
        const FRAMES: usize = 30;
        // Warm up (driver loads the model on first use).
        for _ in 0..10 {
            vsr.process(context, input, 0, src, dst, BT709).unwrap();
        }
        unsafe { context.Flush() };
        unsafe {
            context.Begin(&disjoint);
            context.End(&begin);
        }
        for _ in 0..FRAMES {
            vsr.process(context, input, 0, src, dst, BT709).unwrap();
        }
        unsafe {
            context.End(&end);
            context.End(&disjoint);
            context.Flush();
        }
        let poll = |q: &ID3D11Query, size: usize, out: *mut c_void| loop {
            // windows-rs maps S_FALSE to Ok, so the data must be checked.
            let hr = unsafe { context.GetData(q, Some(out), size as u32, 0) };
            if hr.is_ok() && unsafe { *(out as *const u64) } != 0 {
                break;
            }
            std::thread::yield_now();
        };
        let mut dj = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
        // Frequency is the first field; zero until the query resolves.
        poll(
            &disjoint,
            size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>(),
            &mut dj as *mut _ as *mut c_void,
        );
        let (mut t0, mut t1) = (0u64, 0u64);
        poll(&begin, 8, &mut t0 as *mut _ as *mut c_void);
        poll(&end, 8, &mut t1 as *mut _ as *mut c_void);
        assert!(!dj.Disjoint.as_bool(), "timestamp query disjoint");
        eprintln!("  ticks {t0} -> {t1}, freq {}", dj.Frequency);
        (t1.wrapping_sub(t0)) as f64 / dj.Frequency as f64 * 1000.0 / FRAMES as f64
    }

    /// Wall-clock cost per blt with the GPU drained after every batch; covers
    /// work on the video engine that 3D-queue timestamps may not bracket.
    fn wall_ms(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        vsr: &mut D3d11VideoSuperResolution,
        input: &ID3D11Texture2D,
        src: (u32, u32),
        dst: (u32, u32),
    ) -> f64 {
        use ::windows::Win32::Graphics::Direct3D11::D3D11_QUERY_EVENT;
        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut q: Option<ID3D11Query> = None;
        unsafe { device.CreateQuery(&desc, Some(&mut q)) }.unwrap();
        let q = q.unwrap();
        let drain = || {
            unsafe { context.End(&q) };
            unsafe { context.Flush() };
            let mut done = 0u32;
            while done == 0 {
                let _ =
                    unsafe { context.GetData(&q, Some(&mut done as *mut _ as *mut c_void), 4, 0) };
            }
        };
        for _ in 0..10 {
            vsr.process(context, input, 0, src, dst, BT709).unwrap();
        }
        drain();
        const FRAMES: usize = 60;
        let start = std::time::Instant::now();
        for _ in 0..FRAMES {
            vsr.process(context, input, 0, src, dst, BT709).unwrap();
        }
        drain();
        start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64
    }

    #[test]
    #[ignore]
    fn d3d11_vsr_gpu_benchmark() {
        let Some((device, context)) = create_device() else {
            return;
        };
        let Ok(mut vsr) = D3d11VideoSuperResolution::new(&device) else {
            return;
        };
        for (src, dst) in [((1280, 720), (2560, 1440)), ((1920, 1080), (3840, 2160))] {
            let input = make_input(&device, src.0, src.1, DXGI_FORMAT_NV12);
            let on = gpu_ms(&device, &context, &mut vsr, &input, src, dst);
            let on_wall = wall_ms(&device, &context, &mut vsr, &input, src, dst);
            vsr.extension_enabled = false;
            vsr.state = None;
            let off = gpu_ms(&device, &context, &mut vsr, &input, src, dst);
            let off_wall = wall_ms(&device, &context, &mut vsr, &input, src, dst);
            vsr.extension_enabled = true;
            vsr.state = None;
            eprintln!(
                "{:?} NV12 {src:?}->{dst:?}: extension on {on:.3} ms gpu-timestamp / {on_wall:.3} ms wall; \
                 fixed-function {off:.3} ms gpu-timestamp / {off_wall:.3} ms wall",
                vsr.vendor()
            );
        }
    }
}
