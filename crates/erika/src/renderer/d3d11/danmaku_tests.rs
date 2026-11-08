//! These tests use the production shaders, upload and DrawInstanced path on an
//! offscreen texture. They never attach a surface or open/activate a window.
//! Default: WARP. Set ERIKA_D3D11_DANMAKU_TEST_DRIVER=hardware to exercise a GPU.

use super::*;
use crate::danmaku::DanmakuColorGlyph;
use crate::renderer::d3d11::tests::{gpu_test_renderer, read_bgra_pixels, read_texture_rows};
use crate::renderer::danmaku_batch::test_support::{
    check_boundaries, check_opacity, check_overlap, check_target_alpha, plan,
};
use ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
use std::sync::Arc;

struct Offscreen {
    renderer: D3d11Renderer,
    texture: ID3D11Texture2D,
    render_target: ID3D11RenderTargetView,
    target: TargetColorState,
    linear: bool,
}

impl Offscreen {
    fn new(feature_level: D3D_FEATURE_LEVEL, linear: bool) -> Self {
        let driver = match std::env::var("ERIKA_D3D11_DANMAKU_TEST_DRIVER").as_deref() {
            Ok("hardware") => D3D_DRIVER_TYPE_HARDWARE,
            Ok("warp") | Err(std::env::VarError::NotPresent) => D3D_DRIVER_TYPE_WARP,
            other => {
                panic!("expected ERIKA_D3D11_DANMAKU_TEST_DRIVER=hardware or warp, got {other:?}")
            }
        };
        let renderer = gpu_test_renderer(driver, feature_level);
        let device = &renderer.state.as_ref().unwrap().device;
        assert_eq!(unsafe { device.GetFeatureLevel() }, feature_level);
        let dxgi: IDXGIDevice = device.cast().unwrap();
        let desc = unsafe { dxgi.GetAdapter().unwrap().GetDesc().unwrap() };
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        eprintln!(
            "danmaku GPU: {} FL={:#x} linear={linear}",
            String::from_utf16_lossy(&desc.Description[..end]),
            feature_level.0
        );
        let (texture, render_target) = if linear {
            let (rtv, srv) = create_linear_render_target(device, 32, 24).unwrap();
            let texture = unsafe { srv.GetResource() }
                .unwrap()
                .cast::<ID3D11Texture2D>()
                .unwrap();
            (texture, rtv)
        } else {
            create_flutter_texture_target(device, 32, 24).unwrap()
        };
        Self {
            renderer,
            texture,
            render_target,
            target: if linear {
                TargetColorState::hdr10(ColorPrimaries::Bt2020)
            } else {
                TargetColorState::sdr(ColorPrimaries::Bt709)
            },
            linear,
        }
    }

    fn clear(&self, background: [f32; 4]) {
        unsafe {
            self.renderer
                .state
                .as_ref()
                .unwrap()
                .context
                .ClearRenderTargetView(&self.render_target, &background);
        }
    }

    fn render(&mut self, plan: &DanmakuRenderPlan, background: [f32; 4]) -> Vec<[f32; 4]> {
        let draws = self
            .renderer
            .prepare_danmaku_draws_for_target(Some(plan), self.target)
            .unwrap();
        self.clear(background);
        self.renderer
            .state
            .as_ref()
            .unwrap()
            .draw_danmaku(&draws, &self.render_target, viewport())
            .unwrap();
        self.read()
    }

    fn read(&self) -> Vec<[f32; 4]> {
        let state = self.renderer.state.as_ref().unwrap();
        if self.linear {
            read_texture_rows(state, &self.texture, 8)
                .chunks_exact(8)
                .map(|pixel| {
                    std::array::from_fn(|i| {
                        from_f16(u16::from_le_bytes([pixel[i * 2], pixel[i * 2 + 1]]))
                    })
                })
                .collect()
        } else {
            read_bgra_pixels(state, &self.texture)
                .into_iter()
                .map(|[b, g, r, a]| [r, g, b, a].map(|value| f32::from(value) / 255.0))
                .collect()
        }
    }
}

