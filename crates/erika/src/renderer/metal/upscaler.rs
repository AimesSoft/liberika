//! Neural luma upscaler (ArtCNN C-series) as Metal compute passes.
//!
//! The C-series models are 2x luma doublers: 7 convolutions (3x3, zero
//! padding), one residual skip from the first conv, DepthToSpace (DCR) and a
//! clip to [0, 1]. Weights are converted from the upstream ONNX releases by
//! `assets/artcnn/export_artcnn.py`; the blob layout is documented there.
//!
//! Two kernel backends share this front-end (selected by [`UpscalerBackend`],
//! built asynchronously because runtime shader compilation can take seconds):
//!
//! - **Scalar** (this file): feature maps in `texture2d_array<half>` textures,
//!   each thread accumulates a small output block with `half4x4` math.
//!   Portable fallback for GPUs without `simdgroup_matrix` (Intel/AMD).
//! - **SimdgroupMatrix** (`upscaler_matmul.rs`): convolutions evaluated as
//!   per-tap `simdgroup_half8x8` matrix multiplies. Default on Apple Silicon;
//!   measured 1.1x (C4F16) and 1.5x (C4F32) over the scalar backend on M2.
//!
//! Every pass runs at source resolution on the same command buffer as the
//! video render pass, so the upscaled luma is consumed zero-copy by the
//! existing YCbCr fragment shader while chroma keeps its original resolution.

use std::ffi::c_void;
use std::mem;
use std::ptr::NonNull;

use objc2::Message;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLComputeCommandEncoder,
    MTLComputePipelineState, MTLDevice, MTLLibrary, MTLPixelFormat, MTLResource,
    MTLResourceOptions, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};

use crate::core::{LumaUpscalerBackendStatus, PlayerError, Result};
use crate::renderer::artcnn::{
    self, ArtCnnBackendOverride, ArtCnnExecutionPolicy, ArtCnnModel, FrameTokenCache, LayerOffsets,
    MIDDLE_LAYER_COUNT as MID_LAYERS,
};
use crate::renderer::pipeline::LumaUpscalerMode;

#[path = "upscaler_matmul.rs"]
mod matmul;

#[repr(C)]
#[derive(Clone, Copy)]
struct ConvParams {
    weight_offset: u32,
    bias_offset: u32,
    relu: u32,
    add_residual: u32,
}

struct TexturePool {
    width: usize,
    height: usize,
    output_format: MTLPixelFormat,
    features: [Retained<ProtocolObject<dyn MTLTexture>>; 3],
    output: Retained<ProtocolObject<dyn MTLTexture>>,
    /// Frame token of the upscale currently held in `output`. When a frame is
    /// presented for several vsync ticks, the cached output is reused instead
    /// of re-running the network.
    frame_cache: FrameTokenCache,
}

struct UpscalerResources {
    mode: LumaUpscalerMode,
    slices: u32,
    block: (usize, usize),
    offsets: LayerOffsets,
    weights: Retained<ProtocolObject<dyn MTLBuffer>>,
    conv0_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    conv_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    conv6_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    pool: Option<TexturePool>,
}

/// Kernel implementation selection. `Auto` picks the `simdgroup_matrix`
/// backend on Apple7+ GPUs (all Apple Silicon) and falls back to the scalar
/// texture kernels elsewhere. `ERIKA_SR_BACKEND=scalar|matmul` overrides
/// `Auto` for experiments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpscalerBackend {
    #[default]
    Auto,
    Scalar,
    SimdgroupMatrix,
}

enum BackendResources {
    Scalar(UpscalerResources),
    Matmul(matmul::Resources),
}

/// Wrappers to move Metal objects into the background build thread.
/// SAFETY: `MTLDevice`, `MTLLibrary`, `MTLComputePipelineState` and
/// `MTLBuffer` are documented thread-safe; the texture/buffer pools inside
/// `BackendResources` start out empty and are only created on the render
/// thread after the hand-off.
struct SendResources(BackendResources);
unsafe impl Send for SendResources {}
struct SendDevice(Retained<ProtocolObject<dyn MTLDevice>>);
unsafe impl Send for SendDevice {}

/// Runtime shader compilation of the unrolled kernels takes seconds (the
/// matmul backend measured ~2.7 s on an M2), so resources are built on a
/// background thread; frames render without upscaling until the build lands.
struct PendingBuild {
    mode: LumaUpscalerMode,
    matmul: bool,
    slot: std::sync::Arc<std::sync::Mutex<Option<Result<SendResources>>>>,
}

