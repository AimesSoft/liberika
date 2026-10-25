#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::{
    collections::VecDeque,
    env,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock, PoisonError, TryLockError,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use regex::Regex;

pub(crate) fn enabled() -> bool {
    env_flag("ERIKA_CLOCK_TRACE")
        || env_flag("ERIKA_DANMAKU_TRACE")
        || env_flag("ERIKA_PLAYBACK_TRACE")
}

pub(crate) fn log(line: impl AsRef<str>) {
    if !enabled() {
        return;
    }
    append_line(line.as_ref(), default_trace_path());
}

/// Emits an unconditional operational diagnostic. Decoder/backend transitions
/// use this path because a hardware fallback must remain visible even when the
/// optional playback trace is disabled.
///
/// The line is also kept in the process-wide log ring (see [`export_logs`]).
/// Its ring level is derived from the `event`/`stage` names of the structured
/// line; use [`diagnostic_at`] when the call site knows the severity.
pub(crate) fn diagnostic(line: impl AsRef<str>) {
    let line = line.as_ref();
    emit_diagnostic(classify_diagnostic(line), line);
}

/// Same output as [`diagnostic`], with an explicit level for the log ring.
/// The platform sinks (stderr, logcat WARN, hilog WARN) are unchanged.
pub(crate) fn diagnostic_at(level: LogLevel, line: impl AsRef<str>) {
    emit_diagnostic(level, line.as_ref());
}

fn emit_diagnostic(level: LogLevel, line: &str) {
    #[cfg(target_os = "android")]
    android_log(line);
    #[cfg(target_env = "ohos")]
    ohos_log(line);
    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    eprintln!("{line}");
    record(level, LogSource::Erika, line);
}

#[cfg(target_os = "android")]
fn android_log(line: &str) {
    use std::ffi::{CString, c_char, c_int};

    const ANDROID_LOG_WARN: c_int = 5;
    const TAG: &[u8] = b"Erika\0";

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(priority: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    let Ok(message) = CString::new(line.replace('\0', "\\0")) else {
        return;
    };
    unsafe {
        let _ = __android_log_write(
            ANDROID_LOG_WARN,
            TAG.as_ptr().cast::<c_char>(),
            message.as_ptr(),
        );
    }
}

#[cfg(target_env = "ohos")]
fn ohos_log(line: &str) {
    use std::ffi::{CString, c_char, c_int, c_uint};

    const LOG_APP: c_int = 0;
    const LOG_WARN: c_int = 5;
    const ERIKA_LOG_DOMAIN: c_uint = 0xE901;
    const TAG: &[u8] = b"Erika\0";
    const PUBLIC_STRING_FORMAT: &[u8] = b"%{public}s\0";

    #[link(name = "hilog_ndk.z")]
    unsafe extern "C" {
        fn OH_LOG_Print(
            log_type: c_int,
            level: c_int,
            domain: c_uint,
            tag: *const c_char,
            format: *const c_char,
            ...
        ) -> c_int;
    }

    let Ok(message) = CString::new(line.replace('\0', "\\0")) else {
        return;
    };
    unsafe {
        let _ = OH_LOG_Print(
            LOG_APP,
            LOG_WARN,
            ERIKA_LOG_DOMAIN,
            TAG.as_ptr().cast::<c_char>(),
            PUBLIC_STRING_FORMAT.as_ptr().cast::<c_char>(),
            message.as_ptr(),
        );
    }
}

pub(crate) fn append_line(line: impl AsRef<str>, path: impl AsRef<Path>) {
    let line = line.as_ref();
    let path = path.as_ref();
    eprintln!("{line}");
    if let Err(error) = append_line_to_path(line, path) {
        let fallback = fallback_trace_path(path);
        if fallback.as_path() != path {
            let _ = append_line_to_path(line, &fallback);
        }
        let _ = error;
    }
}

fn append_line_to_path(line: &str, path: &Path) -> std::io::Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{line}"))
}

fn fallback_trace_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_owned())
        .unwrap_or_else(|| "erika_playback_trace.log".into());
    env::temp_dir().join(file_name)
}

pub(crate) fn default_trace_path() -> PathBuf {
    if env_flag("ERIKA_PLAYBACK_TRACE") {
        env::var_os("ERIKA_PLAYBACK_TRACE_FILE")
            .or_else(|| env::var_os("ERIKA_DANMAKU_TRACE_FILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/erika_playback_trace.log"))
    } else {
        env::var_os("ERIKA_DANMAKU_TRACE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/erika_danmaku_trace.log"))
    }
}

pub(crate) fn duration_label(value: Option<Duration>) -> String {
    value
        .map(|duration| format!("{:.3}", duration.as_secs_f64()))
        .unwrap_or_else(|| "-".to_string())
}

pub(crate) fn duration_regressed(next: Duration, previous: Duration) -> bool {
    previous
        .checked_sub(next)
        .is_some_and(|delta| delta > Duration::from_millis(5))
}

pub(crate) fn duration_diff(a: Duration, b: Duration) -> Duration {
    a.checked_sub(b)
        .or_else(|| b.checked_sub(a))
        .unwrap_or(Duration::ZERO)
}