fn viewport() -> D3d11DrawRect {
    D3d11DrawRect {
        x: 0.0,
        y: 0.0,
        width: 32.0,
        height: 24.0,
    }
}

// No half-float dependency is needed just to inspect the production HDR target.
fn from_f16(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = u32::from((bits >> 10) & 31);
    let fraction = u32::from(bits & 1023);
    if exponent == 0 {
        let value = fraction as f32 / 16_777_216.0;
        return if sign == 0 { value } else { -value };
    }
    let exponent = if exponent == 31 { 255 } else { exponent + 112 };
    f32::from_bits(sign | (exponent << 23) | (fraction << 13))
}

#[test]
fn d3d11_danmaku_shader_compiles_for_feature_level_10() {
    compile_shader_source(SHADER, "danmaku_vs_main", "vs_4_0").unwrap();
    compile_shader_source(SHADER, "danmaku_ps_main", "ps_4_0").unwrap();
    assert_eq!(mem::size_of::<DanmakuBatchInstance>(), 32);
    assert_eq!(mem::offset_of!(DanmakuBatchInstance, rect), 0);
    assert_eq!(mem::offset_of!(DanmakuBatchInstance, glyph_start), 16);
    assert_eq!(mem::offset_of!(DanmakuBatchInstance, glyph_count), 20);
    assert_eq!(mem::offset_of!(DanmakuBatchInstance, opacity), 24);
    assert_eq!(mem::size_of::<DanmakuUniforms>(), 32);
    assert_eq!(mem::offset_of!(DanmakuUniforms, ui_nits), 16);
}

fn check_matrix(feature_level: D3D_FEATURE_LEVEL, linear: bool) {
    let mut gpu = Offscreen::new(feature_level, linear);
    check_opacity(|p, background| gpu.render(p, background));
    check_boundaries(|p, background| gpu.render(p, background));
    check_overlap(|p, background| gpu.render(p, background));
    check_target_alpha(|p, background| gpu.render(p, background));
}

#[test]
fn d3d11_danmaku_sdr_gpu_pixels() {
    check_matrix(D3D_FEATURE_LEVEL_11_0, false);
}

#[test]
fn d3d11_danmaku_hdr_linear_gpu_pixels() {
    check_matrix(D3D_FEATURE_LEVEL_11_0, true);
}

#[test]
fn d3d11_danmaku_feature_level_10_gpu_pixels() {
    check_matrix(D3D_FEATURE_LEVEL_10_0, false);
    check_matrix(D3D_FEATURE_LEVEL_10_0, true);
}

#[test]
fn d3d11_danmaku_old_per_effect_opacity_was_wrong() {
    let mut gpu = Offscreen::new(D3D_FEATURE_LEVEL_11_0, false);
    let p = plan(1, 0.5, [1.0; 3]);
    let (fill, outline) = gpu
        .renderer
        .prepare_danmaku_atlas_textures(p.atlas.as_ref().unwrap())
        .unwrap();
    let item = &p.items[0];
    let mut shadow_rect = item.rect;
    shadow_rect[0] += item.shadow_offset[0];
    shadow_rect[1] += item.shadow_offset[1];
    // Deliberately reconstruct the old bug only in this regression test. Each
    // effect used to apply the same comment opacity to the framebuffer again.
    let old_draws = [
        (outline.clone(), shadow_rect, item.shadow_rgba),
        (outline, item.rect, item.outline_rgba),
        (fill, item.rect, item.color_rgba),
    ]
    .into_iter()
    .map(|(texture, rect, color)| D3d11OverlayDraw {
        texture,
        constants: OverlayUniforms {
            rect,
            tex_rect: item.tex_rect,
            viewport: [32.0, 24.0],
            overlay_mode: 1,
            reserved0: 0,
            color,
            ui_nits: overlay_ui_nits(gpu.target),
        },
    })
    .collect::<Vec<_>>();
    gpu.clear([1.0; 4]);
    gpu.renderer
        .state
        .as_ref()
        .unwrap()
        .draw_overlays(&old_draws, &gpu.render_target, viewport())
        .unwrap();
    let old = gpu.read()[7 * 32 + 7];
    for value in &old[..3] {
        assert!((*value - 0.68125).abs() <= 2.0 / 255.0, "old: {old:?}");
    }
    let corrected = gpu.render(&p, [1.0; 4])[7 * 32 + 7];
    assert_eq!(corrected, [1.0; 4], "old={old:?}, corrected={corrected:?}");
    eprintln!("white text over white at opacity=0.5: old={old:?}, corrected={corrected:?}");
}

