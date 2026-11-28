use std::cell::RefCell;
use std::env;
use std::ffi::CString;
use std::ffi::c_void;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::process;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use erika::core::{LumaUpscalerBackendStatus, PlayerConfig};
use erika::danmaku::{
    DanmakuColor, DanmakuFrameStats, DanmakuItem, DanmakuLayoutConfig, DanmakuMode,
    DanmakuShadowStyle, DanmakuTimeline, DanmakuViewport, DfmLayoutEngine,
};
use erika::playback::{PlaybackSessionConfig, VideoDecodePreference, VideoPlaybackEngine};
use erika::presenter::{PresenterConfig, PresenterRuntime};
use erika::renderer::metal::{LumaUpscalerMode, MetalRendererConfig};
use erika::{MediaRequest, MetalSurfaceHandle, PlatformSurface};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn erika_perf_lab_run_app();
}

static WINDOW_OPTIONS: OnceLock<PerfOptions> = OnceLock::new();
static METRICS_TEXT: Mutex<Option<CString>> = Mutex::new(None);

thread_local! {
    static LAB: RefCell<Option<WindowLabState>> = const { RefCell::new(None) };
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let options = parse_args(&args).unwrap_or_else(|error| {
        eprintln!("{error}");
        eprintln!(
            "usage: cargo run -p danmaku_perf_lab -- [--window] [--fullscreen] [--uncapped] [--danmaku PATH] [--video PATH] [--software|--videotoolbox|--d3d11va] \
             [--items N|--rate N] [--duration SECONDS] [--frames N] [--fps N] [--size WxH] \
             [--prewarm-frames N] \
             [--font-size N] [--display-area N] [--scroll-duration N] [--scroll-overwrite] \
             [--window-size WxH] [--hide-panel] [--surface-scale N] [--target-fps N] \
             [--start-time SECONDS] [--self-every N] [--metrics-log PATH] [--auto-exit SECONDS] \
             [--upscaler off|artcnn-c4f16|artcnn-c4f16-ds|artcnn-c4f32] \
             [--pattern mixed|scroll|fixed|dense]"
        );
        process::exit(2);
    });

    match run(options) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("danmaku perf lab failed: {error}");
            process::exit(1);
        }
    }
}

