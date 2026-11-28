//! Portable wgpu ArtCNN C4 luma upscaler.
//!
//! The implementation is spatially tiled. Keeping three full-frame F-channel
//! feature maps would cost roughly 190/380 MiB at 1080p and 759/1519 MiB at 4K
//! for C4F16/C4F32. Instead, the seven convolutions run to completion for one
//! bounded tile before the same three feature textures are reused for the next
//! tile. The final DepthToSpace result is packed into one source-sized
//! `Rgba16Float` texture: RGBA stores TL/TR/BL/BR luma, respectively.

use std::borrow::Cow;
use std::fmt;
use std::mem;
use std::num::NonZeroU64;
use std::time::{Duration, Instant};

use wgpu::util::DeviceExt;

use crate::renderer::artcnn::{
    ArtCnnModelLayout, FrameTokenCache, LayerOffsets, MIDDLE_LAYER_COUNT, model_for_mode,
};
use crate::renderer::pipeline::LumaUpscalerMode;

const SHADER_TEMPLATE: &str = include_str!("wgpu_artcnn.wgsl");

const NETWORK_RADIUS: u32 = 7;
const FEATURE_HALO: u32 = NETWORK_RADIUS - 1;
const WORKGROUP_X: u32 = 8;
const WORKGROUP_Y: u32 = 8;

fn mode_label(mode: LumaUpscalerMode) -> &'static str {
    mode.label()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WgpuArtCnnStatus {
    #[default]
    Off,
    Building,
    Inactive,
    Scalar,
}

/// Input representation consumed by conv0.
///
/// `NonlinearRgb` is the Android AHardwareBuffer path after native Vulkan YCbCr
/// conversion. ArtCNN operates on reconstructed nonlinear Y; the presentation
/// shader can preserve chroma with `rgb + (Y_sr - dot(rgb, coefficients))`.
#[derive(Debug, Clone, Copy)]
pub enum WgpuArtCnnInput<'a> {
    PlanarLuma {
        view: &'a wgpu::TextureView,
        p010_le_bytes: bool,
    },
    NonlinearRgb {
        view: &'a wgpu::TextureView,
        luma_coefficients: [f32; 3],
    },
}

