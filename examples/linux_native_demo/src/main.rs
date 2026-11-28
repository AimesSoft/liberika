#[cfg(not(all(target_os = "linux", not(target_env = "ohos"))))]
fn main() {
    eprintln!("linux_native_demo only runs on desktop Linux.");
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux_demo::run()
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
mod linux_demo {
    use erika::presenter::{PresenterConfig, PresenterRuntime};
    use erika::{MediaRequest, PlatformSurface, WgpuSurfaceHandle, WgpuSurfaceKind};
    use std::time::{Duration, Instant};
    use winit::application::ApplicationHandler;
    use winit::dpi::LogicalSize;
    use winit::event::{ElementState, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::keyboard::{Key, NamedKey};
    use winit::raw_window_handle::{
        HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    };
    use winit::window::{Window, WindowId};

    struct App {
        // Destroy the presenter (including its borrowed surface) before the window.
        presenter: Option<PresenterRuntime>,
        window: Option<Window>,
        media: Option<String>,
        smoke_duration: Option<Duration>,
        min_media_time: Option<Duration>,
        require_audio: bool,
        start: Instant,
        next_tick: Instant,
        error: Option<String>,
    }

    impl App {
        fn initialize(
            &mut self,
            event_loop: &ActiveEventLoop,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let window = event_loop.create_window(
                Window::default_attributes()
                    .with_title("Erika — Linux (Space: pause, ←/→: seek)")
                    .with_inner_size(LogicalSize::new(960.0, 540.0)),
            )?;
            let (kind, raw_window, raw_display) = match (
                window.window_handle()?.as_raw(),
                window.display_handle()?.as_raw(),
            ) {
                (RawWindowHandle::Xlib(window), RawDisplayHandle::Xlib(display)) => (
                    WgpuSurfaceKind::XlibWindow,
                    window.window,
                    display.display.ok_or("Xlib display is null")?.as_ptr() as u64,
                ),
                (RawWindowHandle::Wayland(window), RawDisplayHandle::Wayland(display)) => (
                    WgpuSurfaceKind::WaylandSurface,
                    window.surface.as_ptr() as u64,
                    display.display.as_ptr() as u64,
                ),
                _ => return Err("expected an Xlib or Wayland window".into()),
            };
            let size = window.inner_size();
            let mut presenter = PresenterRuntime::new(PresenterConfig {
                render_test_pattern_when_idle: self.media.is_none(),
                ..PresenterConfig::default()
            })?;
            presenter.attach_surface(PlatformSurface::Wgpu(WgpuSurfaceHandle::new(
                kind,
                raw_window,
                raw_display,
                size.width,
                size.height,
                window.scale_factor(),
            )))?;
            if let Some(media) = &self.media {
                presenter.open(MediaRequest::new(media.clone()))?;
                presenter.play()?;
            }
            println!("Linux surface: {kind:?}");
            self.start = Instant::now();
            self.next_tick = self.start;
            self.presenter = Some(presenter);
            self.window = Some(window);
            Ok(())
        }

        fn fail(&mut self, event_loop: &ActiveEventLoop, error: impl ToString) {
            self.error = Some(error.to_string());
            event_loop.exit();
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_none() {
                if let Err(error) = self.initialize(event_loop) {
                    self.fail(event_loop, error);
                }
            }
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            let Some(presenter) = &mut self.presenter else {
                return;
            };
            let result = match event {
                WindowEvent::CloseRequested => {
                    event_loop.exit();
                    Ok(())
                }
                WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => presenter
                    .resize_surface(
                        size.width,
                        size.height,
                        self.window.as_ref().unwrap().scale_factor(),
                    ),
                WindowEvent::RedrawRequested => {
                    let size = self.window.as_ref().unwrap().inner_size();
                    if size.width > 0 && size.height > 0 {
                        presenter
                            .render_tick(self.start.elapsed().as_secs_f64())
                            .map(|_| ())
                    } else {
                        Ok(())
                    }
                }
                WindowEvent::KeyboardInput { event, .. }
                    if event.state == ElementState::Pressed && !event.repeat =>
                {
                    let position = presenter.runtime_snapshot().media_time;
                    match event.logical_key {
                        Key::Named(NamedKey::Space) => {
                            if presenter.is_playing() {
                                presenter.pause()
                            } else {
                                presenter.play()
                            }
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            presenter.seek(position.saturating_sub(Duration::from_secs(5)))
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            presenter.seek(position + Duration::from_secs(5))
                        }
                        Key::Named(NamedKey::Escape) => {
                            event_loop.exit();
                            Ok(())
                        }
                        _ => Ok(()),
                    }
                }
                _ => Ok(()),
            };
            if let Err(error) = result {
                self.fail(event_loop, error);
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if self
                .smoke_duration
                .is_some_and(|limit| self.start.elapsed() >= limit)
            {
                event_loop.exit();
                return;
            }
            let now = Instant::now();
            if now >= self.next_tick {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                self.next_tick = now + Duration::from_millis(16);
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_tick));
        }

        fn exiting(&mut self, _: &ActiveEventLoop) {
            if let Some(mut presenter) = self.presenter.take() {
                let snapshot = presenter.runtime_snapshot();
                println!(
                    "rendered_video_frames={} rendered_test_frames={} audio_read_frames={} render_failures={} audio_failures={} media_time={:.3}",
                    snapshot.stats.rendered_video_frames,
                    snapshot.stats.rendered_test_frames,
                    snapshot.audio_output_read_frames,
                    snapshot.stats.render_failures,
                    snapshot.stats.audio_failures,
                    snapshot.media_time.as_secs_f64()
                );
                if self.require_audio && snapshot.audio_output_read_frames == 0 {
                    self.error = Some("smoke check: no audio frames reached the output".into());
                }
                if self
                    .min_media_time
                    .is_some_and(|minimum| snapshot.media_time < minimum)
                {
                    self.error =
                        Some("smoke check: playback did not reach the required media time".into());
                }
                if self.smoke_duration.is_some()
                    && (snapshot.stats.render_failures > 0
                        || snapshot.stats.audio_failures > 0
                        || snapshot.stats.rendered_video_frames
                            + snapshot.stats.rendered_test_frames
                            == 0)
                {
                    self.error = Some("smoke check: playback/rendering failed".into());
                }
                let _ = presenter.close();
                let _ = presenter.detach_surface();
            }
        }
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = EventLoop::builder();
        let mut app = App {
            presenter: None,
            window: None,
            media: None,
            smoke_duration: None,
            min_media_time: None,
            require_audio: false,
            start: Instant::now(),
            next_tick: Instant::now(),
            error: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--x11" => {
                    use winit::platform::x11::EventLoopBuilderExtX11;
                    builder.with_x11();
                }
                "--wayland" => {
                    use winit::platform::wayland::EventLoopBuilderExtWayland;
                    builder.with_wayland();
                }
                "--smoke-seconds" => {
                    let seconds: f64 = args.next().ok_or("missing smoke duration")?.parse()?;
                    if !seconds.is_finite() || seconds <= 0.0 {
                        return Err("smoke duration must be positive".into());
                    }
                    app.smoke_duration = Some(Duration::try_from_secs_f64(seconds)?);
                }
                "--require-audio" => app.require_audio = true,
                "--min-media-seconds" => {
                    let seconds: f64 = args.next().ok_or("missing minimum media time")?.parse()?;
                    app.min_media_time = Some(Duration::try_from_secs_f64(seconds)?);
                }
                "--help" | "-h" => {
                    println!(
                        "linux_native_demo [--x11|--wayland] [--smoke-seconds N] [--require-audio] [--min-media-seconds N] [FILE_OR_URL]"
                    );
                    return Ok(());
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown option: {value}").into());
                }
                _ if app.media.is_none() => app.media = Some(arg),
                _ => return Err("only one media source may be supplied".into()),
            }
        }
        builder.build()?.run_app(&mut app)?;
        if let Some(error) = app.error {
            return Err(error.into());
        }
        Ok(())
    }
}
