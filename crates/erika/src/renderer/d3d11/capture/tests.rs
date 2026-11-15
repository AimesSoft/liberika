//! Pixel checks run the production D3D11 shaders. Use
//! ERIKA_D3D11_CAPTURE_TEST_DRIVER=hardware for the physical GPU (default: WARP).
use super::*;
use crate::core::FlutterTextureHandle;
use crate::danmaku::{DanmakuColorAtlas, DanmakuColorGlyph, DanmakuViewport};
use crate::overlay::OverlayViewport;
use crate::renderer::d3d11::tests::{gpu_test_renderer, read_bgra_pixels};
use crate::renderer::pipeline::ColorRange;
use crate::subtitle::SubtitleBitmapPlane;
use std::sync::Arc;

fn renderer() -> D3d11Renderer {
    let driver = match std::env::var("ERIKA_D3D11_CAPTURE_TEST_DRIVER").as_deref() {
        Ok("hardware") => D3D_DRIVER_TYPE_HARDWARE,
        Ok("warp") | Err(std::env::VarError::NotPresent) => {
            ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP
        }
        other => panic!("expected capture driver hardware or warp, got {other:?}"),
    };
    let renderer = gpu_test_renderer(driver, D3D_FEATURE_LEVEL_11_0);
    let dxgi: IDXGIDevice = renderer.state.as_ref().unwrap().device.cast().unwrap();
    let desc = unsafe { dxgi.GetAdapter().unwrap().GetDesc().unwrap() };
    let end = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    eprintln!(
        "capture GPU: {}",
        String::from_utf16_lossy(&desc.Description[..end])
    );
    renderer
}

fn upload(renderer: &mut D3d11Renderer, width: u32, height: u32, y: u8, uv: [u8; 2]) {
    renderer
        .upload_planar_frame(
            PlanarFrame {
                format: PlanarPixelFormat::Nv12,
                width,
                height,
                luma: vec![y; (width * height) as usize],
                chroma: uv.repeat((width.div_ceil(2) * height.div_ceil(2)) as usize),
            },
            SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Bt1886)
                .range(ColorRange::Limited),
        )
        .unwrap();
}

fn capture(renderer: &mut D3d11Renderer, width: u32, height: u32) -> RendererFrameCapture {
    renderer
        .capture_current_frame(RenderFrameContext::new(Duration::ZERO, 1), width, height)
        .unwrap()
        .expect("uploaded video must produce a capture")
}

fn pixel(capture: &RendererFrameCapture, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * capture.width + x) * 4) as usize;
    capture.rgba[offset..offset + 4].try_into().unwrap()
}

#[test]
fn capture_headless_rgba_order_row_pitch_letterbox_and_validation() {
    let mut renderer = renderer();
    assert!(
        renderer
            .capture_current_frame(RenderFrameContext::new(Duration::ZERO, 1), 7, 5)
            .unwrap()
            .is_none()
    );
    upload(&mut renderer, 3, 3, 128, [64, 192]);
    let token = renderer.current_video.as_ref().unwrap().frame_token;
    assert!(
        renderer
            .capture_current_frame(RenderFrameContext::new(Duration::ZERO, 1), 0, 5)
            .is_err()
    );
    assert!(
        renderer
            .capture_current_frame(RenderFrameContext::new(Duration::ZERO, 1), u32::MAX, 5)
            .is_err()
    );
    let image = capture(&mut renderer, 7, 5);
    assert_eq!((image.width, image.height, image.rgba.len()), (7, 5, 140));
    let red = pixel(&image, 3, 2);
    assert!(
        red[0] > 220 && red[2] < 30 && red[3] == 255,
        "RGBA order: {red:?}"
    );
    for y in 0..5 {
        assert_eq!(pixel(&image, 0, y), [0, 0, 0, 255]);
        assert_eq!(pixel(&image, 6, y), [0, 0, 0, 255]);
        assert_eq!(
            pixel(&image, 3, y),
            red,
            "row {y} must exclude staging padding"
        );
    }
    assert!(renderer.surface.is_none());
    assert_eq!(renderer.current_video.as_ref().unwrap().frame_token, token);
    renderer.clear_current_frame().unwrap();
    assert!(renderer.current_video.is_none());
}

