//! Disjoint strips let the fragment shader composite a whole comment before
//! applying its opacity, without a render target or draw call per comment.
use crate::danmaku::DanmakuGlyphInstance;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct DanmakuBatchInstance {
    pub rect: [f32; 4],
    pub glyph_start: u32,
    pub glyph_count: u32,
    pub opacity: f32,
    reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct DanmakuBatchGlyph {
    pub rect: [f32; 4],
    pub tex_rect: [f32; 4],
    pub color: [f32; 4],
    pub outline: [f32; 4],
    pub shadow: [f32; 4],
    pub shadow_offset: [f32; 4],
}

#[derive(Default)]
pub(super) struct DanmakuBatch {
    pub instances: Vec<DanmakuBatchInstance>,
    pub glyphs: Vec<DanmakuBatchGlyph>,
}

impl DanmakuBatch {
    pub fn new(items: &[DanmakuGlyphInstance]) -> Self {
        let mut batch = Self::default();
        let mut start = 0;
        while start < items.len() {
            let end = items[start..]
                .iter()
                .position(|item| item.item_id != items[start].item_id)
                .map_or(items.len(), |offset| start + offset);
            batch.append_comment(&items[start..end]);
            start = end;
        }
        batch
    }

    fn append_comment(&mut self, items: &[DanmakuGlyphInstance]) {
        // The planner gives all glyphs of a comment the same fill/outline
        // alpha, with a relative shadow strength. Factor the shared opacity
        // out once. Taking the maximum also supports caller-supplied plans
        // whose glyphs have different intrinsic alpha values.
        let opacity = items
            .iter()
            .fold(0.0_f32, |a, item| {
                a.max(item.color_rgba[3])
                    .max(item.outline_rgba[3])
                    .max(item.shadow_rgba[3])
            })
            .clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return;
        }
        let base = self.glyphs.len();
        let normalize = |mut color: [f32; 4]| {
            color[3] = (color[3] / opacity).clamp(0.0, 1.0);
            color
        };
        let mut bounds = Vec::with_capacity(items.len());
        let mut events = Vec::with_capacity(items.len() * 2);
        for (index, item) in items.iter().enumerate() {
            self.glyphs.push(DanmakuBatchGlyph {
                rect: item.rect,
                tex_rect: item.tex_rect,
                color: normalize(item.color_rgba),
                outline: normalize(item.outline_rgba),
                shadow: normalize(item.shadow_rgba),
                shadow_offset: [item.shadow_offset[0], item.shadow_offset[1], 0.0, 0.0],
            });
            let [x, y, w, h] = item.rect;
            let [dx, dy] = if item.shadow_rgba[3] > 0.0 {
                item.shadow_offset
            } else {
                [0.0; 2]
            };
            let b = [
                x + dx.min(0.0),
                y + dy.min(0.0),
                x + w + dx.max(0.0),
                y + h + dy.max(0.0),
            ];
            bounds.push(b);
            if b.iter().all(|v| v.is_finite()) && b[2] > b[0] && b[3] > b[1] {
                events.push((b[0], index, true));
                events.push((b[2], index, false));
            }
        }
        events.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let mut active = Vec::new();
        let mut cursor = 0;
        while cursor < events.len() {
            let left = events[cursor].0;
            while cursor < events.len() && events[cursor].0 == left {
                let (_, index, enters) = events[cursor];
                if enters {
                    active.push(index);
                } else if let Some(at) = active.iter().position(|&i| i == index) {
                    active.swap_remove(at);
                }
                cursor += 1;
            }
            if active.is_empty() || cursor == events.len() {
                continue;
            }
            let right = events[cursor].0;
            let first = *active.iter().min().unwrap();
            let last = *active.iter().max().unwrap();
            let top = active
                .iter()
                .map(|&i| bounds[i][1])
                .fold(f32::INFINITY, f32::min);
            let bottom = active
                .iter()
                .map(|&i| bounds[i][3])
                .fold(f32::NEG_INFINITY, f32::max);
            self.instances.push(DanmakuBatchInstance {
                rect: [left, top, right - left, bottom - top],
                glyph_start: (base + first) as u32,
                glyph_count: (last - first + 1) as u32,
                opacity,
                reserved: 0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(item_id: u64, x: f32) -> DanmakuGlyphInstance {
        DanmakuGlyphInstance {
            item_id,
            rect: [x, 2.0, 10.0, 10.0],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            color_rgba: [1.0, 1.0, 1.0, 0.5],
            outline_rgba: [0.0, 0.0, 0.0, 0.5],
            shadow_rgba: [0.0, 0.0, 0.0, 0.275],
            shadow_offset: [-2.0, 3.0],
        }
    }

    #[test]
    fn strips_cover_effects_once_and_keep_comment_order() {
        let batch = DanmakuBatch::new(&[glyph(1, 4.0), glyph(1, 10.0), glyph(2, 4.0)]);
        assert_eq!(batch.glyphs.len(), 3);
        for (index, group) in [(0..2, &batch.instances[..3]), (2..3, &batch.instances[3..])] {
            for x in 0..44 {
                let px = x as f32 * 0.5 + 0.25;
                let covered = group
                    .iter()
                    .filter(|s| px >= s.rect[0] && px < s.rect[0] + s.rect[2])
                    .count();
                let right = if index.start == 0 { 20.0 } else { 14.0 };
                assert_eq!(covered, usize::from(px >= 2.0 && px < right));
            }
            for strip in group {
                assert!(index.contains(&(strip.glyph_start as usize)));
                assert!(strip.glyph_start as usize + strip.glyph_count as usize <= index.end);
                assert_eq!(strip.opacity, 0.5);
                assert_eq!(strip.rect[1], 2.0);
                assert_eq!(strip.rect[3], 13.0);
            }
        }
        assert_eq!(batch.glyphs[0].color[3], 1.0);
        assert_eq!(batch.glyphs[0].outline[3], 1.0);
        assert!((batch.glyphs[0].shadow[3] - 0.55).abs() < 1e-6);
    }

    #[test]
    fn gpu_structs_follow_metal_alignment() {
        assert_eq!(std::mem::size_of::<DanmakuBatchInstance>(), 32);
        assert_eq!(std::mem::offset_of!(DanmakuBatchInstance, glyph_start), 16);
        assert_eq!(std::mem::size_of::<DanmakuBatchGlyph>(), 96);
        assert_eq!(std::mem::offset_of!(DanmakuBatchGlyph, shadow_offset), 80);
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::super::*;
    use crate::danmaku::{DanmakuFrameStats, DanmakuGlyphInstance, DanmakuViewport};
    use std::sync::Arc;

    fn plan(scale: usize, opacity: f32, color: [f32; 3]) -> DanmakuRenderPlan {
        let size = 8 * scale;
        let mut fill = vec![0; size * size];
        let mut outline = fill.clone();
        for y in 0..size {
            for x in 0..size {
                let (x0, y0) = (x / scale, y / scale);
                if (1..7).contains(&x0) && (1..7).contains(&y0) {
                    outline[y * size + x] = 255;
                }
                if (2..6).contains(&x0) && (2..6).contains(&y0) {
                    fill[y * size + x] = 128;
                }
                if (3..5).contains(&x0) && (3..5).contains(&y0) {
                    fill[y * size + x] = 255;
                }
            }
        }
        let stroke = if color == [0.0; 3] { 1.0 } else { 0.0 };
        DanmakuRenderPlan {
            media_time: Duration::ZERO,
            generation: 1,
            viewport: DanmakuViewport::new(32, 24),
            atlas: Some(Arc::new(DanmakuGlyphAtlas {
                width: size as u32,
                height: size as u32,
                stride: size,
                fill_alpha: fill,
                outline_alpha: outline,
                version: scale as u64,
                update: None,
            })),
            items: vec![DanmakuGlyphInstance {
                item_id: 1,
                rect: [4.0, 4.0, 8.0, 8.0],
                tex_rect: [0.0, 0.0, 1.0, 1.0],
                color_rgba: [color[0], color[1], color[2], opacity],
                outline_rgba: [stroke, stroke, stroke, opacity],
                shadow_rgba: [0.0, 0.0, 0.0, opacity * 0.55],
                shadow_offset: [1.0, 1.0],
            }],
            color_atlas: None,
            color_glyphs: Vec::new(),
            frame_stats: DanmakuFrameStats::default(),
        }
    }

    // Exercise the same shader, buffer packing, atlas upload and blend state as
    // the live player, then read GPU pixels (not a CPU copy of the shader).
    fn render(
        renderer: &mut MetalRendererImpl,
        plan: &DanmakuRenderPlan,
        background: f64,
    ) -> Vec<[f32; 4]> {
        let (w, h) = (32, 24);
        let format = renderer.drawable_pixel_format;
        unsafe {
            let descriptor =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    metal_pixel_format(format),
                    w,
                    h,
                    false,
                );
            descriptor.setStorageMode(MTLStorageMode::Private);
            descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            let texture = renderer
                .device
                .newTextureWithDescriptor(&descriptor)
                .unwrap();
            let command = renderer.queue.commandBuffer().unwrap();
            let pass = MTLRenderPassDescriptor::new();
            let attachment = pass.colorAttachments().objectAtIndexedSubscript(0);
            attachment.setTexture(Some(&texture));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: background,
                green: background,
                blue: background,
                alpha: 1.0,
            });
            let encoder = command.renderCommandEncoderWithDescriptor(&pass).unwrap();
            renderer
                .draw_danmaku_plan(
                    &encoder,
                    &command,
                    plan,
                    VideoPresentationLayout::aspect_fit(w as u32, h as u32, w as u32, h as u32),
                    TargetColorState::sdr(ColorPrimaries::Bt709),
                )
                .unwrap();
            encoder.endEncoding();
            let bpp = if format == MetalDrawablePixelFormat::Bgra8Unorm {
                4
            } else {
                8
            };
            let bytes = w * h * bpp;
            let buffer = renderer
                .device
                .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)
                .unwrap();
            let blit = command.blitCommandEncoder().unwrap();
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                &texture, 0, 0, MTLOrigin { x: 0, y: 0, z: 0 }, MTLSize { width: w, height: h, depth: 1 },
                &buffer, 0, w * bpp, bytes);
            blit.endEncoding();
            command.commit();
            command.waitUntilCompleted();
            assert_eq!(command.status(), MTLCommandBufferStatus::Completed);
            let raw = std::slice::from_raw_parts(buffer.contents().as_ptr().cast::<u8>(), bytes);
            raw.chunks_exact(bpp)
                .map(|p| {
                    if bpp == 4 {
                        [
                            p[2] as f32 / 255.0,
                            p[1] as f32 / 255.0,
                            p[0] as f32 / 255.0,
                            p[3] as f32 / 255.0,
                        ]
                    } else {
                        std::array::from_fn(|i| {
                            half_to_f32(u16::from_le_bytes([p[2 * i], p[2 * i + 1]]))
                        })
                    }
                })
                .collect()
        }
    }

    fn near(actual: [f32; 4], rgb: [f32; 3]) {
        for i in 0..3 {
            assert!(
                (actual[i] - rgb[i]).abs() <= 2.0 / 255.0,
                "actual={actual:?}, expected={rgb:?}"
            );
        }
        assert!((actual[3] - 1.0).abs() < 0.001);
    }

    fn renderer(edr: bool) -> MetalRendererImpl {
        let mut r = MetalRendererImpl::new(MetalRendererConfig::default())
            .expect("Metal device required for pixel regression");
        // This test renders offscreen, so configure the target explicitly.
        if edr {
            r.output_mode = MetalOutputMode::apple_edr(4.0);
            r.drawable_pixel_format = MetalDrawablePixelFormat::Rgba16Float;
        }
        r
    }

    #[test]
    fn danmaku_gpu_opacity_applies_once_to_fill_outline_shadow_and_antialiasing() {
        for edr in [false, true] {
            let mut r = renderer(edr);
            for scale in [1, 2] {
                for opacity in [0.0, 0.25, 0.5, 0.8, 1.0] {
                    for background in [0.0, 0.5, 1.0] {
                        for color in [[1.0; 3], [0.0; 3], [1.0, 0.0, 0.0]] {
                            let plan = plan(scale, opacity, color);
                            let pixels = render(&mut r, &plan, background as f64);
                            let expected =
                                color.map(|c| c * opacity + background * (1.0 - opacity));
                            near(pixels[7 * 32 + 7], expected);
                            let stroke = plan.items[0].outline_rgba[0];
                            near(
                                pixels[7 * 32 + 5],
                                [stroke * opacity + background * (1.0 - opacity); 3],
                            );
                            let f = 128.0 / 255.0;
                            near(
                                pixels[7 * 32 + 6],
                                color.map(|c| {
                                    (c * f + stroke * (1.0 - f)) * opacity
                                        + background * (1.0 - opacity)
                                }),
                            );
                            near(
                                pixels[11 * 32 + 11],
                                [background * (1.0 - 0.55 * opacity); 3],
                            );
                            near(pixels[0], [background; 3]);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn danmaku_gpu_moving_strip_boundaries_have_no_seams_or_double_opacity() {
        let mut r = renderer(false);
        let mut p = plan(2, 0.5, [1.0; 3]);
        let atlas = Arc::make_mut(p.atlas.as_mut().unwrap());
        atlas.fill_alpha.fill(255);
        atlas.outline_alpha.fill(255);
        atlas.version = 99;
        p.items.push(p.items[0].clone());
        for shift in [0.0, 0.125, 0.25, 0.5, 0.75, 0.875] {
            p.items[0].rect[0] = 4.0 + shift;
            p.items[1].rect[0] = 9.0 + shift;
            let pixels = render(&mut r, &p, 0.0);
            for x in 5..17 {
                near(pixels[7 * 32 + x], [0.5; 3]);
            }
        }
    }

    #[test]
    fn danmaku_gpu_overlapping_glyphs_are_one_group_but_comments_remain_independent() {
        let mut r = renderer(false);
        let mut p = plan(2, 0.5, [1.0; 3]);
        p.items.push(p.items[0].clone());
        let pixels = render(&mut r, &p, 0.0);
        near(pixels[7 * 32 + 7], [0.5; 3]);
        // Neighboring glyph's black outline must not pollute this glyph's fill.
        p.items[1].rect[0] = 7.0;
        let pixels = render(&mut r, &p, 0.0);
        near(pixels[7 * 32 + 8], [0.5; 3]);
        // A different comment really is a separate translucent layer.
        p.items[1].rect[0] = 4.0;
        p.items[1].item_id = 2;
        p.items[1].color_rgba = [1.0, 0.0, 0.0, 0.5];
        let pixels = render(&mut r, &p, 0.0);
        near(pixels[7 * 32 + 7], [0.75, 0.25, 0.25]);
        // Negative shadow offsets must stay inside the strip's coverage.
        p.items.truncate(1);
        p.items[0].shadow_offset = [-3.0, -2.0];
        let pixels = render(&mut r, &p, 1.0);
        near(pixels[3 * 32 + 2], [1.0 - 0.5 * 0.55; 3]);
    }
}
