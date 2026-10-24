use std::collections::BTreeMap;
#[cfg(target_os = "android")]
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
#[cfg(target_os = "android")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
#[cfg(target_os = "android")]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

mod http;

use http::HttpIo;
pub use http::SourceCancellation;

use crate::core::MediaSourceHint;
use crate::trace;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("source read cancelled")]
    Cancelled,
    #[error("io error: {0}")]
    Io(String),
    #[error("http error: {0}")]
    Http(String),
    /// An answer whose position cannot be trusted: a 206 without
    /// Content-Range on a resumed offset, or a served start that disagrees
    /// with the request. Distinct from `Http` so the tail rule of issue #163
    /// (an undelivered final range ends the read as EOF) never normalizes a
    /// server that misanswers ranges -- that must stay an error.
    #[error("http range verification error: {0}")]
    HttpRangeVerification(String),
    #[error("http error: resource changed or cannot be safely validated during Range retry")]
    HttpEntityChanged,
    #[error("unsupported source URI: {0}")]
    Unsupported(String),
    #[error("invalid owned file descriptor URI: {0}")]
    InvalidFileDescriptorUri(String),
}

pub type Result<T> = std::result::Result<T, SourceError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub length: Option<u64>,
}

impl ByteRange {
    pub fn suffix_from(start: u64) -> Self {
        Self {
            start,
            length: None,
        }
    }
}

pub trait MediaSource: Send {
    fn uri(&self) -> &str;
    fn len(&mut self) -> Result<Option<u64>>;
    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>>;

    /// Release expendable read-ahead storage while keeping later reads valid.
    fn release_buffer(&mut self) {}

    /// Return a thread-safe handle that can interrupt a blocked source read.
    fn cancellation(&self) -> Option<SourceCancellation> {
        None
    }
}

#[derive(Debug)]
pub struct LocalFileSource {
    uri: String,
    path: PathBuf,
}

impl LocalFileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let uri = format!("file://{}", path.display());
        Ok(Self { uri, path })
    }
}

impl MediaSource for LocalFileSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        let metadata =
            std::fs::metadata(&self.path).map_err(|error| SourceError::Io(error.to_string()))?;
        Ok(Some(metadata.len()))
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let mut file =
            File::open(&self.path).map_err(|error| SourceError::Io(error.to_string()))?;
        file.seek(SeekFrom::Start(range.start))
            .map_err(|error| SourceError::Io(error.to_string()))?;
        let mut reader: Box<dyn Read> = match range.length {
            Some(length) => Box::new(file.take(length)),
            None => Box::new(file),
        };
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| SourceError::Io(error.to_string()))?;
        Ok(bytes)
    }
}

/// A seekable Android content descriptor owned by the media source.
///
/// The descriptor is closed automatically when this value is dropped. `offset`
/// and `length` expose an `AssetFileDescriptor` slice as a zero-based media file.
#[cfg(target_os = "android")]
#[derive(Debug)]
pub struct OwnedFileDescriptorSource {
    uri: String,
    file: File,
    offset: u64,
    length: Option<u64>,
}

/// Keeps an Android-owned descriptor registered until a synchronous native
/// source call either adopts it or returns an error.
///
/// Dropping the registration closes the descriptor when no `MediaSource`
/// consumed it. This closes the ownership gap between JNI validation and the
/// point where playback constructs `OwnedFileDescriptorSource`.
#[cfg(target_os = "android")]
#[derive(Debug)]
pub struct AndroidOwnedFdRegistration {
    fd: RawFd,
}

#[cfg(target_os = "android")]
impl Drop for AndroidOwnedFdRegistration {
    fn drop(&mut self) {
        if let Ok(mut registry) = android_owned_fd_registry().lock() {
            let _ = registry.remove(&self.fd);
        }
    }
}

/// Registers a descriptor transferred by the Android host for one synchronous
/// native invocation. `source_from_uri` consumes the registered `File`; if the
/// invocation fails before that boundary, the returned guard closes it.
#[cfg(target_os = "android")]
pub fn register_android_owned_fd(file: File) -> Result<AndroidOwnedFdRegistration> {
    let fd = file.as_raw_fd();
    if fd < 0 {
        return Err(SourceError::InvalidFileDescriptorUri(format!(
            "negative descriptor {fd}"
        )));
    }
    let mut registry = android_owned_fd_registry()
        .lock()
        .map_err(|_| SourceError::Io("Android owned-fd registry mutex poisoned".to_string()))?;
    if registry.contains_key(&fd) {
        // The existing entry already owns this raw descriptor. Closing a second
        // File wrapper here would invalidate that entry, so discard only the
        // duplicate wrapper and preserve the original ownership.
        std::mem::forget(file);
        return Err(SourceError::InvalidFileDescriptorUri(format!(
            "descriptor {fd} is already awaiting source adoption"
        )));
    }
    registry.insert(fd, file);
    Ok(AndroidOwnedFdRegistration { fd })
}

#[cfg(target_os = "android")]
fn android_owned_fd_registry() -> &'static Mutex<HashMap<RawFd, File>> {
    static REGISTRY: OnceLock<Mutex<HashMap<RawFd, File>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(target_os = "android")]
fn take_registered_android_owned_fd(fd: RawFd) -> Option<File> {
    android_owned_fd_registry().lock().ok()?.remove(&fd)
}

#[cfg(target_os = "android")]
impl OwnedFileDescriptorSource {
    /// Takes ownership of `fd`; callers must not close or reuse it afterwards.
    ///
    /// # Safety
    ///
    /// `fd` must be a valid, uniquely-owned, seekable descriptor.
    pub unsafe fn from_owned_fd(
        fd: RawFd,
        offset: u64,
        length: Option<u64>,
        uri: impl Into<String>,
    ) -> Result<Self> {
        if fd < 0 {
            return Err(SourceError::InvalidFileDescriptorUri(format!(
                "negative descriptor {fd}"
            )));
        }
        // SAFETY: ownership is transferred by the function contract.
        let file = unsafe { File::from_raw_fd(fd) };
        Self::from_owned_file(file, offset, length, uri.into())
    }

    fn from_owned_file(file: File, offset: u64, length: Option<u64>, uri: String) -> Result<Self> {
        let metadata = file
            .metadata()
            .map_err(|error| SourceError::Io(error.to_string()))?;
        let length = length.or_else(|| metadata.len().checked_sub(offset));
        Ok(Self {
            uri,
            file,
            offset,
            length,
        })
    }

    unsafe fn open_uri(uri: &str) -> Result<Self> {
        let fd = parse_owned_fd(uri)?;
        // Safe URI dispatch may only consume descriptors registered by the JNI
        // transferred-fd contract. Never adopt a registry miss by raw number:
        // that could seize or double-close an unrelated process descriptor.
        // Direct native callers with unique ownership must use `from_owned_fd`.
        let file = take_registered_android_owned_fd(fd).ok_or_else(|| {
            SourceError::InvalidFileDescriptorUri(format!(
                "{uri} (descriptor was not explicitly transferred)"
            ))
        })?;
        let spec = parse_fd_uri(uri)?;
        Self::from_owned_file(file, spec.offset, spec.length, uri.to_string())
    }
}

#[cfg(target_os = "android")]
impl MediaSource for OwnedFileDescriptorSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        Ok(self.length)
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let length = match self.length {
            Some(total) if range.start >= total => return Ok(Vec::new()),
            Some(total) => Some(
                range
                    .length
                    .unwrap_or_else(|| total.saturating_sub(range.start))
                    .min(total.saturating_sub(range.start)),
            ),
            None => range.length,
        };
        let absolute_start = self.offset.checked_add(range.start).ok_or_else(|| {
            SourceError::Io("owned descriptor seek offset overflowed u64".to_string())
        })?;
        self.file
            .seek(SeekFrom::Start(absolute_start))
            .map_err(|error| SourceError::Io(error.to_string()))?;
        let mut bytes = Vec::new();
        match length {
            Some(length) => (&mut self.file)
                .take(length)
                .read_to_end(&mut bytes)
                .map_err(|error| SourceError::Io(error.to_string()))?,
            None => self
                .file
                .read_to_end(&mut bytes)
                .map_err(|error| SourceError::Io(error.to_string()))?,
        };
        Ok(bytes)
    }
}

#[cfg(any(target_os = "android", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnedFdUri {
    fd: i32,
    offset: u64,
    length: Option<u64>,
}

#[cfg(any(target_os = "android", test))]
fn parse_fd_uri(uri: &str) -> Result<OwnedFdUri> {
    let body = uri
        .strip_prefix("fd://")
        .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))?;
    let (fd, query) = body.split_once('?').unwrap_or((body, ""));
    let fd = parse_owned_fd_value(fd, uri)?;
    let mut offset = None;
    let mut length = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))?;
        match key {
            "offset" => {
                if offset.is_some() {
                    return Err(SourceError::InvalidFileDescriptorUri(uri.to_string()));
                }
                offset = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| SourceError::InvalidFileDescriptorUri(uri.to_string()))?,
                );
            }
            "length" => {
                if length.is_some() {
                    return Err(SourceError::InvalidFileDescriptorUri(uri.to_string()));
                }
                length = Some(if value.is_empty() || value == "-1" {
                    None
                } else {
                    Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| SourceError::InvalidFileDescriptorUri(uri.to_string()))?,
                    )
                });
            }
            // Display names/URIs may be appended by the Android host for diagnostics.
            "name" | "display_uri" => {}
            _ => return Err(SourceError::InvalidFileDescriptorUri(uri.to_string())),
        }
    }
    Ok(OwnedFdUri {
        fd,
        offset: offset.unwrap_or(0),
        length: length.flatten(),
    })
}

#[cfg(target_os = "android")]
fn parse_owned_fd(uri: &str) -> Result<i32> {
    let body = uri
        .strip_prefix("fd://")
        .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))?;
    let fd = body.split_once(['?', '/', '#']).map_or(body, |(fd, _)| fd);
    parse_owned_fd_value(fd, uri)
}

#[cfg(any(target_os = "android", test))]
fn parse_owned_fd_value(value: &str, uri: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))
}

pub struct HttpRangeSource {
    uri: String,
    agent: HttpIo,
    http_headers: Vec<(String, String)>,
    content_length: Option<u64>,
    /// Set once a completed probe answered `None` (chunked / connection-close
    /// origin). Without it every piece fetch re-issues the HEAD + range probe
    /// because `content_length` stays `None`.
    length_probed: bool,
    cache_start: u64,
    cache_bytes: Vec<u8>,
    /// Cache depth target: how much *unread* data to keep buffered ahead of the
    /// reader. Deeper windows ride out longer origin hiccups; the cost is
    /// memory. This is deliberately not the on-the-wire request size -- see
    /// `HTTP_REQUEST_MAX_BYTES`.
    read_ahead_bytes: u64,
    /// Largest body a single request may ask for: `read_ahead_bytes` capped by
    /// `HTTP_REQUEST_MAX_BYTES`. The window is filled by successive requests of
    /// at most this size instead of one request for the whole window.
    request_bytes: u64,
    /// How much already-played data stays in the cache (the rewind budget).
    /// `HTTP_CACHE_RETAIN_BYTES` is the default; hosts sizing by media bitrate
    /// override it through the open options (`http_back_buffer_bytes`).
    cache_retain_bytes: u64,
    /// One persistent prefetch stream (`bytes=anchor-`), delivering stripes in
    /// file order. Multiple open-ended streams would download overlapping
    /// tails, even if their handoffs were assigned different stripe indices.
    /// When the window is full the worker stops reading its socket, letting
    /// TCP flow control throttle the origin without per-piece requests.
    streams: Option<StreamSession>,
    /// Next stripe index the reader expects to append (`streams` frontier).
    stream_frontier: u64,
    /// The consumer position the window budget is measured against, updated
    /// by every read.
    stream_reader_end: u64,
    /// Consecutive failed background workers. A failed worker is never fatal
    /// (the read path falls back to a synchronous fetch), but retrying on
    /// every read would hammer a sick origin, so spawning parks after a few
    /// and resumes once a synchronous fetch proves the origin is alive.
    prefetch_failures: u32,
    /// Sticky across worker replacement and buffer release: a failed entity
    /// check must never become an unvalidated synchronous fallback.
    entity_changed: Arc<AtomicBool>,
}

/// Handoff channel between the stream workers and the reader thread.
///
/// Workers never touch `cache_bytes` (the reader thread owns it); they hand
/// completed stripes over under this lock and the reader appends them on its
/// own schedule. `epoch` is the re-anchoring counter: a seek bumps it and
/// workers of older epochs exit without touching anything.
struct StreamShared {
    inner: Mutex<StreamInner>,
    signal: Condvar,
    entity_changed: Arc<AtomicBool>,
}

struct StreamSession {
    shared: Arc<StreamShared>,
    io: HttpIo,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        {
            let mut inner = lock_stream(&self.shared);
            inner.stopped = true;
            inner.epoch = inner.epoch.wrapping_add(1);
        }
        self.io.cancel();
        self.shared.signal.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct StreamInner {
    epoch: u64,
    /// Completed stripes waiting for the reader, by stripe index.
    pending: BTreeMap<u64, StripeHandoff>,
    /// The worker finished (EOF or failure); no further stripes will arrive.
    worker_done: bool,
    /// Whether completion was a failure (counted by the reader once).
    worker_failed: bool,
    failure_acked: bool,
    /// Body bytes received so far, for the reader's stall detection.
    progress_bytes: u64,
    last_progress: Instant,
    /// Absolute byte position past which no worker may start another stripe:
    /// the reader's position plus the configured window, published by every
    /// read. Workers self-throttle against this rather than a "window full"
    /// flag, because a flag is only refreshed when the reader reads -- a reader
    /// parked on a full packet queue would otherwise leave the workers
    /// unbounded, and a fast origin then prefetches the whole resource.
    window_end: u64,
    /// Set when the source is dropped: every worker exits.
    stopped: bool,
}

struct StripeHandoff {
    start: u64,
    bytes: Vec<u8>,
}

/// Bytes fetched for one HTTP range request plus the resource total reported
/// by the server (`Content-Range` on 206, `Content-Length` on a whole-file
/// 200), or measured at EOF of a whole-file 200. The total lets callers
/// backfill `content_length` when HEAD or length headers are unavailable.
struct HttpRangeResponse {
    bytes: Vec<u8>,
    total_length: Option<u64>,
}

impl HttpRangeSource {
    const DEFAULT_READ_AHEAD_BYTES: u64 = 2 * 1024 * 1024;

    pub fn new(uri: impl Into<String>) -> Self {
        Self::with_http_headers(uri, Vec::new())
    }

    pub fn with_http_headers(uri: impl Into<String>, http_headers: Vec<(String, String)>) -> Self {
        Self::with_http_headers_and_window(uri, http_headers, None, None)
    }

    /// `read_ahead`: explicit read-ahead window in bytes; `None` (or `Some(0)`)
    /// falls back to the `ERIKA_HTTP_READAHEAD_BYTES` env override, then the
    /// 2 MiB engine default.
    pub fn with_http_headers_and_read_ahead(
        uri: impl Into<String>,
        http_headers: Vec<(String, String)>,
        read_ahead: Option<u64>,
    ) -> Self {
        Self::with_http_headers_and_window(uri, http_headers, read_ahead, None)
    }