pub(crate) fn env_flag(name: &str) -> bool {
    match env::var(name).ok().as_deref() {
        Some("0" | "false" | "FALSE" | "off" | "OFF" | "") | None => false,
        Some(_) => true,
    }
}

// ---------------------------------------------------------------------------
// Process-wide log ring
//
// Every operational diagnostic, the converted operational `eprintln!` sites and
// FFmpeg warnings (through the `av_log` bridge in `ffmpeg.rs`) land in one
// bounded in-memory ring. It is process-global on purpose: diagnostics are
// emitted by free functions with no player identity, and FFmpeg's log callback
// is a process-wide hook shared by every player. Hosts read it on demand with
// `export_logs`; nothing is persisted or sent anywhere.
// ---------------------------------------------------------------------------

/// Severity of a log ring entry, ordered from least to most severe.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    /// Parses `trace`/`debug`/`info`/`warn`/`warning`/`error`, ignoring case.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    pub const fn from_raw(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Trace),
            1 => Some(Self::Debug),
            2 => Some(Self::Info),
            3 => Some(Self::Warn),
            4 => Some(Self::Error),
            _ => None,
        }
    }
}

/// Producer of a log ring entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogSource {
    /// liberika's own operational diagnostics.
    Erika,
    /// FFmpeg `av_log` messages at warning level or above.
    Ffmpeg,
}

impl LogSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Erika => "erika",
            Self::Ffmpeg => "ffmpeg",
        }
    }
}

/// Whether FFmpeg warnings are being copied into the ring.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfmpegLogCapture {
    /// FFmpeg has not been used by this process yet.
    NotInstalled = 0,
    Active = 1,
    /// Disabled with `ERIKA_FFMPEG_LOG_CAPTURE=0`.
    Disabled = 2,
    /// FFmpeg is a shared system library that other components of the process
    /// may also configure; capture requires `ERIKA_FFMPEG_LOG_CAPTURE=1`.
    SharedLibrary = 3,
}

impl FfmpegLogCapture {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::SharedLibrary => "shared_library_opt_in_required",
        }
    }

    const fn from_raw(value: u8) -> Self {
        match value {
            1 => Self::Active,
            2 => Self::Disabled,
            3 => Self::SharedLibrary,
            _ => Self::NotInstalled,
        }
    }
}

/// Selection for [`export_logs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogQuery {
    /// Most recent entries to return after filtering; `0` returns every
    /// retained entry.
    pub limit: usize,
    /// Drop entries below this level. `None` keeps every level.
    pub min_level: Option<LogLevel>,
    /// Only entries with `seq >= since_seq`. Pass the previous export's
    /// `nextSeq` to read incrementally.
    pub since_seq: u64,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            limit: 200,
            min_level: None,
            since_seq: 0,
        }
    }
}

pub(crate) const LOG_RING_MAX_ENTRIES: usize = 2000;
pub(crate) const LOG_RING_MAX_BYTES: usize = 512 * 1024;
pub(crate) const LOG_ENTRY_MAX_TEXT_BYTES: usize = 4096;
const LOG_ENTRY_OVERHEAD_BYTES: usize = 64;
const LOG_TRUNCATION_MARKER: &str = "…[truncated]";
/// New (non-coalesced) FFmpeg entries admitted per second. Corrupt streams
/// can emit a warning per macroblock; the rest are counted as rate-limited.
const FFMPEG_ENTRIES_PER_SECOND: u32 = 50;

#[derive(Debug, Clone)]
struct LogEntry {
    seq: u64,
    unix_ms: u64,
    last_unix_ms: u64,
    level: LogLevel,
    source: LogSource,
    count: u32,
    text: Box<str>,
}

impl LogEntry {
    fn cost(&self) -> usize {
        self.text.len() + LOG_ENTRY_OVERHEAD_BYTES
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushOutcome {
    Stored,
    Coalesced,
    RateLimited,
}

struct LogRing {
    entries: VecDeque<LogEntry>,
    bytes: usize,
    next_seq: u64,
    evicted: u64,
    rate_limited: u64,
    max_entries: usize,
    max_bytes: usize,
    ffmpeg_window_start_ms: u64,
    ffmpeg_window_count: u32,
}

impl LogRing {
    const fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            next_seq: 1,
            evicted: 0,
            rate_limited: 0,
            max_entries,
            max_bytes,
            ffmpeg_window_start_ms: 0,
            ffmpeg_window_count: 0,
        }
    }