#[test]
fn d3d11_danmaku_hdr_converts_rgb_before_group_composition() {
    let mut gpu = Offscreen::new(D3D_FEATURE_LEVEL_11_0, true);
    let p = plan(1, 0.5, [0.5, 0.25, 0.75]);
    let pixels = gpu.render(&p, [0.1, 0.1, 0.1, 1.0]);
    let expected = [0.5_f32, 0.25, 0.75].map(|c| c.powf(2.2) * 0.5 + 0.05);
    for (actual, expected) in pixels[7 * 32 + 7][..3].iter().zip(expected) {
        assert!((actual - expected).abs() < 0.002, "{actual} != {expected}");
    }
}

#[test]
fn d3d11_danmaku_preserves_premultiplied_emoji_and_overlay_state() {
    for linear in [false, true] {
        let mut gpu = Offscreen::new(D3D_FEATURE_LEVEL_11_0, linear);
        let mut p = plan(1, 0.5, [1.0; 3]);
        p.color_atlas = Some(Arc::new(DanmakuColorAtlas {
            width: 1,
            height: 1,
            rgba: vec![128, 0, 0, 128],
            version: 1,
        }));
        p.color_glyphs.push(DanmakuColorGlyph {
            rect: [6.0, 6.0, 4.0, 4.0],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            opacity: 0.5,
        });
        let alpha = 128.0 / 255.0 * 0.5;
        for text in [true, false] {
            if !text {
                p.items.clear();
                p.atlas = None;
            }
            let pixel = gpu.render(&p, [0.0, 0.0, 0.0, 1.0])[7 * 32 + 7];
            let base = if text { 0.5 } else { 0.0 };
            let expected = [
                alpha + base * (1.0 - alpha),
                base * (1.0 - alpha),
                base * (1.0 - alpha),
                1.0,
            ];
            for (a, e) in pixel.into_iter().zip(expected) {
                assert!((a - e).abs() < 2.0 / 255.0, "{pixel:?} != {expected:?}");
            }
        }
    }
}

#[test]
fn d3d11_danmaku_loads_glyph_texture_rows_on_feature_level_10() {
    let mut gpu = Offscreen::new(D3D_FEATURE_LEVEL_10_0, false);
    let mut p = plan(1, 0.5, [1.0; 3]);
    let visible = p.items[0].clone();
    p.items = (0..1400)
        .map(|i| {
            let mut glyph = visible.clone();
            glyph.item_id = i + 1;
            glyph.rect[0] = 64.0;
            glyph.color_rgba = [1.0, 0.0, 0.0, 0.5];
            glyph
        })
        .collect();
    let mut last = visible;
    last.item_id = 1401;
    p.items.push(last);
    let draws = gpu
        .renderer
        .prepare_danmaku_draws_for_target(Some(&p), gpu.target)
        .unwrap();
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        draws
            .text
            .as_ref()
            .unwrap()
            .glyph_data
            ._texture
            .GetDesc(&mut desc);
    }
    assert!(desc.Height > 1);
    assert!(desc.Width <= 8192);
    gpu.clear([0.0, 0.0, 0.0, 1.0]);
    gpu.renderer
        .state
        .as_ref()
        .unwrap()
        .draw_danmaku(&draws, &gpu.render_target, viewport())
        .unwrap();
    let pixel = gpu.read()[7 * 32 + 7];
    for value in &pixel[..3] {
        assert!((*value - 0.5).abs() <= 2.0 / 255.0, "{pixel:?}");
    }
}

#[path = "danmaku_capture.rs"]
mod capture;