#[test]
fn capture_composites_subtitles_and_matching_optional_danmaku() {
    let mut renderer = renderer();
    upload(&mut renderer, 4, 4, 16, [128, 128]);
    let overlay = OverlayFrame {
        pts: Duration::ZERO,
        viewport: OverlayViewport::new(4, 4),
        subtitle_planes: vec![SubtitleBitmapPlane::new(
            0,
            0,
            4,
            4,
            [255, 0, 0, 255].repeat(16),
        )],
        subtitle_alpha_planes: vec![],
        subtitle_changed: true,
    };
    let mut plan = DanmakuRenderPlan::empty(Duration::ZERO, 1, DanmakuViewport::new(8, 8));
    plan.color_atlas = Some(Arc::new(DanmakuColorAtlas {
        width: 1,
        height: 1,
        rgba: vec![0, 255, 0, 255],
        version: 1,
    }));
    plan.color_glyphs.push(DanmakuColorGlyph {
        rect: [2.0, 2.0, 4.0, 4.0],
        tex_rect: [0.0, 0.0, 1.0, 1.0],
        opacity: 1.0,
    });
    let context = RenderFrameContext::new(Duration::ZERO, 1).overlay(Some(&overlay));
    let subtitle_only = renderer
        .capture_current_frame(context, 8, 8)
        .unwrap()
        .unwrap();
    assert_eq!(pixel(&subtitle_only, 4, 4), [255, 0, 0, 255]);
    let with_danmaku = renderer
        .capture_current_frame(context.danmaku(Some(&plan)), 8, 8)
        .unwrap()
        .unwrap();
    assert_eq!(pixel(&with_danmaku, 4, 4), [0, 255, 0, 255]);
    assert_eq!(pixel(&with_danmaku, 0, 0), [255, 0, 0, 255]);
    plan.generation = 2;
    assert_eq!(
        renderer
            .capture_current_frame(context.danmaku(Some(&plan)), 8, 8)
            .unwrap()
            .unwrap(),
        subtitle_only
    );
    plan.generation = 1;
    plan.viewport = DanmakuViewport::new(9, 8);
    assert_eq!(
        renderer
            .capture_current_frame(context.danmaku(Some(&plan)), 8, 8)
            .unwrap()
            .unwrap(),
        subtitle_only
    );
}

#[test]
fn capture_hdr_uses_sdr_tone_mapping_without_changing_live_uniforms() {
    let mut renderer = renderer();
    let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
        .range(ColorRange::Limited);
    renderer
        .upload_planar_frame(
            PlanarFrame {
                format: PlanarPixelFormat::P010,
                width: 4,
                height: 4,
                luma: [500u16 << 6]
                    .repeat(16)
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
                chroma: [512u16 << 6]
                    .repeat(8)
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            },
            source,
        )
        .unwrap();
    // The live HDR pass uses linear HDR uniforms. Capture must ignore that
    // target encoding while retaining the source's PQ/10-bit metadata.
    let hdr = constants_for_frame(
        source,
        D3d11VideoTextureFormat::P010,
        D3d11OutputMode::Hdr10.target_color(),
    );
    renderer.current_video.as_mut().unwrap().constants = hdr;
    let status = renderer.output_status();
    let image = capture(&mut renderer, 4, 4);
    let gray = pixel(&image, 2, 2);
    assert!(
        gray[0] > 140 && gray[0] < 254,
        "tone-mapped gray must retain headroom: {gray:?}"
    );
    assert!(gray[0].abs_diff(gray[1]) < 3 && gray[0].abs_diff(gray[2]) < 3);
    assert_eq!(gray[3], 255);
    assert_eq!(renderer.current_video.as_ref().unwrap().constants, hdr);
    assert_eq!(renderer.output_status(), status);
}

