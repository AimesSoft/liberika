//! VideoToolbox super resolution (`VTLowLatencySuperResolutionScaler`).
//!
//! Availability (probed on macOS 26.7 / Apple M5 Pro with the macOS 26 SDK):
//!
//! - `VTLowLatencySuperResolutionScalerConfiguration` (macOS/iOS/tvOS 26):
//!   2x only, sources from 96x96 up to 960x960, 8-bit 4:2:0 video range
//!   (`'420v'`) in and out. This is what `PlatformVideo` uses.
//! - `VTSuperResolutionScalerConfiguration` (macOS/iOS 26): 4x only, RGBA
//!   half float, and its ML model must be downloaded on demand. Not used: a
//!   4x RGBA16F pass per frame is an offline workflow, not playback.
//!
//! Both are driven through the Objective-C runtime because Erika deploys to
//! systems that predate them. The scaler is encoded into the renderer's own
//! command buffer (`processWithCommandBuffer:parameters:`), so the render
//! thread never waits on it; the session itself (which loads an ML model and
//! can take longer than a frame) starts on a worker thread, and frames render
//! natively until it is ready. Every frame is copied with a GPU blit into a
//! pixel buffer from a pool created with the processor's source attributes
//! (the decoder's buffers need not have the required padding or IOSurface
//! backing), and the 2x output pool's planes are sampled directly as the
//! new luma and chroma.

use std::collections::VecDeque;
use std::ffi::CStr;
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2::encode::{Encode, Encoding};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject};
use objc2_core_foundation::{CFDictionary, CFRetained};
use objc2_core_video::{
    CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBuffer,
    CVPixelBufferPool, kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLDevice, MTLPixelFormat,
    MTLTexture,
};

use crate::core::{PlayerError, Result};

const VIDEO_TOOLBOX_PATH: &CStr = c"/System/Library/Frameworks/VideoToolbox.framework/VideoToolbox";
const SCALE: f32 = 2.0;
/// Frames whose pixel buffers stay referenced until the GPU is done.
const IN_FLIGHT: usize = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

// SAFETY: matches CoreMedia's `CMTime` layout and type encoding.
unsafe impl Encode for CMTime {
    const ENCODING: Encoding = Encoding::Struct(
        "?",
        &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING],
    );
}

fn class(name: &CStr) -> std::result::Result<&'static AnyClass, String> {
    static LOADED: OnceLock<bool> = OnceLock::new();
    let loaded = *LOADED.get_or_init(|| {
        // SAFETY: dlopen of a system framework path kept for the process
        // lifetime.
        !unsafe {
            libc::dlopen(
                VIDEO_TOOLBOX_PATH.as_ptr(),
                libc::RTLD_LAZY | libc::RTLD_LOCAL,
            )
        }
        .is_null()
    });
    if !loaded {
        return Err("VideoToolbox.framework could not be loaded".to_string());
    }
    AnyClass::get(name).ok_or_else(|| {
        format!(
            "{} is unavailable (requires macOS / iOS / tvOS 26)",
            name.to_string_lossy()
        )
    })
}

/// Why a `width x height` source cannot use the low-latency scaler, if it
/// cannot. Cheap enough to call per size change.
pub fn unsupported_reason(width: usize, height: usize) -> Option<String> {
    let configuration = match class(c"VTLowLatencySuperResolutionScalerConfiguration") {
        Ok(class) => class,
        Err(reason) => return Some(reason),
    };
    // SAFETY: class property getter returning BOOL.
    let supported: Bool = unsafe { msg_send![configuration, isSupported] };
    if !supported.as_bool() {
        return Some(
            "VTLowLatencySuperResolutionScaler is not supported on this device".to_string(),
        );
    }
    // SAFETY: class method returning NSArray<NSNumber *> *.
    let factors: Retained<NSArray<NSNumber>> = unsafe {
        msg_send![
            configuration,
            supportedScaleFactorsForFrameWidth: width as isize,
            frameHeight: height as isize
        ]
    };
    let has_2x = factors
        .iter()
        .any(|factor| (factor.floatValue() - SCALE).abs() < 1e-3);
    if has_2x {
        None
    } else if factors.is_empty() {
        Some(format!(
            "VTLowLatencySuperResolutionScaler does not support {width}x{height} sources (supported sources are 96x96 to 960x960)"
        ))
    } else {
        Some(format!(
            "VTLowLatencySuperResolutionScaler has no 2x mode for {width}x{height}"
        ))
    }
}

