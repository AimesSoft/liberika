use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use erika::playback::VideoDecodePreference;
use erika::source::{ByteRange, ColdOpenCancellation, HttpRangeSource, MediaSource, SourceError};
use erika::{MediaRequest, MediaSourceHint, Player, PlayerConfig, PlayerState};

const WAIT: Duration = Duration::from_secs(5);
const CANCEL_LIMIT: Duration = Duration::from_millis(500);

fn read_head(stream: &TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut head = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line == "\r\n" => break,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                break;
            }
            Err(error) => panic!("read HTTP request: {error}"),
        }
        head.push_str(&line);
    }
    head
}

fn write_range(stream: &mut TcpStream, start: usize, body: bool) {
    write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/1000\r\nContent-Length: 100\r\nConnection: close\r\n\r\n", start + 99).unwrap();
    if body {
        stream.write_all(&[42; 100]).unwrap();
    }
}

fn assert_peer_closed(stream: &mut TcpStream) {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
        result => panic!("cancelled HTTP connection stayed open: {result:?}"),
    }
}

#[test]
fn cancellation_interrupts_metadata_headers_and_body() {
    for phase in ["metadata", "headers", "body"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/media", listener.local_addr().unwrap());
        let (started_tx, started_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(WAIT)).unwrap();
            assert!(read_head(&stream).starts_with("HEAD"));
            if phase != "metadata" {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                drop(stream);
                stream = listener.accept().unwrap().0;
                stream.set_read_timeout(Some(WAIT)).unwrap();
                assert!(read_head(&stream).starts_with("GET"));
                if phase == "body" {
                    write_range(&mut stream, 0, false);
                }
            }
            started_tx.send(()).unwrap();
            assert_peer_closed(&mut stream);
        });
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(uri, vec![], Some(100));
        let cancellation = source.cancellation().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let result = if phase == "metadata" {
                source.len().map(|_| ())
            } else {
                source
                    .read_range(ByteRange {
                        start: 0,
                        length: Some(100),
                    })
                    .map(|_| ())
            };
            drop(source);
            done_tx.send(result).unwrap();
        });
        started_rx.recv_timeout(WAIT).unwrap();
        let started = Instant::now();
        cancellation.cancel();
        let result = done_rx
            .recv_timeout(CANCEL_LIMIT)
            .expect("cancel did not interrupt HTTP I/O");
        assert!(matches!(result, Err(SourceError::Cancelled)));
        eprintln!(
            "HTTP {phase} cancelled in {:.2} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        reader.join().unwrap();
        server.join().unwrap();
    }
}

#[test]
fn cancellation_interrupts_a_stalled_tls_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("https://{}/media", listener.local_addr().unwrap());
    let (started_tx, started_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut hello = [0; 16384];
        assert!(stream.read(&mut hello).unwrap() > 0);
        started_tx.send(()).unwrap();
        assert_peer_closed(&mut stream);
    });
    let mut source = HttpRangeSource::new(uri);
    let cancellation = source.cancellation().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let _ = done_tx.send(source.len());
    });
    started_rx.recv_timeout(WAIT).unwrap();
    let started = Instant::now();
    cancellation.cancel();
    assert!(matches!(
        done_rx.recv_timeout(CANCEL_LIMIT).unwrap(),
        Err(SourceError::Cancelled)
    ));
    eprintln!(
        "TLS handshake cancelled in {:.2} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    reader.join().unwrap();
    server.join().unwrap();
}

