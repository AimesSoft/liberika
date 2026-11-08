//! Opt-in visual evidence from the actual Windows GPU, not a CPU shader copy.
use super::*;
use crate::danmaku::{
    DanmakuColor, DanmakuItem, DanmakuLayoutConfig, DanmakuMode, DanmakuTimeline, DanmakuViewport,
    DfmLayoutEngine,
};
use std::{fs::File, io::BufWriter, path::Path};

const WIDTH: u32 = 600;
const HEIGHT: u32 = 360;

fn real_text_plan() -> DanmakuRenderPlan {
    let timeline = DanmakuTimeline::new(vec![DanmakuItem {
        id: 1,
        pts: Duration::ZERO,
        text: "弹幕不透明度 Danmaku".into(),
        mode: DanmakuMode::Top,
        font_size: 25.0,
        color: DanmakuColor::WHITE,
        opacity: 1.0,
        is_self: false,
    }])
    .unwrap();
    let mut engine = DfmLayoutEngine::new(
        timeline,
        DanmakuLayoutConfig {
            font_size: 38.0,
            ..Default::default()
        },
    );
    let mut plan = engine.render_plan(
        Duration::from_millis(500),
        DanmakuViewport::new(WIDTH, HEIGHT),
        1,
    );
    assert!(!plan.items.is_empty());
    let glyphs = std::mem::take(&mut plan.items);
    let left = glyphs
        .iter()
        .map(|g| g.rect[0])
        .fold(f32::INFINITY, f32::min);
    let top = glyphs
        .iter()
        .map(|g| g.rect[1])
        .fold(f32::INFINITY, f32::min);
    let right = glyphs
        .iter()
        .map(|g| g.rect[0] + g.rect[2])
        .fold(0.0, f32::max);
    assert!(right - left < WIDTH as f32 - 32.0);
    for (row, opacity) in [1.0, 0.75, 0.5, 0.25, 0.0].into_iter().enumerate() {
        for original in &glyphs {
            let mut glyph = original.clone();
            glyph.item_id = row as u64 + 1;
            glyph.rect[0] += 16.0 - left;
            glyph.rect[1] += 14.0 + row as f32 * 72.0 - top;
            glyph.color_rgba[3] *= opacity;
            glyph.outline_rgba[3] *= opacity;
            glyph.shadow_rgba[3] *= opacity;
            plan.items.push(glyph);
        }
    }
    plan
}

fn background_bgra() -> Vec<u8> {
    let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let rgb = if x < 200 {
                [255, 255, 255]
            } else if x < 400 {
                if ((x / 24) + (y / 24)) % 2 == 0 {
                    [148, 198, 220]
                } else {
                    [227, 211, 176]
                }
            } else {
                [43, 69, 94]
            };
            pixels.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
    }
    pixels
}

fn save_png(gpu: &Offscreen, path: &Path) {
    let pixels = read_bgra_pixels(gpu.renderer.state.as_ref().unwrap(), &gpu.texture);
    let rgba: Vec<u8> = pixels
        .into_iter()
        .flat_map(|[b, g, r, a]| [r, g, b, a])
        .collect();
    let mut encoder = png::Encoder::new(BufWriter::new(File::create(path).unwrap()), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&rgba)
        .unwrap();
}

#[test]
#[ignore = "writes opt-in GPU comparison images; set ERIKA_DANMAKU_CAPTURE_DIR"]
fn d3d11_danmaku_capture_before_after() {
    let directory =
        std::env::var_os("ERIKA_DANMAKU_CAPTURE_DIR").expect("capture directory required");
    let directory = Path::new(&directory);
    std::fs::create_dir_all(directory).unwrap();
    let mut gpu = Offscreen::new(D3D_FEATURE_LEVEL_11_0, false);
    (gpu.texture, gpu.render_target) =
        create_flutter_texture_target(&gpu.renderer.state.as_ref().unwrap().device, WIDTH, HEIGHT)
            .unwrap();
    let plan = real_text_plan();
    let rect = D3d11DrawRect {
        x: 0.0,
        y: 0.0,
        width: WIDTH as f32,
        height: HEIGHT as f32,
    };
    let background = background_bgra();
    let (fill, outline) = gpu
        .renderer
        .prepare_danmaku_atlas_textures(plan.atlas.as_ref().unwrap())
        .unwrap();
    let mut legacy = Vec::new();
    for glyph in &plan.items {
        let mut shadow_rect = glyph.rect;
        shadow_rect[0] += glyph.shadow_offset[0];
        shadow_rect[1] += glyph.shadow_offset[1];
        for (texture, rect, color) in [
            (outline.clone(), shadow_rect, glyph.shadow_rgba),
            (outline.clone(), glyph.rect, glyph.outline_rgba),
            (fill.clone(), glyph.rect, glyph.color_rgba),
        ] {
            legacy.push(D3d11OverlayDraw {
                texture,
                constants: OverlayUniforms {
                    rect,
                    tex_rect: glyph.tex_rect,
                    viewport: [WIDTH as f32, HEIGHT as f32],
                    overlay_mode: 1,
                    reserved0: 0,
                    color,
                    ui_nits: overlay_ui_nits(gpu.target),
                },
            });
        }
    }
    let corrected = gpu
        .renderer
        .prepare_danmaku_draws_for_target(Some(&plan), gpu.target)
        .unwrap();
    let state = gpu.renderer.state.as_ref().unwrap();
    for old in [true, false] {
        unsafe {
            state.context.UpdateSubresource(
                &gpu.texture,
                0,
                None,
                background.as_ptr().cast(),
                WIDTH * 4,
                0,
            );
        }
        if old {
            state
                .draw_overlays(&legacy, &gpu.render_target, rect)
                .unwrap();
        } else {
            state
                .draw_danmaku(&corrected, &gpu.render_target, rect)
                .unwrap();
        }
        save_png(
            &gpu,
            &directory.join(if old { "before.png" } else { "after.png" }),
        );
    }
    eprintln!(
        "GPU comparison captured: {} (100/75/50/25/0 percent from top)",
        directory.display()
    );
}
