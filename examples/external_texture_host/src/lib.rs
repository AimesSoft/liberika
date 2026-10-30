pub mod import;

use anyhow::{Context, Result, ensure};

/// An independent wgpu 26 compositor. The producer uses wgpu 29.
pub struct Host {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter_info: wgpu::AdapterInfo,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    target: Option<wgpu::Texture>,
}

impl Host {
    pub fn new(require_hardware: bool) -> Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::from_env().unwrap_or_default(),
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))?;
        let adapter_info = adapter.get_info();
        ensure!(
            !require_hardware || adapter_info.device_type != wgpu::DeviceType::Cpu,
            "hardware was required but selected {:?}",
            adapter_info
        );
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("external-texture-host-wgpu-26"),
                ..Default::default()
            }))?;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(
                r#"
@group(0) @binding(0) var frame: texture_2d<f32>;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = array<vec2<f32>,3>(vec2(-1.,-1.),vec2(3.,-1.),vec2(-1.,3.));
    return vec4(p[i],0.,1.);
}
@fragment fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(frame,vec2<i32>(p.xy),0);
}
"#,
            )),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        Ok(Self {
            device,
            queue,
            adapter_info,
            pipeline,
            layout,
            target: None,
        })
    }

    pub fn upload_rgba(&self, width: u32, height: u32, rgba: &[u8]) -> wgpu::Texture {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("explicit-cpu-upload"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        self.queue.write_texture(
            texture.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            texture.size(),
        );
        texture
    }

    pub fn draw(&mut self, texture: &wgpu::Texture) {
        let size = texture.size();
        if self
            .target
            .as_ref()
            .is_none_or(|target| target.size() != size)
        {
            self.target = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("host-composition-target"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            }));
        }
        let input = texture.create_view(&Default::default());
        let target = self
            .target
            .as_ref()
            .unwrap()
            .create_view(&Default::default());
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&input),
            }],
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
    }

    pub fn wait(&self) -> Result<()> {
        self.device
            .poll(wgpu::PollType::Wait)
            .context("host GPU completion")?;
        Ok(())
    }

    /// Validation only: production import/draw never maps a pixel buffer.
    pub fn read_pixels(&self) -> Result<Vec<u8>> {
        let target = self.target.as_ref().context("no composed frame")?;
        let width = target.width();
        let height = target.height();
        let stride = (width * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(stride) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(height),
                },
            },
            target.size(),
        );
        self.queue.submit(Some(encoder.finish()));
        let (tx, rx) = std::sync::mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        self.wait()?;
        rx.recv()??;
        let mapped = buffer.slice(..).get_mapped_range();
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for row in mapped.chunks_exact(stride as usize) {
            pixels.extend_from_slice(&row[..width as usize * 4]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(pixels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use erika::danmaku::{
        DanmakuColor, DanmakuItem, DanmakuLayoutConfig, DanmakuMode, DanmakuTimeline,
        DanmakuViewport, DfmLayoutEngine,
    };
    use erika::overlay::{OverlayFrame, OverlayViewport};
    use erika::renderer::{
        pipeline::{VideoRenderPipeline, VideoUniforms},
        wgpu::WgpuRenderer,
    };
    use erika::subtitle::SubtitleBitmapPlane;
    use erika::{RenderFrameContext, RendererBackend};
    use std::time::Duration;

    #[test]
    fn cross_version_native_frames_survive_resize_and_producer_drop() -> Result<()> {
        let mut producer = WgpuRenderer::new()?;
        let uniforms =
            VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);
        producer.upload_nv12(64, 36, &[40; 64 * 36], &[128; 64 * 18], uniforms)?;
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            id: 1,
            pts: Duration::ZERO,
            text: "NATIVE HOST".into(),
            mode: DanmakuMode::Top,
            font_size: 24.,
            color: DanmakuColor::WHITE,
            opacity: 1.,
            is_self: false,
        }])?;
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let mut host = Host::new(std::env::var_os("ERIKA_REQUIRE_HARDWARE_GPU").is_some())?;
        let mut retained = Vec::new();
        for (width, height) in [(320, 180), (640, 360), (319, 181)] {
            let plan = engine.render_plan(
                Duration::from_secs(1),
                DanmakuViewport::new(width, height),
                1,
            );
            let overlay = OverlayFrame {
                pts: Duration::from_secs(1),
                viewport: OverlayViewport::new(width, height),
                subtitle_planes: vec![SubtitleBitmapPlane::new(
                    4,
                    100,
                    32,
                    8,
                    [255, 0, 0, 255].repeat(32 * 8),
                )],
                subtitle_alpha_planes: vec![],
                subtitle_changed: true,
            };
            let context = RenderFrameContext::new(Duration::from_secs(1), 1)
                .overlay(Some(&overlay))
                .danmaku(Some(&plan))
                .output_size(width, height);
            let expected = producer
                .capture_current_frame(context, width, height)?
                .context("reference frame")?
                .rgba;
            let frame = producer
                .export_current_frame_texture(context, width, height)?
                .context("native frame")?;
            let texture = import::import_frame(&host.device, &host.queue, frame)?;
            host.draw(&texture);
            ensure!(
                host.read_pixels()? == expected,
                "wgpu 26 composition differs from wgpu 29 output at {width}x{height}"
            );
            retained.push((texture, expected));
        }
        drop(producer);
        for (texture, expected) in retained {
            host.draw(&texture);
            ensure!(
                host.read_pixels()? == expected,
                "retained host frame changed"
            );
        }
        host.wait()?;
        Ok(())
    }
}