impl WgpuArtCnnInput<'_> {
    pub const fn kind(self) -> WgpuArtCnnInputKind {
        match self {
            Self::PlanarLuma { .. } => WgpuArtCnnInputKind::PlanarLuma,
            Self::NonlinearRgb { .. } => WgpuArtCnnInputKind::NonlinearRgb,
        }
    }

    pub(crate) fn luma_coefficients(self) -> [f32; 3] {
        match self {
            Self::PlanarLuma {
                p010_le_bytes: true,
                ..
            } => [1.0 / 257.0, 256.0 / 257.0, 0.0],
            Self::PlanarLuma {
                p010_le_bytes: false,
                ..
            } => [1.0, 0.0, 0.0],
            Self::NonlinearRgb {
                luma_coefficients, ..
            } => luma_coefficients,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WgpuArtCnnInputKind {
    PlanarLuma,
    NonlinearRgb,
}

impl WgpuArtCnnInputKind {
    const fn label(self) -> &'static str {
        match self {
            Self::PlanarLuma => "planar_luma",
            Self::NonlinearRgb => "nonlinear_rgb",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WgpuArtCnnConfig {
    pub tile_width: u32,
    pub tile_height: u32,
}

impl Default for WgpuArtCnnConfig {
    fn default() -> Self {
        Self {
            tile_width: 512,
            tile_height: 256,
        }
    }
}

/// Adapter/device facts used to decide whether SR can be enabled without
/// endangering ordinary video presentation.
#[derive(Debug, Clone)]
pub struct WgpuArtCnnCapability {
    pub supported: bool,
    pub adapter_name: String,
    pub backend: wgpu::Backend,
    pub compute_shaders: bool,
    pub rgba16float_usages: wgpu::TextureUsages,
    pub max_texture_dimension_2d: u32,
    pub max_texture_array_layers: u32,
    pub max_storage_buffer_binding_size: u64,
    pub max_buffer_size: u64,
    pub max_storage_buffers_per_shader_stage: u32,
    pub max_storage_textures_per_shader_stage: u32,
    pub max_sampled_textures_per_shader_stage: u32,
    pub max_dynamic_uniform_buffers_per_pipeline_layout: u32,
    pub max_compute_invocations_per_workgroup: u32,
    pub max_compute_workgroup_size_x: u32,
    pub max_compute_workgroup_size_y: u32,
    pub max_compute_workgroups_per_dimension: u32,
    pub min_uniform_buffer_offset_alignment: u32,
    pub reasons: Vec<String>,
}

impl WgpuArtCnnCapability {
    pub fn inspect(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Self {
        let info = adapter.get_info();
        let downlevel = adapter.get_downlevel_capabilities();
        let limits = device.limits();
        let format = adapter.get_texture_format_features(wgpu::TextureFormat::Rgba16Float);
        let required_format_usages = wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::COPY_SRC;
        let compute_shaders = downlevel
            .flags
            .contains(wgpu::DownlevelFlags::COMPUTE_SHADERS);
        let mut reasons = Vec::new();
        if !compute_shaders {
            reasons.push(format!(
                "adapter downlevel flags {:?} do not include COMPUTE_SHADERS",
                downlevel.flags
            ));
        }
        if !format.allowed_usages.contains(required_format_usages) {
            reasons.push(format!(
                "Rgba16Float usages {:?} do not include {:?}",
                format.allowed_usages, required_format_usages
            ));
        }
        if limits.max_bind_groups < 2 {
            reasons.push(format!(
                "max_bind_groups={} is below 2",
                limits.max_bind_groups
            ));
        }
        if limits.max_storage_buffers_per_shader_stage < 1 {
            reasons.push(format!(
                "max_storage_buffers_per_shader_stage={} is below 1",
                limits.max_storage_buffers_per_shader_stage
            ));
        }
        if limits.max_storage_textures_per_shader_stage < 1 {
            reasons.push(format!(
                "max_storage_textures_per_shader_stage={} is below 1",
                limits.max_storage_textures_per_shader_stage
            ));
        }
        if limits.max_sampled_textures_per_shader_stage < 2 {
            reasons.push(format!(
                "max_sampled_textures_per_shader_stage={} is below 2",
                limits.max_sampled_textures_per_shader_stage
            ));
        }
        if limits.max_dynamic_uniform_buffers_per_pipeline_layout < 1 {
            reasons.push(format!(
                "max_dynamic_uniform_buffers_per_pipeline_layout={} is below 1",
                limits.max_dynamic_uniform_buffers_per_pipeline_layout
            ));
        }
        if limits.max_uniform_buffer_binding_size < mem::size_of::<TileParams>() as u64 {
            reasons.push(format!(
                "max_uniform_buffer_binding_size={} is below TileParams size {}",
                limits.max_uniform_buffer_binding_size,
                mem::size_of::<TileParams>()
            ));
        }
        if limits.max_compute_invocations_per_workgroup < WORKGROUP_X * WORKGROUP_Y {
            reasons.push(format!(
                "max_compute_invocations_per_workgroup={} is below {}",
                limits.max_compute_invocations_per_workgroup,
                WORKGROUP_X * WORKGROUP_Y
            ));
        }
        if limits.max_compute_workgroup_size_x < WORKGROUP_X
            || limits.max_compute_workgroup_size_y < WORKGROUP_Y
        {
            reasons.push(format!(
                "max compute workgroup size {}x{} is below {}x{}",
                limits.max_compute_workgroup_size_x,
                limits.max_compute_workgroup_size_y,
                WORKGROUP_X,
                WORKGROUP_Y
            ));
        }
        if limits.max_compute_workgroups_per_dimension == 0 {
            reasons.push("max_compute_workgroups_per_dimension is zero".to_string());
        }

        Self {
            supported: reasons.is_empty(),
            adapter_name: info.name,
            backend: info.backend,
            compute_shaders,
            rgba16float_usages: format.allowed_usages,
            max_texture_dimension_2d: limits.max_texture_dimension_2d,
            max_texture_array_layers: limits.max_texture_array_layers,
            max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
            max_buffer_size: limits.max_buffer_size,
            max_storage_buffers_per_shader_stage: limits.max_storage_buffers_per_shader_stage,
            max_storage_textures_per_shader_stage: limits.max_storage_textures_per_shader_stage,
            max_sampled_textures_per_shader_stage: limits.max_sampled_textures_per_shader_stage,
            max_dynamic_uniform_buffers_per_pipeline_layout: limits
                .max_dynamic_uniform_buffers_per_pipeline_layout,
            max_compute_invocations_per_workgroup: limits.max_compute_invocations_per_workgroup,
            max_compute_workgroup_size_x: limits.max_compute_workgroup_size_x,
            max_compute_workgroup_size_y: limits.max_compute_workgroup_size_y,
            max_compute_workgroups_per_dimension: limits.max_compute_workgroups_per_dimension,
            min_uniform_buffer_offset_alignment: limits.min_uniform_buffer_offset_alignment.max(1),
            reasons,
        }
    }

    fn mode_reasons(&self, mode: LumaUpscalerMode, config: WgpuArtCnnConfig) -> Vec<String> {
        let mut reasons = self.reasons.clone();
        if !mode.is_enabled() {
            return reasons;
        }
        if !mode.is_legacy_artcnn() {
            reasons.push(format!(
                "upscaler mode {} is not implemented by the wgpu backend",
                mode.label()
            ));
            return reasons;
        }
        if config.tile_width == 0 || config.tile_height == 0 {
            reasons.push("tile dimensions must be non-zero".to_string());
            return reasons;
        }
        let feature_width = config.tile_width.saturating_add(FEATURE_HALO * 2);
        let feature_height = config.tile_height.saturating_add(FEATURE_HALO * 2);
        if feature_width > self.max_texture_dimension_2d
            || feature_height > self.max_texture_dimension_2d
        {
            reasons.push(format!(
                "tile feature extent {feature_width}x{feature_height} exceeds max_texture_dimension_2d={}",
                self.max_texture_dimension_2d
            ));
        }
        let Some(layout) = ArtCnnModelLayout::for_mode(mode) else {
            return reasons;
        };
        let slices = layout.feature_slices;
        if slices > self.max_texture_array_layers {
            reasons.push(format!(
                "feature array layers {slices} exceed max_texture_array_layers={}",
                self.max_texture_array_layers
            ));
        }
        let workgroups_x = feature_width.div_ceil(WORKGROUP_X);
        let workgroups_y = feature_height.div_ceil(WORKGROUP_Y);
        if workgroups_x > self.max_compute_workgroups_per_dimension
            || workgroups_y > self.max_compute_workgroups_per_dimension
        {
            reasons.push(format!(
                "tile dispatch {workgroups_x}x{workgroups_y} exceeds max_compute_workgroups_per_dimension={}",
                self.max_compute_workgroups_per_dimension
            ));
        }
        if let Ok(model) = model_for_mode(mode) {
            let payload_len = model.payload.len() as u64;
            if payload_len > self.max_storage_buffer_binding_size {
                reasons.push(format!(
                    "model payload {payload_len} exceeds max_storage_buffer_binding_size={}",
                    self.max_storage_buffer_binding_size
                ));
            }
            if payload_len > self.max_buffer_size {
                reasons.push(format!(
                    "model payload {payload_len} exceeds max_buffer_size={}",
                    self.max_buffer_size
                ));
            }
        }
        reasons
    }

    pub fn diagnostic_json(&self, mode: LumaUpscalerMode) -> serde_json::Value {
        serde_json::json!({
            "event": "luma_upscaler",
            "stage": if self.supported { "capability_supported" } else { "capability_unsupported" },
            "renderer": "wgpu",
            "requestedMode": mode_label(mode),
            "adapter": self.adapter_name,
            "backend": format!("{:?}", self.backend),
            "computeShaders": self.compute_shaders,
            "rgba16FloatUsages": format!("{:?}", self.rgba16float_usages),
            "maxTextureDimension2D": self.max_texture_dimension_2d,
            "maxTextureArrayLayers": self.max_texture_array_layers,
            "maxStorageBufferBindingSize": self.max_storage_buffer_binding_size,
            "maxStorageBuffersPerShaderStage": self.max_storage_buffers_per_shader_stage,
            "maxStorageTexturesPerShaderStage": self.max_storage_textures_per_shader_stage,
            "maxComputeInvocationsPerWorkgroup": self.max_compute_invocations_per_workgroup,
            "maxComputeWorkgroupSize": [self.max_compute_workgroup_size_x, self.max_compute_workgroup_size_y],
            "reasons": self.reasons,
            "fallback": if self.supported { serde_json::Value::Null } else { serde_json::Value::String("native_luma_sampling".to_string()) },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WgpuArtCnnFailureStage {
    Capability,
    Blob,
    Pipeline,
    Frame,
    Resource,
    Encode,
}

impl WgpuArtCnnFailureStage {
    const fn label(self) -> &'static str {
        match self {
            Self::Capability => "capability",
            Self::Blob => "blob",
            Self::Pipeline => "pipeline",
            Self::Frame => "frame",
            Self::Resource => "resource",
            Self::Encode => "encode",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WgpuArtCnnFailure {
    pub stage: WgpuArtCnnFailureStage,
    pub mode: LumaUpscalerMode,
    pub input_kind: Option<WgpuArtCnnInputKind>,
    pub reason: String,
    pub recoverable: bool,
    pub fallback: &'static str,
}

impl WgpuArtCnnFailure {
    pub(crate) fn new(
        stage: WgpuArtCnnFailureStage,
        mode: LumaUpscalerMode,
        input_kind: Option<WgpuArtCnnInputKind>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            stage,
            mode,
            input_kind,
            reason: reason.into(),
            recoverable: true,
            fallback: "native_luma_sampling",
        }
    }

    pub fn diagnostic_json(&self) -> serde_json::Value {
        serde_json::json!({
            "event": "luma_upscaler",
            "stage": self.stage.label(),
            "renderer": "wgpu",
            "requestedMode": mode_label(self.mode),
            "inputKind": self.input_kind.map(WgpuArtCnnInputKind::label),
            "reason": self.reason,
            "recoverable": self.recoverable,
            "fallback": self.fallback,
        })
    }
}

impl fmt::Display for WgpuArtCnnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "wgpu ArtCNN {} failed at {}: {}",
            mode_label(self.mode),
            self.stage.label(),
            self.reason
        )
    }
}

impl std::error::Error for WgpuArtCnnFailure {}

#[derive(Debug, Clone, Copy, Default)]
pub struct WgpuArtCnnStats {
    pub upscaled_frames: u64,
    pub cache_hits: u64,
    pub fallback_count: u64,
    pub encoded_tiles: u64,
    pub compute_dispatches: u64,
    pub last_encode_duration: Duration,
}

#[derive(Debug, Clone)]
pub struct WgpuArtCnnOutput {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub packed_width: u32,
    pub packed_height: u32,
    pub virtual_width: u32,
    pub virtual_height: u32,
    pub input_kind: WgpuArtCnnInputKind,
    pub frame_token: Option<u64>,
    pub cache_hit: bool,
    tentative_serial: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct PendingEncode {
    serial: u64,
    frame_token: Option<u64>,
    input_kind: WgpuArtCnnInputKind,
    width: u32,
    height: u32,
    encoded_tiles: u64,
    compute_dispatches: u64,
    encode_duration: Duration,
}

#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TileParams {
    image_core: [u32; 4],
    origins: [i32; 4],
    dispatch: [u32; 4],
    layer: [u32; 4],
    luma_coefficients: [f32; 4],
}

struct ModelResources {
    mode: LumaUpscalerMode,
    slices: u32,
    offsets: LayerOffsets,
    weights: wgpu::Buffer,
    common_layout: wgpu::BindGroupLayout,
    conv0_luma_layout: wgpu::BindGroupLayout,
    conv0_rgb_layout: wgpu::BindGroupLayout,
    mid_layout: wgpu::BindGroupLayout,
    conv6_layout: wgpu::BindGroupLayout,
    conv0_luma_pipeline: wgpu::ComputePipeline,
    conv0_rgb_pipeline: wgpu::ComputePipeline,
    mid_pipeline: wgpu::ComputePipeline,
    conv6_pipeline: wgpu::ComputePipeline,
}

impl ModelResources {
    fn build(device: &wgpu::Device, mode: LumaUpscalerMode) -> Result<Self, WgpuArtCnnFailure> {
        let model = model_for_mode(mode).map_err(|error| {
            WgpuArtCnnFailure::new(WgpuArtCnnFailureStage::Blob, mode, None, error.to_string())
        })?;
        let weight_words: Vec<u32> = model
            .payload
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("four-byte chunk")))
            .collect();
        let slices = model.layout.feature_slices;
        let offsets = model.layout.layer_offsets;
        with_device_error_scopes(device, mode, WgpuArtCnnFailureStage::Pipeline, || {
            let weights = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("erika-wgpu-artcnn-weights"),
                contents: bytemuck::cast_slice(&weight_words),
                usage: wgpu::BufferUsages::STORAGE,
            });

            let common_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("erika-wgpu-artcnn-common-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: NonZeroU64::new((weight_words.len() * 4) as u64),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: NonZeroU64::new(mem::size_of::<TileParams>() as u64),
                        },
                        count: None,
                    },
                ],
            });
            let sampled_2d = wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            };
            let sampled_array = wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            };
            let storage_array = wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: wgpu::TextureFormat::Rgba16Float,
                view_dimension: wgpu::TextureViewDimension::D2Array,
            };
            let storage_2d = wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: wgpu::TextureFormat::Rgba16Float,
                view_dimension: wgpu::TextureViewDimension::D2,
            };

            let conv0_luma_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("erika-wgpu-artcnn-conv0-luma-layout"),
                    entries: &[
                        bind_group_layout_entry(0, sampled_2d),
                        bind_group_layout_entry(1, storage_array),
                    ],
                });
            let conv0_rgb_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("erika-wgpu-artcnn-conv0-rgb-layout"),
                    entries: &[
                        bind_group_layout_entry(1, storage_array),
                        bind_group_layout_entry(7, sampled_2d),
                    ],
                });
            let mid_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("erika-wgpu-artcnn-mid-layout"),
                entries: &[
                    bind_group_layout_entry(2, sampled_array),
                    bind_group_layout_entry(3, storage_array),
                    bind_group_layout_entry(4, sampled_array),
                ],
            });
            let conv6_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("erika-wgpu-artcnn-conv6-layout"),
                entries: &[
                    bind_group_layout_entry(5, sampled_array),
                    bind_group_layout_entry(6, storage_2d),
                ],
            });

            let source = SHADER_TEMPLATE.replace("{FEATURE_SLICES}", &slices.to_string());
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("erika-wgpu-artcnn-shader"),
                source: wgpu::ShaderSource::Wgsl(Cow::Owned(source)),
            });
            let pipeline = |label: &'static str,
                            entry_point: &'static str,
                            resource_layout: &wgpu::BindGroupLayout| {
                let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some(label),
                    bind_group_layouts: &[Some(resource_layout), Some(&common_layout)],
                    immediate_size: 0,
                });
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(label),
                    layout: Some(&layout),
                    module: &shader,
                    entry_point: Some(entry_point),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    cache: None,
                })
            };
            let conv0_luma_pipeline = pipeline(
                "erika-wgpu-artcnn-conv0-luma-pipeline",
                "artcnn_conv0",
                &conv0_luma_layout,
            );
            let conv0_rgb_pipeline = pipeline(
                "erika-wgpu-artcnn-conv0-rgb-pipeline",
                "artcnn_conv0_rgb",
                &conv0_rgb_layout,
            );
            let mid_pipeline = pipeline(
                "erika-wgpu-artcnn-mid-pipeline",
                "artcnn_conv_mid",
                &mid_layout,
            );
            let conv6_pipeline = pipeline(
                "erika-wgpu-artcnn-conv6-pipeline",
                "artcnn_conv6",
                &conv6_layout,
            );

            Self {
                mode,
                slices,
                offsets,
                weights,
                common_layout,
                conv0_luma_layout,
                conv0_rgb_layout,
                mid_layout,
                conv6_layout,
                conv0_luma_pipeline,
                conv0_rgb_pipeline,
                mid_pipeline,
                conv6_pipeline,
            }
        })
    }
}