#[derive(Clone, Copy)]
struct BackendChoice {
    matmul: bool,
    fallback_to_scalar: bool,
}

#[derive(Default)]
pub struct LumaUpscaler {
    mode: LumaUpscalerMode,
    backend: UpscalerBackend,
    resources: Option<BackendResources>,
    pending: Option<PendingBuild>,
    auto_matmul_failed: bool,
    auto_matmul_fallbacks: u64,
}

fn build_backend(
    device: &ProtocolObject<dyn MTLDevice>,
    mode: LumaUpscalerMode,
    matmul: bool,
) -> Result<BackendResources> {
    let model = artcnn::model_for_mode(mode)?;
    if matmul {
        Ok(BackendResources::Matmul(matmul::build_resources(
            device, model,
        )?))
    } else {
        Ok(BackendResources::Scalar(build_resources(device, model)?))
    }
}

impl LumaUpscaler {
    pub fn set_mode(&mut self, mode: LumaUpscalerMode) {
        if self.mode != mode {
            self.mode = mode;
            self.resources = None;
            self.pending = None;
            self.auto_matmul_failed = false;
        }
    }

    pub fn mode(&self) -> LumaUpscalerMode {
        self.mode
    }

    pub fn set_backend(&mut self, backend: UpscalerBackend) {
        if self.backend != backend {
            self.backend = backend;
            self.resources = None;
            self.pending = None;
            self.auto_matmul_failed = false;
        }
    }

    pub fn backend(&self) -> UpscalerBackend {
        self.backend
    }

    pub fn active_backend(&self) -> LumaUpscalerBackendStatus {
        if self.mode == LumaUpscalerMode::Off {
            return LumaUpscalerBackendStatus::Off;
        }
        match self.resources.as_ref() {
            Some(BackendResources::Scalar(_)) => LumaUpscalerBackendStatus::Scalar,
            Some(BackendResources::Matmul(_)) => LumaUpscalerBackendStatus::SimdgroupMatrix,
            None if self.pending.is_some() => LumaUpscalerBackendStatus::Building,
            None => LumaUpscalerBackendStatus::Inactive,
        }
    }

    pub fn auto_matmul_fallbacks(&self) -> u64 {
        self.auto_matmul_fallbacks
    }

    pub fn allocated_bytes(&self) -> u64 {
        match self.resources.as_ref() {
            Some(BackendResources::Scalar(resources)) => {
                let pool_bytes = resources.pool.as_ref().map_or(0, |pool| {
                    pool.features
                        .iter()
                        .fold(pool.output.allocatedSize() as u64, |total, texture| {
                            total.saturating_add(texture.allocatedSize() as u64)
                        })
                });
                (resources.weights.allocatedSize() as u64).saturating_add(pool_bytes)
            }
            Some(BackendResources::Matmul(resources)) => resources.allocated_bytes(),
            None => 0,
        }
    }

    fn record_auto_matmul_failure(&mut self, error: &PlayerError) {
        if !self.auto_matmul_failed {
            self.auto_matmul_fallbacks = self.auto_matmul_fallbacks.saturating_add(1);
            crate::trace::diagnostic_at(
                crate::trace::LogLevel::Warn,
                format!(
                    "Erika luma upscaler falling back to scalar backend after matmul build failed: {error}"
                ),
            );
        }
        self.auto_matmul_failed = true;
    }

    fn backend_choice(&self, device: &ProtocolObject<dyn MTLDevice>) -> BackendChoice {
        match self.backend {
            UpscalerBackend::Scalar => BackendChoice {
                matmul: false,
                fallback_to_scalar: false,
            },
            UpscalerBackend::SimdgroupMatrix => BackendChoice {
                matmul: true,
                fallback_to_scalar: false,
            },
            UpscalerBackend::Auto => match ArtCnnBackendOverride::from_environment() {
                Some(ArtCnnBackendOverride::Scalar) => BackendChoice {
                    matmul: false,
                    fallback_to_scalar: false,
                },
                Some(ArtCnnBackendOverride::Matrix) => BackendChoice {
                    matmul: true,
                    fallback_to_scalar: false,
                },
                _ => BackendChoice {
                    matmul: !self.auto_matmul_failed
                        && device.supportsFamily(objc2_metal::MTLGPUFamily::Apple7),
                    fallback_to_scalar: true,
                },
            },
        }
    }

