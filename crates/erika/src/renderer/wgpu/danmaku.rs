//! Whole-comment compositing, without per-comment textures or draw calls.
use super::*;
use crate::renderer::danmaku_batch::{DanmakuBatch, DanmakuBatchInstance};

pub(super) struct DanmakuPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    format: wgpu::TextureFormat,
}

pub(super) struct DanmakuResources {
    glyph_texture: wgpu::Texture,
    instances: wgpu::Buffer,
    uniforms: wgpu::Buffer,
}

struct TextDraw {
    bind_group: wgpu::BindGroup,
    instances: wgpu::Buffer,
    instance_count: u32,
}

#[derive(Default)]
pub(super) struct DanmakuDraws {
    text: Option<TextDraw>,
    pictures: Vec<OverlayDraw>,
}

impl DanmakuDraws {
    pub(super) fn len(&self) -> usize {
        usize::from(self.text.is_some()) + self.pictures.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_none() && self.pictures.is_empty()
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    viewport: [f32; 2],
    output_encoding: u32,
    glyph_width: u32,
}

fn capacity(required: u64, limit: u64) -> u64 {
    required
        .checked_next_power_of_two()
        .unwrap_or(limit)
        .min(limit)
}

fn error(message: &str) -> PlayerError {
    PlayerError::Renderer(message.to_string())
}

impl WgpuRenderer {
    pub(super) fn ensure_danmaku_pipeline(&mut self, format: wgpu::TextureFormat) {
        if self
            .danmaku_pipeline
            .as_ref()
            .is_some_and(|p| p.format == format)
        {
            return;
        }
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("erika-wgpu-danmaku-shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../wgpu_danmaku.wgsl").into()),
            });
        let texture_entry = |binding, filterable| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = self
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("erika-wgpu-danmaku-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: std::num::NonZeroU64::new(
                                std::mem::size_of::<Uniforms>() as u64,
                            ),
                        },
                        count: None,
                    },
                    // Rgba32Float is load-only: FLOAT32_FILTERABLE is not required.
                    texture_entry(1, false),
                    texture_entry(2, true),
                    texture_entry(3, true),
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let pipeline_layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("erika-wgpu-danmaku-layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
        let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("erika-wgpu-danmaku-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("danmaku_vertex"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<DanmakuBatchInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Uint32x2, 2 => Float32],
                }],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("danmaku_fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("erika-wgpu-danmaku-mask-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        self.danmaku_pipeline = Some(DanmakuPipeline {
            pipeline,
            layout,
            sampler,
            format,
        });
    }

    pub(super) fn prepare_danmaku_draws(
        &mut self,
        plan: &DanmakuRenderPlan,
        output: OutputDescription,
    ) -> Result<DanmakuDraws> {
        if self.overlay_pipeline.is_none() || self.danmaku_pipeline.is_none() {
            return Err(error("danmaku pipelines not initialized"));
        }
        // Check before constructing the batch, whose instance indices use u32.
        if plan.items.len() > (u32::MAX / 6) as usize {
            return Err(error("danmaku glyph indices exceed u32 texture addressing"));
        }
        let batch = DanmakuBatch::new(&plan.items);
        let text = if batch.instances.is_empty() {
            None
        } else {
            Some(self.prepare_danmaku_text(plan, &batch, output)?)
        };
        // A text-free plan (including one without a mask atlas) still draws emoji.
        let pictures = self.prepare_danmaku_pictures(plan, output)?;
        Ok(DanmakuDraws { text, pictures })
    }

    fn prepare_danmaku_text(
        &mut self,
        plan: &DanmakuRenderPlan,
        batch: &DanmakuBatch,
        output: OutputDescription,
    ) -> Result<TextDraw> {
        let limits = self.device.limits();
        let atlas = plan
            .atlas
            .as_ref()
            .ok_or_else(|| error("danmaku text has no glyph atlas"))?;
        if !atlas.is_valid()
            || atlas.width > limits.max_texture_dimension_2d
            || atlas.height > limits.max_texture_dimension_2d
            || u32::try_from(atlas.stride).is_err()
        {
            return Err(error(
                "danmaku glyph atlas is invalid or exceeds texture limits",
            ));
        }
        let instance_count = u32::try_from(batch.instances.len())
            .map_err(|_| error("danmaku instance count exceeds u32"))?;
        let instance_size = (batch.instances.len() as u64)
            .checked_mul(std::mem::size_of::<DanmakuBatchInstance>() as u64)
            .filter(|&size| size <= limits.max_buffer_size)
            .ok_or_else(|| error("danmaku instance buffer exceeds device limits"))?;
        let (width, height, texels) = batch
            .glyph_texture(limits.max_texture_dimension_2d)
            .ok_or_else(|| error("danmaku glyph data exceeds texture limits"))?;
        let row_bytes = width
            .checked_mul(16)
            .ok_or_else(|| error("danmaku glyph texture row size overflow"))?;
        let (fill, outline) = self.prepare_danmaku_atlas_textures(atlas);
        if self.danmaku_resources.is_none() {
            self.danmaku_resources = Some(DanmakuResources {
                glyph_texture: self.new_danmaku_data_texture(width, height),
                instances: self.new_danmaku_instance_buffer(instance_size),
                uniforms: self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("erika-wgpu-danmaku-uniforms"),
                    size: std::mem::size_of::<Uniforms>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
            });
        }
        let resources = self.danmaku_resources.as_ref().unwrap();
        let grow_texture =
            resources.glyph_texture.width() < width || resources.glyph_texture.height() < height;
        let texture = grow_texture.then(|| {
            self.new_danmaku_data_texture(
                width.max(resources.glyph_texture.width()),
                height.max(resources.glyph_texture.height()),
            )
        });
        let instances = (resources.instances.size() < instance_size)
            .then(|| self.new_danmaku_instance_buffer(instance_size));
        let resources = self.danmaku_resources.as_mut().unwrap();
        if let Some(texture) = texture {
            resources.glyph_texture = texture;
        }
        if let Some(instances) = instances {
            resources.instances = instances;
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &resources.glyph_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row_bytes),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.write_buffer(
            &resources.instances,
            0,
            bytemuck::cast_slice(&batch.instances),
        );
        self.queue.write_buffer(
            &resources.uniforms,
            0,
            bytemuck::bytes_of(&Uniforms {
                viewport: [
                    plan.viewport.width.max(1) as f32,
                    plan.viewport.height.max(1) as f32,
                ],
                output_encoding: u32::from(output.extended_linear),
                // Address the uploaded rectangle, not the larger retained capacity.
                glyph_width: width,
            }),
        );
        let pipeline = self.danmaku_pipeline.as_ref().unwrap();
        let glyph_view = resources.glyph_texture.create_view(&Default::default());
        let fill_view = fill.create_view(&Default::default());
        let outline_view = outline.create_view(&Default::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("erika-wgpu-danmaku-bind-group"),
            layout: &pipeline.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: resources.uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&glyph_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&fill_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&outline_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
                },
            ],
        });
        Ok(TextDraw {
            bind_group,
            instances: resources.instances.clone(),
            instance_count,
        })
    }

    fn new_danmaku_data_texture(&self, width: u32, height: u32) -> wgpu::Texture {
        let limit = self.device.limits().max_texture_dimension_2d as u64;
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("erika-wgpu-danmaku-glyph-data"),
            size: wgpu::Extent3d {
                width: capacity(width as u64, limit) as u32,
                height: capacity(height as u64, limit) as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    fn new_danmaku_instance_buffer(&self, size: u64) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("erika-wgpu-danmaku-instances"),
            size: capacity(size, self.device.limits().max_buffer_size),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn prepare_danmaku_pictures(
        &mut self,
        plan: &DanmakuRenderPlan,
        output: OutputDescription,
    ) -> Result<Vec<OverlayDraw>> {
        if plan.color_glyphs.is_empty() {
            return Ok(Vec::new());
        }
        let atlas = plan
            .color_atlas
            .as_ref()
            .ok_or_else(|| error("danmaku emoji has no picture atlas"))?;
        let limit = self.device.limits().max_texture_dimension_2d;
        let expected = (atlas.width as usize)
            .checked_mul(atlas.height as usize)
            .and_then(|n| n.checked_mul(4));
        if atlas.width == 0
            || atlas.height == 0
            || atlas.width > limit
            || atlas.height > limit
            || expected != Some(atlas.rgba.len())
            || atlas.width.checked_mul(4).is_none()
        {
            return Err(error(
                "danmaku picture atlas is invalid or exceeds texture limits",
            ));
        }
        let uniform_size = std::mem::size_of::<OverlayUniforms>();
        let alignment = self
            .device
            .limits()
            .min_uniform_buffer_offset_alignment
            .max(1) as usize;
        let stride = uniform_size.div_ceil(alignment) * alignment;
        let size = stride
            .checked_mul(plan.color_glyphs.len())
            .filter(|&n| n as u64 <= self.device.limits().max_buffer_size && n <= u32::MAX as usize)
            .ok_or_else(|| error("danmaku picture uniforms exceed buffer limits"))?;
        let mut bytes = vec![0u8; size];
        for (i, glyph) in plan.color_glyphs.iter().enumerate() {
            let uniform = OverlayUniforms::picture_rect(
                glyph.opacity,
                glyph.rect,
                glyph.tex_rect,
                plan.viewport.width,
                plan.viewport.height,
            )
            .for_output(output);
            bytes[i * stride..i * stride + uniform_size]
                .copy_from_slice(bytemuck::bytes_of(&uniform));
        }
        let texture = self.prepare_danmaku_picture_texture(atlas);
        let buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("erika-wgpu-danmaku-picture-uniforms"),
                contents: &bytes,
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let pipeline = self.overlay_pipeline.as_ref().unwrap();
        let view = texture.create_view(&Default::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("erika-wgpu-danmaku-picture-bind-group"),
            layout: &pipeline.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: 0,
                        size: std::num::NonZeroU64::new(uniform_size as u64),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
                },
            ],
        });
        Ok((0..plan.color_glyphs.len())
            .map(|i| OverlayDraw {
                bind_group: bind_group.clone(),
                dynamic_offset: (i * stride) as u32,
                _texture: texture.clone(),
                _uniform: buffer.clone(),
            })
            .collect())
    }

    pub(super) fn encode_danmaku_draws(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        draws: &DanmakuDraws,
    ) {
        if let Some(text) = &draws.text {
            pass.set_pipeline(
                &self
                    .danmaku_pipeline
                    .as_ref()
                    .expect("danmaku pipeline initialized")
                    .pipeline,
            );
            pass.set_bind_group(0, &text.bind_group, &[]);
            pass.set_vertex_buffer(0, text.instances.slice(..));
            pass.draw(0..4, 0..text.instance_count);
        }
        if !draws.pictures.is_empty() {
            pass.set_pipeline(
                &self
                    .overlay_pipeline
                    .as_ref()
                    .expect("overlay pipeline initialized")
                    .pipeline,
            );
            for draw in &draws.pictures {
                pass.set_bind_group(0, &draw.bind_group, &[draw.dynamic_offset]);
                pass.draw(0..4, 0..1);
            }
        }
    }
}

#[cfg(test)]
mod tests;