    /// `back_buffer`: how much already-played data the cache retains behind
    /// the reader, i.e. the rewind budget; `None` (or `Some(0)`) uses the
    /// 16 MiB engine default. High-bitrate sources need more -- a -10 s skip
    /// at 71 Mbps covers ~89 MB, which a fixed 16 MiB tail cannot hold.
    pub fn with_http_headers_and_window(
        uri: impl Into<String>,
        http_headers: Vec<(String, String)>,
        read_ahead: Option<u64>,
        back_buffer: Option<u64>,
    ) -> Self {
        let agent = HttpIo::new();
        Self {
            uri: uri.into(),
            agent,
            http_headers,
            content_length: None,
            length_probed: false,
            cache_start: 0,
            cache_bytes: Vec::new(),
            read_ahead_bytes: read_ahead
                .filter(|bytes| *bytes > 0)
                .unwrap_or_else(http_read_ahead_bytes)
                .min(HTTP_READ_AHEAD_MAX_BYTES),
            request_bytes: read_ahead
                .filter(|bytes| *bytes > 0)
                .unwrap_or_else(http_read_ahead_bytes)
                .min(HTTP_REQUEST_MAX_BYTES),
            cache_retain_bytes: back_buffer
                .filter(|bytes| *bytes > 0)
                .unwrap_or(HTTP_CACHE_RETAIN_BYTES)
                .min(HTTP_READ_AHEAD_MAX_BYTES),
            streams: None,
            stream_frontier: 0,
            stream_reader_end: 0,
            prefetch_failures: 0,
            entity_changed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn cache_end(&self) -> u64 {
        self.cache_start
            .saturating_add(self.cache_bytes.len() as u64)
    }

    fn cached_slice(&self, range: ByteRange) -> Option<Vec<u8>> {
        let length = range.length?;
        let end = range.start.checked_add(length)?;
        if range.start < self.cache_start || end > self.cache_end() {
            return None;
        }
        let start_index = usize::try_from(range.start - self.cache_start).ok()?;
        let length = usize::try_from(length).ok()?;
        let end_index = start_index.checked_add(length)?;
        Some(self.cache_bytes[start_index..end_index].to_vec())
    }

    /// Cut the part of the cache that is further than `HTTP_CACHE_RETAIN_BYTES`
    /// behind the reader.
    ///
    /// The retained tail is what turns a small rewind into a cache hit instead
    /// of a fresh download. Trimming only once the tail is
    /// `HTTP_CACHE_TRIM_SLACK` over budget keeps the O(n) buffer move rare
    /// (once per few MiB of playback) instead of once per read.
    fn trim_cache(&mut self, range: ByteRange) {
        if range.length.is_none() {
            return;
        }
        // Measured from the *start* of the current read, never past it: the tail
        // is what a rewind can hit, but nothing the current read needs may be
        // dropped (a read larger than the retention budget still has to be
        // served from the cache it started in).
        let retain_floor = range.start.saturating_sub(self.cache_retain_bytes);
        let drop = retain_floor.saturating_sub(self.cache_start);
        if drop < HTTP_CACHE_TRIM_SLACK {
            return;
        }
        let drop = drop.min(self.cache_bytes.len() as u64) as usize;
        self.cache_bytes.drain(..drop);
        self.cache_start = self.cache_start.saturating_add(drop as u64);
        http_trace_log(format!(
            "{{\"event\":\"http_cache_trim\",\"dropped\":{},\"cache_start\":{},\"cache_end\":{}}}",
            drop,
            self.cache_start,
            self.cache_end(),
        ));
    }

    fn fetch_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        self.check_entity()?;
        let response = fetch_http_range(
            &self.agent,
            &self.uri,
            &self.http_headers,
            range,
            "http_range",
        )
        .inspect_err(|error| {
            if matches!(error, SourceError::HttpEntityChanged) {
                self.entity_changed.store(true, Ordering::Release);
            }
        })?;
        self.check_entity()?;
        if self.content_length.is_none() {
            self.content_length = response.total_length;
        }
        // A synchronous fetch that worked proves the origin is alive: let the
        // background prefetch chain resume.
        self.prefetch_failures = 0;
        Ok(response.bytes)
    }

    fn check_entity(&self) -> Result<()> {
        if self.entity_changed.load(Ordering::Acquire) {
            return Err(SourceError::HttpEntityChanged);
        }
        Ok(())
    }

    fn fetch_length(&mut self, range: ByteRange) -> Result<Option<u64>> {
        Ok(match range.length {
            Some(length) => {
                // At least the caller's request with a capped piece of look-ahead,
                // and never more than one request body may carry. A caller asking
                // for more than the cap gets a short read (the read loops re-issue
                // the rest), which keeps "no request body exceeds the cap" an
                // invariant instead of a property of today's callers.
                let mut wanted = length.max(self.request_bytes).min(HTTP_REQUEST_MAX_BYTES);
                if let Some(total) = self.content_length.or_else(|| self.len().ok().flatten()) {
                    if range.start >= total {
                        return Ok(Some(0));
                    }
                    // Never ask past the end of the resource either.
                    wanted = wanted.min(total.saturating_sub(range.start));
                }
                Some(wanted)
            }
            None => None,
        })
    }

    /// Whether a fetch window `[start, start + length)` runs to the end of the
    /// resource, i.e. `fetch_length` truncated it at `content_length`. Windows
    /// that reach the total are boundary traffic: players skip into the
    /// closing moments of a file and read to its natural end, and origins
    /// mishandle the final range (a 206 that promises the tail and delivers
    /// nothing). An empty or failed answer there ends the read instead of
    /// failing the session (issue #163); anywhere else an uncovered read
    /// stays an error.
    fn window_ends_at_total(&self, start: u64, length: u64) -> bool {
        self.content_length
            .is_some_and(|total| start.saturating_add(length) >= total)
    }

    /// Some(total) while an empty read at `offset` is a fetch failure rather
    /// than the resource's end: the resource is known to hold more than the
    /// tail slack beyond it. A fetch whose request window was truncated by
    /// the resource total starts at most `HTTP_REQUEST_MAX_BYTES` short of
    /// it, so the slack mirrors the largest window a tail flush can leave
    /// uncovered and this guard agrees with the fetch-side
    /// `window_ends_at_total` rule.
    fn uncovered_read(&self, offset: u64) -> Option<u64> {
        let total = self.content_length?;
        (offset < total && total - offset > HTTP_TAIL_EOF_SLACK_BYTES).then_some(total)
    }

    /// Append stripes the workers completed since the last read. Called
    /// before the hit checks so they see everything that has already arrived.
    /// A sync fetch that ran past the frontier (the fallback path) may have
    /// covered a stripe already; such stripes are skipped or trimmed instead
    /// of being spliced twice.
    fn drain_stripes(&mut self) {
        let Some(shared) = self
            .streams
            .as_ref()
            .map(|session| Arc::clone(&session.shared))
        else {
            return;
        };
        loop {
            let handoff = {
                let mut inner = lock_stream(&shared);
                match inner.pending.remove(&self.stream_frontier) {
                    Some(handoff) => {
                        self.stream_frontier += 1;
                        handoff
                    }
                    None => break,
                }
            };
            if handoff.start > self.cache_end() {
                // Never disguise a hole as contiguous media. Retire the stream
                // and let the synchronous path fetch from the real cache end.
                self.kill_streams();
                self.prefetch_failures = self.prefetch_failures.saturating_add(1);
                return;
            }
            if handoff.start == self.cache_end() {
                self.cache_bytes.extend_from_slice(&handoff.bytes);
            } else if let Ok(skip) = usize::try_from(self.cache_end() - handoff.start)
                && skip < handoff.bytes.len()
            {
                self.cache_bytes.extend_from_slice(&handoff.bytes[skip..]);
            }
            self.update_stream_backpressure();
        }
    }

    /// Publish the window boundary the workers self-throttle against: the
    /// reader's position plus the configured read-ahead. The budget is measured
    /// from the consumer position, never from `cache_start`: the retained tail
    /// behind the reader must not count against the window or the streams would
    /// pause forever.
    ///
    /// This is the reader's only lever over the workers, and it must be an
    /// absolute position rather than a "window full" flag: a flag is a snapshot
    /// the reader refreshes only when it reads, so a reader parked on a full
    /// packet queue would leave the workers unbounded and a fast origin would
    /// prefetch the whole resource.
    fn update_stream_backpressure(&mut self) {
        let Some(shared) = self
            .streams
            .as_ref()
            .map(|session| Arc::clone(&session.shared))
        else {
            return;
        };
        let window_end = self.stream_reader_end.saturating_add(self.read_ahead_bytes);
        let mut inner = lock_stream(&shared);
        if inner.window_end != window_end {
            inner.window_end = window_end;
            shared.signal.notify_all();
        }
    }

    /// Close every worker (a seek re-anchored the window, the resource ended,
    /// or the source is going away). Dropping the session cancels I/O and
    /// wakes the worker, then joins it before releasing the handoff storage.
    fn kill_streams(&mut self) {
        self.stream_frontier = 0;
        self.streams.take();
    }

    /// Spawn the persistent stream at `cache_end`, or re-anchor it after a
    /// failure while bytes are still missing ahead.
    ///
    /// The open-ended GET pays the origin's request/seek cost once. A single
    /// stream covers the window without downloading any byte twice, handing
    /// stripes to the reader and pausing between stripes when the window fills.
    fn ensure_streams(&mut self) {
        if self.entity_changed.load(Ordering::Acquire) {
            self.kill_streams();
            return;
        }
        if self.cache_bytes.is_empty() {
            return;
        }
        let Some(total) = self.content_length else {
            return;
        };
        if self.cache_end() >= total {
            self.kill_streams();
            return;
        }
        if let Some(shared) = self
            .streams
            .as_ref()
            .map(|session| Arc::clone(&session.shared))
        {
            let (done, newly_failed) = {
                let mut inner = lock_stream(&shared);
                let newly_failed = inner.worker_done && inner.worker_failed && !inner.failure_acked;
                if newly_failed {
                    inner.failure_acked = true;
                }
                (inner.worker_done, u32::from(newly_failed))
            };
            self.prefetch_failures = self.prefetch_failures.saturating_add(newly_failed);
            self.update_stream_backpressure();
            if !done {
                return;
            }
            // Completion may have raced the read's earlier drain. Preserve the
            // final handoffs before retiring the worker or reopening its tail.
            self.drain_stripes();
            self.kill_streams();
            if self.cache_end() >= total {
                return;
            }
        }
        if self.entity_changed.load(Ordering::Acquire)
            || self.prefetch_failures >= HTTP_PREFETCH_MAX_FAILURES
        {
            return;
        }
        let anchor = self.cache_end();
        let shared = Arc::new(StreamShared {
            inner: Mutex::new(StreamInner {
                epoch: 1,
                pending: BTreeMap::new(),
                worker_done: false,
                worker_failed: false,
                failure_acked: false,
                progress_bytes: 0,
                last_progress: Instant::now(),
                window_end: anchor.saturating_add(self.read_ahead_bytes),
                stopped: false,
            }),
            signal: Condvar::new(),
            entity_changed: Arc::clone(&self.entity_changed),
        });
        let epoch = lock_stream(&shared).epoch;
        let worker_shared = Arc::clone(&shared);
        let io = self.agent.child();
        let worker_io = io.clone();
        let uri = self.uri.clone();
        let http_headers = self.http_headers.clone();
        let worker = thread::Builder::new()
            .name("erika-http-stream".to_string())
            .spawn(move || {
                stream_worker_main(
                    Arc::clone(&worker_shared),
                    worker_io,
                    uri,
                    http_headers,
                    epoch,
                    anchor,
                    total,
                );
                // Every exit, including cancellation, must wake a foreground
                // reader waiting for bytes that will no longer arrive.
                lock_stream(&worker_shared).worker_done = true;
                worker_shared.signal.notify_all();
            })
            .expect("spawn HTTP stream worker");
        self.streams = Some(StreamSession {
            shared,
            io,
            worker: Some(worker),
        });
        self.stream_frontier = 0;
        self.stream_reader_end = self.cache_end();
    }

    /// Wait until the streams have delivered coverage through `end`.
    ///
    /// Returns true when `cache_end >= end` (the caller re-checks its slice),
    /// false when the workers are dead, stalled, or the wait budget ran out
    /// and the caller must fall back to the synchronous path. Progress resets
    /// the stall clock: a slow origin is waited out, a dead one is not.
    fn wait_for_stream_coverage(&mut self, end: u64) -> bool {
        let Some(shared) = self
            .streams
            .as_ref()
            .map(|session| Arc::clone(&session.shared))
        else {
            return false;
        };
        let started = Instant::now();
        // AVIO's final read normally extends past EOF. All existing bytes are
        // sufficient; waiting for the rest of that buffer can never succeed.
        let end = self.content_length.map_or(end, |total| end.min(total));
        loop {
            self.drain_stripes();
            if self.streams.is_none() {
                return false;
            }
            self.update_stream_backpressure();
            if self.cache_end() >= end {
                return true;
            }
            let inner = lock_stream(&shared);
            if self.agent.is_cancelled()
                || inner.stopped
                || inner.worker_done
                || inner.last_progress.elapsed() >= HTTP_STREAM_STALL
                || started.elapsed() >= HTTP_STREAM_WAIT_BUDGET
            {
                return false;
            }
            let _ = shared
                .signal
                .wait_timeout(inner, Duration::from_millis(500));
        }
    }

    /// Make the cache cover `range`, downloading only what is missing.
    fn fetch_missing(&mut self, range: ByteRange) -> Result<()> {
        let Some(length) = range.length else {
            // Open-ended read (whole sidecar files): stream to EOF in capped
            // pieces. The previous shape asked for the entire tail in one
            // request, which meets the same body deadline on a slow link.
            return self.stream_to_eof(range.start);
        };
        let end = range.start.saturating_add(length);
        if self.cache_bytes.is_empty() {
            // Nothing is buffered, so there is no anchor to continue from:
            // start the window at the read. A server ignoring Range will replay
            // the whole file; the fetch discards its prefix before caching.
            return self.reanchor_window(range);
        }
        if range.start < self.cache_start {
            // A rewind past the retained tail: nothing in the cache sits before
            // the read. Re-anchor the window here -- this is the one path that
            // still discards the buffered future, and it matches what every
            // player does once a seek lands past its back buffer.
            return self.reanchor_window(range);
        }
        if range.start > self.cache_end().saturating_add(HTTP_REQUEST_MAX_BYTES) {
            // A forward jump beyond the window: downloading the gap would cost
            // more than starting over at the read.
            return self.reanchor_window(range);
        }
        let mut pieces = 0;
        while self.cache_end() < end {
            if pieces >= HTTP_FETCH_MAX_PIECES_PER_READ {
                // Walking away here would make the caller see a short (or empty,
                // i.e. EOF) read for bytes that exist, so fail loudly instead.
                return Err(SourceError::Http(format!(
                    "origin answered {pieces} short pieces without covering bytes {}..{end}",
                    range.start,
                )));
            }
            pieces += 1;
            let start = self.cache_end();
            let Some(request_length) = self.fetch_length(ByteRange {
                start,
                length: Some(end - start),
            })?
            else {
                break;
            };
            if request_length == 0 {
                break;
            }
            let fetched = match self.fetch_range(ByteRange {
                start,
                length: Some(request_length),
            }) {
                Ok(fetched) => fetched,
                Err(error @ SourceError::Http(_))
                    if self.window_ends_at_total(start, request_length) =>
                {
                    // The final range of the resource is boundary traffic: a
                    // 206 that promises the tail and delivers nothing, a proxy
                    // dropping the last body, a status the origin only serves
                    // for the edge. Whatever the delivery failure, the fetch
                    // already retried, and a window that runs to the resource
                    // total ends the read instead of the session (issue #163).
                    // Unverifiable answers (HttpRangeVerification) and entity
                    // changes stay errors: those must fail loudly.
                    http_trace_log(format!(
                        "{{\"event\":\"http_tail_eof\",\"start\":{},\"length\":{},\"error\":\"{}\"}}",
                        start,
                        request_length,
                        json_escape(&error.to_string()),
                    ));
                    break;
                }
                Err(error) => return Err(error),
            };
            if fetched.is_empty() {
                // An empty answer when bytes are known to exist is not EOF,
                // unless the piece window runs to the end of the resource and
                // the empty answer marks the boundary (issue #163). Checked
                // after the fetch: a successful response may be the first to
                // name the total for an origin whose length probe failed.
                if self.content_length.is_some_and(|total| start < total)
                    && !self.window_ends_at_total(start, request_length)
                {
                    return Err(SourceError::Http(format!(
                        "origin answered an empty body for bytes {start}.. although the resource is larger"
                    )));
                }
                break;
            }
            self.cache_bytes.extend_from_slice(&fetched);
            // A short answer (a server that caps response sizes, or EOF before
            // the read is covered) just means the loop owes another piece.
        }
        Ok(())
    }

    /// Start a fresh window at the read position, discarding what the cache held
    /// (including any stream workers anchored to the old window).
    fn reanchor_window(&mut self, range: ByteRange) -> Result<()> {
        self.kill_streams();
        let Some(request_length) = self.fetch_length(range)? else {
            return Ok(());
        };
        if request_length == 0 {
            self.cache_start = range.start;
            self.cache_bytes.clear();
            return Ok(());
        }
        let fetched = match self.fetch_range(ByteRange {
            start: range.start,
            length: Some(request_length),
        }) {
            Ok(fetched) => fetched,
            Err(error @ SourceError::Http(_))
                if self.window_ends_at_total(range.start, request_length) =>
            {
                // Same boundary rule as `fetch_missing`: a window that runs to
                // the resource total may meet an origin that cannot deliver
                // the final bytes, and that ends the read (issue #163).
                http_trace_log(format!(
                    "{{\"event\":\"http_tail_eof\",\"start\":{},\"length\":{},\"error\":\"{}\"}}",
                    range.start,
                    request_length,
                    json_escape(&error.to_string()),
                ));
                Vec::new()
            }
            Err(error) => return Err(error),
        };
        self.cache_start = range.start;
        self.cache_bytes = fetched;
        if self.cache_bytes.is_empty()
            && self.content_length.is_some_and(|total| range.start < total)
            && !self.window_ends_at_total(range.start, request_length)
        {
            // Checked after the fetch: a successful response may be the first
            // to name the total for an origin whose length probe failed.
            return Err(SourceError::Http(format!(
                "origin answered an empty body for bytes {}.. although the resource is larger",
                range.start,
            )));
        }
        http_trace_log(format!(
            "{{\"event\":\"http_cache_reanchored\",\"start\":{},\"bytes\":{}}}",
            range.start,
            self.cache_bytes.len(),
        ));
        Ok(())
    }

    /// Read from `start` to EOF in capped pieces.
    ///
    /// The loop ends at the resource total when it is known, at a short answer
    /// (EOF) otherwise, and at `HTTP_STREAM_TO_EOF_BYTE_LIMIT` as a hard stop so
    /// an origin that keeps answering full pieces cannot grow the cache without
    /// bound. It deliberately has no attempt cap: a sidecar larger than a few
    /// pieces must not be truncated (the old shape read the entire tail in one
    /// request, which is unbounded in the other direction). Hitting the hard
    /// stop is reported as an error, never a silent short read.
    fn stream_to_eof(&mut self, start: u64) -> Result<()> {
        self.kill_streams();
        self.cache_start = start;
        self.cache_bytes.clear();
        loop {
            let chunk_start = self.cache_end();
            if let Some(total) = self.content_length
                && chunk_start >= total
            {
                break;
            }
            // EOF first so a read that has reached the declared total is a
            // clean end even when that total exceeds the limit (a final full
            // piece can land the cache exactly on such a total). With the EOF
            // check ahead, the hard stop only rejects a read that still owes
            // bytes beyond the limit: an origin with no known length that
            // keeps answering full pieces, or a resource genuinely larger
            // than the limit.
            if self.cache_bytes.len() as u64 > HTTP_STREAM_TO_EOF_BYTE_LIMIT {
                return Err(SourceError::Http(format!(
                    "open-ended read exceeded the {HTTP_STREAM_TO_EOF_BYTE_LIMIT}-byte limit at {}",
                    self.cache_end(),
                )));
            }
            let fetched = self.fetch_range(ByteRange {
                start: chunk_start,
                length: Some(HTTP_REQUEST_MAX_BYTES),
            })?;
            if fetched.is_empty() {
                break;
            }
            let short = (fetched.len() as u64) < HTTP_REQUEST_MAX_BYTES;
            self.cache_bytes.extend_from_slice(&fetched);
            if short {
                break;
            }
        }
        Ok(())
    }
}

fn lock_stream(shared: &StreamShared) -> MutexGuard<'_, StreamInner> {
    shared
        .inner
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn hand_off_stripe(shared: &StreamShared, epoch: u64, index: u64, start: u64, bytes: Vec<u8>) {
    let mut inner = lock_stream(shared);
    if inner.stopped || inner.epoch != epoch || bytes.is_empty() {
        return;
    }
    inner.pending.insert(index, StripeHandoff { start, bytes });
    shared.signal.notify_all();
}

fn mark_worker_done(shared: &StreamShared, failed: bool) {
    let mut inner = lock_stream(shared);
    inner.worker_done = true;
    inner.worker_failed = failed;
    shared.signal.notify_all();
    drop(inner);
    http_trace_log(format!(
        "{{\"event\":\"http_stream_worker_done\",\"worker\":0,\"failed\":{failed}}}"
    ));
}

enum StreamOpened {
    Body {
        response: reqwest::Response,
        skip_bytes: u64,
    },
    Eof,
}

/// Opens a worker's persistent GET: open-ended (`bytes=offset-`), so the
/// origin seeks once and streams. `416` means the offset sits at/past EOF.
/// The timeouts are per-worker and generous -- a background stream must not
/// be killed by the 15 s response deadline that shapes the synchronous path.
fn open_stream_response(
    io: &HttpIo,
    uri: &str,
    http_headers: &[(String, String)],
    offset: u64,
    validator: Option<String>,
) -> Result<StreamOpened> {
    validate_resume_validator(validator.as_deref())?;
    let range = ByteRange {
        start: offset,
        length: None,
    };
    let mut request = io
        .client()
        .get(uri)
        .header("Range", http_range_header(range));
    for (name, value) in http_headers {
        request = request.header(name, value);
    }
    if let Some(validator) = validator.as_deref() {
        request = request.header("If-Range", validator);
    }
    let response = io.run(async move {
        tokio::time::timeout(HTTP_STREAM_RESPONSE_TIMEOUT, request.send())
            .await
            .map_err(|_| SourceError::Http("HTTP stream response timed out".to_string()))?
            .map_err(|error| SourceError::Http(error.without_url().to_string()))
    })?;
    let status = response.status().as_u16();
    match status {
        206 => {
            // A stream that answers from anywhere but the requested offset
            // would be spliced onto the cache as silent corruption, exactly
            // like the synchronous path's resume check.
            let content_range = response
                .headers()
                .get("content-range")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if let Some(start) = content_range.as_deref().and_then(parse_content_range_start) {
                if start != offset {
                    return Err(SourceError::HttpRangeVerification(format!(
                        "stream served from {start}, expected {offset}"
                    )));
                }
            } else if offset > 0 {
                // Without Content-Range a resumed stream cannot prove it is
                // answering from the requested offset.
                return Err(SourceError::HttpRangeVerification(
                    "stream served 206 without Content-Range; cannot verify the resumed offset"
                        .to_string(),
                ));
            }
            Ok(StreamOpened::Body {
                response,
                skip_bytes: 0,
            })
        }
        416 => Ok(StreamOpened::Eof),
        200 => {
            validate_full_response_entity(&response, validator.as_deref())?;
            Ok(StreamOpened::Body {
                response,
                skip_bytes: offset,
            })
        }
        other => Err(SourceError::Http(format!(
            "unexpected HTTP status {other} for stream request"
        ))),
    }
}

/// Gate each stripe, including multiple stripes received in one body chunk.
fn wait_for_stream_window(
    shared: &StreamShared,
    io: &HttpIo,
    epoch: u64,
    offset: u64,
    stripe_started: bool,
) -> bool {
    loop {
        let inner = lock_stream(shared);
        if inner.stopped || inner.epoch != epoch || io.is_cancelled() {
            return false;
        }
        // Finish partial stripes so a small window cannot strand reader data.
        if stripe_started || offset < inner.window_end {
            return true;
        }
        let _ = shared
            .signal
            .wait_timeout(inner, Duration::from_millis(200));
    }
}

/// One persistent open-ended GET, delivering fixed-size stripes. Seeks,
/// cancellation and backpressure are checked before starting each stripe.
/// Body errors reconnect at the reached offset with the entity validator.
fn stream_worker_main(
    shared: Arc<StreamShared>,
    io: HttpIo,
    uri: String,
    http_headers: Vec<(String, String)>,
    epoch: u64,
    start: u64,
    total: u64,
) {
    let mut offset = start;
    let mut stripe_index = 0;
    let mut stripe_start = start;
    let mut stripe: Vec<u8> = Vec::new();
    let mut stripe_opened: Option<Instant> = None;
    let mut validator: Option<String> = None;
    let mut live: Option<reqwest::Response> = None;
    let mut skip_remaining = 0;
    let mut response_start = start;
    let mut resumes_since_progress: u32 = 0;

    loop {
        // Finish a partial final stripe without needing another socket read or
        // waiting for the read-ahead window to move beyond the known file end.
        if offset >= total {
            hand_off_stripe(&shared, epoch, stripe_index, stripe_start, stripe);
            mark_worker_done(&shared, false);
            return;
        }
        if !wait_for_stream_window(&shared, &io, epoch, offset, !stripe.is_empty()) {
            return;
        }
        if live.is_none() {
            match open_stream_response(&io, &uri, &http_headers, offset, validator.clone()) {
                Ok(StreamOpened::Body {
                    response,
                    skip_bytes,
                }) => {
                    if validator.is_none() {
                        validator = response_entity_validator(&response);
                    }
                    response_start = offset;
                    skip_remaining = skip_bytes;
                    live = Some(response);
                }
                Ok(StreamOpened::Eof) => {
                    // We still owe bytes below the known total. A premature
                    // 416 is a failed prefetch, not the end of the media.
                    mark_worker_done(&shared, true);
                    return;
                }
                Err(SourceError::HttpEntityChanged) => {
                    shared.entity_changed.store(true, Ordering::Release);
                    mark_worker_done(&shared, true);
                    return;
                }
                Err(error) => {
                    resumes_since_progress += 1;
                    http_trace_log(format!(
                        "{{\"event\":\"http_stream_open_error\",\"worker\":0,\"offset\":{},\"attempt\":{},\"error\":\"{}\"}}",
                        offset,
                        resumes_since_progress,
                        json_escape(&error.to_string()),
                    ));
                    if resumes_since_progress > HTTP_STREAM_MAX_RESUMES {
                        mark_worker_done(&shared, true);
                        return;
                    }
                    if io.wait_cancelled(Duration::from_millis(300)) {
                        return;
                    }
                    continue;
                }
            }
        }
        let mut response = live.take().expect("live response exists");
        let body = io.run(async move {
            let chunk = tokio::time::timeout(HTTP_STREAM_BODY_TIMEOUT, response.chunk())
                .await
                .map_err(|_| SourceError::Http("HTTP stream body timed out".to_string()))?
                .map_err(|error| SourceError::Http(error.without_url().to_string()))?;
            Ok((response, chunk))
        });
        match body {
            Ok((_response, None)) => {
                // A valid 206 can cover less than the requested tail. Keep its
                // bytes in this stripe and continue from the reached offset,
                // replaying the entity validator just as on a transport error.
                // Empty responses must still exhaust the no-progress allowance.
                live = None;
                resumes_since_progress += 1;
                if resumes_since_progress > HTTP_STREAM_MAX_RESUMES {
                    mark_worker_done(&shared, true);
                    return;
                }
                if offset == response_start {
                    if io.wait_cancelled(Duration::from_millis(300)) {
                        return;
                    }
                }
            }
            Ok((response, Some(bytes))) => {
                live = Some(response);
                // A 200 starts at byte zero even after a reconnect. Discard
                // the prefix in chunks without changing the cache offset.
                let skip = skip_remaining.min(bytes.len() as u64) as usize;
                skip_remaining -= skip as u64;
                {
                    let mut inner = lock_stream(&shared);
                    inner.progress_bytes += bytes.len() as u64;
                    inner.last_progress = Instant::now();
                }
                let bytes = &bytes[skip..];
                if bytes.is_empty() {
                    continue;
                }
                if stripe_opened.is_none() {
                    stripe_opened = Some(Instant::now());
                }
                let received = bytes.len().min((total - offset) as usize);
                resumes_since_progress = 0;
                let mut consumed = 0usize;
                while consumed < received {
                    // A single HTTP body chunk can contain several stripes.
                    // Apply backpressure at every stripe boundary, including
                    // boundaries inside that chunk, before publishing more.
                    if !wait_for_stream_window(&shared, &io, epoch, offset, !stripe.is_empty()) {
                        return;
                    }
                    let available = HTTP_STREAM_STRIPE_BYTES as usize - stripe.len();
                    let take = available.min(received - consumed);
                    stripe.extend_from_slice(&bytes[consumed..consumed + take]);
                    consumed += take;
                    offset += take as u64;
                    if stripe.len() as u64 == HTTP_STREAM_STRIPE_BYTES {
                        let elapsed = stripe_opened
                            .take()
                            .map_or(0.0, |opened| opened.elapsed().as_secs_f64() * 1000.0);
                        http_trace_log(format!(
                            "{{\"event\":\"http_stream_stripe\",\"worker\":0,\"index\":{stripe_index},\"start\":{},\"bytes\":{},\"elapsed_ms\":{elapsed:.3}}}",
                            stripe_start,
                            stripe.len(),
                        ));
                        hand_off_stripe(
                            &shared,
                            epoch,
                            stripe_index,
                            stripe_start,
                            std::mem::take(&mut stripe),
                        );
                        stripe_index += 1;
                        stripe_start = offset;
                        if consumed < received {
                            stripe_opened = Some(Instant::now());
                        }
                    }
                }
            }
            Err(SourceError::Cancelled) => return,
            Err(error) => {
                resumes_since_progress += 1;
                http_trace_log(format!(
                    "{{\"event\":\"http_stream_body_error\",\"worker\":0,\"offset\":{},\"attempt\":{},\"error\":\"{}\"}}",
                    offset,
                    resumes_since_progress,
                    json_escape(&error.to_string()),
                ));
                if resumes_since_progress > HTTP_STREAM_MAX_RESUMES {
                    mark_worker_done(&shared, true);
                    return;
                }
                if io.wait_cancelled(Duration::from_millis(300)) {
                    return;
                }
            }
        }
    }
}

const HTTP_FETCH_MAX_ATTEMPTS: u32 = 3;
/// Total attempts allowed for one logical fetch once attempts start making
/// progress (see `HttpRetryGate`).
const HTTP_FETCH_MAX_RESUME_ATTEMPTS: u32 = 8;
const HTTP_FETCH_RETRY_BACKOFF: [Duration; 2] =
    [Duration::from_millis(200), Duration::from_secs(1)];
/// Wall-clock ceiling on one logical fetch, retries and backoff included.
///
/// `read_range` runs on the demuxer thread, so every retry freezes playback;
/// this bounds the stall. It leaves room for multiple response/body attempts
/// on a slow origin: giving up ends playback (EIO is terminal) rather than
/// merely stalling it. Attempts without any progress still stop after
/// `HTTP_FETCH_MAX_ATTEMPTS`, so a broken origin fails fast.
///
/// Every request receives only the remaining budget, so connect, headers,
/// response bodies, retry delays, and retries all share this one ceiling.
const HTTP_FETCH_TOTAL_BUDGET: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
struct HttpFetchTimeouts {
    response: Duration,
    body: Duration,
    total: Duration,
}

impl Default for HttpFetchTimeouts {
    fn default() -> Self {
        Self {
            response: Duration::from_secs(15),
            body: Duration::from_secs(60),
            total: HTTP_FETCH_TOTAL_BUDGET,
        }
    }
}

/// Hard ceiling on the body of one HTTP request.
///
/// Deliberately *not* the read-ahead window. Capping each exchange bounds its
/// transient body allocation and makes retries resume in small pieces. The
/// cache window is still filled to its configured depth by successive capped
/// requests or by the persistent stream worker.
const HTTP_REQUEST_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// How close to the end of the resource an empty read may fall and still
/// count as reaching the end instead of a fetch failure (issue #163).
///
/// Players skip into the closing moments of a file and read to its natural
/// end, so short reads at the resource boundary are normal traffic; origins
/// answer the final range with a 206 that promises the tail and delivers
/// nothing. A fetch whose request window was truncated by the resource total
/// starts at most `HTTP_REQUEST_MAX_BYTES` short of it, so mirroring that
/// ceiling here keeps this guard in agreement with the fetch-side
/// `window_ends_at_total` rule. Beyond the slack, an uncovered read is still
/// an error: bytes that exist must never look like EOF.
const HTTP_TAIL_EOF_SLACK_BYTES: u64 = HTTP_REQUEST_MAX_BYTES;

/// How much already-played data stays in the cache. A rewind inside this tail is
/// served locally instead of re-downloaded: mpv keeps 50 MiB of back buffer by
/// default (`--demuxer-max-back-bytes`), VLC reuses a 3x4 MiB ring set. 16 MiB
/// sits between them and covers a 10 s step at up to ~13 Mbps. This is the
/// DEFAULT budget -- `http_back_buffer_bytes` on the open options overrides it,
/// which is how hosts size the tail from media bitrate (a -10 s step at
/// 71 Mbps covers ~89 MB).
const HTTP_CACHE_RETAIN_BYTES: u64 = 16 * 1024 * 1024;

/// The retained tail is only cut once it is this much over budget, so the O(n)
/// buffer move happens per few MiB of playback instead of once per read.
const HTTP_CACHE_TRIM_SLACK: u64 = 8 * 1024 * 1024;

/// Hard stop for open-ended reads (`read_uri_to_end`: danmaku/subtitle sidecars).
/// Matches the kernel's other sidecar ceilings; it only exists so an origin that
/// answers full pieces forever cannot grow the cache without bound. Hitting it is
/// reported as an error, never a silent short read.
const HTTP_STREAM_TO_EOF_BYTE_LIMIT: u64 = 256 * 1024 * 1024;

/// Ceiling for the configured read-ahead window. The window is a memory buffer,
/// so an untrusted or extreme value (e.g. `u64::MAX` from a caller) must not be
/// allowed to make the cache grow toward the whole media file. Default is 2 MiB;
/// the App's picker tops out at 32 MiB, well under this.
const HTTP_READ_AHEAD_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Most pieces one read may pull. An origin that answers short bodies needs
/// several pieces per read (that is normal); this only stops a pathological one,
/// and hitting it is reported as an error rather than a silent short read.
const HTTP_FETCH_MAX_PIECES_PER_READ: u32 = 64;

/// Consecutive background-prefetch failures before the chain is parked until a
/// synchronous fetch succeeds.
const HTTP_PREFETCH_MAX_FAILURES: u32 = 3;

/// Stripe size: the unit a worker buffers before handing bytes to the reader.
/// Purely bookkeeping -- the request itself is open-ended and never re-issued
/// except on resume -- so it bounds transient memory, not request count.
const HTTP_STREAM_STRIPE_BYTES: u64 = 4 * 1024 * 1024;
/// A stream that delivers nothing for this long while the reader waits is
/// considered dead; the read falls back to the synchronous capped path.
const HTTP_STREAM_STALL: Duration = Duration::from_secs(20);
/// Overall ceiling on one read's wait for stream coverage; past it the sync
/// path (with its own retry budget) takes over.
const HTTP_STREAM_WAIT_BUDGET: Duration = Duration::from_secs(60);
/// Reconnects a worker may make without delivering a single byte. Any
/// progress resets the counter, so a truncating origin that still moves the
/// offset never trips it.
const HTTP_STREAM_MAX_RESUMES: u32 = 3;
/// Per-worker response/body timeouts. Generous on purpose: the stream is
/// background traffic, and a healthy long-lived body must not be killed by
/// the 15 s response deadline that shapes the synchronous request cap.
const HTTP_STREAM_RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);
const HTTP_STREAM_BODY_TIMEOUT: Duration = Duration::from_secs(60);

/// Retry policy for one logical fetch (every attempt at the same range).
///
/// Two-tier: without progress the old behaviour stands (three attempts, so a
/// broken origin fails fast instead of freezing the demuxer thread); with
/// progress the fetch may keep resuming, because on a slow link a capped 4 MiB
/// request may need several attempts. Resuming is bounded by the
/// attempt count and `HTTP_FETCH_TOTAL_BUDGET` so a foreground stall stays
/// finite.
struct HttpRetryGate {
    started: Instant,
    budget: Duration,
    attempts: u32,
    attempts_without_progress: u32,
    last_bytes: u64,
}

impl HttpRetryGate {
    fn new(budget: Duration) -> Self {
        Self {
            started: Instant::now(),
            budget,
            attempts: 0,
            attempts_without_progress: 0,
            last_bytes: 0,
        }
    }