type Planes = (
    Retained<ProtocolObject<dyn MTLTexture>>,
    Retained<ProtocolObject<dyn MTLTexture>>,
);

struct InFlight {
    _source: CFRetained<CVPixelBuffer>,
    _destination: CFRetained<CVPixelBuffer>,
    _textures: Vec<CFRetained<CVMetalTexture>>,
    _parameters: Retained<AnyObject>,
}

/// A started low-latency super resolution session for one source size.
pub struct VideoToolboxScaler {
    width: usize,
    height: usize,
    processor: Retained<AnyObject>,
    source_pool: CFRetained<CVPixelBufferPool>,
    destination_pool: CFRetained<CVPixelBufferPool>,
    texture_cache: Option<CFRetained<CVMetalTextureCache>>,
    in_flight: VecDeque<InFlight>,
    output: Option<Planes>,
    frames: i64,
}

// SAFETY: VTFrameProcessor is used from one thread at a time: the session is
// started on the build worker and then handed to the render thread.
unsafe impl Send for VideoToolboxScaler {}

impl Drop for VideoToolboxScaler {
    fn drop(&mut self) {
        // SAFETY: `endSession` takes no arguments.
        unsafe {
            let _: () = msg_send![&self.processor, endSession];
        }
    }
}

fn create_pool(attributes: &NSDictionary) -> Result<CFRetained<CVPixelBufferPool>> {
    let mut raw: *mut CVPixelBufferPool = std::ptr::null_mut();
    // SAFETY: NSDictionary is toll-free bridged to CFDictionary.
    let attributes = unsafe { &*(attributes as *const NSDictionary).cast::<CFDictionary>() };
    let status = unsafe {
        CVPixelBufferPool::create(
            None,
            None,
            Some(attributes),
            NonNull::new(&mut raw).expect("stack pointer"),
        )
    };
    if status != kCVReturnSuccess {
        return Err(PlayerError::Renderer(format!(
            "CVPixelBufferPoolCreate failed: {status}"
        )));
    }
    let raw = NonNull::new(raw)
        .ok_or_else(|| PlayerError::Renderer("CVPixelBufferPoolCreate returned null".into()))?;
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn pool_buffer(pool: &CVPixelBufferPool) -> Result<CFRetained<CVPixelBuffer>> {
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::new(&mut raw).expect("ptr"))
    };
    if status != kCVReturnSuccess {
        return Err(PlayerError::Renderer(format!(
            "CVPixelBufferPoolCreatePixelBuffer failed: {status}"
        )));
    }
    let raw = NonNull::new(raw)
        .ok_or_else(|| PlayerError::Renderer("pixel buffer pool returned null".into()))?;
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn plane_texture(
    cache: &CVMetalTextureCache,
    buffer: &CVPixelBuffer,
    plane: usize,
    format: MTLPixelFormat,
) -> Result<(
    CFRetained<CVMetalTexture>,
    Retained<ProtocolObject<dyn MTLTexture>>,
)> {
    let width = objc2_core_video::CVPixelBufferGetWidthOfPlane(buffer, plane);
    let height = objc2_core_video::CVPixelBufferGetHeightOfPlane(buffer, plane);
    let mut raw: *mut CVMetalTexture = std::ptr::null_mut();
    let status = unsafe {
        CVMetalTextureCache::create_texture_from_image(
            None,
            cache,
            buffer,
            None,
            format,
            width,
            height,
            plane,
            NonNull::new(&mut raw).expect("ptr"),
        )
    };
    if status != kCVReturnSuccess {
        return Err(PlayerError::Renderer(format!(
            "CVMetalTextureCacheCreateTextureFromImage failed for VT plane {plane}: {status}"
        )));
    }
    let raw =
        NonNull::new(raw).ok_or_else(|| PlayerError::Renderer("null VT plane texture".into()))?;
    let texture = unsafe { CFRetained::<CVMetalTexture>::from_raw(raw) };
    let metal = CVMetalTextureGetTexture(&texture)
        .ok_or_else(|| PlayerError::Renderer("CVMetalTextureGetTexture returned nil".into()))?;
    Ok((texture, metal))
}

