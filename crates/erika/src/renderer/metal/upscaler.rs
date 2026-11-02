//! Luma super resolution on Metal: one front-end for every
//! [`LumaUpscalerMode`].
//!
//! | Mode family | Implementation | Status |
//! |---|---|---|
//! | Network (ArtCNN, Anime4K, custom) | [`network`]: generic engine generated from the [`SrModel`]; Metal 4 tensor ops on Apple10 GPUs, `simdgroup_matrix` on Apple7+, scalar elsewhere | `TensorOps` / `SimdgroupMatrix` / `Scalar` |
//! | Legacy ArtCNN C4 kernels | hand-written scalar (this file) and `simdgroup_matrix` (`upscaler_matmul.rs`) kernels, selectable through [`UpscalerBackend`] | `Scalar` / `SimdgroupMatrix` |
//! | `Fsr1` | [`fsr`]: EASU + RCAS straight to the presented size | `Scalar` |
//! | `MetalFxSpatial` | [`metalfx`]: `MTLFXSpatialScaler` on the luma plane | `MetalFx` |
//! | `PlatformVideo` | [`video_toolbox`]: `VTLowLatencySuperResolutionScaler` (2x, sources up to 960x960, 8-bit 4:2:0), replaces luma and chroma | `VideoToolbox` |
//!
//! Shader compilation and VideoToolbox session start-up can take seconds, so
//! they run on a worker thread; frames are presented with native sampling
//! (status `Building`) until the resources land. Whenever a mode cannot run
//! (unsupported GPU/OS, source size or format, build failure) the frame is
//! sampled natively, the status is `Inactive` and [`LumaUpscaler::status_reason`]
//! says why.
//!
//! Every pass runs on the video frame's command buffer, so the upscaled
//! planes are consumed zero-copy by the YCbCr fragment shader, which samples
//! them with normalized coordinates (any output size works).
//!
//! The legacy scalar ArtCNN kernels below keep feature maps in
//! `texture2d_array<half>` textures; each thread accumulates a small output
//! block with `half4x4` math.

use std::ffi::c_void;
use std::mem;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

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
    self, ArtCnnExecutionPolicy, ArtCnnModel, LayerOffsets, MIDDLE_LAYER_COUNT as MID_LAYERS,
};
use crate::renderer::pipeline::{LumaUpscalerMode, UpscalerFamily};
use crate::renderer::sr::catalog;
use crate::renderer::sr::model::SrModel;
pub use crate::renderer::sr::reference::LumaRange;

#[path = "upscaler_fsr.rs"]
pub mod fsr;
#[path = "upscaler_matmul.rs"]
mod matmul;
#[path = "upscaler_metalfx.rs"]
pub mod metalfx;
#[path = "upscaler_network.rs"]
pub mod network;
#[path = "upscaler_vt.rs"]
pub mod video_toolbox;

use network::{EngineKind, NetworkEngine};

/// Whether `Auto` runs the three legacy ArtCNN modes on the hand-written
/// kernels instead of the generic engine. The generic engine executes the
/// folded network (26-33% fewer MACs) and measured faster for every kernel
/// family on Apple M5 Pro at 1080p: scalar 1.2-1.5x, `simdgroup_matrix`
/// 1.3-1.5x, tensor ops 2.8-4.4x. It is also more accurate (MAE vs ONNX
/// about half), so it is the default; the legacy kernels stay selectable
/// through [`UpscalerBackend`].
const AUTO_PREFERS_LEGACY_KERNELS: bool = false;

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
}

struct UpscalerResources {
    slices: u32,
    block: (usize, usize),
    offsets: LayerOffsets,
    weights: Retained<ProtocolObject<dyn MTLBuffer>>,
    conv0_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    conv_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    conv6_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    pool: Option<TexturePool>,
}