#[test]
fn stalled_prefetch_is_cancelled_on_drop_or_release_and_source_can_replay() {
    for release_only in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/media", listener.local_addr().unwrap());
        let (started_tx, started_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(WAIT)).unwrap();
                let head = read_head(&stream);
                assert!(
                    head.to_lowercase()
                        .contains("authorization: bearer test-token")
                );
                if index == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    write_range(&mut stream, (index - 1) * 100, index == 1);
                    if index == 2 {
                        started_tx.send(()).unwrap();
                        assert_peer_closed(&mut stream);
                        closed_tx.send(()).unwrap();
                    }
                }
            }
            if release_only {
                let (mut stream, _) = listener.accept().unwrap();
                assert!(
                    read_head(&stream)
                        .to_lowercase()
                        .contains("range: bytes=0-99")
                );
                write_range(&mut stream, 0, true);
            }
        });
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            vec![("Authorization".into(), "Bearer test-token".into())],
            Some(100),
        );
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(60)
                })
                .unwrap(),
            vec![42; 60]
        );
        started_rx.recv_timeout(WAIT).unwrap();
        let started = Instant::now();
        if release_only {
            source.release_buffer();
        } else {
            drop(source);
            closed_rx.recv_timeout(CANCEL_LIMIT).unwrap();
            assert!(started.elapsed() < CANCEL_LIMIT);
            eprintln!(
                "HTTP source drop cancelled prefetch in {:.2} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
            server.join().unwrap();
            continue;
        }
        closed_rx.recv_timeout(CANCEL_LIMIT).unwrap();
        assert!(started.elapsed() < CANCEL_LIMIT);
        eprintln!(
            "HTTP release_buffer cancelled prefetch in {:.2} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(10)
                })
                .unwrap(),
            vec![42; 10]
        );
        server.join().unwrap();
    }
}

struct MediaServer {
    uri: String,
    stall_next: Arc<AtomicBool>,
    stalled: mpsc::Receiver<()>,
    cancelled: mpsc::Receiver<()>,
    shutdown: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl MediaServer {
    fn start() -> Self {
        Self::with_bytes(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/testdata/playback/playback-fixture.mkv"
            ))
            .unwrap(),
        )
    }

    fn with_bytes(bytes: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let uri = format!("http://{}/fixture.mkv", listener.local_addr().unwrap());
        let bytes = Arc::new(bytes);
        let stall_next = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (stalled_tx, stalled) = mpsc::channel();
        let (cancelled_tx, cancelled) = mpsc::channel();
        let worker = {
            let stall_next = stall_next.clone();
            let shutdown = shutdown.clone();
            thread::spawn(move || {
                let mut clients: Vec<thread::JoinHandle<()>> = Vec::new();
                while !shutdown.load(Ordering::Acquire) {
                    let mut index = 0;
                    while index < clients.len() {
                        if clients[index].is_finished() {
                            clients.swap_remove(index).join().unwrap();
                        } else {
                            index += 1;
                        }
                    }
                    let (mut stream, _) = match listener.accept() {
                        Ok(client) => client,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("accept: {error}"),
                    };
                    let bytes = bytes.clone();
                    let stall_next = stall_next.clone();
                    let stalled_tx = stalled_tx.clone();
                    let cancelled_tx = cancelled_tx.clone();
                    clients.push(thread::spawn(move || {
                        if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(WAIT)).is_err() {
                            // A superseded seek can cancel before its request
                            // reaches this accepted socket.
                            return;
                        }
                        let head = read_head(&stream);
                        if head.starts_with("HEAD") {
                            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
                            return;
                        }
                        let head = head.to_lowercase();
                        // Cancellation may close a just-connected socket before
                        // the request headers have been written.
                        let Some(range) = head.lines().find_map(|line| line.strip_prefix("range: bytes=")) else { return; };
                        let (start, end) = range.trim().split_once('-').unwrap();
                        let start = start.parse::<usize>().unwrap();
                        let end = end.parse::<usize>().unwrap_or(bytes.len() - 1).min(bytes.len() - 1);
                        if start >= bytes.len() {
                            let _ = stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                            return;
                        }
                        let _ = write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len(), end - start + 1);
                        if stall_next.swap(false, Ordering::AcqRel) {
                            let _ = stalled_tx.send(());
                            assert_peer_closed(&mut stream);
                            let _ = cancelled_tx.send(());
                        } else {
                            let _ = stream.write_all(&bytes[start..=end]);
                        }
                    }));
                }
                for client in clients {
                    client.join().unwrap();
                }
            })
        };
        Self {
            uri,
            stall_next,
            stalled,
            cancelled,
            shutdown,
            worker: Some(worker),
        }
    }

    fn request(&self) -> MediaRequest {
        MediaRequest {
            uri: self.uri.clone(),
            source_hint: MediaSourceHint::Http,
            http_headers: vec![],
            http_read_ahead_bytes: Some(64 * 1024),
            http_back_buffer_bytes: Some(64 * 1024),
            start_position: None,
        }
    }
}