fn frame(buffer: &CVPixelBuffer, index: i64) -> Result<Retained<AnyObject>> {
    let class = class(c"VTFrameProcessorFrame").map_err(PlayerError::Renderer)?;
    let time = CMTime {
        value: index,
        timescale: 60,
        flags: 1, // kCMTimeFlags_Valid
        epoch: 0,
    };
    // SAFETY: `-initWithBuffer:presentationTimeStamp:` takes a
    // CVPixelBufferRef and a CMTime by value.
    let frame: Option<Retained<AnyObject>> = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        msg_send![allocated, initWithBuffer: buffer, presentationTimeStamp: time]
    };
    frame.ok_or_else(|| PlayerError::Renderer("VTFrameProcessorFrame init failed".into()))
}

impl VideoToolboxScaler {
    /// Starts a session for `width x height` sources. Blocking (model load);
    /// call from a worker thread.
    pub fn start(width: usize, height: usize) -> Result<Self> {
        if let Some(reason) = unsupported_reason(width, height) {
            return Err(PlayerError::Renderer(reason));
        }
        let configuration_class = class(c"VTLowLatencySuperResolutionScalerConfiguration")
            .map_err(PlayerError::Renderer)?;
        let processor_class = class(c"VTFrameProcessor").map_err(PlayerError::Renderer)?;
        // SAFETY: designated initializers with their declared argument types
        // (NSInteger, NSInteger, float); attribute getters return
        // NSDictionary *.
        let (processor, source_attributes, destination_attributes) = unsafe {
            let allocated: Allocated<AnyObject> = msg_send![configuration_class, alloc];
            let configuration: Option<Retained<AnyObject>> = msg_send![
                allocated,
                initWithFrameWidth: width as isize,
                frameHeight: height as isize,
                scaleFactor: SCALE
            ];
            let configuration = configuration.ok_or_else(|| {
                PlayerError::Renderer(
                    "VTLowLatencySuperResolutionScalerConfiguration init failed".into(),
                )
            })?;
            let source: Retained<NSDictionary> =
                msg_send![&configuration, sourcePixelBufferAttributes];
            let destination: Retained<NSDictionary> =
                msg_send![&configuration, destinationPixelBufferAttributes];
            let processor: Retained<AnyObject> = msg_send![processor_class, new];
            let started: std::result::Result<(), Retained<NSError>> = msg_send![
                &processor,
                startSessionWithConfiguration: &*configuration,
                error: _
            ];
            started.map_err(|error| {
                PlayerError::Renderer(format!(
                    "VTFrameProcessor session failed: {}",
                    error.localizedDescription()
                ))
            })?;
            (processor, source, destination)
        };
        Ok(Self {
            width,
            height,
            processor,
            source_pool: create_pool(&source_attributes)?,
            destination_pool: create_pool(&destination_attributes)?,
            texture_cache: None,
            in_flight: VecDeque::new(),
            output: None,
            frames: 0,
        })
    }