fn bind_group_layout_entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    }
}

fn with_device_error_scopes<T>(
    device: &wgpu::Device,
    mode: LumaUpscalerMode,
    stage: WgpuArtCnnFailureStage,
    operation: impl FnOnce() -> T,
) -> Result<T, WgpuArtCnnFailure> {
    let out_of_memory_scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let internal_scope = device.push_error_scope(wgpu::ErrorFilter::Internal);
    let validation_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let value = operation();
    let validation = pollster::block_on(validation_scope.pop());
    let internal = pollster::block_on(internal_scope.pop());
    let out_of_memory = pollster::block_on(out_of_memory_scope.pop());
    if let Some(error) = validation.or(internal).or(out_of_memory) {
        return Err(WgpuArtCnnFailure::new(stage, mode, None, error.to_string()));
    }
    Ok(value)
}

struct TexturePool {
    width: u32,
    height: u32,
    _feature_textures: [wgpu::Texture; 3],
    feature_views: [wgpu::TextureView; 3],
    output_texture: wgpu::Texture,
    output_view: wgpu::TextureView,
    frame_cache: FrameTokenCache,
    cached_input_kind: Option<WgpuArtCnnInputKind>,
}

impl TexturePool {
    fn build(
        device: &wgpu::Device,
        mode: LumaUpscalerMode,
        slices: u32,
        config: WgpuArtCnnConfig,
        width: u32,
        height: u32,
    ) -> Result<Self, WgpuArtCnnFailure> {
        with_device_error_scopes(device, mode, WgpuArtCnnFailureStage::Resource, || {
            let feature_size = wgpu::Extent3d {
                width: config.tile_width + FEATURE_HALO * 2,
                height: config.tile_height + FEATURE_HALO * 2,
                depth_or_array_layers: slices,
            };
            let feature = |label| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: feature_size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba16Float,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::STORAGE_BINDING,
                    view_formats: &[],
                })
            };
            let feature_textures = [
                feature("erika-wgpu-artcnn-feature-a"),
                feature("erika-wgpu-artcnn-feature-b"),
                feature("erika-wgpu-artcnn-feature-c"),
            ];
            let feature_views = feature_textures.each_ref().map(|texture| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("erika-wgpu-artcnn-feature-view"),
                    dimension: Some(wgpu::TextureViewDimension::D2Array),
                    ..Default::default()
                })
            });
            let output_texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("erika-wgpu-artcnn-packed-d2s-output"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let output_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());
            Self {
                width,
                height,
                _feature_textures: feature_textures,
                feature_views,
                output_texture,
                output_view,
                frame_cache: FrameTokenCache::default(),
                cached_input_kind: None,
            }
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum DispatchKind {
    Conv0,
    Mid(usize),
    Conv6,
}

