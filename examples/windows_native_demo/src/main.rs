#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("windows_native_demo only runs on Windows.");
}

#[cfg(target_os = "windows")]
fn main() -> erika::Result<()> {
    windows_demo::run()
}

#[cfg(target_os = "windows")]
mod windows_demo {
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::ptr;
    use std::thread;
    use std::time::{Duration, Instant};

    use erika::{
        MediaRequest, PlatformSurface, RendererBackendPreference, WgpuSurfaceHandle,
        WgpuSurfaceKind,
        danmaku::{DanmakuColor, DanmakuItem, DanmakuMode, DanmakuShadowStyle, DanmakuTimeline},
        presenter::{PresenterConfig, PresenterRuntime},
        renderer::pipeline::LumaUpscalerMode,
    };
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, VK_LEFT, VK_RIGHT, VK_SHIFT, VK_SPACE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW,
        GetClientRect, IDC_ARROW, LoadCursorW, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage,
        RegisterClassW, SW_SHOW, SetWindowTextW, ShowWindow, TranslateMessage, WM_DESTROY,
        WM_KEYDOWN, WM_QUIT, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    const SEEK_STEP: Duration = Duration::from_secs(5);
    const TITLE_UPDATE_INTERVAL: Duration = Duration::from_millis(250);

    #[derive(Debug, Clone)]
    struct DemoOptions {
        media: Option<String>,
        smoke_duration: Option<Duration>,
        metrics_log_path: Option<String>,
        danmaku_path: Option<String>,
        synthetic_danmaku_rate: Option<f64>,
        renderer: RendererBackendPreference,
        upscaler: LumaUpscalerMode,
        upscaler_model: Option<String>,
    }

    #[derive(Debug, Default)]
    struct SmokeMetrics {
        render_ticks: u64,
        tick_durations: Vec<Duration>,
        render_durations: Vec<Duration>,
        pump_durations: Vec<Duration>,
        video_pump_durations: Vec<Duration>,
        danmaku_plan_durations: Vec<Duration>,
        gpu_durations: Vec<Duration>,
    }

    #[derive(Debug, Clone, Copy, Default)]
    struct TimingSummary {
        avg: Duration,
        p95: Duration,
        max: Duration,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DemoCommand {
        TogglePlayback,
        SeekBackward,
        SeekForward,
        /// `U` / `Shift+U`: next / previous upscaler mode.
        CycleUpscaler(bool),
        /// `I`: print the upscaler status.
        PrintUpscaler,
    }

    pub fn run() -> erika::Result<()> {
        let options = parse_options()?;
        let hwnd = unsafe { create_window()? };
        let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
        let mut presenter = PresenterRuntime::new(presenter_config(&options)?)?;
        if let Some(path) = options.upscaler_model.as_deref() {
            let bytes = std::fs::read(path).map_err(|error| {
                erika::PlayerError::Renderer(format!("cannot read {path}: {error}"))
            })?;
            presenter.load_custom_upscaler_model(path, &bytes)?;
        }
        presenter.set_luma_upscaler(options.upscaler);
        let (mut width, mut height) = client_size(hwnd);
        let mut scale = dpi_scale(hwnd);
        presenter.attach_surface(PlatformSurface::Wgpu(WgpuSurfaceHandle::new(
            WgpuSurfaceKind::WindowsHwnd,
            hwnd as usize as u64,
            hinstance as usize as u64,
            width,
            height,
            scale,
        )))?;

        if let Some(media) = options.media.clone() {
            presenter.open(MediaRequest::new(media))?;
            presenter.play()?;
        }

        let mut metrics_log = open_metrics_log(options.metrics_log_path.as_deref())?;
        let mut last_metrics_log = Instant::now() - Duration::from_millis(250);
        let start = Instant::now();
        let mut last_title_update = Instant::now() - TITLE_UPDATE_INTERVAL;
        let mut metrics = SmokeMetrics::default();
        let mut running = true;
        while running {
            let (next_running, commands) = pump_messages();
            running = next_running;
            for command in commands {
                apply_command(&mut presenter, command);
            }
            let (next_width, next_height) = client_size(hwnd);
            let next_scale = dpi_scale(hwnd);
            if next_width != width || next_height != height || (next_scale - scale).abs() > 0.001 {
                width = next_width;
                height = next_height;
                scale = next_scale;
                presenter.resize_surface(width, height, scale)?;
            }
            presenter.render_tick(start.elapsed().as_secs_f64())?;
            metrics.record(&presenter);
            log_metrics_if_due(
                metrics_log.as_mut(),
                &presenter,
                start.elapsed(),
                &metrics,
                &mut last_metrics_log,
            );
            if last_title_update.elapsed() >= TITLE_UPDATE_INTERVAL {
                update_window_title(hwnd, &presenter);
                last_title_update = Instant::now();
            }
            if options
                .smoke_duration
                .is_some_and(|duration| start.elapsed() >= duration)
            {
                running = false;
            }
            thread::sleep(Duration::from_millis(16));
        }

        print_smoke_summary(&presenter, start.elapsed(), &metrics);
        presenter.detach_surface()?;
        Ok(())
    }