    /// Note that an attempt is starting; returns its 1-based number.
    fn begin_attempt(&mut self) -> u32 {
        self.attempts = self.attempts.saturating_add(1);
        self.attempts
    }

    /// Whether the wall-clock ceiling is already spent. Requests additionally
    /// receive the remaining budget so their header/body waits cannot exceed it.
    fn expired(&self) -> bool {
        self.started.elapsed() >= self.budget
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Record a failed attempt (`received` = bytes of this logical fetch so
    /// far): `Some(backoff)` to wait and try again, `None` to give up.
    fn fail(&mut self, received: u64) -> Option<Duration> {
        if received > self.last_bytes {
            self.last_bytes = received;
            self.attempts_without_progress = 0;
        } else {
            self.attempts_without_progress = self.attempts_without_progress.saturating_add(1);
        }
        if self.attempts_without_progress >= HTTP_FETCH_MAX_ATTEMPTS
            || self.attempts >= HTTP_FETCH_MAX_RESUME_ATTEMPTS
        {
            return None;
        }
        let backoff = http_retry_backoff(self.attempts);
        if self.started.elapsed().saturating_add(backoff) >= self.budget {
            return None;
        }
        Some(backoff)
    }
}

fn http_retry_backoff(attempt: u32) -> Duration {
    let index = usize::try_from(attempt.saturating_sub(1)).unwrap_or(0);
    HTTP_FETCH_RETRY_BACKOFF
        .get(index)
        .copied()
        .unwrap_or(Duration::from_secs(1))
}

#[derive(Debug, Error)]
enum HttpRequestError {
    #[error("{0}")]
    Request(#[from] reqwest::Error),
    #[error("HTTP phase timed out")]
    Timeout(#[from] tokio::time::error::Elapsed),
}

/// Phase timeouts, transport errors and 5xx are transient; 4xx are not.
fn http_error_is_retryable(error: &HttpRequestError) -> bool {
    match error {
        HttpRequestError::Timeout(_) => true,
        HttpRequestError::Request(error) => {
            error.status().is_none_or(|status| status.is_server_error())
        }
    }
}

// The request's own timeout remains active through the body and caps the
// entire exchange at the remaining fetch budget. This shorter response timer
// leaves time to retry when an origin accepts a connection but sends no head.
async fn send_http_request(
    request: reqwest::RequestBuilder,
    remaining: Duration,
    response_timeout: Duration,
) -> std::result::Result<reqwest::Response, HttpRequestError> {
    Ok(
        tokio::time::timeout(response_timeout, request.timeout(remaining).send())
            .await?
            .and_then(reqwest::Response::error_for_status)
            .map_err(reqwest::Error::without_url)?,
    )
}

/// Parses the `total` out of a `Content-Range: bytes start-end/total` header.
/// Returns `None` for missing headers, unsatisfied-range (`*/total` still
/// yields the total), and unknown totals (`bytes 0-1/*`).
fn parse_content_range_total(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (_, total) = rest.rsplit_once('/')?;
    total.trim().parse::<u64>().ok()
}

/// Parses the first byte offset out of a `Content-Range: bytes start-end/total`
/// header. `None` for an unsatisfied-range form (`bytes */total`), which names
/// no offset.
fn parse_content_range_start(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, _) = rest.rsplit_once('/')?;
    let (start, _) = range.trim().split_once('-')?;
    start.trim().parse::<u64>().ok()
}

/// The strongest entity validator the response offers, preferred in the order
/// RFC 9110 recommends for `If-Range`.
fn response_entity_validator(response: &reqwest::Response) -> Option<String> {
    ["etag", "last-modified"].into_iter().find_map(|name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// Weak ETags describe semantic equivalence, not byte identity. They cannot
/// authorize a range splice or be sent in If-Range (RFC 9110 section 13.1.5).
fn validate_resume_validator(validator: Option<&str>) -> Result<()> {
    if validator.is_some_and(|value| value.starts_with("W/")) {
        return Err(SourceError::HttpEntityChanged);
    }
    Ok(())
}

/// A 200 may mean that If-Range detected a changed resource. Only splice its
/// tail onto previously received bytes when the original validator still
/// matches; missing validators cannot prove that the replay is the same file.
fn validate_full_response_entity(
    response: &reqwest::Response,
    validator: Option<&str>,
) -> Result<()> {
    validate_resume_validator(validator)?;
    if let Some(expected) = validator
        && response_entity_validator(response).as_deref() != Some(expected)
    {
        return Err(SourceError::HttpEntityChanged);
    }
    Ok(())
}

/// Learns the total length from a one-byte GET, for origins that reject HEAD.
///
/// The body is deliberately never read. An origin that rejects HEAD *and*
/// ignores Range answers 200 with the whole object, so buffering the response
/// would turn a `len()` call into a full download of the media -- gigabytes
/// into memory before playback, just to learn a number the headers already
/// carry.
#[cfg(test)]
fn probe_http_total_length(
    agent: &HttpIo,
    uri: &str,
    http_headers: &[(String, String)],
) -> Result<Option<u64>> {
    let client = agent.client.clone();
    let uri = uri.to_owned();
    let http_headers = http_headers.to_vec();
    agent.run(async move {
        probe_http_total_length_async(
            &client,
            &uri,
            &http_headers,
            HttpFetchTimeouts::default().response,
        )
        .await
    })
}

async fn probe_http_total_length_async(
    client: &reqwest::Client,
    uri: &str,
    http_headers: &[(String, String)],
    timeout: Duration,
) -> Result<Option<u64>> {
    let probe = ByteRange {
        start: 0,
        length: Some(1),
    };
    let mut request = client.get(uri).header("Range", http_range_header(probe));
    for (name, value) in http_headers {
        request = request.header(name, value);
    }
    if timeout.is_zero() {
        return Err(SourceError::Http(
            "http metadata deadline exceeded".to_string(),
        ));
    }
    let response = request
        .timeout(timeout)
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .map_err(|error| {
            http_trace_log(format!(
                "{{\"event\":\"http_length_probe_error\",\"phase\":\"request\",\"error\":\"{}\"}}",
                json_escape(&error.to_string()),
            ));
            SourceError::Http(error.to_string())
        })?;
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let total_length = match status {
        206 | 416 => header("content-range")
            .as_deref()
            .and_then(parse_content_range_total),
        // Range was ignored; Content-Length is the whole object, which is
        // exactly the total being probed for.
        200 => header("content-length").and_then(|value| value.trim().parse::<u64>().ok()),
        status if status >= 400 => {
            let error = SourceError::Http(format!("http status: {status}"));
            http_trace_log(format!(
                "{{\"event\":\"http_length_probe_error\",\"phase\":\"status\",\"status\":{status}}}"
            ));
            return Err(error);
        }
        _ => None,
    };
    http_trace_log(format!(
        "{{\"event\":\"http_length_probe\",\"status\":{},\"total\":{}}}",
        status,
        total_length.map_or_else(|| "null".to_string(), |total| total.to_string()),
    ));
    Ok(total_length)
}

fn http_range_header(range: ByteRange) -> String {
    match range.length {
        Some(length) if length > 0 => {
            let end = range.start.saturating_add(length).saturating_sub(1);
            format!("bytes={}-{}", range.start, end)
        }
        _ => format!("bytes={}-", range.start),
    }
}

fn fetch_http_range(
    agent: &HttpIo,
    uri: &str,
    http_headers: &[(String, String)],
    range: ByteRange,
    event: &str,
) -> Result<HttpRangeResponse> {
    let client = agent.client.clone();
    let uri = uri.to_owned();
    let http_headers = http_headers.to_vec();
    let event = event.to_owned();
    agent.run(async move {
        fetch_http_range_async(
            &client,
            &uri,
            &http_headers,
            range,
            &event,
            HttpFetchTimeouts::default(),
        )
        .await
    })
}

async fn fetch_http_range_async(
    client: &reqwest::Client,
    uri: &str,
    http_headers: &[(String, String)],
    range: ByteRange,
    event: &str,
    timeouts: HttpFetchTimeouts,
) -> Result<HttpRangeResponse> {
    let mut bytes = Vec::new();
    let mut total_length = None;
    let mut validator: Option<String> = None;
    let mut gate = HttpRetryGate::new(timeouts.total);
    loop {
        let attempt = gate.begin_attempt();
        if gate.expired() {
            http_trace_log(format!(
                "{{\"event\":\"{}_error\",\"phase\":\"budget\",\"attempt\":{},\"start\":{},\"elapsed_ms\":{:.3}}}",
                event,
                attempt,
                range.start,
                gate.elapsed().as_secs_f64() * 1000.0,
            ));
            return Err(SourceError::Http(format!(
                "fetch budget of {:?} exhausted for bytes {}..",
                timeouts.total, range.start,
            )));
        }
        let received = bytes.len() as u64;
        if range.length.is_some_and(|length| received >= length) {
            return Ok(HttpRangeResponse {
                bytes,
                total_length,
            });
        }
        if received > 0 {
            validate_resume_validator(validator.as_deref())?;
        }
        let resume_range = ByteRange {
            start: range.start.saturating_add(received),
            length: range.length.map(|length| length.saturating_sub(received)),
        };
        let header = http_range_header(resume_range);
        let started = Instant::now();
        let mut request = client.get(uri).header("Range", &header);
        for (name, value) in http_headers {
            request = request.header(name, value);
        }
        if received > 0
            && let Some(validator) = validator.as_deref()
        {
            request = request.header("If-Range", validator);
        }
        let remaining = timeouts.total.saturating_sub(gate.elapsed());
        let mut response = match send_http_request(request, remaining, timeouts.response).await {
            Ok(response) => response,
            Err(error) => {
                http_trace_log(format!(
                    "{{\"event\":\"{}_error\",\"phase\":\"request\",\"attempt\":{},\"start\":{},\"length\":{},\"elapsed_ms\":{:.3},\"error\":\"{}\"}}",
                    event,
                    attempt,
                    resume_range.start,
                    resume_range
                        .length
                        .map_or_else(|| "null".to_string(), |length| length.to_string()),
                    started.elapsed().as_secs_f64() * 1000.0,
                    json_escape(&error.to_string()),
                ));
                if http_error_is_retryable(&error)
                    && let Some(backoff) = gate.fail(received)
                {
                    http_trace_log(format!(
                        "{{\"event\":\"{}_retry\",\"phase\":\"request\",\"attempt\":{},\"start\":{},\"received\":{},\"backoff_ms\":{}}}",
                        event,
                        attempt,
                        resume_range.start,
                        received,
                        backoff.as_millis(),
                    ));
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                return Err(SourceError::Http(error.to_string()));
            }
        };
        let status = response.status().as_u16();
        match status {
            206 => {
                let content_range = response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                if let Some(start) = content_range.as_deref().and_then(parse_content_range_start) {
                    if start != resume_range.start {
                        http_trace_log(format!(
                            "{{\"event\":\"{}_error\",\"phase\":\"content_range\",\"attempt\":{},\"start\":{},\"served_start\":{}}}",
                            event, attempt, resume_range.start, start,
                        ));
                        return Err(SourceError::HttpRangeVerification(format!(
                            "server served range from {start}, expected {}",
                            resume_range.start
                        )));
                    }
                } else if resume_range.start > 0 {
                    // A 206 without Content-Range cannot be position-verified,
                    // and a spliced answer would be silent cache corruption;
                    // only offset 0 is unambiguous, so lenient servers keep
                    // working for whole-file requests.
                    http_trace_log(format!(
                        "{{\"event\":\"{}_error\",\"phase\":\"content_range\",\"attempt\":{},\"start\":{},\"served_start\":null}}",
                        event, attempt, resume_range.start,
                    ));
                    return Err(SourceError::HttpRangeVerification(
                        "server served 206 without Content-Range; cannot verify the resumed offset"
                            .to_string(),
                    ));
                }
                if total_length.is_none() {
                    total_length = content_range.as_deref().and_then(parse_content_range_total);
                }
                if validator.is_none() {
                    validator = response_entity_validator(&response);
                }
            }
            200 => {
                if received > 0 {
                    validate_full_response_entity(&response, validator.as_deref())?;
                }
                if total_length.is_none() {
                    total_length = response
                        .headers()
                        .get("content-length")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok());
                }
                if validator.is_none() {
                    validator = response_entity_validator(&response);
                }
            }
            _ => {
                http_trace_log(format!(
                    "{{\"event\":\"{}_error\",\"phase\":\"status\",\"attempt\":{},\"start\":{},\"status\":{}}}",
                    event, attempt, resume_range.start, status,
                ));
                return Err(SourceError::Http(format!(
                    "unexpected HTTP status {status} for Range request"
                )));
            }
        }
        let mut skip_remaining = if status == 200 { resume_range.start } else { 0 };
        let mut response_bytes = 0u64;
        let mut reached_eof = false;
        let body_result: std::result::Result<(), HttpRequestError> = async {
            while let Some(chunk) = tokio::time::timeout(timeouts.body, response.chunk())
                .await?
                .map_err(reqwest::Error::without_url)?
            {
                response_bytes = response_bytes.saturating_add(chunk.len() as u64);
                let skip = skip_remaining.min(chunk.len() as u64) as usize;
                skip_remaining -= skip as u64;
                let chunk = &chunk[skip..];
                let take = range.length.map_or(chunk.len(), |length| {
                    length
                        .saturating_sub(bytes.len() as u64)
                        .min(chunk.len() as u64) as usize
                });
                bytes.extend_from_slice(&chunk[..take]);
                // Drop the remaining whole-file body once the requested range
                // is covered; never buffer it just because Range was ignored.
                if range
                    .length
                    .is_some_and(|length| bytes.len() as u64 >= length)
                {
                    return Ok(());
                }
            }
            reached_eof = true;
            Ok(())
        }
        .await;
        if let Err(error) = body_result {
            // Release the stalled socket before sleeping or opening its resume.
            drop(response);
            http_trace_log(format!(
                "{{\"event\":\"{}_error\",\"phase\":\"body\",\"attempt\":{},\"start\":{},\"length\":{},\"status\":{},\"bytes\":{},\"elapsed_ms\":{:.3},\"error\":\"{}\"}}",
                event,
                attempt,
                resume_range.start,
                resume_range
                    .length
                    .map_or_else(|| "null".to_string(), |length| length.to_string()),
                status,
                bytes.len(),
                started.elapsed().as_secs_f64() * 1000.0,
                json_escape(&error.to_string()),
            ));
            if let Some(backoff) = gate.fail(bytes.len() as u64) {
                http_trace_log(format!(
                    "{{\"event\":\"{}_retry\",\"phase\":\"body\",\"attempt\":{},\"start\":{},\"received\":{},\"backoff_ms\":{}}}",
                    event,
                    attempt,
                    range.start.saturating_add(bytes.len() as u64),
                    bytes.len(),
                    backoff.as_millis(),
                ));
                tokio::time::sleep(backoff).await;
                continue;
            }
            return Err(SourceError::Http(error.to_string()));
        }
        if status == 200 && reached_eof && total_length.is_none() {
            // Count the discarded prefix too. This also discovers EOF when
            // the requested offset is beyond a lengthless response's body.
            total_length = Some(response_bytes);
        }
        http_trace_log(format!(
            "{{\"event\":\"{}\",\"attempt\":{},\"start\":{},\"length\":{},\"status\":{},\"bytes\":{},\"elapsed_ms\":{:.3}}}",
            event,
            attempt,
            range.start,
            range
                .length
                .map_or_else(|| "null".to_string(), |length| length.to_string()),
            status,
            bytes.len(),
            started.elapsed().as_secs_f64() * 1000.0,
        ));
        return Ok(HttpRangeResponse {
            bytes,
            total_length,
        });
    }
}

async fn fetch_http_length(
    client: &reqwest::Client,
    uri: &str,
    http_headers: &[(String, String)],
    started: Instant,
    timeouts: HttpFetchTimeouts,
) -> Result<Option<u64>> {
    let mut gate = HttpRetryGate::new(timeouts.total);
    let head_error = loop {
        let attempt = gate.begin_attempt();
        if gate.expired() {
            return Err(SourceError::Http(
                "http metadata deadline exceeded".to_string(),
            ));
        }
        let mut request = client.head(uri);
        for (name, value) in http_headers {
            request = request.header(name, value);
        }
        let remaining = timeouts.total.saturating_sub(gate.elapsed());
        match send_http_request(request, remaining, timeouts.response).await {
            Ok(response) => {
                let status = response.status().as_u16();
                let length = response
                    .headers()
                    .get("content-length")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok());
                if length == Some(0) {
                    http_trace_log(format!(
                        "[erika-http-trace] stage=head_zero_length_fallback status={} elapsed_ms={:.3}",
                        status,
                        started.elapsed().as_secs_f64() * 1000.0,
                    ));
                    return match probe_http_total_length_async(
                        client,
                        uri,
                        http_headers,
                        timeouts
                            .response
                            .min(timeouts.total.saturating_sub(gate.elapsed())),
                    )
                    .await
                    {
                        Ok(total_length) => Ok(total_length),
                        Err(error) => Err(SourceError::Http(format!(
                            "HEAD reported Content-Length: 0 and range probe failed: {error}"
                        ))),
                    };
                }
                http_trace_log(format!(
                    "[erika-http-trace] stage=head_response status={} length={} elapsed_ms={:.3}",
                    status,
                    length.map_or_else(|| "null".to_string(), |length| length.to_string()),
                    started.elapsed().as_secs_f64() * 1000.0,
                ));
                return Ok(length);
            }
            Err(error) => {
                http_trace_log(format!(
                    "[erika-http-trace] stage=head_error attempt={} elapsed_ms={:.3} error={}",
                    attempt,
                    started.elapsed().as_secs_f64() * 1000.0,
                    json_escape(&error.to_string()),
                ));
                if http_error_is_retryable(&error)
                    && let Some(backoff) = gate.fail(0)
                {
                    http_trace_log(format!(
                        "[erika-http-trace] stage=head_retry attempt={} backoff_ms={}",
                        attempt,
                        backoff.as_millis(),
                    ));
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                break error;
            }
        }
    };
    http_trace_log(format!(
        "[erika-http-trace] stage=head_fallback_range error={}",
        json_escape(&head_error.to_string()),
    ));
    match probe_http_total_length_async(
        client,
        uri,
        http_headers,
        timeouts
            .response
            .min(timeouts.total.saturating_sub(gate.elapsed())),
    )
    .await
    {
        Ok(total_length) => Ok(total_length),
        Err(_) => Err(SourceError::Http(head_error.to_string())),
    }
}

impl std::fmt::Debug for HttpRangeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRangeSource")
            .field("uri", &redacted_uri(&self.uri))
            .field("content_length", &self.content_length)
            .field("cache_start", &self.cache_start)
            .field("cache_bytes", &self.cache_bytes.len())
            .field("read_ahead_bytes", &self.read_ahead_bytes)
            .field("request_bytes", &self.request_bytes)
            .finish()
    }
}

impl Drop for HttpRangeSource {
    fn drop(&mut self) {
        self.agent.cancel();
        self.kill_streams();
    }
}

impl MediaSource for HttpRangeSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn cancellation(&self) -> Option<SourceCancellation> {
        Some(self.agent.cancellation.clone())
    }

    fn release_buffer(&mut self) {
        self.kill_streams();
        self.cache_bytes = Vec::new();
        self.cache_start = 0;
        self.stream_reader_end = 0;
        self.prefetch_failures = 0;
    }

    fn len(&mut self) -> Result<Option<u64>> {
        if self.content_length.is_some() {
            return Ok(self.content_length);
        }
        if self.length_probed {
            // A finished probe already established that the origin reports no
            // length (chunked / connection-close). Only transient probe errors
            // stay retryable.
            return Ok(None);
        }
        let started = Instant::now();
        http_trace_log(format!(
            "[erika-http-trace] stage=head_request uri={} cache_start={} cache_end={} read_ahead={}",
            redacted_uri(&self.uri),
            self.cache_start,
            self.cache_end(),
            self.read_ahead_bytes,
        ));
        let client = self.agent.client.clone();
        let uri = self.uri.clone();
        let http_headers = self.http_headers.clone();
        self.content_length = self.agent.run(async move {
            fetch_http_length(
                &client,
                &uri,
                &http_headers,
                started,
                HttpFetchTimeouts::default(),
            )
            .await
        })?;
        self.length_probed = true;
        Ok(self.content_length)
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        if self.agent.cancellation.is_cancelled() {
            return Err(SourceError::Cancelled);
        }
        self.check_entity()?;
        if self
            .streams
            .as_ref()
            .is_some_and(|session| session.io.is_cancelled())
        {
            self.kill_streams();
        }
        // Fold in stripes the workers completed since the last read: the hit
        // checks below only see what is already in `cache_bytes`.
        self.drain_stripes();
        self.trim_cache(range);
        self.stream_reader_end = range.start.saturating_add(range.length.unwrap_or(0));
        self.update_stream_backpressure();

        if let Some(bytes) = self.cached_slice(range) {
            self.ensure_streams();
            self.check_entity()?;
            http_trace_log(format!(
                "{{\"event\":\"http_cache_hit\",\"start\":{},\"length\":{},\"bytes\":{}}}",
                range.start,
                range.length.unwrap_or_default(),
                bytes.len(),
            ));
            return Ok(bytes);
        }

        if let Some(total) = self.content_length
            && range.start >= total
        {
            // Past EOF: an empty read, which the AVIO layer turns into EOF.
            return Ok(Vec::new());
        }

        // The read is not covered yet. The streams are already delivering
        // bytes from `cache_end` onward -- wait for them instead of issuing a
        // duplicate download. A read behind the cache (a rewind) skips the
        // wait: re-anchoring in the fetch below handles it. Dead or stalled
        // streams hand the wait back to the synchronous path.
        //
        // The skip is keyed off the read's *start*, matching the boundary
        // `fetch_missing` re-anchors at: the wait is skipped exactly when the
        // fetch will re-anchor, so there is no band where the wait is skipped
        // *and* the fetch fills from `cache_end` -- which would re-download the
        // range the workers are already delivering (and `drain_stripes` would
        // then discard their copy as overlap). Keying it off the read's *end*
        // opened that band for any read longer than one request cap. A read
        // that jumps farther ahead than the fetch could cover re-anchors at the
        // read instead, so waiting for it could only burn the stall clock.
        let waiting_end = range.start.saturating_add(range.length.unwrap_or(0));
        if waiting_end > self.cache_end()
            && range.start <= self.cache_end().saturating_add(HTTP_REQUEST_MAX_BYTES)
            && self.wait_for_stream_coverage(waiting_end)
            && let Some(bytes) = self.cached_slice(range)
        {
            self.ensure_streams();
            self.check_entity()?;
            return Ok(bytes);
        }

        self.check_entity()?;
        self.fetch_missing(range)?;
        self.drain_stripes();
        self.ensure_streams();
        self.check_entity()?;

        // Serve whatever the cache now holds from the read position. A short
        // read is legitimate (EOF, or an origin that answered short); an empty
        // result becomes EOF in the AVIO layer, so it is only allowed when the
        // resource really ends before the read.
        let offset = usize::try_from(range.start.saturating_sub(self.cache_start)).unwrap_or(0);
        let Some(tail) = self.cache_bytes.get(offset..) else {
            if let Some(total) = self.uncovered_read(range.start) {
                return Err(SourceError::Http(format!(
                    "no data for bytes {}.. (origin total {total})",
                    range.start,
                )));
            }
            // Within the tail slack an empty result is the resource's end: a
            // fetch window truncated by the total answered with nothing
            // (issue #163) rather than a failure mid-read.
            return Ok(Vec::new());
        };
        if tail.is_empty() && self.uncovered_read(range.start).is_some() {
            // A whole-file 200 may have just discovered EOF before this read's
            // offset. Empty data below the total still means the fetch failed
            // (e.g. an empty 206), which must not look like EOF.
            return Err(SourceError::Http(format!(
                "cache holds no bytes at {}.. although the resource is larger",
                range.start,
            )));
        }
        let copy_len = range.length.map_or(tail.len(), |length| {
            usize::try_from(length)
                .unwrap_or(usize::MAX)
                .min(tail.len())
        });
        Ok(tail[..copy_len].to_vec())
    }
}

pub fn source_from_uri(uri: &str) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint(uri, MediaSourceHint::Auto)
}

/// Reads an entire URI through the same MediaSource abstraction used by FFmpeg.
///
/// This is intentionally synchronous for small sidecar assets such as danmaku or
/// subtitle files. On Android it also establishes and completes the ownership
/// transfer for `fd://` descriptors within the native call.
pub fn read_uri_to_end(uri: &str) -> Result<Vec<u8>> {
    let mut source = source_from_uri(uri)?;
    source.read_range(ByteRange::suffix_from(0))
}

pub fn source_from_uri_with_hint(
    uri: &str,
    source_hint: MediaSourceHint,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint_and_headers(uri, source_hint, Vec::new())
}

pub fn source_from_uri_with_hint_and_headers(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_options(uri, source_hint, http_headers, None, None)
}

/// `http_read_ahead_bytes` only applies to HTTP(S) sources and overrides the
/// per-request read-ahead window; `None` keeps the default resolution
/// (env override, then the 2 MiB engine default). `http_back_buffer_bytes`
/// likewise overrides the rewind budget; `None` keeps the 16 MiB default.
pub fn source_from_uri_with_options(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    http_read_ahead_bytes: Option<u64>,
    http_back_buffer_bytes: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    match source_hint {
        MediaSourceHint::Auto => source_from_auto_uri(
            uri,
            http_headers,
            http_read_ahead_bytes,
            http_back_buffer_bytes,
        ),
        MediaSourceHint::LocalFile => source_from_local_uri(uri),
        MediaSourceHint::Http => {
            if uri.starts_with("http://") || uri.starts_with("https://") {
                Ok(Box::new(HttpRangeSource::with_http_headers_and_window(
                    uri,
                    http_headers,
                    http_read_ahead_bytes,
                    http_back_buffer_bytes,
                )))
            } else {
                Err(SourceError::Unsupported(uri.to_string()))
            }
        }
    }
}

fn source_from_auto_uri(
    uri: &str,
    http_headers: Vec<(String, String)>,
    http_read_ahead_bytes: Option<u64>,
    http_back_buffer_bytes: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    if uri.starts_with("fd://") {
        return source_from_local_uri(uri);
    }
    if let Some(path) = uri.strip_prefix("file://") {
        return Ok(Box::new(LocalFileSource::open(file_uri_path(path))?));
    }
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Ok(Box::new(HttpRangeSource::with_http_headers_and_window(
            uri,
            http_headers,
            http_read_ahead_bytes,
            http_back_buffer_bytes,
        )));
    }
    let path = Path::new(uri);
    if path.exists() {
        return Ok(Box::new(LocalFileSource::open(path)?));
    }
    Err(SourceError::Unsupported(uri.to_string()))
}

fn source_from_local_uri(uri: &str) -> Result<Box<dyn MediaSource>> {
    if uri.starts_with("fd://") {
        #[cfg(target_os = "android")]
        {
            // SAFETY: accepting this URI is the ownership-transfer boundary.
            return Ok(Box::new(unsafe {
                OwnedFileDescriptorSource::open_uri(uri)?
            }));
        }
        #[cfg(not(target_os = "android"))]
        {
            return Err(SourceError::Unsupported(uri.to_string()));
        }
    }
    Ok(Box::new(LocalFileSource::open(local_path_from_uri(uri))?))
}

fn local_path_from_uri(uri: &str) -> PathBuf {
    match uri.strip_prefix("file://") {
        Some(path) => file_uri_path(path),
        None => PathBuf::from(uri),
    }
}

/// The local path of a `file:` URI, given what follows `file://`. RFC 8089
/// URIs are percent-encoded (`file:///Videos/%E5%BD%B1.mkv`), as hosts such
/// as Bern and the system file pickers hand them over. liberika itself
/// writes URIs around raw paths (`LocalFileSource::uri`), so a raw path that
/// exists wins when its decoding does not.
fn file_uri_path(path: &str) -> PathBuf {
    let path = path
        .strip_prefix("localhost")
        .filter(|rest| rest.starts_with('/'))
        .unwrap_or(path);
    let raw = PathBuf::from(path);
    let decoded = percent_decoded_path(path);
    if decoded != raw && !decoded.exists() && raw.exists() {
        raw
    } else {
        decoded
    }
}

/// `text` with each valid `%XX` escape replaced by its byte; anything else
/// is kept as written.
fn percent_decoded_path(text: &str) -> PathBuf {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escape {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(decoded))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(&decoded).into_owned())
    }
}