fn run(options: PerfOptions) -> Result<(), String> {
    if options.window {
        return run_window(options);
    }
    let timeline = load_or_generate_timeline(&options)?;
    let config = DanmakuLayoutConfig {
        font_size: options.font_size,
        display_area: options.display_area,
        scroll_duration_seconds: options.scroll_duration_seconds,
        outline_width: options.outline_width,
        shadow_style: options.shadow_style,
        merge_duplicates: options.merge_duplicates,
        allow_stacking: options.allow_stacking,
        allow_scroll_overwrite: options.allow_scroll_overwrite,
        ..DanmakuLayoutConfig::default()
    };
    let viewport = DanmakuViewport::new(options.width, options.height);
    let mut engine = DfmLayoutEngine::new(timeline, config);

    println!("Erika danmaku perf lab");
    println!(
        "mode: {}",
        if options.video_uri.is_some() {
            "video-driven"
        } else {
            "synthetic-clock"
        }
    );
    println!(
        "viewport: {}x{} frames={} fps={:.3} duration={:.3}s start={:.3}s",
        viewport.width,
        viewport.height,
        options.frames,
        options.fps,
        options.duration_seconds,
        options.start_time_seconds
    );
    println!(
        "danmaku: items={} rate={:.1}/s pattern={:?} font={:.1} display_area={:.2} scroll_dur={:.2}s self_every={}",
        options.item_count,
        options.rate,
        options.pattern,
        options.font_size,
        options.display_area,
        options.scroll_duration_seconds,
        format_self_every(options.self_every)
    );

    let prepare_started = Instant::now();
    let prepared = engine.prepare(viewport, 1);
    let prepare_elapsed = prepare_started.elapsed();
    let prepared_stats = prepared.stats();
    println!(
        "prepare: {:.3}ms prepared_items={}",
        ms(prepare_elapsed),
        prepared.items().len()
    );
    println!(
        "prepare_sensor: source/supported/prepared/filtered={}/{}/{}/{} scroll/top/bottom={}/{}/{} expected/dfm_tracks={}/{} scroll_rows={} y={:.0}..{:.0} buckets={}",
        prepared_stats.source_items,
        prepared_stats.supported_items,
        prepared_stats.prepared_items,
        prepared_stats.filtered_items,
        prepared_stats.prepared_scroll_items,
        prepared_stats.prepared_top_items,
        prepared_stats.prepared_bottom_items,
        prepared_stats.expected_scroll_tracks,
        prepared_stats.dfm_track_count,
        prepared_stats.prepared_scroll_rows,
        prepared_stats.prepared_scroll_min_y,
        prepared_stats.prepared_scroll_max_y,
        format_buckets(
            prepared_stats.scroll_bucket_count,
            &prepared_stats.scroll_buckets
        ),
    );

    let prewarm = prewarm_atlas(&mut engine, viewport, &options);
    if prewarm.frames > 0 {
        println!(
            "prewarm: {:.3}ms frames={} glyph_instances={} atlas_version={} atlas_bytes={}",
            ms(prewarm.elapsed),
            prewarm.frames,
            prewarm.glyph_instances,
            prewarm.atlas_version,
            prewarm.atlas_bytes
        );
    }

    let mut samples = Vec::with_capacity(options.frames);
    if let Some(uri) = &options.video_uri {
        run_video_driven(uri, &options, viewport, &mut engine, &mut samples)?;
    } else {
        run_synthetic(&options, viewport, &mut engine, &mut samples);
    }

    print_report(prepare_elapsed, &samples);
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_window(options: PerfOptions) -> Result<(), String> {
    WINDOW_OPTIONS
        .set(options)
        .map_err(|_| "window options already initialized".to_string())?;
    unsafe { erika_perf_lab_run_app() };
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn run_window(_options: PerfOptions) -> Result<(), String> {
    Err("danmaku_perf_lab --window is macOS/Metal-only; use windows_native_demo for Windows HWND/D3D11 presenter smoke tests".to_string())
}

fn run_synthetic(
    options: &PerfOptions,
    viewport: DanmakuViewport,
    engine: &mut DfmLayoutEngine,
    samples: &mut Vec<FrameSample>,
) {
    for frame_index in 0..options.frames {
        let media_time =
            Duration::from_secs_f64(options.start_time_seconds + frame_index as f64 / options.fps);
        samples.push(run_one_frame(
            engine,
            viewport,
            media_time,
            frame_index as u64 + 1,
            None,
        ));
    }
}

fn run_video_driven(
    uri: &str,
    options: &PerfOptions,
    viewport: DanmakuViewport,
    engine: &mut DfmLayoutEngine,
    samples: &mut Vec<FrameSample>,
) -> Result<(), String> {
    let mut playback = VideoPlaybackEngine::open(
        &MediaRequest::new(uri),
        PlaybackSessionConfig {
            video_decode: options.decode_preference,
            ..PlaybackSessionConfig::default()
        },
    )
    .map_err(|error| error.to_string())?;
    let info = playback.info();
    println!(
        "video: duration={:?} params={:?} decoder={:?}",
        info.duration, info.video_params, info.video_decode_backend
    );
    playback.play();
    let started = Instant::now();
    let mut empty_ticks = 0u64;
    while samples.len() < options.frames {
        if started.elapsed() > Duration::from_secs(30) {
            return Err(format!(
                "timed out waiting for video frames: got {} / {}",
                samples.len(),
                options.frames
            ));
        }
        match playback.tick() {
            Ok(Some(frame)) => {
                let media_time = frame.pts.unwrap_or(frame.media_time);
                samples.push(run_one_frame(
                    engine,
                    viewport,
                    media_time,
                    samples.len() as u64 + 1,
                    Some(VideoSample {
                        width: frame.frame.width() as usize,
                        height: frame.frame.height() as usize,
                        is_hardware: frame.frame.is_videotoolbox() || frame.frame.is_d3d11va(),
                        late_by: frame.late_by,
                    }),
                ));
            }
            Ok(None) => {
                empty_ticks += 1;
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    println!("video_ticks: empty={empty_ticks}");
    Ok(())
}

struct WindowLabState {
    presenter: PresenterRuntime,
    options: PerfOptions,
    media_opened: bool,
    started_at: Instant,
    fps: f64,
    last_fps_update: Instant,
    last_presented_frames: u64,
    metrics_log: Option<BufWriter<File>>,
    last_metrics_log: Instant,
    last_logged_danmaku_passes: u64,
    last_logged_danmaku_draw_items: u64,
    last_error: Option<String>,
}

impl WindowLabState {
    fn new(options: PerfOptions) -> Result<Self, String> {
        let timeline = load_or_generate_timeline(&options)?;
        let presenter = PresenterRuntime::new(PresenterConfig {
            player: PlayerConfig {
                playback: PlaybackSessionConfig {
                    video_decode: options.decode_preference,
                    ..PlaybackSessionConfig::default()
                },
                ..PlayerConfig::default()
            },
            renderer: MetalRendererConfig {
                luma_upscaler: options.luma_upscaler,
                ..MetalRendererConfig::default()
            },
            danmaku: Some(timeline),
            danmaku_config: layout_config_from_options(&options),
            render_test_pattern_when_idle: options.video_uri.is_none(),
            ..PresenterConfig::default()
        })
        .map_err(|error| error.to_string())?;
        let metrics_log = match options.metrics_log_path.as_deref() {
            Some(path) => {
                Some(BufWriter::new(File::create(path).map_err(|error| {
                    format!("metrics log create failed: {error}")
                })?))
            }
            None => None,
        };
        let now = Instant::now();
        Ok(Self {
            presenter,
            options,
            media_opened: false,
            started_at: now,
            fps: 0.0,
            last_fps_update: now,
            last_presented_frames: 0,
            metrics_log,
            last_metrics_log: now,
            last_logged_danmaku_passes: 0,
            last_logged_danmaku_draw_items: 0,
            last_error: None,
        })
    }

    fn attach_surface(&mut self, layer: *mut c_void, width: u32, height: u32, scale: f64) {
        let surface =
            PlatformSurface::Metal(MetalSurfaceHandle::new(layer as u64, width, height, scale));
        match self.presenter.attach_surface(surface) {
            Ok(()) => self.ensure_media_opened(),
            Err(error) => self.last_error = Some(format!("attach failed: {error}")),
        }
    }

    fn resize_surface(&mut self, width: u32, height: u32, scale: f64) {
        if let Err(error) = self.presenter.resize_surface(width, height, scale) {
            self.last_error = Some(format!("resize failed: {error}"));
        }
    }

    fn render_tick(&mut self, host_time_seconds: f64) {
        self.ensure_media_opened();
        if let Err(error) = self.presenter.render_tick(host_time_seconds) {
            self.last_error = Some(format!("render failed: {error}"));
        }
        self.update_fps();
        self.log_metrics_if_due();
    }

    fn should_auto_exit(&self) -> bool {
        self.options
            .auto_exit_seconds
            .is_some_and(|seconds| self.started_at.elapsed().as_secs_f64() >= seconds)
    }

    fn ensure_media_opened(&mut self) {
        if self.media_opened {
            return;
        }
        self.media_opened = true;
        let Some(uri) = self.options.video_uri.clone() else {
            return;
        };
        match self.presenter.open(MediaRequest::new(uri)) {
            Ok(()) => {
                if self.options.start_time_seconds > 0.0 {
                    if let Err(error) = self
                        .presenter
                        .seek(Duration::from_secs_f64(self.options.start_time_seconds))
                    {
                        self.last_error = Some(format!("initial seek failed: {error}"));
                    }
                }
                if let Err(error) = self.presenter.play() {
                    self.last_error = Some(format!("play failed: {error}"));
                }
            }
            Err(error) => self.last_error = Some(format!("open failed: {error}")),
        }
    }

    fn toggle_play_pause(&mut self) {
        let result = if self.presenter.is_playing() {
            self.presenter.pause()
        } else {
            self.presenter.play()
        };
        if let Err(error) = result {
            self.last_error = Some(format!("play/pause failed: {error}"));
        }
    }

    fn seek(&mut self, seconds: f64) {
        if !seconds.is_finite() || seconds < 0.0 {
            return;
        }
        if let Err(error) = self.presenter.seek(Duration::from_secs_f64(seconds)) {
            self.last_error = Some(format!("seek failed: {error}"));
        }
    }

    fn set_density(&mut self, comments_per_second: f64) {
        let comments_per_second = comments_per_second.clamp(1.0, 2_000.0);
        self.options.rate = comments_per_second;
        self.options.item_count =
            (self.options.duration_seconds * comments_per_second).round() as usize;
        match generate_timeline(&self.options) {
            Ok(timeline) => self.presenter.set_danmaku_timeline(timeline),
            Err(error) => self.last_error = Some(format!("danmaku regenerate failed: {error}")),
        }
    }

    fn set_font_size(&mut self, font_size: f64) {
        self.options.font_size = font_size.clamp(8.0, 120.0) as f32;
        self.apply_config();
    }

    fn set_display_area(&mut self, display_area: f64) {
        self.options.display_area = display_area.clamp(0.05, 1.0) as f32;
        self.apply_config();
    }

    fn set_outline(&mut self, outline_width: f64) {
        self.options.outline_width = outline_width.clamp(0.0, 12.0) as f32;
        self.apply_config();
    }

    fn apply_config(&mut self) {
        self.presenter
            .set_danmaku_config(layout_config_from_options(&self.options));
    }

    fn update_fps(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_fps_update);
        if elapsed < Duration::from_millis(250) {
            return;
        }
        let snapshot = self.presenter.runtime_snapshot();
        let presented = snapshot
            .stats
            .rendered_video_frames
            .saturating_add(snapshot.stats.rendered_test_frames);
        let delta = presented.saturating_sub(self.last_presented_frames);
        self.fps = delta as f64 / elapsed.as_secs_f64().max(0.001);
        self.last_presented_frames = presented;
        self.last_fps_update = now;
    }

    fn metrics_text(&mut self) -> String {
        self.update_fps();
        let snapshot = self.presenter.runtime_snapshot();
        let stats = snapshot.stats;
        let renderer = snapshot.renderer;
        let duration = self.presenter.duration().unwrap_or(Duration::ZERO);
        let draw_ratio = if renderer.danmaku_passes > 0 {
            renderer.danmaku_draw_items as f64 / renderer.danmaku_passes as f64
        } else {
            0.0
        };
        let atlas_mb = snapshot.current_danmaku_atlas_bytes as f64 / (1024.0 * 1024.0);
        let error = self.last_error.as_deref().unwrap_or("-");
        let prepared = snapshot.current_danmaku_prepared;
        let prepared_buckets =
            format_buckets(prepared.scroll_bucket_count, &prepared.scroll_buckets);
        let frame_buckets = format_buckets(
            snapshot.current_danmaku_scroll_bucket_count,
            &snapshot.current_danmaku_scroll_buckets,
        );
        format!(
            "Playback\n  fps: {fps:.1}\n  media: {media:.3}s / {duration:.3}s\n  generation: {generation}\n  state: {state}\n\nTick timings\n  total: {tick:.3} ms\n  pump/layout: {pump:.3} ms\n  render: {render:.3} ms\n\nDanmaku load\n  pattern: {pattern:?}\n  density: {density:.0}/s\n  items: {items}\n  font/area: {font:.1} / {area:.2}\n  scroll dur: {scroll_dur:.2}s\n  self every: {self_every}\n  stacking/overwrite: {stacking}/{overwrite}\n  visible glyph quads: {glyphs}\n  placed items: {placed}\n  scroll/top/bottom: {scroll}/{top}/{bottom}\n\nScroll frame sensor\n  rows: {scroll_rows}\n  tracks: {track_min}..{track_max}\n  y span: {scroll_min:.0}..{scroll_max:.0}\n  buckets: {frame_buckets}\n\nPrepared sensor\n  source/supported/prepared/filtered: {src}/{supported}/{prep}/{filtered}\n  scroll/top/bottom: {prep_scroll}/{prep_top}/{prep_bottom}\n  expected/dfm tracks: {expected_tracks}/{dfm_tracks}\n  area h scroll/display: {scroll_area:.0}/{display_area_h:.0}\n  track h: {track_h:.1}\n  scroll rows: {prep_rows}\n  scroll y span: {prep_min:.0}..{prep_max:.0}\n  buckets: {prepared_buckets}\n\nViewport/atlas\n  viewport: {dw}x{dh}\n  atlas: v{atlas_version} {atlas_mb:.2} MiB\n\nRenderer\n  surface: {sw}x{sh}\n  rendered video: {rendered_video}\n  rendered test: {rendered_test}\n  danmaku passes: {passes}\n  danmaku draw items: {draw_items}\n  draw items/pass: {draw_ratio:.1}\n  atlas uploads: {uploads}\n  atlas reuses: {reuses}\n\nUpscaler\n  mode: {upscaler}\n  backend: {upscaler_backend}\n  fallbacks: {upscaler_fallbacks}\n  upscaled frames: {upscaled_frames}\n  encode: {upscaler_encode:.3} ms\n  gpu frame: {gpu_frame:.3} ms\n\nDecoder / queues\n  decoded video: {decoded_video}\n  pushed audio: {pushed_audio}\n  audio queued: {audio_queued:.3}s / {audio_queued_frames} frames\n  audio underflow: {audio_underflow}\n  import failures: {import_failures}\n  render failures: {render_failures}\n  audio failures: {audio_failures}\n\nError\n  {error}",
            fps = self.fps,
            media = snapshot.media_time.as_secs_f64(),
            duration = duration.as_secs_f64(),
            generation = snapshot.generation,
            state = if snapshot.playing {
                "playing"
            } else {
                "paused"
            },
            tick = ms(snapshot.last_tick_duration),
            pump = ms(snapshot.last_pump_duration),
            render = ms(snapshot.last_render_duration),
            pattern = self.options.pattern,
            density = self.options.rate,
            items = self.options.item_count,
            font = self.options.font_size,
            area = self.options.display_area,
            scroll_dur = self.options.scroll_duration_seconds,
            self_every = format_self_every(self.options.self_every),
            stacking = self.options.allow_stacking,
            overwrite = self.options.allow_scroll_overwrite,
            glyphs = snapshot.current_danmaku_items,
            placed = snapshot.current_danmaku_placed_items,
            scroll = snapshot.current_danmaku_scroll_items,
            top = snapshot.current_danmaku_top_items,
            bottom = snapshot.current_danmaku_bottom_items,
            scroll_rows = snapshot.current_danmaku_scroll_rows,
            track_min = snapshot.current_danmaku_scroll_track_min,
            track_max = snapshot.current_danmaku_scroll_track_max,
            scroll_min = snapshot.current_danmaku_scroll_min_y,
            scroll_max = snapshot.current_danmaku_scroll_max_y,
            frame_buckets = frame_buckets,
            src = prepared.source_items,
            supported = prepared.supported_items,
            prep = prepared.prepared_items,
            filtered = prepared.filtered_items,
            prep_scroll = prepared.prepared_scroll_items,
            prep_top = prepared.prepared_top_items,
            prep_bottom = prepared.prepared_bottom_items,
            expected_tracks = prepared.expected_scroll_tracks,
            dfm_tracks = prepared.dfm_track_count,
            scroll_area = prepared.scroll_area_height,
            display_area_h = prepared.display_area_height,
            track_h = prepared.track_height,
            prep_rows = prepared.prepared_scroll_rows,
            prep_min = prepared.prepared_scroll_min_y,
            prep_max = prepared.prepared_scroll_max_y,
            prepared_buckets = prepared_buckets,
            dw = snapshot.current_danmaku_viewport_width,
            dh = snapshot.current_danmaku_viewport_height,
            atlas_version = snapshot.current_danmaku_atlas_version,
            atlas_mb = atlas_mb,
            sw = renderer.surface_width,
            sh = renderer.surface_height,
            rendered_video = stats.rendered_video_frames,
            rendered_test = stats.rendered_test_frames,
            passes = renderer.danmaku_passes,
            draw_items = renderer.danmaku_draw_items,
            uploads = renderer.overlay_alpha_atlas_uploads,
            reuses = renderer.overlay_alpha_atlas_reuses,
            upscaler = upscaler_label(self.options.luma_upscaler),
            upscaler_backend = upscaler_backend_label(renderer.upscaler_backend),
            upscaler_fallbacks = renderer.upscaler_fallbacks,
            upscaled_frames = renderer.upscaled_frames,
            upscaler_encode = ms(renderer.last_upscaler_encode_duration),
            gpu_frame = ms(renderer.last_gpu_duration),
            decoded_video = stats.decoded_video_frames,
            pushed_audio = stats.pushed_audio_frames,
            audio_queued = snapshot
                .audio_output_queued_duration
                .map_or(0.0, |duration| duration.as_secs_f64()),
            audio_queued_frames = snapshot.audio_output_queued_frames,
            audio_underflow = snapshot.audio_output_underflow_frames,
            import_failures = stats.import_failures,
            render_failures = stats.render_failures,
            audio_failures = stats.audio_failures,
        )
    }

    fn log_metrics_if_due(&mut self) {
        if self.metrics_log.is_none() {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_metrics_log) < Duration::from_millis(250) {
            return;
        }
        self.last_metrics_log = now;
        let line = self.metrics_json_line(now);
        if let Some(log) = &mut self.metrics_log {
            if let Err(error) = writeln!(log, "{line}").and_then(|_| log.flush()) {
                self.last_error = Some(format!("metrics log write failed: {error}"));
            }
        }
    }

    fn metrics_json_line(&mut self, now: Instant) -> String {
        self.update_fps();
        let snapshot = self.presenter.runtime_snapshot();
        let stats = snapshot.stats;
        let renderer = snapshot.renderer;
        let duration = self.presenter.duration().unwrap_or(Duration::ZERO);
        let new_danmaku_passes = renderer
            .danmaku_passes
            .saturating_sub(self.last_logged_danmaku_passes);
        let new_danmaku_draw_items = renderer
            .danmaku_draw_items
            .saturating_sub(self.last_logged_danmaku_draw_items);
        self.last_logged_danmaku_passes = renderer.danmaku_passes;
        self.last_logged_danmaku_draw_items = renderer.danmaku_draw_items;
        format!(
            "{{\"elapsed_s\":{elapsed:.3},\"fps\":{fps:.3},\"target_fps\":{target_fps:.3},\"uncapped\":{uncapped},\"media_s\":{media:.3},\"duration_s\":{duration:.3},\"generation\":{generation},\"playing\":{playing},\"tick_ms\":{tick:.3},\"pump_ms\":{pump:.3},\"audio_pump_ms\":{audio_pump:.3},\"subtitle_pump_ms\":{subtitle_pump:.3},\"video_pump_ms\":{video_pump:.3},\"clock_sync_ms\":{clock_sync:.3},\"danmaku_plan_ms\":{danmaku_plan:.3},\"render_ms\":{render:.3},\"render_current_ms\":{render_current:.3},\"render_test_ms\":{render_test:.3},\"density\":{density:.3},\"items\":{items},\"font_size\":{font:.3},\"display_area\":{area:.3},\"scroll_duration_s\":{scroll_dur:.3},\"self_every\":{self_every},\"visible_glyph_quads\":{glyphs},\"placed_items\":{placed},\"scroll_items\":{scroll},\"top_items\":{top},\"bottom_items\":{bottom},\"scroll_rows\":{scroll_rows},\"scroll_track_min\":{track_min},\"scroll_track_max\":{track_max},\"atlas_version\":{atlas_version},\"atlas_bytes\":{atlas_bytes},\"surface_width\":{sw},\"surface_height\":{sh},\"rendered_video\":{rendered_video},\"rendered_test\":{rendered_test},\"danmaku_passes\":{passes},\"danmaku_draw_items\":{draw_items},\"draw_items_per_pass\":{draw_ratio:.3},\"danmaku_passes_delta\":{passes_delta},\"danmaku_draw_items_delta\":{draw_items_delta},\"draw_items_per_new_pass\":{draw_ratio_delta:.3},\"danmaku_atlas_ms\":{danmaku_atlas:.3},\"danmaku_vertex_build_ms\":{danmaku_vertex_build:.3},\"danmaku_vertex_copy_ms\":{danmaku_vertex_copy:.3},\"danmaku_encode_ms\":{danmaku_encode:.3},\"danmaku_vertex_bytes\":{danmaku_vertex_bytes},\"danmaku_vertex_count\":{danmaku_vertex_count},\"atlas_uploads\":{uploads},\"atlas_reuses\":{reuses},\"upscaler\":\"{upscaler}\",\"upscaler_backend\":\"{upscaler_backend}\",\"upscaler_fallbacks\":{upscaler_fallbacks},\"upscaled_frames\":{upscaled_frames},\"upscaler_encode_ms\":{upscaler_encode:.3},\"gpu_frame_ms\":{gpu_frame:.3},\"decoded_video\":{decoded_video},\"pushed_audio\":{pushed_audio},\"audio_queued_s\":{audio_queued:.3},\"audio_queued_frames\":{audio_queued_frames},\"audio_underflow\":{audio_underflow},\"import_failures\":{import_failures},\"render_failures\":{render_failures},\"audio_failures\":{audio_failures},\"error\":\"{error}\"}}",
            elapsed = now.duration_since(self.started_at).as_secs_f64(),
            fps = self.fps,
            target_fps = self.options.target_fps,
            uncapped = self.options.uncapped,
            media = snapshot.media_time.as_secs_f64(),
            duration = duration.as_secs_f64(),
            generation = snapshot.generation,
            playing = snapshot.playing,
            tick = ms(snapshot.last_tick_duration),
            pump = ms(snapshot.last_pump_duration),
            audio_pump = ms(snapshot.last_audio_pump_duration),
            subtitle_pump = ms(snapshot.last_subtitle_pump_duration),
            video_pump = ms(snapshot.last_video_pump_duration),
            clock_sync = ms(snapshot.last_clock_sync_duration),
            danmaku_plan = ms(snapshot.last_danmaku_plan_duration),
            render = ms(snapshot.last_render_duration),
            render_current = ms(snapshot.last_render_current_duration),
            render_test = ms(snapshot.last_render_test_duration),
            density = self.options.rate,
            items = self.options.item_count,
            font = self.options.font_size,
            area = self.options.display_area,
            scroll_dur = self.options.scroll_duration_seconds,
            self_every = self.options.self_every.unwrap_or(0),
            glyphs = snapshot.current_danmaku_items,
            placed = snapshot.current_danmaku_placed_items,
            scroll = snapshot.current_danmaku_scroll_items,
            top = snapshot.current_danmaku_top_items,
            bottom = snapshot.current_danmaku_bottom_items,
            scroll_rows = snapshot.current_danmaku_scroll_rows,
            track_min = snapshot.current_danmaku_scroll_track_min,
            track_max = snapshot.current_danmaku_scroll_track_max,
            atlas_version = snapshot.current_danmaku_atlas_version,
            atlas_bytes = snapshot.current_danmaku_atlas_bytes,
            sw = renderer.surface_width,
            sh = renderer.surface_height,
            rendered_video = stats.rendered_video_frames,
            rendered_test = stats.rendered_test_frames,
            passes = renderer.danmaku_passes,
            draw_items = renderer.danmaku_draw_items,
            draw_ratio = if renderer.danmaku_passes > 0 {
                renderer.danmaku_draw_items as f64 / renderer.danmaku_passes as f64
            } else {
                0.0
            },
            passes_delta = new_danmaku_passes,
            draw_items_delta = new_danmaku_draw_items,
            draw_ratio_delta = if new_danmaku_passes > 0 {
                new_danmaku_draw_items as f64 / new_danmaku_passes as f64
            } else {
                0.0
            },
            danmaku_atlas = ms(renderer.last_danmaku_atlas_duration),
            danmaku_vertex_build = ms(renderer.last_danmaku_vertex_build_duration),
            danmaku_vertex_copy = ms(renderer.last_danmaku_vertex_copy_duration),
            danmaku_encode = ms(renderer.last_danmaku_encode_duration),
            danmaku_vertex_bytes = renderer.last_danmaku_vertex_bytes,
            danmaku_vertex_count = renderer.last_danmaku_vertex_count,
            uploads = renderer.overlay_alpha_atlas_uploads,
            reuses = renderer.overlay_alpha_atlas_reuses,
            upscaler = upscaler_label(self.options.luma_upscaler),
            upscaler_backend = upscaler_backend_label(renderer.upscaler_backend),
            upscaler_fallbacks = renderer.upscaler_fallbacks,
            upscaled_frames = renderer.upscaled_frames,
            upscaler_encode = ms(renderer.last_upscaler_encode_duration),
            gpu_frame = ms(renderer.last_gpu_duration),
            decoded_video = stats.decoded_video_frames,
            pushed_audio = stats.pushed_audio_frames,
            audio_queued = snapshot
                .audio_output_queued_duration
                .map_or(0.0, |duration| duration.as_secs_f64()),
            audio_queued_frames = snapshot.audio_output_queued_frames,
            audio_underflow = snapshot.audio_output_underflow_frames,
            import_failures = stats.import_failures,
            render_failures = stats.render_failures,
            audio_failures = stats.audio_failures,
            error = json_escape(self.last_error.as_deref().unwrap_or("")),
        )
    }
}

/// CLI spelling of a mode: its diagnostics label with dashes.
fn upscaler_label(mode: LumaUpscalerMode) -> String {
    mode.label().replace('_', "-")
}

fn upscaler_backend_label(status: LumaUpscalerBackendStatus) -> &'static str {
    match status {
        LumaUpscalerBackendStatus::Off => "off",
        LumaUpscalerBackendStatus::Inactive => "inactive",
        LumaUpscalerBackendStatus::Building => "building",
        LumaUpscalerBackendStatus::Scalar => "scalar",
        LumaUpscalerBackendStatus::SimdgroupMatrix => "matmul",
        LumaUpscalerBackendStatus::CooperativeMatrix => "cooperative-matrix",
        LumaUpscalerBackendStatus::LinalgMatrix => "linalg-matrix",
        LumaUpscalerBackendStatus::VideoProcessor => "video-processor",
        LumaUpscalerBackendStatus::VideoToolbox => "video-toolbox",
        LumaUpscalerBackendStatus::MetalFx => "metalfx",
        LumaUpscalerBackendStatus::TensorOps => "tensor-ops",
    }
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn layout_config_from_options(options: &PerfOptions) -> DanmakuLayoutConfig {
    DanmakuLayoutConfig {
        font_size: options.font_size,
        display_area: options.display_area,
        scroll_duration_seconds: options.scroll_duration_seconds,
        outline_width: options.outline_width,
        shadow_style: options.shadow_style,
        merge_duplicates: options.merge_duplicates,
        allow_stacking: options.allow_stacking,
        allow_scroll_overwrite: options.allow_scroll_overwrite,
        ..DanmakuLayoutConfig::default()
    }
}

fn format_buckets(
    count: usize,
    buckets: &[erika::danmaku::DanmakuDebugBucket; erika::danmaku::DANMAKU_DEBUG_BUCKETS],
) -> String {
    if count == 0 {
        return "-".to_string();
    }
    let mut parts = Vec::new();
    for bucket in buckets.iter().take(count.min(buckets.len())) {
        if bucket.count == 0 {
            continue;
        }
        parts.push(format!("{}:{}", bucket.key, bucket.count));
    }
    if count > buckets.len() {
        parts.push(format!("+{}", count - buckets.len()));
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

fn with_lab_mut<R>(fallback: R, f: impl FnOnce(&mut WindowLabState) -> R) -> R {
    LAB.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let options = WINDOW_OPTIONS
                .get()
                .cloned()
                .unwrap_or_else(PerfOptions::default);
            match WindowLabState::new(options) {
                Ok(state) => *slot = Some(state),
                Err(error) => {
                    eprintln!("danmaku perf lab create failed: {error}");
                    return fallback;
                }
            }
        }
        slot.as_mut().map_or(fallback, f)
    })
}

fn window_options_or_default() -> PerfOptions {
    WINDOW_OPTIONS
        .get()
        .cloned()
        .unwrap_or_else(PerfOptions::default)
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_attach_layer(
    layer: *mut c_void,
    width: u32,
    height: u32,
    scale: f64,
) {
    with_lab_mut((), |lab| lab.attach_surface(layer, width, height, scale));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_resize_layer(width: u32, height: u32, scale: f64) {
    with_lab_mut((), |lab| lab.resize_surface(width, height, scale));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_render_frame(host_time_seconds: f64) {
    with_lab_mut((), |lab| lab.render_tick(host_time_seconds));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_should_auto_exit() -> bool {
    with_lab_mut(false, |lab| lab.should_auto_exit())
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_toggle_play_pause() {
    with_lab_mut((), WindowLabState::toggle_play_pause);
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_seek_seconds(seconds: f64) {
    with_lab_mut((), |lab| lab.seek(seconds));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_position_seconds() -> f64 {
    with_lab_mut(0.0, |lab| lab.presenter.media_time().as_secs_f64())
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_duration_seconds() -> f64 {
    with_lab_mut(0.0, |lab| {
        lab.presenter
            .duration()
            .map(|duration| duration.as_secs_f64())
            .unwrap_or(0.0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_is_playing() -> bool {
    with_lab_mut(false, |lab| lab.presenter.is_playing())
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_metrics_text() -> *const i8 {
    let text = with_lab_mut(String::new(), WindowLabState::metrics_text);
    let c_string = CString::new(text).unwrap_or_else(|_| CString::new("invalid metrics").unwrap());
    if let Ok(mut slot) = METRICS_TEXT.lock() {
        *slot = Some(c_string);
        slot.as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr())
    } else {
        std::ptr::null()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_set_density(comments_per_second: f64) {
    with_lab_mut((), |lab| lab.set_density(comments_per_second));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_set_font_size(font_size: f64) {
    with_lab_mut((), |lab| lab.set_font_size(font_size));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_set_display_area(display_area: f64) {
    with_lab_mut((), |lab| lab.set_display_area(display_area));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_set_outline(outline_width: f64) {
    with_lab_mut((), |lab| lab.set_outline(outline_width));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_density() -> f64 {
    LAB.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|lab| lab.options.rate)
            .unwrap_or_else(|| window_options_or_default().rate)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_font_size() -> f64 {
    LAB.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|lab| lab.options.font_size as f64)
            .unwrap_or_else(|| window_options_or_default().font_size as f64)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_display_area() -> f64 {
    LAB.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|lab| lab.options.display_area as f64)
            .unwrap_or_else(|| window_options_or_default().display_area as f64)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_outline() -> f64 {
    LAB.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|lab| lab.options.outline_width as f64)
            .unwrap_or_else(|| window_options_or_default().outline_width as f64)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_window_width() -> f64 {
    window_options_or_default()
        .window_width
        .map_or(0.0, f64::from)
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_window_height() -> f64 {
    window_options_or_default()
        .window_height
        .map_or(0.0, f64::from)
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_fullscreen() -> bool {
    window_options_or_default().fullscreen
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_uncapped() -> bool {
    window_options_or_default().uncapped
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_target_fps() -> f64 {
    window_options_or_default().target_fps
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_hide_panel() -> bool {
    window_options_or_default().hide_panel
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_perf_lab_surface_scale_override() -> f64 {
    window_options_or_default()
        .surface_scale_override
        .unwrap_or(0.0)
}

fn prewarm_atlas(
    engine: &mut DfmLayoutEngine,
    viewport: DanmakuViewport,
    options: &PerfOptions,
) -> PrewarmStats {
    if options.prewarm_frames == 0 {
        return PrewarmStats::default();
    }
    let started = Instant::now();
    let mut glyph_instances = 0usize;
    let mut atlas_version = 0u64;
    let mut atlas_bytes = 0usize;
    let denominator = options.prewarm_frames.saturating_sub(1).max(1) as f64;
    for index in 0..options.prewarm_frames {
        let ratio = index as f64 / denominator;
        let media_time = Duration::from_secs_f64(options.duration_seconds * ratio);
        let plan = engine.render_plan(media_time, viewport, index as u64 + 1);
        glyph_instances = glyph_instances.saturating_add(plan.items.len());
        if let Some(atlas) = plan.atlas.as_ref() {
            atlas_version = atlas.version;
            atlas_bytes = atlas.required_len().saturating_mul(2);
        }
    }
    PrewarmStats {
        elapsed: started.elapsed(),
        frames: options.prewarm_frames,
        glyph_instances,
        atlas_version,
        atlas_bytes,
    }
}

fn run_one_frame(
    engine: &mut DfmLayoutEngine,
    viewport: DanmakuViewport,
    media_time: Duration,
    generation: u64,
    video: Option<VideoSample>,
) -> FrameSample {
    let frame_started = Instant::now();
    let layout_started = Instant::now();
    let frame_layout = engine.frame_layout(media_time, viewport, generation);
    let layout_elapsed = layout_started.elapsed();

    let render_started = Instant::now();
    let plan = engine.render_plan(media_time, viewport, generation);
    let render_elapsed = render_started.elapsed();
    let frame_stats = plan.frame_stats;
    let atlas = plan.atlas.as_ref();
    let glyph_instances = plan.items.len();
    let visible_items = frame_layout.items.len();
    let shadow_draws = plan
        .items
        .iter()
        .filter(|item| item.shadow_rgba[3] > 0.0)
        .count();
    let outline_draws = plan
        .items
        .iter()
        .filter(|item| item.outline_rgba[3] > 0.0)
        .count();
    let fill_draws = glyph_instances;

    FrameSample {
        media_time,
        total_elapsed: frame_started.elapsed(),
        layout_elapsed,
        render_plan_elapsed: render_elapsed,
        visible_items,
        glyph_instances,
        current_metal_draws: shadow_draws + outline_draws + fill_draws,
        batched_draws: estimated_batched_draws(shadow_draws, outline_draws, fill_draws),
        frame_stats,
        atlas_version: atlas.map_or(0, |atlas| atlas.version),
        atlas_bytes: atlas.map_or(0, |atlas| atlas.required_len().saturating_mul(2)),
        video,
    }
}

fn estimated_batched_draws(shadow_draws: usize, outline_draws: usize, fill_draws: usize) -> usize {
    usize::from(shadow_draws > 0) + usize::from(outline_draws > 0) + usize::from(fill_draws > 0)
}

fn print_report(prepare_elapsed: Duration, samples: &[FrameSample]) {
    let mut layout = samples
        .iter()
        .map(|sample| sample.layout_elapsed)
        .collect::<Vec<_>>();
    let mut render_plan = samples
        .iter()
        .map(|sample| sample.render_plan_elapsed)
        .collect::<Vec<_>>();
    let mut total = samples
        .iter()
        .map(|sample| sample.total_elapsed)
        .collect::<Vec<_>>();
    let visible = samples
        .iter()
        .map(|sample| sample.visible_items)
        .collect::<Vec<_>>();
    let glyphs = samples
        .iter()
        .map(|sample| sample.glyph_instances)
        .collect::<Vec<_>>();
    let current_draws = samples
        .iter()
        .map(|sample| sample.current_metal_draws)
        .collect::<Vec<_>>();
    let batched_draws = samples
        .iter()
        .map(|sample| sample.batched_draws)
        .collect::<Vec<_>>();
    let atlas_changes = samples
        .windows(2)
        .filter(|pair| pair[0].atlas_version != pair[1].atlas_version)
        .count()
        + usize::from(
            samples
                .first()
                .is_some_and(|sample| sample.atlas_version > 0),
        );
    let max_atlas_bytes = samples
        .iter()
        .map(|sample| sample.atlas_bytes)
        .max()
        .unwrap_or(0);

    println!("\nsummary");
    println!("prepare_ms: {:.3}", ms(prepare_elapsed));
    print_duration_stats("frame_total_ms", &mut total);
    print_duration_stats("standalone_frame_layout_ms", &mut layout);
    print_duration_stats("render_plan_total_ms", &mut render_plan);
    print_usize_stats("visible_items", &visible);
    print_usize_stats("glyph_instances", &glyphs);
    print_usize_stats("current_metal_draws", &current_draws);
    print_usize_stats("batched_target_draws", &batched_draws);
    let current_avg = avg_usize(&current_draws);
    let batched_avg = avg_usize(&batched_draws).max(1.0);
    println!(
        "draw_call_reduction_target: {:.1}x fewer draws after batching",
        current_avg / batched_avg
    );
    println!("atlas_changes: {atlas_changes}");
    println!("atlas_bytes_max: {max_atlas_bytes}");

    if let Some(video) = samples.iter().find_map(|sample| sample.video) {
        let hardware_frames = samples
            .iter()
            .filter(|sample| sample.video.is_some_and(|video| video.is_hardware))
            .count();
        let max_late = samples
            .iter()
            .filter_map(|sample| sample.video.and_then(|video| video.late_by))
            .max()
            .unwrap_or(Duration::ZERO);
        println!(
            "video_frames: {} first={}x{} hw_frames={} max_late_ms={:.3}",
            samples
                .iter()
                .filter(|sample| sample.video.is_some())
                .count(),
            video.width,
            video.height,
            hardware_frames,
            ms(max_late)
        );
    }

    if let Some(worst) = samples
        .iter()
        .max_by_key(|sample| sample.render_plan_elapsed)
    {
        println!(
            "worst_render_plan_frame: t={:.3}s render_plan_total_ms={:.3} visible={} glyphs={} atlas_version={}",
            worst.media_time.as_secs_f64(),
            ms(worst.render_plan_elapsed),
            worst.visible_items,
            worst.glyph_instances,
            worst.atlas_version
        );
        print_frame_sensor("worst_frame_sensor", worst);
    }
    if let Some(last) = samples.last() {
        print_frame_sensor("last_frame_sensor", last);
    }
}

fn print_frame_sensor(label: &str, sample: &FrameSample) {
    let stats = sample.frame_stats;
    println!(
        "{label}: t={:.3}s placed={} scroll/top/bottom={}/{}/{} scroll_rows={} tracks={}..{} y={:.0}..{:.0} buckets={}",
        sample.media_time.as_secs_f64(),
        stats.placed_items,
        stats.scroll_items,
        stats.top_items,
        stats.bottom_items,
        stats.scroll_rows,
        stats.scroll_track_min,
        stats.scroll_track_max,
        stats.scroll_min_y,
        stats.scroll_max_y,
        format_buckets(stats.scroll_bucket_count, &stats.scroll_buckets),
    );
}

fn print_duration_stats(label: &str, values: &mut [Duration]) {
    values.sort_unstable();
    println!(
        "{label}: avg={:.3} p50={:.3} p95={:.3} max={:.3}",
        ms(avg_duration(values)),
        ms(percentile_duration(values, 50)),
        ms(percentile_duration(values, 95)),
        ms(*values.last().unwrap_or(&Duration::ZERO))
    );
}

fn print_usize_stats(label: &str, values: &[usize]) {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let avg = avg_usize(&sorted);
    println!(
        "{label}: avg={avg:.1} p50={} p95={} max={}",
        percentile_usize(&sorted, 50),
        percentile_usize(&sorted, 95),
        sorted.last().copied().unwrap_or(0)
    );
}

fn avg_usize(values: &[usize]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<usize>() as f64 / values.len() as f64
    }
}

fn percentile_duration(values: &[Duration], percentile: usize) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    values[percentile_index(values.len(), percentile)]
}

fn percentile_usize(values: &[usize], percentile: usize) -> usize {
    if values.is_empty() {
        return 0;
    }
    values[percentile_index(values.len(), percentile)]
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

fn load_or_generate_timeline(options: &PerfOptions) -> Result<DanmakuTimeline, String> {
    if let Some(path) = &options.danmaku_path {
        return DanmakuTimeline::from_file(path).map_err(|error| error.to_string());
    }
    generate_timeline(options)
}

fn generate_timeline(options: &PerfOptions) -> Result<DanmakuTimeline, String> {
    let mut items = Vec::with_capacity(options.item_count);
    let duration = options.duration_seconds.max(0.001);
    for index in 0..options.item_count {
        let time = (index as f64 / options.rate.max(0.001)).min(duration);
        let mode = mode_for(options.pattern, index);
        let color = color_for(index);
        let text = text_for(options.pattern, index);
        items.push(DanmakuItem {
            id: index as u64 + 1,
            pts: Duration::from_secs_f64(time),
            text,
            mode,
            font_size: 25.0,
            color,
            opacity: 1.0,
            is_self: options.self_every.is_some_and(|every| index % every == 0),
            special: None,
        });
    }
    DanmakuTimeline::new(items).map_err(|error| error.to_string())
}

fn format_self_every(self_every: Option<usize>) -> String {
    self_every.map_or_else(|| "off".to_string(), |every| every.to_string())
}

fn mode_for(pattern: PerfPattern, index: usize) -> DanmakuMode {
    match pattern {
        PerfPattern::Scroll => DanmakuMode::Scroll,
        PerfPattern::Fixed => {
            if index % 2 == 0 {
                DanmakuMode::Top
            } else {
                DanmakuMode::Bottom
            }
        }
        PerfPattern::Dense | PerfPattern::Mixed => match index % 20 {
            0 | 1 => DanmakuMode::Top,
            2 => DanmakuMode::Bottom,
            3 if matches!(pattern, PerfPattern::Mixed) => DanmakuMode::ScrollReverse,
            _ => DanmakuMode::Scroll,
        },
    }
}

fn text_for(pattern: PerfPattern, index: usize) -> String {
    const BASE: &[&str] = &[
        "这句好戳",
        "现在这条应该跟着画面走",
        "Erika 原生弹幕压测",
        "seek 后不能回跳",
        "DFM+ track collision",
        "莉可丽丝",
        "画面停弹幕也停",
        "glyph atlas reuse",
        "批量绘制前基准",
        "高密度滚动测试",
    ];
    match pattern {
        PerfPattern::Dense => format!("{} #{:05}", BASE[index % BASE.len()], index),
        _ => format!("{} {}", BASE[index % BASE.len()], suffix_for(index)),
    }
}

fn suffix_for(index: usize) -> &'static str {
    match index % 8 {
        0 => "草",
        1 => "2333",
        2 => "wwww",
        3 => "!!!",
        4 => "x5",
        5 => "弹幕雨",
        6 => "同步",
        _ => "GPU",
    }
}

fn color_for(index: usize) -> DanmakuColor {
    const COLORS: &[(u8, u8, u8)] = &[
        (255, 255, 255),
        (255, 218, 86),
        (101, 220, 255),
        (255, 112, 169),
        (122, 255, 159),
        (218, 189, 255),
    ];
    let (red, green, blue) = COLORS[index % COLORS.len()];
    DanmakuColor::rgb_u8(red, green, blue)
}

#[derive(Debug, Clone, PartialEq)]
struct PerfOptions {
    window: bool,
    fullscreen: bool,
    uncapped: bool,
    danmaku_path: Option<String>,
    video_uri: Option<String>,
    decode_preference: VideoDecodePreference,
    item_count: usize,
    rate: f64,
    duration_seconds: f64,
    frames: usize,
    prewarm_frames: usize,
    fps: f64,
    start_time_seconds: f64,
    width: u32,
    height: u32,
    font_size: f32,
    display_area: f32,
    scroll_duration_seconds: f32,
    outline_width: f32,
    shadow_style: DanmakuShadowStyle,
    merge_duplicates: bool,
    allow_stacking: bool,
    allow_scroll_overwrite: bool,
    self_every: Option<usize>,
    metrics_log_path: Option<String>,
    auto_exit_seconds: Option<f64>,
    pattern: PerfPattern,
    window_width: Option<u32>,
    window_height: Option<u32>,
    hide_panel: bool,
    surface_scale_override: Option<f64>,
    target_fps: f64,
    luma_upscaler: LumaUpscalerMode,
}

impl Default for PerfOptions {
    fn default() -> Self {
        let duration_seconds = 120.0;
        let rate = 50.0;
        Self {
            window: false,
            fullscreen: false,
            uncapped: false,
            danmaku_path: None,
            video_uri: None,
            decode_preference: VideoDecodePreference::default(),
            item_count: (duration_seconds * rate) as usize,
            rate,
            duration_seconds,
            frames: 600,
            prewarm_frames: 0,
            fps: 60.0,
            start_time_seconds: 0.0,
            width: 1920,
            height: 1080,
            font_size: 30.0,
            display_area: 1.0,
            scroll_duration_seconds: 10.0,
            outline_width: 1.0,
            shadow_style: DanmakuShadowStyle::Strong,
            merge_duplicates: false,
            allow_stacking: false,
            allow_scroll_overwrite: false,
            self_every: None,
            metrics_log_path: None,
            auto_exit_seconds: None,
            pattern: PerfPattern::Mixed,
            window_width: None,
            window_height: None,
            hide_panel: false,
            surface_scale_override: None,
            target_fps: 60.0,
            luma_upscaler: LumaUpscalerMode::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerfPattern {
    Mixed,
    Scroll,
    Fixed,
    Dense,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VideoSample {
    width: usize,
    height: usize,
    is_hardware: bool,
    late_by: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
struct FrameSample {
    media_time: Duration,
    total_elapsed: Duration,
    layout_elapsed: Duration,
    render_plan_elapsed: Duration,
    visible_items: usize,
    glyph_instances: usize,
    current_metal_draws: usize,
    batched_draws: usize,
    frame_stats: DanmakuFrameStats,
    atlas_version: u64,
    atlas_bytes: usize,
    video: Option<VideoSample>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PrewarmStats {
    elapsed: Duration,
    frames: usize,
    glyph_instances: usize,
    atlas_version: u64,
    atlas_bytes: usize,
}

fn parse_args(args: &[String]) -> Result<PerfOptions, String> {
    let mut options = PerfOptions::default();
    let mut explicit_items = false;
    let mut explicit_rate = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--window" => options.window = true,
            "--fullscreen" => {
                options.window = true;
                options.fullscreen = true;
            }
            "--uncapped" => {
                options.window = true;
                options.uncapped = true;
            }
            "--danmaku" => options.danmaku_path = Some(next_string(args, &mut index, "--danmaku")?),
            "--video" => options.video_uri = Some(next_string(args, &mut index, "--video")?),
            "--software" => options.decode_preference = VideoDecodePreference::Software,
            "--upscaler" => {
                let value = next_string(args, &mut index, "--upscaler")?;
                options.luma_upscaler = LumaUpscalerMode::ALL
                    .into_iter()
                    .find(|mode| upscaler_label(*mode) == value.replace('_', "-"))
                    .ok_or_else(|| {
                        let expected: Vec<String> = LumaUpscalerMode::ALL
                            .into_iter()
                            .map(upscaler_label)
                            .collect();
                        format!(
                            "invalid --upscaler value: {value} (expected {})",
                            expected.join("|")
                        )
                    })?;
            }
            "--videotoolbox" => options.decode_preference = VideoDecodePreference::VideoToolbox,
            "--d3d11va" => options.decode_preference = VideoDecodePreference::D3d11va,
            "--items" => {
                options.item_count = next_parse(args, &mut index, "--items")?;
                explicit_items = true;
            }
            "--rate" => {
                options.rate = next_parse(args, &mut index, "--rate")?;
                explicit_rate = true;
            }
            "--duration" => options.duration_seconds = next_parse(args, &mut index, "--duration")?,
            "--frames" => options.frames = next_parse(args, &mut index, "--frames")?,
            "--prewarm-frames" => {
                options.prewarm_frames = next_parse(args, &mut index, "--prewarm-frames")?
            }
            "--fps" => options.fps = next_parse(args, &mut index, "--fps")?,
            "--start-time" => {
                let seconds: f64 = next_parse(args, &mut index, "--start-time")?;
                options.start_time_seconds = if seconds.is_finite() {
                    seconds.max(0.0)
                } else {
                    0.0
                };
            }
            "--size" => {
                let value = next_string(args, &mut index, "--size")?;
                let (width, height) = parse_size(&value)?;
                options.width = width;
                options.height = height;
            }
            "--window-size" => {
                let value = next_string(args, &mut index, "--window-size")?;
                let (width, height) = parse_size(&value)?;
                options.window_width = Some(width);
                options.window_height = Some(height);
            }
            "--hide-panel" => options.hide_panel = true,
            "--surface-scale" => {
                let scale: f64 = next_parse(args, &mut index, "--surface-scale")?;
                options.surface_scale_override =
                    (scale.is_finite() && scale > 0.0).then_some(scale);
            }
            "--target-fps" => options.target_fps = next_parse(args, &mut index, "--target-fps")?,
            "--font-size" => options.font_size = next_parse(args, &mut index, "--font-size")?,
            "--display-area" => {
                options.display_area = next_parse(args, &mut index, "--display-area")?
            }
            "--scroll-duration" => {
                options.scroll_duration_seconds = next_parse(args, &mut index, "--scroll-duration")?
            }
            "--outline" => options.outline_width = next_parse(args, &mut index, "--outline")?,
            "--no-shadow" => options.shadow_style = DanmakuShadowStyle::None,
            "--merge" => options.merge_duplicates = true,
            "--stacking" => options.allow_stacking = true,
            "--scroll-overwrite" => options.allow_scroll_overwrite = true,
            "--self-every" => {
                let value: usize = next_parse(args, &mut index, "--self-every")?;
                options.self_every = (value > 0).then_some(value);
            }
            "--metrics-log" => {
                options.metrics_log_path = Some(next_string(args, &mut index, "--metrics-log")?)
            }
            "--auto-exit" => {
                let seconds: f64 = next_parse(args, &mut index, "--auto-exit")?;
                if seconds.is_finite() && seconds > 0.0 {
                    options.auto_exit_seconds = Some(seconds);
                }
            }
            "--pattern" => {
                options.pattern = parse_pattern(&next_string(args, &mut index, "--pattern")?)?
            }
            value if value.starts_with("--") => return Err(format!("unknown option: {value}")),
            value => {
                if options.video_uri.replace(value.to_string()).is_some() {
                    return Err("video path was provided more than once".to_string());
                }
            }
        }
        index += 1;
    }
    validate_options(&mut options, explicit_items, explicit_rate)?;
    Ok(options)
}

fn validate_options(
    options: &mut PerfOptions,
    explicit_items: bool,
    explicit_rate: bool,
) -> Result<(), String> {
    if !options.duration_seconds.is_finite() || options.duration_seconds <= 0.0 {
        return Err("--duration must be positive and finite".to_string());
    }
    if !options.rate.is_finite() || options.rate <= 0.0 {
        return Err("--rate must be positive and finite".to_string());
    }
    if options.frames == 0 {
        return Err("--frames must be greater than zero".to_string());
    }
    if !options.fps.is_finite() || options.fps <= 0.0 {
        return Err("--fps must be positive and finite".to_string());
    }
    if !options.target_fps.is_finite() || options.target_fps <= 0.0 {
        return Err("--target-fps must be positive and finite".to_string());
    }
    if options.width == 0 || options.height == 0 {
        return Err("--size dimensions must be greater than zero".to_string());
    }
    if !options.font_size.is_finite() || options.font_size <= 0.0 {
        return Err("--font-size must be positive and finite".to_string());
    }
    if !options.display_area.is_finite() || options.display_area <= 0.0 {
        return Err("--display-area must be positive and finite".to_string());
    }
    if !options.scroll_duration_seconds.is_finite() || options.scroll_duration_seconds <= 0.0 {
        return Err("--scroll-duration must be positive and finite".to_string());
    }
    if !options.outline_width.is_finite() || options.outline_width < 0.0 {
        return Err("--outline must be finite and non-negative".to_string());
    }
    if !explicit_items && explicit_rate {
        options.item_count = (options.duration_seconds * options.rate).round() as usize;
    } else if explicit_items && !explicit_rate {
        options.rate = options.item_count as f64 / options.duration_seconds;
    }
    if options.item_count == 0 {
        return Err("--items must be greater than zero".to_string());
    }
    Ok(())
}

fn next_string(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn next_parse<T>(args: &[String], index: &mut usize, flag: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    let value = next_string(args, index, flag)?;
    value
        .parse::<T>()
        .map_err(|_| format!("invalid {flag} value: {value}"))
}

fn parse_size(value: &str) -> Result<(u32, u32), String> {
    let Some((width, height)) = value.split_once('x').or_else(|| value.split_once('X')) else {
        return Err("--size must look like 1920x1080".to_string());
    };
    let width = width
        .parse::<u32>()
        .map_err(|_| format!("invalid width in --size: {value}"))?;
    let height = height
        .parse::<u32>()
        .map_err(|_| format!("invalid height in --size: {value}"))?;
    Ok((width, height))
}

fn parse_pattern(value: &str) -> Result<PerfPattern, String> {
    match value {
        "mixed" => Ok(PerfPattern::Mixed),
        "scroll" => Ok(PerfPattern::Scroll),
        "fixed" => Ok(PerfPattern::Fixed),
        "dense" => Ok(PerfPattern::Dense),
        _ => Err(format!("unknown pattern: {value}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_derives_items_from_rate_and_duration() {
        let options = parse_args(&[
            "--rate".to_string(),
            "100".to_string(),
            "--duration".to_string(),
            "10".to_string(),
        ])
        .unwrap();

        assert_eq!(options.item_count, 1000);
        assert_eq!(options.rate, 100.0);
    }

    #[test]
    fn parse_args_accepts_size_and_pattern() {
        let options = parse_args(&[
            "--size".to_string(),
            "1280x720".to_string(),
            "--pattern".to_string(),
            "dense".to_string(),
        ])
        .unwrap();

        assert_eq!(options.width, 1280);
        assert_eq!(options.height, 720);
        assert_eq!(options.pattern, PerfPattern::Dense);
    }

    #[test]
    fn parse_args_accepts_c4f16_ds_upscaler() {
        let options =
            parse_args(&["--upscaler".to_string(), "artcnn-c4f16-ds".to_string()]).unwrap();

        assert_eq!(options.luma_upscaler, LumaUpscalerMode::ArtCnnC4F16Ds);
    }
}