    fn parse_options() -> erika::Result<DemoOptions> {
        let mut media = None;
        let mut smoke_duration = None;
        let mut metrics_log_path = None;
        let mut danmaku_path = None;
        let mut synthetic_danmaku_rate = None;
        let mut renderer = RendererBackendPreference::PlatformNative;
        let mut upscaler = LumaUpscalerMode::Off;
        let mut upscaler_model = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--smoke-seconds" => {
                    let value = args.next().ok_or_else(|| {
                        erika::PlayerError::Renderer("--smoke-seconds requires a value".to_string())
                    })?;
                    let seconds = value.parse::<f64>().map_err(|error| {
                        erika::PlayerError::Renderer(format!(
                            "invalid --smoke-seconds value `{value}`: {error}"
                        ))
                    })?;
                    smoke_duration = Some(Duration::from_secs_f64(seconds.max(0.1)));
                }
                "--metrics-log" => {
                    metrics_log_path = Some(args.next().ok_or_else(|| {
                        erika::PlayerError::Renderer("--metrics-log requires a path".to_string())
                    })?);
                }
                "--danmaku" => {
                    danmaku_path = Some(args.next().ok_or_else(|| {
                        erika::PlayerError::Renderer("--danmaku requires a path".to_string())
                    })?);
                }
                "--synthetic-danmaku" => {
                    let value = args.next().ok_or_else(|| {
                        erika::PlayerError::Renderer(
                            "--synthetic-danmaku requires a comments-per-second value".to_string(),
                        )
                    })?;
                    let rate = value.parse::<f64>().map_err(|error| {
                        erika::PlayerError::Renderer(format!(
                            "invalid --synthetic-danmaku value `{value}`: {error}"
                        ))
                    })?;
                    synthetic_danmaku_rate = Some(rate.clamp(1.0, 2_000.0));
                }
                "--wgpu-fallback" => {
                    renderer = RendererBackendPreference::WgpuFallback;
                }
                "--platform-native" => {
                    renderer = RendererBackendPreference::PlatformNative;
                }
                "--upscaler" => {
                    let value = args.next().ok_or_else(|| {
                        erika::PlayerError::Renderer("--upscaler requires a mode".to_string())
                    })?;
                    upscaler = LumaUpscalerMode::ALL
                        .into_iter()
                        .find(|mode| mode.label() == value.replace('-', "_"))
                        .ok_or_else(|| {
                            let modes: Vec<&str> = LumaUpscalerMode::ALL
                                .iter()
                                .map(|mode| mode.label())
                                .collect();
                            erika::PlayerError::Renderer(format!(
                                "invalid --upscaler value `{value}` (expected {})",
                                modes.join("|")
                            ))
                        })?;
                }
                "--upscaler-model" => {
                    upscaler_model = Some(args.next().ok_or_else(|| {
                        erika::PlayerError::Renderer(
                            "--upscaler-model requires an .esr or mpv shader path".to_string(),
                        )
                    })?);
                }
                _ if media.is_none() => media = Some(arg),
                _ => {
                    return Err(erika::PlayerError::Renderer(format!(
                        "unexpected argument `{arg}`"
                    )));
                }
            }
        }
        Ok(DemoOptions {
            media,
            smoke_duration,
            metrics_log_path,
            danmaku_path,
            synthetic_danmaku_rate,
            renderer,
            upscaler,
            upscaler_model,
        })
    }

    fn presenter_config(options: &DemoOptions) -> erika::Result<PresenterConfig> {
        let mut config = PresenterConfig::default();
        config.player.renderer = options.renderer;
        config.danmaku = load_danmaku_timeline(options)?;
        config.danmaku_config.font_size = 28.0;
        config.danmaku_config.display_area = 0.85;
        config.danmaku_config.shadow_style = DanmakuShadowStyle::Strong;
        config.render_test_pattern_when_idle = true;
        Ok(config)
    }

    fn load_danmaku_timeline(options: &DemoOptions) -> erika::Result<Option<DanmakuTimeline>> {
        if let Some(path) = options.danmaku_path.as_deref() {
            return DanmakuTimeline::from_file(path)
                .map(Some)
                .map_err(|error| erika::PlayerError::Renderer(error.to_string()));
        }
        options
            .synthetic_danmaku_rate
            .map(generate_synthetic_danmaku)
            .transpose()
    }

    fn generate_synthetic_danmaku(rate: f64) -> erika::Result<DanmakuTimeline> {
        let duration_seconds = 300.0;
        let count = (duration_seconds * rate.max(1.0)).round() as usize;
        let mut items = Vec::with_capacity(count);
        for index in 0..count {
            let pts = Duration::from_secs_f64(index as f64 / rate.max(1.0));
            items.push(DanmakuItem {
                id: index as u64 + 1,
                pts,
                text: format!("Erika Windows D3D11 overlay #{index:05}"),
                mode: if index % 9 == 0 {
                    DanmakuMode::Top
                } else if index % 13 == 0 {
                    DanmakuMode::Bottom
                } else {
                    DanmakuMode::Scroll
                },
                font_size: 25.0,
                color: match index % 5 {
                    0 => DanmakuColor::rgb_u8(255, 255, 255),
                    1 => DanmakuColor::rgb_u8(255, 220, 80),
                    2 => DanmakuColor::rgb_u8(105, 220, 255),
                    3 => DanmakuColor::rgb_u8(255, 125, 175),
                    _ => DanmakuColor::rgb_u8(135, 255, 165),
                },
                opacity: 1.0,
                is_self: false,
                special: None,
            });
        }
        DanmakuTimeline::new(items).map_err(|error| erika::PlayerError::Renderer(error.to_string()))
    }

    unsafe fn create_window() -> erika::Result<HWND> {
        let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
        let class_name = wide("ErikaWindowsNativeDemo");
        let title = wide("Erika Windows Native Demo");
        let window_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) },
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        if unsafe { RegisterClassW(&window_class) } == 0 {
            return Err(erika::PlayerError::Renderer(
                "RegisterClassW failed".to_string(),
            ));
        }

        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1280,
                720,
                ptr::null_mut(),
                ptr::null_mut(),
                hinstance,
                ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            return Err(erika::PlayerError::Renderer(
                "CreateWindowExW failed".to_string(),
            ));
        }
        unsafe { ShowWindow(hwnd, SW_SHOW) };
        Ok(hwnd)
    }

    fn pump_messages() -> (bool, Vec<DemoCommand>) {
        let mut msg = MSG::default();
        let mut commands = Vec::new();
        unsafe {
            while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    return (false, commands);
                }
                if msg.message == WM_KEYDOWN {
                    if let Some(command) = command_from_key(msg.wParam as u16) {
                        commands.push(command);
                    }
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        (true, commands)
    }

    fn command_from_key(key: u16) -> Option<DemoCommand> {
        match key {
            VK_SPACE => Some(DemoCommand::TogglePlayback),
            VK_LEFT => Some(DemoCommand::SeekBackward),
            VK_RIGHT => Some(DemoCommand::SeekForward),
            0x55 => Some(DemoCommand::CycleUpscaler(unsafe {
                GetKeyState(i32::from(VK_SHIFT)) < 0
            })),
            0x49 => Some(DemoCommand::PrintUpscaler),
            _ => None,
        }
    }

    fn apply_command(presenter: &mut PresenterRuntime, command: DemoCommand) {
        let result = match command {
            DemoCommand::TogglePlayback => {
                if presenter.is_playing() {
                    presenter.pause()
                } else {
                    presenter.play()
                }
            }
            DemoCommand::SeekBackward => {
                let target = presenter.media_time().saturating_sub(SEEK_STEP);
                presenter.seek(target)
            }
            DemoCommand::SeekForward => {
                let target = presenter
                    .media_time()
                    .checked_add(SEEK_STEP)
                    .unwrap_or(Duration::MAX);
                let target = presenter
                    .duration()
                    .map_or(target, |duration| target.min(duration));
                presenter.seek(target)
            }
            DemoCommand::CycleUpscaler(backwards) => {
                let modes = LumaUpscalerMode::ALL;
                let current = presenter.runtime_snapshot().renderer.upscaler_mode;
                let index = modes.iter().position(|mode| *mode == current).unwrap_or(0);
                let next = if backwards {
                    (index + modes.len() - 1) % modes.len()
                } else {
                    (index + 1) % modes.len()
                };
                presenter.set_luma_upscaler(modes[next]);
                println!("upscaler -> {}", modes[next].label());
                Ok(())
            }
            DemoCommand::PrintUpscaler => {
                let renderer = presenter.runtime_snapshot().renderer;
                println!(
                    "upscaler mode={} backend={:?} frames={} fallbacks={} gpu_ms={:.3}",
                    renderer.upscaler_mode.label(),
                    renderer.upscaler_backend,
                    renderer.upscaled_frames,
                    renderer.upscaler_fallbacks,
                    renderer.last_gpu_duration.as_secs_f64() * 1000.0
                );
                Ok(())
            }
        };
        if let Err(error) = result {
            eprintln!("Erika Windows native demo command {command:?} failed: {error}");
        }
    }

    fn update_window_title(hwnd: HWND, presenter: &PresenterRuntime) {
        let snapshot = presenter.runtime_snapshot();
        let duration = snapshot.media_time.as_secs_f64().max(0.0);
        let title = format!(
            "Erika Windows Native Demo  t={duration:.2}s  sr={}/{:?} {:.2}ms  sw={} hw={} zero={} direct={} shared={} cpu={} hdr10={} hdr_src={} sdr_tm={} audio_q={} underflow={}",
            snapshot.renderer.upscaler_mode.label(),
            snapshot.renderer.upscaler_backend,
            snapshot.renderer.last_gpu_duration.as_secs_f64() * 1000.0,
            snapshot.renderer.software_video_frames,
            snapshot.renderer.hardware_video_frames,
            snapshot.renderer.zero_copy_video_frames,
            snapshot.renderer.direct_zero_copy_video_frames,
            snapshot.renderer.shared_handle_video_frames,
            snapshot.renderer.cpu_video_frame_fallbacks,
            u8::from(snapshot.renderer.hdr10_output_active),
            snapshot.renderer.hdr_source_frames,
            snapshot.renderer.sdr_tonemap_frames,
            snapshot.audio_output_queued_frames,
            snapshot.audio_output_underflow_frames,
        );
        let title = wide(&title);
        unsafe {
            SetWindowTextW(hwnd, title.as_ptr());
        }
    }

    impl SmokeMetrics {
        fn record(&mut self, presenter: &PresenterRuntime) {
            let snapshot = presenter.runtime_snapshot();
            self.render_ticks = self.render_ticks.saturating_add(1);
            self.tick_durations.push(snapshot.last_tick_duration);
            self.render_durations.push(snapshot.last_render_duration);
            self.pump_durations.push(snapshot.last_pump_duration);
            self.video_pump_durations
                .push(snapshot.last_video_pump_duration);
            self.danmaku_plan_durations
                .push(snapshot.last_danmaku_plan_duration);
            if snapshot.renderer.last_gpu_duration > Duration::ZERO {
                self.gpu_durations.push(snapshot.renderer.last_gpu_duration);
            }
        }
    }

    fn open_metrics_log(path: Option<&str>) -> erika::Result<Option<BufWriter<File>>> {
        path.map(|path| {
            File::create(path).map(BufWriter::new).map_err(|error| {
                erika::PlayerError::Renderer(format!("metrics log create failed: {error}"))
            })
        })
        .transpose()
    }

    fn log_metrics_if_due(
        log: Option<&mut BufWriter<File>>,
        presenter: &PresenterRuntime,
        elapsed: Duration,
        metrics: &SmokeMetrics,
        last_metrics_log: &mut Instant,
    ) {
        let Some(log) = log else {
            return;
        };
        if last_metrics_log.elapsed() < Duration::from_millis(250) {
            return;
        }
        *last_metrics_log = Instant::now();
        let line = smoke_metrics_json(presenter, elapsed, metrics);
        if let Err(error) = writeln!(log, "{line}").and_then(|_| log.flush()) {
            eprintln!("Erika Windows native demo metrics log failed: {error}");
        }
    }

    fn print_smoke_summary(
        presenter: &PresenterRuntime,
        elapsed: Duration,
        metrics: &SmokeMetrics,
    ) {
        let snapshot = presenter.runtime_snapshot();
        let elapsed_seconds = elapsed.as_secs_f64().max(0.001);
        let tick = timing_summary(&metrics.tick_durations);
        let render = timing_summary(&metrics.render_durations);
        let pump = timing_summary(&metrics.pump_durations);
        let video_pump = timing_summary(&metrics.video_pump_durations);
        let danmaku_plan = timing_summary(&metrics.danmaku_plan_durations);
        let gpu = timing_summary(&metrics.gpu_durations);
        println!(
            "erika_windows_smoke elapsed={:.3}s ticks={} tick_fps={:.2} decoded={} rendered_video={} rendered_total={} hw={} zero={} direct={} shared={} cpu={} hdr10_active={} hdr_source={} hdr10_output={} sdr_tonemap={} hdr_meta_updates={} hdr_meta_failures={} hdr_output_failures={} import_failures={} render_failures={} audio_underflow={} danmaku_passes={} danmaku_draw_items={} atlas_uploads={} atlas_reuses={} tick_ms_avg={:.3} tick_ms_p95={:.3} tick_ms_max={:.3} render_ms_avg={:.3} render_ms_p95={:.3} render_ms_max={:.3} pump_ms_avg={:.3} pump_ms_p95={:.3} video_pump_ms_p95={:.3} danmaku_plan_ms_p95={:.3} gpu_ms_p95={:.3}",
            elapsed_seconds,
            metrics.render_ticks,
            metrics.render_ticks as f64 / elapsed_seconds,
            snapshot.stats.decoded_video_frames,
            snapshot.stats.rendered_video_frames,
            snapshot.renderer.rendered_frames,
            snapshot.renderer.hardware_video_frames,
            snapshot.renderer.zero_copy_video_frames,
            snapshot.renderer.direct_zero_copy_video_frames,
            snapshot.renderer.shared_handle_video_frames,
            snapshot.renderer.cpu_video_frame_fallbacks,
            u8::from(snapshot.renderer.hdr10_output_active),
            snapshot.renderer.hdr_source_frames,
            snapshot.renderer.hdr10_output_frames,
            snapshot.renderer.sdr_tonemap_frames,
            snapshot.renderer.hdr10_metadata_updates,
            snapshot.renderer.hdr10_metadata_failures,
            snapshot.renderer.hdr10_output_failures,
            snapshot.stats.import_failures,
            snapshot.stats.render_failures,
            snapshot.audio_output_underflow_frames,
            snapshot.renderer.danmaku_passes,
            snapshot.renderer.danmaku_draw_items,
            snapshot.renderer.overlay_alpha_atlas_uploads,
            snapshot.renderer.overlay_alpha_atlas_reuses,
            ms(tick.avg),
            ms(tick.p95),
            ms(tick.max),
            ms(render.avg),
            ms(render.p95),
            ms(render.max),
            ms(pump.avg),
            ms(pump.p95),
            ms(video_pump.p95),
            ms(danmaku_plan.p95),
            ms(gpu.p95),
        );
    }

    fn smoke_metrics_json(
        presenter: &PresenterRuntime,
        elapsed: Duration,
        metrics: &SmokeMetrics,
    ) -> String {
        let snapshot = presenter.runtime_snapshot();
        let tick = timing_summary(&metrics.tick_durations);
        let render = timing_summary(&metrics.render_durations);
        let elapsed_seconds = elapsed.as_secs_f64().max(0.001);
        format!(
            "{{\"elapsed_s\":{elapsed:.3},\"ticks\":{ticks},\"tick_fps\":{tick_fps:.3},\"decoded\":{decoded},\"rendered_video\":{rendered_video},\"rendered_total\":{rendered_total},\"hw\":{hw},\"zero\":{zero},\"direct\":{direct},\"shared\":{shared},\"cpu\":{cpu},\"hdr10_active\":{hdr10_active},\"hdr_source\":{hdr_source},\"hdr10_output\":{hdr10_output},\"sdr_tonemap\":{sdr_tonemap},\"hdr_meta_updates\":{hdr_meta_updates},\"hdr_meta_failures\":{hdr_meta_failures},\"hdr_output_failures\":{hdr_output_failures},\"import_failures\":{import_failures},\"render_failures\":{render_failures},\"audio_underflow\":{audio_underflow},\"danmaku_passes\":{danmaku_passes},\"danmaku_draw_items\":{danmaku_draw_items},\"atlas_uploads\":{atlas_uploads},\"atlas_reuses\":{atlas_reuses},\"tick_ms_avg\":{tick_avg:.3},\"tick_ms_p95\":{tick_p95:.3},\"render_ms_avg\":{render_avg:.3},\"render_ms_p95\":{render_p95:.3}}}",
            elapsed = elapsed_seconds,
            ticks = metrics.render_ticks,
            tick_fps = metrics.render_ticks as f64 / elapsed_seconds,
            decoded = snapshot.stats.decoded_video_frames,
            rendered_video = snapshot.stats.rendered_video_frames,
            rendered_total = snapshot.renderer.rendered_frames,
            hw = snapshot.renderer.hardware_video_frames,
            zero = snapshot.renderer.zero_copy_video_frames,
            direct = snapshot.renderer.direct_zero_copy_video_frames,
            shared = snapshot.renderer.shared_handle_video_frames,
            cpu = snapshot.renderer.cpu_video_frame_fallbacks,
            hdr10_active = u8::from(snapshot.renderer.hdr10_output_active),
            hdr_source = snapshot.renderer.hdr_source_frames,
            hdr10_output = snapshot.renderer.hdr10_output_frames,
            sdr_tonemap = snapshot.renderer.sdr_tonemap_frames,
            hdr_meta_updates = snapshot.renderer.hdr10_metadata_updates,
            hdr_meta_failures = snapshot.renderer.hdr10_metadata_failures,
            hdr_output_failures = snapshot.renderer.hdr10_output_failures,
            import_failures = snapshot.stats.import_failures,
            render_failures = snapshot.stats.render_failures,
            audio_underflow = snapshot.audio_output_underflow_frames,
            danmaku_passes = snapshot.renderer.danmaku_passes,
            danmaku_draw_items = snapshot.renderer.danmaku_draw_items,
            atlas_uploads = snapshot.renderer.overlay_alpha_atlas_uploads,
            atlas_reuses = snapshot.renderer.overlay_alpha_atlas_reuses,
            tick_avg = ms(tick.avg),
            tick_p95 = ms(tick.p95),
            render_avg = ms(render.avg),
            render_p95 = ms(render.p95),
        )
    }

    fn timing_summary(values: &[Duration]) -> TimingSummary {
        if values.is_empty() {
            return TimingSummary::default();
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        TimingSummary {
            avg: avg_duration(&sorted),
            p95: sorted[percentile_index(sorted.len(), 95)],
            max: *sorted.last().unwrap_or(&Duration::ZERO),
        }
    }

    fn percentile_index(len: usize, percentile: usize) -> usize {
        let percentile = percentile.min(100);
        (((len - 1) * percentile) + 50) / 100
    }

    fn avg_duration(values: &[Duration]) -> Duration {
        if values.is_empty() {
            return Duration::ZERO;
        }
        let total_ns = values.iter().map(Duration::as_nanos).sum::<u128>();
        Duration::from_nanos((total_ns / values.len() as u128).min(u64::MAX as u128) as u64)
    }

    fn ms(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1000.0
    }

    fn client_size(hwnd: HWND) -> (u32, u32) {
        let mut rect = RECT::default();
        let ok = unsafe { GetClientRect(hwnd, &mut rect) } != 0;
        if !ok {
            return (1, 1);
        }
        let width = (rect.right - rect.left).max(1) as u32;
        let height = (rect.bottom - rect.top).max(1) as u32;
        (width, height)
    }

    fn dpi_scale(hwnd: HWND) -> f64 {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
}