    /// Polls the background build, spawning one if necessary. Returns
    /// `Ok(true)` once matching resources are installed.
    fn poll_pending_build(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        mode: LumaUpscalerMode,
        matmul: bool,
    ) -> Result<bool> {
        if let Some(pending) = self.pending.as_ref() {
            if pending.mode != mode || pending.matmul != matmul {
                // Stale build for a previous configuration; drop the handle
                // and let the thread's result fall on the floor.
                self.pending = None;
            }
        }
        if let Some(pending) = self.pending.as_ref() {
            let finished = pending.slot.lock().expect("build slot poisoned").take();
            return match finished {
                Some(Ok(resources)) => {
                    self.pending = None;
                    self.resources = Some(resources.0);
                    Ok(true)
                }
                Some(Err(error)) => {
                    self.pending = None;
                    Err(error)
                }
                None => Ok(false),
            };
        }

        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let thread_slot = std::sync::Arc::clone(&slot);
        let thread_device = SendDevice(device.retain());
        std::thread::Builder::new()
            .name("erika-upscaler-build".to_string())
            .spawn(move || {
                let device = thread_device;
                let result = build_backend(&device.0, mode, matmul).map(SendResources);
                *thread_slot.lock().expect("build slot poisoned") = Some(result);
            })
            .map_err(|error| {
                PlayerError::Renderer(format!("upscaler build thread spawn failed: {error}"))
            })?;
        self.pending = Some(PendingBuild { mode, matmul, slot });
        Ok(false)
    }

    /// Builds the active configuration synchronously. Intended for tests and
    /// benchmarks; playback uses the background build instead.
    pub fn prepare_blocking(&mut self, device: &ProtocolObject<dyn MTLDevice>) -> Result<()> {
        if self.mode == LumaUpscalerMode::Off {
            return Ok(());
        }
        let mode = self.mode;
        let choice = self.backend_choice(device);
        let resources_match = match self.resources.as_ref() {
            Some(BackendResources::Scalar(resources)) => !choice.matmul && resources.mode == mode,
            Some(BackendResources::Matmul(resources)) => choice.matmul && resources.mode() == mode,
            None => false,
        };
        if !resources_match {
            self.pending = None;
            self.resources = match build_backend(device, mode, choice.matmul) {
                Ok(resources) => Some(resources),
                Err(error) if choice.matmul && choice.fallback_to_scalar => {
                    self.record_auto_matmul_failure(&error);
                    Some(build_backend(device, mode, false)?)
                }
                Err(error) => return Err(error),
            };
        }
        Ok(())
    }

    /// Encodes the 2x luma upscale onto `command_buffer` and returns the
    /// upscaled luma texture, or `Ok(None)` when the upscaler is off.
    /// `output_format` should match the source luma plane format so the
    /// downstream YCbCr math keeps the same normalization.
    pub fn encode(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        luma: &ProtocolObject<dyn MTLTexture>,
        output_format: MTLPixelFormat,
    ) -> Result<Option<Retained<ProtocolObject<dyn MTLTexture>>>> {
        self.encode_with_token(device, command_buffer, luma, output_format, None)
    }