#[derive(Debug, Clone, Copy)]
struct DispatchRecord {
    kind: DispatchKind,
    params: TileParams,
}

/// Stateful ArtCNN model, resource pool and frame-token cache.
pub struct WgpuArtCnn {
    config: WgpuArtCnnConfig,
    capability: WgpuArtCnnCapability,
    mode: LumaUpscalerMode,
    status: WgpuArtCnnStatus,
    resources: Option<ModelResources>,
    pool: Option<TexturePool>,
    next_tentative_serial: u64,
    pending_encode: Option<PendingEncode>,
    last_failure: Option<WgpuArtCnnFailure>,
    stats: WgpuArtCnnStats,
    timestamps: Option<wgpu::QuerySet>,
}

impl WgpuArtCnn {
    pub fn new(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Self {
        Self::new_with_config(adapter, device, WgpuArtCnnConfig::default())
    }

    pub fn new_with_config(
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        config: WgpuArtCnnConfig,
    ) -> Self {
        Self {
            config,
            capability: WgpuArtCnnCapability::inspect(adapter, device),
            mode: LumaUpscalerMode::Off,
            status: WgpuArtCnnStatus::Off,
            resources: None,
            pool: None,
            next_tentative_serial: 0,
            pending_encode: None,
            last_failure: None,
            stats: WgpuArtCnnStats::default(),
            timestamps: None,
        }
    }

    pub fn capability(&self) -> &WgpuArtCnnCapability {
        &self.capability
    }

    pub fn mode(&self) -> LumaUpscalerMode {
        self.mode
    }

    pub fn status(&self) -> WgpuArtCnnStatus {
        self.status
    }

    pub fn stats(&self) -> WgpuArtCnnStats {
        self.stats
    }

    /// Optional timestamps for measuring the retained legacy baseline.
    pub fn set_timestamp_queries(&mut self, queries: Option<wgpu::QuerySet>) {
        self.timestamps = queries;
    }

    pub fn last_failure(&self) -> Option<&WgpuArtCnnFailure> {
        self.last_failure.as_ref()
    }

    /// Commits the frame-token cache and success statistics after the caller
    /// has finished validation and successfully submitted the command buffer.
    /// Cache-hit outputs are already committed and make this a no-op.
    pub fn commit_encoded_output(
        &mut self,
        output: &WgpuArtCnnOutput,
    ) -> Result<(), WgpuArtCnnFailure> {
        if output.cache_hit {
            return Ok(());
        }
        let serial = output.tentative_serial.ok_or_else(|| {
            WgpuArtCnnFailure::new(
                WgpuArtCnnFailureStage::Encode,
                self.mode,
                Some(output.input_kind),
                "non-cached ArtCNN output has no tentative encode serial",
            )
        })?;
        let pending = self.pending_encode.ok_or_else(|| {
            WgpuArtCnnFailure::new(
                WgpuArtCnnFailureStage::Encode,
                self.mode,
                Some(output.input_kind),
                format!("tentative ArtCNN encode {serial} is no longer pending"),
            )
        })?;
        if pending.serial != serial
            || pending.frame_token != output.frame_token
            || pending.input_kind != output.input_kind
            || pending.width != output.packed_width
            || pending.height != output.packed_height
        {
            return Err(WgpuArtCnnFailure::new(
                WgpuArtCnnFailureStage::Encode,
                self.mode,
                Some(output.input_kind),
                format!(
                    "tentative ArtCNN output does not match pending encode {}",
                    pending.serial
                ),
            ));
        }
        let pool = self.pool.as_mut().ok_or_else(|| {
            WgpuArtCnnFailure::new(
                WgpuArtCnnFailureStage::Encode,
                self.mode,
                Some(output.input_kind),
                "ArtCNN texture pool disappeared before encode commit",
            )
        })?;
        if pool.width != pending.width || pool.height != pending.height {
            return Err(WgpuArtCnnFailure::new(
                WgpuArtCnnFailureStage::Encode,
                self.mode,
                Some(output.input_kind),
                "ArtCNN texture pool changed before encode commit",
            ));
        }

        self.pending_encode = None;
        pool.frame_cache.commit(pending.frame_token);
        pool.cached_input_kind = Some(pending.input_kind);
        self.stats.upscaled_frames = self.stats.upscaled_frames.saturating_add(1);
        self.stats.encoded_tiles = self
            .stats
            .encoded_tiles
            .saturating_add(pending.encoded_tiles);
        self.stats.compute_dispatches = self
            .stats
            .compute_dispatches
            .saturating_add(pending.compute_dispatches);
        self.stats.last_encode_duration = pending.encode_duration;
        Ok(())
    }

    /// Records a validation/internal/OOM error observed after the caller
    /// finishes the command encoder. wgpu can defer compute-pass validation
    /// until `CommandEncoder::finish`; invalidating here discards the pending
    /// encode so a failed output can never enter the frame-token cache.
    pub fn handle_deferred_encode_failure(
        &mut self,
        input_kind: WgpuArtCnnInputKind,
        reason: impl Into<String>,
    ) -> WgpuArtCnnFailure {
        self.pending_encode = None;
        if let Some(pool) = self.pool.as_mut() {
            pool.frame_cache.invalidate();
            pool.cached_input_kind = None;
        }
        let failure = WgpuArtCnnFailure::new(
            WgpuArtCnnFailureStage::Encode,
            self.mode,
            Some(input_kind),
            reason,
        );
        self.record_failure(failure.clone());
        failure
    }

    pub fn set_mode(
        &mut self,
        device: &wgpu::Device,
        mode: LumaUpscalerMode,
    ) -> Result<(), WgpuArtCnnFailure> {
        let discarded_pending = self.pending_encode.is_some();
        self.pending_encode = None;
        if discarded_pending {
            if let Some(pool) = self.pool.as_mut() {
                pool.frame_cache.invalidate();
                pool.cached_input_kind = None;
            }
        }
        if mode == self.mode
            && (mode == LumaUpscalerMode::Off || self.status == WgpuArtCnnStatus::Scalar)
        {
            return Ok(());
        }
        self.mode = mode;
        self.resources = None;
        self.pool = None;
        self.last_failure = None;
        if mode == LumaUpscalerMode::Off {
            self.status = WgpuArtCnnStatus::Off;
            return Ok(());
        }

        self.status = WgpuArtCnnStatus::Building;
        let reasons = self.capability.mode_reasons(mode, self.config);
        if !reasons.is_empty() {
            let failure = WgpuArtCnnFailure::new(
                WgpuArtCnnFailureStage::Capability,
                mode,
                None,
                reasons.join("; "),
            );
            self.record_failure(failure.clone());
            return Err(failure);
        }
        match ModelResources::build(device, mode) {
            Ok(resources) => {
                self.resources = Some(resources);
                self.status = WgpuArtCnnStatus::Scalar;
                Ok(())
            }
            Err(failure) => {
                self.record_failure(failure.clone());
                Err(failure)
            }
        }
    }

    /// Encodes ArtCNN passes into `encoder` and returns the source-sized packed
    /// D2S texture. `frame_token` must uniquely identify an uploaded frame; a
    /// matching token returns the cached texture without dispatching the network.
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        input: WgpuArtCnnInput<'_>,
        width: u32,
        height: u32,
        frame_token: Option<u64>,
    ) -> Result<Option<WgpuArtCnnOutput>, WgpuArtCnnFailure> {
        if self.mode == LumaUpscalerMode::Off {
            return Ok(None);
        }
        if let Some(pending) = self.pending_encode {
            let failure = WgpuArtCnnFailure::new(
                WgpuArtCnnFailureStage::Encode,
                self.mode,
                Some(input.kind()),
                format!(
                    "tentative ArtCNN encode {} must be committed or failed before encoding another frame",
                    pending.serial
                ),
            );
            self.record_runtime_failure(failure.clone());
            return Err(failure);
        }
        if self.status != WgpuArtCnnStatus::Scalar {
            let failure = self.last_failure.clone().unwrap_or_else(|| {
                WgpuArtCnnFailure::new(
                    WgpuArtCnnFailureStage::Encode,
                    self.mode,
                    Some(input.kind()),
                    format!("upscaler status is {:?}", self.status),
                )
            });
            return Err(failure);
        }
        if let Err(failure) = self.validate_frame(input, width, height) {
            self.record_runtime_failure(failure.clone());
            return Err(failure);
        }

        let resources = self
            .resources
            .as_ref()
            .expect("scalar status has resources");
        debug_assert_eq!(resources.mode, self.mode);
        let rebuild_pool = self
            .pool
            .as_ref()
            .is_none_or(|pool| pool.width != width || pool.height != height);
        if rebuild_pool {
            match TexturePool::build(
                device,
                self.mode,
                resources.slices,
                self.config,
                width,
                height,
            ) {
                Ok(pool) => self.pool = Some(pool),
                Err(failure) => {
                    self.record_runtime_failure(failure.clone());
                    return Err(failure);
                }
            }
        }

        if self
            .pool
            .as_ref()
            .is_some_and(|pool| pool.frame_cache.matches(frame_token))
        {
            self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
            let pool = self.pool.as_ref().expect("pool built above");
            return Ok(Some(output_from_pool(
                pool,
                pool.cached_input_kind.unwrap_or(input.kind()),
                frame_token,
                true,
                None,
            )));
        }

        let started = Instant::now();
        let records = build_dispatch_records(
            width,
            height,
            self.config,
            &resources.offsets,
            resources.slices,
            input.luma_coefficients(),
        );
        let parameter_stride = align_up(
            mem::size_of::<TileParams>() as u64,
            u64::from(self.capability.min_uniform_buffer_offset_alignment),
        );
        let parameter_bytes = encode_parameter_records(
            &records,
            parameter_stride,
            self.capability.max_buffer_size,
            self.mode,
            input.kind(),
        )?;
        let parameter_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("erika-wgpu-artcnn-tile-params"),
            contents: &parameter_bytes,
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let pool = self.pool.as_ref().expect("pool built above");
        let common_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("erika-wgpu-artcnn-common-bind-group"),
            layout: &resources.common_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: resources.weights.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &parameter_buffer,
                        offset: 0,
                        size: NonZeroU64::new(mem::size_of::<TileParams>() as u64),
                    }),
                },
            ],
        });
        let conv0_bind_group = match input {
            WgpuArtCnnInput::PlanarLuma { view, .. } => {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("erika-wgpu-artcnn-conv0-luma-bind-group"),
                    layout: &resources.conv0_luma_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&pool.feature_views[0]),
                        },
                    ],
                })
            }
            WgpuArtCnnInput::NonlinearRgb { view, .. } => {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("erika-wgpu-artcnn-conv0-rgb-bind-group"),
                    layout: &resources.conv0_rgb_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&pool.feature_views[0]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 7,
                            resource: wgpu::BindingResource::TextureView(view),
                        },
                    ],
                })
            }
        };
        let chain = [(0usize, 1usize), (1, 2), (2, 1), (1, 2), (2, 1)];
        let mid_bind_groups: Vec<_> = chain
            .iter()
            .enumerate()
            .map(|(layer, &(src, dst))| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(match layer {
                        0 => "erika-wgpu-artcnn-mid1-bind-group",
                        1 => "erika-wgpu-artcnn-mid2-bind-group",
                        2 => "erika-wgpu-artcnn-mid3-bind-group",
                        3 => "erika-wgpu-artcnn-mid4-bind-group",
                        _ => "erika-wgpu-artcnn-mid5-bind-group",
                    }),
                    layout: &resources.mid_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&pool.feature_views[src]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(&pool.feature_views[dst]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::TextureView(&pool.feature_views[0]),
                        },
                    ],
                })
            })
            .collect();
        let conv6_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("erika-wgpu-artcnn-conv6-bind-group"),
            layout: &resources.conv6_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&pool.feature_views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&pool.output_view),
                },
            ],
        });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("erika-wgpu-artcnn-pass"),
                timestamp_writes: self.timestamps.as_ref().map(|query_set| {
                    wgpu::ComputePassTimestampWrites {
                        query_set,
                        beginning_of_pass_write_index: Some(0),
                        end_of_pass_write_index: Some(1),
                    }
                }),
            });
            for (index, record) in records.iter().enumerate() {
                let dynamic_offset =
                    u32::try_from(index as u64 * parameter_stride).map_err(|_| {
                        WgpuArtCnnFailure::new(
                            WgpuArtCnnFailureStage::Encode,
                            self.mode,
                            Some(input.kind()),
                            "dynamic uniform offset exceeds u32",
                        )
                    })?;
                match record.kind {
                    DispatchKind::Conv0 => {
                        let pipeline = match input.kind() {
                            WgpuArtCnnInputKind::PlanarLuma => &resources.conv0_luma_pipeline,
                            WgpuArtCnnInputKind::NonlinearRgb => &resources.conv0_rgb_pipeline,
                        };
                        pass.set_pipeline(pipeline);
                        pass.set_bind_group(0, &conv0_bind_group, &[]);
                    }
                    DispatchKind::Mid(layer) => {
                        pass.set_pipeline(&resources.mid_pipeline);
                        pass.set_bind_group(0, &mid_bind_groups[layer], &[]);
                    }
                    DispatchKind::Conv6 => {
                        pass.set_pipeline(&resources.conv6_pipeline);
                        pass.set_bind_group(0, &conv6_bind_group, &[]);
                    }
                }
                pass.set_bind_group(1, &common_bind_group, &[dynamic_offset]);
                pass.dispatch_workgroups(
                    record.params.dispatch[1].div_ceil(WORKGROUP_X),
                    record.params.dispatch[2].div_ceil(WORKGROUP_Y),
                    1,
                );
            }
        }

        let serial = self.next_tentative_serial;
        self.next_tentative_serial = self.next_tentative_serial.wrapping_add(1);
        self.pending_encode = Some(PendingEncode {
            serial,
            frame_token,
            input_kind: input.kind(),
            width,
            height,
            encoded_tiles: (records.len() / 7) as u64,
            compute_dispatches: records.len() as u64,
            encode_duration: started.elapsed(),
        });
        let pool = self.pool.as_ref().expect("pool built above");
        Ok(Some(output_from_pool(
            pool,
            input.kind(),
            frame_token,
            false,
            Some(serial),
        )))
    }

    fn validate_frame(
        &self,
        input: WgpuArtCnnInput<'_>,
        width: u32,
        height: u32,
    ) -> Result<(), WgpuArtCnnFailure> {
        if width == 0 || height == 0 {
            return Err(WgpuArtCnnFailure::new(
                WgpuArtCnnFailureStage::Frame,
                self.mode,
                Some(input.kind()),
                "input dimensions must be non-zero",
            ));
        }
        if width > self.capability.max_texture_dimension_2d
            || height > self.capability.max_texture_dimension_2d
        {
            return Err(WgpuArtCnnFailure::new(
                WgpuArtCnnFailureStage::Frame,
                self.mode,
                Some(input.kind()),
                format!(
                    "input {width}x{height} exceeds max_texture_dimension_2d={}",
                    self.capability.max_texture_dimension_2d
                ),
            ));
        }
        if let WgpuArtCnnInput::NonlinearRgb {
            luma_coefficients, ..
        } = input
        {
            if !luma_coefficients.iter().all(|value| value.is_finite())
                || luma_coefficients.iter().copied().sum::<f32>() <= 0.0
            {
                return Err(WgpuArtCnnFailure::new(
                    WgpuArtCnnFailureStage::Frame,
                    self.mode,
                    Some(input.kind()),
                    format!("invalid nonlinear RGB luma coefficients {luma_coefficients:?}"),
                ));
            }
        }
        Ok(())
    }

    fn record_failure(&mut self, failure: WgpuArtCnnFailure) {
        self.status = WgpuArtCnnStatus::Inactive;
        self.pending_encode = None;
        self.stats.fallback_count = self.stats.fallback_count.saturating_add(1);
        self.last_failure = Some(failure);
    }

    fn record_runtime_failure(&mut self, failure: WgpuArtCnnFailure) {
        self.status = WgpuArtCnnStatus::Inactive;
        self.pending_encode = None;
        if let Some(pool) = self.pool.as_mut() {
            pool.frame_cache.invalidate();
            pool.cached_input_kind = None;
        }
        self.stats.fallback_count = self.stats.fallback_count.saturating_add(1);
        self.last_failure = Some(failure);
    }
}