/// Kernel selection for network modes. `Auto` picks the best family the GPU
/// supports (tensor ops on Apple10, `simdgroup_matrix` on Apple7+, scalar
/// otherwise); `ERIKA_SR_BACKEND=scalar|matmul|tensor|legacy|legacy-scalar`
/// overrides `Auto` for experiments. Non-network modes ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpscalerBackend {
    #[default]
    Auto,
    /// Generic engine, scalar kernels.
    Scalar,
    /// Generic engine, `simdgroup_matrix` kernels.
    SimdgroupMatrix,
    /// Generic engine, Metal 4 tensor ops (GPU matrix units on Apple10).
    TensorOps,
    /// Hand-written ArtCNN C4F16/C4F16-DS/C4F32 scalar kernels.
    LegacyScalar,
    /// Hand-written ArtCNN C4F16/C4F16-DS/C4F32 `simdgroup_matrix` kernels.
    LegacySimdgroupMatrix,
}

impl UpscalerBackend {
    fn resolve(self) -> Self {
        if self != Self::Auto {
            return self;
        }
        static OVERRIDE: std::sync::OnceLock<UpscalerBackend> = std::sync::OnceLock::new();
        *OVERRIDE.get_or_init(|| match std::env::var("ERIKA_SR_BACKEND").as_deref() {
            Ok("scalar") => Self::Scalar,
            Ok("matmul") => Self::SimdgroupMatrix,
            Ok("tensor") => Self::TensorOps,
            Ok("legacy") => Self::LegacySimdgroupMatrix,
            Ok("legacy-scalar") => Self::LegacyScalar,
            _ => Self::Auto,
        })
    }
}

/// What has to be built for the current mode.
#[derive(Clone)]
enum Target {
    LegacyScalar,
    LegacyMatmul,
    Network(EngineKind, Arc<SrModel>),
    Fsr,
    MetalFx,
    VideoToolbox(usize, usize),
}

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::LegacyScalar, Self::LegacyScalar)
            | (Self::LegacyMatmul, Self::LegacyMatmul)
            | (Self::Fsr, Self::Fsr)
            | (Self::MetalFx, Self::MetalFx) => true,
            (Self::Network(a, model_a), Self::Network(b, model_b)) => {
                a == b && Arc::ptr_eq(model_a, model_b)
            }
            (Self::VideoToolbox(wa, ha), Self::VideoToolbox(wb, hb)) => wa == wb && ha == hb,
            _ => false,
        }
    }
}

enum Active {
    LegacyScalar(UpscalerResources),
    LegacyMatmul(matmul::Resources),
    Network(NetworkEngine),
    Fsr(fsr::FsrUpscaler),
    MetalFx(metalfx::MetalFxSpatial),
    VideoToolbox(video_toolbox::VideoToolboxScaler),
}

impl Active {
    fn status(&self) -> LumaUpscalerBackendStatus {
        match self {
            Self::LegacyScalar(_) | Self::Fsr(_) => LumaUpscalerBackendStatus::Scalar,
            Self::LegacyMatmul(_) => LumaUpscalerBackendStatus::SimdgroupMatrix,
            Self::Network(engine) => match engine.kind() {
                EngineKind::Scalar => LumaUpscalerBackendStatus::Scalar,
                EngineKind::Simdgroup => LumaUpscalerBackendStatus::SimdgroupMatrix,
                EngineKind::TensorOps => LumaUpscalerBackendStatus::TensorOps,
            },
            Self::MetalFx(_) => LumaUpscalerBackendStatus::MetalFx,
            Self::VideoToolbox(_) => LumaUpscalerBackendStatus::VideoToolbox,
        }
    }

    fn allocated_bytes(&self) -> u64 {
        match self {
            Self::LegacyScalar(resources) => {
                let pool_bytes = resources.pool.as_ref().map_or(0, |pool| {
                    pool.features
                        .iter()
                        .fold(pool.output.allocatedSize() as u64, |total, texture| {
                            total.saturating_add(texture.allocatedSize() as u64)
                        })
                });
                (resources.weights.allocatedSize() as u64).saturating_add(pool_bytes)
            }
            Self::LegacyMatmul(resources) => resources.allocated_bytes(),
            Self::Network(engine) => engine.allocated_bytes(),
            Self::Fsr(fsr) => fsr.allocated_bytes(),
            Self::MetalFx(scaler) => scaler.allocated_bytes(),
            Self::VideoToolbox(_) => 0,
        }
    }
}