    /// Like [`Self::encode`], but when `frame_token` matches the previously
    /// upscaled frame the cached output is returned without re-running the
    /// network (frames are typically presented for several vsync ticks).
    pub fn encode_with_token(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        luma: &ProtocolObject<dyn MTLTexture>,
        output_format: MTLPixelFormat,
        frame_token: Option<u64>,
    ) -> Result<Option<Retained<ProtocolObject<dyn MTLTexture>>>> {
        if self.mode == LumaUpscalerMode::Off {
            return Ok(None);
        }
        let mode = self.mode;
        let choice = self.backend_choice(device);
        let resources_match = match self.resources.as_ref() {
            Some(BackendResources::Scalar(resources)) => !choice.matmul && resources.mode == mode,
            Some(BackendResources::Matmul(resources)) => choice.matmul && resources.mode() == mode,
            None => false,
        };
        if !resources_match {
            self.resources = None;
            match self.poll_pending_build(device, mode, choice.matmul) {
                Ok(true) => {}
                Ok(false) => {
                    // Still compiling on the background thread; render this
                    // frame without upscaling.
                    return Ok(None);
                }
                Err(error) if choice.matmul && choice.fallback_to_scalar => {
                    self.record_auto_matmul_failure(&error);
                    self.resources = None;
                    self.pending = None;
                    if !self.poll_pending_build(device, mode, false)? {
                        return Ok(None);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let resources = match self.resources.as_mut().expect("resources built above") {
            BackendResources::Matmul(resources) => {
                return matmul::encode(
                    resources,
                    device,
                    command_buffer,
                    luma,
                    output_format,
                    frame_token,
                )
                .map(Some);
            }
            BackendResources::Scalar(resources) => resources,
        };

        let width = luma.width();
        let height = luma.height();
        ensure_pool(resources, device, width, height, output_format)?;
        let pool = resources.pool.as_ref().expect("pool built above");
        if pool.frame_cache.matches(frame_token) {
            return Ok(Some(pool.output.clone()));
        }

        let Some(encoder) = command_buffer.computeCommandEncoder() else {
            return Err(PlayerError::Renderer(
                "computeCommandEncoder returned nil".to_string(),
            ));
        };
        encoder.setLabel(Some(&NSString::from_str("erika_luma_upscaler")));
        unsafe { encoder.setBuffer_offset_atIndex(Some(&resources.weights), 0, 0) };

        // Uniform threadgroups: the kernels load weights into threadgroup
        // memory cooperatively before the bounds check, so every threadgroup
        // must be fully populated. Each thread computes a BX x BY pixel
        // block, so a threadgroup covers TG*BX x TG*BY source pixels. TG must
        // match the shader template.
        const TG: usize = 16;
        let (block_x, block_y) = resources.block;
        let grid = MTLSize {
            width: width.div_ceil(TG * block_x),
            height: height.div_ceil(TG * block_y),
            depth: 1,
        };
        let threadgroup = MTLSize {
            width: TG,
            height: TG,
            depth: 1,
        };
        let offsets = &resources.offsets;
        let feats = &pool.features;

        // conv0: luma -> A (linear, kept for the residual skip)
        let params = ConvParams {
            weight_offset: offsets.conv0_w,
            bias_offset: offsets.conv0_b,
            relu: 0,
            add_residual: 0,
        };
        encoder.setComputePipelineState(&resources.conv0_pipeline);
        unsafe { encoder.setTexture_atIndex(Some(luma), 0) };
        unsafe { encoder.setTexture_atIndex(Some(&feats[0]), 1) };
        set_params(&encoder, &params);
        encoder.dispatchThreadgroups_threadsPerThreadgroup(grid, threadgroup);

        // conv1..conv5 ping-pong: A->B->C->B->C->B, conv5 adds the A residual.
        encoder.setComputePipelineState(&resources.conv_pipeline);
        let chain: [(usize, usize); MID_LAYERS] = [(0, 1), (1, 2), (2, 1), (1, 2), (2, 1)];
        for (layer, (src, dst)) in chain.iter().enumerate() {
            let last = layer == MID_LAYERS - 1;
            let params = ConvParams {
                weight_offset: offsets.mid_w[layer],
                bias_offset: offsets.mid_b[layer],
                relu: u32::from(!last),
                add_residual: u32::from(last),
            };
            unsafe { encoder.setTexture_atIndex(Some(&feats[*src]), 0) };
            unsafe { encoder.setTexture_atIndex(Some(&feats[*dst]), 1) };
            unsafe { encoder.setTexture_atIndex(Some(&feats[0]), 2) };
            set_params(&encoder, &params);
            encoder.dispatchThreadgroups_threadsPerThreadgroup(grid, threadgroup);
        }

        // conv6 + DepthToSpace(DCR) + clip: B -> 2x luma
        let params = ConvParams {
            weight_offset: offsets.conv6_w,
            bias_offset: offsets.conv6_b,
            relu: 0,
            add_residual: 0,
        };
        encoder.setComputePipelineState(&resources.conv6_pipeline);
        unsafe { encoder.setTexture_atIndex(Some(&feats[1]), 0) };
        unsafe { encoder.setTexture_atIndex(Some(&pool.output), 1) };
        set_params(&encoder, &params);
        encoder.dispatchThreadgroups_threadsPerThreadgroup(grid, threadgroup);

        let output = pool.output.clone();
        encoder.endEncoding();
        resources
            .pool
            .as_mut()
            .expect("pool built above")
            .frame_cache
            .commit(frame_token);
        Ok(Some(output))
    }
}

fn set_params(encoder: &ProtocolObject<dyn MTLComputeCommandEncoder>, params: &ConvParams) {
    unsafe {
        encoder.setBytes_length_atIndex(
            NonNull::new((params as *const ConvParams).cast::<c_void>().cast_mut())
                .expect("params pointer is non-null"),
            mem::size_of::<ConvParams>(),
            1,
        );
    }
}

fn build_resources(
    device: &ProtocolObject<dyn MTLDevice>,
    model: ArtCnnModel<'_>,
) -> Result<UpscalerResources> {
    let mode = model.mode;
    let slices = model.layout.feature_slices;
    let payload = model.payload;

    let weights = unsafe {
        device.newBufferWithBytes_length_options(
            NonNull::new(payload.as_ptr().cast::<c_void>().cast_mut())
                .expect("payload pointer is non-null"),
            payload.len(),
            MTLResourceOptions::StorageModeShared,
        )
    }
    .ok_or_else(|| PlayerError::Renderer("newBufferWithBytes returned nil".to_string()))?;

    let policy = ArtCnnExecutionPolicy::for_mode(mode);
    let (block_x, block_y) = policy.scalar_block;
    let source = UPSCALER_SHADER_TEMPLATE
        .replace("{SLICES}", &slices.to_string())
        .replace("{BX}", &block_x.to_string())
        .replace("{BY}", &block_y.to_string());
    let library = device
        .newLibraryWithSource_options_error(&NSString::from_str(&source), None)
        .map_err(|error| {
            PlayerError::Renderer(format!("Metal upscaler shader compile failed: {error}"))
        })?;
    let pipeline = |name: &str| -> Result<Retained<ProtocolObject<dyn MTLComputePipelineState>>> {
        let function = library
            .newFunctionWithName(&NSString::from_str(name))
            .ok_or_else(|| PlayerError::Renderer(format!("Metal shader missing {name}")))?;
        device
            .newComputePipelineStateWithFunction_error(&function)
            .map_err(|error| {
                PlayerError::Renderer(format!("compute pipeline {name} failed: {error}"))
            })
    };

    Ok(UpscalerResources {
        mode,
        slices,
        block: policy.scalar_block,
        offsets: model.layout.layer_offsets,
        weights,
        conv0_pipeline: pipeline("artcnn_conv0")?,
        conv_pipeline: pipeline("artcnn_conv")?,
        conv6_pipeline: pipeline("artcnn_conv6")?,
        pool: None,
    })
}

fn ensure_pool(
    resources: &mut UpscalerResources,
    device: &ProtocolObject<dyn MTLDevice>,
    width: usize,
    height: usize,
    output_format: MTLPixelFormat,
) -> Result<()> {
    if let Some(pool) = resources.pool.as_ref() {
        if pool.width == width && pool.height == height && pool.output_format == output_format {
            return Ok(());
        }
    }

    let feature_texture = || -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        let descriptor = unsafe {
            let descriptor = MTLTextureDescriptor::new();
            descriptor.setTextureType(MTLTextureType::Type2DArray);
            descriptor.setPixelFormat(MTLPixelFormat::RGBA16Float);
            descriptor.setWidth(width);
            descriptor.setHeight(height);
            descriptor.setArrayLength(resources.slices as usize);
            descriptor.setStorageMode(MTLStorageMode::Private);
            descriptor.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
            descriptor
        };
        device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| PlayerError::Renderer("feature texture alloc failed".to_string()))
    };

    let output_descriptor = unsafe {
        let descriptor =
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                output_format,
                width * 2,
                height * 2,
                false,
            );
        descriptor.setStorageMode(MTLStorageMode::Private);
        descriptor.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
        descriptor
    };
    let output = device
        .newTextureWithDescriptor(&output_descriptor)
        .ok_or_else(|| PlayerError::Renderer("upscaled luma texture alloc failed".to_string()))?;

    resources.pool = Some(TexturePool {
        width,
        height,
        output_format,
        features: [feature_texture()?, feature_texture()?, feature_texture()?],
        output,
        frame_cache: FrameTokenCache::default(),
    });
    Ok(())
}

/// `{SLICES}` is substituted with the feature-slice count (features / 4)
/// before compilation, so each variant compiles with unrolled loops.
///
/// Weight loads use dynamically-uniform offsets into the `constant` buffer,
/// which hit the constant cache / uniform path on Apple GPUs. Each thread
/// computes a BX x BY block of output pixels so every loaded weight matrix
/// is reused across the block (measured faster than threadgroup-memory
/// staging and far faster than forced full unrolling, which explodes
/// register pressure).
const UPSCALER_SHADER_TEMPLATE: &str = r#"
#include <metal_stdlib>
using namespace metal;

constant constexpr uint S = {SLICES};
constant constexpr uint BX = {BX};
constant constexpr uint BY = {BY};
constant constexpr uint TG = 16;
constant constexpr uint CONV0_W = S * 9 + S;       // taps + bias
constant constexpr uint MID_W = S * 9 * S * 4 + S; // matrices + bias
constant constexpr uint CONV6_W = 9 * S * 4 + 1;   // matrices + bias

struct ConvParams {
    uint weight_offset;
    uint bias_offset;
    uint relu;
    uint add_residual;
};

inline half4x4 weight_matrix(constant half4* weights, uint offset) {
    return half4x4(weights[offset], weights[offset + 1], weights[offset + 2], weights[offset + 3]);
}

kernel void artcnn_conv0(
    texture2d<half, access::read> luma [[texture(0)]],
    texture2d_array<half, access::write> dst [[texture(1)]],
    constant half4* weights [[buffer(0)]],
    constant ConvParams& params [[buffer(1)]],
    uint2 gid [[thread_position_in_grid]])
{
    uint width = luma.get_width();
    uint height = luma.get_height();
    uint2 base = gid * uint2(BX, BY);
    if (base.x >= width || base.y >= height) {
        return;
    }
    constant half4* w = weights + params.weight_offset;
    constant half4* bias = w + S * 9;

    half window[BY + 2][BX + 2];
    for (uint wy = 0; wy < BY + 2; ++wy) {
        for (uint wx = 0; wx < BX + 2; ++wx) {
            int2 coord = int2(base) + int2(int(wx) - 1, int(wy) - 1);
            bool outside = coord.x < 0 || coord.y < 0 || coord.x >= int(width) || coord.y >= int(height);
            window[wy][wx] = outside ? 0.0h : luma.read(uint2(coord)).x;
        }
    }
    half4 acc[S][BY][BX];
    for (uint s = 0; s < S; ++s) {
        for (uint py = 0; py < BY; ++py) {
            for (uint px = 0; px < BX; ++px) {
                acc[s][py][px] = bias[s];
            }
        }
    }
    for (uint s = 0; s < S; ++s) {
        for (uint dy = 0; dy < 3; ++dy) {
            for (uint dx = 0; dx < 3; ++dx) {
                half4 tap_w = w[s * 9 + dy * 3 + dx];
                for (uint py = 0; py < BY; ++py) {
                    for (uint px = 0; px < BX; ++px) {
                        acc[s][py][px] += tap_w * window[py + dy][px + dx];
                    }
                }
            }
        }
    }
    for (uint s = 0; s < S; ++s) {
        for (uint py = 0; py < BY; ++py) {
            for (uint px = 0; px < BX; ++px) {
                uint2 coord = base + uint2(px, py);
                if (coord.x < width && coord.y < height) {
                    dst.write(acc[s][py][px], coord, s);
                }
            }
        }
    }
}

kernel void artcnn_conv(
    texture2d_array<half, access::read> src [[texture(0)]],
    texture2d_array<half, access::write> dst [[texture(1)]],
    texture2d_array<half, access::read> residual [[texture(2)]],
    constant half4* weights [[buffer(0)]],
    constant ConvParams& params [[buffer(1)]],
    uint2 gid [[thread_position_in_grid]])
{
    uint width = src.get_width();
    uint height = src.get_height();
    uint2 base = gid * uint2(BX, BY);
    if (base.x >= width || base.y >= height) {
        return;
    }
    constant half4* w = weights + params.weight_offset;
    constant half4* bias = w + S * 9 * S * 4;

    half4 acc[S][BY][BX];
    for (uint s = 0; s < S; ++s) {
        for (uint py = 0; py < BY; ++py) {
            for (uint px = 0; px < BX; ++px) {
                acc[s][py][px] = bias[s];
            }
        }
    }
    // One input slice at a time: its window stays in registers and every
    // staged weight matrix is reused by all output pixels of the block.
    for (uint i = 0; i < S; ++i) {
        half4 window[BY + 2][BX + 2];
        for (uint wy = 0; wy < BY + 2; ++wy) {
            for (uint wx = 0; wx < BX + 2; ++wx) {
                int2 coord = int2(base) + int2(int(wx) - 1, int(wy) - 1);
                bool outside = coord.x < 0 || coord.y < 0 || coord.x >= int(width) || coord.y >= int(height);
                window[wy][wx] = outside ? half4(0.0h) : src.read(uint2(coord), i);
            }
        }
        for (uint s = 0; s < S; ++s) {
            for (uint dy = 0; dy < 3; ++dy) {
                for (uint dx = 0; dx < 3; ++dx) {
                    // offset(s, tap, i) in half4 units: (((s * 9) + tap) * S + i) * 4
                    half4x4 m = weight_matrix(w, (((s * 9u) + dy * 3 + dx) * S + i) * 4u);
                    for (uint py = 0; py < BY; ++py) {
                        for (uint px = 0; px < BX; ++px) {
                            acc[s][py][px] += m * window[py + dy][px + dx];
                        }
                    }
                }
            }
        }
    }
    for (uint s = 0; s < S; ++s) {
        for (uint py = 0; py < BY; ++py) {
            for (uint px = 0; px < BX; ++px) {
                uint2 coord = base + uint2(px, py);
                if (coord.x >= width || coord.y >= height) {
                    continue;
                }
                half4 value = acc[s][py][px];
                if (params.add_residual != 0) {
                    value += residual.read(coord, s);
                }
                if (params.relu != 0) {
                    value = max(value, half4(0.0h));
                }
                dst.write(value, coord, s);
            }
        }
    }
}

kernel void artcnn_conv6(
    texture2d_array<half, access::read> src [[texture(0)]],
    texture2d<half, access::write> dst [[texture(1)]],
    constant half4* weights [[buffer(0)]],
    constant ConvParams& params [[buffer(1)]],
    uint2 gid [[thread_position_in_grid]])
{
    uint width = src.get_width();
    uint height = src.get_height();
    uint2 base = gid * uint2(BX, BY);
    if (base.x >= width || base.y >= height) {
        return;
    }
    constant half4* w = weights + params.weight_offset;
    half4 bias = w[9 * S * 4];
    half4 acc[BY][BX];
    for (uint py = 0; py < BY; ++py) {
        for (uint px = 0; px < BX; ++px) {
            acc[py][px] = bias;
        }
    }
    for (uint i = 0; i < S; ++i) {
        half4 window[BY + 2][BX + 2];
        for (uint wy = 0; wy < BY + 2; ++wy) {
            for (uint wx = 0; wx < BX + 2; ++wx) {
                int2 coord = int2(base) + int2(int(wx) - 1, int(wy) - 1);
                bool outside = coord.x < 0 || coord.y < 0 || coord.x >= int(width) || coord.y >= int(height);
                window[wy][wx] = outside ? half4(0.0h) : src.read(uint2(coord), i);
            }
        }
        for (uint dy = 0; dy < 3; ++dy) {
            for (uint dx = 0; dx < 3; ++dx) {
                // offset(tap, i) in half4 units: ((tap * S) + i) * 4
                half4x4 m = weight_matrix(w, (((dy * 3 + dx) * S) + i) * 4u);
                for (uint py = 0; py < BY; ++py) {
                    for (uint px = 0; px < BX; ++px) {
                        acc[py][px] += m * window[py + dy][px + dx];
                    }
                }
            }
        }
    }
    // DepthToSpace DCR: channel c maps to subpixel (dx, dy) = (c % 2, c / 2).
    for (uint py = 0; py < BY; ++py) {
        for (uint px = 0; px < BX; ++px) {
            uint2 coord = base + uint2(px, py);
            if (coord.x >= width || coord.y >= height) {
                continue;
            }
            half4 value = clamp(acc[py][px], 0.0h, 1.0h);
            uint2 out = coord * 2;
            dst.write(half4(value.x), out);
            dst.write(half4(value.y), out + uint2(1, 0));
            dst.write(half4(value.z), out + uint2(0, 1));
            dst.write(half4(value.w), out + uint2(1, 1));
        }
    }
}
"#;