impl Drop for MediaServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

#[test]
fn interrupting_a_pending_open_closes_the_media_request_and_allows_replay() {
    let server = MediaServer::start();
    server.stall_next.store(true, Ordering::Release);
    let request = server.request();
    let ticket = ColdOpenCancellation::default();
    let capability = ticket.claim().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let player = Player::new(PlayerConfig::default());
        let result = player.open_with_cold_open_cancellation(request, Some(capability));
        done_tx.send((player, result)).unwrap();
    });
    server.stalled.recv_timeout(WAIT).unwrap();
    ticket.interrupt();
    let (player, result) = done_rx
        .recv_timeout(CANCEL_LIMIT)
        .expect("open remained blocked");
    assert!(result.is_err());
    assert_eq!(player.state(), PlayerState::Stopped);
    server
        .cancelled
        .recv_timeout(CANCEL_LIMIT)
        .expect("media socket stayed open");
    worker.join().unwrap();

    let next = ColdOpenCancellation::default();
    // The original retry-only signal still permits a healthy first request.
    next.cancel();
    player
        .open_with_cold_open_cancellation(server.request(), next.claim())
        .unwrap();
    // A duplicate exit signal and a late interrupt after successful open must
    // leave the new HTTP source usable.
    ticket.interrupt();
    next.interrupt();
    let frames = player.subscribe_video_frames();
    player.play().unwrap();
    frames
        .recv_timeout(WAIT)
        .expect("sealed ticket affected playback");
    player.close().unwrap();
}

#[test]
fn interrupt_before_binding_an_http_source_skips_network_io() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut source =
        HttpRangeSource::new(format!("http://{}/media", listener.local_addr().unwrap()));
    let ticket = ColdOpenCancellation::default();
    ticket.interrupt();
    source.set_cold_open_cancellation(Some(ticket.clone()));
    assert!(source.cancellation().unwrap().is_cancelled());
    assert!(matches!(source.len(), Err(SourceError::Cancelled)));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!ticket.finish());
}