fn output_from_pool(
    pool: &TexturePool,
    input_kind: WgpuArtCnnInputKind,
    frame_token: Option<u64>,
    cache_hit: bool,
    tentative_serial: Option<u64>,
) -> WgpuArtCnnOutput {
    WgpuArtCnnOutput {
        texture: pool.output_texture.clone(),
        view: pool.output_view.clone(),
        packed_width: pool.width,
        packed_height: pool.height,
        virtual_width: pool.width.saturating_mul(2),
        virtual_height: pool.height.saturating_mul(2),
        input_kind,
        frame_token,
        cache_hit,
        tentative_serial,
    }
}

fn build_dispatch_records(
    width: u32,
    height: u32,
    config: WgpuArtCnnConfig,
    offsets: &LayerOffsets,
    slices: u32,
    luma_coefficients: [f32; 3],
) -> Vec<DispatchRecord> {
    let tiles_x = width.div_ceil(config.tile_width);
    let tiles_y = height.div_ceil(config.tile_height);
    let mut records = Vec::with_capacity((tiles_x * tiles_y * 7) as usize);
    for tile_y in 0..tiles_y {
        for tile_x in 0..tiles_x {
            let core_x = tile_x * config.tile_width;
            let core_y = tile_y * config.tile_height;
            let core_width = (width - core_x).min(config.tile_width);
            let core_height = (height - core_y).min(config.tile_height);
            let feature_origin_x = core_x as i32 - FEATURE_HALO as i32;
            let feature_origin_y = core_y as i32 - FEATURE_HALO as i32;
            let base = |inset: u32,
                        dispatch_width: u32,
                        dispatch_height: u32,
                        weight_offset: u32,
                        bias_offset: u32,
                        relu: bool,
                        add_residual: bool| TileParams {
                image_core: [width, height, core_width, core_height],
                origins: [
                    core_x as i32,
                    core_y as i32,
                    feature_origin_x,
                    feature_origin_y,
                ],
                dispatch: [inset, dispatch_width, dispatch_height, slices],
                layer: [
                    weight_offset,
                    bias_offset,
                    u32::from(relu),
                    u32::from(add_residual),
                ],
                luma_coefficients: [
                    luma_coefficients[0],
                    luma_coefficients[1],
                    luma_coefficients[2],
                    0.0,
                ],
            };
            records.push(DispatchRecord {
                kind: DispatchKind::Conv0,
                params: base(
                    0,
                    core_width + FEATURE_HALO * 2,
                    core_height + FEATURE_HALO * 2,
                    offsets.conv0_w,
                    offsets.conv0_b,
                    false,
                    false,
                ),
            });
            for layer in 0..MIDDLE_LAYER_COUNT {
                let convolution = layer as u32 + 1;
                records.push(DispatchRecord {
                    kind: DispatchKind::Mid(layer),
                    params: base(
                        convolution,
                        core_width + (FEATURE_HALO - convolution) * 2,
                        core_height + (FEATURE_HALO - convolution) * 2,
                        offsets.mid_w[layer],
                        offsets.mid_b[layer],
                        layer != MIDDLE_LAYER_COUNT - 1,
                        layer == MIDDLE_LAYER_COUNT - 1,
                    ),
                });
            }
            records.push(DispatchRecord {
                kind: DispatchKind::Conv6,
                params: base(
                    FEATURE_HALO,
                    core_width,
                    core_height,
                    offsets.conv6_w,
                    offsets.conv6_b,
                    false,
                    false,
                ),
            });
        }
    }
    records
}

