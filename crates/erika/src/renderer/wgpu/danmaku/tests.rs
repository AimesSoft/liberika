use super::*;
use crate::danmaku::{DanmakuColorGlyph, DanmakuViewport};
use crate::renderer::danmaku_batch::test_support::{
    check_boundaries, check_opacity, check_overlap, check_target_alpha, plan,
};
use crate::renderer::sr::model::f16_to_f32;

/// Exercises exactly the production preparation and render-pass binding paths.
/// Failure to obtain a real adapter is a test failure, never a silent skip.
struct Fixture {
    renderer: WgpuRenderer,
    target: wgpu::Texture,
    readback: wgpu::Buffer,
    format: wgpu::TextureFormat,
    output: OutputDescription,
}

impl Fixture {
    fn new(format: wgpu::TextureFormat) -> Self {
        let mut renderer =
            WgpuRenderer::new().expect("danmaku tests require a working GPU adapter");
        eprintln!(
            "danmaku GPU: {:?}, target={format:?}",
            renderer.adapter.get_info()
        );
        renderer.ensure_overlay_pipeline(format);
        let target = renderer.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("danmaku-test-target"),
            size: wgpu::Extent3d {
                width: 32,
                height: 24,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        // Both RGBA8 and RGBA16Float rows fit in one 256-byte aligned row.
        let readback = renderer.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("danmaku-test-readback"),
            size: 256 * 24,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let output = if format == wgpu::TextureFormat::Rgba16Float {
            OutputDescription::extended_linear(4.0)
        } else {
            OutputDescription::sdr()
        };
        Self {
            renderer,
            target,
            readback,
            format,
            output,
        }
    }

    fn render(&mut self, plan: &DanmakuRenderPlan, background: [f32; 4]) -> Vec<[f32; 4]> {
        let draws = self
            .renderer
            .prepare_danmaku_draws(plan, self.output)
            .unwrap();
        assert_eq!(
            draws.len(),
            usize::from(draws.text.is_some()) + plan.color_glyphs.len()
        );
        let view = self.target.create_view(&Default::default());
        let mut encoder =
            self.renderer
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("danmaku-test-encoder"),
                });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("danmaku-test-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: background[0] as f64,
                            g: background[1] as f64,
                            b: background[2] as f64,
                            a: background[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.renderer.encode_danmaku_draws(&mut pass, &draws);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(24),
                },
            },
            wgpu::Extent3d {
                width: 32,
                height: 24,
                depth_or_array_layers: 1,
            },
        );
        self.renderer.queue.submit(Some(encoder.finish()));
        let slice = self.readback.slice(..);
        let (send, receive) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            send.send(result).unwrap()
        });
        self.renderer
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        receive.recv().unwrap().unwrap();
        let bytes = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity(32 * 24);
        for y in 0..24 {
            for x in 0..32 {
                pixels.push(if self.format == wgpu::TextureFormat::Rgba16Float {
                    let offset = y * 256 + x * 8;
                    std::array::from_fn(|c| {
                        f16_to_f32(u16::from_le_bytes([
                            bytes[offset + c * 2],
                            bytes[offset + c * 2 + 1],
                        ]))
                    })
                } else {
                    let offset = y * 256 + x * 4;
                    std::array::from_fn(|c| bytes[offset + c] as f32 / 255.0)
                });
            }
        }
        drop(bytes);
        self.readback.unmap();
        pixels
    }
}

fn check_gpu(format: wgpu::TextureFormat) {
    let mut fixture = Fixture::new(format);
    check_opacity(|plan, background| fixture.render(plan, background));
    check_boundaries(|plan, background| fixture.render(plan, background));
    check_overlap(|plan, background| fixture.render(plan, background));
    check_target_alpha(|plan, background| fixture.render(plan, background));

    // Non-primary colors catch accidental Metal EDR transforms or gamma applied
    // after compositing instead of independently to the layer colors.
    let p = plan(1, 0.5, [0.25, 0.5, 0.75]);
    let pixel = fixture.render(&p, [0.0, 0.0, 0.0, 1.0])[7 * 32 + 7];
    for (actual, color) in pixel[..3].iter().zip([0.25_f32, 0.5, 0.75]) {
        let expected = if fixture.output.extended_linear {
            color.powf(2.2)
        } else {
            color
        } * 0.5;
        assert!(
            (actual - expected).abs() < 2.0 / 255.0,
            "{pixel:?}: {expected}"
        );
    }
    // No per-frame GPU allocation at a retained batch capacity, even when the
    // uploaded rectangle has a different width from the texture's capacity.
    let resources = fixture.renderer.danmaku_resources.as_ref().unwrap();
    let (texture, buffer) = (resources.glyph_texture.clone(), resources.instances.clone());
    fixture.render(&p, [0.0; 4]);
    let resources = fixture.renderer.danmaku_resources.as_ref().unwrap();
    assert_eq!(resources.glyph_texture, texture);
    assert_eq!(resources.instances, buffer);
}

