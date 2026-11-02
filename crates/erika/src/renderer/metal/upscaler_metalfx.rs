//! MetalFX spatial scaling (`MTLFXSpatialScaler`) of the luma plane.
//!
//! MetalFX exists from macOS 13 / iOS 16 / tvOS 16 while Erika deploys to
//! older systems, so the framework is loaded at runtime and driven through
//! the Objective-C runtime instead of being linked.
//!
//! The scaler accepts single-channel color textures (`R8Unorm`,
//! `R16Unorm`, `R16Float` were verified on macOS 26), so the decoded luma
//! plane is fed directly and the output keeps its pixel format; chroma is
//! reconstructed by the regular presentation sampler. Luma is gamma encoded,
//! hence the perceptual color processing mode.

use std::ffi::CStr;
use std::sync::OnceLock;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject};
use objc2_metal::{
    MTLCommandBuffer, MTLDevice, MTLPixelFormat, MTLResource, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};

use crate::core::{PlayerError, Result};

const METALFX_PATH: &CStr = c"/System/Library/Frameworks/MetalFX.framework/MetalFX";
/// `MTLFXSpatialScalerColorProcessingModePerceptual`.
const COLOR_PROCESSING_PERCEPTUAL: isize = 0;

fn descriptor_class() -> std::result::Result<&'static AnyClass, String> {
    static LOADED: OnceLock<bool> = OnceLock::new();
    let loaded = *LOADED.get_or_init(|| {
        // SAFETY: dlopen of a system framework path; the handle is kept for
        // the process lifetime.
        !unsafe { libc::dlopen(METALFX_PATH.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) }
            .is_null()
    });
    if !loaded {
        return Err("MetalFX.framework is not available (requires macOS 13 / iOS 16)".to_string());
    }
    AnyClass::get(c"MTLFXSpatialScalerDescriptor")
        .ok_or_else(|| "MTLFXSpatialScalerDescriptor class not found".to_string())
}

/// Why MetalFX spatial scaling cannot run on `device`, if it cannot.
pub fn unavailable_reason(device: &ProtocolObject<dyn MTLDevice>) -> Option<String> {
    let class = match descriptor_class() {
        Ok(class) => class,
        Err(reason) => return Some(reason),
    };
    // SAFETY: `+supportsDevice:` takes an `id<MTLDevice>` and returns BOOL.
    let supported: Bool = unsafe { msg_send![class, supportsDevice: device] };
    (!supported.as_bool())
        .then(|| "MTLFXSpatialScalerDescriptor reports the GPU as unsupported".to_string())
}

struct Configuration {
    input: (usize, usize),
    output: (usize, usize),
    format: MTLPixelFormat,
}

pub struct MetalFxSpatial {
    scaler: Option<(Configuration, Retained<AnyObject>)>,
    output: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
}

// SAFETY: created and used on the render thread only; `Send` lets the
// upscaler front-end own it next to the other backends.
unsafe impl Send for MetalFxSpatial {}

impl MetalFxSpatial {
    pub fn new(device: &ProtocolObject<dyn MTLDevice>) -> std::result::Result<Self, String> {
        if let Some(reason) = unavailable_reason(device) {
            return Err(reason);
        }
        Ok(Self {
            scaler: None,
            output: None,
        })
    }

    pub fn allocated_bytes(&self) -> u64 {
        self.output
            .as_ref()
            .map_or(0, |texture| texture.allocatedSize() as u64)
    }

    fn ensure_scaler(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        input: (usize, usize),
        output: (usize, usize),
        format: MTLPixelFormat,
    ) -> Result<()> {
        if let Some((config, _)) = self.scaler.as_ref()
            && config.input == input
            && config.output == output
            && config.format == format
        {
            return Ok(());
        }
        self.scaler = None;
        self.output = None;
        let class = descriptor_class().map_err(PlayerError::Renderer)?;
        // SAFETY: plain property setters of MTLFXSpatialScalerDescriptor with
        // their declared argument types (NSUInteger, MTLPixelFormat,
        // NSInteger enum).
        let scaler: Option<Retained<AnyObject>> = unsafe {
            let descriptor: Retained<AnyObject> = msg_send![class, new];
            let _: () = msg_send![&descriptor, setInputWidth: input.0];
            let _: () = msg_send![&descriptor, setInputHeight: input.1];
            let _: () = msg_send![&descriptor, setOutputWidth: output.0];
            let _: () = msg_send![&descriptor, setOutputHeight: output.1];
            let _: () = msg_send![&descriptor, setColorTextureFormat: format];
            let _: () = msg_send![&descriptor, setOutputTextureFormat: format];
            let _: () = msg_send![&descriptor, setColorProcessingMode: COLOR_PROCESSING_PERCEPTUAL];
            msg_send![&descriptor, newSpatialScalerWithDevice: device]
        };
        let scaler = scaler.ok_or_else(|| {
            PlayerError::Renderer(format!(
                "MetalFX rejected a {}x{} -> {}x{} spatial scaler for {format:?}",
                input.0, input.1, output.0, output.1
            ))
        })?;
        // SAFETY: `outputTextureUsage` is an MTLTextureUsage property.
        let usage: MTLTextureUsage = unsafe { msg_send![&scaler, outputTextureUsage] };
        let descriptor = unsafe {
            let descriptor =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    format, output.0, output.1, false,
                );
            descriptor.setStorageMode(MTLStorageMode::Private);
            descriptor.setUsage(usage | MTLTextureUsage::ShaderRead);
            descriptor
        };
        self.output = Some(
            device
                .newTextureWithDescriptor(&descriptor)
                .ok_or_else(|| {
                    PlayerError::Renderer("MetalFX output texture alloc failed".to_string())
                })?,
        );
        self.scaler = Some((
            Configuration {
                input,
                output,
                format,
            },
            scaler,
        ));
        Ok(())
    }

    /// Encodes the spatial upscale of `luma` to `width x height`.
    pub fn encode(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        luma: &ProtocolObject<dyn MTLTexture>,
        width: usize,
        height: usize,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        let input = (luma.width(), luma.height());
        self.ensure_scaler(device, input, (width, height), luma.pixelFormat())?;
        let (_, scaler) = self.scaler.as_ref().expect("scaler built above");
        let output = self.output.clone().expect("output built above");
        // SAFETY: MTLFXSpatialScaler setters take `id<MTLTexture>` /
        // NSUInteger; `encodeToCommandBuffer:` takes `id<MTLCommandBuffer>`.
        unsafe {
            let _: () = msg_send![scaler, setColorTexture: luma];
            let _: () = msg_send![scaler, setOutputTexture: &*output];
            let _: () = msg_send![scaler, setInputContentWidth: input.0];
            let _: () = msg_send![scaler, setInputContentHeight: input.1];
            let _: () = msg_send![scaler, encodeToCommandBuffer: command_buffer];
        }
        Ok(output)
    }
}