fn encode_parameter_records(
    records: &[DispatchRecord],
    stride: u64,
    max_buffer_size: u64,
    mode: LumaUpscalerMode,
    input_kind: WgpuArtCnnInputKind,
) -> Result<Vec<u8>, WgpuArtCnnFailure> {
    let total = stride.checked_mul(records.len() as u64).ok_or_else(|| {
        WgpuArtCnnFailure::new(
            WgpuArtCnnFailureStage::Frame,
            mode,
            Some(input_kind),
            "parameter buffer size overflow",
        )
    })?;
    if total > max_buffer_size || total > usize::MAX as u64 || total > u64::from(u32::MAX) {
        return Err(WgpuArtCnnFailure::new(
            WgpuArtCnnFailureStage::Frame,
            mode,
            Some(input_kind),
            format!(
                "parameter buffer size {total} exceeds max_buffer_size={max_buffer_size} or dynamic-offset range"
            ),
        ));
    }
    let mut bytes = vec![0; total as usize];
    for (index, record) in records.iter().enumerate() {
        let start = index * stride as usize;
        let params = bytemuck::bytes_of(&record.params);
        bytes[start..start + params.len()].copy_from_slice(params);
    }
    Ok(bytes)
}

fn align_up(value: u64, alignment: u64) -> u64 {
    value.div_ceil(alignment.max(1)) * alignment.max(1)
}

