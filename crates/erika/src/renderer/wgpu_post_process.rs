//! wgpu RGB post-processing pass driven by a validated [`PostProcessShader`].

use std::sync::Arc;
use std::time::Duration;

use wgpu::util::DeviceExt;

use crate::core::{PlayerError, Result};
use crate::renderer::post_process::{
    PostProcessRuntimeInfo, PostProcessShader, PostProcessStage, PostProcessStatus,
    PostProcessUniforms,
};

struct CompiledPipeline {
    format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

struct SourceTexture {
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

pub(crate) struct WgpuPostProcess {
    shader: Option<Arc<PostProcessShader>>,
    compiled: Option<CompiledPipeline>,
    source: Option<SourceTexture>,
    status: PostProcessStatus,
    reason: Option<String>,
    last_gpu_duration: Duration,
}

impl Default for WgpuPostProcess {
    fn default() -> Self {
        Self {
            shader: None,
            compiled: None,
            source: None,
            status: PostProcessStatus::Off,
            reason: None,
            last_gpu_duration: Duration::ZERO,
        }
    }
}

impl WgpuPostProcess {
    pub(crate) fn stage(&self) -> PostProcessStage {
        self.shader
            .as_ref()
            .map_or(PostProcessStage::Main, |shader| shader.stage)
    }

    #[cfg_attr(
        not(all(target_os = "linux", not(target_env = "ohos"))),
        allow(dead_code)
    )]
    pub(crate) fn shader(&self) -> Option<Arc<PostProcessShader>> {
        self.shader.clone()
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
        self.compiled = None;
        self.reason = None;
        self.last_gpu_duration = Duration::ZERO;
        if self.shader.is_none() {
            self.source = None;
            self.status = PostProcessStatus::Off;
        } else {
            self.status = PostProcessStatus::Building;
        }
    }

    pub(crate) fn status_info(&self) -> PostProcessRuntimeInfo {
        PostProcessRuntimeInfo {
            status: self.status,
            name: self.shader.as_ref().map(|shader| shader.name.clone()),
            reason: self.reason.clone(),
            last_gpu_duration: self.last_gpu_duration,
        }
    }

    /// Prepares the offscreen source texture and render pipeline for
    /// `(width, height, format)`. Returns `true` when active and ready.
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> bool {
        let Some(shader) = self.shader.clone() else {
            return false;
        };
        if self
            .compiled
            .as_ref()
            .is_none_or(|compiled| compiled.format != format)
        {
            match compile_pipeline(device, &shader, format) {
                Ok(compiled) => {
                    self.compiled = Some(compiled);
                    self.reason = None;
                }
                Err(error) => {
                    self.compiled = None;
                    self.status = PostProcessStatus::Inactive;
                    self.reason = Some(error.to_string());
                    return false;
                }
            }
        }
        let width = width.max(1);
        let height = height.max(1);
        let needs_texture = self
            .source
            .as_ref()
            .is_none_or(|src| src.width != width || src.height != height || src.format != format);
        if needs_texture {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("erika-wgpu-post-src"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.source = Some(SourceTexture {
                width,
                height,
                format,
                _texture: texture,
                view,
            });
        }
        self.status = PostProcessStatus::Active;
        self.reason = None;
        true
    }

    pub(crate) fn source_view(&self) -> Option<&wgpu::TextureView> {
        self.source.as_ref().map(|src| &src.view)
    }

    /// Encodes the post-processing render pass from the prepared source texture
    /// into `target_view` within `(vx, vy, vw, vh)`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn encode_pass(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        clear_color: wgpu::Color,
        viewport: (f32, f32, f32, f32),
        uniforms: &PostProcessUniforms,
    ) {
        let (Some(compiled), Some(source)) = (self.compiled.as_ref(), self.source.as_ref()) else {
            return;
        };
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("erika-wgpu-post-uniforms"),
            contents: bytemuck::bytes_of(uniforms),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("erika-wgpu-post-bind-group"),
            layout: &compiled.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&source.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&compiled.sampler),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("erika-wgpu-post-process-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear_color),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&compiled.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.set_viewport(viewport.0, viewport.1, viewport.2, viewport.3, 0.0, 1.0);
        pass.draw(0..3, 0..1);
    }
}

fn compile_pipeline(
    device: &wgpu::Device,
    shader: &PostProcessShader,
    format: wgpu::TextureFormat,
) -> Result<CompiledPipeline> {
    let (wgsl, frag_entry) = shader.to_wgsl()?;
    let validation_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("erika-wgpu-post-process-shader"),
        source: wgpu::ShaderSource::Wgsl(wgsl.into()),
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("erika-wgpu-post-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("erika-wgpu-post-layout"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("erika-wgpu-post-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("erika_post_vs"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(&frag_entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    let validation = pollster::block_on(validation_scope.pop());
    if let Some(err) = validation {
        return Err(PlayerError::Renderer(format!(
            "wgpu post-process pipeline validation failed for {:?}: {err}",
            shader.name
        )));
    }
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("erika-wgpu-post-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    Ok(CompiledPipeline {
        format,
        pipeline,
        bind_group_layout,
        sampler,
    })
}