#[test]
fn wgpu_danmaku_gpu_comment_opacity_sdr() {
    check_gpu(wgpu::TextureFormat::Rgba8Unorm);
}

#[test]
fn wgpu_danmaku_gpu_comment_opacity_float() {
    check_gpu(wgpu::TextureFormat::Rgba16Float);
}

#[test]
fn wgpu_danmaku_gpu_reads_glyph_ranges_across_data_texture_rows() {
    for format in [
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureFormat::Rgba16Float,
    ] {
        let mut fixture = Fixture::new(format);
        let mut p = plan(1, 0.5, [1.0; 3]);
        let visible = p.items[0].clone();
        let row_glyphs = fixture.renderer.device.limits().max_texture_dimension_2d as usize / 6;
        let mut hidden = visible.clone();
        hidden.item_id = 0;
        hidden.rect[0] = -100.0;
        p.items = vec![hidden; row_glyphs - 1];
        p.items.push(visible.clone());
        let mut front = visible;
        front.item_id = 2;
        front.color_rgba = [1.0, 0.0, 0.0, 0.5];
        p.items.push(front);
        let pixels = fixture.render(&p, [0.0, 0.0, 0.0, 1.0]);
        let pixel = pixels[7 * 32 + 7];
        for (a, e) in pixel.into_iter().zip([0.75, 0.25, 0.25, 1.0]) {
            assert!((a - e).abs() < 2.0 / 255.0, "{pixel:?}");
        }
        assert!(
            fixture
                .renderer
                .danmaku_resources
                .as_ref()
                .unwrap()
                .glyph_texture
                .height()
                >= 2
        );
        // Now the same strip's glyph loop crosses the row boundary.
        p.items.last_mut().unwrap().item_id = 1;
        let pixel = fixture.render(&p, [0.0, 0.0, 0.0, 1.0])[7 * 32 + 7];
        for (a, e) in pixel.into_iter().zip([0.5, 0.0, 0.0, 1.0]) {
            assert!((a - e).abs() < 2.0 / 255.0, "{pixel:?}");
        }
    }
}

#[test]
fn wgpu_danmaku_gpu_preserves_emoji_without_text_or_mask_atlas() {
    for format in [
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureFormat::Rgba16Float,
    ] {
        let mut fixture = Fixture::new(format);
        let mut p =
            DanmakuRenderPlan::empty(std::time::Duration::ZERO, 1, DanmakuViewport::new(32, 24));
        p.color_atlas = Some(std::sync::Arc::new(DanmakuColorAtlas {
            width: 1,
            height: 1,
            rgba: vec![128, 0, 0, 128],
            version: 1,
        }));
        p.color_glyphs.push(DanmakuColorGlyph {
            rect: [4.0, 4.0, 8.0, 8.0],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            opacity: 0.5,
        });
        let pixels = fixture.render(&p, [0.0; 4]);
        let pixel = pixels[7 * 32 + 7];
        for (a, e) in pixel
            .into_iter()
            .zip([0.5 * 128.0 / 255.0, 0.0, 0.0, 0.5 * 128.0 / 255.0])
        {
            assert!((a - e).abs() < 2.0 / 255.0, "{pixel:?}");
        }
        assert_eq!(pixels[0], [0.0; 4]);
        assert!(fixture.renderer.danmaku_resources.is_none());
    }
}

#[test]
fn wgpu_danmaku_gpu_incremental_mask_upload_changes_pixels_without_reallocation() {
    let mut fixture = Fixture::new(wgpu::TextureFormat::Rgba8Unorm);
    let mut p = plan(1, 0.5, [1.0; 3]);
    let first = fixture.render(&p, [0.0, 0.0, 0.0, 1.0])[7 * 32 + 7];
    let texture = fixture
        .renderer
        .danmaku_atlas_cache
        .as_ref()
        .unwrap()
        .fill_texture
        .clone();
    let atlas = std::sync::Arc::make_mut(p.atlas.as_mut().unwrap());
    atlas.fill_alpha[3 * 8 + 3] = 0;
    atlas.update = Some(DanmakuAtlasUpdate {
        from_version: atlas.version,
        x: 3,
        y: 3,
        width: 1,
        height: 1,
    });
    atlas.version += 1;
    let second = fixture.render(&p, [0.0, 0.0, 0.0, 1.0])[7 * 32 + 7];
    assert!(first[0] > 0.49, "{first:?}");
    assert!(second[0] < 0.01, "{second:?}");
    assert_eq!(
        fixture
            .renderer
            .danmaku_atlas_cache
            .as_ref()
            .unwrap()
            .fill_texture,
        texture
    );
}