/// Decodes a tightly packed source-sized RGBA16Float readback into the virtual
/// 2W x 2H scalar luma plane used by the ONNX numeric regression tests.
pub fn unpack_packed_d2s_rgba16f(
    packed: &[u8],
    width: u32,
    height: u32,
    bytes_per_row: u32,
) -> Result<Vec<f32>, String> {
    let tight_row_bytes = width
        .checked_mul(8)
        .ok_or_else(|| "packed row byte overflow".to_string())?;
    if bytes_per_row < tight_row_bytes {
        return Err(format!(
            "bytes_per_row {bytes_per_row} is below tight row size {tight_row_bytes}"
        ));
    }
    let required = u64::from(bytes_per_row)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "packed readback size overflow".to_string())?;
    if required > packed.len() as u64 {
        return Err(format!(
            "packed readback is {} bytes, expected at least {required}",
            packed.len()
        ));
    }
    let output_width = width as usize * 2;
    let output_height = height as usize * 2;
    let mut output = vec![0.0; output_width * output_height];
    for y in 0..height as usize {
        let row = y * bytes_per_row as usize;
        for x in 0..width as usize {
            let pixel = row + x * 8;
            let values = [
                half_to_f32(u16::from_le_bytes([packed[pixel], packed[pixel + 1]])),
                half_to_f32(u16::from_le_bytes([packed[pixel + 2], packed[pixel + 3]])),
                half_to_f32(u16::from_le_bytes([packed[pixel + 4], packed[pixel + 5]])),
                half_to_f32(u16::from_le_bytes([packed[pixel + 6], packed[pixel + 7]])),
            ];
            let out_x = x * 2;
            let out_y = y * 2;
            output[out_y * output_width + out_x] = values[0];
            output[out_y * output_width + out_x + 1] = values[1];
            output[(out_y + 1) * output_width + out_x] = values[2];
            output[(out_y + 1) * output_width + out_x + 1] = values[3];
        }
    }
    Ok(output)
}

fn half_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = u32::from((bits >> 10) & 0x1F);
    let mantissa = u32::from(bits & 0x03FF);
    let value = match (exponent, mantissa) {
        (0, 0) => sign,
        (0, _) => {
            let shift = mantissa.leading_zeros() - 21;
            let exponent = 127 - 15 - shift;
            let mantissa = (mantissa << (shift + 1)) & 0x03FF;
            sign | (exponent << 23) | (mantissa << 13)
        }
        (0x1F, 0) => sign | 0x7F80_0000,
        (0x1F, _) => sign | 0x7FC0_0000,
        _ => sign | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    };
    f32::from_bits(value)
}