    pub fn source_size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn texture_cache(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
    ) -> Result<CFRetained<CVMetalTextureCache>> {
        if let Some(cache) = self.texture_cache.as_ref() {
            return Ok(cache.clone());
        }
        let mut raw: *mut CVMetalTextureCache = std::ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCache::create(
                None,
                None,
                device,
                None,
                NonNull::new(&mut raw).expect("ptr"),
            )
        };
        if status != kCVReturnSuccess {
            return Err(PlayerError::Renderer(format!(
                "CVMetalTextureCacheCreate failed: {status}"
            )));
        }
        let cache = unsafe {
            CFRetained::from_raw(
                NonNull::new(raw)
                    .ok_or_else(|| PlayerError::Renderer("null texture cache".into()))?,
            )
        };
        self.texture_cache = Some(cache.clone());
        Ok(cache)
    }

    /// The last output (luma, chroma), for repeated presentation of a frame.
    pub fn last_output(&self) -> Option<Planes> {
        self.output.clone()
    }

    /// Encodes a copy of the 8-bit 4:2:0 frame (`luma` R8, `chroma` RG8)
    /// and the 2x super resolution into `command_buffer`; returns the 2x
    /// luma and chroma textures.
    pub fn encode(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        luma: &ProtocolObject<dyn MTLTexture>,
        chroma: &ProtocolObject<dyn MTLTexture>,
    ) -> Result<Planes> {
        if luma.pixelFormat() != MTLPixelFormat::R8Unorm
            || chroma.pixelFormat() != MTLPixelFormat::RG8Unorm
        {
            return Err(PlayerError::Renderer(
                "VideoToolbox super resolution needs 8-bit 4:2:0 frames".into(),
            ));
        }
        let cache = self.texture_cache(device)?;
        let source = pool_buffer(&self.source_pool)?;
        let destination = pool_buffer(&self.destination_pool)?;
        let (source_luma_cv, source_luma) =
            plane_texture(&cache, &source, 0, MTLPixelFormat::R8Unorm)?;
        let (source_chroma_cv, source_chroma) =
            plane_texture(&cache, &source, 1, MTLPixelFormat::RG8Unorm)?;
        let (output_luma_cv, output_luma) =
            plane_texture(&cache, &destination, 0, MTLPixelFormat::R8Unorm)?;
        let (output_chroma_cv, output_chroma) =
            plane_texture(&cache, &destination, 1, MTLPixelFormat::RG8Unorm)?;

        let Some(blit) = command_buffer.blitCommandEncoder() else {
            return Err(PlayerError::Renderer(
                "blitCommandEncoder returned nil".into(),
            ));
        };
        for (from, to) in [(luma, &*source_luma), (chroma, &*source_chroma)] {
            let size = objc2_metal::MTLSize {
                width: from.width().min(to.width()),
                height: from.height().min(to.height()),
                depth: 1,
            };
            let origin = objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 };
            unsafe {
                blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    from, 0, 0, origin, size, to, 0, 0, origin,
                );
            }
        }
        blit.endEncoding();

        let index = self.frames;
        self.frames += 1;
        let source_frame = frame(&source, index)?;
        let destination_frame = frame(&destination, index)?;
        let parameters_class =
            class(c"VTLowLatencySuperResolutionScalerParameters").map_err(PlayerError::Renderer)?;
        // SAFETY: `-initWithSourceFrame:destinationFrame:` takes two
        // VTFrameProcessorFrame objects; `processWithCommandBuffer:parameters:`
        // an `id<MTLCommandBuffer>` and the parameters object.
        let parameters: Retained<AnyObject> = unsafe {
            let allocated: Allocated<AnyObject> = msg_send![parameters_class, alloc];
            let parameters: Option<Retained<AnyObject>> = msg_send![
                allocated,
                initWithSourceFrame: &*source_frame,
                destinationFrame: &*destination_frame
            ];
            let parameters = parameters.ok_or_else(|| {
                PlayerError::Renderer(
                    "VTLowLatencySuperResolutionScalerParameters init failed".into(),
                )
            })?;
            let _: () = msg_send![
                &self.processor,
                processWithCommandBuffer: command_buffer,
                parameters: &*parameters
            ];
            parameters
        };
        self.in_flight.push_back(InFlight {
            _source: source,
            _destination: destination,
            _textures: vec![
                source_luma_cv,
                source_chroma_cv,
                output_luma_cv,
                output_chroma_cv,
            ],
            _parameters: parameters,
        });
        while self.in_flight.len() > IN_FLIGHT {
            self.in_flight.pop_front();
        }
        self.output = Some((output_luma.clone(), output_chroma.clone()));
        Ok((output_luma, output_chroma))
    }
}
