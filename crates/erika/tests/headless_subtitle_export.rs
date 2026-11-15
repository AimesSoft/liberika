//! Manual end-to-end regression for #197 using real decode, libass, and Metal export.
//! Set ERIKA_HEADLESS_SUBTITLE_FIXTURE to an eight-second 1080p black H.264 MKV
//! containing a white ASS cue from 0 to 8 seconds, then run this ignored test
//! with --features wgpu -- --ignored --nocapture. No surface is attached.
#![cfg(all(feature = "wgpu", feature = "libass", target_os = "macos"))]

use std::time::{Duration, Instant};

use erika::core::{MediaRequest, RendererBackendPreference, TrackKind};
use erika::playback::VideoDecodePreference;
use erika::presenter::{PresenterConfig, PresenterRuntime};
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder,
    MTLCommandQueue, MTLDevice, MTLOrigin, MTLResource, MTLResourceOptions, MTLSize, MTLTexture,
};

fn read_export(frame: &erika::ExternalTextureFrame) -> (String, Vec<u8>) {
    let info = frame.info();
    assert_eq!(info.kind, erika::ExternalTextureKind::Metal);
    assert_eq!((info.width, info.height), (640, 360));
    // Read the exported native handle as an independent Metal consumer. This
    // is test-only readback, not a second presenter capture/render operation.
    let texture = unsafe { &*(info.native_handle as *const ProtocolObject<dyn MTLTexture>) };
    let device = texture.device();
    assert_eq!(device.registryID(), info.device_id);
    let queue = device.newCommandQueue().unwrap();
    let command = queue.commandBuffer().unwrap();
    let row_bytes = info.width as usize * 4;
    let length = row_bytes * info.height as usize;
    let buffer = device
        .newBufferWithLength_options(length, MTLResourceOptions::StorageModeShared)
        .unwrap();
    let blit = command.blitCommandEncoder().unwrap();
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
            texture,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
            MTLSize { width: info.width as usize, height: info.height as usize, depth: 1 },
            &buffer,
            0,
            row_bytes,
            length,
        );
    }
    blit.endEncoding();
    command.commit();
    command.waitUntilCompleted();
    assert_eq!(command.status(), MTLCommandBufferStatus::Completed);
    let pixels = unsafe {
        std::slice::from_raw_parts(buffer.contents().as_ptr().cast::<u8>(), length).to_vec()
    };
    (device.name().to_string(), pixels)
}

#[test]
#[ignore = "requires a real Metal GPU and ERIKA_HEADLESS_SUBTITLE_FIXTURE"]
fn real_1080p_ass_playback_exports_subtitles_without_a_surface_overlay() {
    let fixture = std::env::var("ERIKA_HEADLESS_SUBTITLE_FIXTURE")
        .expect("set ERIKA_HEADLESS_SUBTITLE_FIXTURE to the generated 1080p ASS MKV");
    let expect_redundant = std::env::var("ERIKA_EXPECT_REDUNDANT_OVERLAY").as_deref() == Ok("1");
    let mut config = PresenterConfig::default();
    config.player.renderer = RendererBackendPreference::WgpuFallback;
    config.player.playback.video_decode = VideoDecodePreference::Software;
    let mut presenter = PresenterRuntime::new(config).unwrap();
    presenter.open(MediaRequest::new(fixture)).unwrap();
    let subtitle = presenter
        .tracks()
        .into_iter()
        .find(|track| track.kind == TrackKind::Subtitle)
        .expect("fixture must contain an embedded ASS track");
    presenter.select_subtitle_track(Some(subtitle.id)).unwrap();
    presenter.play().unwrap();

    let started = Instant::now();
    let mut exported = 0;
    let mut inspected = 0;
    let mut subtitle_pixels_min = usize::MAX;
    let mut subtitle_pixels_max = 0;
    let mut device_name = String::new();
    let mut sample = None;
    while started.elapsed() < Duration::from_secs(4) {
        if let Some(frame) = presenter.export_frame_texture(640, 360, 1.0).unwrap() {
            exported += 1;
            let snapshot = presenter.runtime_snapshot();
            assert!(!snapshot.renderer.attached);
            assert_eq!(snapshot.renderer.surface_width, 0);
            assert_eq!(snapshot.renderer.surface_height, 0);
            if snapshot.media_time >= Duration::from_millis(500) {
                let (name, pixels) = read_export(&frame);
                device_name = name;
                let white_pixels = pixels
                    .chunks_exact(4)
                    .filter(|pixel| pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200)
                    .count();
                assert!(
                    white_pixels > 50,
                    "export lost the white ASS cue: {white_pixels}"
                );
                subtitle_pixels_min = subtitle_pixels_min.min(white_pixels);
                subtitle_pixels_max = subtitle_pixels_max.max(white_pixels);
                inspected += 1;
                sample = Some(pixels);
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    let snapshot = presenter.runtime_snapshot();
    let diagnostics = presenter.diagnostics_snapshot();
    println!(
        "HEADLESS_ASS_RESULT {}",
        serde_json::json!({
            "expectRedundantOverlay": expect_redundant,
            "backend": "wgpu/Metal",
            "device": device_name,
            "decoder": diagnostics["decoder"],
            "sourceSize": [1920, 1080],
            "exportSize": [640, 360],
            "wallSeconds": started.elapsed().as_secs_f64(),
            "mediaSeconds": snapshot.media_time.as_secs_f64(),
            "exportedFrames": exported,
            "inspectedFrames": inspected,
            "decodedFrames": snapshot.stats.decoded_video_frames,
            "decodedSubtitleFrames": snapshot.stats.decoded_subtitle_frames,
            "surfaceAttached": snapshot.renderer.attached,
            "playbackOverlayFrames": snapshot.stats.overlay_frames,
            "whiteSubtitlePixelsMin": subtitle_pixels_min,
            "whiteSubtitlePixelsMax": subtitle_pixels_max,
        })
    );
    assert!(exported >= 60 && inspected >= 30);
    assert!(snapshot.stats.decoded_video_frames >= 60);
    assert!(snapshot.stats.decoded_subtitle_frames > 0);
    assert_eq!(
        subtitle_pixels_min, subtitle_pixels_max,
        "static ASS cue changed between export frames"
    );
    assert!(snapshot.media_time >= Duration::from_secs(3));
    assert_eq!(snapshot.stats.import_failures, 0);
    assert_eq!(snapshot.stats.render_failures, 0);
    if expect_redundant {
        assert!(snapshot.stats.overlay_frames >= 60);
    } else {
        assert_eq!(snapshot.stats.overlay_frames, 0);
    }
    if let Ok(path) = std::env::var("ERIKA_HEADLESS_SUBTITLE_PNG") {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(file, 640, 360);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&sample.unwrap())
            .unwrap();
    }
    presenter.stop().unwrap();
}