#[test]
fn http_player_cancels_reads_on_seek_stop_reopen_and_close() {
    let server = MediaServer::start();
    let mut config = PlayerConfig::default();
    config.playback.video_decode = VideoDecodePreference::Software;
    let player = Player::new(config);
    let video = player.subscribe_video_frames();
    let audio = player.subscribe_audio_frames();

    for operation in ["seek", "stop", "reopen", "close"] {
        player.open(server.request()).unwrap();
        server.stall_next.store(true, Ordering::Release);
        player.seek(Duration::from_secs(4)).unwrap();
        if player.state() != PlayerState::Playing {
            player.play().unwrap();
        }
        server
            .stalled
            .recv_timeout(WAIT)
            .expect("playback did not issue the controlled HTTP read");
        let started = Instant::now();
        match operation {
            "seek" => {
                // Several queued seeks must not resurrect an older cancelled
                // I/O generation or publish its frames.
                player.seek(Duration::from_secs(2)).unwrap();
                player.seek(Duration::ZERO).unwrap();
            }
            "stop" => player.stop().unwrap(),
            "reopen" => player.open(server.request()).unwrap(),
            _ => player.close().unwrap(),
        }
        server
            .cancelled
            .recv_timeout(CANCEL_LIMIT)
            .expect("playback teardown did not cancel the HTTP read");
        assert!(started.elapsed() < CANCEL_LIMIT);
        eprintln!(
            "Player::{operation} cancelled HTTP in {:.2} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        while video.try_recv().is_ok() {}
        while audio.try_recv().is_ok() {}
        if operation == "stop" || operation == "seek" {
            if operation == "stop" {
                player.play().unwrap();
            }
            let frame = video
                .recv_timeout(WAIT)
                .expect("stop failed to replay after cancelled I/O");
            assert!(frame.pts.unwrap_or_default() < Duration::from_secs(1));
        } else if operation == "reopen" {
            player.play().unwrap();
            video
                .recv_timeout(WAIT)
                .expect("player failed to play after replacing blocked media");
        }
    }
    assert_eq!(player.state(), PlayerState::Closed);
    assert!(player.open(server.request()).is_err());
}

/// Run alone so other tests do not contaminate the process-wide counters:
/// cargo test -p erika --test media_lifecycle memory_probe -- --ignored --nocapture --test-threads=1
#[cfg(target_os = "macos")]
#[test]
#[ignore = "manual process-wide memory experiment"]
fn memory_probe_repeated_http_open_stop_close() {
    fn sample(cycle: usize, stage: &str) {
        let mut malloc = std::mem::MaybeUninit::<libc::malloc_statistics_t>::zeroed();
        let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
        unsafe {
            libc::malloc_zone_statistics(std::ptr::null_mut(), malloc.as_mut_ptr());
            assert_eq!(
                libc::proc_pid_rusage(
                    libc::getpid(),
                    libc::RUSAGE_INFO_V0,
                    usage.as_mut_ptr().cast()
                ),
                0
            );
            let malloc = malloc.assume_init();
            let usage = usage.assume_init();
            eprintln!(
                "MEMORY cycle={cycle} stage={stage} malloc_live={} malloc_allocated={} footprint={}",
                malloc.size_in_use, malloc.size_allocated, usage.ri_phys_footprint
            );
        }
    }
    let server = MediaServer::start();
    let mut config = PlayerConfig::default();
    config.playback.video_decode = VideoDecodePreference::Software;
    sample(0, "baseline");
    for cycle in 1..=12 {
        let player = Player::new(config.clone());
        let video = player.subscribe_video_frames();
        let audio = player.subscribe_audio_frames();
        player.open(server.request()).unwrap();
        player.play().unwrap();
        drop(video.recv_timeout(WAIT).unwrap());
        sample(cycle, "playing");
        player.stop().unwrap();
        thread::sleep(Duration::from_millis(20));
        while video.try_recv().is_ok() {}
        while audio.try_recv().is_ok() {}
        sample(cycle, "stopped");
        player.close().unwrap();
        while video.try_recv().is_ok() {}
        while audio.try_recv().is_ok() {}
        sample(cycle, "closed");
        assert_eq!(player.state(), PlayerState::Closed);
    }
}

/// Real loopback HTTP Range transport, FFmpeg, demux worker and playback worker.
/// The server withholds one 206 body per round until a seek closes the socket;
/// no MediaSource implementation is substituted.
/// Generate a longer fixture to avoid keeping the entire media in FFmpeg's
/// probe/cache buffers:
/// ffmpeg -stream_loop 19 -i crates/erika/testdata/playback/playback-fixture.mkv \
///   -map 0 -c copy -t 160 /tmp/erika-http-seek-fixture.mkv
/// ERIKA_HTTP_SEEK_FIXTURE=/tmp/erika-http-seek-fixture.mkv cargo test -p erika \
///   --test media_lifecycle http_seek_interruption -- --ignored --nocapture
#[test]
#[ignore = "manual HTTP seek stress probe with ERIKA_HTTP_SEEK_FIXTURE"]
fn http_seek_interruption_recovers_and_reaches_natural_eof() {
    use erika::PlayerEvent;

    let fixture = std::env::var_os("ERIKA_HTTP_SEEK_FIXTURE")
        .expect("set ERIKA_HTTP_SEEK_FIXTURE to a generated 160-second fixture");
    let server = MediaServer::with_bytes(std::fs::read(fixture).unwrap());
    let mut config = PlayerConfig::default();
    config.playback.video_decode = VideoDecodePreference::Software;
    let player = Player::new(config);
    let events = player.subscribe();
    let video = player.subscribe_video_frames();
    let audio = player.subscribe_audio_frames();
    player.open(server.request()).unwrap();
    let duration = player.duration().expect("fixture duration");
    assert!(
        duration >= Duration::from_secs(60),
        "use the longer fixture"
    );
    player.play().unwrap();
    let first = video
        .recv_timeout(WAIT)
        .expect("HTTP playback did not start");
    eprintln!("HTTP_SEEK_E2E initial_frame_pts={:?}", first.pts);

    for round in 1..=12 {
        server.stall_next.store(true, Ordering::Release);
        player.seek(duration / 2).unwrap();
        server
            .stalled
            .recv_timeout(WAIT)
            .expect("server did not receive the controlled Range request");
        // Give the real HTTP client time to enter its response-body wait.
        thread::sleep(Duration::from_millis(10));
        let started = Instant::now();
        player.seek(Duration::from_secs(2)).unwrap();
        player.seek(Duration::ZERO).unwrap();
        player.seek(Duration::ZERO).unwrap(); // Duplicate slider release.
        server
            .cancelled
            .recv_timeout(CANCEL_LIMIT)
            .expect("seek did not cancel the in-flight HTTP response");
        let cancel_ms = started.elapsed().as_secs_f64() * 1000.0;
        let generation = player.playback_snapshot().generation;
        let deadline = Instant::now() + WAIT;
        let mut recovered_frames = 0;
        let recovered_pts = loop {
            for event in events.try_iter() {
                if let PlayerEvent::Error(error) = event {
                    panic!("round {round}: playback fatal: {error:?}");
                }
            }
            while audio.try_recv().is_ok() {}
            for frame in video.try_iter() {
                if frame.generation == generation {
                    recovered_frames += 1;
                    if frame.pts.unwrap_or_default() >= Duration::from_millis(200) {
                        break;
                    }
                }
            }
            let position = player.playback_snapshot().media_time();
            if recovered_frames >= 3 && position >= Duration::from_millis(200) {
                break position;
            }
            assert!(
                Instant::now() < deadline,
                "round {round}: seek did not resume"
            );
            thread::sleep(Duration::from_millis(2));
        };
        assert_ne!(player.state(), PlayerState::Error);
        eprintln!(
            "HTTP_SEEK_E2E round={round} cancelled_ms={cancel_ms:.2} generation={generation} recovered_frames={recovered_frames} position={recovered_pts:?}"
        );
    }

    player
        .seek(duration.saturating_sub(Duration::from_secs(3)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut final_frames = 0;
    let mut final_pts = Duration::ZERO;
    loop {
        while audio.try_recv().is_ok() {}
        for frame in video.try_iter() {
            final_frames += 1;
            final_pts = final_pts.max(frame.pts.unwrap_or_default());
        }
        let mut ended = false;
        for event in events.try_iter() {
            match event {
                PlayerEvent::Error(error) => panic!("EOF phase playback fatal: {error:?}"),
                PlayerEvent::Ended => ended = true,
                _ => {}
            }
        }
        if ended {
            assert_eq!(player.state(), PlayerState::Stopped);
            assert!(
                final_pts >= duration.saturating_sub(Duration::from_secs(1)),
                "last frame: {final_pts:?}"
            );
            eprintln!(
                "HTTP_SEEK_E2E natural_eof=true fatal_events=0 final_frames={final_frames} last_pts={final_pts:?}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "playback did not reach natural EOF"
        );
        thread::sleep(Duration::from_millis(2));
    }
    player.close().unwrap();
}
