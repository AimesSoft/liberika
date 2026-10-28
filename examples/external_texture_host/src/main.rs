//! Windowless renderer/interop acceptance harness, not a decoder benchmark.
use anyhow::{Context, Result, ensure};
use erika::danmaku::{
    DanmakuColor, DanmakuItem, DanmakuLayoutConfig, DanmakuMode, DanmakuTimeline, DanmakuViewport,
    DfmLayoutEngine,
};
use erika::overlay::{OverlayFrame, OverlayViewport};
use erika::renderer::{
    pipeline::{VideoRenderPipeline, VideoUniforms},
    wgpu::WgpuRenderer,
};
use erika::subtitle::SubtitleBitmapPlane;
use erika::{RenderFrameContext, RendererBackend};
use erika_texture_host::{Host, import::import_frame};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let mut width = 960u32;
    let mut height = 540u32;
    let mut count = 120u32;
    let mut rgba = false;
    let mut capture_only = false;
    let mut hardware = false;
    let mut report = None;
    let mut png = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--width" => width = args.next().context("width")?.parse()?,
            "--height" => height = args.next().context("height")?.parse()?,
            "--frames" => count = args.next().context("frame count")?.parse()?,
            "--rgba" => rgba = true,
            "--capture-only" => {
                rgba = true;
                capture_only = true;
            }
            "--require-hardware" => hardware = true,
            "--report" => report = Some(PathBuf::from(args.next().context("report path")?)),
            "--png" => png = Some(PathBuf::from(args.next().context("PNG path")?)),
            _ => anyhow::bail!(
                "usage: erika_texture_host [--rgba | --capture-only] [--require-hardware] [--width W --height H --frames N] [--report JSON --png PNG]"
            ),
        }
    }
    ensure!(
        width > 0 && height > 0 && count > 0,
        "dimensions/frame count must be positive"
    );
    if hardware {
        // This runs before creating either renderer or spawning worker threads.
        unsafe { std::env::set_var("ERIKA_REQUIRE_HARDWARE_GPU", "1") };
    }
    ensure!(
        capture_only || !rgba || wgpu::Backends::from_env() != Some(wgpu::Backends::GL),
        "use --capture-only for GLES: the two wgpu versions do not coordinate EGL display teardown; native import uses Vulkan/Metal"
    );
    let mut host = if capture_only {
        None
    } else {
        Some(Host::new(hardware)?)
    };
    let mut producer = WgpuRenderer::new()?;
    let producer_adapter = format!("{:?}", producer.adapter_info());
    let uniforms = VideoUniforms::from_pipeline(&VideoRenderPipeline::sdr_default(), false, false);
    let luma = (0..64 * 36)
        .map(|i| 32 + (i % 64) * 3)
        .map(|v| v as u8)
        .collect::<Vec<_>>();
    producer.upload_nv12(64, 36, &luma, &[128; 64 * 18], uniforms)?;
    let timeline = DanmakuTimeline::new(vec![DanmakuItem {
        id: 1,
        pts: Duration::ZERO,
        text: "liberika native texture / danmaku".into(),
        mode: DanmakuMode::Top,
        font_size: 24.,
        color: DanmakuColor::WHITE,
        opacity: 1.,
        is_self: false,
    }])?;
    let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
    let plan = engine.render_plan(
        Duration::from_secs(1),
        DanmakuViewport::new(width, height),
        1,
    );
    let overlay = OverlayFrame {
        pts: Duration::from_secs(1),
        viewport: OverlayViewport::new(width, height),
        subtitle_planes: vec![SubtitleBitmapPlane::new(
            8,
            (height - 8).saturating_sub(12) as i32,
            width.min(160),
            12,
            [220, 30, 30, 255].repeat(width.min(160) as usize * 12),
        )],
        subtitle_alpha_planes: vec![],
        subtitle_changed: true,
    };
    let context = RenderFrameContext::new(Duration::from_secs(1), 1)
        .overlay(Some(&overlay))
        .danmaku(Some(&plan))
        .output_size(width, height);
    let mut export_ms = Vec::new();
    let mut import_ms = Vec::new();
    let mut composed_ms = Vec::new();
    let mut completed_ms = Vec::new();
    let mut last_capture = None;
    let started = Instant::now();
    for iteration in 0..count + 3 {
        let start = Instant::now();
        if capture_only {
            let pixels = producer
                .capture_current_frame(context, width, height)?
                .context("captured pixels")?
                .rgba;
            last_capture = Some(pixels);
            if iteration >= 3 {
                let elapsed = start.elapsed().as_secs_f64() * 1000.;
                export_ms.push(elapsed);
                import_ms.push(0.);
                composed_ms.push(0.);
                completed_ms.push(elapsed);
            }
            continue;
        }
        let host = host.as_mut().context("consumer")?;
        let (texture, export, import) = if rgba {
            let pixels = producer
                .capture_current_frame(context, width, height)?
                .context("current RGBA frame")?
                .rgba;
            let exported = start.elapsed().as_secs_f64() * 1000.;
            let import_start = Instant::now();
            let texture = host.upload_rgba(width, height, &pixels);
            (
                texture,
                exported,
                import_start.elapsed().as_secs_f64() * 1000.,
            )
        } else {
            let frame = producer
                .export_current_frame_texture(context, width, height)?
                .context("current native frame")?;
            let exported = start.elapsed().as_secs_f64() * 1000.;
            let import_start = Instant::now();
            let texture = import_frame(&host.device, &host.queue, frame)?;
            (
                texture,
                exported,
                import_start.elapsed().as_secs_f64() * 1000.,
            )
        };
        let draw_start = Instant::now();
        host.draw(&texture);
        host.wait()?;
        let composed = draw_start.elapsed().as_secs_f64() * 1000.;
        drop(texture);
        let completed = start.elapsed().as_secs_f64() * 1000.;
        if iteration >= 3 {
            export_ms.push(export);
            import_ms.push(import);
            composed_ms.push(composed);
            completed_ms.push(completed);
        }
    }
    let expected = producer
        .capture_current_frame(context, width, height)?
        .context("reference pixels")?
        .rgba;
    drop(producer); // Imported resources must remain valid independently.
    let pixels = if let Some(host) = &host {
        host.read_pixels()?
    } else {
        last_capture.context("last capture")?
    };
    ensure!(
        pixels == expected,
        "consumer pixels differ from video/subtitle/danmaku reference"
    );
    if let Some(path) = png {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut encoder = png::Encoder::new(std::fs::File::create(path)?, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&pixels)?;
    }
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    let frame_ms = mean(&completed_ms);
    let result = serde_json::json!({
        "workload":"renderer-only static GPU fixture; decoding and window presentation excluded",
        "producer_wgpu":"29.0.3","consumer_wgpu":host.as_ref().map(|_|"26.0.1"),
        "mode":if capture_only {"RGBA capture only; no consumer"} else if rgba {"explicit RGBA readback/upload"} else {"native texture import"},
        "source_width":64,"source_height":36,"width":width,"height":height,"measured_frames":count,"warmup_frames":3,
        "producer_adapter":producer_adapter,
        "consumer_adapter":host.as_ref().map(|host|format!("{:?}",host.adapter_info)),"require_hardware":hardware,
        "export_mean_ms":mean(&export_ms),"import_mean_ms":mean(&import_ms),"compose_wait_mean_ms":mean(&composed_ms),
        "completed_frame_mean_ms":frame_ms,"renderer_throughput_fps":1000./frame_ms,
        "verification_pixel_match":true,"final_verification_readback_excluded_from_timing":true,
        "total_wall_seconds":started.elapsed().as_secs_f64(),
    });
    let json = serde_json::to_string_pretty(&result)?;
    if let Some(path) = report {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &json)?;
    }
    println!("{json}");
    Ok(())
}