#[test]
fn capture_packed_alpha_uses_logical_width_and_transparent_letterbox() {
    let mut renderer = renderer();
    renderer.video_alpha_mode = VideoAlphaMode::PackedAlphaRight;
    renderer
        .upload_planar_frame(
            PlanarFrame {
                format: PlanarPixelFormat::Nv12,
                width: 8,
                height: 4,
                // White color on the left, 50% alpha on the right.
                luma: [235, 235, 235, 235, 126, 126, 126, 126].repeat(4),
                chroma: vec![128; 16],
            },
            SourceColorState::new(ColorPrimaries::Bt709, TransferFunction::Bt1886)
                .range(ColorRange::Limited),
        )
        .unwrap();
    let image = capture(&mut renderer, 8, 4);
    assert_eq!(pixel(&image, 0, 2), [0; 4]);
    assert_eq!(pixel(&image, 7, 2), [0; 4]);
    let center = pixel(&image, 3, 2);
    assert!(
        center[..3].iter().all(|&c| c >= 253),
        "straight RGBA white: {center:?}"
    );
    assert!((126..=130).contains(&center[3]), "half alpha: {center:?}");
}

#[test]
fn capture_preserves_published_surface_and_subsequent_live_draw() {
    let mut renderer = renderer();
    renderer
        .attach_surface(PlatformSurface::FlutterTexture(FlutterTextureHandle::new(
            FlutterTextureKind::WindowsTextureRegistrar,
            1,
            6,
            4,
            1.0,
        )))
        .unwrap();
    upload(&mut renderer, 6, 4, 128, [64, 192]);
    let context = RenderFrameContext::new(Duration::ZERO, 1);
    renderer.render_current_frame(context).unwrap();
    let published = renderer
        .surface
        .as_ref()
        .unwrap()
        .published_texture
        .as_ref()
        .unwrap()
        .clone();
    let before = read_bgra_pixels(renderer.state.as_ref().unwrap(), &published);
    let status = renderer.output_status();
    let scene_token = renderer.flutter_scene.as_ref().unwrap().frame_token;
    let capture = capture(&mut renderer, 7, 5);
    assert_eq!(capture.rgba.len(), 140);
    assert_eq!(
        renderer
            .surface
            .as_ref()
            .unwrap()
            .published_texture
            .as_ref()
            .unwrap()
            .as_raw(),
        published.as_raw()
    );
    assert_eq!(renderer.output_status(), status);
    assert_eq!(
        renderer.flutter_scene.as_ref().unwrap().frame_token,
        scene_token
    );
    assert_eq!(
        read_bgra_pixels(renderer.state.as_ref().unwrap(), &published),
        before
    );
    // Force a real draw after capture, rather than a cached scene hit.
    renderer.flutter_scene = None;
    renderer.render_current_frame(context).unwrap();
    let after = renderer
        .surface
        .as_ref()
        .unwrap()
        .published_texture
        .as_ref()
        .unwrap();
    assert_eq!(
        read_bgra_pixels(renderer.state.as_ref().unwrap(), after),
        before
    );
    renderer.clear_current_frame().unwrap();
}

#[test]
fn capture_fence_protects_headless_video_retirement() {
    let mut renderer = renderer();
    upload(&mut renderer, 4, 4, 128, [128, 128]);
    let fence = create_d3d11_event_query(&renderer.state.as_ref().unwrap().device).unwrap();
    // An unsubmitted event cannot complete, modeling the pending fence that a
    // capture readback timeout leaves behind without stalling a physical GPU.
    renderer.current_video_fence = Some(fence.clone());
    renderer.handoff_timeout = Duration::ZERO;
    assert!(renderer.retire_current_video().is_err());
    assert!(renderer.current_video.is_some());
    assert!(renderer.surface.is_none());
    unsafe { renderer.state.as_ref().unwrap().context.End(&fence) };
    renderer.handoff_timeout = Duration::from_secs(10);
    renderer.retire_current_video().unwrap();
    assert!(renderer.current_video.is_none());
}
