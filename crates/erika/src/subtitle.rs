#[cfg(feature = "libass")]
use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString},
    fs::File,
    io::Read,
    ptr::NonNull,
    sync::Mutex,
    time::SystemTime,
};
use std::{sync::Arc, time::Duration};

use thiserror::Error;

#[cfg(feature = "libass")]
use crate::NIPAPLAY_FALLBACK_FONT;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SubtitleError {
    #[error("invalid subtitle timestamp: {0}")]
    InvalidTimestamp(String),
    #[error("invalid subtitle cue")]
    InvalidCue,
    #[error("invalid subtitle bitmap: width={width} height={height} stride={stride} bytes={bytes}")]
    InvalidBitmap {
        width: u32,
        height: u32,
        stride: usize,
        bytes: usize,
    },
    #[error("subtitle bitmap pointer is null")]
    NullBitmap,
    #[error("subtitle bitmap list exceeded safety limit")]
    BitmapListTooLong,
    #[error("libass error: {0}")]
    Libass(String),
}

pub type Result<T> = std::result::Result<T, SubtitleError>;

pub use crate::font::{
    MAX_MEMORY_SUBTITLE_FONT_BYTES, MAX_MEMORY_SUBTITLE_FONT_TOTAL_BYTES, SubtitleFontAttachment,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssTrackResources {
    pub source_stream_index: i64,
    pub codec_private: Arc<[u8]>,
    pub fonts: Arc<[SubtitleFontAttachment]>,
}

impl AssTrackResources {
    pub fn new(
        source_stream_index: i64,
        codec_private: impl Into<Arc<[u8]>>,
        fonts: impl Into<Arc<[SubtitleFontAttachment]>>,
    ) -> Self {
        Self {
            source_stream_index,
            codec_private: codec_private.into(),
            fonts: fonts.into(),
        }
    }

    pub fn font_bytes(&self) -> usize {
        self.fonts
            .iter()
            .map(SubtitleFontAttachment::byte_len)
            .sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleTrackSource {
    Embedded { stream_index: i64 },
    External { uri: String },
}

impl SubtitleTrackSource {
    pub const fn embedded(stream_index: i64) -> Self {
        Self::Embedded { stream_index }
    }

    pub fn external(uri: impl Into<String>) -> Self {
        Self::External { uri: uri.into() }
    }

    pub const fn is_embedded(&self) -> bool {
        matches!(self, Self::Embedded { .. })
    }

    pub const fn is_external(&self) -> bool {
        matches!(self, Self::External { .. })
    }

    pub const fn can_remove(&self) -> bool {
        self.is_external()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrackConfig {
    pub id: i64,
    pub source: SubtitleTrackSource,
    pub language: Option<String>,
    pub title: Option<String>,
}

impl SubtitleTrackConfig {
    pub fn embedded(id: i64, stream_index: i64) -> Self {
        Self {
            id,
            source: SubtitleTrackSource::embedded(stream_index),
            language: None,
            title: None,
        }
    }

    pub fn external(id: i64, uri: impl Into<String>) -> Self {
        Self {
            id,
            source: SubtitleTrackSource::external(uri),
            language: None,
            title: None,
        }
    }

    pub const fn can_remove(&self) -> bool {
        self.source.can_remove()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleTextFormat {
    PlainText,
    Ass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleFileFormat {
    Srt,
    WebVtt,
    Ass,
}

impl SubtitleFileFormat {
    pub fn from_path(path: impl AsRef<str>) -> Option<Self> {
        Self::from_extension(subtitle_path_extension(path.as_ref())?)
    }

    /// Like [`Self::from_path`] but for a full URI: any query string or
    /// fragment is dropped first, so a signed sidecar URL such as
    /// `https://host/sub.srt?token=...` is still recognised as SRT.
    pub fn from_uri(uri: impl AsRef<str>) -> Option<Self> {
        Self::from_path(uri_path_component(uri.as_ref()))
    }

    fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "srt" => Some(Self::Srt),
            "vtt" | "webvtt" => Some(Self::WebVtt),
            "ass" | "ssa" => Some(Self::Ass),
            _ => None,
        }
    }
}

/// Strips a `?query` and `#fragment` so only the path is left. Applied before
/// extension matching because subtitle URIs are not always bare paths.
pub(crate) fn uri_path_component(uri: &str) -> &str {
    let path = uri.split_once('#').map_or(uri, |(path, _)| path);
    path.split_once('?').map_or(path, |(path, _)| path)
}

/// Returns the extension of the last path segment, or `None` when that segment
/// carries no extension at all. Splitting on the segment rather than the whole
/// string keeps a dot earlier in the path (`/a.b/subs`) from being mistaken for
/// one.
pub(crate) fn subtitle_path_extension(path: &str) -> Option<&str> {
    let leaf = path.rsplit(['/', '\\']).next().unwrap_or(path);
    leaf.rsplit_once('.').map(|(_, extension)| extension)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTextSegment {
    pub format: SubtitleTextFormat,
    pub text: String,
    pub forced: bool,
}

impl SubtitleTextSegment {
    pub fn new(format: SubtitleTextFormat, text: impl Into<String>) -> Self {
        Self {
            format,
            text: text.into(),
            forced: false,
        }
    }

    pub fn with_forced(mut self, forced: bool) -> Self {
        self.forced = forced;
        self
    }

    pub fn display_text(&self) -> String {
        match self.format {
            SubtitleTextFormat::PlainText => self.text.trim().to_string(),
            SubtitleTextFormat::Ass => ass_segment_display_text(&self.text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedSubtitleFrame {
    pub track_id: i64,
    pub start: Option<Duration>,
    pub end: Option<Duration>,
    pub text: Vec<SubtitleTextSegment>,
    pub bitmap: SubtitleFrame,
    pub forced: bool,
    pub ass_track: Option<Arc<AssTrackResources>>,
}

impl DecodedSubtitleFrame {
    pub fn new(track_id: i64, start: Option<Duration>, end: Option<Duration>) -> Self {
        Self {
            track_id,
            start,
            end,
            text: Vec::new(),
            bitmap: SubtitleFrame {
                pts: start.unwrap_or(Duration::ZERO),
                planes: Vec::new(),
            },
            forced: false,
            ass_track: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.bitmap.planes.is_empty()
    }

    pub fn push_text(&mut self, segment: SubtitleTextSegment) {
        self.forced |= segment.forced;
        if !segment.text.is_empty() {
            self.text.push(segment);
        }
    }

    pub fn push_bitmap_plane(&mut self, plane: SubtitleBitmapPlane, forced: bool) {
        self.forced |= forced;
        self.bitmap.planes.push(plane);
    }

    pub fn with_track_id(mut self, track_id: i64) -> Self {
        self.track_id = track_id;
        self
    }

    pub fn with_ass_track(mut self, resources: Option<Arc<AssTrackResources>>) -> Self {
        self.ass_track = resources;
        self
    }

    pub fn has_ass_chunks(&self) -> bool {
        self.text
            .iter()
            .any(|segment| segment.format == SubtitleTextFormat::Ass && !segment.text.is_empty())
    }

    pub fn has_text(&self) -> bool {
        self.text.iter().any(|segment| !segment.text.is_empty())
    }

    pub fn text_cues(&self, fallback_end: Duration) -> Vec<SubtitleCue> {
        let Some(start) = self.start else {
            return Vec::new();
        };
        let end = self.end.unwrap_or(fallback_end).max(start);
        self.text
            .iter()
            .filter_map(|segment| {
                let text = segment.display_text();
                (!text.is_empty()).then_some(SubtitleCue { start, end, text })
            })
            .collect()
    }

    pub fn to_ass_script(&self, fallback_end: Duration) -> Option<String> {
        decoded_subtitle_frames_to_ass_script([self], fallback_end)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleCue {
    pub start: Duration,
    pub end: Duration,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleBitmapPlane {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub canvas_width: u32,
    pub canvas_height: u32,
    pub rgba: Vec<u8>,
}

impl SubtitleBitmapPlane {
    pub fn new(x: i32, y: i32, width: u32, height: u32, rgba: Vec<u8>) -> Self {
        Self {
            x,
            y,
            width,
            height,
            canvas_width: 0,
            canvas_height: 0,
            rgba,
        }
    }

    pub fn with_canvas(mut self, width: u32, height: u32) -> Self {
        self.canvas_width = width;
        self.canvas_height = height;
        self
    }

    pub fn scaled_rect(&self, viewport_width: u32, viewport_height: u32) -> (i32, i32, u32, u32) {
        if self.canvas_width == 0
            || self.canvas_height == 0
            || (self.canvas_width == viewport_width && self.canvas_height == viewport_height)
        {
            return (self.x, self.y, self.width, self.height);
        }
        let viewport_width = viewport_width.max(1) as f64;
        let viewport_height = viewport_height.max(1) as f64;
        let canvas_width = self.canvas_width.max(1) as f64;
        let canvas_height = self.canvas_height.max(1) as f64;
        let scale = (viewport_width / canvas_width).max(viewport_height / canvas_height);
        let offset_x = (viewport_width - canvas_width * scale) * 0.5;
        let offset_y = (viewport_height - canvas_height * scale) * 0.5;
        (
            (offset_x + self.x as f64 * scale).round() as i32,
            (offset_y + self.y as f64 * scale).round() as i32,
            ((self.width as f64 * scale).round() as u32).max(1),
            ((self.height as f64 * scale).round() as u32).max(1),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtitleBitmapPlacement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl SubtitleBitmapPlacement {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn clipped_to(self, frame_width: u32, frame_height: u32) -> Option<Self> {
        let left = self.x.max(0) as i64;
        let top = self.y.max(0) as i64;
        let right = (self.x as i64 + self.width as i64).min(frame_width as i64);
        let bottom = (self.y as i64 + self.height as i64).min(frame_height as i64);
        if right <= left || bottom <= top {
            return None;
        }
        Some(Self::new(
            left as i32,
            top as i32,
            (right - left) as u32,
            (bottom - top) as u32,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleBitmapColorSpace {
    Srgb,
    Video,
}

impl Default for SubtitleBitmapColorSpace {
    fn default() -> Self {
        Self::Srgb
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleAlphaBitmap {
    pub placement: SubtitleBitmapPlacement,
    pub stride: usize,
    pub color_rgba: u32,
    pub alpha: Vec<u8>,
}

impl SubtitleAlphaBitmap {
    pub fn new(
        placement: SubtitleBitmapPlacement,
        stride: usize,
        color_rgba: u32,
        alpha: Vec<u8>,
    ) -> Self {
        Self {
            placement,
            stride: stride.max(placement.width as usize),
            color_rgba,
            alpha,
        }
    }

    pub fn required_len(&self) -> usize {
        if self.placement.height == 0 || self.placement.width == 0 {
            return 0;
        }
        self.stride
            .saturating_mul(self.placement.height.saturating_sub(1) as usize)
            .saturating_add(self.placement.width as usize)
    }

    pub fn is_valid(&self) -> bool {
        self.alpha.len() >= self.required_len()
    }

    pub fn to_rgba_plane(&self) -> Option<SubtitleBitmapPlane> {
        if !self.is_valid() {
            return None;
        }
        let width = self.placement.width as usize;
        let height = self.placement.height as usize;
        if width == 0 || height == 0 {
            return None;
        }

        let color = AssColor::from_libass_rgba(self.color_rgba);
        let mut rgba = vec![0u8; width * height * 4];
        for y in 0..height {
            let row_start = y * self.stride;
            for x in 0..width {
                let coverage = self.alpha[row_start + x];
                let alpha = multiply_u8(color.alpha, coverage);
                let pixel = &mut rgba[(y * width + x) * 4..][..4];
                pixel.copy_from_slice(&[color.red, color.green, color.blue, alpha]);
            }
        }

        Some(SubtitleBitmapPlane::new(
            self.placement.x,
            self.placement.y,
            self.placement.width,
            self.placement.height,
            rgba,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleBitmapSet {
    pub pts: Duration,
    pub frame_width: u32,
    pub frame_height: u32,
    pub color_space: SubtitleBitmapColorSpace,
    pub parts: Vec<SubtitleAlphaBitmap>,
    pub changed: bool,
}

impl SubtitleBitmapSet {
    pub fn new(pts: Duration, frame_width: u32, frame_height: u32) -> Self {
        Self {
            pts,
            frame_width,
            frame_height,
            color_space: SubtitleBitmapColorSpace::default(),
            parts: Vec::new(),
            changed: true,
        }
    }

    pub fn with_color_space(mut self, color_space: SubtitleBitmapColorSpace) -> Self {
        self.color_space = color_space;
        self
    }

    pub fn with_changed(mut self, changed: bool) -> Self {
        self.changed = changed;
        self
    }

    pub fn push(&mut self, bitmap: SubtitleAlphaBitmap) {
        if bitmap.placement.width > 0 && bitmap.placement.height > 0 {
            self.parts.push(bitmap);
        }
    }

    pub fn to_frame(&self) -> SubtitleFrame {
        let planes = self
            .parts
            .iter()
            .filter_map(SubtitleAlphaBitmap::to_rgba_plane)
            .collect();
        SubtitleFrame {
            pts: self.pts,
            planes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleFrame {
    pub pts: Duration,
    pub planes: Vec<SubtitleBitmapPlane>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtitleRenderViewport {
    pub width: u32,
    pub height: u32,
    pub storage_width: u32,
    pub storage_height: u32,
}

impl SubtitleRenderViewport {
    pub fn new(width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            storage_width: width,
            storage_height: height,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtitleRenderRequest {
    pub pts: Duration,
    pub viewport: SubtitleRenderViewport,
}

impl SubtitleRenderRequest {
    pub fn new(pts: Duration, width: u32, height: u32) -> Self {
        Self {
            pts,
            viewport: SubtitleRenderViewport::new(width, height),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleRenderBackend {
    DebugTimeline,
    Libass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleRenderOutput {
    Rgba(SubtitleFrame),
    Alpha(SubtitleBitmapSet),
}

impl SubtitleRenderOutput {
    pub fn into_rgba_frame(self) -> SubtitleFrame {
        match self {
            Self::Rgba(frame) => frame,
            Self::Alpha(bitmaps) => bitmaps.to_frame(),
        }
    }
}

pub trait SubtitleRenderer {
    fn backend(&self) -> SubtitleRenderBackend;
    fn render(&mut self, request: SubtitleRenderRequest) -> Result<SubtitleRenderOutput>;
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RawAssImage {
    pub w: i32,
    pub h: i32,
    pub stride: i32,
    pub bitmap: *const u8,
    pub color: u32,
    pub dst_x: i32,
    pub dst_y: i32,
    pub next: *const RawAssImage,
    pub image_type: i32,
}

#[cfg(feature = "libass")]
mod libass_ffi {
    use libc::{c_char, c_int, c_longlong, c_void, size_t};

    pub type AssImageType = c_int;
    pub type AssLibrary = c_void;
    pub type AssRenderer = c_void;
    pub type AssTrack = c_void;

    #[repr(C)]
    pub struct AssImage {
        pub w: c_int,
        pub h: c_int,
        pub stride: c_int,
        pub bitmap: *mut u8,
        pub color: u32,
        pub dst_x: c_int,
        pub dst_y: c_int,
        pub next: *mut AssImage,
        pub image_type: AssImageType,
    }

    pub type ErikaAssLogSink =
        unsafe extern "C" fn(opaque: *mut c_void, level: c_int, message: *const c_char);

    #[repr(C)]
    #[derive(Debug)]
    pub struct ErikaAssLogBridge {
        pub sink: Option<ErikaAssLogSink>,
        pub opaque: *mut c_void,
    }

    /// Mirrors `ASS_Style` from `ass_types.h`. libass keeps the layout stable
    /// across 0.17.x releases; `name` is stored by pointer while `font_name` is
    /// duplicated, so `name` must outlive the renderer.
    #[repr(C)]
    #[derive(Debug)]
    pub struct AssStyle {
        pub name: *mut c_char,
        pub font_name: *mut c_char,
        pub font_size: f64,
        pub primary_colour: u32,
        pub secondary_colour: u32,
        pub outline_colour: u32,
        pub back_colour: u32,
        pub bold: c_int,
        pub italic: c_int,
        pub underline: c_int,
        pub strike_out: c_int,
        pub scale_x: f64,
        pub scale_y: f64,
        pub spacing: f64,
        pub angle: f64,
        pub border_style: c_int,
        pub outline: f64,
        pub shadow: f64,
        pub alignment: c_int,
        pub margin_l: c_int,
        pub margin_r: c_int,
        pub margin_v: c_int,
        pub encoding: c_int,
        pub treat_fontname_as_pattern: c_int,
        pub blur: f64,
        pub justify: c_int,
    }

    unsafe extern "C" {
        pub fn ass_library_init() -> *mut AssLibrary;
        pub fn ass_library_done(library: *mut AssLibrary);
        pub fn ass_set_extract_fonts(library: *mut AssLibrary, extract: c_int);
        pub fn ass_add_font(
            library: *mut AssLibrary,
            name: *const c_char,
            data: *const c_char,
            data_size: c_int,
        );
        pub fn ass_renderer_init(library: *mut AssLibrary) -> *mut AssRenderer;
        pub fn ass_renderer_done(renderer: *mut AssRenderer);
        pub fn ass_set_frame_size(renderer: *mut AssRenderer, width: c_int, height: c_int);
        pub fn ass_set_storage_size(renderer: *mut AssRenderer, width: c_int, height: c_int);
        pub fn ass_set_font_scale(renderer: *mut AssRenderer, font_scale: f64);
        pub fn ass_set_fonts(
            renderer: *mut AssRenderer,
            default_font: *const c_char,
            default_family: *const c_char,
            default_font_provider: c_int,
            config: *const c_char,
            update: c_int,
        );
        pub fn ass_set_cache_limits(
            renderer: *mut AssRenderer,
            glyph_max: c_int,
            bitmap_max_size: c_int,
        );
        pub fn ass_set_selective_style_override_enabled(renderer: *mut AssRenderer, bits: c_int);
        pub fn ass_set_selective_style_override(renderer: *mut AssRenderer, style: *mut AssStyle);
        pub fn ass_read_memory(
            library: *mut AssLibrary,
            buffer: *mut c_char,
            buffer_size: size_t,
            codepage: *const c_char,
        ) -> *mut AssTrack;
        pub fn ass_new_track(library: *mut AssLibrary) -> *mut AssTrack;
        pub fn ass_process_codec_private(track: *mut AssTrack, data: *const c_char, size: c_int);
        pub fn ass_process_chunk(
            track: *mut AssTrack,
            data: *const c_char,
            size: c_int,
            timecode: c_longlong,
            duration: c_longlong,
        );
        pub fn ass_flush_events(track: *mut AssTrack);
        pub fn ass_free_track(track: *mut AssTrack);
        pub fn ass_render_frame(
            renderer: *mut AssRenderer,
            track: *mut AssTrack,
            now: c_longlong,
            detect_change: *mut c_int,
        ) -> *mut AssImage;
        pub fn erika_ass_install_log_bridge(
            library: *mut AssLibrary,
            bridge: *mut ErikaAssLogBridge,
        );
    }
}

/// Style name handed to libass for the override style. libass stores this
/// pointer without copying it, so it has to be `'static`.
#[cfg(feature = "libass")]
const ASS_OVERRIDE_STYLE_NAME: &CStr = c"Erika";

#[cfg(feature = "libass")]
const ASS_FONTPROVIDER_NONE: libc::c_int = 0;
#[cfg(feature = "libass")]
const ASS_FONTPROVIDER_AUTODETECT: libc::c_int = 1;
#[cfg(feature = "libass")]
const ASS_FONTPROVIDER_CORETEXT: libc::c_int = 2;

#[cfg(feature = "libass")]
const MAX_CUSTOM_SUBTITLE_FONT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibassRenderConfig {
    pub glyph_cache_limit: i32,
    pub bitmap_cache_limit_mb: i32,
}

impl Default for LibassRenderConfig {
    fn default() -> Self {
        Self {
            glyph_cache_limit: 0,
            bitmap_cache_limit_mb: 0,
        }
    }
}

/// Text colour liberika falls back to when nothing else specifies one, as
/// `0xRRGGBBAA` with alpha 255 meaning fully opaque.
pub const DEFAULT_SUBTITLE_PRIMARY_COLOR_RGBA: u32 = 0xffff_ffff;
/// Outline/shadow colour fallback, half-transparent black.
pub const DEFAULT_SUBTITLE_OUTLINE_COLOR_RGBA: u32 = 0x0000_007f;
/// Base subtitle font size in ASS script units, before the viewer's scale.
pub const DEFAULT_SUBTITLE_FONT_SIZE: f64 = DEFAULT_ASS_FONT_SIZE;
/// Base subtitle outline width in ASS script units, before the viewer's scale.
pub const DEFAULT_SUBTITLE_OUTLINE_WIDTH: f64 = DEFAULT_ASS_OUTLINE;
pub const SUBTITLE_OVERRIDE_FONT_SIZE_FIELDS: u32 = 1 << 2;
pub const SUBTITLE_OVERRIDE_FONT_NAME: u32 = 1 << 3;
pub const SUBTITLE_OVERRIDE_COLORS: u32 = 1 << 4;
pub const SUBTITLE_OVERRIDE_ATTRIBUTES: u32 = 1 << 5;
pub const SUBTITLE_OVERRIDE_BORDER: u32 = 1 << 6;
pub const SUBTITLE_OVERRIDE_ALIGNMENT: u32 = 1 << 7;
pub const SUBTITLE_OVERRIDE_MARGINS: u32 = 1 << 8;
pub const SUBTITLE_OVERRIDE_BLUR: u32 = 1 << 11;
pub const SUBTITLE_OVERRIDE_LEGACY_FORCE: u32 = SUBTITLE_OVERRIDE_FONT_SIZE_FIELDS
    | SUBTITLE_OVERRIDE_FONT_NAME
    | SUBTITLE_OVERRIDE_COLORS
    | SUBTITLE_OVERRIDE_BORDER;
pub const SUBTITLE_OVERRIDE_ALL: u32 = SUBTITLE_OVERRIDE_LEGACY_FORCE
    | SUBTITLE_OVERRIDE_ATTRIBUTES
    | SUBTITLE_OVERRIDE_ALIGNMENT
    | SUBTITLE_OVERRIDE_MARGINS
    | SUBTITLE_OVERRIDE_BLUR;

/// User-chosen subtitle look. Empty strings mean "keep liberika's default", so
/// [`SubtitleStyleConfig::default`] reproduces the built-in style exactly.
///
/// The font and colours act as *fallbacks*: a container ASS script keeps its
/// own styling, and these only fill in what the script leaves open (or what the
/// system cannot resolve). Set the corresponding `override_mask` bits to push
/// configured fields onto dialogue events that specify their own style.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleStyleConfig {
    /// Family libass resolves when a script names no font, or names one that is
    /// neither attached to the container nor installed.
    pub font_family: String,
    /// Font file loaded into libass and offered as the last-resort face.
    pub font_file_path: String,
    /// Text colour as `0xRRGGBBAA`.
    pub primary_color_rgba: u32,
    /// Outline and shadow colour as `0xRRGGBBAA`.
    pub outline_color_rgba: u32,
    /// Base font size in ASS script units, before the viewer's subtitle scale.
    pub font_size: f64,
    /// Base outline (border) width in ASS script units, before the scale.
    pub outline_width: f64,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike_out: bool,
    pub spacing: f64,
    pub scale_x_percent: f64,
    pub scale_y_percent: f64,
    pub border_style: i32,
    pub shadow_depth: f64,
    pub blur: f64,
    pub alignment: i32,
    /// Left margin. Unlike the sizes above, libass does not normalize override
    /// margins against the script resolution, so these are script units rather
    /// than pixels: on plain-text subtitles, whose generated script is authored
    /// at the frame size, they are pixels, but on a container ASS track they are
    /// relative to whatever `PlayResY` the script declares.
    pub margin_left: i32,
    /// Right margin, in the same units as [`Self::margin_left`].
    pub margin_right: i32,
    /// Bottom (or top, per `alignment`) margin, in the same units as
    /// [`Self::margin_left`].
    pub margin_vertical: i32,
    pub override_mask: u32,
}

impl Default for SubtitleStyleConfig {
    fn default() -> Self {
        Self {
            font_family: String::new(),
            font_file_path: String::new(),
            primary_color_rgba: DEFAULT_SUBTITLE_PRIMARY_COLOR_RGBA,
            outline_color_rgba: DEFAULT_SUBTITLE_OUTLINE_COLOR_RGBA,
            font_size: DEFAULT_ASS_FONT_SIZE,
            outline_width: DEFAULT_ASS_OUTLINE,
            bold: false,
            italic: false,
            underline: false,
            strike_out: false,
            spacing: 0.0,
            scale_x_percent: 100.0,
            scale_y_percent: 100.0,
            border_style: 1,
            shadow_depth: 0.0,
            blur: 0.0,
            alignment: 2,
            margin_left: 48,
            margin_right: 48,
            margin_vertical: 54,
            override_mask: 0,
        }
    }
}

impl SubtitleStyleConfig {
    /// Trims caller-provided paths and family names, and clamps the metrics to
    /// a range libass can render. An empty string stays the "unset" marker; a
    /// non-finite metric falls back to liberika's default.
    pub fn normalized(mut self) -> Self {
        self.font_family = self.font_family.trim().to_string();
        self.font_file_path = self.font_file_path.trim().to_string();
        self.font_size = if self.font_size.is_finite() {
            self.font_size.clamp(MIN_ASS_FONT_SIZE, MAX_ASS_FONT_SIZE)
        } else {
            DEFAULT_ASS_FONT_SIZE
        };
        self.outline_width = if self.outline_width.is_finite() {
            self.outline_width.clamp(0.0, MAX_ASS_OUTLINE)
        } else {
            DEFAULT_ASS_OUTLINE
        };
        self.spacing = normalize_ass_metric(self.spacing, 0.0, -100.0, 100.0);
        self.scale_x_percent = normalize_ass_metric(self.scale_x_percent, 100.0, 1.0, 1000.0);
        self.scale_y_percent = normalize_ass_metric(self.scale_y_percent, 100.0, 1.0, 1000.0);
        self.shadow_depth = normalize_ass_metric(self.shadow_depth, 0.0, 0.0, MAX_ASS_OUTLINE);
        self.blur = normalize_ass_metric(self.blur, 0.0, 0.0, 100.0);
        self.border_style = if matches!(self.border_style, 1 | 3) {
            self.border_style
        } else {
            1
        };
        self.alignment = if (1..=9).contains(&self.alignment) {
            self.alignment
        } else {
            2
        };
        self.margin_left = self.margin_left.clamp(0, 10_000);
        self.margin_right = self.margin_right.clamp(0, 10_000);
        self.margin_vertical = self.margin_vertical.clamp(0, 10_000);
        self.override_mask &= SUBTITLE_OVERRIDE_ALL;
        self
    }

    pub fn font_family(&self) -> Option<&str> {
        let family = self.font_family.trim();
        (!family.is_empty()).then_some(family)
    }

    pub fn font_file_path(&self) -> Option<&str> {
        let path = self.font_file_path.trim();
        (!path.is_empty()).then_some(path)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleAssStyle {
    pub font_scale: f64,
    pub play_res_width: u32,
    pub play_res_height: u32,
    pub style: SubtitleStyleConfig,
    pub memory_fonts: Arc<[SubtitleFontAttachment]>,
    pub memory_font_revision: u64,
}

impl Default for SubtitleAssStyle {
    fn default() -> Self {
        Self {
            font_scale: 1.0,
            play_res_width: 1920,
            play_res_height: 1080,
            style: SubtitleStyleConfig::default(),
            memory_fonts: Arc::from([]),
            memory_font_revision: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibassRenderOperation {
    SetFrameSize { width: u32, height: u32 },
    SetStorageSize { width: u32, height: u32 },
    SetCacheLimits { glyphs: i32, bitmap_mb: i32 },
    RenderFrame { timestamp_ms: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibassRenderPlan {
    pub request: SubtitleRenderRequest,
    pub config: LibassRenderConfig,
    pub operations: Vec<LibassRenderOperation>,
}

impl LibassRenderPlan {
    pub fn new(request: SubtitleRenderRequest, config: LibassRenderConfig) -> Self {
        let viewport = request.viewport;
        Self {
            request,
            config,
            operations: vec![
                LibassRenderOperation::SetFrameSize {
                    width: viewport.width,
                    height: viewport.height,
                },
                LibassRenderOperation::SetStorageSize {
                    width: viewport.storage_width,
                    height: viewport.storage_height,
                },
                LibassRenderOperation::SetCacheLimits {
                    glyphs: config.glyph_cache_limit,
                    bitmap_mb: config.bitmap_cache_limit_mb,
                },
                LibassRenderOperation::RenderFrame {
                    timestamp_ms: duration_to_millis_i64(request.pts),
                },
            ],
        }
    }
}

#[cfg(feature = "libass")]
#[derive(Debug)]
pub struct LibassSubtitleRenderer {
    runtime: LibassRuntime,
    track: NonNull<libass_ffi::AssTrack>,
    source: LibassTrackSource,
    chunks: Vec<(String, Duration, Option<Duration>)>,
    config: LibassRenderConfig,
    font_scale: f64,
    override_font_scale: f64,
    play_res_height: u32,
    style: SubtitleStyleConfig,
    memory_fonts: Arc<[SubtitleFontAttachment]>,
}

#[cfg(feature = "libass")]
#[derive(Debug, Clone)]
enum LibassTrackSource {
    Script(Arc<[u8]>),
    Track {
        track_id: i64,
        resources: AssTrackResources,
    },
}

/// Font selection currently installed in libass. `ass_set_fonts` rebuilds the
/// font selector, which re-parses every attachment the library holds, so it may
/// only run when this selection actually changes.
#[cfg(feature = "libass")]
#[derive(Debug, PartialEq)]
struct LibassFontSelection {
    family: CString,
    default_font: Option<CString>,
}

#[cfg(feature = "libass")]
#[derive(Debug)]
struct LibassRuntime {
    library: NonNull<libass_ffi::AssLibrary>,
    renderer: NonNull<libass_ffi::AssRenderer>,
    track_id: i64,
    loaded_font_files: HashMap<String, CustomFontFileIdentity>,
    observed_font_files: HashMap<String, CustomFontFileIdentity>,
    fonts: Option<LibassFontSelection>,
    /// Counts `ass_set_fonts` calls. Only meaningful to the tests that pin how
    /// rarely the font selector is rebuilt.
    fonts_generation: u64,
    custom_font_loads: u64,
    _log_context: Box<LibassLogContext>,
    _log_bridge: Box<libass_ffi::ErikaAssLogBridge>,
}

#[cfg(feature = "libass")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CustomFontFileIdentity {
    modified: SystemTime,
    size: u64,
}

#[cfg(feature = "libass")]
#[derive(Debug)]
struct LibassLogContext {
    track_id: i64,
    seen: Mutex<HashSet<String>>,
}

#[cfg(feature = "libass")]
impl LibassRuntime {
    fn new(
        track_id: i64,
        fonts: &[SubtitleFontAttachment],
        memory_fonts: &[SubtitleFontAttachment],
        config: LibassRenderConfig,
        style: &SubtitleStyleConfig,
    ) -> Result<Self> {
        let mut runtime = unsafe {
            let library = NonNull::new(libass_ffi::ass_library_init()).ok_or_else(|| {
                SubtitleError::Libass("failed to initialize libass library".to_string())
            })?;
            let mut log_context = Box::new(LibassLogContext {
                track_id,
                seen: Mutex::new(HashSet::new()),
            });
            let mut log_bridge = Box::new(libass_ffi::ErikaAssLogBridge {
                sink: Some(libass_log_sink),
                opaque: (&mut *log_context as *mut LibassLogContext).cast(),
            });
            libass_ffi::erika_ass_install_log_bridge(library.as_ptr(), &mut *log_bridge);
            libass_ffi::ass_set_extract_fonts(library.as_ptr(), 1);
            add_bundled_ass_fallback_font(library.as_ptr(), track_id);
            add_attached_ass_fonts(library.as_ptr(), track_id, memory_fonts);
            add_attached_ass_fonts(library.as_ptr(), track_id, fonts);

            let Some(renderer) = NonNull::new(libass_ffi::ass_renderer_init(library.as_ptr()))
            else {
                libass_ffi::ass_library_done(library.as_ptr());
                return Err(SubtitleError::Libass(
                    "failed to initialize libass renderer".to_string(),
                ));
            };
            libass_ffi::ass_set_cache_limits(
                renderer.as_ptr(),
                config.glyph_cache_limit,
                config.bitmap_cache_limit_mb,
            );
            Self {
                library,
                renderer,
                track_id,
                loaded_font_files: HashMap::new(),
                observed_font_files: HashMap::new(),
                fonts: None,
                fonts_generation: 0,
                custom_font_loads: 0,
                _log_context: log_context,
                _log_bridge: log_bridge,
            }
        };
        runtime.configure_style(style, memory_fonts, 1.0, 288);
        Ok(runtime)
    }

    /// Pushes the user style into libass: the custom face (if any) becomes a
    /// known font and the last-resort font path, the custom family becomes the
    /// default family, and `override_mask` decides which dialogue styling is
    /// replaced instead of merely backfilled.
    ///
    /// Only the override metrics depend on `font_scale` and `play_res_height`,
    /// and those are cheap to re-push; the font selection is left alone unless
    /// the family or the font file changed.
    fn configure_style(
        &mut self,
        style: &SubtitleStyleConfig,
        memory_fonts: &[SubtitleFontAttachment],
        font_scale: f64,
        play_res_height: u32,
    ) {
        let fonts_changed = self.configure_fonts(style, memory_fonts);
        let override_bits = self.configure_style_override(style, font_scale, play_res_height);
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "subtitle_font_fallback",
                "stage": "configured",
                "trackId": self.track_id,
                "defaultFamily": self.default_family().to_string_lossy(),
                "fontProvider": default_ass_font_provider(),
                "customFontFile": style.font_file_path(),
                "fontsChanged": fonts_changed,
                "fontsGeneration": self.fonts_generation,
                "overrideBits": override_bits,
            })
            .to_string(),
        );
    }

    /// Installs the style's font selection, skipping libass entirely when it is
    /// already the one in effect. `ass_set_fonts` frees the font selector and
    /// builds a new one, which re-parses every font the library holds (the
    /// bundled fallback, every container attachment, every custom face) and
    /// empties the font, metrics and bitmap caches — far too much work to repeat
    /// on a viewport resize. Returns whether libass was reconfigured.
    fn configure_fonts(
        &mut self,
        style: &SubtitleStyleConfig,
        memory_fonts: &[SubtitleFontAttachment],
    ) -> bool {
        if let Some(path) = style.font_file_path() {
            self.load_custom_font_file(path);
        }
        let mut fallback_families = Vec::new();
        if let Some(family) = style.font_family() {
            fallback_families.push(family.to_string());
        }
        for font in memory_fonts {
            fallback_families.extend(font.families.iter().cloned());
        }
        fallback_families.push(BUNDLED_ASS_FALLBACK_FONT_FAMILY.to_string());
        let mut seen = HashSet::new();
        fallback_families.retain(|family| !family.is_empty() && seen.insert(family.clone()));
        let family = CString::new(fallback_families.join("\u{1f}"))
            .unwrap_or_else(|_| default_ass_font_family_cstr().to_owned());
        let selection = LibassFontSelection {
            family,
            default_font: style
                .font_file_path()
                .filter(|path| self.custom_font_file_is_current(path))
                .and_then(|path| CString::new(path).ok()),
        };
        if self.fonts.as_ref() == Some(&selection) {
            return false;
        }
        unsafe {
            libass_ffi::ass_set_fonts(
                self.renderer.as_ptr(),
                selection
                    .default_font
                    .as_ref()
                    .map_or(std::ptr::null(), |path| path.as_ptr()),
                selection.family.as_ptr(),
                default_ass_font_provider(),
                std::ptr::null(),
                1,
            );
        }
        self.fonts = Some(selection);
        self.fonts_generation += 1;
        true
    }

    /// Family libass falls back to, which is also the family the override style
    /// requests. Valid once [`Self::configure_fonts`] has run.
    fn default_family(&self) -> &CStr {
        match self.fonts.as_ref() {
            Some(fonts) => &fonts.family,
            None => default_ass_font_family_cstr(),
        }
    }

    /// Enables libass' selective style override when the user asked for their
    /// font and colours to win over the script's own dialogue styles. Returns
    /// the enabled bits so callers can report them.
    fn configure_style_override(
        &self,
        style: &SubtitleStyleConfig,
        font_scale: f64,
        play_res_height: u32,
    ) -> libc::c_int {
        let family = self.default_family();
        let bits = (style.override_mask & SUBTITLE_OVERRIDE_ALL) as libc::c_int;
        if bits != 0 {
            unsafe {
                libass_ffi::ass_set_selective_style_override_enabled(self.renderer.as_ptr(), 0);
            }
            let mut override_style = libass_ffi::AssStyle {
                name: ASS_OVERRIDE_STYLE_NAME.as_ptr().cast_mut(),
                font_name: family.as_ptr().cast_mut(),
                font_size: selective_override_metric(style.font_size, font_scale, play_res_height),
                primary_colour: libass_style_color(style.primary_color_rgba),
                secondary_colour: libass_style_color(style.primary_color_rgba),
                outline_colour: libass_style_color(style.outline_color_rgba),
                back_colour: libass_style_color(style.outline_color_rgba),
                bold: i32::from(style.bold),
                italic: i32::from(style.italic),
                underline: i32::from(style.underline),
                strike_out: i32::from(style.strike_out),
                scale_x: style.scale_x_percent / 100.0,
                scale_y: style.scale_y_percent / 100.0,
                spacing: selective_override_metric(style.spacing, font_scale, play_res_height),
                angle: 0.0,
                border_style: style.border_style,
                outline: selective_override_metric(
                    style.outline_width,
                    font_scale,
                    play_res_height,
                ),
                shadow: selective_override_metric(style.shadow_depth, font_scale, play_res_height),
                alignment: style.alignment,
                margin_l: style.margin_left,
                margin_r: style.margin_right,
                margin_v: style.margin_vertical,
                encoding: 1,
                treat_fontname_as_pattern: 0,
                blur: selective_override_metric(style.blur, font_scale, play_res_height),
                justify: 0,
            };
            unsafe {
                libass_ffi::ass_set_selective_style_override(
                    self.renderer.as_ptr(),
                    &mut override_style,
                );
            }
        }
        unsafe {
            libass_ffi::ass_set_selective_style_override_enabled(self.renderer.as_ptr(), bits);
        }
        bits
    }

    /// Loads a user-selected font file once per runtime. libass copies the
    /// bytes, so the buffer does not have to outlive the call.
    fn load_custom_font_file(&mut self, path: &str) -> bool {
        let reject = |reason: &str, bytes: usize| {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_custom_font",
                    "stage": "rejected",
                    "trackId": self.track_id,
                    "path": path,
                    "bytes": bytes,
                    "reason": reason,
                })
                .to_string(),
            );
        };
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) => {
                reject(&error.to_string(), 0);
                return false;
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                reject(&error.to_string(), 0);
                return false;
            }
        };
        if !metadata.is_file() {
            reject("custom font path is not a regular file", 0);
            return false;
        }
        let size = metadata.len();
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(error) => {
                reject(
                    &error.to_string(),
                    usize::try_from(size).unwrap_or(usize::MAX),
                );
                return false;
            }
        };
        let identity = CustomFontFileIdentity { modified, size };
        self.observed_font_files.insert(path.to_string(), identity);
        if size == 0 {
            reject("custom font file is empty", 0);
            return false;
        }
        if size > MAX_CUSTOM_SUBTITLE_FONT_BYTES {
            reject(
                "custom font byte limit exceeded",
                usize::try_from(size).unwrap_or(usize::MAX),
            );
            return false;
        }
        if self.loaded_font_files.get(path) == Some(&identity) {
            return false;
        }
        let mut data = Vec::with_capacity(size as usize);
        let read_result = (&mut file)
            .take(MAX_CUSTOM_SUBTITLE_FONT_BYTES + 1)
            .read_to_end(&mut data);
        if let Err(error) = read_result {
            reject(&error.to_string(), data.len());
            return false;
        }
        if data.len() as u64 != size {
            reject("custom font changed while being read", data.len());
            return false;
        }
        let current_identity = file.metadata().ok().and_then(|metadata| {
            Some(CustomFontFileIdentity {
                modified: metadata.modified().ok()?,
                size: metadata.len(),
            })
        });
        if current_identity != Some(identity) {
            reject("custom font changed while being read", data.len());
            return false;
        }
        let mut database = fontdb::Database::new();
        database.load_font_data(data.clone());
        if database.faces().next().is_none() {
            reject("font parser found no faces", data.len());
            return false;
        }
        let Ok(data_size) = libc::c_int::try_from(data.len()) else {
            reject("custom font exceeds libass integer range", data.len());
            return false;
        };
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path);
        let Ok(name) = CString::new(name) else {
            reject("custom font name contains an interior NUL", data.len());
            return false;
        };
        unsafe {
            libass_ffi::ass_add_font(
                self.library.as_ptr(),
                name.as_ptr(),
                data.as_ptr().cast(),
                data_size,
            );
        }
        self.loaded_font_files.insert(path.to_string(), identity);
        self.custom_font_loads = self.custom_font_loads.saturating_add(1);
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "subtitle_custom_font",
                "stage": "loaded",
                "trackId": self.track_id,
                "path": path,
                "bytes": data.len(),
            })
            .to_string(),
        );
        true
    }

    fn custom_font_file_is_current(&self, path: &str) -> bool {
        let Some(loaded) = self.loaded_font_files.get(path) else {
            return false;
        };
        custom_font_file_identity(path).is_some_and(|identity| identity == *loaded)
    }
}

#[cfg(feature = "libass")]
fn custom_font_file_identity(path: &str) -> Option<CustomFontFileIdentity> {
    let metadata = File::open(path).ok()?.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(CustomFontFileIdentity {
        modified: metadata.modified().ok()?,
        size: metadata.len(),
    })
}

#[cfg(feature = "libass")]
impl Drop for LibassRuntime {
    fn drop(&mut self) {
        unsafe {
            libass_ffi::ass_renderer_done(self.renderer.as_ptr());
            libass_ffi::ass_library_done(self.library.as_ptr());
        }
    }
}

#[cfg(feature = "libass")]
impl LibassSubtitleRenderer {
    pub fn from_ass_script(script: impl AsRef<[u8]>, config: LibassRenderConfig) -> Result<Self> {
        Self::from_ass_script_with_style(script, config, &SubtitleStyleConfig::default())
    }

    pub fn from_ass_script_with_style(
        script: impl AsRef<[u8]>,
        config: LibassRenderConfig,
        style: &SubtitleStyleConfig,
    ) -> Result<Self> {
        Self::from_ass_script_with_style_and_fonts(script, config, style, Arc::from([]))
    }

    pub fn from_ass_script_with_style_and_fonts(
        script: impl AsRef<[u8]>,
        config: LibassRenderConfig,
        style: &SubtitleStyleConfig,
        memory_fonts: Arc<[SubtitleFontAttachment]>,
    ) -> Result<Self> {
        let script = Arc::<[u8]>::from(script.as_ref());
        if script.is_empty() {
            return Err(SubtitleError::Libass("ASS script is empty".to_string()));
        }

        let style = style.clone().normalized();
        let mut script_buffer = script.to_vec();
        let runtime = LibassRuntime::new(-1, &[], &memory_fonts, config, &style)?;
        unsafe {
            let Some(track) = NonNull::new(libass_ffi::ass_read_memory(
                runtime.library.as_ptr(),
                script_buffer.as_mut_ptr().cast(),
                script_buffer.len(),
                std::ptr::null(),
            )) else {
                return Err(SubtitleError::Libass(
                    "failed to parse ASS script with libass".to_string(),
                ));
            };
            Ok(Self {
                runtime,
                track,
                source: LibassTrackSource::Script(script),
                chunks: Vec::new(),
                config,
                font_scale: 1.0,
                override_font_scale: 1.0,
                play_res_height: 288,
                style,
                memory_fonts,
            })
        }
    }

    pub fn from_ass_track(
        track_id: i64,
        resources: &AssTrackResources,
        config: LibassRenderConfig,
    ) -> Result<Self> {
        Self::from_ass_track_with_style(
            track_id,
            resources,
            config,
            &SubtitleStyleConfig::default(),
        )
    }

    pub fn from_ass_track_with_style(
        track_id: i64,
        resources: &AssTrackResources,
        config: LibassRenderConfig,
        style: &SubtitleStyleConfig,
    ) -> Result<Self> {
        Self::from_ass_track_with_style_and_fonts(track_id, resources, config, style, Arc::from([]))
    }

    pub fn from_ass_track_with_style_and_fonts(
        track_id: i64,
        resources: &AssTrackResources,
        config: LibassRenderConfig,
        style: &SubtitleStyleConfig,
        memory_fonts: Arc<[SubtitleFontAttachment]>,
    ) -> Result<Self> {
        if resources.codec_private.is_empty() {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_ass_track",
                    "stage": "codec_private_missing",
                    "trackId": track_id,
                    "sourceStreamIndex": resources.source_stream_index,
                })
                .to_string(),
            );
            return Err(SubtitleError::Libass(
                "ASS CodecPrivate is empty".to_string(),
            ));
        }
        let private_size = i32::try_from(resources.codec_private.len()).map_err(|_| {
            SubtitleError::Libass("ASS CodecPrivate exceeds libass integer range".to_string())
        })?;
        let style = style.clone().normalized();
        let runtime =
            LibassRuntime::new(track_id, &resources.fonts, &memory_fonts, config, &style)?;
        unsafe {
            let track = NonNull::new(libass_ffi::ass_new_track(runtime.library.as_ptr()))
                .ok_or_else(|| SubtitleError::Libass("failed to allocate ASS track".to_string()))?;
            libass_ffi::ass_process_codec_private(
                track.as_ptr(),
                resources.codec_private.as_ptr().cast(),
                private_size,
            );
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_ass_track",
                    "stage": "loaded",
                    "trackId": track_id,
                    "sourceStreamIndex": resources.source_stream_index,
                    "headerBytes": resources.codec_private.len(),
                    "fontCount": resources.fonts.len(),
                    "fontBytes": resources.font_bytes(),
                })
                .to_string(),
            );
            Ok(Self {
                runtime,
                track,
                source: LibassTrackSource::Track {
                    track_id,
                    resources: resources.clone(),
                },
                chunks: Vec::new(),
                config,
                font_scale: 1.0,
                override_font_scale: 1.0,
                play_res_height: 288,
                style,
                memory_fonts,
            })
        }
    }

    pub fn process_chunk(
        &mut self,
        chunk: &str,
        start: Duration,
        end: Option<Duration>,
    ) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let chunk_size = i32::try_from(chunk.len()).map_err(|_| {
            SubtitleError::Libass("ASS event chunk exceeds libass integer range".to_string())
        })?;
        let start_ms = duration_to_millis_i64(start);
        let end_ms = end.map(duration_to_millis_i64).unwrap_or(i64::MAX);
        let duration_ms = end_ms.saturating_sub(start_ms).max(1);
        unsafe {
            libass_ffi::ass_process_chunk(
                self.track.as_ptr(),
                chunk.as_ptr().cast(),
                chunk_size,
                start_ms,
                duration_ms,
            );
        }
        self.chunks.push((chunk.to_string(), start, end));
        Ok(())
    }

    pub fn flush_events(&mut self) {
        unsafe { libass_ffi::ass_flush_events(self.track.as_ptr()) };
        self.chunks.clear();
    }

    /// Sets the scale libass applies to the whole script, including whatever the
    /// selective style override replaced. Applied straight from
    /// [`SubtitleRenderer::render`], so it needs no reconfiguration here.
    pub fn set_font_scale(&mut self, scale: f64) {
        self.font_scale = normalize_ass_font_scale(scale);
    }

    /// Sets the scale folded into the override metrics, for the plain-text path
    /// where the generated header bakes the scale into a style the override then
    /// replaces outright. Leave it at `1.0` on tracks that carry their own
    /// script, where [`Self::set_font_scale`] already covers the override.
    pub fn set_override_font_scale(&mut self, scale: f64) {
        let scale = normalize_ass_font_scale(scale);
        if self.override_font_scale == scale {
            return;
        }
        self.override_font_scale = scale;
        self.runtime.configure_style(
            &self.style,
            &self.memory_fonts,
            self.override_font_scale,
            self.play_res_height,
        );
    }

    /// Logical height the override metrics are normalized against: the video's
    /// height before subtitle supersampling, not the script's own `PlayResY`. libass
    /// scales the override style by `PlayResY / 288`, and cancelling that against
    /// the frame height is what makes the metrics land on the same pixels as the
    /// non-override baseline whatever `PlayResY` a script declares.
    pub fn set_play_res_height(&mut self, play_res_height: u32) {
        let play_res_height = play_res_height.max(1);
        if self.play_res_height == play_res_height {
            return;
        }
        self.play_res_height = play_res_height;
        self.runtime.configure_style(
            &self.style,
            &self.memory_fonts,
            self.override_font_scale,
            self.play_res_height,
        );
    }

    /// Re-applies the user font and colours. Cheap and idempotent: an unchanged
    /// style skips libass entirely, so callers can push the current style on
    /// every frame.
    pub fn set_style(&mut self, style: &SubtitleStyleConfig) {
        let style = style.clone().normalized();
        let font_path_changed = self.style.font_file_path() != style.font_file_path();
        let loaded_font_changed = style.font_file_path().is_some_and(|path| {
            let current = custom_font_file_identity(path);
            self.runtime.observed_font_files.get(path).copied() != current
                || current.is_none() && self.runtime.loaded_font_files.contains_key(path)
        });
        if font_path_changed || loaded_font_changed {
            if let Err(error) = self.rebuild_for_style(&style) {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "subtitle_custom_font",
                        "stage": "runtime_rebuild_failed",
                        "trackId": self.runtime.track_id,
                        "path": style.font_file_path(),
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
            }
            return;
        }
        if self.style == style {
            return;
        }
        self.runtime.configure_style(
            &style,
            &self.memory_fonts,
            self.override_font_scale,
            self.play_res_height,
        );
        self.style = style;
    }

    fn rebuild_for_style(&mut self, style: &SubtitleStyleConfig) -> Result<()> {
        let mut replacement = match &self.source {
            LibassTrackSource::Script(script) => Self::from_ass_script_with_style_and_fonts(
                script,
                self.config,
                style,
                self.memory_fonts.clone(),
            )?,
            LibassTrackSource::Track {
                track_id,
                resources,
            } => Self::from_ass_track_with_style_and_fonts(
                *track_id,
                resources,
                self.config,
                style,
                self.memory_fonts.clone(),
            )?,
        };
        for (chunk, start, end) in &self.chunks {
            replacement.process_chunk(chunk, *start, *end)?;
        }
        replacement.font_scale = self.font_scale;
        replacement.override_font_scale = self.override_font_scale;
        replacement.play_res_height = self.play_res_height;
        replacement.runtime.configure_style(
            style,
            &replacement.memory_fonts,
            replacement.override_font_scale,
            replacement.play_res_height,
        );
        *self = replacement;
        Ok(())
    }

    pub fn style(&self) -> &SubtitleStyleConfig {
        &self.style
    }

    pub fn config(&self) -> LibassRenderConfig {
        self.config
    }

    pub fn render_plan(&self, request: SubtitleRenderRequest) -> LibassRenderPlan {
        LibassRenderPlan::new(request, self.config)
    }
}

#[cfg(feature = "libass")]
impl Drop for LibassSubtitleRenderer {
    fn drop(&mut self) {
        unsafe {
            libass_ffi::ass_free_track(self.track.as_ptr());
        }
    }
}

#[cfg(feature = "libass")]
impl SubtitleRenderer for LibassSubtitleRenderer {
    fn backend(&self) -> SubtitleRenderBackend {
        SubtitleRenderBackend::Libass
    }

    fn render(&mut self, request: SubtitleRenderRequest) -> Result<SubtitleRenderOutput> {
        let viewport = request.viewport;
        let frame_width = libass_dimension(viewport.width, "frame width")?;
        let frame_height = libass_dimension(viewport.height, "frame height")?;
        let storage_width = libass_dimension(viewport.storage_width, "storage width")?;
        let storage_height = libass_dimension(viewport.storage_height, "storage height")?;
        let timestamp_ms = duration_to_millis_i64(request.pts);

        unsafe {
            libass_ffi::ass_set_frame_size(
                self.runtime.renderer.as_ptr(),
                frame_width,
                frame_height,
            );
            libass_ffi::ass_set_storage_size(
                self.runtime.renderer.as_ptr(),
                storage_width,
                storage_height,
            );
            libass_ffi::ass_set_font_scale(self.runtime.renderer.as_ptr(), self.font_scale);
            libass_ffi::ass_set_cache_limits(
                self.runtime.renderer.as_ptr(),
                self.config.glyph_cache_limit,
                self.config.bitmap_cache_limit_mb,
            );

            let mut changed = 0;
            let images = libass_ffi::ass_render_frame(
                self.runtime.renderer.as_ptr(),
                self.track.as_ptr(),
                timestamp_ms,
                &mut changed,
            );
            Ok(SubtitleRenderOutput::Alpha(import_libass_image_list(
                request.pts,
                viewport.width,
                viewport.height,
                images,
                changed != 0,
            )?))
        }
    }
}

pub struct LibassImageImporter;

impl LibassImageImporter {
    pub unsafe fn import_raw_list(
        pts: Duration,
        frame_width: u32,
        frame_height: u32,
        first: *const RawAssImage,
        changed: bool,
    ) -> Result<SubtitleBitmapSet> {
        let mut set = SubtitleBitmapSet::new(pts, frame_width, frame_height)
            .with_color_space(SubtitleBitmapColorSpace::Video)
            .with_changed(changed);
        let mut current = first;
        let mut count = 0usize;

        while !current.is_null() {
            if count >= RAW_ASS_IMAGE_LIST_LIMIT {
                return Err(SubtitleError::BitmapListTooLong);
            }
            let image = unsafe { &*current };
            if image.w > 0 && image.h > 0 {
                let bitmap = unsafe { raw_ass_image_to_alpha_bitmap(image)? };
                set.push(bitmap);
            }
            current = image.next;
            count += 1;
        }

        Ok(set)
    }
}

#[cfg(feature = "libass")]
fn libass_dimension(value: u32, label: &str) -> Result<libc::c_int> {
    i32::try_from(value)
        .map_err(|_| SubtitleError::Libass(format!("{label} exceeds libass integer range")))
}

#[cfg(feature = "libass")]
unsafe fn import_libass_image_list(
    pts: Duration,
    frame_width: u32,
    frame_height: u32,
    first: *mut libass_ffi::AssImage,
    changed: bool,
) -> Result<SubtitleBitmapSet> {
    let mut set = SubtitleBitmapSet::new(pts, frame_width, frame_height)
        .with_color_space(SubtitleBitmapColorSpace::Video)
        .with_changed(changed);
    let mut current = first;
    let mut count = 0usize;

    while !current.is_null() {
        if count >= RAW_ASS_IMAGE_LIST_LIMIT {
            return Err(SubtitleError::BitmapListTooLong);
        }
        let image = unsafe { &*current };
        if image.w > 0 && image.h > 0 {
            let raw = RawAssImage {
                w: image.w,
                h: image.h,
                stride: image.stride,
                bitmap: image.bitmap.cast_const(),
                color: image.color,
                dst_x: image.dst_x,
                dst_y: image.dst_y,
                next: std::ptr::null(),
                image_type: image.image_type,
            };
            set.push(unsafe { raw_ass_image_to_alpha_bitmap(&raw)? });
        }
        current = image.next;
        count += 1;
    }

    Ok(set)
}

pub type SubtitleViewport = SubtitleRenderViewport;
pub type SubtitleRendererBackend = SubtitleRenderBackend;

impl SubtitleFrame {
    pub fn from_ass_bitmaps<'a>(
        pts: Duration,
        bitmaps: impl IntoIterator<Item = &'a AssBitmapPlane>,
    ) -> Result<Self> {
        let mut set = SubtitleBitmapSet::new(pts, 1, 1);
        for bitmap in bitmaps {
            if let Some(part) = bitmap.as_alpha_bitmap()? {
                set.push(part);
            }
        }
        Ok(set.to_frame())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleFrameChange {
    Changed,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleRenderResult {
    pub backend: SubtitleRendererBackend,
    pub change: SubtitleFrameChange,
    pub frame: SubtitleFrame,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SubtitleFrameSignature {
    viewport: SubtitleViewport,
    active_cues: Vec<SubtitleCue>,
}

#[derive(Debug, Clone)]
pub struct SubtitleRendererCore {
    timeline: SubtitleTimeline,
    last_signature: Option<SubtitleFrameSignature>,
}

impl SubtitleRendererCore {
    pub fn new_debug(timeline: SubtitleTimeline) -> Self {
        Self {
            timeline,
            last_signature: None,
        }
    }

    pub fn timeline(&self) -> &SubtitleTimeline {
        &self.timeline
    }

    pub fn render(&mut self, pts: Duration, viewport: SubtitleViewport) -> SubtitleRenderResult {
        let active_cues = self
            .timeline
            .active_cues(pts)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let signature = SubtitleFrameSignature {
            viewport,
            active_cues,
        };
        let change = if self.last_signature.as_ref() == Some(&signature) {
            SubtitleFrameChange::Unchanged
        } else {
            SubtitleFrameChange::Changed
        };
        self.last_signature = Some(signature);
        SubtitleRenderResult {
            backend: SubtitleRenderBackend::DebugTimeline,
            change,
            frame: self
                .timeline
                .render_debug_frame(pts, viewport.width, viewport.height),
        }
    }

    pub fn render_ass_bitmaps<'a>(
        pts: Duration,
        bitmaps: impl IntoIterator<Item = &'a AssBitmapPlane>,
    ) -> Result<SubtitleRenderResult> {
        Ok(SubtitleRenderResult {
            backend: SubtitleRenderBackend::Libass,
            change: SubtitleFrameChange::Changed,
            frame: SubtitleFrame::from_ass_bitmaps(pts, bitmaps)?,
        })
    }
}

impl SubtitleRenderer for SubtitleRendererCore {
    fn backend(&self) -> SubtitleRenderBackend {
        SubtitleRenderBackend::DebugTimeline
    }

    fn render(&mut self, request: SubtitleRenderRequest) -> Result<SubtitleRenderOutput> {
        Ok(SubtitleRenderOutput::Rgba(
            self.render(request.pts, request.viewport).frame,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl AssColor {
    pub fn from_libass_rgba(color: u32) -> Self {
        Self {
            red: ((color >> 24) & 0xff) as u8,
            green: ((color >> 16) & 0xff) as u8,
            blue: ((color >> 8) & 0xff) as u8,
            alpha: (0xff - (color & 0xff)) as u8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssBitmapPlane {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub color: u32,
    pub alpha: Vec<u8>,
}

impl AssBitmapPlane {
    pub fn new(
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        stride: usize,
        color: u32,
        alpha: Vec<u8>,
    ) -> Result<Self> {
        validate_ass_bitmap(width, height, stride, alpha.len())?;
        Ok(Self {
            x,
            y,
            width,
            height,
            stride,
            color,
            alpha,
        })
    }

    pub fn to_rgba_plane(&self) -> Result<SubtitleBitmapPlane> {
        self.as_alpha_bitmap()?
            .and_then(|bitmap| bitmap.to_rgba_plane())
            .ok_or_else(|| SubtitleError::InvalidBitmap {
                width: self.width,
                height: self.height,
                stride: self.stride,
                bytes: self.alpha.len(),
            })
    }

    pub fn as_alpha_bitmap(&self) -> Result<Option<SubtitleAlphaBitmap>> {
        validate_ass_bitmap(self.width, self.height, self.stride, self.alpha.len())?;
        if self.width == 0 || self.height == 0 {
            return Ok(None);
        }
        Ok(Some(SubtitleAlphaBitmap::new(
            SubtitleBitmapPlacement::new(self.x, self.y, self.width, self.height),
            self.stride,
            self.color,
            self.alpha.clone(),
        )))
    }
}

#[derive(Debug, Clone, Default)]
pub struct SubtitleTimeline {
    cues: Vec<SubtitleCue>,
}

impl SubtitleTimeline {
    pub fn new(cues: Vec<SubtitleCue>) -> Self {
        let mut timeline = Self { cues };
        timeline.cues.sort_by_key(|cue| cue.start);
        timeline
    }

    pub fn cues(&self) -> &[SubtitleCue] {
        &self.cues
    }

    pub fn active_cues(&self, pts: Duration) -> Vec<&SubtitleCue> {
        self.cues
            .iter()
            .filter(|cue| cue.start <= pts && pts < cue.end)
            .collect()
    }

    pub fn render_debug_frame(&self, pts: Duration, width: u32, height: u32) -> SubtitleFrame {
        let active = self.active_cues(pts);
        let mut planes = Vec::new();
        for (index, cue) in active.iter().enumerate() {
            let text_width = (cue.text.chars().count() as u32).saturating_mul(10).max(16);
            let plane_width = text_width.min(width.max(1));
            let plane_height = 28u32.min(height.max(1));
            let x = ((width.saturating_sub(plane_width)) / 2) as i32;
            let y = height
                .saturating_sub(plane_height.saturating_mul(index as u32 + 1))
                .saturating_sub(24) as i32;
            planes.push(SubtitleBitmapPlane::new(
                x,
                y,
                plane_width,
                plane_height,
                debug_rgba_plane(plane_width, plane_height),
            ));
        }
        SubtitleFrame { pts, planes }
    }
}

pub fn parse_srt(input: &str) -> Result<SubtitleTimeline> {
    let mut cues = Vec::new();
    // SRT and WebVTT separators may contain spaces or tabs. Treat any
    // whitespace-only line as a boundary, preserving multiline cue text.
    let normalized = input.replace("\r\n", "\n");
    let lines = normalized.lines().collect::<Vec<_>>();
    for lines in lines.split(|line| line.trim().is_empty()) {
        if lines.is_empty() {
            continue;
        }
        let time_line_index = usize::from(!lines[0].contains("-->"));
        let Some(time_line) = lines.get(time_line_index) else {
            continue;
        };
        if !time_line.contains("-->") {
            continue;
        }
        let text = lines[time_line_index + 1..].join("\n");
        cues.push(parse_timed_text_cue(time_line, &text)?);
    }
    Ok(SubtitleTimeline::new(cues))
}

pub fn parse_webvtt(input: &str) -> Result<SubtitleTimeline> {
    let normalized = input.replace("\r\n", "\n");
    let without_header = normalized.strip_prefix("WEBVTT").unwrap_or(&normalized);
    parse_srt(without_header)
}

pub fn parse_ass_events(input: &str) -> Result<SubtitleTimeline> {
    let mut in_events = false;
    let mut format_fields = Vec::new();
    let mut cues = Vec::new();

    for line in input.lines() {
        let line = line.trim();
        if line.eq_ignore_ascii_case("[events]") {
            in_events = true;
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(format) = line.strip_prefix("Format:") {
            format_fields = format
                .split(',')
                .map(|field| field.trim().to_ascii_lowercase())
                .collect();
            continue;
        }
        if let Some(dialogue) = line.strip_prefix("Dialogue:") {
            let field_count = format_fields.len().max(10);
            let values = dialogue
                .splitn(field_count, ',')
                .map(str::trim)
                .collect::<Vec<_>>();
            let index = |name: &str, default: usize| {
                format_fields
                    .iter()
                    .position(|field| field == name)
                    .unwrap_or(default)
            };
            let start = values
                .get(index("start", 1))
                .ok_or(SubtitleError::InvalidCue)
                .and_then(|value| parse_timestamp(value))?;
            let end = values
                .get(index("end", 2))
                .ok_or(SubtitleError::InvalidCue)
                .and_then(|value| parse_timestamp(value))?;
            let text = values
                .get(index("text", 9))
                .map(|value| clean_ass_text(value))
                .unwrap_or_default();
            cues.push(SubtitleCue { start, end, text });
        }
    }

    Ok(SubtitleTimeline::new(cues))
}

pub fn parse_subtitle_text_file(
    format: SubtitleFileFormat,
    input: &str,
) -> Result<SubtitleTimeline> {
    match format {
        SubtitleFileFormat::Srt => parse_srt(input),
        SubtitleFileFormat::WebVtt => parse_webvtt(input),
        SubtitleFileFormat::Ass => parse_ass_events(input),
    }
}

pub fn decoded_subtitle_frames_to_timeline<'a>(
    frames: impl IntoIterator<Item = &'a DecodedSubtitleFrame>,
    fallback_end: Duration,
) -> SubtitleTimeline {
    let cues = frames
        .into_iter()
        .flat_map(|frame| frame.text_cues(fallback_end))
        .collect();
    SubtitleTimeline::new(cues)
}

pub fn decoded_subtitle_frames_to_ass_script<'a>(
    frames: impl IntoIterator<Item = &'a DecodedSubtitleFrame>,
    fallback_end: Duration,
) -> Option<String> {
    decoded_subtitle_frames_to_ass_script_with_style(
        frames,
        fallback_end,
        &SubtitleAssStyle::default(),
    )
}

pub fn decoded_subtitle_frames_to_ass_script_with_style<'a>(
    frames: impl IntoIterator<Item = &'a DecodedSubtitleFrame>,
    fallback_end: Duration,
    style: &SubtitleAssStyle,
) -> Option<String> {
    let mut events = String::new();
    for frame in frames {
        if !frame.has_text() {
            continue;
        }
        let start = frame.start.unwrap_or(Duration::ZERO);
        let end = frame.end.unwrap_or(fallback_end).max(start);
        for segment in &frame.text {
            if segment.text.trim().is_empty() {
                continue;
            }
            match segment.format {
                SubtitleTextFormat::Ass if is_ass_dialogue_line(&segment.text) => {
                    events.push_str(segment.text.trim());
                    events.push('\n');
                }
                SubtitleTextFormat::Ass => {
                    let text = ass_segment_display_text(&segment.text);
                    if !text.is_empty() {
                        push_ass_dialogue(&mut events, start, end, &text);
                    }
                }
                SubtitleTextFormat::PlainText => {
                    let text = segment.text.trim();
                    if !text.is_empty() {
                        push_ass_dialogue(&mut events, start, end, text);
                    }
                }
            }
        }
    }

    if events.is_empty() {
        return None;
    }

    let mut script = default_ass_script_header(style);
    script.push_str(&events);
    Some(script)
}

fn parse_timed_text_cue(time_line: &str, text: &str) -> Result<SubtitleCue> {
    let (start, end) = time_line
        .split_once("-->")
        .ok_or(SubtitleError::InvalidCue)?;
    let start = parse_timestamp(start.trim())?;
    let end_part = end.split_whitespace().next().unwrap_or(end).trim();
    let end = parse_timestamp(end_part)?;
    Ok(SubtitleCue {
        start,
        end,
        text: text.trim().to_string(),
    })
}

fn parse_timestamp(value: &str) -> Result<Duration> {
    let value = value.trim().replace(',', ".");
    let parts = value.split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (0u64, parse_int(minutes)?, parse_seconds(seconds)?),
        [hours, minutes, seconds] => (
            parse_int(hours)?,
            parse_int(minutes)?,
            parse_seconds(seconds)?,
        ),
        _ => return Err(SubtitleError::InvalidTimestamp(value)),
    };
    // Checked so an absurd-but-parseable timestamp ("99999999999:00:00.000")
    // reports an error instead of overflowing or panicking.
    let base = hours
        .checked_mul(3600)
        .and_then(|product| product.checked_add(minutes.checked_mul(60)?))
        .ok_or_else(|| SubtitleError::InvalidTimestamp(value.clone()))?;
    Duration::from_secs(base)
        .checked_add(seconds)
        .ok_or_else(|| SubtitleError::InvalidTimestamp(value))
}

fn parse_int(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|_| SubtitleError::InvalidTimestamp(value.to_string()))
}

fn parse_seconds(value: &str) -> Result<Duration> {
    let seconds = value
        .parse::<f64>()
        .map_err(|_| SubtitleError::InvalidTimestamp(value.to_string()))?;
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| SubtitleError::InvalidTimestamp(value.to_string()))
}

fn clean_ass_text(value: &str) -> String {
    let mut output = String::new();
    let mut in_override = false;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => in_override = true,
            '}' => in_override = false,
            _ if in_override => {}
            '\\' => match chars.peek().copied() {
                Some('N') | Some('n') => {
                    let _ = chars.next();
                    output.push('\n');
                }
                _ => output.push(ch),
            },
            _ => output.push(ch),
        }
    }
    output.trim().to_string()
}

fn ass_segment_display_text(value: &str) -> String {
    let value = value.trim();
    if let Some(dialogue) = strip_ass_dialogue_prefix(value) {
        let fields = dialogue.splitn(10, ',').collect::<Vec<_>>();
        return fields
            .get(9)
            .map(|text| clean_ass_text(text))
            .unwrap_or_default();
    }
    let fields = value.splitn(9, ',').collect::<Vec<_>>();
    if fields.len() == 9 && fields[0].trim().parse::<i64>().is_ok() {
        return clean_ass_text(fields[8]);
    }
    clean_ass_text(value)
}

fn is_ass_dialogue_line(value: &str) -> bool {
    strip_ass_dialogue_prefix(value).is_some()
}

fn strip_ass_dialogue_prefix(value: &str) -> Option<&str> {
    let value = value.trim_start();
    let (prefix, rest) = value.split_once(':')?;
    prefix
        .eq_ignore_ascii_case("Dialogue")
        .then_some(rest.trim_start())
}

fn push_ass_dialogue(output: &mut String, start: Duration, end: Duration, text: &str) {
    output.push_str("Dialogue: 0,");
    output.push_str(&format_ass_timestamp(start));
    output.push(',');
    output.push_str(&format_ass_timestamp(end));
    output.push_str(",Default,,0,0,0,,");
    output.push_str(&escape_ass_text(text));
    output.push('\n');
}

fn format_ass_timestamp(value: Duration) -> String {
    let centiseconds = value.as_millis().saturating_add(5) / 10;
    let seconds_total = centiseconds / 100;
    let hours = seconds_total / 3600;
    let minutes = (seconds_total / 60) % 60;
    let seconds = seconds_total % 60;
    let centiseconds = centiseconds % 100;
    format!("{hours}:{minutes:02}:{seconds:02}.{centiseconds:02}")
}

fn escape_ass_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\n' => output.push_str("\\N"),
            '\\' => output.push_str("\\\\"),
            '{' => output.push_str("\\{"),
            '}' => output.push_str("\\}"),
            _ => output.push(ch),
        }
    }
    output
}

const DEFAULT_ASS_FONT_SIZE: f64 = 48.0;
const DEFAULT_ASS_OUTLINE: f64 = 2.0;
const MIN_ASS_FONT_SIZE: f64 = 8.0;
const MAX_ASS_FONT_SIZE: f64 = 400.0;
const MAX_ASS_OUTLINE: f64 = 32.0;

/// Attachment name liberika registers the bundled `assets/subfont.ttf` under.
const BUNDLED_ASS_FALLBACK_FONT_NAME: &str = "subfont.ttf";
/// Family name carried by the bundled fallback face. Keep in sync with the
/// asset itself; `bundled_ass_fallback_family_matches_asset` locks the two.
const BUNDLED_ASS_FALLBACK_FONT_FAMILY: &str = "Droid Sans Fallback";

#[cfg(target_os = "macos")]
const DEFAULT_ASS_FONT_FAMILY: &str = "PingFang SC";
#[cfg(target_os = "windows")]
const DEFAULT_ASS_FONT_FAMILY: &str = "Arial";
// Every other target renders without a system font provider (mobile selects
// `ASS_FONTPROVIDER_NONE`, and the vendored libass build has fontconfig
// disabled), so the bundled fallback family is the only one libass resolves.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const DEFAULT_ASS_FONT_FAMILY: &str = BUNDLED_ASS_FALLBACK_FONT_FAMILY;

#[cfg(feature = "libass")]
fn default_ass_font_provider() -> libc::c_int {
    if cfg!(any(
        target_os = "ios",
        target_os = "tvos",
        target_os = "android"
    )) {
        ASS_FONTPROVIDER_NONE
    } else if cfg!(target_os = "macos") {
        ASS_FONTPROVIDER_CORETEXT
    } else {
        ASS_FONTPROVIDER_AUTODETECT
    }
}

#[cfg(feature = "libass")]
fn default_ass_font_family_cstr() -> &'static CStr {
    if cfg!(target_os = "macos") {
        c"Droid Sans Fallback"
    } else if cfg!(target_os = "windows") {
        c"Arial"
    } else {
        c"Droid Sans Fallback"
    }
}

#[cfg(feature = "libass")]
unsafe extern "C" fn libass_log_sink(
    opaque: *mut libc::c_void,
    level: libc::c_int,
    message: *const libc::c_char,
) {
    if opaque.is_null() || message.is_null() || level > 5 {
        return;
    }
    let context = unsafe { &*(opaque.cast::<LibassLogContext>()) };
    let message = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .trim()
        .to_string();
    if message.is_empty() {
        return;
    }
    let lowercase = message.to_ascii_lowercase();
    let stage = if lowercase.contains("no style named") {
        "missing_style"
    } else if lowercase.contains("failed to find any fallback")
        || lowercase.contains("glyph") && lowercase.contains("not found")
    {
        "missing_font"
    } else if lowercase.contains("using default font") || lowercase.contains("using default family")
    {
        "font_fallback"
    } else if level <= 2 {
        "error"
    } else {
        "message"
    };
    let key = format!("{level}:{message}");
    let Ok(mut seen) = context.seen.lock() else {
        return;
    };
    if !seen.insert(key) {
        return;
    }
    drop(seen);
    let line = serde_json::json!({
        "event": "subtitle_libass",
        "stage": stage,
        "trackId": context.track_id,
        "level": level,
        "message": message,
    })
    .to_string();
    // libass: 0 fatal, 1 error, 2 warning, 3..5 informational. Missing styles
    // and fonts stay at least a warning whatever level libass used.
    let libass_level = match level {
        0 | 1 => crate::trace::LogLevel::Error,
        2 => crate::trace::LogLevel::Warn,
        _ => crate::trace::LogLevel::Info,
    };
    crate::trace::diagnostic_at(
        libass_level.max(crate::trace::classify_diagnostic(&line)),
        line,
    );
}

#[cfg(feature = "libass")]
unsafe fn add_attached_ass_fonts(
    library: *mut libass_ffi::AssLibrary,
    track_id: i64,
    fonts: &[SubtitleFontAttachment],
) {
    for font in fonts {
        let Ok(data_size) = i32::try_from(font.data.len()) else {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_font_attachment",
                    "stage": "rejected",
                    "trackId": track_id,
                    "name": font.name,
                    "bytes": font.data.len(),
                    "reason": "font exceeds libass integer range",
                })
                .to_string(),
            );
            continue;
        };
        let Ok(name) = CString::new(font.name.as_bytes()) else {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_font_attachment",
                    "stage": "rejected",
                    "trackId": track_id,
                    "name": font.name,
                    "bytes": font.data.len(),
                    "reason": "attachment name contains an interior NUL",
                })
                .to_string(),
            );
            continue;
        };
        unsafe {
            libass_ffi::ass_add_font(library, name.as_ptr(), font.data.as_ptr().cast(), data_size);
        }
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "subtitle_font_attachment",
                "stage": "loaded",
                "trackId": track_id,
                "name": font.name,
                "mimeType": font.mime_type,
                "families": font.families,
                "bytes": font.data.len(),
            })
            .to_string(),
        );
    }
}

/// Registers the bundled CJK-capable face so libass always has a last-resort
/// family. Container attachments and system faces still win when they cover the
/// requested family, but a script that names a font nobody has - or one that
/// covers none of the codepoints in the line - falls back here instead of
/// rendering nothing.
#[cfg(feature = "libass")]
unsafe fn add_bundled_ass_fallback_font(library: *mut libass_ffi::AssLibrary, track_id: i64) {
    let Ok(data_size) = libc::c_int::try_from(NIPAPLAY_FALLBACK_FONT.len()) else {
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "subtitle_font_fallback",
                "stage": "rejected",
                "trackId": track_id,
                "name": BUNDLED_ASS_FALLBACK_FONT_NAME,
                "bytes": NIPAPLAY_FALLBACK_FONT.len(),
                "reason": "bundled fallback font exceeds libass integer range",
            })
            .to_string(),
        );
        return;
    };
    unsafe {
        libass_ffi::ass_add_font(
            library,
            c"subfont.ttf".as_ptr(),
            NIPAPLAY_FALLBACK_FONT.as_ptr().cast(),
            data_size,
        );
    }
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "subtitle_font_fallback",
            "stage": "loaded",
            "trackId": track_id,
            "name": BUNDLED_ASS_FALLBACK_FONT_NAME,
            "family": BUNDLED_ASS_FALLBACK_FONT_FAMILY,
            "bytes": NIPAPLAY_FALLBACK_FONT.len(),
        })
        .to_string(),
    );
}

fn normalize_ass_font_scale(scale: f64) -> f64 {
    if scale.is_finite() {
        scale.clamp(0.25, 4.0)
    } else {
        1.0
    }
}

fn normalize_ass_metric(value: f64, default: f64, min: f64, max: f64) -> f64 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

#[cfg(feature = "libass")]
fn selective_override_metric(value: f64, font_scale: f64, play_res_height: u32) -> f64 {
    value * normalize_ass_font_scale(font_scale) * 288.0 / f64::from(play_res_height.max(1))
}

/// Formats `0xRRGGBBAA` as an ASS `&HAABBGGRR` literal, where ASS' leading byte
/// is transparency rather than alpha.
fn ass_color_tag(rgba: u32) -> String {
    let [red, green, blue, alpha] = rgba.to_be_bytes();
    let transparency = 0xff - alpha;
    format!("&H{transparency:02X}{blue:02X}{green:02X}{red:02X}")
}

/// Converts `0xRRGGBBAA` to the colour representation `ASS_Style` uses, which
/// keeps RGB in the high bytes and stores transparency in the low byte.
#[cfg(feature = "libass")]
fn libass_style_color(rgba: u32) -> u32 {
    let [red, green, blue, alpha] = rgba.to_be_bytes();
    u32::from_be_bytes([red, green, blue, 0xff - alpha])
}

fn ass_number(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    if rounded.fract().abs() < 0.001 {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.2}")
    }
}

fn default_ass_script_header(style: &SubtitleAssStyle) -> String {
    let scale = normalize_ass_font_scale(style.font_scale);
    let metrics = style.style.clone().normalized();
    let font_size = ass_number(metrics.font_size * scale);
    let outline = ass_number(metrics.outline_width * scale);
    let shadow = ass_number(metrics.shadow_depth * scale);
    let spacing = ass_number(metrics.spacing * scale);
    let scale_x = ass_number(metrics.scale_x_percent);
    let scale_y = ass_number(metrics.scale_y_percent);
    let bold = i32::from(metrics.bold) * -1;
    let italic = i32::from(metrics.italic) * -1;
    let underline = i32::from(metrics.underline) * -1;
    let strike_out = i32::from(metrics.strike_out) * -1;
    let font_family = style
        .style
        .font_family()
        .unwrap_or(DEFAULT_ASS_FONT_FAMILY)
        .to_string();
    let primary_color = ass_color_tag(style.style.primary_color_rgba);
    let outline_color = ass_color_tag(style.style.outline_color_rgba);
    let play_res_width = style.play_res_width.max(1);
    let play_res_height = style.play_res_height.max(1);
    format!(
        r#"[Script Info]
ScriptType: v4.00+
PlayResX: {play_res_width}
PlayResY: {play_res_height}

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,{font_family},{font_size},{primary_color},&H000000FF,{outline_color},{outline_color},{bold},{italic},{underline},{strike_out},{scale_x},{scale_y},{spacing},0,{border_style},{outline},{shadow},{alignment},{margin_left},{margin_right},{margin_vertical},1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"#,
        border_style = metrics.border_style,
        alignment = metrics.alignment,
        margin_left = metrics.margin_left,
        margin_right = metrics.margin_right,
        margin_vertical = metrics.margin_vertical,
    )
}

fn debug_rgba_plane(width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[255, 255, 255, 220]);
    }
    rgba
}

fn validate_ass_bitmap(width: u32, height: u32, stride: usize, bytes: usize) -> Result<()> {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let required = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|prefix| prefix.checked_add(width))
        .unwrap_or(usize::MAX);
    if stride < width || bytes < required {
        return Err(SubtitleError::InvalidBitmap {
            width: width.min(u32::MAX as usize) as u32,
            height: height.min(u32::MAX as usize) as u32,
            stride,
            bytes,
        });
    }
    Ok(())
}

fn multiply_u8(a: u8, b: u8) -> u8 {
    ((a as u16 * b as u16 + 127) / 255) as u8
}

const RAW_ASS_IMAGE_LIST_LIMIT: usize = 16_384;

unsafe fn raw_ass_image_to_alpha_bitmap(image: &RawAssImage) -> Result<SubtitleAlphaBitmap> {
    if image.bitmap.is_null() {
        return Err(SubtitleError::NullBitmap);
    }
    let width = image.w as u32;
    let height = image.h as u32;
    let stride = image.stride.max(0) as usize;
    let required = required_bitmap_len(width, height, stride)?;
    let alpha = unsafe { std::slice::from_raw_parts(image.bitmap, required) }.to_vec();
    Ok(SubtitleAlphaBitmap::new(
        SubtitleBitmapPlacement::new(image.dst_x, image.dst_y, width, height),
        stride,
        image.color,
        alpha,
    ))
}

fn required_bitmap_len(width: u32, height: u32, stride: usize) -> Result<usize> {
    validate_ass_bitmap(width, height, stride, usize::MAX)?;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Ok(0);
    }
    Ok(stride * height.saturating_sub(1) + width as usize)
}

fn duration_to_millis_i64(value: Duration) -> i64 {
    value.as_millis().min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "libass")]
    const SIMPLE_ASS_SCRIPT: &str = r#"[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,32,&H00FFFFFF,&H000000FF,&H80000000,&H80000000,0,0,0,0,100,100,0,0,1,2,0,2,20,20,24,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,Hello libass
"#;

    #[cfg(feature = "libass")]
    const STREAM_ASS_HEADER: &str = r#"[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Sign,Droid Sans Fallback,40,&H00FFFFFF,&H000000FF,&H80000000,&H80000000,0,0,0,0,100,100,0,0,1,2,0,5,20,20,24,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"#;

    #[test]
    fn parses_srt_and_finds_active_cue() {
        let srt =
            "1\n00:00:01,000 --> 00:00:03,500\nHello\n\n2\n00:00:04,000 --> 00:00:05,000\nWorld\n";
        let timeline = parse_srt(srt).unwrap();

        assert_eq!(timeline.cues().len(), 2);
        assert_eq!(
            timeline.active_cues(Duration::from_millis(1500))[0].text,
            "Hello"
        );
        assert!(timeline.active_cues(Duration::from_millis(3500)).is_empty());
    }

    #[test]
    fn parse_srt_rejects_unrepresentable_timestamps_instead_of_panicking() {
        // "inf" parses as f64 infinity and "-1.5" as a negative value, both of
        // which Duration::from_secs_f64 panics on; absurd hour counts overflow
        // the seconds arithmetic. All of them are invalid timestamps.
        let finite_overflow = parse_srt("1\n00:00:01,000 --> 0:00:1e100\nboom\n");
        assert!(finite_overflow.is_err());

        let infinite = parse_srt("1\n00:00:01,000 --> 0:00:inf\nboom\n");
        assert!(infinite.is_err());

        let negative = parse_srt("1\n00:00:01,000 --> 0:00:-1.500\nboom\n");
        assert!(negative.is_err());

        let overflow =
            parse_srt("1\n99999999999999999:00:00,000 --> 99999999999999999:00:01,000\nboom\n");
        assert!(overflow.is_err());
    }

    #[test]
    fn subtitle_cues_split_on_whitespace_separators_with_lf_and_crlf() {
        for newline in ["\n", "\r\n"] {
            for separator in [" ", "\t", " \t "] {
                let srt = [
                    "1",
                    "00:00:01,000 --> 00:00:02,000",
                    "First",
                    "continued",
                    separator,
                    "2",
                    "00:00:03,000 --> 00:00:04,000",
                    "Second",
                ]
                .join(newline);
                let timeline = parse_srt(&srt).unwrap();
                assert_eq!(
                    timeline.cues().len(),
                    2,
                    "newline={newline:?}, separator={separator:?}"
                );
                assert_eq!(timeline.cues()[0].text, "First\ncontinued");
                assert_eq!(timeline.cues()[1].text, "Second");
                assert_eq!(timeline.active_cues(Duration::from_millis(3500)).len(), 1);
            }
        }
    }

    #[test]
    fn webvtt_cues_split_on_whitespace_separators_and_preserve_multiline_text() {
        let vtt = "WEBVTT\r\n\r\n00:00:01.000 --> 00:00:02.000\r\nFirst\r\ncontinued\r\n \t \r\n00:00:03.000 --> 00:00:04.000\r\nSecond\r\n";
        let timeline = parse_webvtt(vtt).unwrap();
        assert_eq!(timeline.cues().len(), 2);
        assert_eq!(timeline.cues()[0].text, "First\ncontinued");
        assert_eq!(timeline.cues()[1].text, "Second");
    }

    #[test]
    fn subtitle_file_format_detects_text_subtitle_extensions() {
        assert_eq!(
            SubtitleFileFormat::from_path("/tmp/movie.en.srt"),
            Some(SubtitleFileFormat::Srt)
        );
        assert_eq!(
            SubtitleFileFormat::from_path("/tmp/movie.VTT"),
            Some(SubtitleFileFormat::WebVtt)
        );
        assert_eq!(
            SubtitleFileFormat::from_path("/tmp/movie.ass"),
            Some(SubtitleFileFormat::Ass)
        );
        assert_eq!(SubtitleFileFormat::from_path("/tmp/movie.sup"), None);
    }

    #[test]
    fn subtitle_file_format_ignores_query_strings_and_fragments() {
        assert_eq!(
            SubtitleFileFormat::from_uri("https://host/sub.srt?token=abc&x=1"),
            Some(SubtitleFileFormat::Srt)
        );
        assert_eq!(
            SubtitleFileFormat::from_uri("https://host/sub.ass#cue3"),
            Some(SubtitleFileFormat::Ass)
        );
        // A dot inside the query must not be mistaken for the extension.
        assert_eq!(
            SubtitleFileFormat::from_uri("https://host/sub.vtt?sig=a.b.c"),
            Some(SubtitleFileFormat::WebVtt)
        );
        assert_eq!(
            SubtitleFileFormat::from_uri("https://host/sub.sup?t=1"),
            None
        );
    }

    #[test]
    fn subtitle_path_extension_reads_the_last_segment_only() {
        assert_eq!(subtitle_path_extension("/tmp/movie.en.srt"), Some("srt"));
        assert_eq!(subtitle_path_extension("C:\\media\\movie.ass"), Some("ass"));
        // A dot in a parent directory is not an extension.
        assert_eq!(subtitle_path_extension("/media/season.1/subs"), None);
        // Android hands us an fd URI that names no file at all.
        assert_eq!(
            subtitle_path_extension(uri_path_component("fd://42?offset=0&length=99")),
            None
        );
    }

    #[test]
    fn bitmap_plane_scales_from_subtitle_canvas_to_video_viewport() {
        let plane = SubtitleBitmapPlane::new(800, 905, 322, 60, Vec::new()).with_canvas(1920, 1080);

        assert_eq!(plane.scaled_rect(3840, 1816), (1600, 1638, 644, 120));
    }

    #[test]
    fn parse_subtitle_text_file_dispatches_by_format() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nHello\n";
        let timeline = parse_subtitle_text_file(SubtitleFileFormat::Srt, srt).unwrap();

        assert_eq!(timeline.cues().len(), 1);
        assert_eq!(timeline.cues()[0].text, "Hello");
    }

    #[test]
    fn parses_ass_dialogue() {
        let ass = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\i1}Hi\\Nthere";
        let timeline = parse_ass_events(ass).unwrap();

        assert_eq!(timeline.cues().len(), 1);
        assert_eq!(timeline.cues()[0].text, "Hi\nthere");
    }

    #[test]
    fn decoded_plain_text_subtitle_can_become_ass_script() {
        let mut frame = DecodedSubtitleFrame::new(
            2,
            Some(Duration::from_millis(1250)),
            Some(Duration::from_millis(2500)),
        );
        frame.push_text(SubtitleTextSegment::new(
            SubtitleTextFormat::PlainText,
            "hello\nworld",
        ));

        let script = frame.to_ass_script(Duration::from_secs(5)).unwrap();

        assert!(script.contains("[Script Info]"));
        assert!(script.contains("Dialogue: 0,0:00:01.25,0:00:02.50"));
        assert!(script.contains("hello\\Nworld"));
    }

    #[test]
    fn decoded_plain_text_subtitle_ass_script_honors_font_scale() {
        let mut frame = DecodedSubtitleFrame::new(
            2,
            Some(Duration::from_millis(1250)),
            Some(Duration::from_millis(2500)),
        );
        frame.push_text(SubtitleTextSegment::new(
            SubtitleTextFormat::PlainText,
            "scaled",
        ));

        let script = decoded_subtitle_frames_to_ass_script_with_style(
            [&frame],
            Duration::from_secs(5),
            &SubtitleAssStyle {
                font_scale: 1.5,
                ..SubtitleAssStyle::default()
            },
        )
        .unwrap();

        assert!(script.contains(&format!("Style: Default,{DEFAULT_ASS_FONT_FAMILY},72,")));
    }

    #[test]
    fn decoded_plain_text_subtitle_ass_script_uses_render_viewport_as_play_res() {
        let mut frame = DecodedSubtitleFrame::new(
            2,
            Some(Duration::from_millis(1250)),
            Some(Duration::from_millis(2500)),
        );
        frame.push_text(SubtitleTextSegment::new(
            SubtitleTextFormat::PlainText,
            "wide",
        ));

        let script = decoded_subtitle_frames_to_ass_script_with_style(
            [&frame],
            Duration::from_secs(5),
            &SubtitleAssStyle {
                play_res_width: 1920,
                play_res_height: 816,
                ..SubtitleAssStyle::default()
            },
        )
        .unwrap();

        assert!(script.contains("PlayResX: 1920"));
        assert!(script.contains("PlayResY: 816"));
    }

    #[test]
    fn decoded_ass_dialogue_preserves_event_line_for_libass() {
        let mut frame = DecodedSubtitleFrame::new(
            2,
            Some(Duration::from_secs(1)),
            Some(Duration::from_secs(2)),
        );
        frame.push_text(SubtitleTextSegment::new(
            SubtitleTextFormat::Ass,
            "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\i1}Hi",
        ));

        let script = frame.to_ass_script(Duration::from_secs(5)).unwrap();

        assert!(script.contains("Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\i1}Hi"));
        assert_eq!(frame.text[0].display_text(), "Hi");
    }

    #[test]
    fn decoded_lavc_ass_payload_uses_payload_text_field() {
        let segment = SubtitleTextSegment::new(
            SubtitleTextFormat::Ass,
            "0,0,Default,,0,0,0,,External subtitle",
        );

        assert_eq!(segment.display_text(), "External subtitle");
    }

    #[test]
    fn bitmap_plane_scales_to_matching_aspect_video_viewport() {
        let plane = SubtitleBitmapPlane::new(800, 905, 322, 60, Vec::new()).with_canvas(1920, 1080);

        assert_eq!(plane.scaled_rect(3840, 2160), (1600, 1810, 644, 120));
    }

    #[test]
    fn decoded_text_frames_can_become_debug_timeline() {
        let mut frame = DecodedSubtitleFrame::new(
            2,
            Some(Duration::from_secs(1)),
            Some(Duration::from_secs(3)),
        );
        frame.push_text(SubtitleTextSegment::new(
            SubtitleTextFormat::Ass,
            "{\\b1}Hello",
        ));

        let timeline = decoded_subtitle_frames_to_timeline([&frame], Duration::from_secs(5));

        assert_eq!(timeline.cues().len(), 1);
        assert_eq!(timeline.cues()[0].text, "Hello");
        assert_eq!(timeline.cues()[0].start, Duration::from_secs(1));
        assert_eq!(timeline.cues()[0].end, Duration::from_secs(3));
    }

    #[test]
    fn debug_frame_produces_rgba_planes() {
        let timeline = SubtitleTimeline::new(vec![SubtitleCue {
            start: Duration::from_secs(1),
            end: Duration::from_secs(3),
            text: "Hello".to_string(),
        }]);

        let frame = timeline.render_debug_frame(Duration::from_secs(2), 640, 360);

        assert_eq!(frame.planes.len(), 1);
        assert_eq!(
            frame.planes[0].rgba.len(),
            frame.planes[0].width as usize * frame.planes[0].height as usize * 4
        );
    }

    #[test]
    fn ass_color_decodes_libass_inverse_alpha() {
        assert_eq!(
            AssColor::from_libass_rgba(0x11223300),
            AssColor {
                red: 0x11,
                green: 0x22,
                blue: 0x33,
                alpha: 0xff,
            }
        );
        assert_eq!(AssColor::from_libass_rgba(0x112233ff).alpha, 0);
    }

    #[test]
    fn ass_bitmap_plane_expands_alpha_mask_to_straight_rgba() {
        let bitmap =
            AssBitmapPlane::new(3, 4, 2, 2, 4, 0x20406080, vec![0, 255, 9, 9, 128, 64]).unwrap();

        let plane = bitmap.to_rgba_plane().unwrap();

        assert_eq!(plane.x, 3);
        assert_eq!(plane.y, 4);
        assert_eq!(plane.width, 2);
        assert_eq!(plane.height, 2);
        assert_eq!(
            plane.rgba,
            vec![
                0x20, 0x40, 0x60, 0, 0x20, 0x40, 0x60, 127, 0x20, 0x40, 0x60, 64, 0x20, 0x40, 0x60,
                32,
            ]
        );
    }

    #[test]
    fn subtitle_alpha_bitmap_expands_libass_color_and_coverage() {
        let bitmap = SubtitleAlphaBitmap::new(
            SubtitleBitmapPlacement::new(2, 3, 2, 1),
            2,
            0x12345680,
            vec![255, 128],
        );

        let plane = bitmap.to_rgba_plane().unwrap();

        assert_eq!(plane.x, 2);
        assert_eq!(plane.y, 3);
        assert_eq!(
            plane.rgba,
            vec![0x12, 0x34, 0x56, 127, 0x12, 0x34, 0x56, 64]
        );
    }

    #[test]
    fn subtitle_bitmap_set_converts_alpha_parts_to_rgba_frame() {
        let mut set = SubtitleBitmapSet::new(Duration::from_secs(7), 640, 360)
            .with_color_space(SubtitleBitmapColorSpace::Video)
            .with_changed(false);
        set.push(SubtitleAlphaBitmap::new(
            SubtitleBitmapPlacement::new(0, 0, 1, 1),
            1,
            0x00ff0000,
            vec![255],
        ));

        let frame = SubtitleRenderOutput::Alpha(set).into_rgba_frame();

        assert_eq!(frame.pts, Duration::from_secs(7));
        assert_eq!(frame.planes.len(), 1);
        assert_eq!(frame.planes[0].rgba, vec![0, 255, 0, 255]);
    }

    #[test]
    fn subtitle_track_source_distinguishes_embedded_and_external_removal() {
        let embedded = SubtitleTrackConfig::embedded(2, 2);
        let external = SubtitleTrackConfig::external(100, "/tmp/subs.ass");

        assert!(embedded.source.is_embedded());
        assert!(!embedded.can_remove());
        assert!(external.source.is_external());
        assert!(external.can_remove());
    }

    #[test]
    fn ass_bitmap_validation_accepts_unpadded_last_row() {
        let bitmap = AssBitmapPlane::new(0, 0, 3, 2, 8, 0xffffff00, vec![0; 11]).unwrap();

        assert_eq!(bitmap.to_rgba_plane().unwrap().rgba.len(), 24);
    }

    #[test]
    fn ass_bitmap_validation_rejects_short_alpha_buffer() {
        let error = AssBitmapPlane::new(0, 0, 3, 2, 8, 0xffffff00, vec![0; 10]).unwrap_err();

        assert!(matches!(error, SubtitleError::InvalidBitmap { .. }));
    }

    #[test]
    fn subtitle_renderer_core_reports_unchanged_timeline_frame() {
        let timeline = SubtitleTimeline::new(vec![SubtitleCue {
            start: Duration::from_secs(1),
            end: Duration::from_secs(3),
            text: "Hello".to_string(),
        }]);
        let mut renderer = SubtitleRendererCore::new_debug(timeline);
        let viewport = SubtitleViewport::new(640, 360);

        let first = renderer.render(Duration::from_secs(2), viewport);
        let second = renderer.render(Duration::from_millis(2500), viewport);

        assert_eq!(first.change, SubtitleFrameChange::Changed);
        assert_eq!(second.change, SubtitleFrameChange::Unchanged);
        assert_eq!(second.frame.planes.len(), 1);
    }

    #[test]
    fn subtitle_renderer_core_converts_ass_bitmaps() {
        let bitmap = AssBitmapPlane::new(0, 0, 1, 1, 1, 0xff000000, vec![255]).unwrap();
        let result =
            SubtitleRendererCore::render_ass_bitmaps(Duration::from_secs(1), [&bitmap]).unwrap();

        assert_eq!(result.backend, SubtitleRendererBackend::Libass);
        assert_eq!(result.frame.planes.len(), 1);
        assert_eq!(result.frame.planes[0].rgba, vec![255, 0, 0, 255]);
    }

    #[test]
    fn libass_render_plan_keeps_renderer_operation_order() {
        let request = SubtitleRenderRequest {
            pts: Duration::from_millis(1234),
            viewport: SubtitleRenderViewport {
                width: 1920,
                height: 1080,
                storage_width: 3840,
                storage_height: 2160,
            },
        };
        let plan = LibassRenderPlan::new(
            request,
            LibassRenderConfig {
                glyph_cache_limit: 128,
                bitmap_cache_limit_mb: 32,
            },
        );

        assert_eq!(
            plan.operations,
            vec![
                LibassRenderOperation::SetFrameSize {
                    width: 1920,
                    height: 1080,
                },
                LibassRenderOperation::SetStorageSize {
                    width: 3840,
                    height: 2160,
                },
                LibassRenderOperation::SetCacheLimits {
                    glyphs: 128,
                    bitmap_mb: 32,
                },
                LibassRenderOperation::RenderFrame { timestamp_ms: 1234 },
            ]
        );
    }

    #[test]
    fn raw_ass_image_list_imports_alpha_bitmaps() {
        let alpha = [255u8, 128, 64, 0];
        let image = RawAssImage {
            w: 2,
            h: 2,
            stride: 2,
            bitmap: alpha.as_ptr(),
            color: 0x80402000,
            dst_x: 10,
            dst_y: 20,
            next: std::ptr::null(),
            image_type: 0,
        };

        let set = unsafe {
            LibassImageImporter::import_raw_list(Duration::from_secs(1), 1920, 1080, &image, true)
        }
        .unwrap();

        assert_eq!(set.parts.len(), 1);
        let plane = set.to_frame().planes.remove(0);
        assert_eq!(plane.x, 10);
        assert_eq!(plane.y, 20);
        assert_eq!(plane.rgba[0..4], [0x80, 0x40, 0x20, 255]);
        assert_eq!(plane.rgba[4..8], [0x80, 0x40, 0x20, 128]);
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_renderer_rejects_empty_script() {
        let error =
            LibassSubtitleRenderer::from_ass_script("", LibassRenderConfig::default()).unwrap_err();

        assert!(matches!(error, SubtitleError::Libass(message) if message.contains("empty")));
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_renderer_renders_ass_script_to_alpha_bitmaps() {
        let config = LibassRenderConfig {
            glyph_cache_limit: 64,
            bitmap_cache_limit_mb: 16,
        };
        let mut renderer =
            LibassSubtitleRenderer::from_ass_script(SIMPLE_ASS_SCRIPT, config).unwrap();
        let request = SubtitleRenderRequest::new(Duration::from_millis(500), 640, 360);

        assert_eq!(renderer.backend(), SubtitleRenderBackend::Libass);
        assert_eq!(renderer.config(), config);
        assert_eq!(
            renderer.render_plan(request).operations,
            LibassRenderPlan::new(request, config).operations
        );

        let output = renderer.render(request).unwrap();
        let SubtitleRenderOutput::Alpha(bitmaps) = output else {
            panic!("libass renderer should produce alpha bitmap output");
        };

        assert_eq!(bitmaps.pts, request.pts);
        assert_eq!(bitmaps.frame_width, 640);
        assert_eq!(bitmaps.frame_height, 360);
        assert_eq!(bitmaps.color_space, SubtitleBitmapColorSpace::Video);
        assert!(!bitmaps.parts.is_empty());
        assert!(bitmaps.parts.iter().all(SubtitleAlphaBitmap::is_valid));
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_stream_track_preserves_matroska_chunk_and_flushes_on_seek() {
        let font = SubtitleFontAttachment::new(
            "subfont.ttf",
            Some("font/ttf".to_string()),
            vec!["Droid Sans Fallback".to_string()],
            Arc::<[u8]>::from(crate::NIPAPLAY_FALLBACK_FONT),
        );
        let resources = AssTrackResources::new(
            2,
            Arc::<[u8]>::from(STREAM_ASS_HEADER.as_bytes()),
            Arc::<[SubtitleFontAttachment]>::from(vec![font]),
        );
        let raw_chunk = "7,0,Sign,sign,0,0,0,,{\\pos(100,80)\\clip(0,0,300,200)\\t(0,500,\\blur3)\\fad(100,100)}Streamed";
        let mut renderer =
            LibassSubtitleRenderer::from_ass_track(2, &resources, LibassRenderConfig::default())
                .unwrap();
        renderer
            .process_chunk(raw_chunk, Duration::ZERO, Some(Duration::from_secs(2)))
            .unwrap();

        let request = SubtitleRenderRequest::new(Duration::from_millis(500), 640, 360);
        let SubtitleRenderOutput::Alpha(first) = renderer.render(request).unwrap() else {
            panic!("stream renderer should produce alpha bitmaps");
        };
        assert!(!first.parts.is_empty());
        assert!(first.parts.iter().all(|part| part.placement.x < 220));
        assert!(first.parts.iter().all(|part| part.placement.y < 180));

        // Matroska ReadOrder duplicate checking remains enabled, so processing
        // the same frame twice in one generation must not create a second event.
        renderer
            .process_chunk(raw_chunk, Duration::ZERO, Some(Duration::from_secs(2)))
            .unwrap();
        let SubtitleRenderOutput::Alpha(duplicate) = renderer.render(request).unwrap() else {
            panic!("stream renderer should produce alpha bitmaps");
        };
        assert_eq!(duplicate.parts, first.parts);

        renderer.flush_events();
        let SubtitleRenderOutput::Alpha(flushed) = renderer.render(request).unwrap() else {
            panic!("stream renderer should produce alpha bitmaps");
        };
        assert!(flushed.parts.is_empty());

        // A seek/generation reset flushes the duplicate cache as well.
        renderer
            .process_chunk(raw_chunk, Duration::ZERO, Some(Duration::from_secs(2)))
            .unwrap();
        let SubtitleRenderOutput::Alpha(after_seek) = renderer.render(request).unwrap() else {
            panic!("stream renderer should produce alpha bitmaps");
        };
        assert!(!after_seek.parts.is_empty());
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_log_bridge_captures_missing_style_exactly() {
        let resources = AssTrackResources::new(
            2,
            Arc::<[u8]>::from(STREAM_ASS_HEADER.as_bytes()),
            Arc::<[SubtitleFontAttachment]>::from([]),
        );
        let mut renderer =
            LibassSubtitleRenderer::from_ass_track(2, &resources, LibassRenderConfig::default())
                .unwrap();
        renderer
            .process_chunk(
                "8,0,Missing Style,,0,0,0,,text",
                Duration::ZERO,
                Some(Duration::from_secs(1)),
            )
            .unwrap();
        let _ = renderer
            .render(SubtitleRenderRequest::new(
                Duration::from_millis(100),
                640,
                360,
            ))
            .unwrap();

        let seen = renderer.runtime._log_context.seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|message| message.contains("no style named"))
        );
    }

    #[test]
    fn default_subtitle_style_keeps_the_built_in_ass_colors() {
        let header = default_ass_script_header(&SubtitleAssStyle::default());

        assert!(header.contains(&format!(
            "Style: Default,{DEFAULT_ASS_FONT_FAMILY},48,&H00FFFFFF,&H000000FF,&H80000000,&H80000000,"
        )));
    }

    #[test]
    fn subtitle_style_config_drives_the_generated_script_header() {
        let style = SubtitleAssStyle {
            style: SubtitleStyleConfig {
                font_family: "  Erika Sans  ".to_string(),
                primary_color_rgba: 0xff00_00ff,
                outline_color_rgba: 0x0000_ff80,
                ..SubtitleStyleConfig::default()
            },
            ..SubtitleAssStyle::default()
        };

        let header = default_ass_script_header(&style);

        // Family is trimmed, and 0xRRGGBBAA becomes ASS' &HAABBGGRR with the
        // leading byte carrying transparency instead of alpha.
        assert!(header.contains("Style: Default,Erika Sans,48,&H000000FF,&H000000FF,&H7FFF0000,"));
    }

    #[test]
    fn subtitle_style_config_drives_font_size_and_outline_width() {
        let style = SubtitleAssStyle {
            font_scale: 1.5,
            style: SubtitleStyleConfig {
                font_size: 30.0,
                outline_width: 4.0,
                ..SubtitleStyleConfig::default()
            },
            ..SubtitleAssStyle::default()
        };

        let header = default_ass_script_header(&style);

        // Both metrics are multiplied by the viewer's subtitle scale.
        assert!(header.contains(&format!("Style: Default,{DEFAULT_ASS_FONT_FAMILY},45,")));
        assert!(header.contains(",1,6,0,2,48,48,54,1"));
    }

    #[test]
    fn subtitle_style_config_drives_extended_ass_style_fields() {
        let style = SubtitleAssStyle {
            font_scale: 1.5,
            style: SubtitleStyleConfig {
                bold: true,
                italic: true,
                underline: true,
                strike_out: true,
                spacing: 2.0,
                scale_x_percent: 90.0,
                scale_y_percent: 110.0,
                border_style: 3,
                shadow_depth: 4.0,
                alignment: 8,
                margin_left: 12,
                margin_right: 34,
                margin_vertical: 56,
                ..SubtitleStyleConfig::default()
            },
            ..SubtitleAssStyle::default()
        };

        let header = default_ass_script_header(&style);

        assert!(header.contains(",-1,-1,-1,-1,90,110,3,0,3,3,6,8,12,34,56,1"));
    }

    #[test]
    fn subtitle_style_config_clamps_metrics_and_rejects_non_finite() {
        let clamped = SubtitleStyleConfig {
            font_size: 10_000.0,
            outline_width: -5.0,
            ..SubtitleStyleConfig::default()
        }
        .normalized();
        assert_eq!(clamped.font_size, MAX_ASS_FONT_SIZE);
        assert_eq!(clamped.outline_width, 0.0);

        let restored = SubtitleStyleConfig {
            font_size: f64::NAN,
            outline_width: f64::INFINITY,
            ..SubtitleStyleConfig::default()
        }
        .normalized();
        assert_eq!(restored.font_size, DEFAULT_SUBTITLE_FONT_SIZE);
        assert_eq!(restored.outline_width, DEFAULT_SUBTITLE_OUTLINE_WIDTH);

        let extended = SubtitleStyleConfig {
            spacing: f64::NAN,
            scale_x_percent: 0.0,
            scale_y_percent: 10_000.0,
            border_style: 2,
            shadow_depth: f64::INFINITY,
            blur: -1.0,
            alignment: 10,
            margin_left: -1,
            margin_right: 20_000,
            margin_vertical: -5,
            override_mask: u32::MAX,
            ..SubtitleStyleConfig::default()
        }
        .normalized();
        assert_eq!(extended.spacing, 0.0);
        assert_eq!(extended.scale_x_percent, 1.0);
        assert_eq!(extended.scale_y_percent, 1000.0);
        assert_eq!(extended.border_style, 1);
        assert_eq!(extended.shadow_depth, 0.0);
        assert_eq!(extended.blur, 0.0);
        assert_eq!(extended.alignment, 2);
        assert_eq!(extended.margin_left, 0);
        assert_eq!(extended.margin_right, 10_000);
        assert_eq!(extended.margin_vertical, 0);
        assert_eq!(extended.override_mask, SUBTITLE_OVERRIDE_ALL);
    }

    #[test]
    fn subtitle_override_masks_match_libass_selective_bits() {
        assert_eq!(SUBTITLE_OVERRIDE_LEGACY_FORCE, 0x005c);
        assert_eq!(SUBTITLE_OVERRIDE_ALL, 0x09fc);
    }

    #[test]
    fn ass_color_tag_inverts_alpha_and_reverses_rgb() {
        assert_eq!(ass_color_tag(0xffff_ffff), "&H00FFFFFF");
        assert_eq!(ass_color_tag(0x0000_007f), "&H80000000");
        assert_eq!(ass_color_tag(0x1234_5600), "&HFF563412");
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_style_color_moves_alpha_into_the_transparency_byte() {
        assert_eq!(libass_style_color(0xffff_ffff), 0xffff_ff00);
        assert_eq!(libass_style_color(0xff00_0000), 0xff00_00ff);
    }

    #[cfg(feature = "libass")]
    #[test]
    fn selective_override_metrics_cancel_libass_play_res_normalization() {
        assert_eq!(selective_override_metric(48.0, 1.0, 288), 48.0);
        assert_eq!(selective_override_metric(48.0, 1.0, 720), 19.2);
        assert_eq!(selective_override_metric(48.0, 1.0, 1080), 12.8);
        assert_eq!(selective_override_metric(48.0, 1.5, 1080), 19.2);
        assert_eq!(selective_override_metric(2.0, 1.5, 1080), 0.8);
    }

    #[test]
    fn bundled_ass_fallback_family_matches_asset() {
        let mut database = fontdb::Database::new();
        database.load_font_data(crate::NIPAPLAY_FALLBACK_FONT.to_vec());
        let families = database
            .faces()
            .flat_map(|face| face.families.iter().map(|(family, _)| family.clone()))
            .collect::<Vec<_>>();

        assert!(
            families
                .iter()
                .any(|family| family == BUNDLED_ASS_FALLBACK_FONT_FAMILY),
            "bundled fallback font families {families:?} must contain {BUNDLED_ASS_FALLBACK_FONT_FAMILY}"
        );
        assert!(BUNDLED_ASS_FALLBACK_FONT_NAME.ends_with(".ttf"));
    }

    #[cfg(feature = "libass")]
    #[test]
    fn default_ass_font_family_matches_platform_fallback_policy() {
        #[cfg(target_os = "macos")]
        assert_eq!(
            default_ass_font_family_cstr().to_str().unwrap(),
            BUNDLED_ASS_FALLBACK_FONT_FAMILY
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            default_ass_font_family_cstr().to_str().unwrap(),
            DEFAULT_ASS_FONT_FAMILY
        );
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_forced_override_repaints_dialogue_in_the_configured_color() {
        let request = SubtitleRenderRequest::new(Duration::from_millis(500), 640, 360);
        let red = |bitmaps: &SubtitleBitmapSet| {
            bitmaps.parts.iter().any(|part| {
                let color = AssColor::from_libass_rgba(part.color_rgba);
                color.red == 0xff && color.green == 0 && color.blue == 0
            })
        };

        // The script paints white; a fallback-only style must not touch it.
        let mut renderer = LibassSubtitleRenderer::from_ass_script(
            SIMPLE_ASS_SCRIPT,
            LibassRenderConfig::default(),
        )
        .unwrap();
        let style = SubtitleStyleConfig {
            primary_color_rgba: 0xff00_00ff,
            ..SubtitleStyleConfig::default()
        };
        renderer.set_style(&style);
        let SubtitleRenderOutput::Alpha(fallback) = renderer.render(request).unwrap() else {
            panic!("libass renderer should produce alpha bitmap output");
        };
        assert!(!fallback.parts.is_empty());
        assert!(
            !red(&fallback),
            "colors must stay a fallback until the color override bit is set"
        );

        renderer.set_style(&SubtitleStyleConfig {
            override_mask: SUBTITLE_OVERRIDE_COLORS,
            ..style
        });
        let SubtitleRenderOutput::Alpha(overridden) = renderer.render(request).unwrap() else {
            panic!("libass renderer should produce alpha bitmap output");
        };
        assert!(red(&overridden), "forced override must repaint dialogue");
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_forced_override_uses_viewport_sized_metrics() {
        let request = SubtitleRenderRequest::new(Duration::from_millis(500), 1920, 1080);
        let tallest = |bitmaps: &SubtitleBitmapSet| {
            bitmaps
                .parts
                .iter()
                .map(|part| part.placement.height)
                .max()
                .unwrap_or(0)
        };

        let style = SubtitleStyleConfig {
            font_size: 48.0,
            outline_width: 2.0,
            override_mask: SUBTITLE_OVERRIDE_FONT_SIZE_FIELDS | SUBTITLE_OVERRIDE_BORDER,
            ..SubtitleStyleConfig::default()
        };
        let mut heights = Vec::new();
        for font_scale in [1.0, 1.5, 2.0] {
            let mut renderer = LibassSubtitleRenderer::from_ass_script_with_style(
                SIMPLE_ASS_SCRIPT,
                LibassRenderConfig::default(),
                &style,
            )
            .unwrap();
            renderer.set_override_font_scale(font_scale);
            renderer.set_play_res_height(1080);
            let SubtitleRenderOutput::Alpha(overridden) = renderer.render(request).unwrap() else {
                panic!("libass renderer should produce alpha bitmap output");
            };

            heights.push(tallest(&overridden));
        }
        assert!(heights[0] < heights[1]);
        assert!(heights[1] < heights[2]);
        assert!(heights[0] < 80);
    }

    /// `ass_set_fonts` rebuilds libass' font selector, re-parsing every font the
    /// library holds. Metric-only changes arrive from the render path on every
    /// resize and every scale tweak, so they must not reach it.
    #[cfg(feature = "libass")]
    #[test]
    fn libass_rebuilds_the_font_selector_only_when_the_font_selection_changes() {
        let style = SubtitleStyleConfig {
            override_mask: SUBTITLE_OVERRIDE_ALL,
            ..SubtitleStyleConfig::default()
        };
        let mut renderer = LibassSubtitleRenderer::from_ass_script_with_style(
            SIMPLE_ASS_SCRIPT,
            LibassRenderConfig::default(),
            &style,
        )
        .unwrap();
        let installed = renderer.runtime.fonts_generation;
        assert_eq!(
            installed, 1,
            "construction installs the font selection once"
        );

        renderer.set_play_res_height(1080);
        renderer.set_play_res_height(720);
        renderer.set_override_font_scale(1.5);
        renderer.set_font_scale(2.0);
        renderer.set_style(&SubtitleStyleConfig {
            font_size: 64.0,
            primary_color_rgba: 0xff00_00ff,
            margin_vertical: 96,
            ..style.clone()
        });
        assert_eq!(
            renderer.runtime.fonts_generation, installed,
            "metric and colour changes must not rebuild the font selector"
        );

        // A family that differs from every platform's default fallback.
        renderer.set_style(&SubtitleStyleConfig {
            font_family: "Erika Override Family".to_string(),
            ..style
        });
        assert_eq!(
            renderer.runtime.fonts_generation,
            installed + 1,
            "a new family must reach libass"
        );
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_memory_font_family_chain_preserves_selection_order() {
        let fonts = Arc::from(vec![
            SubtitleFontAttachment::new(
                "first.ttf",
                None,
                vec!["First Family".to_string()],
                crate::NIPAPLAY_FALLBACK_FONT,
            ),
            SubtitleFontAttachment::new(
                "second.ttf",
                None,
                vec!["Second Family".to_string()],
                crate::NIPAPLAY_FALLBACK_FONT,
            ),
        ]);
        let renderer = LibassSubtitleRenderer::from_ass_script_with_style_and_fonts(
            SIMPLE_ASS_SCRIPT,
            LibassRenderConfig::default(),
            &SubtitleStyleConfig::default(),
            fonts,
        )
        .unwrap();

        assert_eq!(
            renderer.runtime.default_family().to_bytes(),
            b"First Family\x1fSecond Family\x1fDroid Sans Fallback"
        );
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_tries_later_default_families_when_an_earlier_one_is_missing() {
        let script = SIMPLE_ASS_SCRIPT.replace("Arial", "Erika Definitely Missing");
        let fonts = Arc::from(vec![
            SubtitleFontAttachment::new(
                "missing.ttf",
                None,
                vec!["Erika Missing Memory Family".to_string()],
                Arc::<[u8]>::from(b"not a font".as_slice()),
            ),
            SubtitleFontAttachment::new(
                "fallback.ttf",
                None,
                vec![BUNDLED_ASS_FALLBACK_FONT_FAMILY.to_string()],
                crate::NIPAPLAY_FALLBACK_FONT,
            ),
        ]);
        let mut renderer = LibassSubtitleRenderer::from_ass_script_with_style_and_fonts(
            script,
            LibassRenderConfig::default(),
            &SubtitleStyleConfig::default(),
            fonts,
        )
        .unwrap();

        let SubtitleRenderOutput::Alpha(bitmaps) = renderer
            .render(SubtitleRenderRequest::new(
                Duration::from_millis(500),
                640,
                360,
            ))
            .unwrap()
        else {
            panic!("libass renderer should produce alpha bitmap output");
        };
        assert!(!bitmaps.parts.is_empty());
        let messages = renderer.runtime._log_context.seen.lock().unwrap();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("Using default font family")),
            "the patched selector must advance past the missing first family"
        );
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_loads_a_custom_font_file_for_the_configured_family() {
        // Keep the name unique so concurrent test runs cannot clobber each other.
        let path = std::env::temp_dir().join(format!(
            "erika_subtitle_custom_font_{}.ttf",
            std::process::id()
        ));
        std::fs::write(&path, crate::NIPAPLAY_FALLBACK_FONT).unwrap();

        let style = SubtitleStyleConfig {
            font_family: BUNDLED_ASS_FALLBACK_FONT_FAMILY.to_string(),
            font_file_path: path.to_string_lossy().to_string(),
            ..SubtitleStyleConfig::default()
        };
        let mut renderer = LibassSubtitleRenderer::from_ass_script_with_style(
            SIMPLE_ASS_SCRIPT,
            LibassRenderConfig::default(),
            &style,
        )
        .unwrap();

        assert_eq!(renderer.style(), &style);
        assert!(
            renderer
                .runtime
                .loaded_font_files
                .contains_key(&style.font_file_path)
        );
        let SubtitleRenderOutput::Alpha(bitmaps) = renderer
            .render(SubtitleRenderRequest::new(
                Duration::from_millis(500),
                640,
                360,
            ))
            .unwrap()
        else {
            panic!("libass renderer should produce alpha bitmap output");
        };
        assert!(!bitmaps.parts.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_rejects_a_missing_custom_font_file_without_failing_the_renderer() {
        let style = SubtitleStyleConfig {
            font_file_path: "/erika/no/such/font.ttf".to_string(),
            ..SubtitleStyleConfig::default()
        };
        let mut renderer = LibassSubtitleRenderer::from_ass_script_with_style(
            SIMPLE_ASS_SCRIPT,
            LibassRenderConfig::default(),
            &style,
        )
        .unwrap();

        assert!(renderer.runtime.loaded_font_files.is_empty());
        let SubtitleRenderOutput::Alpha(bitmaps) = renderer
            .render(SubtitleRenderRequest::new(
                Duration::from_millis(500),
                640,
                360,
            ))
            .unwrap()
        else {
            panic!("libass renderer should produce alpha bitmap output");
        };
        assert!(!bitmaps.parts.is_empty());
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_loads_a_custom_font_that_appears_after_initial_configuration() {
        let path = std::env::temp_dir().join(format!(
            "erika_subtitle_late_custom_font_{}.ttf",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let style = SubtitleStyleConfig {
            font_file_path: path.to_string_lossy().to_string(),
            ..SubtitleStyleConfig::default()
        };
        let mut renderer = LibassSubtitleRenderer::from_ass_script_with_style(
            SIMPLE_ASS_SCRIPT,
            LibassRenderConfig::default(),
            &style,
        )
        .unwrap();
        assert!(renderer.runtime.loaded_font_files.is_empty());

        std::fs::write(&path, crate::NIPAPLAY_FALLBACK_FONT).unwrap();
        renderer.set_style(&style);

        assert!(
            renderer
                .runtime
                .loaded_font_files
                .contains_key(&style.font_file_path)
        );
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_rejects_an_oversized_custom_font_before_reading_it() {
        let path = std::env::temp_dir().join(format!(
            "erika_subtitle_oversized_custom_font_{}.ttf",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CUSTOM_SUBTITLE_FONT_BYTES + 1).unwrap();
        let style = SubtitleStyleConfig {
            font_file_path: path.to_string_lossy().to_string(),
            ..SubtitleStyleConfig::default()
        };

        let renderer = LibassSubtitleRenderer::from_ass_script_with_style(
            SIMPLE_ASS_SCRIPT,
            LibassRenderConfig::default(),
            &style,
        )
        .unwrap();

        assert!(renderer.runtime.loaded_font_files.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_rejects_a_non_font_custom_file() {
        let path = std::env::temp_dir().join(format!(
            "erika_subtitle_invalid_custom_font_{}.ttf",
            std::process::id()
        ));
        std::fs::write(&path, b"not a font").unwrap();
        let style = SubtitleStyleConfig {
            font_file_path: path.to_string_lossy().to_string(),
            ..SubtitleStyleConfig::default()
        };

        let renderer = LibassSubtitleRenderer::from_ass_script_with_style(
            SIMPLE_ASS_SCRIPT,
            LibassRenderConfig::default(),
            &style,
        )
        .unwrap();

        assert!(renderer.runtime.loaded_font_files.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_reloads_a_changed_custom_font_at_the_same_path() {
        let path = std::env::temp_dir().join(format!(
            "erika_subtitle_replaced_custom_font_{}.ttf",
            std::process::id()
        ));
        std::fs::write(&path, crate::NIPAPLAY_FALLBACK_FONT).unwrap();
        let style = SubtitleStyleConfig {
            font_file_path: path.to_string_lossy().to_string(),
            ..SubtitleStyleConfig::default()
        };
        let mut renderer = LibassSubtitleRenderer::from_ass_script_with_style(
            SIMPLE_ASS_SCRIPT,
            LibassRenderConfig::default(),
            &style,
        )
        .unwrap();
        let initial = renderer.runtime.loaded_font_files[&style.font_file_path];

        let mut replacement = crate::NIPAPLAY_FALLBACK_FONT.to_vec();
        replacement.push(0);
        std::fs::write(&path, replacement).unwrap();
        renderer.set_style(&style);

        let replaced = renderer.runtime.loaded_font_files[&style.font_file_path];
        assert_ne!(initial, replaced);
        assert_eq!(replaced.size, initial.size + 1);
        assert_eq!(
            renderer.runtime.custom_font_loads, 1,
            "same-path replacement must use a fresh libass library instead of appending another face",
        );
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "libass")]
    #[test]
    fn libass_falls_back_to_bundled_font_for_unavailable_family() {
        const MISSING_FONT_ASS_SCRIPT: &str = r#"[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Erika No Such Font 9c1f,32,&H00FFFFFF,&H000000FF,&H80000000,&H80000000,0,0,0,0,100,100,0,0,1,2,0,2,20,20,24,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,中文字幕 fallback
"#;

        let mut renderer = LibassSubtitleRenderer::from_ass_script(
            MISSING_FONT_ASS_SCRIPT,
            LibassRenderConfig::default(),
        )
        .unwrap();
        let SubtitleRenderOutput::Alpha(bitmaps) = renderer
            .render(SubtitleRenderRequest::new(
                Duration::from_millis(500),
                640,
                360,
            ))
            .unwrap()
        else {
            panic!("libass renderer should produce alpha bitmap output");
        };

        assert!(
            !bitmaps.parts.is_empty(),
            "an unavailable style font must still rasterize through the bundled fallback"
        );
        let seen = renderer.runtime._log_context.seen.lock().unwrap();
        assert!(
            !seen.iter().any(|message| message
                .to_ascii_lowercase()
                .contains("failed to find any fallback")),
            "libass reported no usable fallback font: {seen:?}"
        );
    }
}