/// Wrappers to move Metal objects into the background build thread.
/// SAFETY: `MTLDevice`, `MTLLibrary`, `MTLComputePipelineState` and
/// `MTLBuffer` are documented thread-safe; texture/buffer pools are created on
/// the render thread after the hand-off, and a VideoToolbox session is only
/// used by one thread at a time.
struct SendActive(Active);
unsafe impl Send for SendActive {}
struct SendDevice(Retained<ProtocolObject<dyn MTLDevice>>);
unsafe impl Send for SendDevice {}

type BuildSlot = Arc<Mutex<Option<Result<SendActive>>>>;

struct PendingBuild {
    target: Target,
    slot: BuildSlot,
}

/// One frame to upscale.
pub struct UpscaleRequest<'a> {
    pub luma: &'a ProtocolObject<dyn MTLTexture>,
    /// The frame's chroma plane; platform video scalers replace it.
    pub chroma: Option<&'a ProtocolObject<dyn MTLTexture>>,
    /// Format of network/FSR outputs; it should match the luma plane so the
    /// YCbCr math keeps the same normalization.
    pub output_format: MTLPixelFormat,
    pub range: LumaRange,
    /// Size, in luma texels, at which the luma plane is presented. Spatial
    /// and MetalFX modes scale straight to it; networks always double.
    pub presented: (usize, usize),
    /// Decoded-frame identity: repeated presentations reuse the output.
    pub frame_token: Option<u64>,
}

/// Upscaled planes for the presentation shader.
#[derive(Clone)]
pub struct UpscaledFrame {
    pub luma: Retained<ProtocolObject<dyn MTLTexture>>,
    /// Replacement chroma plane when the whole frame was scaled.
    pub chroma: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
}

#[derive(Clone, Copy, PartialEq)]
struct CacheKey {
    token: u64,
    range: LumaRange,
    presented: (usize, usize),
    output_format: MTLPixelFormat,
    luma: (usize, usize),
}

pub struct LumaUpscaler {
    mode: LumaUpscalerMode,
    backend: UpscalerBackend,
    custom_model: Option<Arc<SrModel>>,
    active: Option<(Target, Active)>,
    pending: Option<PendingBuild>,
    /// Build failure of a target; not retried until the configuration
    /// changes.
    failed: Option<(Target, String)>,
    fallbacks: u64,
    cached: Option<(CacheKey, UpscaledFrame)>,
    status: LumaUpscalerBackendStatus,
    reason: Option<String>,
    video_toolbox_support: Option<((usize, usize), Option<String>)>,
    /// The bundled network of the current mode, resolved once so build
    /// targets compare by identity.
    bundled_model: Option<(LumaUpscalerMode, Arc<SrModel>)>,
}

impl Default for LumaUpscaler {
    fn default() -> Self {
        Self {
            mode: LumaUpscalerMode::Off,
            backend: UpscalerBackend::Auto,
            custom_model: None,
            active: None,
            pending: None,
            failed: None,
            fallbacks: 0,
            cached: None,
            status: LumaUpscalerBackendStatus::Off,
            reason: None,
            video_toolbox_support: None,
            bundled_model: None,
        }
    }
}

fn build_target(
    device: &ProtocolObject<dyn MTLDevice>,
    mode: LumaUpscalerMode,
    target: &Target,
) -> Result<Active> {
    Ok(match target {
        Target::LegacyScalar => {
            Active::LegacyScalar(build_resources(device, artcnn::model_for_mode(mode)?)?)
        }
        Target::LegacyMatmul => Active::LegacyMatmul(matmul::build_resources(
            device,
            artcnn::model_for_mode(mode)?,
        )?),
        Target::Network(kind, model) => {
            Active::Network(NetworkEngine::build(device, model.clone(), *kind)?)
        }
        Target::Fsr => Active::Fsr(fsr::FsrUpscaler::build(device)?),
        Target::MetalFx => {
            Active::MetalFx(metalfx::MetalFxSpatial::new(device).map_err(PlayerError::Renderer)?)
        }
        Target::VideoToolbox(width, height) => {
            Active::VideoToolbox(video_toolbox::VideoToolboxScaler::start(*width, *height)?)
        }
    })
}