    fn push(&mut self, level: LogLevel, source: LogSource, text: &str, now_ms: u64) -> PushOutcome {
        // Consecutive repeats of one message (numbers and addresses aside)
        // fold into the newest entry, so a flood cannot evict the history.
        if let Some(last) = self.entries.back_mut()
            && last.source == source
            && last.level == level
            && same_template(&last.text, text)
        {
            last.count = last.count.saturating_add(1);
            last.last_unix_ms = now_ms;
            return PushOutcome::Coalesced;
        }
        if source == LogSource::Ffmpeg {
            if now_ms.saturating_sub(self.ffmpeg_window_start_ms) >= 1000 {
                self.ffmpeg_window_start_ms = now_ms;
                self.ffmpeg_window_count = 0;
            }
            if self.ffmpeg_window_count >= FFMPEG_ENTRIES_PER_SECOND {
                self.rate_limited = self.rate_limited.saturating_add(1);
                return PushOutcome::RateLimited;
            }
            self.ffmpeg_window_count += 1;
        }
        let entry = LogEntry {
            seq: self.next_seq,
            unix_ms: now_ms,
            last_unix_ms: now_ms,
            level,
            source,
            count: 1,
            text: truncate_log_text(text).into_boxed_str(),
        };
        let cost = entry.cost();
        while !self.entries.is_empty()
            && (self.entries.len() >= self.max_entries
                || self.bytes.saturating_add(cost) > self.max_bytes)
        {
            if let Some(removed) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed.cost());
                self.evicted = self.evicted.saturating_add(1);
            }
        }
        self.next_seq = self.next_seq.saturating_add(1);
        self.bytes = self.bytes.saturating_add(cost);
        self.entries.push_back(entry);
        PushOutcome::Stored
    }

    fn select(&self, query: &LogQuery) -> Vec<LogEntry> {
        let limit = if query.limit == 0 {
            usize::MAX
        } else {
            query.limit
        };
        let mut selected: Vec<LogEntry> = self
            .entries
            .iter()
            .rev()
            .filter(|entry| entry.seq >= query.since_seq)
            .filter(|entry| query.min_level.is_none_or(|min| entry.level >= min))
            .take(limit)
            .cloned()
            .collect();
        selected.reverse();
        selected
    }
}

static LOG_RING: Mutex<LogRing> =
    Mutex::new(LogRing::new(LOG_RING_MAX_ENTRIES, LOG_RING_MAX_BYTES));
static LOG_DROPPED_CONTENDED: AtomicU64 = AtomicU64::new(0);
static FFMPEG_LOG_CAPTURE: AtomicU8 = AtomicU8::new(FfmpegLogCapture::NotInstalled as u8);

fn unix_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        let millis = js_sys::Date::now();
        if millis.is_finite() && millis >= 0.0 {
            millis.min(u64::MAX as f64) as u64
        } else {
            0
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    }
}

fn record(level: LogLevel, source: LogSource, text: &str) {
    let now_ms = unix_ms();
    let mut ring = LOG_RING.lock().unwrap_or_else(PoisonError::into_inner);
    ring.push(level, source, text, now_ms);
}