fn http_read_ahead_bytes() -> u64 {
    env::var("ERIKA_HTTP_READAHEAD_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(HttpRangeSource::DEFAULT_READ_AHEAD_BYTES)
}

fn http_trace_log(line: impl AsRef<str>) {
    if !trace::env_flag("ERIKA_HTTP_TRACE") {
        return;
    }
    let path = env::var_os("ERIKA_HTTP_TRACE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/erika_http_trace.jsonl"));
    trace::append_line(line.as_ref(), path);
}

fn redacted_uri(uri: &str) -> String {
    let mut value = uri.to_string();
    for key in ["api_key=", "AccessToken="] {
        let mut search_from = 0;
        while let Some(relative) = value[search_from..].find(key) {
            let start = search_from + relative + key.len();
            let end = value[start..]
                .find('&')
                .map(|relative_end| start + relative_end)
                .unwrap_or(value.len());
            value.replace_range(start..end, "REDACTED");
            search_from = start + "REDACTED".len();
        }
    }
    value
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn http_retry_gate_fails_fast_when_no_bytes_arrive() {
        let mut gate = HttpRetryGate::new(HTTP_FETCH_TOTAL_BUDGET);
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_some());
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_some());
        // A fetch that never moved must fail fast: read_range blocks the
        // demuxer thread.
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_none(), "three stalls must stop the fetch");
    }

    #[test]
    fn http_retry_gate_keeps_resuming_while_bytes_arrive() {
        let mut gate = HttpRetryGate::new(HTTP_FETCH_TOTAL_BUDGET);
        let mut allowed = 0;
        for received in 1..=u64::from(HTTP_FETCH_MAX_RESUME_ATTEMPTS) {
            let _ = gate.begin_attempt();
            match gate.fail(received) {
                Some(_) => allowed += 1,
                None => break,
            }
        }
        // The whole resume allowance is available once every attempt moves the
        // resume point: a capped 4 MiB request on a slow origin legitimately
        // spans several 15 s attempts, and stopping after three would end
        // playback instead of merely slowing it down.
        assert_eq!(allowed, HTTP_FETCH_MAX_RESUME_ATTEMPTS - 1);
    }

    #[test]
    fn http_retry_gate_forgets_stalls_once_bytes_arrive() {
        let mut gate = HttpRetryGate::new(HTTP_FETCH_TOTAL_BUDGET);
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_some());
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_some());
        let _ = gate.begin_attempt();
        assert!(gate.fail(64).is_some(), "progress resets the stall count");
        let _ = gate.begin_attempt();
        assert!(gate.fail(64).is_some());
        let _ = gate.begin_attempt();
        assert!(gate.fail(64).is_some());
        let _ = gate.begin_attempt();
        assert!(
            gate.fail(64).is_none(),
            "progress does not license endless stalls"
        );
    }

    #[test]
    fn http_retry_gate_respects_the_wall_clock_ceiling() {
        let mut gate = HttpRetryGate::new(HTTP_FETCH_TOTAL_BUDGET);
        gate.started = Instant::now() - HTTP_FETCH_TOTAL_BUDGET;
        let _ = gate.begin_attempt();
        assert!(
            gate.fail(1).is_none(),
            "the ceiling stops even a progressing fetch"
        );
    }

    struct MockResponse {
        delay: Duration,
        raw: Vec<u8>,
    }

    impl MockResponse {
        fn immediate(raw: Vec<u8>) -> Self {
            Self {
                delay: Duration::ZERO,
                raw,
            }
        }
    }

    /// Serves each response over one connection (in order) and reports every
    /// received request head through the returned channel.
    fn spawn_mock_http_server(responses: Vec<MockResponse>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let _ = sender.send(head);
                if !response.delay.is_zero() {
                    thread::sleep(response.delay);
                }
                let _ = stream.write_all(&response.raw);
                let _ = stream.flush();
            }
        });
        (uri, receiver)
    }

    fn http_206_response(start: u64, total: u64, body: &[u8]) -> Vec<u8> {
        let end = start + body.len() as u64 - 1;
        let mut raw = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    /// A 206 head that promises `declared_length` bytes but sends fewer before
    /// the connection closes, producing a body-phase transport error.
    fn http_206_truncated_response(
        start: u64,
        total: u64,
        declared_length: usize,
        body: &[u8],
    ) -> Vec<u8> {
        let end = start + declared_length as u64 - 1;
        let mut raw = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n",
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    fn http_simple_response(status_line: &str, body: &[u8]) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    fn recv_request_head(requests: &mpsc::Receiver<String>) -> String {
        requests
            .recv_timeout(Duration::from_secs(5))
            .expect("mock server should have received a request")
            .to_lowercase()
    }

    /// Serves up to `connections` Range requests, trickling each body at
    /// `bytes_per_sec` so the client spends real time inside the body phase --
    /// the way a slow origin does, and the only way to exercise the client's own
    /// body deadline. `piece_limit` caps each answer the way a CDN that ignores
    /// the requested length does; `bytes_per_sec == 0` means no throttling.
    fn spawn_drip_http_server(
        total: u64,
        bytes_per_sec: u64,
        piece_limit: Option<u64>,
        connections: usize,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for _ in 0..connections {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let _ = sender.send(head.clone());
                let (start, mut end) = parse_range_head(&head, total);
                if let Some(limit) = piece_limit {
                    end = end.min(start.saturating_add(limit).saturating_sub(1));
                }
                let length = end.saturating_sub(start) + 1;
                let response_head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(response_head.as_bytes()).is_err() {
                    continue;
                }
                let slice = 32 * 1024u64;
                let mut sent = 0u64;
                while sent < length {
                    let count = slice.min(length - sent) as usize;
                    let chunk: Vec<u8> = (0..count)
                        .map(|index| ((start + sent + index as u64) % 251) as u8)
                        .collect();
                    if stream.write_all(&chunk).is_err() {
                        break;
                    }
                    sent += count as u64;
                    if bytes_per_sec > 0 {
                        thread::sleep(Duration::from_secs_f64(count as f64 / bytes_per_sec as f64));
                    }
                }
                let _ = stream.flush();
            }
        });
        (uri, receiver)
    }

    /// `Range: bytes=start-end` from a raw request head, clamped to the file.
    fn parse_range_head(head: &str, total: u64) -> (u64, u64) {
        let value = head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("range:"))
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim().to_string())
            .unwrap_or_default();
        let spec = value.strip_prefix("bytes=").unwrap_or_default();
        let mut parts = spec.split('-');
        let start = parts
            .next()
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        let end = parts
            .next()
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or_else(|_| total.saturating_sub(1));
        (start, end.min(total.saturating_sub(1)))
    }

    /// Serves every connection on its own thread, so concurrent range requests
    /// genuinely overlap, reports the high-water mark of simultaneously
    /// in-flight requests, and records every request head. Each answer serves
    /// the exact requested range (open-ended included) with position-dependent
    /// bytes: `body[offset] == (offset % 251)`.
    fn spawn_concurrent_mock_http_server(
        total: u64,
    ) -> (
        String,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let (uri, in_flight, max_in_flight, heads, _) = spawn_counted_http_server(total);
        (uri, in_flight, max_in_flight, heads)
    }

    fn spawn_counted_http_server(
        total: u64,
    ) -> (
        String,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
        Arc<AtomicUsize>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let sent_bytes = Arc::new(AtomicUsize::new(0));
        let sent_seen = Arc::clone(&sent_bytes);
        let heads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (in_flight_seen, max_seen, heads_seen) = (
            Arc::clone(&in_flight),
            Arc::clone(&max_in_flight),
            Arc::clone(&heads),
        );
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    return;
                };
                let in_flight = Arc::clone(&in_flight_seen);
                let max_in_flight = Arc::clone(&max_seen);
                let heads = Arc::clone(&heads_seen);
                let sent_bytes = Arc::clone(&sent_seen);
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        head.push_str(&line);
                    }
                    heads.lock().unwrap().push(head.clone());
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_in_flight.fetch_max(now, Ordering::SeqCst);
                    // Hold the request open so the stream workers really do
                    // connect while this one is still on the wire.
                    thread::sleep(Duration::from_millis(150));
                    let (start, end) = parse_range_head(&head, total);
                    let length = end.saturating_sub(start) + 1;
                    let response = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                    );
                    if stream.write_all(response.as_bytes()).is_ok() {
                        let mut sent = 0u64;
                        while sent < length {
                            let count = (32 * 1024u64).min(length - sent) as usize;
                            let chunk = (0..count)
                                .map(|index| ((start + sent + index as u64) % 251) as u8)
                                .collect::<Vec<u8>>();
                            if stream.write_all(&chunk).is_err() {
                                break;
                            }
                            sent += count as u64;
                            sent_bytes.fetch_add(count, Ordering::SeqCst);
                        }
                    }
                    let _ = stream.flush();
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        (uri, in_flight, max_in_flight, heads, sent_bytes)
    }

    /// Origin that serves bounded ranges fully but closes every open-ended
    /// stream right after the headers, leaving the declared body unmet: the
    /// workers hit a body-phase transport error and must fail, exercising the
    /// synchronous fallback.
    fn spawn_flaky_stream_server(total: u64) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let heads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let heads_seen = Arc::clone(&heads);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                heads_seen.lock().unwrap().push(head.clone());
                let (start, end) = parse_range_head(&head, total);
                let range_line = head
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                if range_line.ends_with('-') {
                    // Declare the full tail, deliver nothing, close.
                    let length = total.saturating_sub(start);
                    let response = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{total}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
                        total - 1
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    continue;
                }
                let length = end.saturating_sub(start) + 1;
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(response.as_bytes()).is_ok() {
                    let mut sent = 0u64;
                    while sent < length {
                        let count = (32 * 1024u64).min(length - sent) as usize;
                        let chunk = (0..count)
                            .map(|index| ((start + sent + index as u64) % 251) as u8)
                            .collect::<Vec<u8>>();
                        if stream.write_all(&chunk).is_err() {
                            break;
                        }
                        sent += count as u64;
                    }
                }
                let _ = stream.flush();
            }
        });
        (uri, heads)
    }

    #[test]
    fn local_file_source_reads_ranges() {
        let path = std::env::temp_dir().join(format!("erika-source-{}.bin", std::process::id()));
        {
            let mut file = File::create(&path).unwrap();
            file.write_all(b"abcdef").unwrap();
        }

        let mut source = LocalFileSource::open(&path).unwrap();
        assert_eq!(source.len().unwrap(), Some(6));
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 2,
                    length: Some(3)
                })
                .unwrap(),
            b"cde"
        );
        assert_eq!(
            read_uri_to_end(&format!("file://{}", path.display())).unwrap(),
            b"abcdef"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_uris_are_percent_decoded_unless_the_raw_path_exists() {
        let dir = std::env::temp_dir().join(format!("erika-file-uri-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let named = dir.join("影 #1 100%.mkv");
        std::fs::write(&named, b"named").unwrap();
        let encoded = format!("file://{}/%E5%BD%B1%20%231%20100%25.mkv", dir.display());
        assert_eq!(read_uri_to_end(&encoded).unwrap(), b"named");
        let local = format!(
            "file://localhost{}/%E5%BD%B1%20%231%20100%25.mkv",
            dir.display()
        );
        assert_eq!(read_uri_to_end(&local).unwrap(), b"named");
        // A raw path written into a URI keeps working, also when it
        // contains what looks like an escape.
        let literal = dir.join("a%20b.mkv");
        std::fs::write(&literal, b"literal").unwrap();
        assert_eq!(
            read_uri_to_end(&format!("file://{}", literal.display())).unwrap(),
            b"literal"
        );
        assert_eq!(percent_decoded_path("%zz%4"), PathBuf::from("%zz%4"));
        assert_eq!(percent_decoded_path("%+1"), PathBuf::from("%+1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn source_from_uri_rejects_unknown_scheme() {
        match source_from_uri("smb://example/video.mkv") {
            Ok(_) => panic!("unexpectedly accepted unsupported source"),
            Err(error) => assert!(matches!(error, SourceError::Unsupported(_))),
        }
    }

    #[test]
    fn source_hint_controls_selection() {
        let source =
            source_from_uri_with_hint("https://example.invalid/video.mp4", MediaSourceHint::Http)
                .unwrap();
        assert_eq!(source.uri(), "https://example.invalid/video.mp4");

        assert!(matches!(
            source_from_uri_with_hint("file:///tmp/video.mp4", MediaSourceHint::Http),
            Err(SourceError::Unsupported(_))
        ));
    }

    #[test]
    fn owned_fd_uri_parses_asset_slice() {
        assert_eq!(
            parse_fd_uri("fd://42?offset=4096&length=8192").unwrap(),
            OwnedFdUri {
                fd: 42,
                offset: 4096,
                length: Some(8192),
            }
        );
        assert_eq!(
            parse_fd_uri("fd://7?length=-1").unwrap(),
            OwnedFdUri {
                fd: 7,
                offset: 0,
                length: None,
            }
        );
    }

    #[test]
    fn owned_fd_uri_rejects_invalid_or_ambiguous_values() {
        for uri in [
            "fd://-1",
            "fd://not-a-number",
            "fd://3?offset=x",
            "fd://3?offset=1&offset=2",
            "fd://3?unknown=1",
        ] {
            assert!(matches!(
                parse_fd_uri(uri),
                Err(SourceError::InvalidFileDescriptorUri(_))
            ));
        }
    }

    #[cfg(target_os = "android")]
    #[test]
    fn unregistered_owned_fd_uri_cannot_adopt_a_numeric_descriptor() {
        let error = unsafe { OwnedFileDescriptorSource::open_uri("fd://2147483647") }
            .expect_err("an unregistered descriptor must be rejected");
        assert!(matches!(
            error,
            SourceError::InvalidFileDescriptorUri(message)
                if message.contains("not explicitly transferred")
        ));
    }

    #[test]
    fn http_default_read_ahead_is_streaming_sized() {
        assert_eq!(HttpRangeSource::DEFAULT_READ_AHEAD_BYTES, 2 * 1024 * 1024);
    }

    #[test]
    fn http_source_constructor_preserves_custom_headers() {
        let source = HttpRangeSource::with_http_headers(
            "https://example.invalid/video.mp4",
            vec![
                ("Authorization".to_string(), "Bearer test".to_string()),
                ("X-Playback-Session".to_string(), "session-123".to_string()),
            ],
        );
        assert_eq!(
            source.http_headers,
            vec![
                ("Authorization".to_string(), "Bearer test".to_string()),
                ("X-Playback-Session".to_string(), "session-123".to_string()),
            ]
        );
    }

    #[test]
    fn http_source_constructor_preserves_explicit_read_ahead() {
        let source = HttpRangeSource::with_http_headers_and_read_ahead(
            "https://example.invalid/video.mp4",
            Vec::new(),
            Some(16 * 1024 * 1024),
        );

        assert_eq!(source.read_ahead_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn http_source_new_starts_without_custom_headers() {
        let source = HttpRangeSource::new("https://example.invalid/video.mp4");

        assert!(source.http_headers.is_empty());
    }

    #[test]
    fn http_source_preserves_headers_without_normalizing_values() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer a+b/c==".to_string()),
            (
                "X-Client-Tag".to_string(),
                "  preserve whitespace  ".to_string(),
            ),
        ];
        let source = HttpRangeSource::with_http_headers(
            "https://example.invalid/video.mp4",
            headers.clone(),
        );

        assert_eq!(source.http_headers, headers);
    }

    #[test]
    fn content_range_total_parses_totals_and_rejects_unknown() {
        assert_eq!(parse_content_range_total("bytes 0-99/1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 100-199/200"), Some(200));
        assert_eq!(parse_content_range_total("bytes */555"), Some(555));
        assert_eq!(parse_content_range_total("bytes 0-99/*"), None);
        assert_eq!(parse_content_range_total("items 0-99/1234"), None);
        assert_eq!(parse_content_range_total(""), None);
    }

    #[test]
    fn length_probe_keeps_non_range_http_statuses_as_errors() {
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )]);
        let error = probe_http_total_length(&HttpIo::new(), &uri, &[]).unwrap_err();
        assert!(error.to_string().contains("404"));
    }

    #[test]
    fn http_range_accepts_status_200_for_nonzero_offset() {
        let body: Vec<u8> = (0..100).collect();
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_simple_response("200 OK", &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(100);
        let bytes = source
            .read_range(ByteRange {
                start: 10,
                length: Some(10),
            })
            .unwrap();
        assert_eq!(bytes, body[10..20]);
        assert_eq!(source.cache_bytes, body[10..]);
        assert!(recv_request_head(&requests).contains("range: bytes=10-99"));
    }

    fn http_200_without_length(body: &[u8], chunked: bool) -> Vec<u8> {
        let mut raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n".to_vec();
        if chunked {
            raw.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
            for chunk in body.chunks(7) {
                raw.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                raw.extend_from_slice(chunk);
                raw.extend_from_slice(b"\r\n");
            }
            raw.extend_from_slice(b"0\r\n\r\n");
        } else {
            raw.extend_from_slice(b"\r\n");
            raw.extend_from_slice(body);
        }
        raw
    }

    #[test]
    fn http_range_200_without_length_skips_prefix_and_learns_eof() {
        let body: Vec<u8> = (0..100).collect();
        for chunked in [false, true] {
            for start in [0, 10, 100, 120] {
                let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
                    http_200_without_length(&body, chunked),
                )]);
                let response = fetch_http_range(
                    &HttpIo::new(),
                    &uri,
                    &[],
                    ByteRange {
                        start,
                        length: Some(256),
                    },
                    "test_range",
                )
                .unwrap();
                assert_eq!(response.bytes, body[(start as usize).min(body.len())..]);
                assert_eq!(response.total_length, Some(body.len() as u64));
                assert!(recv_request_head(&requests).contains(&format!("range: bytes={start}-")));
            }
        }
    }

    #[test]
    fn http_range_200_buffers_only_the_requested_bytes() {
        let body: Vec<u8> = (0..192 * 1024).map(|index| (index % 251) as u8).collect();
        for raw in [
            http_simple_response("200 OK", &body),
            http_200_without_length(&body, false),
        ] {
            let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(raw)]);
            let response = fetch_http_range(
                &HttpIo::new(),
                &uri,
                &[],
                ByteRange {
                    start: 100 * 1024 + 3,
                    length: Some(17),
                },
                "test_range",
            )
            .unwrap();
            assert_eq!(response.bytes, body[100 * 1024 + 3..100 * 1024 + 20]);
        }
    }

    #[test]
    fn http_source_200_without_length_handles_reads_at_and_past_eof() {
        for start in [0, 10, 100, 120] {
            let body: Vec<u8> = (0..100).collect();
            let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
                http_200_without_length(&body, true),
            )]);
            let mut source = HttpRangeSource::new(uri);
            assert_eq!(
                source.read_range(ByteRange::suffix_from(start)).unwrap(),
                body[(start as usize).min(body.len())..],
            );
            assert_eq!(source.len().unwrap(), Some(100));
            assert!(
                source
                    .read_range(ByteRange {
                        start: 100,
                        length: Some(64)
                    })
                    .unwrap()
                    .is_empty()
            );
        }
    }

    fn http_200_with_validator(body: &[u8], validator: &str, declared_length: usize) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {declared_length}\r\nETag: {validator}\r\nConnection: close\r\n\r\n"
        ).into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    #[test]
    fn http_range_weak_etag_allows_reads_but_rejects_resuming_partial_bytes() {
        for start in [0, 10] {
            let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
                http_200_with_validator(&[b'a'; 100], "W/\"v1\"", 100),
            )]);
            let response = fetch_http_range(
                &HttpIo::new(),
                &uri,
                &[],
                ByteRange {
                    start,
                    length: Some(40),
                },
                "test_range",
            )
            .unwrap();
            assert_eq!(response.bytes, vec![b'a'; 40]);
            assert!(!recv_request_head(&requests).contains("if-range:"));

            let (uri, requests) = spawn_mock_http_server(vec![
                MockResponse::immediate(http_200_with_validator(&[b'a'; 20], "W/\"v1\"", 100)),
                MockResponse::immediate(http_200_with_validator(&[b'b'; 100], "W/\"v1\"", 100)),
            ]);
            let result = fetch_http_range(
                &HttpIo::new(),
                &uri,
                &[],
                ByteRange {
                    start,
                    length: Some(40),
                },
                "test_range",
            );
            assert!(matches!(result, Err(SourceError::HttpEntityChanged)));
            let _ = recv_request_head(&requests);
            assert!(
                requests.try_recv().is_err(),
                "weak ETags must not be sent in If-Range"
            );
        }
    }

    #[test]
    fn http_stream_entity_failure_blocks_fallback_and_survives_buffer_release() {
        for (validator, resumed) in [
            (
                "\"v1\"",
                http_200_with_validator(&[b'b'; 1024], "\"v2\"", 1024),
            ),
            ("\"v1\"", http_simple_response("200 OK", &[b'b'; 1024])),
            (
                "W/\"v1\"",
                http_200_with_validator(&[b'b'; 1024], "W/\"v1\"", 1024),
            ),
        ] {
            let mut responses = vec![MockResponse::immediate(http_200_with_validator(
                &[b'a'; 512],
                validator,
                1024,
            ))];
            responses.extend((0..8).map(|_| MockResponse::immediate(resumed.clone())));
            let (uri, requests) = spawn_mock_http_server(responses);
            let mut source = HttpRangeSource::new(uri);
            source.content_length = Some(1024);
            source.cache_bytes = vec![b'a'; 64];
            source.ensure_streams();

            let result = source.read_range(ByteRange {
                start: 0,
                length: Some(1024),
            });
            assert!(
                matches!(result, Err(SourceError::HttpEntityChanged)),
                "{result:?}"
            );
            assert!(recv_request_head(&requests).contains("range: bytes=64-"));
            if !validator.starts_with("W/") {
                let resumed = recv_request_head(&requests);
                assert!(resumed.contains("range: bytes=512-"));
                assert!(resumed.contains("if-range: \"v1\""));
            }
            // A cached read and a seek after releasing/replacing the worker
            // must retain the failure, without fetching the changed entity.
            assert!(matches!(
                source.read_range(ByteRange {
                    start: 0,
                    length: Some(1)
                }),
                Err(SourceError::HttpEntityChanged)
            ));
            source.release_buffer();
            assert!(matches!(
                source.read_range(ByteRange {
                    start: 700,
                    length: Some(1)
                }),
                Err(SourceError::HttpEntityChanged)
            ));
            assert!(
                requests.try_recv().is_err(),
                "entity failures must not retry or fall back"
            );
        }
    }

    #[test]
    fn http_range_200_retry_skips_prefix_and_preserves_received_bytes() {
        let body: Vec<u8> = (0..100).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_200_with_validator(&body[..20], "\"v1\"", 100)),
            MockResponse::immediate(http_200_with_validator(&body, "\"v1\"", 100)),
        ]);
        let response = fetch_http_range(
            &HttpIo::new(),
            &uri,
            &[],
            ByteRange {
                start: 10,
                length: Some(40),
            },
            "test_range",
        )
        .unwrap();
        assert_eq!(response.bytes, body[10..50]);
        assert_eq!(response.total_length, Some(100));
        assert!(recv_request_head(&requests).contains("range: bytes=10-49"));
        let resumed = recv_request_head(&requests);
        assert!(resumed.contains("range: bytes=20-49"));
        assert!(resumed.contains("if-range: \"v1\""));
    }

    #[test]
    fn http_range_200_retry_rejects_a_changed_or_missing_validator() {
        let body: Vec<u8> = (0..100).collect();
        for raw in [
            http_200_with_validator(&body, "\"v2\"", 100),
            http_simple_response("200 OK", &body),
        ] {
            let (uri, _requests) = spawn_mock_http_server(vec![
                MockResponse::immediate(http_200_with_validator(&body[..20], "\"v1\"", 100)),
                MockResponse::immediate(raw),
            ]);
            let error = fetch_http_range(
                &HttpIo::new(),
                &uri,
                &[],
                ByteRange {
                    start: 10,
                    length: Some(40),
                },
                "test_range",
            )
            .err()
            .expect("changed entities must not be spliced");
            assert!(error.to_string().contains("resource changed"));
        }
    }

    #[test]
    fn http_stream_200_skips_prefix_across_chunks_and_stripes() {
        let total = HTTP_STREAM_STRIPE_BYTES + 1024;
        let body: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
        let initial = 100 * 1024 + 3;
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(0, total, &body[..initial])),
            MockResponse::immediate(http_200_without_length(&body, false)),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        source.request_bytes = initial as u64;
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(initial as u64)
                })
                .unwrap(),
            body[..initial]
        );
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: initial as u64,
                    length: Some(total - initial as u64)
                })
                .unwrap(),
            body[initial..]
        );
        assert!(recv_request_head(&requests).contains(&format!("range: bytes=0-{}", initial - 1)));
        assert!(recv_request_head(&requests).contains(&format!("range: bytes={initial}-\r\n")));
        assert!(
            requests.try_recv().is_err(),
            "the persistent stream should cover the tail"
        );
    }

    #[test]
    fn http_stream_200_retry_preserves_a_partial_stripe() {
        let body: Vec<u8> = (0..1024).map(|index| (index % 251) as u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(0, 1024, &body[..64])),
            MockResponse::immediate(http_200_with_validator(&body[..512], "\"v1\"", 1024)),
            MockResponse::immediate(http_200_with_validator(&body, "\"v1\"", 1024)),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(1024);
        source.request_bytes = 64;
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64)
                })
                .unwrap(),
            body[..64]
        );
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 64,
                    length: Some(960)
                })
                .unwrap(),
            body[64..]
        );
        let _ = recv_request_head(&requests);
        assert!(recv_request_head(&requests).contains("range: bytes=64-\r\n"));
        let resumed = recv_request_head(&requests);
        assert!(resumed.contains("range: bytes=512-\r\n"));
        assert!(resumed.contains("if-range: \"v1\""));
    }

    #[test]
    fn http_stream_200_retry_rejects_a_changed_entity() {
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_200_with_validator(b"new body", "\"v2\"", 8),
        )]);
        let result = open_stream_response(&HttpIo::new(), &uri, &[], 4, Some("\"v1\"".to_string()));
        assert!(matches!(result, Err(SourceError::HttpEntityChanged)));
    }

    #[test]
    fn http_lengthless_200_supports_ffmpeg_custom_avio_reads_and_seek() {
        let body = include_bytes!("../testdata/playback/playback-fixture.mkv");
        let (uri, requests) = spawn_mock_http_server(
            (0..32)
                .map(|_| MockResponse::immediate(http_200_without_length(body, false)))
                .collect(),
        );
        let source =
            HttpRangeSource::with_http_headers_and_read_ahead(uri, Vec::new(), Some(64 * 1024));
        let mut demuxer = crate::ffmpeg::Demuxer::open_source(Box::new(source)).unwrap();
        assert_eq!(demuxer.probe().video.len(), 1);
        let mut packets = 0;
        while demuxer.read_packet().unwrap().is_some() {
            packets += 1;
        }
        assert!(packets >= 240);
        demuxer.seek(Duration::from_secs(2)).unwrap();
        assert!(demuxer.read_packet().unwrap().is_some());
        assert!(
            requests
                .try_iter()
                .any(|head| { head.to_lowercase().contains("range: bytes=65536-") }),
            "FFmpeg must read past the first capped whole-file response"
        );
    }

    /// Serves `body` by Range (bounded and open-ended), clamping every answer
    /// that would reach past `empty_from`, and answering any request that
    /// starts at or beyond it the way the origin in issue #163 did: a 206
    /// with a correct Content-Range and Content-Length but zero body bytes
    /// before the connection closes.
    fn spawn_fixture_range_server(
        body: Arc<Vec<u8>>,
        empty_from: u64,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let heads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let heads_seen = Arc::clone(&heads);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                heads_seen.lock().unwrap().push(head.clone());
                let total = body.len() as u64;
                if head.starts_with("HEAD") {
                    // Metadata answers report the real resource size; only the
                    // body serving is broken near the tail.
                    let _ = stream.write_all(
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n").as_bytes(),
                    );
                    continue;
                }
                let (start, requested_end) = parse_range_head(&head, total);
                // An injected answer promises exactly the requested range and
                // delivers nothing; every other answer is clamped so the tail
                // stays reachable only through an injected request.
                let injected = start >= empty_from;
                let end = if injected {
                    requested_end
                } else {
                    requested_end.min(empty_from - 1)
                };
                let length = end.saturating_sub(start) + 1;
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(response.as_bytes()).is_err() {
                    continue;
                }
                if injected {
                    // Declared but never delivered: the injected failure.
                    continue;
                }
                let slice = &body[start as usize..=end as usize];
                let _ = stream.write_all(slice);
                let _ = stream.flush();
            }
        });
        (uri, heads)
    }

    #[test]
    fn http_tail_empty_range_injection_ends_playback_cleanly() {
        // End-to-end shape of issue #163: an origin that cannot deliver the
        // final bytes of the resource. Before the tail rule the demuxer
        // turned those answers into a fatal av_read_frame EIO; the read must
        // end the stream at EOF instead, after playing to its natural end.
        let body = Arc::new(include_bytes!("../testdata/playback/playback-fixture.mkv").to_vec());
        let total = body.len() as u64;
        let empty_from = total - 4096;
        let (uri, heads) = spawn_fixture_range_server(Arc::clone(&body), empty_from);
        let source =
            HttpRangeSource::with_http_headers_and_read_ahead(uri, Vec::new(), Some(64 * 1024));
        let mut demuxer = crate::ffmpeg::Demuxer::open_source(Box::new(source)).unwrap();
        assert_eq!(demuxer.probe().video.len(), 1);
        let mut packets = 0;
        while demuxer.read_packet().unwrap().is_some() {
            packets += 1;
        }
        assert!(
            packets >= 240,
            "playback must reach its natural end, got {packets} packets"
        );
        let heads_snapshot = heads.lock().unwrap().clone();
        let saw_injected_request = heads_snapshot.iter().any(|head| {
            head.to_lowercase()
                .lines()
                .find(|line| line.starts_with("range:"))
                .and_then(|line| line.split_once("bytes="))
                .and_then(|(_, spec)| spec.split('-').next())
                .and_then(|start| start.trim().parse::<u64>().ok())
                .is_some_and(|start| start >= empty_from)
        });
        assert!(
            saw_injected_request,
            "the reproduction must actually exercise the injected empty tail; requests: {heads_snapshot:?}"
        );
    }

    #[test]
    fn http_range_accepts_status_200_for_whole_file_and_backfills_total() {
        let body = b"whole-file-payload".to_vec();
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_simple_response("200 OK", &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(
            source.read_range(ByteRange::suffix_from(0)).unwrap(),
            body.clone()
        );
        // Content-Length of the 200 response backfills the total without HEAD.
        assert_eq!(source.len().unwrap(), Some(body.len() as u64));
    }

    #[test]
    fn http_range_backfills_total_from_206_content_range() {
        let body = vec![b'x'; 16];
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, 4096, &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.read_range(ByteRange::suffix_from(0)).unwrap(), body);
        // The 206 Content-Range total satisfies len() without a HEAD request.
        assert_eq!(source.len().unwrap(), Some(4096));
    }

    #[test]
    fn http_range_caps_bodies_that_overrun_the_requested_length() {
        // Answers bytes 0-63/4096 to a bytes=0-7 request: the excess must be
        // discarded instead of buffered, or a Range-ignoring origin (which
        // sends the whole file for offset 0) would balloon the cache. The
        // 8-byte read-ahead window keeps the wire request exactly the 8 bytes
        // the read asked for (fetch_length would otherwise widen it).
        let mut raw =
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-63/4096\r\nContent-Length: 64\r\nConnection: close\r\n\r\n"
                .to_vec();
        raw.extend_from_slice(&[b'x'; 64]);
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(raw)]);
        let mut source =
            HttpRangeSource::with_http_headers_and_window(uri, Vec::new(), Some(8), None);
        source.content_length = Some(4096);
        let fetched = source
            .read_range(ByteRange {
                start: 0,
                length: Some(8),
            })
            .unwrap();
        assert_eq!(fetched, vec![b'x'; 8]);
        assert!(recv_request_head(&requests).contains("range: bytes=0-7"));
    }

    #[test]
    fn http_range_rejects_206_without_content_range_on_a_resumed_offset() {
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 8\r\nConnection: close\r\n\r\n01234567"
                .to_vec(),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(4096);
        let error = source
            .read_range(ByteRange {
                start: 32,
                length: Some(8),
            })
            .expect_err("a 206 without Content-Range cannot prove the served offset");
        assert!(
            matches!(
                &error,
                SourceError::HttpRangeVerification(message) if message.contains("Content-Range")
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn length_probe_caches_an_absent_content_length() {
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(),
        )]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), None);
        assert_eq!(source.len().unwrap(), None);
        assert!(
            requests.try_recv().is_ok(),
            "the first len() probes the origin"
        );
        assert!(
            requests.try_recv().is_err(),
            "a finished probe must not re-issue HEAD for every piece"
        );
    }

    #[test]
    fn http_range_retries_after_server_error() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_simple_response("500 Internal Server Error", b"boom")),
            MockResponse::immediate(http_206_response(0, 64, &body)),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64),
                })
                .unwrap(),
            body
        );
        assert!(recv_request_head(&requests).contains("range: bytes=0-63"));
        assert!(recv_request_head(&requests).contains("range: bytes=0-63"));
    }

    #[test]
    fn http_range_resumes_truncated_body_from_received_offset() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            // Promises 64 bytes but closes after 32: a body-phase error.
            MockResponse::immediate(http_206_truncated_response(0, 64, 64, &body[..32])),
            MockResponse::immediate(http_206_response(32, 64, &body[32..])),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64),
                })
                .unwrap(),
            body
        );
        assert!(recv_request_head(&requests).contains("range: bytes=0-63"));
        // The resumed request must start where the truncated body stopped.
        assert!(recv_request_head(&requests).contains("range: bytes=32-63"));
    }

    /// A 206 whose Content-Length frames an empty body cleanly (unlike
    /// `http_206_truncated_response`, which promises bytes and closes).
    fn http_206_clean_empty_response(start: u64, end: u64, total: u64) -> Vec<u8> {
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn http_tail_flush_empty_206_body_reads_as_eof() {
        // Issue #163: a window that runs to the resource total is boundary
        // traffic. An origin answering it with a clean empty 206 marks the
        // resource's end, and the read must surface EOF, not a fatal fetch
        // failure.
        let total = 8 * 1024 * 1024u64;
        let start = total - 4096;
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_clean_empty_response(start, total - 1, total),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        let bytes = source
            .read_range(ByteRange {
                start,
                length: Some(4096),
            })
            .unwrap();
        assert!(bytes.is_empty(), "a tail-flush empty answer is EOF");
        assert!(recv_request_head(&requests).contains(&format!("range: bytes={start}-")));
    }

    #[test]
    fn http_tail_flush_empty_206_body_reads_as_eof_when_the_response_reveals_the_total() {
        // An origin whose length probe finds nothing: the empty tail response
        // is the first and only source of the total, and the boundary rule
        // must still hold -- the window's reach is judged after the fetch,
        // once the response has named the resource size.
        let total = 8 * 1024 * 1024u64;
        let start = total - 4096;
        let (uri, _requests) = spawn_mock_http_server(vec![
            // HEAD: no usable length (Content-Length: 0 triggers the probe).
            MockResponse::immediate(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ),
            // Probe: Range ignored, and no Content-Length either.
            MockResponse::immediate(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec()),
            MockResponse::immediate(http_206_clean_empty_response(start, total - 1, total)),
        ]);
        let mut source = HttpRangeSource::new(uri);
        let bytes = source
            .read_range(ByteRange {
                start,
                length: Some(4096),
            })
            .unwrap();
        assert!(bytes.is_empty(), "a tail-flush empty answer is EOF");
        assert_eq!(
            source.len().unwrap(),
            Some(total),
            "the response supplies the total"
        );
    }

    #[test]
    fn http_midfile_empty_206_body_is_still_an_error() {
        // 576c388's rule stands outside the tail: an empty answer for a
        // window that ends short of the resource total is a fetch failure,
        // and bytes that exist must never look like EOF.
        let total = 8 * 1024 * 1024u64;
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_clean_empty_response(0, 2 * 1024 * 1024 - 1, total),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        let error = source
            .read_range(ByteRange {
                start: 0,
                length: Some(1024),
            })
            .expect_err("an empty mid-file answer must not look like EOF");
        assert!(
            matches!(&error, SourceError::Http(message) if message.contains("empty body")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn http_tail_flush_zero_byte_truncated_body_reads_as_eof() {
        // The injection from issue #163's reproduction: a 206 that declares
        // the tail's Content-Length but delivers zero bytes and closes. The
        // fetch exhausts its retries; the tail-flush window then ends the
        // read as EOF instead of failing the session.
        let total = 8 * 1024 * 1024u64;
        let start = total - 2048;
        let responses = (0..HTTP_FETCH_MAX_ATTEMPTS)
            .map(|_| {
                MockResponse::immediate(http_206_truncated_response(
                    start,
                    total,
                    (total - start) as usize,
                    &[],
                ))
            })
            .collect();
        let (uri, requests) = spawn_mock_http_server(responses);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        let bytes = source
            .read_range(ByteRange {
                start,
                length: Some(2048),
            })
            .unwrap();
        assert!(bytes.is_empty(), "a tail-flush undelivered range is EOF");
        for _ in 0..HTTP_FETCH_MAX_ATTEMPTS {
            assert!(recv_request_head(&requests).contains(&format!("range: bytes={start}-")));
        }
    }

    #[test]
    fn http_midfile_truncated_body_exhausting_retries_is_still_an_error() {
        // The transport-failure downgrade is tail-only: exhausted retries on
        // a mid-file window still surface their error.
        let total = 8 * 1024 * 1024u64;
        let responses = (0..HTTP_FETCH_MAX_ATTEMPTS)
            .map(|_| {
                MockResponse::immediate(http_206_truncated_response(0, total, 2 * 1024 * 1024, &[]))
            })
            .collect();
        let (uri, _requests) = spawn_mock_http_server(responses);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        let error = source
            .read_range(ByteRange {
                start: 0,
                length: Some(1024),
            })
            .expect_err("mid-file transport failures must stay errors");
        assert!(
            matches!(&error, SourceError::Http(_)),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn http_stream_read_at_the_frontier_waits_without_a_duplicate_request() {
        // The reader catches up to the delivery frontier: it waits for the
        // streams instead of re-requesting bytes that are already on the
        // wire, and the server never sees a second, overlapping GET.
        let total = 64 * 1024 * 1024u64;
        let (uri, _in_flight, _max, heads) = spawn_concurrent_mock_http_server(total);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);
        source.cache_start = 0;
        source.cache_bytes = vec![b'c'; 1024 * 1024];
        source.ensure_streams();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && heads.lock().unwrap().is_empty() {
            thread::sleep(Duration::from_millis(10));
        }

        let bytes = source
            .read_range(ByteRange {
                start: 1024 * 1024 - 1024,
                length: Some(2048),
            })
            .unwrap();
        assert_eq!(bytes.len(), 2048);
        assert!(bytes[..1024].iter().all(|byte| *byte == b'c'));
        assert!(
            bytes[1024..]
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == ((1024 * 1024 + index) % 251) as u8)
        );
        // The only GET is the open-ended worker stream; the straddling
        // read produced no request at all.
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 1, "heads: {heads:?}");
        for head in heads.iter() {
            let lower = head.to_lowercase();
            let range_line = lower
                .lines()
                .find(|line| line.starts_with("range:"))
                .unwrap_or_default();
            assert!(
                range_line.trim_end().ends_with('-'),
                "worker GETs must be open-ended: {range_line}"
            );
        }
    }

    #[test]
    fn http_read_past_the_request_cap_is_still_covered_by_the_streams() {
        // A read that starts inside the window but ends past `cache_end +
        // HTTP_REQUEST_MAX_BYTES` must be served by the streaming workers too.
        // The wait was keyed off the read's *end* while the re-anchor was keyed
        // off its *start*, so this read skipped the wait and `fetch_missing`
        // re-downloaded the gap from `cache_end` -- the very range the workers
        // were delivering, which `drain_stripes` then discarded as overlap.
        // The origin saw the bytes twice and the demuxer thread blocked for the
        // whole synchronous fill.
        let total = 64 * 1024 * 1024u64;
        let (uri, _in_flight, _max, heads) = spawn_concurrent_mock_http_server(total);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);
        source.cache_start = 0;
        source.cache_bytes = vec![b'c'; 1024 * 1024];
        source.ensure_streams();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && heads.lock().unwrap().is_empty() {
            thread::sleep(Duration::from_millis(10));
        }

        // Starts at the cache end and runs 8 MiB: two request caps past it.
        let read_start = 1024 * 1024u64;
        let read_length = 8 * 1024 * 1024u64;
        let bytes = source
            .read_range(ByteRange {
                start: read_start,
                length: Some(read_length),
            })
            .expect("the streams must cover a read that starts inside the window");
        assert_eq!(bytes.len() as u64, read_length);
        for (index, byte) in bytes.iter().enumerate() {
            assert_eq!(*byte, ((read_start + index as u64) % 251) as u8);
        }

        // Only the open-ended worker GET: a closed-range request here
        // means the read was re-downloaded instead of waited for.
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 1, "heads: {heads:?}");
        for head in heads.iter() {
            let lower = head.to_lowercase();
            let range_line = lower
                .lines()
                .find(|line| line.starts_with("range:"))
                .unwrap_or_default();
            assert!(
                range_line.trim_end().ends_with('-'),
                "a closed-range request means the read was re-downloaded: {range_line}"
            );
        }
    }

    #[test]
    fn http_stream_worker_failure_falls_back_to_synchronous_fetch() {
        let total = 64 * 1024 * 1024u64;
        let (uri, heads) = spawn_flaky_stream_server(total);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);
        source.cache_start = 0;
        source.cache_bytes = (0..1024 * 1024u64)
            .map(|offset| (offset % 251) as u8)
            .collect();
        source.ensure_streams();

        // The workers fail (no body byte ever arrives), but the read must
        // not: the synchronous capped path serves the gap, position-exact.
        let read_start = 1024 * 1024u64;
        let bytes = source
            .read_range(ByteRange {
                start: read_start,
                length: Some(1024),
            })
            .expect("a failed stream must degrade to the synchronous path");
        assert_eq!(bytes.len(), 1024);
        for (index, byte) in bytes.iter().enumerate() {
            assert_eq!(*byte, ((read_start + index as u64) % 251) as u8);
        }
        assert!(
            source.prefetch_failures >= 1,
            "the worker failure must be counted"
        );
        let heads = heads.lock().unwrap();
        assert!(
            heads
                .iter()
                .any(|head| head.to_lowercase().contains("range: bytes=1048576-")),
            "the sync fallback must be a bounded GET: {heads:?}"
        );
    }

    #[test]
    fn http_short_stream_response_resumes_with_entity_validator() {
        let start = 1024 * 1024;
        let split = start + 256 * 1024;
        let total = 2 * 1024 * 1024 + 123;
        let expected: Vec<u8> = (start..total).map(|offset| (offset % 251) as u8).collect();
        let mut first = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{total}\r\nContent-Length: {}\r\nETag: \"version-1\"\r\nConnection: close\r\n\r\n",
            split - 1, split - start,
        ).into_bytes();
        first.extend_from_slice(&expected[..(split - start) as usize]);
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(first),
            MockResponse::immediate(http_206_response(
                split,
                total,
                &expected[(split - start) as usize..],
            )),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        source.cache_bytes = vec![0; start as usize];
        source.ensure_streams();
        let bytes = source
            .read_range(ByteRange {
                start,
                length: Some(total - start + 1024),
            })
            .unwrap();
        assert_eq!(bytes, expected);
        let first = recv_request_head(&requests).to_lowercase();
        assert!(first.contains(&format!("range: bytes={start}-\r\n")));
        let resumed = recv_request_head(&requests).to_lowercase();
        assert!(resumed.contains(&format!("range: bytes={split}-\r\n")));
        assert!(resumed.contains("if-range: \"version-1\""));
    }

    #[test]
    fn http_empty_stream_responses_exhaust_retries_and_fall_back() {
        let start = 1024 * 1024;
        let total = 8 * 1024 * 1024;
        let expected: Vec<u8> = (start..start + 2 * 1024 * 1024)
            .map(|offset| (offset % 251) as u8)
            .collect();
        let mut responses: Vec<_> = (0..=HTTP_STREAM_MAX_RESUMES)
            .map(|_| MockResponse::immediate(http_simple_response("206 Partial Content", b"")))
            .collect();
        responses.push(MockResponse::immediate(http_206_response(
            start, total, &expected,
        )));
        let (uri, requests) = spawn_mock_http_server(responses);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        source.cache_bytes = vec![0; start as usize];
        source.ensure_streams();
        let began = Instant::now();
        let bytes = source
            .read_range(ByteRange {
                start,
                length: Some(1024),
            })
            .unwrap();
        assert_eq!(bytes, expected[..1024]);
        assert!(began.elapsed() < Duration::from_secs(5));
        for _ in 0..=HTTP_STREAM_MAX_RESUMES {
            assert!(recv_request_head(&requests).contains(&format!("range: bytes={start}-\r\n")));
        }
        assert!(recv_request_head(&requests).contains("range: bytes=1048576-3145727\r\n"));
    }

    #[test]
    fn http_stream_to_eof_reads_past_eight_pieces() {
        // An open-ended read (danmaku/subtitle sidecar) larger than one read's
        // fetch allowance must still reach EOF. The first rewrite of this path
        // capped the loop at 8 x 4 MiB = 32 MiB and truncated silently.
        let chunks = 9u64;
        let total = chunks * HTTP_REQUEST_MAX_BYTES;
        let responses = (0..chunks)
            .map(|index| {
                MockResponse::immediate(http_206_response(
                    index * HTTP_REQUEST_MAX_BYTES,
                    total,
                    &vec![b'x'; HTTP_REQUEST_MAX_BYTES as usize],
                ))
            })
            .collect();
        let (uri, requests) = spawn_mock_http_server(responses);
        let mut source = HttpRangeSource::new(uri);

        let bytes = source.read_range(ByteRange::suffix_from(0)).unwrap();
        assert_eq!(
            bytes.len() as u64,
            total,
            "an open-ended read must reach EOF, not stop at the attempt cap"
        );
        for index in 0..chunks {
            let head = recv_request_head(&requests);
            assert!(
                head.contains(&format!("range: bytes={}-", index * HTTP_REQUEST_MAX_BYTES)),
                "request {index}: {head}"
            );
        }
    }

    #[test]
    fn http_short_pieces_still_cover_the_read() {
        // An origin that caps every body at 256 KiB needs many pieces to cover a
        // gap. Walking away would hand the caller an empty read, which the AVIO
        // layer turns into EOF: bytes that exist must never look like the end of
        // the resource.
        let total = 8 * 1024 * 1024u64;
        let piece = 256 * 1024u64;
        // A range-aware origin that caps every answer at 256 KiB.
        let (uri, _requests) = spawn_drip_http_server(total, 0, Some(piece), 40);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);

        // Warm the cache with the first short piece, then read just inside the
        // 4 MiB forward-gap guard: covering it takes ~15 more pieces.
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(1024)
                })
                .unwrap()
                .len(),
            1024
        );
        // A later worker used to append its far-away short response here,
        // disguising a hole as contiguous bytes. Check the first gap itself.
        let gap_start = 2 * piece;
        let gap = source
            .read_range(ByteRange {
                start: gap_start,
                length: Some(1024),
            })
            .unwrap();
        assert!(
            gap.iter()
                .enumerate()
                .all(|(index, byte)| *byte == ((gap_start + index as u64) % 251) as u8)
        );
        let target = HTTP_REQUEST_MAX_BYTES;
        let bytes = source
            .read_range(ByteRange {
                start: target,
                length: Some(1024),
            })
            .expect("short pieces must still cover the read");
        assert_eq!(bytes.len(), 1024, "a covered read must not look like EOF");
        assert!(
            bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == ((target + index as u64) % 251) as u8)
        );
    }

    #[test]
    fn http_caller_requests_are_capped_too() {
        let total = 64 * 1024 * 1024u64;
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, total, &vec![b'a'; 4096]),
        )]);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);

        // No cache yet, so the window is anchored at the read -- and a 32 MiB
        // caller request must not become a 32 MiB body (that is the issue #1
        // shape). The caller sees a short read and asks again.
        let bytes = source
            .read_range(ByteRange {
                start: 0,
                length: Some(32 * 1024 * 1024),
            })
            .unwrap();
        assert_eq!(bytes.len(), 4096);
        let head = recv_request_head(&requests);
        assert!(
            head.contains("range: bytes=0-4194303"),
            "request head: {head}"
        );
    }

    #[test]
    fn http_trim_never_drops_the_current_read_position() {
        let mut source = HttpRangeSource::new("https://example.invalid/video.mp4");
        source.cache_start = 0;
        source.cache_bytes = vec![b'c'; 25 * 1024 * 1024];

        // A read starting at 0 may not have its own start trimmed away, however
        // large it is.
        source.trim_cache(ByteRange {
            start: 0,
            length: Some(20 * 1024 * 1024),
        });
        assert_eq!(source.cache_start, 0);

        // A read near the cache end trims the head down to the retention budget.
        source.trim_cache(ByteRange {
            start: 25 * 1024 * 1024,
            length: Some(1024),
        });
        assert_eq!(
            source.cache_start,
            25 * 1024 * 1024 - HTTP_CACHE_RETAIN_BYTES
        );
    }

    #[test]
    fn http_back_buffer_budget_is_honored_by_trim() {
        // The rewind budget is host-tunable: a host sizing it from media
        // bitrate (a -10 s skip at 71 Mbps covers ~89 MB) must see the trim
        // honor the larger tail instead of the 16 MiB default.
        let mut source = HttpRangeSource::with_http_headers_and_window(
            "https://example.invalid/video.mp4",
            Vec::new(),
            None,
            Some(96 * 1024 * 1024),
        );
        source.cache_start = 0;
        source.cache_bytes = vec![b'c'; 128 * 1024 * 1024];

        source.trim_cache(ByteRange {
            start: 128 * 1024 * 1024,
            length: Some(1024),
        });
        assert_eq!(
            source.cache_start,
            128 * 1024 * 1024 - 96 * 1024 * 1024,
            "the tail must be trimmed to the configured back-buffer budget"
        );
    }

    #[test]
    fn http_stream_spawns_one_open_ended_worker() {
        let total = 64 * 1024 * 1024;
        let (uri, _in_flight, _max, heads) = spawn_concurrent_mock_http_server(total);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);
        source.cache_bytes = vec![b'c'; 20 * 1024 * 1024];
        source.ensure_streams();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && heads.lock().unwrap().is_empty() {
            thread::sleep(Duration::from_millis(10));
        }
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 1, "heads: {heads:?}");
        assert!(
            heads[0]
                .to_lowercase()
                .contains("range: bytes=20971520-\r\n")
        );
    }

    #[test]
    fn dropping_http_source_interrupts_blocked_stream_worker() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let (request_sender, request_receiver) = mpsc::channel();
        let (closed_sender, closed_receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-8388607/8388608\r\nContent-Length: 8388607\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            stream.flush().unwrap();
            request_sender.send(()).unwrap();

            let mut byte = [0u8; 1];
            let closed = reader.read(&mut byte).unwrap_or(0) == 0;
            closed_sender.send(closed).unwrap();
        });

        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(8 * 1024 * 1024);
        source.cache_bytes = vec![0];
        source.ensure_streams();
        request_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("stream worker should reach the blocked response body");

        let started = Instant::now();
        drop(source);
        assert!(
            closed_receiver
                .recv_timeout(Duration::from_millis(500))
                .expect("dropping the source must close the worker socket"),
            "worker socket should close cleanly"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "source drop took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn cancelling_http_source_interrupts_foreground_body_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let (request_sender, request_receiver) = mpsc::channel();
        let (closed_sender, closed_receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-8388607/8388608\r\nContent-Length: 8388608\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            stream.flush().unwrap();
            request_sender.send(()).unwrap();
            let mut byte = [0u8; 1];
            closed_sender
                .send(reader.read(&mut byte).unwrap_or(0) == 0)
                .unwrap();
        });

        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(8 * 1024 * 1024);
        let cancellation = source.cancellation().unwrap();
        let (result_sender, result_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = source.read_range(ByteRange {
                start: 0,
                length: Some(1024),
            });
            result_sender.send(result).unwrap();
        });
        request_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("foreground request should reach the blocked response body");

        let started = Instant::now();
        cancellation.cancel();
        assert!(matches!(
            result_receiver.recv_timeout(Duration::from_millis(500)),
            Ok(Err(SourceError::Cancelled))
        ));
        assert!(
            closed_receiver
                .recv_timeout(Duration::from_millis(500))
                .expect("cancellation must close the foreground socket")
        );
        worker.join().unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn cancellation_wakes_a_reader_waiting_for_prefetch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/media", listener.local_addr().unwrap());
        let (started_tx, started_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-999/1000\r\nContent-Length: 999\r\nConnection: close\r\n\r\n").unwrap();
            started_tx.send(()).unwrap();
            assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        });
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(1000);
        source.cache_bytes = vec![0];
        source.ensure_streams();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let cancellation = source.cancellation().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let result = source.read_range(ByteRange {
                start: 1,
                length: Some(10),
            });
            let _ = done_tx.send(result);
        });
        // Let the foreground reader enter the prefetch condition-variable wait.
        thread::sleep(Duration::from_millis(50));
        cancellation.cancel();
        assert!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(500)),
                Ok(Err(SourceError::Cancelled))
            ),
            "cancel must wake the prefetch consumer as well as its socket"
        );
        reader.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn http_deep_window_against_a_slow_origin_still_serves_the_read() {
        // The issue #1 shape: a 32 MiB window against an origin that can only
        // deliver ~900 KB/s. Before the request cap this asked for all 32 MiB in
        // one body, which the client's 15 s body deadline killed
        // (`timeout: receive response` -> EIO -> playback ended). A capped 4 MiB
        // request finishes in about five seconds.
        let total = 64 * 1024 * 1024u64;
        let (uri, requests) = spawn_drip_http_server(total, 900 * 1024, None, 2);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);

        let started = Instant::now();
        let bytes = source
            .read_range(ByteRange {
                start: 0,
                length: Some(1024),
            })
            .expect("a deep window must not turn a slow origin into a failed read");
        assert_eq!(bytes.len(), 1024);
        // The discriminating failure is the `expect` above (the old shape errors
        // out at the 15 s body deadline); this bound only catches a fetch that
        // silently stopped making progress, so it is sized for a loaded machine.
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "first read took {:?}",
            started.elapsed()
        );
        let head = recv_request_head(&requests);
        assert!(
            head.contains("range: bytes=0-4194303"),
            "request head: {head}"
        );
    }

    #[test]
    fn http_resumes_more_than_three_times_while_bytes_arrive() {
        // Four pieces, each on its own connection: three truncated bodies, then
        // the rest. Every attempt moves the resume point, so the fetch has to
        // keep going -- the flat three-attempt rule used to end playback here
        // (a capped request on a slow origin legitimately spans several
        // attempts, each cut short by the 15 s body deadline).
        let total = 4 * 1024 * 1024u64;
        let piece = 1024 * 1024usize;
        let held = vec![b'a'; piece];
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_truncated_response(
                0,
                total,
                4 * 1024 * 1024,
                &held,
            )),
            MockResponse::immediate(http_206_truncated_response(
                1024 * 1024,
                total,
                3 * 1024 * 1024,
                &held,
            )),
            MockResponse::immediate(http_206_truncated_response(
                2 * 1024 * 1024,
                total,
                2 * 1024 * 1024,
                &held,
            )),
            MockResponse::immediate(http_206_response(3 * 1024 * 1024, total, &held)),
        ]);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(8 * 1024 * 1024),
        );
        source.content_length = Some(total);

        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(1024)
                })
                .unwrap()
                .len(),
            1024
        );
        let first = recv_request_head(&requests);
        assert!(first.contains("range: bytes=0-4194303"), "{first}");
        let second = recv_request_head(&requests);
        assert!(second.contains("range: bytes=1048576-4194303"), "{second}");
        let third = recv_request_head(&requests);
        assert!(third.contains("range: bytes=2097152-4194303"), "{third}");
        // A fourth attempt: the old gate stopped at three, no matter how much
        // each attempt had already delivered.
        let fourth = recv_request_head(&requests);
        assert!(fourth.contains("range: bytes=3145728-4194303"), "{fourth}");
    }

    #[test]
    fn http_request_body_is_capped_regardless_of_the_read_ahead_window() {
        let total = 64 * 1024 * 1024u64;
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, total, &vec![b'a'; 4096]),
        )]);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);
        assert_eq!(source.read_ahead_bytes, 32 * 1024 * 1024);
        assert_eq!(source.request_bytes, HTTP_REQUEST_MAX_BYTES);

        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(1024)
                })
                .unwrap()
                .len(),
            1024
        );
        // A 32 MiB window used to go out as one 32 MiB request (~18 Mbps at the
        // 15 s body deadline, i.e. an issue #1 failure on anything slower). It
        // must now be a capped request.
        let head = recv_request_head(&requests);
        assert!(
            head.contains("range: bytes=0-4194303"),
            "request head: {head}"
        );
    }

    #[test]
    fn http_stream_downloads_each_byte_once() {
        let total = 32 * 1024 * 1024;
        let (uri, in_flight, _max, heads, sent) = spawn_counted_http_server(total);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);
        let mut offset = 0;
        while offset < total {
            let bytes = source
                .read_range(ByteRange {
                    start: offset,
                    length: Some(64 * 1024),
                })
                .unwrap();
            assert!(!bytes.is_empty());
            assert!(
                bytes
                    .iter()
                    .enumerate()
                    .all(|(index, byte)| *byte == ((offset + index as u64) % 251) as u8)
            );
            offset += bytes.len() as u64;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(sent.load(Ordering::SeqCst) as u64, total);
        let heads = heads.lock().unwrap();
        assert_eq!(
            heads.len(),
            2,
            "one initial fetch and one persistent stream: {heads:?}"
        );
    }

    #[test]
    fn http_final_partial_read_does_not_wait_past_eof() {
        let total = 3 * 1024 * 1024 + 123;
        let (uri, _in_flight, _max, _heads) = spawn_concurrent_mock_http_server(total);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        source
            .read_range(ByteRange {
                start: 0,
                length: Some(64 * 1024),
            })
            .unwrap();
        let started = Instant::now();
        let bytes = source
            .read_range(ByteRange {
                start: total - 123,
                length: Some(64 * 1024),
            })
            .unwrap();
        assert_eq!(bytes.len(), 123);
        assert!(
            bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == ((total - 123 + index as u64) % 251) as u8)
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "tail read stalled {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn http_stream_gap_falls_back_without_splicing_wrong_bytes() {
        let total = 8 * 1024 * 1024;
        let (uri, _in_flight, _max, _heads) = spawn_concurrent_mock_http_server(total);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        source.cache_bytes = (0..64 * 1024).map(|offset| (offset % 251) as u8).collect();
        source.streams = Some(StreamSession {
            shared: Arc::new(StreamShared {
                inner: Mutex::new(StreamInner {
                    epoch: 1,
                    pending: BTreeMap::from([(
                        0,
                        StripeHandoff {
                            start: 128 * 1024,
                            bytes: vec![255; 64 * 1024],
                        },
                    )]),
                    worker_done: true,
                    worker_failed: false,
                    failure_acked: false,
                    progress_bytes: 64 * 1024,
                    last_progress: Instant::now(),
                    window_end: total,
                    stopped: false,
                }),
                signal: Condvar::new(),
                entity_changed: Arc::clone(&source.entity_changed),
            }),
            io: HttpIo::new(),
            worker: None,
        });
        let started = Instant::now();
        let bytes = source
            .read_range(ByteRange {
                start: 64 * 1024,
                length: Some(1024),
            })
            .unwrap();
        assert_eq!(bytes.len(), 1024);
        assert!(
            bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == ((64 * 1024 + index) % 251) as u8)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn http_stream_delivers_contiguous_bytes_across_stripes() {
        // One read crossing two delivered stripes: the appends must land in
        // stripe order (frontier semantics) and hand back exactly the
        // position-dependent bytes.
        let total = 64 * 1024 * 1024u64;
        let (uri, _in_flight, _max, _heads) = spawn_concurrent_mock_http_server(total);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);
        source.cache_start = 0;
        source.cache_bytes = (0..1024 * 1024u64)
            .map(|offset| (offset % 251) as u8)
            .collect();

        let read_start = 1024 * 1024u64;
        let read_len = 8 * 1024 * 1024u64;
        let bytes = source
            .read_range(ByteRange {
                start: read_start,
                length: Some(read_len),
            })
            .expect("a read across stream stripes must be served");
        assert_eq!(bytes.len() as u64, read_len);
        for (index, byte) in bytes.iter().enumerate() {
            assert_eq!(
                *byte,
                ((read_start + index as u64) % 251) as u8,
                "byte {} of the read",
                read_start + index as u64
            );
        }
    }

    #[test]
    fn http_stream_far_forward_read_reanchors_without_the_stall_wait() {
        // A read far beyond the window (a demuxer probing the tail, a large
        // forward seek) can never be covered by the streams: the workers stop
        // at the read-ahead boundary. It must skip the coverage wait and
        // re-anchor immediately, not burn the full HTTP_STREAM_STALL clock on
        // a wait that cannot succeed. The window is 8 MiB (two stripe budgets
        // in flight), so any far-forward read sees the workers paused.
        let total = 128 * 1024 * 1024u64;
        let (uri, _in_flight, _max_in_flight, heads) = spawn_concurrent_mock_http_server(total);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(8 * 1024 * 1024),
        );
        source.content_length = Some(total);

        // The first read anchors the window and spawns the workers; the far
        // read follows immediately, before the workers could have drained the
        // whole file into the cache.
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64 * 1024)
                })
                .unwrap()
                .len(),
            64 * 1024
        );

        let far_start = total - 4096;
        let started = Instant::now();
        let bytes = source
            .read_range(ByteRange {
                start: far_start,
                length: Some(4096),
            })
            .expect("a far-forward read must re-anchor instead of stalling");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "far-forward read took {:?} (the stall wait is 20s)",
            started.elapsed()
        );
        assert_eq!(bytes.len(), 4096);
        for (index, byte) in bytes.iter().enumerate() {
            assert_eq!(*byte, ((far_start + index as u64) % 251) as u8);
        }
        // The read was served by a synchronous re-anchor at the far position,
        // not by draining a window the workers could never cover.
        let expected = format!("range: bytes={far_start}-{}", far_start + 4095);
        let heads = heads.lock().unwrap();
        assert!(
            heads
                .iter()
                .any(|head| head.to_lowercase().contains(&expected)),
            "a re-anchor request must be issued for the far position: {heads:?}"
        );
    }

    #[test]
    fn http_default_window_keeps_the_streams_delivering() {
        // A 2 MiB window must allow the worker to finish a 4 MiB stripe;
        // otherwise bytes needed by the reader stay in its private buffer and
        // every frontier read burns the 20 s stall clock.
        let total = 32 * 1024 * 1024u64;
        let (uri, _in_flight, _max, heads) = spawn_concurrent_mock_http_server(total);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);

        let started = Instant::now();
        let mut offset = 0u64;
        while offset < 8 * 1024 * 1024 {
            let bytes = source
                .read_range(ByteRange {
                    start: offset,
                    length: Some(64 * 1024),
                })
                .unwrap();
            assert_eq!(bytes.len(), 64 * 1024);
            offset += 64 * 1024;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "default-window sequential reads took {:?}: the 20 s stall clock means the streams never delivered",
            started.elapsed()
        );
        // The run must be served by the open-ended stream GETs, not only by the
        // capped synchronous fallback (which requests a closed range).
        let heads = heads.lock().unwrap();
        assert!(
            heads.iter().any(|head| head
                .to_lowercase()
                .lines()
                .any(|line| line.starts_with("range:") && line.trim_end().ends_with('-'))),
            "the default window must keep the streaming workers delivering: {heads:?}"
        );
    }

    #[test]
    fn http_stream_window_stays_bounded_against_a_fast_origin() {
        // A fast origin and a reader that only ever consumes a small prefix: the
        // workers must not run away with the rest of the resource. The bytes
        // buffered ahead of the reader must stay inside the configured window
        // plus the workers' in-flight stripe budget, never grow with the file.
        let total = 256 * 1024 * 1024u64;
        let window = 8 * 1024 * 1024u64;
        let (uri, _in_flight, _max, _heads) = spawn_concurrent_mock_http_server(total);
        let mut source =
            HttpRangeSource::with_http_headers_and_read_ahead(uri, Vec::new(), Some(window));
        source.content_length = Some(total);

        let mut offset = 0u64;
        while offset < 4 * 1024 * 1024 {
            let bytes = source
                .read_range(ByteRange {
                    start: offset,
                    length: Some(64 * 1024),
                })
                .unwrap();
            assert_eq!(bytes.len(), 64 * 1024);
            offset += 64 * 1024;
        }
        // Let an unthrottled worker flood the cache if the window does not bind.
        thread::sleep(Duration::from_millis(500));
        source.drain_stripes();
        let pending: u64 = source
            .streams
            .as_ref()
            .map(|session| {
                lock_stream(&session.shared)
                    .pending
                    .values()
                    .map(|stripe| stripe.bytes.len() as u64)
                    .sum()
            })
            .unwrap_or(0);
        let buffered = source
            .cache_end()
            .saturating_sub(source.stream_reader_end)
            .saturating_add(pending);
        let slack = HTTP_STREAM_STRIPE_BYTES;
        assert!(
            buffered <= window + slack,
            "prefetch ran away: {buffered} bytes buffered ahead of the reader \
             with a {window}-byte window (slack {slack})"
        );
    }

    #[test]
    fn http_rewind_inside_the_retained_tail_is_served_from_cache() {
        let total = 40 * 1024 * 1024u64;
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, total, b"unused"),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        // Pretend a 40 MiB window has been buffered from byte zero.
        source.cache_start = 0;
        source.cache_bytes = (0..total).map(|offset| (offset % 251) as u8).collect();

        // Reading near the tail trims the head down to the retention budget...
        let tail_start = total - 1024;
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: tail_start,
                    length: Some(1024)
                })
                .unwrap()
                .len(),
            1024
        );
        assert_eq!(
            source.cache_start,
            (tail_start) - HTTP_CACHE_RETAIN_BYTES,
            "the head must be trimmed to the retention budget, measured from the read"
        );

        // ...and a small rewind lands inside what is left: served locally.
        let rewind_start = source.cache_end() - 4 * 1024 * 1024;
        let bytes = source
            .read_range(ByteRange {
                start: rewind_start,
                length: Some(64),
            })
            .unwrap();
        assert_eq!(bytes.len(), 64);
        assert_eq!(bytes[0], (rewind_start % 251) as u8);
        assert!(
            requests.recv_timeout(Duration::from_millis(200)).is_err(),
            "a rewind inside the retained tail must not hit the network"
        );
    }

    #[test]
    fn http_rewind_past_the_retained_tail_reanchors_the_window() {
        let target = 2 * 1024 * 1024u64;
        let total = target + 128;
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(target, total, &[b'r'; 128]),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        // The cache only holds the tail; everything before it was trimmed.
        source.cache_start = 24 * 1024 * 1024;
        source.cache_bytes = vec![b'c'; 1024];

        // Far behind the retained tail: re-anchor at the read position, the
        // same thing every player does once a seek lands past its back buffer.
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: target,
                    length: Some(64)
                })
                .unwrap()
                .len(),
            64
        );
        let head = recv_request_head(&requests);
        assert!(
            head.contains(&format!("range: bytes={target}-")),
            "request head: {head}"
        );
        assert_eq!(source.cache_start, target);
    }

    // Real sockets, with short private deadlines so timeout regressions do not
    // add minutes to the test suite. A stalled response lasts until disconnect.
    fn spawn_timeout_http_server(
        responses: Vec<(Vec<u8>, bool)>,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let uri = format!("http://{}/media", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for (raw, stall) in responses {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return;
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept timeout test connection: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let _ = sender.send(head);
                if stream.write_all(&raw).is_err() {
                    return;
                }
                if stall {
                    // EOF/reset/abort all mean the client stopped the exchange.
                    // The socket timeout also bounds cleanup on test failure.
                    let _ = reader.read(&mut [0]);
                }
            }
        });
        (uri, receiver)
    }

    fn short_http_timeouts() -> HttpFetchTimeouts {
        HttpFetchTimeouts {
            // Real socket scheduling on loaded CI runners needs more than
            // millisecond-scale margins. These remain below production limits.
            response: Duration::from_secs(1),
            body: Duration::from_secs(1),
            total: Duration::from_secs(10),
        }
    }

    fn assert_length_probe_shares_budget(head_status: &str) {
        let (uri, requests) = spawn_timeout_http_server(vec![
            (
                http_simple_response("500 Internal Server Error", b""),
                false,
            ),
            (
                http_simple_response("500 Internal Server Error", b""),
                false,
            ),
            (http_simple_response(head_status, b""), false),
            (Vec::new(), true),
        ]);
        let io = HttpIo::new();
        let client = io.client.clone();
        let timeouts = HttpFetchTimeouts {
            response: Duration::from_secs(3),
            total: Duration::from_secs(2),
            ..short_http_timeouts()
        };
        let result = io
            .run(async move {
                // Two HEAD retries spend at least 1.2 s of the 2 s budget.
                // Allow 1 s of scheduler slack: a fresh 2 s probe deadline
                // would still exceed this watchdog (1.2 + 2 > 3).
                Ok(tokio::time::timeout(
                    Duration::from_secs(3),
                    fetch_http_length(&client, &uri, &[], Instant::now(), timeouts),
                )
                .await)
            })
            .unwrap()
            .expect("fallback GET escaped the metadata budget");
        assert!(result.is_err());
        assert!(recv_request_head(&requests).starts_with("head"));
        assert!(recv_request_head(&requests).starts_with("head"));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn http_timeout_rejected_head_probe_shares_budget() {
        assert_length_probe_shares_budget("405 Method Not Allowed");
    }

    #[test]
    fn http_timeout_zero_length_head_probe_shares_budget() {
        assert_length_probe_shares_budget("200 OK");
    }

    #[test]
    fn http_timeout_head_retries_stalled_headers() {
        let (uri, requests) = spawn_timeout_http_server(vec![
            (Vec::new(), true),
            (http_simple_response("200 OK", b"abcd"), false),
        ]);
        let io = HttpIo::new();
        let client = io.client.clone();
        let length = io
            .run(async move {
                fetch_http_length(&client, &uri, &[], Instant::now(), short_http_timeouts()).await
            })
            .unwrap();
        assert_eq!(length, Some(4));
        assert!(recv_request_head(&requests).starts_with("head"));
        assert!(recv_request_head(&requests).starts_with("head"));
    }

    #[test]
    fn http_timeout_range_retries_stalled_headers() {
        let (uri, requests) = spawn_timeout_http_server(vec![
            (Vec::new(), true),
            (http_206_response(0, 4, b"abcd"), false),
        ]);
        let io = HttpIo::new();
        let client = io.client.clone();
        let response = io
            .run(async move {
                fetch_http_range_async(
                    &client,
                    &uri,
                    &[],
                    ByteRange {
                        start: 0,
                        length: Some(4),
                    },
                    "test_range",
                    short_http_timeouts(),
                )
                .await
            })
            .unwrap();
        assert_eq!(response.bytes, b"abcd");
        assert!(recv_request_head(&requests).contains("range: bytes=0-3"));
        assert!(recv_request_head(&requests).contains("range: bytes=0-3"));
    }

    #[test]
    fn http_timeout_range_resumes_stalled_body() {
        let partial = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nContent-Length: 4\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nab";
        let (uri, requests) = spawn_timeout_http_server(vec![
            (partial.to_vec(), true),
            (http_206_response(2, 4, b"cd"), false),
        ]);
        let io = HttpIo::new();
        let client = io.client.clone();
        let response = io
            .run(async move {
                fetch_http_range_async(
                    &client,
                    &uri,
                    &[],
                    ByteRange {
                        start: 0,
                        length: Some(4),
                    },
                    "test_range",
                    short_http_timeouts(),
                )
                .await
            })
            .unwrap();
        assert_eq!(response.bytes, b"abcd");
        assert!(recv_request_head(&requests).contains("range: bytes=0-3"));
        let resumed = recv_request_head(&requests);
        assert!(resumed.contains("range: bytes=2-3"));
        assert!(resumed.contains("if-range: \"v1\""));
    }

    #[test]
    fn http_timeout_progressing_body_keeps_reading_until_total_budget() {
        for total in [Duration::from_secs(10), Duration::from_millis(500)] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let uri = format!("http://{}/media", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-7/8\r\nContent-Length: 8\r\nConnection: close\r\n\r\n").unwrap();
                for byte in b"abcdefgh" {
                    if socket.write_all(&[*byte]).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            });
            let io = HttpIo::new();
            let client = io.client.clone();
            let result = io.run(async move {
                fetch_http_range_async(
                    &client,
                    &uri,
                    &[],
                    ByteRange {
                        start: 0,
                        length: Some(8),
                    },
                    "test_range",
                    HttpFetchTimeouts {
                        total,
                        ..short_http_timeouts()
                    },
                )
                .await
            });
            if total == Duration::from_secs(10) {
                // The 1.4 s body outlives the 1 s header/idle deadlines, while
                // each 200 ms chunk interval leaves ample scheduling margin.
                assert_eq!(result.unwrap().bytes, b"abcdefgh");
            } else {
                assert!(result.is_err());
                // Without the total deadline the server delivers the full
                // body successfully. Check that result, not scheduler latency.
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn len_retries_head_before_succeeding() {
        let head_response = b"HTTP/1.1 200 OK\r\nContent-Length: 4321\r\nConnection: close\r\n\r\n";
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_simple_response("500 Internal Server Error", b"")),
            MockResponse::immediate(head_response.to_vec()),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(4321));
        assert!(recv_request_head(&requests).starts_with("head"));
        assert!(recv_request_head(&requests).starts_with("head"));
    }

    #[test]
    fn len_probe_does_not_download_a_body_that_ignores_range() {
        // HEAD is rejected and the origin ignores Range, answering 200 with the
        // whole object. The probe must take the total from Content-Length and
        // leave the payload on the wire instead of buffering the media.
        let payload = vec![b'x'; 512 * 1024];
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        raw.extend_from_slice(&payload);
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            MockResponse::immediate(raw),
        ]);

        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(payload.len() as u64));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"), "probe head: {probe}");
    }

    #[test]
    fn resumed_range_is_bound_to_the_first_response_entity() {
        let body: Vec<u8> = (0..64u8).collect();
        // Same truncated-then-resume shape as the resume test above, but the
        // first response carries a validator the retry has to replay.
        let mut truncated = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-63/64\r\nContent-Length: 64\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        truncated.extend_from_slice(&body[..32]);
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(truncated),
            MockResponse::immediate(http_206_response(32, 64, &body[32..])),
        ]);

        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64),
                })
                .unwrap(),
            body
        );

        let first = recv_request_head(&requests);
        assert!(!first.contains("if-range"), "first request: {first}");
        let resumed = recv_request_head(&requests);
        assert!(
            resumed.contains("if-range: \"v1\""),
            "resumed request must replay the validator: {resumed}"
        );
    }

    #[test]
    fn resumed_range_rejects_a_response_served_from_a_different_offset() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, _requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_truncated_response(0, 64, 64, &body[..32])),
            // The resume asked for byte 32; this answers from 0 instead, which
            // would splice mismatched bytes onto the prefix already held.
            MockResponse::immediate(http_206_response(0, 64, &body[..32])),
        ]);

        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        let error = source
            .read_range(ByteRange {
                start: 0,
                length: Some(64),
            })
            .unwrap_err();
        assert!(
            error.to_string().contains("served range from 0"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn len_falls_back_to_range_probe_when_head_is_rejected() {
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            MockResponse::immediate(http_206_response(0, 1234, b"z")),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(1234));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn len_falls_back_to_range_probe_when_head_reports_zero() {
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ),
            MockResponse::immediate(http_206_response(0, 911_198_509, b"z")),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(911_198_509));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn len_preserves_zero_when_range_probe_confirms_empty_resource() {
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ),
            MockResponse::immediate(
                b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(0));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn redacted_uri_hides_access_tokens() {
        assert_eq!(
            redacted_uri("https://example.invalid/video.mkv?api_key=secret&x=1"),
            "https://example.invalid/video.mkv?api_key=REDACTED&x=1"
        );
        assert_eq!(
            redacted_uri("https://example.invalid/video.mkv?AccessToken=secret"),
            "https://example.invalid/video.mkv?AccessToken=REDACTED"
        );
    }
    #[test]
    fn http_range_caps_the_buffered_response_body() {
        let (uri, _) = spawn_mock_http_server(vec![MockResponse::immediate(http_206_response(
            0,
            64,
            &[b'x'; 64],
        ))]);
        let response = fetch_http_range(
            &HttpIo::new(),
            &uri,
            &[],
            ByteRange {
                start: 0,
                length: Some(8),
            },
            "review_body_cap",
        )
        .unwrap();
        assert_eq!(
            response.bytes.len(),
            8,
            "bound the downloaded buffer, not just the returned read slice"
        );
    }

    #[test]
    fn http_range_stops_at_the_exact_limit_before_an_idle_body() {
        let (uri, requests) = spawn_timeout_http_server(vec![(
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-63/64\r\nContent-Length: 64\r\nConnection: close\r\n\r\nabcdefgh".to_vec(),
            true,
        )]);
        let io = HttpIo::new();
        let client = io.client.clone();
        let response = io
            .run(async move {
                fetch_http_range_async(
                    &client,
                    &uri,
                    &[],
                    ByteRange {
                        start: 0,
                        length: Some(8),
                    },
                    "review_exact_limit",
                    HttpFetchTimeouts {
                        total: Duration::from_millis(500),
                        ..short_http_timeouts()
                    },
                )
                .await
            })
            .expect("the requested bytes are complete; do not wait for an unwanted body tail");
        assert_eq!(response.bytes, b"abcdefgh");
        assert!(recv_request_head(&requests).contains("range: bytes=0-7"));
        assert!(
            requests.try_recv().is_err(),
            "a complete bounded range needs no resume"
        );
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Origin that always answers full `piece`-sized 206 bodies for the
    /// requested range, no matter how big the request was.
    fn spawn_capping_origin(
        total: u64,
        piece: u64,
    ) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.bin", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let maxlen = Arc::new(AtomicUsize::new(0));
        let (c, m) = (count.clone(), maxlen.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                c.fetch_add(1, Ordering::SeqCst);
                let lower = head.to_lowercase();
                if lower.starts_with("head") {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    continue;
                }
                // parse "range: bytes=N-M"
                let start: u64 = lower
                    .lines()
                    .find(|l| l.starts_with("range:"))
                    .and_then(|l| l.split_once("bytes="))
                    .and_then(|(_, v)| v.split('-').next())
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let end = (start + piece - 1).min(total.saturating_sub(1));
                let len = end.saturating_sub(start) + 1;
                m.fetch_max(len as usize, Ordering::SeqCst);
                let resp = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(resp.as_bytes()).is_err() {
                    continue;
                }
                let mut sent = 0u64;
                while sent < len {
                    let n = (32 * 1024u64).min(len - sent) as usize;
                    if stream.write_all(&vec![b'z'; n]).is_err() {
                        break;
                    }
                    sent += n as u64;
                }
                let _ = stream.flush();
            }
        });
        (uri, count, maxlen)
    }

    #[test]
    fn probe_stream_to_eof_known_total_just_past_the_limit_ends_cleanly() {
        // The old ordering checked the hard limit before the declared-total
        // EOF, so a resource whose total is the limit plus one full piece
        // (260 MiB with the current constants) was misreported as a limit
        // violation after the final piece landed exactly on the total. The EOF
        // check must win over the limit check.
        let total = HTTP_STREAM_TO_EOF_BYTE_LIMIT + HTTP_REQUEST_MAX_BYTES;
        let (uri, _, _) = spawn_capping_origin(total, HTTP_REQUEST_MAX_BYTES);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        let bytes = source
            .read_range(ByteRange::suffix_from(0))
            .expect("a declared total just past the limit is still a clean EOF");
        assert_eq!(bytes.len() as u64, total);
    }

    #[test]
    fn probe_stream_to_eof_beyond_the_hard_limit() {
        let total = HTTP_STREAM_TO_EOF_BYTE_LIMIT + 16 * 1024 * 1024;
        let (uri, _, _) = spawn_capping_origin(total, HTTP_REQUEST_MAX_BYTES);
        let mut source = HttpRangeSource::new(uri);
        match source.read_range(ByteRange::suffix_from(0)) {
            Err(error) => assert!(
                error.to_string().contains("limit"),
                "expected a limit error, got: {error}"
            ),
            Ok(bytes) => panic!(
                "an open-ended read past the hard limit must fail, got {} bytes",
                bytes.len()
            ),
        }
    }

    #[test]
    fn read_that_needs_more_than_the_piece_cap_must_fail_loudly() {
        // An origin that caps every body at 64 KiB: covering a forward gap much
        // larger than `HTTP_FETCH_MAX_PIECES_PER_READ` pieces needs more fetches
        // than one read may pull, and must error rather than hand the caller a
        // short read that looks like EOF on bytes that exist. The gap is sized
        // far past the cap so the answer is the same whether or not a prefetch
        // piece lands before the read.
        let total = 64 * 1024 * 1024u64;
        let piece = 64 * 1024u64;
        let (uri, _, _) = spawn_capping_origin(total, piece);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(8 * 1024 * 1024),
        );
        // Warm the cache so fetch_missing takes its multi-piece loop instead of
        // re-anchoring a fresh window at the read. The streams are then killed
        // and the total hidden: this pins the synchronous fallback's own piece
        // cap, which is what bounds the read when streaming cannot serve.
        source
            .read_range(ByteRange {
                start: 0,
                length: Some(piece),
            })
            .expect("the first piece must serve");
        source.kill_streams();
        source.content_length = None;
        let error = source
            .read_range(ByteRange {
                start: piece,
                length: Some(200 * piece),
            })
            .expect_err("a gap past the piece cap must error");
        assert!(
            error.to_string().contains("short pieces"),
            "expected a piece-cap error, got: {error}"
        );
    }
}