impl LumaUpscaler {
    fn reset(&mut self) {
        self.active = None;
        self.pending = None;
        self.failed = None;
        self.cached = None;
        self.reason = None;
        self.status = if self.mode == LumaUpscalerMode::Off {
            LumaUpscalerBackendStatus::Off
        } else {
            LumaUpscalerBackendStatus::Inactive
        };
    }

    pub fn set_mode(&mut self, mode: LumaUpscalerMode) {
        if self.mode != mode {
            self.mode = mode;
            self.reset();
        }
    }

    pub fn mode(&self) -> LumaUpscalerMode {
        self.mode
    }

    pub fn set_backend(&mut self, backend: UpscalerBackend) {
        if self.backend != backend {
            self.backend = backend;
            self.reset();
        }
    }

    pub fn backend(&self) -> UpscalerBackend {
        self.backend
    }

    /// Installs the network run by [`LumaUpscalerMode::Custom`].
    pub fn set_custom_model(&mut self, model: Option<Arc<SrModel>>) {
        let same = match (&self.custom_model, &model) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.custom_model = model;
            if self.mode == LumaUpscalerMode::Custom {
                self.reset();
            }
        }
    }

    pub fn active_backend(&self) -> LumaUpscalerBackendStatus {
        if self.mode == LumaUpscalerMode::Off {
            LumaUpscalerBackendStatus::Off
        } else {
            self.status
        }
    }

    /// Why the mode is `Inactive` (or runs on a downgraded kernel family).
    pub fn status_reason(&self) -> Option<&str> {
        if self.mode == LumaUpscalerMode::Off {
            return None;
        }
        self.reason.as_deref()
    }

    /// Kernel family label of the active network engine, if any.
    pub fn engine_label(&self) -> Option<&'static str> {
        match self.active.as_ref() {
            Some((_, Active::Network(engine))) => Some(engine.kind().label()),
            Some((_, Active::LegacyMatmul(_))) => Some("legacy_simdgroup_matrix"),
            Some((_, Active::LegacyScalar(_))) => Some("legacy_scalar"),
            Some((_, Active::Fsr(_))) => Some("fsr1"),
            Some((_, Active::MetalFx(_))) => Some("metalfx_spatial"),
            Some((_, Active::VideoToolbox(_))) => Some("vt_low_latency_super_resolution"),
            None => None,
        }
    }

    /// Builds or kernel families that failed and were replaced by native
    /// sampling or a simpler kernel family.
    pub fn auto_matmul_fallbacks(&self) -> u64 {
        self.fallbacks
    }

    pub fn allocated_bytes(&self) -> u64 {
        self.active
            .as_ref()
            .map_or(0, |(_, active)| active.allocated_bytes())
    }

    /// Records that the renderer sampled the frame natively without asking
    /// for an upscale (for example because the video is not shown larger
    /// than its source).
    pub fn note_skipped(&mut self, reason: &str) {
        if self.mode != LumaUpscalerMode::Off {
            if self.reason.as_deref() != Some(reason) {
                self.reason = Some(reason.to_string());
            }
            self.status = LumaUpscalerBackendStatus::Inactive;
        }
    }

    fn inactive(&mut self, reason: String) -> Result<Option<UpscaledFrame>> {
        if self.status != LumaUpscalerBackendStatus::Inactive
            || self.reason.as_deref() != Some(reason.as_str())
        {
            crate::trace::diagnostic_at(
                crate::trace::LogLevel::Info,
                format!(
                    "Erika luma upscaler {} inactive, sampling natively: {reason}",
                    self.mode.label()
                ),
            );
        }
        self.status = LumaUpscalerBackendStatus::Inactive;
        self.reason = Some(reason);
        Ok(None)
    }

    /// The target the current mode needs for a `luma` source.
    fn desired_target(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        luma: (usize, usize),
    ) -> std::result::Result<Target, String> {
        match self.mode.family() {
            UpscalerFamily::Off => Err("upscaler is off".to_string()),
            UpscalerFamily::Network => {
                let model = if self.mode == LumaUpscalerMode::Custom {
                    self.custom_model
                        .clone()
                        .ok_or_else(|| "no custom upscaler model is loaded".to_string())?
                } else {
                    match self.bundled_model.as_ref() {
                        Some((mode, model)) if *mode == self.mode => model.clone(),
                        _ => {
                            let model = catalog::execution_model(self.mode)
                                .map_err(|error| error.to_string())?;
                            self.bundled_model = Some((self.mode, model.clone()));
                            model
                        }
                    }
                };
                let legacy = self.mode.is_legacy_artcnn();
                let apple7 = device.supportsFamily(objc2_metal::MTLGPUFamily::Apple7);
                match self.backend.resolve() {
                    UpscalerBackend::Auto if legacy && AUTO_PREFERS_LEGACY_KERNELS => {
                        Ok(if apple7 {
                            Target::LegacyMatmul
                        } else {
                            Target::LegacyScalar
                        })
                    }
                    UpscalerBackend::Auto => {
                        Ok(Target::Network(EngineKind::preferred(device), model))
                    }
                    UpscalerBackend::Scalar => Ok(Target::Network(EngineKind::Scalar, model)),
                    UpscalerBackend::SimdgroupMatrix => {
                        Ok(Target::Network(EngineKind::Simdgroup, model))
                    }
                    UpscalerBackend::TensorOps => Ok(Target::Network(EngineKind::TensorOps, model)),
                    UpscalerBackend::LegacyScalar | UpscalerBackend::LegacySimdgroupMatrix
                        if !legacy =>
                    {
                        Err(format!(
                            "legacy ArtCNN kernels do not implement {}",
                            self.mode.label()
                        ))
                    }
                    UpscalerBackend::LegacyScalar => Ok(Target::LegacyScalar),
                    UpscalerBackend::LegacySimdgroupMatrix => Ok(Target::LegacyMatmul),
                }
            }
            UpscalerFamily::Spatial => Ok(Target::Fsr),
            UpscalerFamily::Platform => match self.mode {
                LumaUpscalerMode::MetalFxSpatial => Ok(Target::MetalFx),
                _ => Ok(Target::VideoToolbox(luma.0, luma.1)),
            },
        }
    }

    /// Polls (or starts) the background build of `target`. Returns `Ok(true)`
    /// once it is installed as the active backend.
    fn poll_build(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        target: &Target,
    ) -> Result<bool> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.target != *target)
        {
            // Stale build for a previous configuration; its result is dropped.
            self.pending = None;
        }
        if let Some(pending) = self.pending.as_ref() {
            let finished = pending.slot.lock().expect("build slot poisoned").take();
            return match finished {
                Some(Ok(active)) => {
                    self.pending = None;
                    self.active = Some((target.clone(), active.0));
                    Ok(true)
                }
                Some(Err(error)) => {
                    self.pending = None;
                    Err(error)
                }
                None => Ok(false),
            };
        }
        let slot: BuildSlot = Arc::new(Mutex::new(None));
        let thread_slot = Arc::clone(&slot);
        let thread_device = SendDevice(device.retain());
        let thread_target = target.clone();
        let mode = self.mode;
        // Model Arcs are Send + Sync; the target only carries plain data.
        struct SendTarget(Target);
        unsafe impl Send for SendTarget {}
        let thread_target = SendTarget(thread_target);
        std::thread::Builder::new()
            .name("erika-upscaler-build".to_string())
            .spawn(move || {
                let device = thread_device;
                let target = thread_target;
                let result = build_target(&device.0, mode, &target.0).map(SendActive);
                *thread_slot.lock().expect("build slot poisoned") = Some(result);
            })
            .map_err(|error| {
                PlayerError::Renderer(format!("upscaler build thread spawn failed: {error}"))
            })?;
        self.pending = Some(PendingBuild {
            target: target.clone(),
            slot,
        });
        Ok(false)
    }

    /// Builds the active configuration synchronously. Intended for tests and
    /// benchmarks; playback uses the background build instead. VideoToolbox
    /// sessions depend on the frame size and are started by the first frame.
    pub fn prepare_blocking(&mut self, device: &ProtocolObject<dyn MTLDevice>) -> Result<()> {
        if self.mode == LumaUpscalerMode::Off || self.mode == LumaUpscalerMode::PlatformVideo {
            return Ok(());
        }
        let target = self
            .desired_target(device, (0, 0))
            .map_err(PlayerError::Renderer)?;
        if self
            .active
            .as_ref()
            .is_some_and(|(active, _)| *active == target)
        {
            return Ok(());
        }
        self.pending = None;
        let active = build_target(device, self.mode, &target)?;
        self.active = Some((target, active));
        Ok(())
    }

    /// Encodes the 2x luma upscale onto `command_buffer` and returns the
    /// upscaled luma texture, or `Ok(None)` when the upscaler is off or not
    /// ready. Treats the luma as full range.
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
        let request = UpscaleRequest {
            luma,
            chroma: None,
            output_format,
            range: LumaRange::Full,
            presented: (luma.width() * 2, luma.height() * 2),
            frame_token,
        };
        Ok(self
            .encode_frame(device, command_buffer, &request)?
            .map(|frame| frame.luma))
    }

    /// Encodes the current mode for one frame. `Ok(None)` means the frame
    /// should be sampled natively (off, building, or inactive; see
    /// [`Self::active_backend`] and [`Self::status_reason`]).
    pub fn encode_frame(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        request: &UpscaleRequest<'_>,
    ) -> Result<Option<UpscaledFrame>> {
        if self.mode == LumaUpscalerMode::Off {
            self.status = LumaUpscalerBackendStatus::Off;
            return Ok(None);
        }
        let luma_size = (request.luma.width(), request.luma.height());
        let key = request.frame_token.map(|token| CacheKey {
            token,
            range: request.range,
            presented: request.presented,
            output_format: request.output_format,
            luma: luma_size,
        });
        if let (Some(key), Some((cached_key, frame))) = (key, self.cached.as_ref())
            && key == *cached_key
        {
            if let Some((_, active)) = self.active.as_ref() {
                self.status = active.status();
            }
            return Ok(Some(frame.clone()));
        }

        let target = match self.desired_target(device, luma_size) {
            Ok(target) => target,
            Err(reason) => return self.inactive(reason),
        };
        // Per-frame applicability.
        match &target {
            Target::Fsr | Target::MetalFx
                if request.presented.0 <= luma_size.0 && request.presented.1 <= luma_size.1 =>
            {
                return self
                    .inactive("the video is not presented above its source size".to_string());
            }
            Target::VideoToolbox(width, height) => {
                let formats_ok = request.luma.pixelFormat() == MTLPixelFormat::R8Unorm
                    && request
                        .chroma
                        .is_some_and(|chroma| chroma.pixelFormat() == MTLPixelFormat::RG8Unorm);
                if !formats_ok {
                    return self.inactive(
                        "VideoToolbox super resolution needs 8-bit 4:2:0 frames (no 10-bit or HDR)"
                            .to_string(),
                    );
                }
                let support = match self.video_toolbox_support.as_ref() {
                    Some((size, support)) if *size == (*width, *height) => support.clone(),
                    _ => {
                        let support = video_toolbox::unsupported_reason(*width, *height);
                        self.video_toolbox_support = Some(((*width, *height), support.clone()));
                        support
                    }
                };
                if let Some(reason) = support {
                    return self.inactive(reason);
                }
            }
            _ => {}
        }
        if let Some((failed, reason)) = self.failed.as_ref()
            && *failed == target
        {
            let reason = reason.clone();
            return self.inactive(reason);
        }
        if !self
            .active
            .as_ref()
            .is_some_and(|(active, _)| *active == target)
        {
            let ready = if target == Target::MetalFx {
                // Cheap to create; depends on the frame size anyway.
                match build_target(device, self.mode, &target) {
                    Ok(active) => {
                        self.active = Some((target.clone(), active));
                        Ok(true)
                    }
                    Err(error) => Err(error),
                }
            } else {
                self.poll_build(device, &target)
            };
            match ready {
                Ok(true) => {}
                Ok(false) => {
                    self.status = LumaUpscalerBackendStatus::Building;
                    self.reason = None;
                    return Ok(None);
                }
                Err(error) => {
                    self.fallbacks = self.fallbacks.saturating_add(1);
                    crate::trace::diagnostic_at(
                        crate::trace::LogLevel::Warn,
                        format!(
                            "Erika luma upscaler {} unavailable, sampling natively: {error}",
                            self.mode.label()
                        ),
                    );
                    let reason = error.to_string();
                    self.failed = Some((target, reason.clone()));
                    return self.inactive(reason);
                }
            }
        }

        let (_, active) = self.active.as_mut().expect("active backend installed");
        let output = match active {
            Active::LegacyScalar(resources) => encode_legacy_scalar(
                resources,
                device,
                command_buffer,
                request.luma,
                request.output_format,
            )
            .map(|luma| UpscaledFrame { luma, chroma: None }),
            Active::LegacyMatmul(resources) => matmul::encode(
                resources,
                device,
                command_buffer,
                request.luma,
                request.output_format,
                None,
            )
            .map(|luma| UpscaledFrame { luma, chroma: None }),
            Active::Network(engine) => engine
                .encode(
                    device,
                    command_buffer,
                    request.luma,
                    request.output_format,
                    request.range,
                )
                .map(|luma| UpscaledFrame { luma, chroma: None }),
            Active::Fsr(fsr) => fsr
                .encode(
                    device,
                    command_buffer,
                    request.luma,
                    request.output_format,
                    request.presented.0,
                    request.presented.1,
                )
                .map(|luma| UpscaledFrame { luma, chroma: None }),
            Active::MetalFx(scaler) => scaler
                .encode(
                    device,
                    command_buffer,
                    request.luma,
                    request.presented.0,
                    request.presented.1,
                )
                .map(|luma| UpscaledFrame { luma, chroma: None }),
            Active::VideoToolbox(scaler) => scaler
                .encode(
                    device,
                    command_buffer,
                    request.luma,
                    request.chroma.expect("checked above"),
                )
                .map(|(luma, chroma)| UpscaledFrame {
                    luma,
                    chroma: Some(chroma),
                }),
        };
        match output {
            Ok(frame) => {
                self.status = active.status();
                self.reason = match active {
                    Active::Network(engine) => engine.downgrade_reason().map(str::to_string),
                    _ => None,
                };
                self.cached = key.map(|key| (key, frame.clone()));
                Ok(Some(frame))
            }
            Err(error) => {
                self.fallbacks = self.fallbacks.saturating_add(1);
                crate::trace::diagnostic_at(
                    crate::trace::LogLevel::Warn,
                    format!(
                        "Erika luma upscaler {} failed, sampling natively: {error}",
                        self.mode.label()
                    ),
                );
                let reason = error.to_string();
                self.active = None;
                self.cached = None;
                self.failed = Some((target, reason.clone()));
                self.inactive(reason)
            }
        }
    }
}

fn encode_legacy_scalar(
    resources: &mut UpscalerResources,
    device: &ProtocolObject<dyn MTLDevice>,
    command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    luma: &ProtocolObject<dyn MTLTexture>,
    output_format: MTLPixelFormat,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
    let width = luma.width();
    let height = luma.height();
    ensure_pool(resources, device, width, height, output_format)?;
    let pool = resources.pool.as_ref().expect("pool built above");

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
    Ok(output)
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