/// Records a message from FFmpeg's log callback. Never blocks: when another
/// thread holds the ring the message is counted as dropped instead.
pub(crate) fn record_ffmpeg(level: LogLevel, text: &str) {
    let now_ms = unix_ms();
    let mut ring = match LOG_RING.try_lock() {
        Ok(ring) => ring,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            LOG_DROPPED_CONTENDED.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    ring.push(level, LogSource::Ffmpeg, text, now_ms);
}

pub(crate) fn set_ffmpeg_log_capture(state: FfmpegLogCapture) {
    FFMPEG_LOG_CAPTURE.store(state as u8, Ordering::Relaxed);
}

pub fn ffmpeg_log_capture() -> FfmpegLogCapture {
    FfmpegLogCapture::from_raw(FFMPEG_LOG_CAPTURE.load(Ordering::Relaxed))
}

/// Exports recent log entries as a JSON document.
///
/// Entries are oldest first. Text is redacted on the way out: URL query
/// strings and user info, credential-like `key=value` pairs and headers, and
/// the local user's home directory. Structured diagnostics are returned as a
/// parsed `data` object; other lines as `message`.
pub fn export_logs(query: &LogQuery) -> serde_json::Value {
    export_ring(
        &LOG_RING,
        LOG_DROPPED_CONTENDED.load(Ordering::Relaxed),
        ffmpeg_log_capture(),
        query,
    )
}

fn export_ring(
    ring: &Mutex<LogRing>,
    dropped_contended: u64,
    capture: FfmpegLogCapture,
    query: &LogQuery,
) -> serde_json::Value {
    // Copy under the lock, redact and serialize after releasing it so log
    // producers (including the non-blocking FFmpeg callback) are not held up.
    let (entries, retained, retained_bytes, oldest_seq, next_seq, evicted, rate_limited) = {
        let ring = ring.lock().unwrap_or_else(PoisonError::into_inner);
        (
            ring.select(query),
            ring.entries.len(),
            ring.bytes,
            ring.entries.front().map(|entry| entry.seq),
            ring.next_seq,
            ring.evicted,
            ring.rate_limited,
        )
    };
    let entries: Vec<serde_json::Value> = entries.iter().map(entry_json).collect();
    serde_json::json!({
        "schema": 1,
        "capacity": {
            "entries": LOG_RING_MAX_ENTRIES,
            "bytes": LOG_RING_MAX_BYTES,
            "entryTextBytes": LOG_ENTRY_MAX_TEXT_BYTES,
        },
        "retained": retained,
        "retainedBytes": retained_bytes,
        "oldestSeq": oldest_seq,
        "nextSeq": next_seq,
        "evicted": evicted,
        "rateLimited": rate_limited,
        "droppedContended": dropped_contended,
        "ffmpegCapture": capture.as_str(),
        "redacted": true,
        "entries": entries,
    })
}

fn entry_json(entry: &LogEntry) -> serde_json::Value {
    let text = redact_log_text(&entry.text);
    let mut object = serde_json::Map::new();
    object.insert("seq".into(), entry.seq.into());
    object.insert("unixMs".into(), entry.unix_ms.into());
    object.insert("level".into(), entry.level.as_str().into());
    object.insert("source".into(), entry.source.as_str().into());
    object.insert(
        "event".into(),
        json_string_field(&text, "event")
            .map(|event| serde_json::Value::String(event.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    object.insert("count".into(), entry.count.into());
    if entry.count > 1 {
        object.insert("lastUnixMs".into(), entry.last_unix_ms.into());
    }
    let data = text
        .starts_with('{')
        .then(|| serde_json::from_str::<serde_json::Value>(&text).ok())
        .flatten()
        .filter(serde_json::Value::is_object);
    match data {
        Some(data) => {
            object.insert("data".into(), data);
        }
        None => {
            object.insert("message".into(), text.into());
        }
    }
    serde_json::Value::Object(object)
}

fn truncate_log_text(text: &str) -> String {
    if text.len() <= LOG_ENTRY_MAX_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = LOG_ENTRY_MAX_TEXT_BYTES - LOG_TRUNCATION_MARKER.len();
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = String::with_capacity(end + LOG_TRUNCATION_MARKER.len());
    truncated.push_str(&text[..end]);
    truncated.push_str(LOG_TRUNCATION_MARKER);
    truncated
}

/// Returns the string value of `"name":"…"` in a compact JSON line without
/// parsing the whole document.
fn json_string_field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let bytes = line.as_bytes();
    let mut search_from = 0;
    while let Some(relative) = line[search_from..].find(name) {
        let start = search_from + relative;
        search_from = start + name.len();
        if start == 0 || bytes[start - 1] != b'"' || bytes.get(search_from) != Some(&b'"') {
            continue;
        }
        let mut cursor = search_from + 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b':') {
            continue;
        }
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'"') {
            return None;
        }
        let value_start = cursor + 1;
        let mut value_end = value_start;
        while let Some(&byte) = bytes.get(value_end) {
            match byte {
                b'"' => return Some(&line[value_start..value_end]),
                b'\\' => value_end += 2,
                _ => value_end += 1,
            }
        }
        return None;
    }
    None
}

const ERROR_TOKENS: &[&str] = &["fatal", "panic", "panicked"];
const WARN_TOKENS: &[&str] = &[
    "fail",
    "failed",
    "failure",
    "error",
    "errors",
    "timeout",
    "unavailable",
    "lost",
    "rejected",
    "exhausted",
    "stall",
    "stalled",
    "missing",
    "invalid",
    "disconnected",
    "fallback",
    "removed",
    "skipped",
    "unsupported",
    "abandoned",
];
const WARN_EVENTS: &[&str] = &["video_frame_backpressure", "dovi_el_not_composed"];
/// Events whose name describes a routine mechanism rather than a failure;
/// only their stage decides (e.g. the fallback subtitle font being loaded).
const NEUTRAL_EVENTS: &[&str] = &["subtitle_font_fallback"];

/// Default ring level for a structured diagnostic whose call site does not
/// state one: `error` for fatal/panic names, `warn` for failures, timeouts,
/// unavailable/lost/rejected resources and fallbacks (including hardware to
/// software or byte-buffer routes), `info` otherwise.
pub(crate) fn classify_diagnostic(line: &str) -> LogLevel {
    let event = json_string_field(line, "event").unwrap_or("");
    let stage = json_string_field(line, "stage").unwrap_or("");
    let event_tokens = if NEUTRAL_EVENTS.contains(&event) {
        ""
    } else {
        event
    };
    let has_token = |tokens: &[&str]| {
        event_tokens
            .split('_')
            .chain(stage.split('_'))
            .any(|token| tokens.contains(&token))
    };
    if has_token(ERROR_TOKENS) {
        LogLevel::Error
    } else if WARN_EVENTS.contains(&event)
        || has_token(WARN_TOKENS)
        || stage.contains("to_software")
        || stage.contains("to_bytebuffer")
    {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateUnit<'a> {
    Number,
    Text(&'a str),
}

fn is_number_like(byte: u8) -> bool {
    byte.is_ascii_hexdigit() || byte == b'x' || byte == b'X'
}

fn next_template_unit(text: &str, position: usize) -> Option<(TemplateUnit<'_>, usize)> {
    let bytes = text.as_bytes();
    let first = *bytes.get(position)?;
    if is_number_like(first) {
        let mut end = position;
        let mut has_digit = false;
        while let Some(&byte) = bytes.get(end) {
            if !is_number_like(byte) {
                break;
            }
            has_digit |= byte.is_ascii_digit();
            end += 1;
        }
        let unit = if has_digit {
            TemplateUnit::Number
        } else {
            TemplateUnit::Text(&text[position..end])
        };
        return Some((unit, end));
    }
    let width = text[position..].chars().next().map_or(1, char::len_utf8);
    Some((
        TemplateUnit::Text(&text[position..position + width]),
        position + width,
    ))
}

/// True when two messages differ only in numbers and hexadecimal addresses
/// (macroblock coordinates, PTS values, `0x…` context pointers).
fn same_template(left: &str, right: &str) -> bool {
    let (mut left_position, mut right_position) = (0, 0);
    loop {
        match (
            next_template_unit(left, left_position),
            next_template_unit(right, right_position),
        ) {
            (None, None) => return true,
            (Some((left_unit, left_next)), Some((right_unit, right_next)))
                if left_unit == right_unit =>
            {
                left_position = left_next;
                right_position = right_next;
            }
            _ => return false,
        }
    }
}

struct Redactor {
    url: Regex,
    key_value: Regex,
    json_key: Regex,
    bearer: Regex,
    unix_home: Regex,
    windows_home: Regex,
}

fn redactor() -> &'static Redactor {
    static REDACTOR: OnceLock<Redactor> = OnceLock::new();
    REDACTOR.get_or_init(|| {
        const KEYS: &str = "api_key|apikey|access_token|accesstoken|refresh_token|token|auth_token|\
                            authorization|password|passwd|pwd|secret|signature|sig|session|sessionid|\
                            x-emby-token|x-mediabrowser-token|x-plex-token|cookie|set-cookie";
        Redactor {
            url: Regex::new(r#"(?i)\b[a-z][a-z0-9+.\-]*://[^\s"'<>\\]+"#).expect("URL redaction regex"),
            key_value: Regex::new(&format!(r#"(?i)\b({KEYS})(\s*[=:]\s*)[^\s"'&,;]+"#))
                .expect("key=value redaction regex"),
            json_key: Regex::new(&format!(r#"(?i)"({KEYS})"(\s*:\s*)"(?:[^"\\]|\\.)*""#))
                .expect("JSON key redaction regex"),
            bearer: Regex::new(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=\-]+")
                .expect("authorization redaction regex"),
            unix_home: Regex::new(r#"(/Users/|/home/)[^/\s"'\\]+"#).expect("home redaction regex"),
            windows_home: Regex::new(r#"(?i)([a-z]:(?:\\\\|\\|/)users(?:\\\\|\\|/))[^\\/\s"']+"#)
                .expect("Windows home redaction regex"),
        }
    })
}

fn redact_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host = match authority.rfind('@') {
        Some(at) => format!("<redacted>@{}", &authority[at + 1..]),
        None => authority.to_string(),
    };
    let tail = match tail.find(['?', '#']) {
        Some(index) => format!("{}{}<redacted>", &tail[..index], &tail[index..index + 1]),
        None => tail.to_string(),
    };
    format!("{scheme}://{host}{tail}")
}

fn home_directories() -> Vec<String> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(|name| env::var(name).ok())
        .map(|home| home.trim_end_matches(['/', '\\']).to_string())
        .filter(|home| home.len() > 1)
        .collect()
}

/// Removes URL credentials/query strings, credential-like values, and the
/// local user's home directory from exported log text.
pub(crate) fn redact_log_text(text: &str) -> String {
    redact_log_text_with_homes(text, &home_directories())
}

fn redact_log_text_with_homes(text: &str, homes: &[String]) -> String {
    let redactor = redactor();
    let mut value = text.to_string();
    for home in homes {
        if value.contains(home.as_str()) {
            value = value.replace(home.as_str(), "~");
        }
        // JSON-escaped Windows paths double every backslash.
        if home.contains('\\') {
            let escaped = home.replace('\\', "\\\\");
            if value.contains(escaped.as_str()) {
                value = value.replace(escaped.as_str(), "~");
            }
        }
    }
    let value = redactor
        .url
        .replace_all(&value, |captures: &regex::Captures<'_>| {
            redact_url(&captures[0])
        });
    // Scheme-prefixed credentials first, so `Authorization: Bearer x` loses
    // the token rather than only the word `Bearer`.
    let value = redactor.bearer.replace_all(&value, "$1 <redacted>");
    let value = redactor
        .json_key
        .replace_all(&value, "\"$1\"$2\"<redacted>\"");
    let value = redactor.key_value.replace_all(&value, "$1$2<redacted>");
    let value = redactor.unix_home.replace_all(&value, "$1<user>");
    let value = redactor.windows_home.replace_all(&value, "$1<user>");
    value.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn ring() -> LogRing {
        LogRing::new(LOG_RING_MAX_ENTRIES, LOG_RING_MAX_BYTES)
    }

    #[test]
    fn ring_keeps_the_newest_entries_within_the_entry_limit() {
        let mut ring = LogRing::new(3, usize::MAX);
        for index in 0..5 {
            ring.push(
                LogLevel::Info,
                LogSource::Erika,
                &format!("event {}", "x".repeat(index)),
                10,
            );
        }
        assert_eq!(ring.entries.len(), 3);
        assert_eq!(ring.evicted, 2);
        let seqs: Vec<u64> = ring.entries.iter().map(|entry| entry.seq).collect();
        assert_eq!(seqs, vec![3, 4, 5]);
    }

    #[test]
    fn ring_respects_the_byte_limit() {
        let mut ring = LogRing::new(usize::MAX, 3 * (100 + LOG_ENTRY_OVERHEAD_BYTES));
        for index in 0..10u8 {
            let text = String::from(char::from(b'a' + index)).repeat(100);
            ring.push(LogLevel::Info, LogSource::Erika, &text, 0);
        }
        assert_eq!(ring.entries.len(), 3);
        assert!(ring.bytes <= ring.max_bytes);
        assert_eq!(ring.evicted, 7);
    }

    #[test]
    fn long_entries_are_truncated_on_a_char_boundary() {
        let mut ring = ring();
        let text = "界".repeat(LOG_ENTRY_MAX_TEXT_BYTES);
        ring.push(LogLevel::Warn, LogSource::Erika, &text, 0);
        let stored = &ring.entries.back().unwrap().text;
        assert!(stored.len() <= LOG_ENTRY_MAX_TEXT_BYTES);
        assert!(stored.ends_with(LOG_TRUNCATION_MARKER));
    }

    #[test]
    fn consecutive_repeats_that_differ_only_in_numbers_are_coalesced() {
        let mut ring = ring();
        let first = ring.push(
            LogLevel::Warn,
            LogSource::Ffmpeg,
            "[h264 @ 0x7f8b1c00a400] error while decoding MB 23 45, bytestream -5",
            100,
        );
        let second = ring.push(
            LogLevel::Warn,
            LogSource::Ffmpeg,
            "[h264 @ 0x7f8b1c00b800] error while decoding MB 7 12, bytestream -11",
            250,
        );
        let different = ring.push(
            LogLevel::Warn,
            LogSource::Ffmpeg,
            "[h264 @ 0x7f8b1c00b800] concealing 120 DC errors",
            300,
        );
        assert_eq!(first, PushOutcome::Stored);
        assert_eq!(second, PushOutcome::Coalesced);
        assert_eq!(different, PushOutcome::Stored);
        assert_eq!(ring.entries.len(), 2);
        let coalesced = ring.entries.front().unwrap();
        assert_eq!(coalesced.count, 2);
        assert_eq!(coalesced.last_unix_ms, 250);
        assert!(!same_template("decode", "dncodn"));
        assert!(same_template("pts 12 dts 0x1f", "pts 999 dts 0xAB3"));
    }

    #[test]
    fn ffmpeg_entries_are_rate_limited_per_second() {
        let mut ring = ring();
        for index in 0..(FFMPEG_ENTRIES_PER_SECOND + 10) {
            // Distinct words so nothing is coalesced.
            let text = format!("warning {}", "w".repeat(index as usize + 1));
            ring.push(LogLevel::Warn, LogSource::Ffmpeg, &text, 5_000);
        }
        assert_eq!(ring.entries.len(), FFMPEG_ENTRIES_PER_SECOND as usize);
        assert_eq!(ring.rate_limited, 10);
        ring.push(LogLevel::Warn, LogSource::Ffmpeg, "next second", 6_000);
        assert_eq!(ring.entries.len(), FFMPEG_ENTRIES_PER_SECOND as usize + 1);
        // liberika's own diagnostics are never rate-limited.
        ring.push(LogLevel::Info, LogSource::Erika, "own", 6_000);
        assert_eq!(ring.entries.back().unwrap().source, LogSource::Erika);
    }

    #[test]
    fn selection_filters_by_level_sequence_and_limit() {
        let mut ring = ring();
        ring.push(LogLevel::Info, LogSource::Erika, "a info", 0);
        ring.push(LogLevel::Warn, LogSource::Erika, "b warn", 0);
        ring.push(LogLevel::Error, LogSource::Erika, "c error", 0);
        ring.push(LogLevel::Debug, LogSource::Erika, "d debug", 0);
        ring.push(LogLevel::Warn, LogSource::Erika, "e warn", 0);
        let warn = ring.select(&LogQuery {
            limit: 0,
            min_level: Some(LogLevel::Warn),
            since_seq: 0,
        });
        let texts: Vec<&str> = warn.iter().map(|entry| &*entry.text).collect();
        assert_eq!(texts, vec!["b warn", "c error", "e warn"]);
        let newest_two = ring.select(&LogQuery {
            limit: 2,
            min_level: None,
            since_seq: 0,
        });
        let texts: Vec<&str> = newest_two.iter().map(|entry| &*entry.text).collect();
        assert_eq!(texts, vec!["d debug", "e warn"]);
        let since = ring.select(&LogQuery {
            limit: 0,
            min_level: None,
            since_seq: 4,
        });
        assert_eq!(since.len(), 2);
        assert_eq!(since[0].seq, 4);
    }

    #[test]
    fn concurrent_writers_never_exceed_the_limits() {
        let ring = Arc::new(Mutex::new(LogRing::new(64, 16 * 1024)));
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let ring = Arc::clone(&ring);
                thread::spawn(move || {
                    for index in 0..500 {
                        let text = format!(
                            "worker-{worker} message {} {}",
                            "z".repeat(index % 7),
                            index
                        );
                        ring.lock()
                            .unwrap()
                            .push(LogLevel::Info, LogSource::Erika, &text, 1);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let ring = ring.lock().unwrap();
        assert!(ring.entries.len() <= 64);
        assert!(ring.bytes <= 16 * 1024);
        let total: u64 = ring
            .entries
            .iter()
            .map(|entry| u64::from(entry.count))
            .sum::<u64>()
            + ring.evicted;
        assert!(total <= 8 * 500);
        let seqs: Vec<u64> = ring.entries.iter().map(|entry| entry.seq).collect();
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn concurrent_global_recording_and_export_stay_consistent() {
        let writers: Vec<_> = (0..4)
            .map(|worker| {
                thread::spawn(move || {
                    for index in 0..300 {
                        if index % 2 == 0 {
                            let text = format!("w{worker} {} n{index}", "q".repeat(index % 5));
                            record(LogLevel::Info, LogSource::Erika, &text);
                        } else {
                            let text = format!("ff{worker} {} n{index}", "r".repeat(index % 5));
                            record_ffmpeg(LogLevel::Warn, &text);
                        }
                    }
                })
            })
            .collect();
        let reader = thread::spawn(|| {
            for _ in 0..50 {
                let exported = export_logs(&LogQuery {
                    limit: 0,
                    min_level: None,
                    since_seq: 0,
                });
                let entries = exported["entries"].as_array().unwrap();
                assert!(entries.len() <= LOG_RING_MAX_ENTRIES);
                let seqs: Vec<u64> = entries
                    .iter()
                    .map(|entry| entry["seq"].as_u64().unwrap())
                    .collect();
                assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]));
            }
        });
        for writer in writers {
            writer.join().unwrap();
        }
        reader.join().unwrap();
        let exported = export_logs(&LogQuery {
            limit: 0,
            min_level: None,
            since_seq: 0,
        });
        assert!(exported["retained"].as_u64().unwrap() > 0);
        assert!(exported["retainedBytes"].as_u64().unwrap() <= LOG_RING_MAX_BYTES as u64);
    }

    #[test]
    fn diagnostics_reach_the_global_ring_with_their_level() {
        let marker = "ring-marker-7f3c";
        diagnostic(
            serde_json::json!({ "event": "video_decoder", "stage": "open_avcodec_to_software", "marker": marker })
                .to_string(),
        );
        diagnostic_at(LogLevel::Error, format!("explicit {marker}"));
        let exported = export_logs(&LogQuery {
            limit: 0,
            min_level: None,
            since_seq: 0,
        });
        let entries = exported["entries"].as_array().unwrap();
        let structured = entries
            .iter()
            .find(|entry| entry["data"]["marker"] == marker)
            .expect("structured diagnostic in the ring");
        assert_eq!(structured["level"], "warn");
        assert_eq!(structured["event"], "video_decoder");
        let explicit = entries
            .iter()
            .find(|entry| entry["message"] == format!("explicit {marker}"))
            .expect("explicit diagnostic in the ring");
        assert_eq!(explicit["level"], "error");
    }

    #[test]
    fn diagnostics_get_a_level_from_their_event_and_stage() {
        let classify = |event: &str, stage: &str| {
            classify_diagnostic(&serde_json::json!({ "event": event, "stage": stage }).to_string())
        };
        assert_eq!(classify("playback_fatal", "open"), LogLevel::Error);
        assert_eq!(
            classify("playback_worker_shutdown", "join_panicked"),
            LogLevel::Error
        );
        assert_eq!(
            classify("android_mediacodec_decoder", "decoder_open_failed"),
            LogLevel::Warn
        );
        assert_eq!(
            classify("video_decoder", "open_avcodec_to_software"),
            LogLevel::Warn
        );
        assert_eq!(
            classify(
                "android_mediacodec_seek_reopen",
                "open_surface_to_bytebuffer"
            ),
            LogLevel::Warn
        );
        assert_eq!(
            classify("player_frame_output", "ack_timeout"),
            LogLevel::Warn
        );
        assert_eq!(classify("subtitle_font_fallback", "loaded"), LogLevel::Info);
        assert_eq!(
            classify("subtitle_font_fallback", "rejected"),
            LogLevel::Warn
        );
        assert_eq!(
            classify("audio_decoder_fallback", "selected"),
            LogLevel::Warn
        );
        assert_eq!(
            classify("video_frame_backpressure", "renderer_upload_drop"),
            LogLevel::Warn
        );
        assert_eq!(classify("video_seek_preroll", "complete"), LogLevel::Info);
        assert_eq!(
            classify("video_decoder", "software_decoder_selected"),
            LogLevel::Info
        );
        assert_eq!(classify_diagnostic("plain text line"), LogLevel::Info);
    }

    #[test]
    fn json_string_field_reads_compact_and_spaced_lines() {
        assert_eq!(
            json_string_field(r#"{"event":"a_b","stage":"c"}"#, "event"),
            Some("a_b")
        );
        assert_eq!(
            json_string_field(r#"{"x":"event", "event" : "real"}"#, "event"),
            Some("real")
        );
        assert_eq!(
            json_string_field(r#"{"stage":"say \"hi\""}"#, "stage"),
            Some(r#"say \"hi\""#)
        );
        assert_eq!(json_string_field(r#"{"event":3}"#, "event"), None);
        assert_eq!(json_string_field("no json", "event"), None);
    }

    #[test]
    fn redaction_removes_query_strings_user_info_and_credentials() {
        let homes = vec!["/Users/sakiko".to_string()];
        let redacted = redact_log_text_with_homes(
            "open https://alice:hunter2@media.example.com:8920/emby/Videos/12/stream.mkv?api_key=abc&Static=true#t=5 \
             token=xyz Authorization: Bearer eyJhbGciOi X-Emby-Token: 0123",
            &homes,
        );
        assert!(!redacted.contains("alice"), "{redacted}");
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(!redacted.contains("abc"), "{redacted}");
        assert!(!redacted.contains("xyz"), "{redacted}");
        assert!(!redacted.contains("eyJhbGciOi"), "{redacted}");
        assert!(!redacted.contains("0123"), "{redacted}");
        assert!(redacted.contains(
            "https://<redacted>@media.example.com:8920/emby/Videos/12/stream.mkv?<redacted>"
        ));
    }

    #[test]
    fn redaction_removes_home_directories_and_keeps_json_valid() {
        let homes = vec!["/Users/sakiko".to_string(), r"C:\Users\Sakiko".to_string()];
        let line = serde_json::json!({
            "event": "subtitle_custom_font",
            "path": "/Users/sakiko/Movies/a.ass",
            "other": "/home/bob/x.srt",
            "win": r"C:\Users\Sakiko\Videos\b.mkv",
            "winOther": r"D:\Users\carol\c.mkv",
            "uri": "http://h/x?token=1",
            "token": "secret-value",
        })
        .to_string();
        let redacted = redact_log_text_with_homes(&line, &homes);
        let parsed: serde_json::Value = serde_json::from_str(&redacted).expect("still JSON");
        assert_eq!(parsed["path"], "~/Movies/a.ass");
        assert_eq!(parsed["other"], "/home/<user>/x.srt");
        assert_eq!(parsed["win"], r"~\Videos\b.mkv");
        assert_eq!(parsed["winOther"], r"D:\Users\<user>\c.mkv");
        assert_eq!(parsed["uri"], "http://h/x?<redacted>");
        assert_eq!(parsed["token"], "<redacted>");
        assert_eq!(parsed["event"], "subtitle_custom_font");
    }

    #[test]
    fn export_returns_redacted_structured_entries() {
        let ring = Mutex::new(ring());
        {
            let mut ring = ring.lock().unwrap();
            let line = serde_json::json!({
                "event": "video_decoder",
                "stage": "open_avcodec_to_software",
                "uri": "https://example.com/a.mkv?api_key=k",
            })
            .to_string();
            ring.push(classify_diagnostic(&line), LogSource::Erika, &line, 1_000);
            ring.push(
                LogLevel::Error,
                LogSource::Erika,
                "Erika presenter audio start failed: device busy",
                1_001,
            );
            ring.push(
                LogLevel::Warn,
                LogSource::Ffmpeg,
                "[hevc @ 0x1] Could not find ref with POC 12",
                1_002,
            );
            ring.push(
                LogLevel::Warn,
                LogSource::Ffmpeg,
                "[hevc @ 0x2] Could not find ref with POC 13",
                1_003,
            );
        }
        let exported = export_ring(&ring, 2, FfmpegLogCapture::Active, &LogQuery::default());
        assert_eq!(exported["schema"], 1);
        assert_eq!(exported["redacted"], true);
        assert_eq!(exported["droppedContended"], 2);
        assert_eq!(exported["ffmpegCapture"], "active");
        assert_eq!(exported["retained"], 3);
        assert_eq!(exported["oldestSeq"], 1);
        let entries = exported["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["level"], "warn");
        assert_eq!(entries[0]["source"], "erika");
        assert_eq!(entries[0]["unixMs"], 1_000);
        assert_eq!(entries[0]["event"], "video_decoder");
        assert_eq!(
            entries[0]["data"]["uri"],
            "https://example.com/a.mkv?<redacted>"
        );
        assert!(entries[0].get("message").is_none());
        assert_eq!(entries[1]["level"], "error");
        assert_eq!(
            entries[1]["message"],
            "Erika presenter audio start failed: device busy"
        );
        assert!(entries[1]["event"].is_null());
        assert_eq!(entries[2]["source"], "ffmpeg");
        assert_eq!(entries[2]["count"], 2);
        assert_eq!(entries[2]["lastUnixMs"], 1_003);
        let next = exported["nextSeq"].as_u64().unwrap();
        assert_eq!(next, 4);
        let errors = export_ring(
            &ring,
            0,
            FfmpegLogCapture::Active,
            &LogQuery {
                limit: 10,
                min_level: Some(LogLevel::Error),
                since_seq: 0,
            },
        );
        assert_eq!(errors["entries"].as_array().unwrap().len(), 1);
        let incremental = export_ring(
            &ring,
            0,
            FfmpegLogCapture::Active,
            &LogQuery {
                limit: 10,
                min_level: None,
                since_seq: next,
            },
        );
        assert!(incremental["entries"].as_array().unwrap().is_empty());
    }

    #[test]
    fn levels_parse_from_names_and_raw_values() {
        assert_eq!(LogLevel::parse("WARNING"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("error"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("loud"), None);
        assert_eq!(LogLevel::from_raw(0), Some(LogLevel::Trace));
        assert_eq!(LogLevel::from_raw(4), Some(LogLevel::Error));
        assert_eq!(LogLevel::from_raw(-1), None);
        assert!(LogLevel::Error > LogLevel::Warn);
    }
}
