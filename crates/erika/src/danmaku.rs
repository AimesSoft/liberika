//! Native liberika danmaku pipeline.
//!
//! The DFM-style track retainer and filters are adapted from the MIT-licensed
//! NipaPlay-Reload DFM+ Rust implementation:
//! Copyright (c) 2025 MCDFsteve.

mod dfm;
pub mod dfm_core;
mod outline;
mod typography;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ab_glyph::{
    CodepointIdIter, Font, FontArc, FontRef, FontVec, Glyph, GlyphId, GlyphSvg, Outline, ScaleFont,
    v2,
};
use serde_json::Value;
use thiserror::Error;

use crate::NIPAPLAY_FALLBACK_FONT;
use crate::font::SubtitleFontAttachment;
use crate::text::TextShaper;
use outline::{raster_radius, resolve_width_px};
use typography::{
    DEFAULT_CONFIG_FONT_SIZE, DEFAULT_NATIVE_FONT_SIZE, DEFAULT_SOURCE_FONT_SIZE,
    effective_config_font_size, effective_font_size, px_scale_for_em,
};

const DEFAULT_SCROLL_DURATION: Duration = Duration::from_millis(9000);
const DEFAULT_STATIC_DURATION: Duration = Duration::from_millis(3800);
const DEFAULT_GENERATION: u64 = 1;
const GLYPH_ATLAS_WIDTH: u32 = 2048;
const GLYPH_ATLAS_INITIAL_HEIGHT: u32 = 256;
// Supersample glyphs, never the video surface. Large glyphs already have
// enough samples; cap their extra raster cost and recycle caches between
// planning windows so a long session cannot accumulate every size forever.
const MAX_SUPERSAMPLED_FONT_SIZE: f32 = 128.0;
const GLYPH_CACHE_SOFT_LIMIT: usize = 4096;
const TEXT_CACHE_SOFT_LIMIT: usize = 2048;
const GLYPH_ATLAS_SOFT_HEIGHT: u32 = 8192;
// Renderer caches key by version and dimensions. A new atlas must never
// alias an old one after a quality toggle, font change, or cache rollover.
static NEXT_GLYPH_ATLAS_VERSION: AtomicU64 = AtomicU64::new(1);
const SCROLL_DURATION_REFERENCE_LOGICAL_WIDTH: f32 = 1280.0;
const SCROLL_DURATION_MIN_WIDTH_SCALE: f32 = 0.9;
const SCROLL_DURATION_MAX_WIDTH_SCALE: f32 = 1.3;
const DEFAULT_DANMAKU_TRACK_ID: u64 = 1;
const MAX_CUSTOM_DANMAKU_FONT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESIDENT_FALLBACK_FACES: usize = 8;
pub const DANMAKU_DEBUG_BUCKETS: usize = 16;

#[derive(Debug, Error)]
pub enum DanmakuError {
    #[error("invalid danmaku field: {0}")]
    InvalidField(String),
    #[error("missing danmaku text")]
    MissingText,
    #[error("danmaku parse error: {0}")]
    Parse(String),
    #[error("danmaku io error: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, DanmakuError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DanmakuMode {
    Scroll,
    ScrollReverse,
    Top,
    Bottom,
    Special,
}

impl DanmakuMode {
    pub fn from_bilibili_mode(value: u32) -> Self {
        match value {
            6 => Self::ScrollReverse,
            5 => Self::Top,
            4 => Self::Bottom,
            7 => Self::Special,
            _ => Self::Scroll,
        }
    }

    fn from_text(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "top" | "fix_top" | "fixed_top" | "5" => Self::Top,
            "bottom" | "fix_bottom" | "fixed_bottom" | "4" => Self::Bottom,
            "reverse" | "scroll_reverse" | "left" | "l2r" | "6" => Self::ScrollReverse,
            "special" | "7" => Self::Special,
            _ => Self::Scroll,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DanmakuColor {
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub alpha: f32,
}

impl DanmakuColor {
    pub const WHITE: Self = Self::rgb_u8(255, 255, 255);

    pub const fn rgb_u8(red: u8, green: u8, blue: u8) -> Self {
        Self {
            red: red as f32 / 255.0,
            green: green as f32 / 255.0,
            blue: blue as f32 / 255.0,
            alpha: 1.0,
        }
    }

    pub fn rgba(self, opacity: f32) -> [f32; 4] {
        [
            self.red.clamp(0.0, 1.0),
            self.green.clamp(0.0, 1.0),
            self.blue.clamp(0.0, 1.0),
            (self.alpha * opacity).clamp(0.0, 1.0),
        ]
    }

    fn outline_rgba(self, opacity: f32) -> [f32; 4] {
        // Only near-black text needs a white stroke. A luminance/contrast
        // threshold wrongly turns saturated red and blue into white-outlined
        // text; the source color (not its opacity) decides this policy.
        let brightest = self.red.max(self.green).max(self.blue);
        let stroke = if brightest <= 64.0 / 255.0 { 1.0 } else { 0.0 };
        [stroke, stroke, stroke, self.rgba(opacity)[3]]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuItem {
    pub id: u64,
    pub pts: Duration,
    pub text: String,
    pub mode: DanmakuMode,
    pub font_size: f32,
    pub color: DanmakuColor,
    pub opacity: f32,
    pub is_self: bool,
}

impl DanmakuItem {
    fn normalized(mut self, fallback_id: u64) -> Result<Self> {
        if self.text.trim().is_empty() {
            return Err(DanmakuError::MissingText);
        }
        if self.id == 0 {
            self.id = fallback_id;
        }
        self.font_size = sanitize_f32(self.font_size, DEFAULT_SOURCE_FONT_SIZE).max(1.0);
        self.opacity = sanitize_f32(self.opacity, 1.0).clamp(0.0, 1.0);
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DanmakuViewport {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

impl DanmakuViewport {
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_scale(width, height, 1.0)
    }

    pub fn with_scale(width: u32, height: u32, scale_factor: f32) -> Self {
        let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        Self {
            width: width.max(1),
            height: height.max(1),
            scale_factor,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuLayoutConfig {
    pub font_size: f32,
    /// Rasterize glyphs at twice their display resolution without changing layout.
    /// Enabled by default; glyphs larger than 128 px retain native resolution.
    pub supersampling: bool,
    pub opacity: f32,
    pub display_area: f32,
    pub scroll_duration_seconds: f32,
    pub scroll_speed_factor: f32,
    pub track_gap_ratio: f32,
    pub outline_width: f32,
    pub shadow_offset: [f32; 2],
    pub shadow_style: DanmakuShadowStyle,
    pub custom_font_family: String,
    pub custom_font_file_path: String,
    #[doc(hidden)]
    pub custom_font_face_index: u32,
    pub merge_duplicates: bool,
    pub allow_stacking: bool,
    pub allow_scroll_overwrite: bool,
    pub max_quantity: Option<u32>,
    pub max_lines_per_mode: Option<u32>,
    pub block_top: bool,
    pub block_bottom: bool,
    pub block_scroll: bool,
    pub block_words: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DanmakuShadowStyle {
    None,
    Soft,
    Medium,
    #[default]
    Strong,
}

impl DanmakuShadowStyle {
    pub fn from_code(value: i32) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Soft,
            2 => Self::Medium,
            3 => Self::Strong,
            _ => Self::Strong,
        }
    }

    pub fn code(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Soft => 1,
            Self::Medium => 2,
            Self::Strong => 3,
        }
    }
}

impl Default for DanmakuLayoutConfig {
    fn default() -> Self {
        Self {
            font_size: DEFAULT_CONFIG_FONT_SIZE,
            supersampling: true,
            opacity: 1.0,
            display_area: 1.0,
            scroll_duration_seconds: 10.0,
            scroll_speed_factor: 1.0,
            track_gap_ratio: 0.15,
            outline_width: 1.0,
            shadow_offset: [1.0, 1.0],
            shadow_style: DanmakuShadowStyle::Strong,
            custom_font_family: String::new(),
            custom_font_file_path: String::new(),
            custom_font_face_index: 0,
            merge_duplicates: false,
            allow_stacking: false,
            allow_scroll_overwrite: false,
            max_quantity: None,
            max_lines_per_mode: None,
            block_top: false,
            block_bottom: false,
            block_scroll: false,
            block_words: Vec::new(),
            enabled: true,
        }
    }
}

impl DanmakuLayoutConfig {
    pub(crate) fn sanitized(&self) -> Self {
        let mut config = self.clone();
        config.font_size = sanitize_f32(config.font_size, DEFAULT_CONFIG_FONT_SIZE).max(1.0);
        config.opacity = sanitize_f32(config.opacity, 1.0).clamp(0.0, 1.0);
        config.display_area = sanitize_f32(config.display_area, 1.0).clamp(0.1, 1.0);
        config.scroll_duration_seconds =
            sanitize_f32(config.scroll_duration_seconds, 10.0).clamp(1.0, 60.0);
        config.scroll_speed_factor = sanitize_f32(config.scroll_speed_factor, 1.0).clamp(0.2, 4.0);
        config.track_gap_ratio = sanitize_f32(config.track_gap_ratio, 0.15).clamp(0.0, 2.0);
        config.outline_width = sanitize_f32(config.outline_width, 1.0).clamp(0.0, 4.0);
        config.shadow_offset = [
            sanitize_f32(config.shadow_offset[0], 1.0),
            sanitize_f32(config.shadow_offset[1], 1.0),
        ];
        config.custom_font_family = config.custom_font_family.trim().to_string();
        config.custom_font_file_path = config.custom_font_file_path.trim().to_string();
        config
    }

    /// Clears the fields that only affect painting, so two configs that differ
    /// solely in those fields compare equal.
    ///
    /// Written as a subtractive normalization rather than a list of
    /// layout-affecting fields on purpose: a field added later is treated as
    /// layout-affecting until it is explicitly listed here. That costs a
    /// needless relayout at worst, whereas forgetting to extend an additive
    /// list would silently drop the setting ("changing it does nothing").
    fn without_paint_only_fields(&self) -> Self {
        let mut config = self.clone();
        config.opacity = 0.0;
        config.shadow_offset = [0.0, 0.0];
        config.shadow_style = DanmakuShadowStyle::default();
        config.enabled = false;
        config
    }

    fn layout_equivalent(&self, other: &Self) -> bool {
        self.without_paint_only_fields() == other.without_paint_only_fields()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DanmakuConfigChange {
    Unchanged,
    PaintOnly,
    Layout,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DanmakuTimeline {
    items: Vec<DanmakuItem>,
}

impl DanmakuTimeline {
    pub fn new(items: Vec<DanmakuItem>) -> Result<Self> {
        let mut timeline = Self { items: Vec::new() };
        timeline.extend(items)?;
        Ok(timeline)
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let input =
            fs::read_to_string(path).map_err(|error| DanmakuError::Io(error.to_string()))?;
        match path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
        {
            Some(ext) if ext == "xml" => Self::from_bilibili_xml(&input),
            Some(ext) if ext == "jsonl" || ext == "ndjson" => Self::from_json_lines(&input),
            Some(ext) if ext == "json" => Self::from_json(&input),
            _ => Self::parse_auto(&input),
        }
    }

    pub fn parse_auto(input: &str) -> Result<Self> {
        let trimmed = input.trim_start();
        if trimmed.starts_with('<') {
            Self::from_bilibili_xml(input)
        } else if trimmed.starts_with('{') {
            Self::from_json(input).or_else(|_| Self::from_json_lines(input))
        } else if trimmed.starts_with('[') {
            Self::from_json(input)
        } else {
            Self::from_json_lines(input)
        }
    }

    pub fn from_json(input: &str) -> Result<Self> {
        let value: Value =
            serde_json::from_str(input).map_err(|error| DanmakuError::Parse(error.to_string()))?;
        let comments = match value {
            Value::Array(items) => items,
            Value::Object(mut object) => object
                .remove("comments")
                .or_else(|| object.remove("danmaku"))
                .or_else(|| object.remove("items"))
                .and_then(|value| value.as_array().cloned())
                .ok_or_else(|| DanmakuError::InvalidField("comments".to_string()))?,
            _ => return Err(DanmakuError::InvalidField("json root".to_string())),
        };
        let mut items = Vec::new();
        for (index, value) in comments.iter().enumerate() {
            match item_from_json_value(value, index as u64 + 1) {
                Ok(item) => items.push(item),
                Err(error) if should_skip_item_error(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        Self::new(items)
    }

    pub fn from_json_lines(input: &str) -> Result<Self> {
        let mut items = Vec::new();
        for (line_index, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line).map_err(|error| {
                DanmakuError::Parse(format!("line {}: {error}", line_index + 1))
            })?;
            match item_from_json_value(&value, items.len() as u64 + 1) {
                Ok(item) => items.push(item),
                Err(error) if should_skip_item_error(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        Self::new(items)
    }

    pub fn from_bilibili_xml(input: &str) -> Result<Self> {
        Self::new(parse_bilibili_xml(input)?)
    }

    pub fn push(&mut self, item: DanmakuItem) -> Result<()> {
        let fallback = self.items.len() as u64 + 1;
        self.items.push(item.normalized(fallback)?);
        self.items.sort_by_key(|item| item.pts);
        Ok(())
    }

    pub fn extend<I>(&mut self, items: I) -> Result<()>
    where
        I: IntoIterator<Item = DanmakuItem>,
    {
        for item in items {
            let fallback = self.items.len() as u64 + 1;
            match item.normalized(fallback) {
                Ok(item) => self.items.push(item),
                Err(DanmakuError::MissingText) => continue,
                Err(error) => return Err(error),
            }
        }
        self.items.sort_by_key(|item| item.pts);
        Ok(())
    }

    pub fn items(&self) -> &[DanmakuItem] {
        &self.items
    }

    pub fn window(&self, start: Duration, end: Duration) -> Self {
        if end < start || self.items.is_empty() {
            return Self::default();
        }
        let items = self
            .items
            .iter()
            .filter(|item| item.pts >= start && item.pts <= end)
            .cloned()
            .collect();
        Self { items }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DanmakuTrackSource {
    Unknown,
    File(PathBuf),
    Json,
    Remote(String),
    Manual,
}

impl DanmakuTrackSource {
    pub fn label(&self) -> String {
        match self {
            Self::Unknown => String::new(),
            Self::File(path) => path.to_string_lossy().into_owned(),
            Self::Json => "json".to_string(),
            Self::Remote(value) => value.clone(),
            Self::Manual => "manual".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuTrack {
    id: u64,
    name: String,
    source: DanmakuTrackSource,
    timeline: DanmakuTimeline,
    enabled: bool,
    offset: i64,
}

impl DanmakuTrack {
    pub fn new(
        id: u64,
        name: impl Into<String>,
        source: DanmakuTrackSource,
        timeline: DanmakuTimeline,
    ) -> Self {
        Self {
            id: id.max(DEFAULT_DANMAKU_TRACK_ID),
            name: name.into(),
            source,
            timeline,
            enabled: true,
            offset: 0,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn source(&self) -> &DanmakuTrackSource {
        &self.source
    }

    pub fn timeline(&self) -> &DanmakuTimeline {
        &self.timeline
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn offset(&self) -> i64 {
        self.offset
    }

    pub fn offset_duration(&self) -> Duration {
        Duration::from_micros(self.offset.max(0) as u64)
    }

    pub fn item_count(&self) -> usize {
        self.timeline.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuTrackInfo {
    pub id: u64,
    pub name: String,
    pub source: String,
    pub enabled: bool,
    pub offset_micros: i64,
    pub item_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuSession {
    tracks: Vec<DanmakuTrack>,
    active_timeline: DanmakuTimeline,
    next_track_id: u64,
    global_offset: i64,
    version: u64,
    dirty: bool,
}

impl Default for DanmakuSession {
    fn default() -> Self {
        Self::new()
    }
}

impl DanmakuSession {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            active_timeline: DanmakuTimeline::default(),
            next_track_id: DEFAULT_DANMAKU_TRACK_ID,
            global_offset: 0,
            version: 1,
            dirty: false,
        }
    }

    pub fn from_timeline(timeline: DanmakuTimeline) -> Self {
        let mut session = Self::new();
        session.replace_default_track(timeline, "default", DanmakuTrackSource::Unknown);
        session
    }

    pub fn add_track(
        &mut self,
        timeline: DanmakuTimeline,
        name: impl Into<String>,
        source: DanmakuTrackSource,
    ) -> u64 {
        let id = self.allocate_track_id();
        self.add_track_with_id(id, timeline, name, source)
    }

    pub fn add_track_with_offset(
        &mut self,
        timeline: DanmakuTimeline,
        name: impl Into<String>,
        source: DanmakuTrackSource,
        offset_micros: i64,
    ) -> u64 {
        let id = self.add_track(timeline, name, source);
        let _ = self.set_track_offset(id, offset_micros);
        id
    }

    pub fn replace_default_track(
        &mut self,
        timeline: DanmakuTimeline,
        name: impl Into<String>,
        source: DanmakuTrackSource,
    ) -> u64 {
        self.tracks.clear();
        self.next_track_id = DEFAULT_DANMAKU_TRACK_ID + 1;
        self.add_track_with_id(DEFAULT_DANMAKU_TRACK_ID, timeline, name, source)
    }

    pub fn clear(&mut self) {
        if self.tracks.is_empty() && self.active_timeline.is_empty() {
            return;
        }
        self.tracks.clear();
        self.active_timeline = DanmakuTimeline::default();
        self.next_track_id = DEFAULT_DANMAKU_TRACK_ID;
        self.mark_changed();
    }

    pub fn remove_track(&mut self, track_id: u64) -> bool {
        let before = self.tracks.len();
        self.tracks.retain(|track| track.id != track_id);
        let removed = self.tracks.len() != before;
        if removed {
            self.mark_dirty();
        }
        removed
    }

    pub fn set_track_enabled(&mut self, track_id: u64, enabled: bool) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        if track.enabled != enabled {
            track.enabled = enabled;
            self.mark_dirty();
        }
        true
    }

    pub fn set_track_offset(&mut self, track_id: u64, offset_micros: i64) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        if track.offset != offset_micros {
            track.offset = offset_micros;
            self.mark_dirty();
        }
        true
    }

    pub fn set_global_offset(&mut self, offset_micros: i64) {
        if self.global_offset != offset_micros {
            self.global_offset = offset_micros;
            self.mark_dirty();
        }
    }

    pub fn global_offset(&self) -> i64 {
        self.global_offset
    }

    pub fn tracks(&self) -> &[DanmakuTrack] {
        &self.tracks
    }

    pub fn track_infos(&self) -> Vec<DanmakuTrackInfo> {
        self.tracks
            .iter()
            .map(|track| DanmakuTrackInfo {
                id: track.id,
                name: track.name.clone(),
                source: track.source.label(),
                enabled: track.enabled,
                offset_micros: track.offset,
                item_count: track.item_count(),
            })
            .collect()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn active_timeline(&mut self) -> &DanmakuTimeline {
        self.rebuild_if_dirty();
        &self.active_timeline
    }

    pub fn active_timeline_clone(&mut self) -> DanmakuTimeline {
        self.active_timeline().clone()
    }

    pub fn is_empty(&mut self) -> bool {
        self.active_timeline().is_empty()
    }

    fn add_track_with_id(
        &mut self,
        id: u64,
        timeline: DanmakuTimeline,
        name: impl Into<String>,
        source: DanmakuTrackSource,
    ) -> u64 {
        let track = DanmakuTrack::new(id, name, source, timeline);
        self.tracks.push(track);
        self.next_track_id = self.next_track_id.max(id.saturating_add(1));
        self.mark_dirty();
        id
    }

    fn allocate_track_id(&mut self) -> u64 {
        let id = self.next_track_id.max(DEFAULT_DANMAKU_TRACK_ID);
        self.next_track_id = id.saturating_add(1).max(DEFAULT_DANMAKU_TRACK_ID + 1);
        id
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.mark_changed();
    }

    fn mark_changed(&mut self) {
        self.version = self.version.saturating_add(1).max(1);
    }

    fn rebuild_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        let mut items = Vec::new();
        let mut layout_item_id = 1u64;
        for track in &self.tracks {
            if !track.enabled {
                continue;
            }
            for item in track.timeline.items() {
                let offset = track.offset.saturating_add(self.global_offset);
                let Some(pts) = apply_track_offset(item.pts, offset) else {
                    continue;
                };
                let mut item = item.clone();
                // Source IDs belong to the host's data model and may be large or
                // duplicated across sources. Layout identity only has to be
                // unique for this immutable session version, so allocate it here
                // instead of packing two arbitrary u64 values into one.
                item.id = layout_item_id;
                layout_item_id += 1;
                item.pts = pts;
                items.push(item);
            }
        }
        items.sort_by_key(|item| item.pts);
        self.active_timeline = DanmakuTimeline { items };
        self.dirty = false;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedDanmakuItem {
    pub id: u64,
    pub time: Duration,
    pub text: Arc<str>,
    pub mode: DanmakuMode,
    pub color: DanmakuColor,
    pub opacity: f32,
    pub font_size: f32,
    pub width: f32,
    pub height: f32,
    pub y: f32,
    pub track_index: usize,
    pub scroll_speed: f32,
    pub duration: Duration,
    pub duplicate_count: u32,
    text_layout: Arc<PreparedTextLayout>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DfmPreparedLayout {
    // Freeze the completed window's atlas. A background font-size rebuild
    // appends masks to the shared rasterizer; foreground previews must not
    // snapshot and upload those partial additions every frame.
    atlas: Option<Arc<DanmakuGlyphAtlas>>,
    viewport: DanmakuViewport,
    config: DanmakuLayoutConfig,
    dfm_layout: dfm::PreparedLayout,
    stats: DanmakuPreparedStats,
    scroll_duration: Duration,
    static_duration: Duration,
    item_times: Vec<f64>,
    items: Vec<PreparedDanmakuItem>,
}

impl DfmPreparedLayout {
    pub fn items(&self) -> &[PreparedDanmakuItem] {
        &self.items
    }

    pub fn stats(&self) -> DanmakuPreparedStats {
        self.stats
    }

    pub(crate) fn atlas(&self) -> Option<Arc<DanmakuGlyphAtlas>> {
        self.atlas.clone()
    }

    pub(crate) fn font_size(&self) -> f32 {
        self.config.font_size
    }

    pub(crate) fn apply_paint_config(&mut self, config: &DanmakuLayoutConfig) {
        let config = config.sanitized();
        // A layout replacement is prepared asynchronously. Its previous
        // geometry can remain on screen as a moving fallback in the meantime,
        // so paint-only fields must also be applicable across layout revisions.
        self.config.opacity = config.opacity;
        self.config.shadow_offset = config.shadow_offset;
        self.config.shadow_style = config.shadow_style;
        self.config.enabled = config.enabled;
    }

    pub fn frame_layout(&self, media_time: Duration, generation: u64) -> DanmakuFrameLayout {
        if !self.config.enabled {
            return DanmakuFrameLayout::empty(media_time, generation, self.viewport);
        }
        let current = media_time.as_secs_f64();
        let mut items = Vec::with_capacity(dfm::frame_candidate_count(&self.dfm_layout, current));
        dfm::for_each_frame_item(&self.dfm_layout, current, |frame_item| {
            let Some(item) = self.items.get(frame_item.item_index) else {
                return;
            };
            if item.mode == DanmakuMode::Special {
                return;
            }
            items.push(DanmakuPlacedItem {
                item_id: item.id,
                text: Arc::clone(&item.text),
                mode: item.mode,
                track_index: frame_item.track_index.max(0) as usize,
                x: frame_item.x as f32,
                y: frame_item.y as f32,
                width: item.width,
                height: item.height,
                font_size: item.font_size,
                color: item.color,
                opacity: item.opacity * self.config.opacity,
                outline_width: resolve_width_px(item.font_size, self.config.outline_width),
                shadow_offset: scale_offset(self.config.shadow_offset, self.viewport.scale_factor),
                shadow_alpha: shadow_style_alpha(self.config.shadow_style),
                duplicate_count: item.duplicate_count,
                text_layout: Arc::clone(&item.text_layout),
            });
        });
        DanmakuFrameLayout {
            media_time,
            generation,
            viewport: self.viewport,
            prepared_stats: self.stats,
            items,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuFrameLayout {
    pub media_time: Duration,
    pub generation: u64,
    pub viewport: DanmakuViewport,
    pub prepared_stats: DanmakuPreparedStats,
    pub items: Vec<DanmakuPlacedItem>,
}

impl DanmakuFrameLayout {
    pub fn empty(media_time: Duration, generation: u64, viewport: DanmakuViewport) -> Self {
        Self {
            media_time,
            generation,
            viewport,
            prepared_stats: DanmakuPreparedStats::default(),
            items: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuPlacedItem {
    pub item_id: u64,
    pub text: Arc<str>,
    pub mode: DanmakuMode,
    pub track_index: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub font_size: f32,
    pub color: DanmakuColor,
    pub opacity: f32,
    pub outline_width: f32,
    pub shadow_offset: [f32; 2],
    pub shadow_alpha: f32,
    pub duplicate_count: u32,
    text_layout: Arc<PreparedTextLayout>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuRenderPlan {
    pub media_time: Duration,
    pub generation: u64,
    pub viewport: DanmakuViewport,
    pub atlas: Option<Arc<DanmakuGlyphAtlas>>,
    pub items: Vec<DanmakuGlyphInstance>,
    pub frame_stats: DanmakuFrameStats,
}

impl DanmakuRenderPlan {
    /// Keep a prepared layout visible during an animated surface resize.
    /// Render backends all receive geometry in the current surface's pixels.
    pub(crate) fn retarget(&mut self, viewport: DanmakuViewport) {
        if self.viewport == viewport {
            return;
        }
        let sx = viewport.width as f32 / self.viewport.width.max(1) as f32;
        let sy = viewport.height as f32 / self.viewport.height.max(1) as f32;
        for item in &mut self.items {
            item.rect[0] *= sx;
            item.rect[1] *= sy;
            item.rect[2] *= sx;
            item.rect[3] *= sy;
            item.shadow_offset[0] *= sx;
            item.shadow_offset[1] *= sy;
        }
        self.viewport = viewport;
    }

    pub fn empty(media_time: Duration, generation: u64, viewport: DanmakuViewport) -> Self {
        Self {
            media_time,
            generation,
            viewport,
            atlas: None,
            items: Vec::new(),
            frame_stats: DanmakuFrameStats::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DanmakuFrameStats {
    pub prepared: DanmakuPreparedStats,
    pub placed_items: usize,
    pub scroll_items: usize,
    pub top_items: usize,
    pub bottom_items: usize,
    pub scroll_rows: usize,
    pub scroll_track_min: usize,
    pub scroll_track_max: usize,
    pub scroll_min_y: f32,
    pub scroll_max_y: f32,
    pub scroll_bucket_count: usize,
    pub scroll_buckets: [DanmakuDebugBucket; DANMAKU_DEBUG_BUCKETS],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DanmakuDebugBucket {
    pub key: i32,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DanmakuPreparedStats {
    pub source_items: usize,
    pub supported_items: usize,
    pub prepared_items: usize,
    pub filtered_items: usize,
    pub prepared_scroll_items: usize,
    pub prepared_top_items: usize,
    pub prepared_bottom_items: usize,
    pub prepared_scroll_rows: usize,
    pub prepared_scroll_min_y: f32,
    pub prepared_scroll_max_y: f32,
    pub expected_scroll_tracks: usize,
    pub dfm_track_count: usize,
    pub display_area_height: f32,
    pub scroll_area_height: f32,
    pub track_height: f32,
    pub scroll_bucket_count: usize,
    pub scroll_buckets: [DanmakuDebugBucket; DANMAKU_DEBUG_BUCKETS],
}

impl DanmakuFrameStats {
    fn from_layout(layout: &DanmakuFrameLayout) -> Self {
        let mut stats = Self::default();
        stats.prepared = layout.prepared_stats;
        let mut scroll_rows = BTreeSet::new();
        let mut scroll_track_min = usize::MAX;
        let mut scroll_track_max = 0usize;
        let mut bucket_counts = BTreeMap::<i32, u32>::new();
        let mut scroll_min_y = f32::MAX;
        let mut scroll_max_y = f32::MIN;
        for item in &layout.items {
            stats.placed_items += 1;
            match item.mode {
                DanmakuMode::Scroll | DanmakuMode::ScrollReverse => {
                    stats.scroll_items += 1;
                    let row_key = item.y.round() as i32;
                    scroll_rows.insert(row_key);
                    *bucket_counts.entry(row_key).or_default() += 1;
                    scroll_track_min = scroll_track_min.min(item.track_index);
                    scroll_track_max = scroll_track_max.max(item.track_index);
                    scroll_min_y = scroll_min_y.min(item.y);
                    scroll_max_y = scroll_max_y.max(item.y + item.height);
                }
                DanmakuMode::Top => stats.top_items += 1,
                DanmakuMode::Bottom => stats.bottom_items += 1,
                DanmakuMode::Special => {}
            }
        }
        stats.scroll_rows = scroll_rows.len();
        if stats.scroll_items > 0 {
            stats.scroll_track_min = scroll_track_min;
            stats.scroll_track_max = scroll_track_max;
            stats.scroll_min_y = scroll_min_y;
            stats.scroll_max_y = scroll_max_y;
        }
        stats.scroll_bucket_count = fill_debug_buckets(&bucket_counts, &mut stats.scroll_buckets);
        stats
    }
}

impl DanmakuPreparedStats {
    fn from_items(
        source_items: usize,
        supported_items: usize,
        prepared: &[PreparedDanmakuItem],
        viewport: DanmakuViewport,
        config: &DanmakuLayoutConfig,
        dfm_track_count: usize,
    ) -> Self {
        let mut stats = Self {
            source_items,
            supported_items,
            prepared_items: prepared.len(),
            filtered_items: supported_items.saturating_sub(prepared.len()),
            dfm_track_count,
            display_area_height: viewport.height as f32 * config.display_area.clamp(0.1, 1.0),
            scroll_area_height: viewport.height as f32 * config.display_area.clamp(0.1, 1.0),
            ..Self::default()
        };
        let mut scroll_rows = BTreeSet::new();
        let mut bucket_counts = BTreeMap::<i32, u32>::new();
        let mut scroll_min_y = f32::MAX;
        let mut scroll_max_y = f32::MIN;
        let mut first_scroll_height = 0.0f32;
        for item in prepared {
            match item.mode {
                DanmakuMode::Scroll | DanmakuMode::ScrollReverse => {
                    stats.prepared_scroll_items += 1;
                    let row_key = item.y.round() as i32;
                    scroll_rows.insert(row_key);
                    *bucket_counts.entry(row_key).or_default() += 1;
                    scroll_min_y = scroll_min_y.min(item.y);
                    scroll_max_y = scroll_max_y.max(item.y + item.height);
                    if first_scroll_height <= 0.0 {
                        first_scroll_height = item.height;
                    }
                }
                DanmakuMode::Top => stats.prepared_top_items += 1,
                DanmakuMode::Bottom => stats.prepared_bottom_items += 1,
                DanmakuMode::Special => {}
            }
        }
        stats.prepared_scroll_rows = scroll_rows.len();
        if stats.prepared_scroll_items > 0 {
            stats.prepared_scroll_min_y = scroll_min_y;
            stats.prepared_scroll_max_y = scroll_max_y;
        }
        let track_height =
            (first_scroll_height + first_scroll_height * config.track_gap_ratio).max(1.0);
        stats.track_height = track_height;
        stats.expected_scroll_tracks =
            (stats.scroll_area_height / track_height).floor().max(1.0) as usize;
        stats.scroll_bucket_count = fill_debug_buckets(&bucket_counts, &mut stats.scroll_buckets);
        stats
    }
}

fn fill_debug_buckets(
    source: &BTreeMap<i32, u32>,
    target: &mut [DanmakuDebugBucket; DANMAKU_DEBUG_BUCKETS],
) -> usize {
    for bucket in target.iter_mut() {
        *bucket = DanmakuDebugBucket::default();
    }
    for (slot, (&key, &count)) in target.iter_mut().zip(source.iter()) {
        *slot = DanmakuDebugBucket { key, count };
    }
    source.len()
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuGlyphInstance {
    pub item_id: u64,
    pub rect: [f32; 4],
    pub tex_rect: [f32; 4],
    pub color_rgba: [f32; 4],
    pub outline_rgba: [f32; 4],
    pub shadow_rgba: [f32; 4],
    pub shadow_offset: [f32; 2],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanmakuAtlasUpdate {
    pub from_version: u64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanmakuGlyphAtlas {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub fill_alpha: Vec<u8>,
    pub outline_alpha: Vec<u8>,
    pub version: u64,
    pub update: Option<DanmakuAtlasUpdate>,
}

impl DanmakuGlyphAtlas {
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.stride >= self.width as usize
            && self.fill_alpha.len() >= self.required_len()
            && self.outline_alpha.len() >= self.required_len()
    }

    pub fn required_len(&self) -> usize {
        self.stride.saturating_mul(self.height as usize)
    }

    pub fn incremental_update_from(
        &self,
        version: u64,
        width: u32,
        height: u32,
        stride: usize,
    ) -> Option<&DanmakuAtlasUpdate> {
        let update = self.update.as_ref()?;
        (update.from_version == version
            && self.width == width
            && self.height == height
            && self.stride == stride
            && update.width > 0
            && update.height > 0
            && update.x.saturating_add(update.width) <= self.width
            && update.y.saturating_add(update.height) <= self.height)
            .then_some(update)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PreparedTextLayout {
    metrics: TextMeasure,
    glyphs: Vec<PreparedGlyphPlacement>,
}

#[derive(Debug, Clone, PartialEq)]
struct PreparedGlyphPlacement {
    glyph: Arc<RasterizedGlyph>,
    packed: PackedGlyph,
    pen_x: f32,
}

#[derive(Debug, Clone)]
struct DanmakuFontFace {
    id: u32,
    font: Arc<FontArc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DanmakuFontSelection {
    pub generation: u64,
    pub fonts: Arc<[SubtitleFontAttachment]>,
}

impl DanmakuFontSelection {
    pub fn new(generation: u64, fonts: impl Into<Arc<[SubtitleFontAttachment]>>) -> Self {
        Self {
            generation,
            fonts: fonts.into(),
        }
    }
}

impl DanmakuFontFace {
    fn new(id: u32, font: FontArc) -> Self {
        Self {
            id,
            font: Arc::new(font),
        }
    }

    fn glyph_id(&self, ch: char) -> GlyphId {
        self.font.glyph_id(ch)
    }

    fn has_glyph(&self, ch: char) -> bool {
        ch.is_whitespace() || self.glyph_id(ch).0 != 0
    }
}

// Keep the existing CJK, generic, then database order. Preferring a color
// emoji face can hide symbols an outline-capable generic font could paint.
const PREFERRED_FALLBACK_FAMILIES: &[&str] = &[
    "PingFang SC",
    "Hiragino Sans GB",
    "Hiragino Sans",
    "Apple SD Gothic Neo",
    "Songti SC",
    "STHeiti",
    "Microsoft YaHei",
    "SimHei",
    "Noto Sans CJK SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK",
    "Source Han Sans SC",
    "Source Han Sans",
    "Arial Unicode MS",
    "Arial Unicode",
    "Segoe UI Symbol",
];

#[derive(Debug)]
struct SystemFontFallback {
    db: fontdb::Database,
    faces: Vec<fontdb::ID>,
    loaded: HashMap<fontdb::ID, Option<DanmakuFontFace>>,
    // GlyphCacheKey and kerning bake DanmakuFontFace::id in, so a face evicted
    // from the resident set must reuse its id when reloaded.
    face_ids: HashMap<fontdb::ID, u32>,
    resident: VecDeque<u32>,
    resident_cap: usize,
    char_cache: HashMap<char, Option<u32>>,
    next_font_id: u32,
}

impl SystemFontFallback {
    fn new(next_font_id: u32) -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        Self::with_database(db, next_font_id)
    }

    fn with_database(db: fontdb::Database, next_font_id: u32) -> Self {
        let mut faces = Vec::new();
        for family in PREFERRED_FALLBACK_FAMILIES {
            let query = fontdb::Query {
                families: &[fontdb::Family::Name(family)],
                weight: fontdb::Weight::NORMAL,
                stretch: fontdb::Stretch::Normal,
                style: fontdb::Style::Normal,
            };
            if let Some(id) = db.query(&query) {
                push_unique_face(&mut faces, id);
            }
        }
        for family in [fontdb::Family::SansSerif, fontdb::Family::Serif] {
            let query = fontdb::Query {
                families: &[family],
                weight: fontdb::Weight::NORMAL,
                stretch: fontdb::Stretch::Normal,
                style: fontdb::Style::Normal,
            };
            if let Some(id) = db.query(&query) {
                push_unique_face(&mut faces, id);
            }
        }
        let face_infos = db.faces().map(|face| face.id).collect::<Vec<_>>();
        for id in face_infos {
            push_unique_face(&mut faces, id);
        }
        Self {
            db,
            faces,
            loaded: HashMap::new(),
            face_ids: HashMap::new(),
            resident: VecDeque::new(),
            resident_cap: MAX_RESIDENT_FALLBACK_FACES,
            char_cache: HashMap::new(),
            next_font_id,
        }
    }

    fn resolve(&mut self, ch: char) -> Option<DanmakuFontFace> {
        if let Some(cached) = self.char_cache.get(&ch).copied() {
            let font_id = cached?;
            if let Some(face) = self.loaded_font_by_id(font_id) {
                self.touch_resident(font_id);
                return Some(face);
            }
            // The cached face was evicted; resolve from scratch so the same
            // face (and id) becomes resident again.
        }
        let faces = self.faces.clone();
        for face_id in faces {
            if !self.face_covers(face_id, ch) {
                continue;
            }
            let Some(face) = self.load_face(face_id) else {
                continue;
            };
            if face.has_glyph(ch) {
                self.char_cache.insert(ch, Some(face.id));
                return Some(face);
            }
        }
        self.char_cache.insert(ch, None);
        None
    }

    // Borrowed cmap lookup over fontdb's memory-mapped face data: faces that
    // do not cover `ch` are skipped without copying the font file. The
    // whitespace shortcut mirrors DanmakuFontFace::has_glyph.
    fn face_covers(&self, id: fontdb::ID, ch: char) -> bool {
        ch.is_whitespace()
            || self
                .db
                .with_face_data(id, |data, face_index| {
                    FontRef::try_from_slice_and_index(data, face_index)
                        .map(|face| face.glyph_id(ch).0 != 0)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
    }

    fn loaded_font_by_id(&self, font_id: u32) -> Option<DanmakuFontFace> {
        self.loaded
            .values()
            .filter_map(|face| face.as_ref())
            .find(|face| face.id == font_id)
            .cloned()
    }

    fn load_face(&mut self, id: fontdb::ID) -> Option<DanmakuFontFace> {
        if let Some(face) = self.loaded.get(&id).cloned() {
            if let Some(face) = &face {
                self.touch_resident(face.id);
            }
            return face;
        }
        let font = self
            .db
            .with_face_data(id, |data, face_index| {
                FontVec::try_from_vec_and_index(data.to_vec(), face_index)
                    .map(FontArc::new)
                    .ok()
            })
            .flatten();
        let face = font.map(|font| {
            let local_id = self.stable_face_id(id);
            DanmakuFontFace::new(local_id, font)
        });
        // A zero cap disables caching entirely: the face is handed out without
        // ever entering the resident set, so the loaded/resident invariant
        // (every cached face is resident) still holds.
        if self.resident_cap > 0 {
            if let Some(face) = &face {
                self.evict_over_cap();
                self.resident.push_back(face.id);
            }
            self.loaded.insert(id, face.clone());
        }
        face
    }

    fn stable_face_id(&mut self, id: fontdb::ID) -> u32 {
        if let Some(local_id) = self.face_ids.get(&id) {
            return *local_id;
        }
        let local_id = self.next_font_id;
        self.next_font_id = self.next_font_id.saturating_add(1);
        self.face_ids.insert(id, local_id);
        local_id
    }

    fn evict_over_cap(&mut self) {
        while self.resident.len() >= self.resident_cap {
            let Some(evicted_id) = self.resident.pop_front() else {
                return;
            };
            let evicted_key = self
                .loaded
                .iter()
                .find(|(_, face)| face.as_ref().is_some_and(|f| f.id == evicted_id))
                .map(|(key, _)| *key);
            if let Some(key) = evicted_key {
                self.loaded.remove(&key);
            }
        }
    }

    fn touch_resident(&mut self, font_id: u32) {
        if let Some(index) = self.resident.iter().position(|id| *id == font_id) {
            self.resident.remove(index);
            self.resident.push_back(font_id);
        }
    }
}

fn push_unique_face(faces: &mut Vec<fontdb::ID>, id: fontdb::ID) {
    if !faces.contains(&id) {
        faces.push(id);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TextLayoutCacheKey {
    text: Arc<str>,
    font_size_milli: u32,
    outline_radius: u16,
}

impl TextLayoutCacheKey {
    fn new(text: Arc<str>, font_size: f32, outline_width: f32) -> Self {
        Self {
            text,
            font_size_milli: (sanitize_f32(font_size, DEFAULT_NATIVE_FONT_SIZE).max(1.0) * 1000.0)
                .round() as u32,
            outline_radius: raster_radius(outline_width),
        }
    }

    fn font_size(&self) -> f32 {
        self.font_size_milli as f32 / 1000.0
    }

    fn outline_width(&self) -> f32 {
        f32::from(self.outline_radius)
    }
}

#[derive(Debug, Clone)]
pub struct DanmakuTextRasterizer {
    shaper: TextShaper,
    supersampling: bool,
    primary_font: Option<DanmakuFontFace>,
    selected_fonts: Arc<[DanmakuFontFace]>,
    selected_before_primary: bool,
    fallback_fonts: Arc<Mutex<SystemFontFallback>>,
    glyph_cache: Arc<Mutex<HashMap<GlyphCacheKey, Arc<RasterizedGlyph>>>>,
    text_layout_cache: Arc<Mutex<HashMap<TextLayoutCacheKey, Arc<PreparedTextLayout>>>>,
    glyph_atlas: Arc<Mutex<PersistentGlyphAtlas>>,
}

impl Default for DanmakuTextRasterizer {
    fn default() -> Self {
        Self::new(TextShaper::default())
    }
}

impl DanmakuTextRasterizer {
    pub fn new(shaper: TextShaper) -> Self {
        Self {
            shaper,
            supersampling: true,
            primary_font: load_default_font().map(|font| DanmakuFontFace::new(0, font)),
            selected_fonts: Arc::from([]),
            selected_before_primary: false,
            fallback_fonts: Arc::new(Mutex::new(SystemFontFallback::new(1))),
            glyph_cache: Arc::new(Mutex::new(HashMap::new())),
            text_layout_cache: Arc::new(Mutex::new(HashMap::new())),
            glyph_atlas: Arc::new(Mutex::new(PersistentGlyphAtlas::new())),
        }
    }

    pub fn for_config(config: &DanmakuLayoutConfig) -> Self {
        Self::for_config_and_selection(config, &DanmakuFontSelection::default())
    }

    pub fn for_config_and_selection(
        config: &DanmakuLayoutConfig,
        selection: &DanmakuFontSelection,
    ) -> Self {
        let shaper = TextShaper::default();
        let primary_font = load_configured_font(
            &config.custom_font_family,
            &config.custom_font_file_path,
            config.custom_font_face_index,
        )
        .map(|font| DanmakuFontFace::new(0, font));
        let first_selected_id = u32::from(primary_font.is_some());
        let selected_fonts = load_selected_fonts(&selection.fonts, first_selected_id);
        let next_font_id = first_selected_id.saturating_add(selected_fonts.len() as u32);
        Self {
            shaper,
            supersampling: config.supersampling,
            primary_font,
            selected_fonts: Arc::from(selected_fonts),
            selected_before_primary: config.custom_font_family.is_empty()
                && config.custom_font_file_path.is_empty(),
            fallback_fonts: Arc::new(Mutex::new(SystemFontFallback::new(next_font_id))),
            glyph_cache: Arc::new(Mutex::new(HashMap::new())),
            text_layout_cache: Arc::new(Mutex::new(HashMap::new())),
            glyph_atlas: Arc::new(Mutex::new(PersistentGlyphAtlas::new())),
        }
    }

    pub fn measure(&self, text: &str, font_size: f32) -> TextMeasure {
        if self.primary_font.is_none() {
            let metrics = self.shaper.measure(text, font_size);
            return TextMeasure {
                width: metrics.width.max(1.0),
                height: metrics.height.max(1.0),
                ascent: metrics.ascent,
                descent: metrics.descent,
            };
        }
        self.measure_with_fallback(text, font_size)
    }

    fn recycle_caches_if_needed(&mut self) {
        let over_budget = {
            let atlas = self.glyph_atlas.lock().expect("danmaku glyph atlas lock");
            atlas.height >= GLYPH_ATLAS_SOFT_HEIGHT || atlas.packed.len() >= GLYPH_CACHE_SOFT_LIMIT
        } || self
            .text_layout_cache
            .lock()
            .expect("danmaku text layout cache lock")
            .len()
            >= TEXT_CACHE_SOFT_LIMIT;
        if over_budget {
            // Existing prepared frames own the old caches through their cloned
            // rasterizer. Do not mutate them while the replacement is built.
            self.glyph_cache = Arc::new(Mutex::new(HashMap::new()));
            self.text_layout_cache = Arc::new(Mutex::new(HashMap::new()));
            self.glyph_atlas = Arc::new(Mutex::new(PersistentGlyphAtlas::new()));
        }
    }

    fn measure_with_fallback(&self, text: &str, font_size: f32) -> TextMeasure {
        let mut width = 0.0f32;
        let mut max_ascent = 0.0f32;
        let mut max_descent = 0.0f32;
        let mut max_line_gap = 0.0f32;
        let mut previous: Option<(u32, GlyphId)> = None;
        for ch in text.chars() {
            let Some(face) = self.resolve_font(ch) else {
                let metrics = self.shaper.measure(&ch.to_string(), font_size);
                width += metrics.width.max(font_size * 0.5);
                max_ascent = max_ascent.max(metrics.ascent);
                max_descent = max_descent.max(metrics.descent);
                previous = None;
                continue;
            };
            let scale = px_scale_for_em(face.font.as_ref(), font_size);
            let scaled = face.font.as_scaled(scale);
            let glyph_id = scaled.glyph_id(ch);
            if let Some((previous_font_id, previous_glyph_id)) = previous {
                if previous_font_id == face.id {
                    width += scaled.kern(previous_glyph_id, glyph_id);
                }
            }
            width += scaled.h_advance(glyph_id).max(0.0);
            max_ascent = max_ascent.max(scaled.ascent());
            max_descent = max_descent.max(scaled.descent().abs());
            max_line_gap = max_line_gap.max(scaled.line_gap());
            previous = Some((face.id, glyph_id));
        }
        let ascent = max_ascent.max(font_size * 0.8);
        let descent = max_descent.max(font_size * 0.2);
        let height = (ascent + descent + max_line_gap).max(font_size * 1.2);
        TextMeasure {
            width: width.max(1.0),
            height,
            ascent,
            descent,
        }
    }

    fn prepare_text_layout_with_metrics(
        &self,
        text: Arc<str>,
        font_size: f32,
        outline_width: f32,
        metrics: TextMeasure,
    ) -> Arc<PreparedTextLayout> {
        let key = TextLayoutCacheKey::new(text, font_size, outline_width);
        if let Some(layout) = self
            .text_layout_cache
            .lock()
            .expect("danmaku text layout cache lock")
            .get(&key)
            .cloned()
        {
            return layout;
        }
        let layout = Arc::new(self.build_text_layout(&key, metrics));
        self.text_layout_cache
            .lock()
            .expect("danmaku text layout cache lock")
            .insert(key, Arc::clone(&layout));
        layout
    }

    fn build_text_layout(
        &self,
        key: &TextLayoutCacheKey,
        metrics: TextMeasure,
    ) -> PreparedTextLayout {
        let font_size = key.font_size();
        let outline_width = key.outline_width();
        let mut glyphs = Vec::new();
        let mut pen_x = 0.0f32;
        let mut previous: Option<(u32, GlyphId)> = None;
        for ch in key.text.chars() {
            let face = self.resolve_font(ch);
            let glyph_id = face.as_ref().map(|face| face.glyph_id(ch));
            if let (Some(face), Some((previous_font_id, previous_glyph_id)), Some(current)) =
                (face.as_ref(), previous, glyph_id)
            {
                if previous_font_id == face.id {
                    pen_x += face
                        .font
                        .as_scaled(px_scale_for_em(face.font.as_ref(), font_size))
                        .kern(previous_glyph_id, current);
                }
            }
            let glyph_key = GlyphCacheKey::new(
                face.as_ref().map(|face| face.id).unwrap_or(u32::MAX),
                ch,
                font_size,
                outline_width,
                self.supersampling,
            );
            let glyph = self.cached_glyph_by_key(glyph_key);
            let packed = if glyph.has_bitmap() {
                self.pack_glyph(&glyph)
            } else {
                PackedGlyph::default()
            };
            let advance = glyph.advance;
            glyphs.push(PreparedGlyphPlacement {
                glyph,
                packed,
                pen_x,
            });
            pen_x += advance;
            previous = face
                .as_ref()
                .and_then(|face| glyph_id.map(|glyph_id| (face.id, glyph_id)));
        }
        PreparedTextLayout { metrics, glyphs }
    }

    fn pack_glyph(&self, glyph: &RasterizedGlyph) -> PackedGlyph {
        self.glyph_atlas
            .lock()
            .expect("danmaku glyph atlas lock")
            .pack(glyph)
    }

    fn resolve_font(&self, ch: char) -> Option<DanmakuFontFace> {
        if self.selected_before_primary {
            if let Some(font) = self.selected_fonts.iter().find(|font| font.has_glyph(ch)) {
                return Some(font.clone());
            }
        }
        if let Some(font) = &self.primary_font {
            if font.has_glyph(ch) {
                return Some(font.clone());
            }
        }
        if !self.selected_before_primary {
            if let Some(font) = self.selected_fonts.iter().find(|font| font.has_glyph(ch)) {
                return Some(font.clone());
            }
        }
        self.fallback_fonts
            .lock()
            .expect("danmaku system font fallback lock")
            .resolve(ch)
    }

    pub fn render_plan(&self, layout: &DanmakuFrameLayout) -> DanmakuRenderPlan {
        if layout.items.is_empty() {
            return DanmakuRenderPlan::empty(layout.media_time, layout.generation, layout.viewport);
        }
        let atlas = self
            .glyph_atlas
            .lock()
            .expect("danmaku glyph atlas lock")
            .snapshot();
        self.render_plan_scaled(layout, 1.0, atlas)
    }

    /// Preview a new font size using existing masks until async shaping and
    /// lane placement finish. UVs and the glyph atlas are unchanged.
    pub(crate) fn render_plan_scaled(
        &self,
        layout: &DanmakuFrameLayout,
        scale: f32,
        atlas: Option<Arc<DanmakuGlyphAtlas>>,
    ) -> DanmakuRenderPlan {
        let frame_stats = DanmakuFrameStats::from_layout(layout);
        if layout.items.is_empty() {
            return DanmakuRenderPlan::empty(layout.media_time, layout.generation, layout.viewport);
        }
        let Some(atlas_snapshot) = atlas else {
            return DanmakuRenderPlan::empty(layout.media_time, layout.generation, layout.viewport);
        };
        let atlas_w = atlas_snapshot.width.max(1) as f32;
        let atlas_h = atlas_snapshot.height.max(1) as f32;
        let capacity = layout
            .items
            .iter()
            .map(|item| item.text_layout.glyphs.len())
            .sum();
        let mut items = Vec::with_capacity(capacity);
        for item in &layout.items {
            let color = item.color.rgba(item.opacity);
            if color[3] <= 0.0 {
                continue;
            }
            self.append_item_glyphs(item, color, atlas_w, atlas_h, scale, &mut items);
        }
        DanmakuRenderPlan {
            media_time: layout.media_time,
            generation: layout.generation,
            viewport: layout.viewport,
            atlas: Some(atlas_snapshot),
            items,
            frame_stats,
        }
    }

    fn append_item_glyphs(
        &self,
        item: &DanmakuPlacedItem,
        color: [f32; 4],
        atlas_w: f32,
        atlas_h: f32,
        scale: f32,
        items: &mut Vec<DanmakuGlyphInstance>,
    ) {
        let x = match item.mode {
            DanmakuMode::Top | DanmakuMode::Bottom => item.x + item.width * (1.0 - scale) * 0.5,
            _ => item.x,
        };
        let y = if item.mode == DanmakuMode::Bottom {
            item.y + item.height * (1.0 - scale)
        } else {
            item.y
        };
        let baseline = y + item.text_layout.metrics.ascent * scale;
        let outline = item.color.outline_rgba(item.opacity);
        for placement in &item.text_layout.glyphs {
            let glyph = &placement.glyph;
            if glyph.has_bitmap() {
                let packed = placement.packed;
                items.push(DanmakuGlyphInstance {
                    item_id: item.item_id,
                    rect: [
                        x + (placement.pen_x + glyph.offset_x) * scale,
                        baseline + glyph.offset_y * scale,
                        glyph.width as f32 / glyph.key.raster_scale() * scale,
                        glyph.height as f32 / glyph.key.raster_scale() * scale,
                    ],
                    tex_rect: [
                        packed.x as f32 / atlas_w,
                        packed.y as f32 / atlas_h,
                        packed.width as f32 / atlas_w,
                        packed.height as f32 / atlas_h,
                    ],
                    color_rgba: color,
                    outline_rgba: outline,
                    shadow_rgba: [0.0, 0.0, 0.0, (color[3] * item.shadow_alpha).min(1.0)],
                    shadow_offset: item.shadow_offset,
                });
            }
        }
    }

    fn cached_glyphs<I>(&self, keys: I) -> Vec<Arc<RasterizedGlyph>>
    where
        I: IntoIterator<Item = GlyphCacheKey>,
    {
        let keys = keys.into_iter().collect::<Vec<_>>();
        if keys.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::with_capacity(keys.len());
        let mut missing = Vec::new();
        {
            let cache = self.glyph_cache.lock().expect("danmaku glyph cache lock");
            for key in keys {
                if let Some(glyph) = cache.get(&key).cloned() {
                    results.push(glyph);
                } else {
                    results.push(Arc::new(RasterizedGlyph::placeholder(key)));
                    missing.push((results.len() - 1, key));
                }
            }
        }
        if missing.is_empty() {
            return results;
        }
        for (index, key) in missing {
            let glyph = Arc::new(self.rasterize_glyph(key));
            let cached = self
                .glyph_cache
                .lock()
                .expect("danmaku glyph cache lock")
                .entry(key)
                .or_insert_with(|| Arc::clone(&glyph))
                .clone();
            results[index] = cached;
        }
        results
    }

    fn cached_glyph_by_key(&self, key: GlyphCacheKey) -> Arc<RasterizedGlyph> {
        self.cached_glyphs([key])
            .into_iter()
            .next()
            .expect("one glyph returned")
    }

    fn rasterize_glyph(&self, key: GlyphCacheKey) -> RasterizedGlyph {
        if let Some(font) = self.resolve_font(key.ch) {
            if font.id == key.font_id {
                return rasterize_font_glyph(&font.font, key);
            }
        }
        rasterize_fallback_glyph(&self.shaper, key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphCacheKey {
    font_id: u32,
    ch: char,
    font_size_milli: u32,
    outline_radius: u16,
    raster_scale: u8,
}

impl GlyphCacheKey {
    fn new(
        font_id: u32,
        ch: char,
        font_size: f32,
        outline_width: f32,
        supersampling: bool,
    ) -> Self {
        Self {
            font_id,
            ch,
            font_size_milli: (sanitize_f32(font_size, DEFAULT_NATIVE_FONT_SIZE).max(1.0) * 1000.0)
                .round() as u32,
            outline_radius: raster_radius(outline_width),
            raster_scale: if supersampling && font_size <= MAX_SUPERSAMPLED_FONT_SIZE {
                2
            } else {
                1
            },
        }
    }

    fn font_size(self) -> f32 {
        self.font_size_milli as f32 / 1000.0
    }

    fn raster_scale(self) -> f32 {
        f32::from(self.raster_scale)
    }

    fn raster_outline_radius(self) -> i32 {
        i32::from(self.outline_radius) * i32::from(self.raster_scale)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RasterizedGlyph {
    key: GlyphCacheKey,
    width: u32,
    height: u32,
    stride: usize,
    fill_alpha: Vec<u8>,
    outline_alpha: Vec<u8>,
    offset_x: f32,
    offset_y: f32,
    advance: f32,
}

impl RasterizedGlyph {
    fn placeholder(key: GlyphCacheKey) -> Self {
        Self::empty(key, 0.0)
    }

    fn empty(key: GlyphCacheKey, advance: f32) -> Self {
        Self {
            key,
            width: 0,
            height: 0,
            stride: 0,
            fill_alpha: Vec::new(),
            outline_alpha: Vec::new(),
            offset_x: 0.0,
            offset_y: 0.0,
            advance,
        }
    }

    fn has_bitmap(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.stride >= self.width as usize
            && self.fill_alpha.len() >= self.required_len()
            && self.outline_alpha.len() >= self.required_len()
    }

    fn required_len(&self) -> usize {
        self.stride.saturating_mul(self.height as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PackedGlyph {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Debug)]
struct PersistentGlyphAtlas {
    width: u32,
    height: u32,
    stride: usize,
    cursor_x: u32,
    cursor_y: u32,
    row_height: u32,
    fill_alpha: Vec<u8>,
    outline_alpha: Vec<u8>,
    packed: HashMap<GlyphCacheKey, PackedGlyph>,
    version: u64,
    dirty: bool,
    dirty_bounds: Option<[u32; 4]>,
    snapshot: Option<Arc<DanmakuGlyphAtlas>>,
}

impl PersistentGlyphAtlas {
    fn new() -> Self {
        let width = GLYPH_ATLAS_WIDTH;
        let height = GLYPH_ATLAS_INITIAL_HEIGHT;
        let stride = width as usize;
        Self {
            width,
            height,
            stride,
            cursor_x: 0,
            cursor_y: 0,
            row_height: 0,
            fill_alpha: vec![0; stride * height as usize],
            outline_alpha: vec![0; stride * height as usize],
            packed: HashMap::new(),
            version: NEXT_GLYPH_ATLAS_VERSION.fetch_add(1, Ordering::Relaxed),
            dirty: true,
            dirty_bounds: None,
            snapshot: None,
        }
    }

    fn pack(&mut self, glyph: &RasterizedGlyph) -> PackedGlyph {
        if let Some(packed) = self.packed.get(&glyph.key).copied() {
            return packed;
        }
        let glyph_width = glyph.width.min(self.width);
        let glyph_height = glyph.height;
        if self.cursor_x + glyph_width > self.width {
            self.cursor_x = 0;
            self.cursor_y += self.row_height;
            self.row_height = 0;
        }
        self.ensure_height(self.cursor_y + glyph_height);
        let packed = PackedGlyph {
            x: self.cursor_x,
            y: self.cursor_y,
            width: glyph_width,
            height: glyph_height,
        };
        for row in 0..glyph_height as usize {
            let src = row * glyph.stride;
            let dst = (packed.y as usize + row) * self.stride + packed.x as usize;
            let len = glyph_width as usize;
            self.fill_alpha[dst..dst + len].copy_from_slice(&glyph.fill_alpha[src..src + len]);
            self.outline_alpha[dst..dst + len]
                .copy_from_slice(&glyph.outline_alpha[src..src + len]);
        }
        self.cursor_x += glyph_width;
        self.row_height = self.row_height.max(glyph_height);
        self.packed.insert(glyph.key, packed);
        self.version = NEXT_GLYPH_ATLAS_VERSION.fetch_add(1, Ordering::Relaxed);
        self.dirty = true;
        if packed.width > 0 && packed.height > 0 {
            let right = packed.x.saturating_add(packed.width);
            let bottom = packed.y.saturating_add(packed.height);
            self.dirty_bounds = Some(match self.dirty_bounds {
                Some([left, top, old_right, old_bottom]) => [
                    left.min(packed.x),
                    top.min(packed.y),
                    old_right.max(right),
                    old_bottom.max(bottom),
                ],
                None => [packed.x, packed.y, right, bottom],
            });
        }
        packed
    }

    fn ensure_height(&mut self, required: u32) {
        if required <= self.height {
            return;
        }
        while required > self.height {
            self.height *= 2;
        }
        let new_len = self.stride * self.height as usize;
        self.fill_alpha.resize(new_len, 0);
        self.outline_alpha.resize(new_len, 0);
        self.dirty = true;
    }

    fn snapshot(&mut self) -> Option<Arc<DanmakuGlyphAtlas>> {
        if self.packed.is_empty() {
            return None;
        }
        if self.dirty || self.snapshot.is_none() {
            let update = self.snapshot.as_ref().and_then(|snapshot| {
                self.dirty_bounds
                    .map(|[left, top, right, bottom]| DanmakuAtlasUpdate {
                        from_version: snapshot.version,
                        x: left,
                        y: top,
                        width: right.saturating_sub(left),
                        height: bottom.saturating_sub(top),
                    })
            });
            self.snapshot = Some(Arc::new(DanmakuGlyphAtlas {
                width: self.width,
                height: self.height,
                stride: self.stride,
                fill_alpha: self.fill_alpha.clone(),
                outline_alpha: self.outline_alpha.clone(),
                version: self.version,
                update,
            }));
            self.dirty = false;
            self.dirty_bounds = None;
        }
        self.snapshot.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextMeasure {
    pub width: f32,
    pub height: f32,
    pub ascent: f32,
    pub descent: f32,
}

#[derive(Debug, Clone)]
pub struct DfmLayoutEngine {
    timeline: DanmakuTimeline,
    config: DanmakuLayoutConfig,
    rasterizer: DanmakuTextRasterizer,
    font_selection: DanmakuFontSelection,
    prepared: Option<DfmPreparedLayout>,
    stable_tracks: HashMap<u64, usize>,
    rejected_items: BTreeSet<u64>,
    stable_viewport: Option<DanmakuViewport>,
}

impl DfmLayoutEngine {
    pub fn new(timeline: DanmakuTimeline, config: DanmakuLayoutConfig) -> Self {
        let config = config.sanitized();
        let rasterizer = DanmakuTextRasterizer::for_config(&config);
        Self {
            timeline,
            config,
            rasterizer,
            font_selection: DanmakuFontSelection::default(),
            prepared: None,
            stable_tracks: HashMap::new(),
            rejected_items: BTreeSet::new(),
            stable_viewport: None,
        }
    }

    pub fn set_timeline(&mut self, timeline: DanmakuTimeline) {
        self.timeline = timeline;
        self.prepared = None;
        self.invalidate_placement_history();
    }

    pub fn sync_timeline(&mut self, timeline: &DanmakuTimeline) {
        if self.timeline != *timeline {
            self.timeline = timeline.clone();
            self.prepared = None;
        }
    }

    pub fn clear_timeline(&mut self) {
        self.timeline = DanmakuTimeline::default();
        self.prepared = None;
        self.invalidate_placement_history();
    }

    pub fn set_config(&mut self, config: DanmakuLayoutConfig) -> bool {
        self.apply_config(config) != DanmakuConfigChange::Unchanged
    }

    pub(crate) fn apply_config(&mut self, config: DanmakuLayoutConfig) -> DanmakuConfigChange {
        let config = config.sanitized();
        if self.config == config {
            return DanmakuConfigChange::Unchanged;
        }
        // Hiding can reuse the prepared geometry and suppress it immediately.
        // Showing cannot assume that geometry still exists because planner
        // windows prepared while disabled intentionally contain no items.
        let re_enabling = !self.config.enabled && config.enabled;
        let layout_changed = re_enabling || !self.config.layout_equivalent(&config);
        let font_changed = self.config.custom_font_family != config.custom_font_family
            || self.config.custom_font_file_path != config.custom_font_file_path
            || self.config.custom_font_face_index != config.custom_font_face_index
            || self.config.supersampling != config.supersampling;
        self.config = config;
        if font_changed {
            self.rasterizer =
                DanmakuTextRasterizer::for_config_and_selection(&self.config, &self.font_selection);
        }
        if layout_changed {
            self.prepared = None;
            self.rejected_items.clear();
            // Track assignments are preferences, not hard constraints. Keeping
            // them across font-size and other layout changes lets overlapping
            // items stay in the same logical lane; the retainer will reject a
            // preference when the new geometry no longer fits.
            DanmakuConfigChange::Layout
        } else {
            if let Some(prepared) = &mut self.prepared {
                prepared.apply_paint_config(&self.config);
            }
            DanmakuConfigChange::PaintOnly
        }
    }

    pub fn set_font_selection(&mut self, selection: DanmakuFontSelection) -> bool {
        if self.font_selection.generation == selection.generation {
            return false;
        }
        self.rasterizer = DanmakuTextRasterizer::for_config_and_selection(&self.config, &selection);
        self.font_selection = selection;
        self.prepared = None;
        self.invalidate_placement_history();
        true
    }

    pub fn config(&self) -> &DanmakuLayoutConfig {
        &self.config
    }

    pub(crate) fn rasterizer_clone(&self) -> DanmakuTextRasterizer {
        self.rasterizer.clone()
    }

    pub(crate) fn set_config_with_rasterizer(
        &mut self,
        config: DanmakuLayoutConfig,
        rasterizer: DanmakuTextRasterizer,
    ) {
        if self.apply_config(config) == DanmakuConfigChange::Layout {
            // The async planner must pack glyphs into the same atlas that the
            // presenter later snapshots. This is especially important after a
            // custom-font change creates a fresh rasterizer generation.
            self.rasterizer = rasterizer;
        }
    }

    pub fn prepare(&mut self, viewport: DanmakuViewport, _generation: u64) -> DfmPreparedLayout {
        if self.stable_viewport != Some(viewport) {
            self.invalidate_placement_history();
            self.stable_viewport = Some(viewport);
        }
        let config = self.config.sanitized();
        let planned_ids = self
            .timeline
            .items()
            .iter()
            .filter(|item| item.mode != DanmakuMode::Special)
            .filter(|item| !self.rejected_items.contains(&item.id))
            .map(|item| item.id)
            .collect::<BTreeSet<_>>();
        let prepared = prepare_layout(
            &self.timeline,
            viewport,
            &config,
            &self.rasterizer,
            &self.stable_tracks,
            &self.rejected_items,
        );
        let active_ids = prepared
            .items()
            .iter()
            .map(|item| item.id)
            .collect::<BTreeSet<_>>();
        if config.enabled {
            // A sliding window may forget older track occupancy, but it must not
            // revise an item's original admission decision. Otherwise an item
            // dropped at birth can reappear halfway across the screen.
            self.rejected_items
                .extend(planned_ids.difference(&active_ids).copied());
        }
        self.stable_tracks
            .retain(|item_id, _| active_ids.contains(item_id));
        for item in prepared.items() {
            self.stable_tracks.insert(item.id, item.track_index);
        }
        self.prepared = Some(prepared.clone());
        prepared
    }

    pub fn invalidate_placement_history(&mut self) {
        self.stable_tracks.clear();
        self.rejected_items.clear();
        self.stable_viewport = None;
    }

    pub fn frame_layout(
        &mut self,
        media_time: Duration,
        viewport: DanmakuViewport,
        generation: u64,
    ) -> DanmakuFrameLayout {
        let generation = generation.max(DEFAULT_GENERATION);
        let needs_prepare = self.prepared.as_ref().is_none_or(|prepared| {
            prepared.viewport != viewport || prepared.config != self.config.sanitized()
        });
        if needs_prepare {
            self.prepare(viewport, generation);
        }
        self.prepared
            .as_ref()
            .expect("prepared layout exists")
            .frame_layout(media_time, generation)
    }

    pub fn render_prepared_plan(
        &self,
        prepared: &DfmPreparedLayout,
        media_time: Duration,
        generation: u64,
    ) -> DanmakuRenderPlan {
        let layout = prepared.frame_layout(media_time, generation);
        self.rasterizer.render_plan(&layout)
    }

    pub fn render_plan(
        &mut self,
        media_time: Duration,
        viewport: DanmakuViewport,
        generation: u64,
    ) -> DanmakuRenderPlan {
        let layout = self.frame_layout(media_time, viewport, generation);
        self.rasterizer.render_plan(&layout)
    }

    /// The presenter pairs each prepared result with its rasterizer clone.
    /// Recycle only there, keeping render_prepared_plan's public API stable.
    pub(crate) fn recycle_raster_caches_if_needed(&mut self) {
        self.rasterizer.recycle_caches_if_needed();
    }
}

fn prepare_layout(
    timeline: &DanmakuTimeline,
    viewport: DanmakuViewport,
    config: &DanmakuLayoutConfig,
    rasterizer: &DanmakuTextRasterizer,
    stable_tracks: &HashMap<u64, usize>,
    rejected_items: &BTreeSet<u64>,
) -> DfmPreparedLayout {
    if timeline.is_empty() || !config.enabled {
        let dfm_layout = dfm::PreparedLayout {
            width: viewport.width as f64,
            height: viewport.height as f64,
            scroll_duration_seconds: config.scroll_duration_seconds.max(1.0) as f64,
            static_duration_seconds: DEFAULT_STATIC_DURATION.as_secs_f64(),
            items: Vec::new(),
            item_times: Vec::new(),
            track_count: 0,
        };
        return DfmPreparedLayout {
            atlas: None,
            viewport,
            config: config.clone(),
            dfm_layout,
            stats: DanmakuPreparedStats::default(),
            scroll_duration: DEFAULT_SCROLL_DURATION,
            static_duration: DEFAULT_STATIC_DURATION,
            item_times: Vec::new(),
            items: Vec::new(),
        };
    }
    let scroll_duration = scroll_duration_for_viewport(config, viewport);
    let base_font_size = effective_config_font_size(config.font_size, viewport.scale_factor);
    let source_count = timeline.items().len();
    let mut source_items = Vec::new();
    for source in timeline.items() {
        if source.mode == DanmakuMode::Special || rejected_items.contains(&source.id) {
            continue;
        }
        let font_size =
            effective_font_size(source.font_size, config.font_size, viewport.scale_factor);
        let measure = rasterizer.measure(&source.text, font_size);
        source_items.push(dfm::PrepareItem {
            source_id: source.id,
            time_seconds: source.pts.as_secs_f64(),
            text: source.text.clone(),
            type_code: mode_to_dfm_type_code(source.mode),
            color_argb: color_to_argb(source.color),
            opacity: source.opacity,
            is_me: source.is_self,
            font_size,
            paint_width: measure.width as f64,
            paint_height: measure.height.max(font_size * 1.2) as f64,
        });
    }
    let supported_count = source_items.len();

    let dfm_layout = dfm::prepare_layout(dfm::PrepareRequest {
        items: source_items,
        width: viewport.width as f64,
        height: viewport.height as f64,
        font_size: base_font_size as f64,
        display_area: config.display_area as f64,
        scroll_duration_seconds: scroll_duration.as_secs_f64(),
        allow_stacking: config.allow_stacking,
        allow_scroll_overwrite: config.allow_scroll_overwrite,
        merge_danmaku: config.merge_duplicates,
        max_quantity: config.max_quantity,
        max_lines_per_type: config.max_lines_per_mode,
        track_gap_ratio: config.track_gap_ratio as f64,
        outline_width: config.outline_width as f64,
        preferred_tracks: stable_tracks,
        block_words: config.block_words.clone(),
        block_top: config.block_top,
        block_bottom: config.block_bottom,
        block_scroll: config.block_scroll,
    })
    .unwrap_or_else(|_| dfm::PreparedLayout {
        width: viewport.width as f64,
        height: viewport.height as f64,
        scroll_duration_seconds: scroll_duration.as_secs_f64(),
        static_duration_seconds: DEFAULT_STATIC_DURATION.as_secs_f64(),
        items: Vec::new(),
        item_times: Vec::new(),
        track_count: 0,
    });

    let prepared = dfm_layout
        .items
        .iter()
        .map(|item| {
            let text: Arc<str> = Arc::from(item.text.as_str());
            let font_size = item.font_size as f32;
            let metrics = rasterizer.measure(&item.text, font_size);
            let outline_width = resolve_width_px(font_size, config.outline_width);
            let text_layout = rasterizer.prepare_text_layout_with_metrics(
                Arc::clone(&text),
                font_size,
                outline_width,
                metrics,
            );
            PreparedDanmakuItem {
                id: item.source_id,
                time: Duration::from_secs_f64(item.time_seconds.max(0.0)),
                text,
                mode: dfm_type_code_to_mode(item.type_code),
                color: DanmakuColor::from_argb(item.color_argb as u32),
                opacity: item.opacity,
                font_size,
                width: item.width as f32,
                height: item.height as f32,
                y: item.y_position as f32,
                track_index: item.track_index.max(0) as usize,
                scroll_speed: item.scroll_speed as f32,
                duration: Duration::from_secs_f64(item.duration_seconds.max(0.0)),
                duplicate_count: item.duplicate_count,
                text_layout,
            }
        })
        .collect::<Vec<_>>();

    let item_times = prepared
        .iter()
        .map(|item| item.time.as_secs_f64())
        .collect();
    let stats = DanmakuPreparedStats::from_items(
        source_count,
        supported_count,
        &prepared,
        viewport,
        config,
        dfm_layout.track_count.max(0) as usize,
    );
    DfmPreparedLayout {
        atlas: rasterizer
            .glyph_atlas
            .lock()
            .expect("danmaku glyph atlas lock")
            .snapshot(),
        viewport,
        config: config.clone(),
        dfm_layout,
        stats,
        scroll_duration,
        static_duration: DEFAULT_STATIC_DURATION,
        item_times,
        items: prepared,
    }
}

pub fn parse_bilibili_xml(input: &str) -> Result<Vec<DanmakuItem>> {
    let mut items = Vec::new();
    for (index, segment) in input.split("<d ").skip(1).enumerate() {
        let Some(p_start) = segment.find("p=\"") else {
            continue;
        };
        let p_rest = &segment[p_start + 3..];
        let Some(p_end) = p_rest.find('"') else {
            continue;
        };
        let fields = p_rest[..p_end].split(',').collect::<Vec<_>>();
        let Some(text_start) = segment.find('>') else {
            continue;
        };
        let Some(text_end) = segment[text_start + 1..].find("</d>") else {
            continue;
        };
        let text = decode_xml_entities(&segment[text_start + 1..text_start + 1 + text_end]);
        if text.trim().is_empty() {
            continue;
        }
        let mode_code = fields
            .get(1)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(1);
        let mode = DanmakuMode::from_bilibili_mode(mode_code);
        if mode == DanmakuMode::Special {
            continue;
        }
        let Some(pts) = parse_float_field(fields.first(), "pts")
            .ok()
            .and_then(item_duration)
        else {
            continue;
        };
        items.push(DanmakuItem {
            id: fields
                .get(7)
                .and_then(|value| value.parse().ok())
                .unwrap_or(index as u64 + 1),
            pts,
            mode,
            font_size: fields
                .get(2)
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_SOURCE_FONT_SIZE),
            color: DanmakuColor::from_decimal(
                fields
                    .get(3)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0xffffff),
            ),
            opacity: 1.0,
            is_self: false,
            text,
        });
    }
    Ok(items)
}

fn should_skip_item_error(error: &DanmakuError) -> bool {
    matches!(
        error,
        DanmakuError::MissingText | DanmakuError::InvalidField(_)
    )
}

impl DanmakuColor {
    fn from_decimal(value: u32) -> Self {
        Self::rgb_u8(
            ((value >> 16) & 0xff) as u8,
            ((value >> 8) & 0xff) as u8,
            (value & 0xff) as u8,
        )
    }

    fn from_argb(value: u32) -> Self {
        Self {
            red: ((value >> 16) & 0xff) as f32 / 255.0,
            green: ((value >> 8) & 0xff) as f32 / 255.0,
            blue: (value & 0xff) as f32 / 255.0,
            alpha: ((value >> 24) & 0xff) as f32 / 255.0,
        }
    }
}

fn mode_to_dfm_type_code(mode: DanmakuMode) -> i32 {
    match mode {
        DanmakuMode::Scroll => 1,
        DanmakuMode::ScrollReverse => 6,
        DanmakuMode::Top => 5,
        DanmakuMode::Bottom => 4,
        DanmakuMode::Special => 7,
    }
}

fn dfm_type_code_to_mode(type_code: i32) -> DanmakuMode {
    DanmakuMode::from_bilibili_mode(type_code.max(0) as u32)
}

fn color_to_argb(color: DanmakuColor) -> i32 {
    let alpha = color.alpha.clamp(0.0, 1.0).mul_add(255.0, 0.0).round() as u32;
    let red = (color.red.clamp(0.0, 1.0) * 255.0).round() as u32;
    let green = (color.green.clamp(0.0, 1.0) * 255.0).round() as u32;
    let blue = (color.blue.clamp(0.0, 1.0) * 255.0).round() as u32;
    ((alpha << 24) | (red << 16) | (green << 8) | blue) as i32
}

fn item_from_json_value(value: &Value, fallback_id: u64) -> Result<DanmakuItem> {
    let object = value
        .as_object()
        .ok_or_else(|| DanmakuError::InvalidField("comment".to_string()))?;
    let text = string_field(object, &["content", "text", "c"]).ok_or(DanmakuError::MissingText)?;
    let mode = string_field(object, &["type", "mode", "y"])
        .map(|value| DanmakuMode::from_text(&value))
        .or_else(|| {
            numeric_field(object, &["type_code", "mode_code"])
                .map(|v| DanmakuMode::from_bilibili_mode(v as u32))
        })
        .unwrap_or(DanmakuMode::Scroll);
    let color = object
        .get("color")
        .or_else(|| object.get("r"))
        .and_then(parse_color_value)
        .unwrap_or(DanmakuColor::WHITE);
    let pts = item_duration(numeric_field(object, &["time", "t"]).unwrap_or(0.0))
        .ok_or_else(|| DanmakuError::InvalidField("time".to_string()))?;
    Ok(DanmakuItem {
        id: u64_field(object, &["id"]).unwrap_or(fallback_id),
        pts,
        text,
        mode,
        font_size: numeric_field(object, &["font_size", "fontSize", "size", "s"])
            .unwrap_or(DEFAULT_SOURCE_FONT_SIZE as f64) as f32,
        color,
        opacity: numeric_field(object, &["opacity", "alpha", "a"]).unwrap_or(1.0) as f32,
        is_self: bool_field(object, &["is_me", "isMe", "self", "mine"]).unwrap_or(false),
    })
}

fn string_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
}

fn numeric_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| match value {
            Value::Number(value) => value.as_f64(),
            Value::String(value) => value.parse().ok(),
            _ => None,
        })
}

fn u64_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| match value {
            Value::Number(value) => value.as_u64(),
            Value::String(value) => value.parse().ok(),
            _ => None,
        })
}

fn bool_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| match value {
            Value::Bool(value) => Some(*value),
            Value::Number(value) => Some(value.as_u64().unwrap_or(0) != 0),
            Value::String(value) => matches!(value.as_str(), "true" | "1" | "yes").then_some(true),
            _ => None,
        })
}

fn parse_color_value(value: &Value) -> Option<DanmakuColor> {
    match value {
        Value::Number(value) => value
            .as_u64()
            .map(|value| DanmakuColor::from_decimal(value as u32)),
        Value::String(value) => parse_color_string(value),
        _ => None,
    }
}

fn parse_color_string(value: &str) -> Option<DanmakuColor> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        let hex = hex.strip_prefix("0x").unwrap_or(hex);
        let rgb = u32::from_str_radix(hex, 16).ok()?;
        return Some(DanmakuColor::from_decimal(rgb));
    }
    if let Some(inner) = value.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts = inner
            .split(',')
            .map(|part| part.trim().parse::<u8>().ok())
            .collect::<Option<Vec<_>>>()?;
        if parts.len() >= 3 {
            return Some(DanmakuColor::rgb_u8(parts[0], parts[1], parts[2]));
        }
    }
    value.parse::<u32>().ok().map(DanmakuColor::from_decimal)
}

fn parse_float_field(field: Option<&&str>, name: &'static str) -> Result<f64> {
    field
        .ok_or_else(|| DanmakuError::InvalidField(name.to_string()))?
        .parse::<f64>()
        .map_err(|_| DanmakuError::InvalidField(name.to_string()))
}

/// Preserve negative-time clamping while rejecting unrepresentable durations.
fn item_duration(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() {
        return None;
    }
    Duration::try_from_secs_f64(seconds.max(0.0)).ok()
}

fn decode_xml_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

pub(crate) fn scroll_duration_for_viewport(
    config: &DanmakuLayoutConfig,
    viewport: DanmakuViewport,
) -> Duration {
    let duration_seconds = sanitize_f32(config.scroll_duration_seconds, 10.0).max(1.0);
    let speed_factor = sanitize_f32(config.scroll_speed_factor, 1.0).max(0.001);
    let logical_width = viewport.width as f32 / viewport.scale_factor.max(0.001);
    let width_scale = (logical_width / SCROLL_DURATION_REFERENCE_LOGICAL_WIDTH).clamp(
        SCROLL_DURATION_MIN_WIDTH_SCALE,
        SCROLL_DURATION_MAX_WIDTH_SCALE,
    );
    let millis = duration_seconds * width_scale * 1000.0 / speed_factor;
    Duration::from_millis(millis.max(1.0).round() as u64)
}

fn scale_offset(offset: [f32; 2], scale_factor: f32) -> [f32; 2] {
    let scale_factor = sanitize_f32(scale_factor, 1.0).max(0.001);
    [offset[0] * scale_factor, offset[1] * scale_factor]
}

fn shadow_style_alpha(style: DanmakuShadowStyle) -> f32 {
    match style {
        DanmakuShadowStyle::None => 0.0,
        DanmakuShadowStyle::Soft => 0.34,
        DanmakuShadowStyle::Medium => 0.44,
        DanmakuShadowStyle::Strong => 0.55,
    }
}

fn apply_track_offset(pts: Duration, offset_micros: i64) -> Option<Duration> {
    if offset_micros >= 0 {
        return Some(pts.saturating_add(Duration::from_micros(offset_micros as u64)));
    }
    pts.checked_sub(Duration::from_micros(offset_micros.unsigned_abs()))
}

fn sanitize_f32(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

fn load_configured_font(family: &str, file_path: &str, face_index: u32) -> Option<FontArc> {
    let file_path = file_path.trim();
    if !file_path.is_empty() {
        match load_font_from_path(Path::new(file_path), face_index) {
            Ok(font) => return Some(font),
            Err(error) => {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "danmaku_custom_font",
                        "stage": "rejected",
                        "path": file_path,
                        "faceIndex": face_index,
                        "bytes": error.bytes,
                        "maxBytes": MAX_CUSTOM_DANMAKU_FONT_BYTES,
                        "reason": error.reason,
                        "detail": error.detail,
                        "fallback": if family.trim().is_empty() { "default" } else { "family_then_default" },
                    })
                    .to_string(),
                );
            }
        }
    }

    let family = family.trim();
    if !family.is_empty() {
        if let Some(font) = load_font_family(family) {
            return Some(font);
        }
    }

    load_default_font()
}

fn load_selected_fonts(
    attachments: &[SubtitleFontAttachment],
    first_font_id: u32,
) -> Vec<DanmakuFontFace> {
    let mut fonts = Vec::new();
    for attachment in attachments {
        let mut database = fontdb::Database::new();
        database.load_font_source(fontdb::Source::Binary(Arc::new(attachment.data.clone())));
        let face_indices = database.faces().map(|face| face.index).collect::<Vec<_>>();
        for face_index in face_indices {
            if let Some(font) = load_shared_font_data(attachment.data.clone(), face_index) {
                let id = first_font_id.saturating_add(fonts.len() as u32);
                fonts.push(DanmakuFontFace::new(id, font));
            }
        }
    }
    fonts
}

#[derive(Debug, PartialEq, Eq)]
struct CustomDanmakuFontError {
    reason: &'static str,
    detail: String,
    bytes: u64,
}

fn load_font_from_path(
    path: &Path,
    face_index: u32,
) -> std::result::Result<FontArc, CustomDanmakuFontError> {
    let metadata = fs::metadata(path).map_err(|error| CustomDanmakuFontError {
        reason: "metadata_failed",
        detail: error.to_string(),
        bytes: 0,
    })?;
    let bytes = metadata.len();
    if !metadata.is_file() {
        return Err(CustomDanmakuFontError {
            reason: "not_regular_file",
            detail: "custom font path is not a regular file".to_string(),
            bytes,
        });
    }
    if bytes > MAX_CUSTOM_DANMAKU_FONT_BYTES {
        return Err(CustomDanmakuFontError {
            reason: "file_too_large",
            detail: format!(
                "custom font exceeds {} byte limit",
                MAX_CUSTOM_DANMAKU_FONT_BYTES
            ),
            bytes,
        });
    }

    let file = File::open(path).map_err(|error| CustomDanmakuFontError {
        reason: "read_failed",
        detail: error.to_string(),
        bytes,
    })?;
    let mut data = Vec::with_capacity(usize::try_from(bytes).unwrap_or(0));
    file.take(MAX_CUSTOM_DANMAKU_FONT_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|error| CustomDanmakuFontError {
            reason: "read_failed",
            detail: error.to_string(),
            bytes,
        })?;
    if data.len() as u64 > MAX_CUSTOM_DANMAKU_FONT_BYTES {
        return Err(CustomDanmakuFontError {
            reason: "file_too_large",
            detail: format!(
                "custom font exceeds {} byte limit",
                MAX_CUSTOM_DANMAKU_FONT_BYTES
            ),
            bytes: data.len() as u64,
        });
    }
    if let Some(font) = load_font_data(data.clone(), face_index) {
        return Ok(font);
    }

    let mut db = fontdb::Database::new();
    db.load_font_data(data);
    db.faces()
        .find(|face| face.index == face_index)
        .and_then(|face| load_fontdb_face(&db, face.id))
        .ok_or_else(|| CustomDanmakuFontError {
            reason: "invalid_font",
            detail: format!("font contains no loadable face at index {face_index}"),
            bytes,
        })
}

fn load_font_family(family: &str) -> Option<FontArc> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    db.query(&query).and_then(|id| load_fontdb_face(&db, id))
}

fn load_fontdb_face(db: &fontdb::Database, id: fontdb::ID) -> Option<FontArc> {
    db.with_face_data(id, |data, face_index| {
        load_font_data(data.to_vec(), face_index)
    })
    .flatten()
}

fn load_font_data(data: Vec<u8>, face_index: u32) -> Option<FontArc> {
    FontVec::try_from_vec_and_index(data, face_index)
        .map(FontArc::new)
        .ok()
}

#[derive(Clone)]
struct SharedFont {
    // This field must be dropped before `data`, because its internal references
    // point into the allocation kept alive by `data`.
    font: FontRef<'static>,
    data: Arc<[u8]>,
}

impl SharedFont {
    fn try_new(data: Arc<[u8]>, face_index: u32) -> Option<Self> {
        // SAFETY: `bytes` points into the allocation owned by `data`. The Arc is
        // stored in the same value and is declared after `font`, so it remains
        // alive until all references held by FontRef have been dropped.
        let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr(), data.len()) };
        let font = FontRef::try_from_slice_and_index(bytes, face_index).ok()?;
        Some(Self { font, data })
    }
}

impl Font for SharedFont {
    fn units_per_em(&self) -> Option<f32> {
        self.font.units_per_em()
    }

    fn ascent_unscaled(&self) -> f32 {
        self.font.ascent_unscaled()
    }

    fn descent_unscaled(&self) -> f32 {
        self.font.descent_unscaled()
    }

    fn line_gap_unscaled(&self) -> f32 {
        self.font.line_gap_unscaled()
    }

    fn italic_angle(&self) -> f32 {
        self.font.italic_angle()
    }

    fn glyph_id(&self, c: char) -> GlyphId {
        self.font.glyph_id(c)
    }

    fn h_advance_unscaled(&self, id: GlyphId) -> f32 {
        self.font.h_advance_unscaled(id)
    }

    fn h_side_bearing_unscaled(&self, id: GlyphId) -> f32 {
        self.font.h_side_bearing_unscaled(id)
    }

    fn v_advance_unscaled(&self, id: GlyphId) -> f32 {
        self.font.v_advance_unscaled(id)
    }

    fn v_side_bearing_unscaled(&self, id: GlyphId) -> f32 {
        self.font.v_side_bearing_unscaled(id)
    }

    fn kern_unscaled(&self, first: GlyphId, second: GlyphId) -> f32 {
        self.font.kern_unscaled(first, second)
    }

    fn outline(&self, id: GlyphId) -> Option<Outline> {
        self.font.outline(id)
    }

    fn glyph_count(&self) -> usize {
        self.font.glyph_count()
    }

    fn codepoint_ids(&self) -> CodepointIdIter<'_> {
        self.font.codepoint_ids()
    }

    fn glyph_raster_image2(&self, id: GlyphId, pixel_size: u16) -> Option<v2::GlyphImage<'_>> {
        self.font.glyph_raster_image2(id, pixel_size)
    }

    fn glyph_svg_image(&self, id: GlyphId) -> Option<GlyphSvg<'_>> {
        self.font.glyph_svg_image(id)
    }

    fn font_data(&self) -> &[u8] {
        &self.data
    }
}

fn load_shared_font_data(data: Arc<[u8]>, face_index: u32) -> Option<FontArc> {
    SharedFont::try_new(data, face_index).map(FontArc::new)
}

fn load_default_font() -> Option<FontArc> {
    if let Ok(font) = FontArc::try_from_slice(NIPAPLAY_FALLBACK_FONT) {
        return Some(font);
    }

    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let families = [
        fontdb::Family::Name("PingFang SC"),
        fontdb::Family::Name("Hiragino Sans GB"),
        fontdb::Family::Name("Microsoft YaHei"),
        fontdb::Family::Name("Noto Sans CJK SC"),
        fontdb::Family::Name("Noto Sans CJK"),
        fontdb::Family::Name("Source Han Sans SC"),
        fontdb::Family::SansSerif,
    ];
    for family in families {
        let query = fontdb::Query {
            families: &[family],
            weight: fontdb::Weight::NORMAL,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };
        if let Some(font) = db.query(&query).and_then(|id| load_fontdb_face(&db, id)) {
            return Some(font);
        }
    }
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/SFNS.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(font) = load_font_from_path(Path::new(path), 0) {
            return Some(font);
        }
    }
    None
}

fn rasterize_font_glyph(font: &FontArc, key: GlyphCacheKey) -> RasterizedGlyph {
    let font_size = key.font_size();
    // Advance/kerning stay in display pixels. Only the outline raster has
    // more samples; its bearings and quad dimensions are mapped back once.
    let raster_scale = key.raster_scale();
    let scaled = font.as_scaled(px_scale_for_em(font, font_size));
    let glyph_id = scaled.glyph_id(key.ch);
    let advance = scaled.h_advance(glyph_id).max(0.0);
    if key.ch.is_whitespace() {
        return RasterizedGlyph::empty(key, advance.max(font_size * 0.35));
    }
    let glyph = Glyph {
        id: glyph_id,
        scale: px_scale_for_em(font, font_size * raster_scale),
        position: ab_glyph::point(0.0, 0.0),
    };
    let Some(outlined) = font.outline_glyph(glyph) else {
        return RasterizedGlyph::empty(key, advance.max(font_size * 0.5));
    };
    let bounds = outlined.px_bounds();
    let radius = key.raster_outline_radius();
    let pad = radius + 2 * i32::from(key.raster_scale);
    let width = (bounds.width().ceil() as i32 + pad * 2).max(1) as u32;
    let height = (bounds.height().ceil() as i32 + pad * 2).max(1) as u32;
    let stride = width as usize;
    let mut fill_alpha = vec![0u8; stride * height as usize];
    outlined.draw(|x, y, coverage| {
        let px = x as i32 + pad;
        let py = y as i32 + pad;
        if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
            return;
        }
        let index = py as usize * stride + px as usize;
        let value = (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
        fill_alpha[index] = fill_alpha[index].max(value);
    });
    let outline_alpha = if key.outline_radius > 0 {
        dilate_alpha(&fill_alpha, width, height, stride, radius)
    } else {
        fill_alpha.clone()
    };
    RasterizedGlyph {
        key,
        width,
        height,
        stride,
        fill_alpha,
        outline_alpha,
        offset_x: (bounds.min.x.floor() - pad as f32) / raster_scale,
        offset_y: (bounds.min.y.floor() - pad as f32) / raster_scale,
        advance,
    }
}

fn rasterize_fallback_glyph(shaper: &TextShaper, key: GlyphCacheKey) -> RasterizedGlyph {
    let font_size = key.font_size();
    if key.ch.is_whitespace() {
        return RasterizedGlyph::empty(key, font_size * 0.35);
    }
    let text = key.ch.to_string();
    let metrics = shaper.measure(&text, font_size);
    let raster_scale = key.raster_scale();
    let radius = key.raster_outline_radius();
    let pad = radius as u32 + 2 * u32::from(key.raster_scale);
    let width = (metrics.width * raster_scale).ceil().max(1.0) as u32 + pad * 2;
    let height = (metrics.height * raster_scale).ceil().max(1.0) as u32 + pad * 2;
    let stride = width as usize;
    let mut fill_alpha = vec![0u8; stride * height as usize];
    for y in pad..height.saturating_sub(pad) {
        for x in pad..width.saturating_sub(pad) {
            let border = x == pad
                || y == pad
                || x + 1 == width.saturating_sub(pad)
                || y + 1 == height.saturating_sub(pad);
            fill_alpha[y as usize * stride + x as usize] = if border { 180 } else { 230 };
        }
    }
    let outline_alpha = if key.outline_radius > 0 {
        dilate_alpha(&fill_alpha, width, height, stride, radius)
    } else {
        fill_alpha.clone()
    };
    RasterizedGlyph {
        key,
        width,
        height,
        stride,
        fill_alpha,
        outline_alpha,
        offset_x: -(pad as f32) / raster_scale,
        offset_y: -metrics.ascent - pad as f32 / raster_scale,
        advance: metrics.width.max(font_size * 0.5),
    }
}

fn dilate_alpha(input: &[u8], width: u32, height: u32, stride: usize, radius: i32) -> Vec<u8> {
    if radius <= 0 {
        return input.to_vec();
    }
    // A circular stroke avoids the square corners of a box dilation. Sliding
    // row maxima keep the supersampled outline O(radius * pixels), rather
    // than scanning a full square for every output pixel.
    let mut output = vec![0; input.len()];
    let mut queue = VecDeque::<usize>::new();
    for oy in -radius..=radius {
        let span = ((radius * radius - oy * oy) as f32).sqrt().floor() as usize;
        for y in 0..height as i32 {
            let source_y = y + oy;
            if source_y < 0 || source_y >= height as i32 {
                continue;
            }
            let row = &input[source_y as usize * stride..][..width as usize];
            queue.clear();
            let mut next = 0;
            for x in 0..width as usize {
                let right = (x + span + 1).min(width as usize);
                while next < right {
                    while queue.back().is_some_and(|&i| row[i] <= row[next]) {
                        queue.pop_back();
                    }
                    queue.push_back(next);
                    next += 1;
                }
                let left = x.saturating_sub(span);
                while queue.front().is_some_and(|&i| i < left) {
                    queue.pop_front();
                }
                let index = y as usize * stride + x;
                output[index] =
                    output[index].max(row[*queue.front().expect("nonempty row window")]);
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danmaku_colors_keep_opaque_fills_and_contrasting_outlines() {
        let colors = [
            (DanmakuColor::WHITE, [0.0, 0.0, 0.0, 1.0]),
            (DanmakuColor::rgb_u8(0, 0, 0), [1.0, 1.0, 1.0, 1.0]),
            (DanmakuColor::rgb_u8(32, 32, 32), [1.0, 1.0, 1.0, 1.0]),
            (DanmakuColor::rgb_u8(0, 0, 255), [0.0, 0.0, 0.0, 1.0]),
            (DanmakuColor::rgb_u8(204, 0, 0), [0.0, 0.0, 0.0, 1.0]),
            (DanmakuColor::rgb_u8(128, 0, 0), [0.0, 0.0, 0.0, 1.0]),
            (DanmakuColor::rgb_u8(255, 255, 0), [0.0, 0.0, 0.0, 1.0]),
            (DanmakuColor::rgb_u8(255, 0, 0), [0.0, 0.0, 0.0, 1.0]),
        ];
        for (color, outline) in colors {
            let timeline = DanmakuTimeline::new(vec![DanmakuItem {
                color,
                ..item(0.0, "彩色弹幕", DanmakuMode::Top)
            }])
            .unwrap();
            for supersampling in [false, true] {
                let mut engine = DfmLayoutEngine::new(
                    timeline.clone(),
                    DanmakuLayoutConfig {
                        supersampling,
                        ..DanmakuLayoutConfig::default()
                    },
                );
                let plan =
                    engine.render_plan(Duration::from_secs(1), DanmakuViewport::new(640, 360), 1);
                assert!(!plan.items.is_empty());
                for glyph in &plan.items {
                    assert_eq!(glyph.color_rgba, color.rgba(1.0));
                    assert_eq!(glyph.outline_rgba, outline);
                }
                let mut config = engine.config().clone();
                config.opacity = 0.5;
                assert_eq!(engine.apply_config(config), DanmakuConfigChange::PaintOnly);
                let translucent = engine.render_plan(Duration::from_secs(1), plan.viewport, 1);
                for glyph in &translucent.items {
                    assert_eq!(glyph.color_rgba, color.rgba(0.5));
                    assert_eq!(&glyph.outline_rgba[..3], &outline[..3]);
                    assert_eq!(glyph.outline_rgba[3], 0.5);
                }
            }
        }
        let transparent_black = DanmakuColor {
            alpha: 0.5,
            ..DanmakuColor::rgb_u8(0, 0, 0)
        };
        assert_eq!(transparent_black.outline_rgba(0.5), [1.0, 1.0, 1.0, 0.25]);
    }

    #[test]
    fn retained_font_preview_keeps_its_atlas_while_new_masks_are_packed() {
        let viewport = DanmakuViewport::new(640, 360);
        let mut engine = DfmLayoutEngine::new(
            DanmakuTimeline::new(vec![item(0.0, "彩色弹幕", DanmakuMode::Top)]).unwrap(),
            DanmakuLayoutConfig::default(),
        );
        let prepared = engine.prepare(viewport, 1);
        let frame = prepared.frame_layout(Duration::from_secs(1), 1);
        let before = engine
            .rasterizer
            .render_plan_scaled(&frame, 1.0, prepared.atlas());
        let mut config = engine.config().clone();
        config.font_size *= 1.5;
        engine.set_config(config);
        let replacement = engine.prepare(viewport, 1);
        assert_ne!(
            replacement.atlas().unwrap().version,
            before.atlas.as_ref().unwrap().version
        );
        let preview = engine
            .rasterizer
            .render_plan_scaled(&frame, 1.5, prepared.atlas());
        assert!(Arc::ptr_eq(
            preview.atlas.as_ref().unwrap(),
            before.atlas.as_ref().unwrap()
        ));
        assert_eq!(preview.items[0].tex_rect, before.items[0].tex_rect);
        assert_eq!(preview.items[0].rect[2], before.items[0].rect[2] * 1.5);
    }

    #[test]
    fn source_opacity_is_not_squared_by_dfm_color_round_trip() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            color: DanmakuColor {
                alpha: 0.5,
                ..DanmakuColor::WHITE
            },
            opacity: 0.5,
            ..item(0.0, "透明度", DanmakuMode::Top)
        }])
        .unwrap();
        let mut engine = DfmLayoutEngine::new(
            timeline,
            DanmakuLayoutConfig {
                opacity: 0.8,
                ..DanmakuLayoutConfig::default()
            },
        );
        let plan = engine.render_plan(Duration::from_secs(1), DanmakuViewport::new(640, 360), 1);
        assert!(!plan.items.is_empty());
        for glyph in &plan.items {
            // The color and DFM opacity each have one 8-bit quantization.
            assert!((glyph.color_rgba[3] - 0.5 * 0.5 * 0.8).abs() < 0.002);
            assert!((glyph.outline_rgba[3] - glyph.color_rgba[3]).abs() < 1e-6);
        }
    }

    #[test]
    fn supersampling_preserves_layout_motion_and_display_size() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "清晰弹幕 Aa", DanmakuMode::Scroll),
            item(0.0, "顶部字幕", DanmakuMode::Top),
            item(0.0, "底部字幕", DanmakuMode::Bottom),
        ])
        .unwrap();
        for density in [1.0, 1.7725, 2.0] {
            let viewport = DanmakuViewport::with_scale(1920, 1080, density);
            let mut native = DfmLayoutEngine::new(
                timeline.clone(),
                DanmakuLayoutConfig {
                    supersampling: false,
                    ..DanmakuLayoutConfig::default()
                },
            );
            let mut high = DfmLayoutEngine::new(timeline.clone(), DanmakuLayoutConfig::default());
            for time in [0.5, 1.5] {
                let time = Duration::from_secs_f64(time);
                let a = native.frame_layout(time, viewport, 1);
                let b = high.frame_layout(time, viewport, 1);
                assert_eq!(a.items.len(), b.items.len());
                assert!(!a.items.is_empty());
                for (a, b) in a.items.iter().zip(&b.items) {
                    assert_eq!((a.x, a.y), (b.x, b.y));
                    assert_eq!(a.text_layout.metrics, b.text_layout.metrics);
                    for (a, b) in a.text_layout.glyphs.iter().zip(&b.text_layout.glyphs) {
                        assert_eq!(a.pen_x, b.pen_x);
                        assert_eq!(a.glyph.advance, b.glyph.advance);
                        assert_eq!(b.glyph.key.raster_scale, 2);
                        assert!((a.glyph.width as f32 - b.glyph.width as f32 / 2.0).abs() <= 1.0);
                        assert!((a.glyph.offset_x - b.glyph.offset_x).abs() <= 0.5);
                        assert!((a.glyph.offset_y - b.glyph.offset_y).abs() <= 0.5);
                    }
                }
                let plan = high.rasterizer.render_plan(&b);
                let atlas = plan.atlas.as_ref().unwrap();
                for glyph in &plan.items {
                    assert_close(glyph.tex_rect[2] * atlas.width as f32, glyph.rect[2] * 2.0);
                    assert_close(glyph.tex_rect[3] * atlas.height as f32, glyph.rect[3] * 2.0);
                }
            }
        }
    }

    #[test]
    fn supersampling_toggle_rebuilds_atlas_and_large_fonts_limit_raster_cost() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "字体", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let first = engine.render_plan(Duration::from_secs(1), DanmakuViewport::new(640, 360), 1);
        let mut config = engine.config().clone();
        config.supersampling = false;
        assert_eq!(engine.apply_config(config), DanmakuConfigChange::Layout);
        let second = engine.render_plan(Duration::from_secs(1), DanmakuViewport::new(640, 360), 2);
        assert_ne!(first.atlas.unwrap().version, second.atlas.unwrap().version);
        assert_eq!(
            GlyphCacheKey::new(0, '字', 129.0, 2.0, true).raster_scale,
            1
        );
    }

    #[test]
    fn raster_cache_rollover_preserves_existing_prepared_frames() {
        let mut rasterizer = DanmakuTextRasterizer::default();
        let key = GlyphCacheKey::new(0, '字', 30.0, 2.0, true);
        let glyph = rasterizer.cached_glyph_by_key(key);
        rasterizer.pack_glyph(&glyph);
        let old = rasterizer.clone();
        let old_atlas = old.glyph_atlas.lock().unwrap().snapshot().unwrap();
        // Trigger the soft budget without allocating a large test texture.
        rasterizer.glyph_atlas.lock().unwrap().height = GLYPH_ATLAS_SOFT_HEIGHT;
        rasterizer.recycle_caches_if_needed();
        assert!(Arc::ptr_eq(
            &old_atlas,
            &old.glyph_atlas.lock().unwrap().snapshot().unwrap()
        ));
        assert!(rasterizer.glyph_cache.lock().unwrap().is_empty());
        rasterizer.pack_glyph(&glyph);
        let new_atlas = rasterizer.glyph_atlas.lock().unwrap().snapshot().unwrap();
        assert_ne!(old_atlas.version, new_atlas.version);
    }

    #[test]
    fn round_outline_matches_disk_reference_with_padded_rows() {
        let (width, height, stride) = (13, 9, 16);
        let mut input = vec![0; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                input[y * stride + x] = ((x * 37 + y * 19) % 256) as u8;
            }
        }
        for radius in [1, 2, 4, 16] {
            let actual = dilate_alpha(&input, width, height, stride, radius);
            for y in 0..height as i32 {
                for x in 0..width as i32 {
                    let mut expected = 0;
                    for dy in -radius..=radius {
                        for dx in -radius..=radius {
                            if dx * dx + dy * dy <= radius * radius
                                && (0..width as i32).contains(&(x + dx))
                                && (0..height as i32).contains(&(y + dy))
                            {
                                expected = expected
                                    .max(input[(y + dy) as usize * stride + (x + dx) as usize]);
                            }
                        }
                    }
                    assert_eq!(actual[y as usize * stride + x as usize], expected);
                }
            }
        }
        let mut dot = vec![0; 49];
        dot[24] = 255;
        let outline = dilate_alpha(&dot, 7, 7, 7, 2);
        assert_eq!(outline[24 + 2], 255);
        assert_eq!(outline[24 + 2 + 14], 0, "stroke corners must be round");
    }
    fn test_ttc() -> Vec<u8> {
        let source = NIPAPLAY_FALLBACK_FONT;
        let face_count = 2u32;
        let header_len = 12 + face_count as usize * 4;
        let first_offset = (header_len + 3) & !3;
        let second_offset = (first_offset + source.len() + 3) & !3;
        let mut collection = vec![0; second_offset + source.len()];
        collection[0..4].copy_from_slice(b"ttcf");
        collection[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        collection[8..12].copy_from_slice(&face_count.to_be_bytes());
        collection[12..16].copy_from_slice(&(first_offset as u32).to_be_bytes());
        collection[16..20].copy_from_slice(&(second_offset as u32).to_be_bytes());

        for (face_index, face_offset) in [first_offset, second_offset].into_iter().enumerate() {
            collection[face_offset..face_offset + source.len()].copy_from_slice(source);
            let table_count = u16::from_be_bytes([source[4], source[5]]) as usize;
            for table_index in 0..table_count {
                let record = 12 + table_index * 16;
                let source_offset =
                    u32::from_be_bytes(source[record + 8..record + 12].try_into().unwrap())
                        as usize;
                let collection_record = face_offset + record + 8;
                collection[collection_record..collection_record + 4]
                    .copy_from_slice(&((face_offset + source_offset) as u32).to_be_bytes());
                if face_index == 1 && &source[record..record + 4] == b"head" {
                    collection[face_offset + source_offset + 18..face_offset + source_offset + 20]
                        .copy_from_slice(&512u16.to_be_bytes());
                }
            }
        }
        collection
    }

    fn item(time: f64, text: &str, mode: DanmakuMode) -> DanmakuItem {
        DanmakuItem {
            id: 0,
            pts: Duration::from_secs_f64(time),
            text: text.to_string(),
            mode,
            font_size: 24.0,
            color: DanmakuColor::WHITE,
            opacity: 1.0,
            is_self: false,
        }
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.001,
            "expected {actual} to be close to {expected}"
        );
    }

    fn selected_font(data: Vec<u8>) -> SubtitleFontAttachment {
        SubtitleFontAttachment::new("selected", None, Vec::new(), Arc::<[u8]>::from(data))
    }

    #[test]
    fn parses_json_long_and_short_fields() {
        let input = r##"{
            "comments": [
                {"time": 1.0, "content": "hello", "type": "scroll", "color": "#ff0000"},
                {"t": 2.0, "c": "top", "y": "top", "r": "rgb(0,255,0)"}
            ]
        }"##;
        let timeline = DanmakuTimeline::from_json(input).unwrap();

        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline.items()[0].mode, DanmakuMode::Scroll);
        assert_eq!(timeline.items()[1].mode, DanmakuMode::Top);
        assert_eq!(timeline.items()[0].color.red, 1.0);
        assert_eq!(timeline.items()[1].color.green, 1.0);
    }

    #[test]
    fn parses_nipaplay_camel_case_item_fields() {
        let input = r#"[{"t":1,"c":"phone","y":"scroll","r":16777215,"fontSize":30,"isMe":true}]"#;
        let timeline = DanmakuTimeline::from_json(input).unwrap();

        assert_eq!(timeline.items()[0].font_size, 30.0);
        assert!(timeline.items()[0].is_self);
    }

    #[test]
    fn parses_json_ids_without_floating_point_rounding() {
        let input = r#"[
            {"id":9007199254740993,"time":1,"content":"first"},
            {"id":"18446744073709551615","time":2,"content":"second"}
        ]"#;
        let timeline = DanmakuTimeline::from_json(input).unwrap();

        assert_eq!(timeline.items()[0].id, 9_007_199_254_740_993);
        assert_eq!(timeline.items()[1].id, u64::MAX);
    }

    #[test]
    fn skips_finite_timestamps_that_cannot_fit_in_a_duration() {
        let xml = r#"<i><d p="1e100,1,25,16777215,0,0,0,0">broken</d><d p="2.0,1,25,16777215,0,0,0,1">good</d></i>"#;
        let timeline = DanmakuTimeline::from_bilibili_xml(xml).unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline.items()[0].text, "good");
        let json =
            r#"{"comments":[{"time":"1e100","content":"broken"},{"time":2.0,"content":"good"}]}"#;
        let timeline = DanmakuTimeline::from_json(json).unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline.items()[0].text, "good");
    }

    #[test]
    fn skips_items_with_non_finite_timestamps_instead_of_panicking() {
        // "1e400" parses as f64 infinity, which Duration::from_secs_f64
        // panics on; such an item must be skipped, not take the load down.
        let xml = r#"<i><d p="1e400,1,25,16777215,0,0,0,0">broken</d><d p="2.0,1,25,16777215,0,0,0,1">good</d></i>"#;
        let timeline = DanmakuTimeline::from_bilibili_xml(xml).unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline.items()[0].text, "good");

        let json = r#"{"comments":[
            {"time":"1e400","content":"broken"},
            {"time":2.0,"content":"good"}
        ]}"#;
        let timeline = DanmakuTimeline::from_json(json).unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline.items()[0].text, "good");
    }

    #[test]
    fn session_allocates_layout_ids_independently_of_source_ids() {
        let mut first = item(1.0, "first", DanmakuMode::Scroll);
        first.id = u64::MAX;
        let mut second = item(2.0, "second", DanmakuMode::Scroll);
        second.id = u64::MAX;
        let timeline = DanmakuTimeline::new(vec![first, second]).unwrap();
        let mut session = DanmakuSession::from_timeline(timeline);

        let ids = session
            .active_timeline()
            .items()
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn parses_json_lines_and_xml() {
        let jsonl = r#"{"t":1,"c":"a","y":"bottom","r":16777215}
{"t":2,"c":"b","y":"scroll"}"#;
        let timeline = DanmakuTimeline::from_json_lines(jsonl).unwrap();
        assert_eq!(timeline.items()[0].mode, DanmakuMode::Bottom);

        let xml = r#"<i><d p="1.0,1,25,16777215,0,0,0,42">滚动&amp;测试</d><d p="2.0,5,25,16711680,0,0,0,0">顶部</d></i>"#;
        let timeline = DanmakuTimeline::from_bilibili_xml(xml).unwrap();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline.items()[0].text, "滚动&测试");
        assert_eq!(timeline.items()[1].mode, DanmakuMode::Top);
    }

    #[test]
    fn dfm_layout_avoids_same_time_scroll_collision() {
        let timeline = DanmakuTimeline::new(vec![
            item(1.0, "wide comment", DanmakuMode::Scroll),
            item(1.0, "another comment", DanmakuMode::Scroll),
        ])
        .unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 7);

        assert_eq!(prepared.items().len(), 2);
        assert_ne!(prepared.items()[0].y, prepared.items()[1].y);
    }

    #[test]
    fn default_scroll_layout_uses_full_display_area_when_configured() {
        let timeline = DanmakuTimeline::new(
            (0..18)
                .map(|index| item(1.0, &format!("line {index}"), DanmakuMode::Scroll))
                .collect(),
        )
        .unwrap();
        let config = DanmakuLayoutConfig {
            track_gap_ratio: 0.0,
            merge_duplicates: false,
            allow_scroll_overwrite: false,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 7);
        let max_y = prepared
            .items()
            .iter()
            .map(|item| item.y)
            .fold(0.0, f32::max);

        assert!(
            max_y > 270.0,
            "display_area=1.0 should allow scroll rows into the lower viewport, got max y {max_y}"
        );
        assert!(
            max_y < 360.0,
            "scroll rows should still stay inside the viewport, got max y {max_y}"
        );
    }

    #[test]
    fn dense_default_layout_keeps_using_the_full_display_area() {
        let timeline = DanmakuTimeline::new(
            (0..960)
                .map(|index| {
                    let mut entry = item(
                        index as f64 / 80.0,
                        &format!("dense default {index}"),
                        DanmakuMode::Scroll,
                    );
                    entry.id = index + 1;
                    entry
                })
                .collect(),
        )
        .unwrap();
        let config = DanmakuLayoutConfig {
            merge_duplicates: false,
            display_area: 1.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let viewport = DanmakuViewport::new(640, 360);
        let prepared = engine.prepare(viewport, 7);
        let frame = prepared.frame_layout(Duration::from_secs_f64(10.0), 7);
        let max_y = frame
            .items
            .iter()
            .filter(|item| item.mode == DanmakuMode::Scroll)
            .map(|item| item.y)
            .fold(0.0, f32::max);

        assert!(
            max_y > viewport.height as f32 * 0.75,
            "dense default layout must keep using the full display area; max y={max_y}"
        );
    }

    #[test]
    fn frame_layout_uses_generation_and_media_time() {
        let timeline = DanmakuTimeline::new(vec![item(1.0, "sync", DanmakuMode::Scroll)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);
        let layout = engine.frame_layout(Duration::from_secs_f64(1.5), viewport, 10);
        let stale = engine.frame_layout(Duration::from_secs_f64(1.5), viewport, 11);

        assert_eq!(layout.generation, 10);
        assert_eq!(layout.items.len(), 1);
        assert_eq!(stale.generation, 11);
    }

    #[test]
    fn generation_change_reuses_prepared_layout() {
        let timeline = DanmakuTimeline::new(vec![item(1.0, "seek", DanmakuMode::Scroll)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);

        let first = engine.prepare(viewport, 1);
        let first_text = Arc::clone(&first.items()[0].text);
        let first_text_layout = Arc::clone(&first.items()[0].text_layout);
        let layout = engine.frame_layout(Duration::from_secs_f64(1.5), viewport, 2);
        let prepared = engine.prepared.as_ref().expect("prepared layout exists");

        assert_eq!(layout.generation, 2);
        assert!(Arc::ptr_eq(&prepared.items()[0].text, &first_text));
        assert!(Arc::ptr_eq(
            &prepared.items()[0].text_layout,
            &first_text_layout
        ));
    }

    #[test]
    fn sliding_timeline_window_preserves_tracks_for_overlapping_items() {
        let timeline = DanmakuTimeline::new(
            (0..80)
                .map(|index| {
                    let mut entry = item(
                        index as f64 * 0.25,
                        &format!("scroll {index}"),
                        DanmakuMode::Scroll,
                    );
                    entry.id = index + 1;
                    entry
                })
                .collect(),
        )
        .unwrap();
        let config = DanmakuLayoutConfig {
            allow_stacking: true,
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let viewport = DanmakuViewport::new(640, 360);
        let first_window = timeline.window(Duration::ZERO, Duration::from_secs(12));
        let second_window = timeline.window(Duration::from_secs(4), Duration::from_secs(16));
        let mut engine = DfmLayoutEngine::new(first_window, config);

        let first = engine.prepare(viewport, 1);
        let first_tracks = first
            .items()
            .iter()
            .map(|item| (item.id, item.track_index))
            .collect::<HashMap<_, _>>();
        engine.sync_timeline(&second_window);
        let second = engine.prepare(viewport, 1);

        let overlap = second
            .items()
            .iter()
            .filter_map(|item| {
                first_tracks
                    .get(&item.id)
                    .map(|track| (*track, item.track_index))
            })
            .collect::<Vec<_>>();
        assert!(!overlap.is_empty());
        assert!(overlap.iter().all(|(before, after)| before == after));
        assert_eq!(engine.stable_tracks.len(), second.items().len());
        assert!(
            engine
                .stable_tracks
                .keys()
                .all(|item_id| second.items().iter().any(|item| item.id == *item_id))
        );
    }

    #[test]
    fn sliding_timeline_window_does_not_revive_rejected_items() {
        let mut entries = vec![
            item(0.0, "occupy", DanmakuMode::Scroll),
            item(0.0, "occupy", DanmakuMode::Scroll),
            item(
                1.0,
                "this rejected danmaku is deliberately very wide",
                DanmakuMode::Scroll,
            ),
            item(4.0, "already visible", DanmakuMode::Scroll),
        ];
        for (index, entry) in entries.iter_mut().enumerate() {
            entry.id = index as u64 + 1;
        }
        let timeline = DanmakuTimeline::new(entries).unwrap();
        let config = DanmakuLayoutConfig {
            track_gap_ratio: 0.0,
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let viewport = DanmakuViewport::new(320, 58);
        let first_window = timeline.window(Duration::ZERO, Duration::from_secs(8));
        let second_window = timeline.window(Duration::from_secs(1), Duration::from_secs(9));
        let mut engine = DfmLayoutEngine::new(first_window, config);

        let first = engine.prepare(viewport, 1);
        assert!(!first.items().iter().any(|item| item.id == 3));
        let original_track = first
            .items()
            .iter()
            .find(|item| item.id == 4)
            .expect("later item is placed after the first pair expires")
            .track_index;

        engine.sync_timeline(&second_window);
        let second = engine.prepare(viewport, 1);

        assert!(!second.items().iter().any(|item| item.id == 3));
        assert_eq!(
            second
                .items()
                .iter()
                .find(|item| item.id == 4)
                .expect("visible item remains prepared")
                .track_index,
            original_track
        );
    }

    #[test]
    fn render_plan_contains_glyph_atlas() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "GPU", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let plan = engine.render_plan(
            Duration::from_millis(500),
            DanmakuViewport::new(640, 360),
            3,
        );

        assert_eq!(plan.generation, 3);
        assert!(!plan.items.is_empty());
        let atlas = plan.atlas.as_ref().expect("glyph atlas exists");
        assert!(atlas.is_valid());
        assert!(plan.items.iter().all(|item| item.tex_rect[2] > 0.0));
    }

    #[test]
    fn render_plan_reuses_glyph_atlas_snapshot_for_stable_glyph_set() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "GPU", DanmakuMode::Scroll)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);

        let first = engine.render_plan(Duration::from_millis(500), viewport, 3);
        let first_atlas = first.atlas.as_ref().expect("glyph atlas exists").clone();
        let first_x = first.items[0].rect[0];
        let second = engine.render_plan(Duration::from_millis(600), viewport, 4);
        let second_atlas = second.atlas.as_ref().expect("glyph atlas exists");

        assert_eq!(second.generation, 4);
        assert_ne!(first_x, second.items[0].rect[0]);
        assert_eq!(first_atlas.version, second_atlas.version);
        assert!(Arc::ptr_eq(&first_atlas, second_atlas));
    }

    #[test]
    fn glyph_atlas_snapshot_describes_only_the_new_dirty_region() {
        let first_timeline = DanmakuTimeline::new(vec![item(0.0, "A", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(first_timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);
        let first = engine.render_plan(Duration::from_millis(500), viewport, 1);
        let first_atlas = first.atlas.as_ref().expect("first glyph atlas exists");
        assert!(first_atlas.update.is_none());

        engine.set_timeline(DanmakuTimeline::new(vec![item(0.0, "AB", DanmakuMode::Top)]).unwrap());
        let second = engine.render_plan(Duration::from_millis(500), viewport, 2);
        let second_atlas = second.atlas.as_ref().expect("second glyph atlas exists");
        let update = second_atlas
            .update
            .as_ref()
            .expect("new glyph produces an incremental update");

        assert_eq!(update.from_version, first_atlas.version);
        assert!(update.width > 0 && update.height > 0);
        assert!(update.width < second_atlas.width);
        assert!(
            second_atlas
                .incremental_update_from(
                    first_atlas.version,
                    first_atlas.width,
                    first_atlas.height,
                    first_atlas.stride,
                )
                .is_some()
        );
    }

    #[test]
    fn layout_equivalence_only_ignores_the_declared_paint_only_fields() {
        let base = DanmakuLayoutConfig::default();

        let mut paint_only = base.clone();
        paint_only.opacity = 0.25;
        paint_only.shadow_offset = [4.0, 4.0];
        paint_only.shadow_style = DanmakuShadowStyle::None;
        paint_only.enabled = !base.enabled;
        assert!(base.layout_equivalent(&paint_only));

        // Anything outside that set must force a relayout; the normalization
        // is subtractive so a field added later is layout-affecting by default.
        let mut layout = base.clone();
        layout.font_size += 1.0;
        assert!(!base.layout_equivalent(&layout));

        let mut blocked = base.clone();
        blocked.block_words = vec!["blocked".to_string()];
        assert!(!base.layout_equivalent(&blocked));
    }

    #[test]
    fn paint_only_config_change_reuses_prepared_layout() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "paint", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);
        let first = engine.prepare(viewport, 1);
        let first_text_layout = Arc::clone(&first.items()[0].text_layout);

        let mut config = engine.config().clone();
        config.opacity = 0.4;
        config.shadow_style = DanmakuShadowStyle::None;
        assert_eq!(engine.apply_config(config), DanmakuConfigChange::PaintOnly);

        let prepared = engine.prepared.as_ref().expect("prepared layout retained");
        assert!(Arc::ptr_eq(
            &prepared.items()[0].text_layout,
            &first_text_layout
        ));
        let frame = prepared.frame_layout(Duration::from_millis(500), 2);
        assert_close(frame.items[0].opacity, 0.4);
        assert_close(frame.items[0].shadow_alpha, 0.0);
    }

    #[test]
    fn font_size_change_preserves_stable_track_preferences() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "first", DanmakuMode::Top),
            item(0.1, "second", DanmakuMode::Top),
            item(0.2, "third", DanmakuMode::Top),
        ])
        .unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(1280, 720);
        let first = engine.prepare(viewport, 1);
        let first_tracks = first
            .items()
            .iter()
            .map(|item| (item.id, item.track_index))
            .collect::<HashMap<_, _>>();

        let mut config = engine.config().clone();
        config.font_size += 2.0;
        assert_eq!(engine.apply_config(config), DanmakuConfigChange::Layout);
        assert_eq!(engine.stable_tracks, first_tracks);

        let second = engine.prepare(viewport, 2);
        for item in second.items() {
            assert_eq!(
                Some(&item.track_index),
                first_tracks.get(&item.id),
                "overlapping item {} changed logical track",
                item.id
            );
        }
    }

    #[test]
    fn disabling_is_immediate_but_re_enabling_invalidates_empty_layouts() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "toggle", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);

        let enabled = engine.prepare(viewport, 1);
        assert!(!enabled.items().is_empty());

        let mut config = engine.config().clone();
        config.enabled = false;
        assert_eq!(
            engine.apply_config(config.clone()),
            DanmakuConfigChange::PaintOnly
        );
        let hidden_frame = engine
            .prepared
            .as_ref()
            .expect("disabling retains the prepared geometry")
            .frame_layout(Duration::from_millis(500), 2);
        assert!(hidden_frame.items.is_empty());

        // A later planner window prepared while disabled is intentionally
        // empty, which is the state that must not survive re-enabling.
        assert!(engine.prepare(viewport, 2).items().is_empty());

        config.enabled = true;
        assert_eq!(engine.apply_config(config), DanmakuConfigChange::Layout);
        assert!(
            engine.prepared.is_none(),
            "the disabled empty layout must not be reused after enabling danmaku"
        );
        assert!(!engine.prepare(viewport, 3).items().is_empty());
    }

    #[test]
    fn scroll_duration_uses_logical_width_and_retina_scale() {
        let config = DanmakuLayoutConfig::default();
        let narrow = scroll_duration_for_viewport(&config, DanmakuViewport::new(640, 360));
        let reference = scroll_duration_for_viewport(&config, DanmakuViewport::new(1280, 720));
        let wide = scroll_duration_for_viewport(&config, DanmakuViewport::new(1920, 1080));
        let retina_reference =
            scroll_duration_for_viewport(&config, DanmakuViewport::with_scale(2560, 1440, 2.0));

        assert_eq!(narrow, Duration::from_secs(9));
        assert_eq!(reference, Duration::from_secs(10));
        assert_eq!(wide, Duration::from_secs(13));
        assert_eq!(retina_reference, reference);
    }

    #[test]
    fn selected_memory_fonts_fallback_in_selection_order() {
        let first = selected_font(NIPAPLAY_FALLBACK_FONT.to_vec());
        let second = selected_font(test_ttc());
        let selection = DanmakuFontSelection::new(1, Arc::from(vec![first, second]));
        let rasterizer = DanmakuTextRasterizer::for_config_and_selection(
            &DanmakuLayoutConfig::default(),
            &selection,
        );

        let resolved = rasterizer
            .resolve_font('A')
            .expect("selected font resolves");

        assert_eq!(resolved.id, 1);
    }

    #[test]
    fn font_selection_generation_atomically_invalidates_rasterizer_caches() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "cache", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);
        let first = engine.render_plan(Duration::from_millis(500), viewport, 1);
        let first_atlas = first.atlas.expect("first atlas exists");
        let selection = DanmakuFontSelection::new(
            2,
            Arc::from(vec![selected_font(NIPAPLAY_FALLBACK_FONT.to_vec())]),
        );

        assert!(engine.set_font_selection(selection.clone()));
        assert!(!engine.set_font_selection(selection));
        assert!(engine.prepared.is_none());
        let second = engine.render_plan(Duration::from_millis(500), viewport, 2);
        let second_atlas = second.atlas.expect("second atlas exists");

        assert!(!Arc::ptr_eq(&first_atlas, &second_atlas));
    }

    #[test]
    fn layout_font_size_uses_nipaplay_logical_units_across_resize() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: DEFAULT_SOURCE_FONT_SIZE,
            ..item(0.0, "scale", DanmakuMode::Scroll)
        }])
        .unwrap();
        let config = DanmakuLayoutConfig {
            font_size: 32.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);

        let small = engine.prepare(DanmakuViewport::new(640, 360), 1);
        let large = engine.prepare(DanmakuViewport::new(1920, 1080), 2);
        let expected = 32.0;

        assert_close(small.items()[0].font_size, expected);
        assert_close(large.items()[0].font_size, expected);
        assert!(large.items()[0].scroll_speed > small.items()[0].scroll_speed);
    }

    #[test]
    fn layout_font_size_uses_surface_scale_for_physical_pixels() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: DEFAULT_SOURCE_FONT_SIZE,
            ..item(0.0, "scale", DanmakuMode::Top)
        }])
        .unwrap();
        let config = DanmakuLayoutConfig {
            font_size: 32.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);

        let one_x = engine.prepare(DanmakuViewport::with_scale(800, 450, 1.0), 1);
        let two_x = engine.prepare(DanmakuViewport::with_scale(1600, 900, 2.0), 2);
        let expected = 32.0;

        assert_close(one_x.items()[0].font_size, expected);
        assert_close(two_x.items()[0].font_size, expected * 2.0);
    }

    #[test]
    fn nipaplay_phone_font_size_uses_fractional_android_density() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: DEFAULT_SOURCE_FONT_SIZE,
            ..item(0.0, "android density", DanmakuMode::Top)
        }])
        .unwrap();
        let config = DanmakuLayoutConfig {
            font_size: 20.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);

        let prepared = engine.prepare(DanmakuViewport::with_scale(1080, 2400, 2.625), 1);

        assert_close(prepared.items()[0].font_size, 52.5);
    }

    #[test]
    fn source_font_size_is_relative_to_configured_reference_size() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: 50.0,
            ..item(0.0, "large", DanmakuMode::Top)
        }])
        .unwrap();
        let config = DanmakuLayoutConfig {
            font_size: 30.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(1920, 1080), 1);

        assert_close(prepared.items()[0].font_size, 60.0);
    }

    #[test]
    fn render_plan_keeps_fill_and_outline_masks_separate() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "Outline", DanmakuMode::Top)]).unwrap();
        let config = DanmakuLayoutConfig {
            outline_width: 2.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let plan = engine.render_plan(
            Duration::from_millis(500),
            DanmakuViewport::new(640, 360),
            3,
        );
        let atlas = plan.atlas.as_ref().expect("glyph atlas exists");
        let fill_sum = atlas.fill_alpha.iter().map(|&v| u64::from(v)).sum::<u64>();
        let outline_sum = atlas
            .outline_alpha
            .iter()
            .map(|&v| u64::from(v))
            .sum::<u64>();

        assert!(outline_sum > fill_sum);
        assert_ne!(atlas.fill_alpha, atlas.outline_alpha);
    }

    #[test]
    fn keyword_filter_blocks_items() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "visible", DanmakuMode::Scroll),
            item(0.1, "blocked bad", DanmakuMode::Scroll),
        ])
        .unwrap();
        let config = DanmakuLayoutConfig {
            block_words: vec!["bad".to_string()],
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        assert_eq!(prepared.items().len(), 1);
        assert_eq!(prepared.items()[0].text.as_ref(), "visible");
    }

    #[test]
    fn duplicate_merge_matches_dfm_prepare_scope() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "same", DanmakuMode::Top),
            item(0.1, "same", DanmakuMode::Top),
            item(11.0, "same", DanmakuMode::Top),
        ])
        .unwrap();
        let config = DanmakuLayoutConfig {
            merge_duplicates: true,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        assert_eq!(prepared.items().len(), 1);
        assert_eq!(prepared.items()[0].duplicate_count, 3);
        assert_eq!(prepared.items()[0].text.as_ref(), "same x3");
    }

    #[test]
    fn parser_skips_bad_individual_items() {
        let input = r#"{"comments":[
            {"time":1,"content":"ok"},
            {"time":2,"content":""},
            {"bad":"shape"},
            {"time":3,"content":"still ok"}
        ]}"#;
        let timeline = DanmakuTimeline::from_json(input).unwrap();

        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline.items()[0].text, "ok");
        assert_eq!(timeline.items()[1].text, "still ok");
    }

    #[test]
    fn dfm_overwrite_marks_displaced_scroll_items_filtered() {
        let mut first = item(0.0, "first wide", DanmakuMode::Scroll);
        first.font_size = 24.0;
        let mut second = item(0.0, "second wide", DanmakuMode::Scroll);
        second.font_size = 24.0;
        let timeline = DanmakuTimeline::new(vec![first, second]).unwrap();
        let config = DanmakuLayoutConfig {
            display_area: 0.1,
            track_gap_ratio: 0.0,
            allow_scroll_overwrite: true,
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(320, 24), 1);

        assert_eq!(prepared.items().len(), 1);
        assert_eq!(prepared.items()[0].text.as_ref(), "second wide");
    }

    #[test]
    fn max_lines_caps_each_danmaku_mode_tracks() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "a", DanmakuMode::Top),
            item(0.0, "b", DanmakuMode::Top),
            item(0.0, "c", DanmakuMode::Bottom),
            item(0.0, "d", DanmakuMode::Bottom),
        ])
        .unwrap();
        let config = DanmakuLayoutConfig {
            max_lines_per_mode: Some(1),
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        let top_count = prepared
            .items()
            .iter()
            .filter(|item| item.mode == DanmakuMode::Top)
            .count();
        let bottom_count = prepared
            .items()
            .iter()
            .filter(|item| item.mode == DanmakuMode::Bottom)
            .count();

        assert!(!prepared.items().is_empty());
        assert!(top_count <= 1);
        assert!(bottom_count <= 1);
        assert!(prepared.items().iter().all(|item| item.track_index == 0));
    }

    #[test]
    fn regex_filter_blocks_matching_items() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "visible", DanmakuMode::Scroll),
            item(0.1, "episode 123 spoiler", DanmakuMode::Scroll),
        ])
        .unwrap();
        let config = DanmakuLayoutConfig {
            block_words: vec!["spoiler/[0-9]+ spoiler/".to_string()],
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        assert_eq!(prepared.items().len(), 1);
        assert_eq!(prepared.items()[0].text.as_ref(), "visible");
    }

    #[test]
    fn json_special_danmaku_is_filtered_from_layout() {
        let input = r#"{"comments":[{"time":1,"content":"path","type":"special"}]}"#;
        let timeline = DanmakuTimeline::from_json(input).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        assert_eq!(prepared.items().len(), 0);
    }

    #[test]
    fn custom_font_rejects_oversized_file_from_metadata() {
        let path = std::env::temp_dir().join(format!(
            "erika_danmaku_oversized_font_{}.ttf",
            std::process::id()
        ));
        let file = File::create(&path).unwrap();
        file.set_len(MAX_CUSTOM_DANMAKU_FONT_BYTES + 1).unwrap();

        let error = load_font_from_path(&path, 0).unwrap_err();

        assert_eq!(error.reason, "file_too_large");
        assert_eq!(error.bytes, MAX_CUSTOM_DANMAKU_FONT_BYTES + 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn custom_font_rejects_non_regular_file_from_metadata() {
        let error = load_font_from_path(std::env::temp_dir().as_path(), 0).unwrap_err();

        assert_eq!(error.reason, "not_regular_file");
    }

    #[test]
    fn custom_font_reports_invalid_font_structure() {
        let path = std::env::temp_dir().join(format!(
            "erika_danmaku_invalid_font_{}.ttf",
            std::process::id()
        ));
        std::fs::write(&path, b"not a font").unwrap();

        let error = load_font_from_path(&path, 0).unwrap_err();

        assert_eq!(error.reason, "invalid_font");
        assert_eq!(error.bytes, 10);
        assert!(!error.detail.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_custom_font_keeps_default_fallback() {
        let path = std::env::temp_dir().join(format!(
            "erika_danmaku_fallback_font_{}.ttf",
            std::process::id()
        ));
        std::fs::write(&path, b"not a font").unwrap();

        let font = load_configured_font("", path.to_str().unwrap(), 0);

        assert!(font.is_some());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn custom_font_path_uses_configured_collection_face_index() {
        let path = std::env::temp_dir().join(format!(
            "erika_danmaku_collection_font_{}.ttc",
            std::process::id()
        ));
        std::fs::write(&path, test_ttc()).unwrap();

        let first = load_font_from_path(&path, 0).unwrap();
        let second = load_font_from_path(&path, 1).unwrap();

        assert_ne!(first.units_per_em(), second.units_per_em());
        assert_eq!(second.units_per_em(), Some(512.0));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn fontdb_face_loader_uses_queried_collection_face_index() {
        let mut db = fontdb::Database::new();
        db.load_font_data(test_ttc());
        let second_id = db.faces().find(|face| face.index == 1).unwrap().id;

        let second = load_fontdb_face(&db, second_id).unwrap();

        assert_eq!(second.units_per_em(), Some(512.0));
    }

    #[test]
    fn selected_collection_faces_share_the_attachment_allocation() {
        let attachment = selected_font(test_ttc());
        let fonts = load_selected_fonts(std::slice::from_ref(&attachment), 1);

        assert_eq!(fonts.len(), 2);
        assert_eq!(fonts[0].font.font_data().as_ptr(), attachment.data.as_ptr());
        assert_eq!(fonts[1].font.font_data().as_ptr(), attachment.data.as_ptr());
    }

    #[test]
    fn fallback_miss_loads_no_faces() {
        let mut db = fontdb::Database::new();
        db.load_font_data(test_ttc());
        db.load_font_data(NIPAPLAY_FALLBACK_FONT.to_vec());
        let mut fallback = SystemFontFallback::with_database(db, 1);

        assert!(fallback.resolve('\u{1F389}').is_none());

        assert!(
            fallback.loaded.is_empty(),
            "a character no face covers must not load any font file"
        );
    }

    #[test]
    fn fallback_hit_loads_only_covering_face() {
        let mut db = fontdb::Database::new();
        db.load_font_data(test_ttc());
        let mut fallback = SystemFontFallback::with_database(db, 1);

        let face = fallback.resolve('A').unwrap();

        assert!(face.has_glyph('A'));
        let loaded = fallback
            .loaded
            .values()
            .filter_map(|face| face.as_ref())
            .count();
        assert_eq!(loaded, 1);
    }

    #[test]
    fn fallback_lru_evicts_least_recently_used_face() {
        let mut db = fontdb::Database::new();
        db.load_font_data(test_ttc());
        db.load_font_data(NIPAPLAY_FALLBACK_FONT.to_vec());
        let mut fallback = SystemFontFallback::with_database(db, 1);
        fallback.resident_cap = 2;
        let (a, b, c) = (fallback.faces[0], fallback.faces[1], fallback.faces[2]);

        fallback.load_face(a).unwrap();
        fallback.load_face(b).unwrap();
        fallback.load_face(a).unwrap(); // touch: a becomes most recently used
        fallback.load_face(c).unwrap(); // evicts b, not a

        assert!(!fallback.loaded.contains_key(&b));
        assert!(fallback.loaded.get(&a).unwrap().is_some());
        assert!(fallback.loaded.get(&c).unwrap().is_some());
    }

    #[test]
    fn fallback_evicted_face_reuses_its_stable_id() {
        let mut db = fontdb::Database::new();
        db.load_font_data(test_ttc());
        let mut fallback = SystemFontFallback::with_database(db, 1);
        fallback.resident_cap = 1;
        let (a, b) = (fallback.faces[0], fallback.faces[1]);

        let first = fallback.load_face(a).unwrap();
        let second = fallback.load_face(b).unwrap();
        assert_ne!(first.id, second.id);

        let reloaded = fallback.load_face(a).unwrap();

        assert_eq!(
            reloaded.id, first.id,
            "a reloaded face must reuse its id because glyph caches key on it"
        );
    }

    #[test]
    fn fallback_resolve_recovers_after_eviction() {
        let mut db = fontdb::Database::new();
        db.load_font_data(test_ttc());
        let mut fallback = SystemFontFallback::with_database(db, 1);
        fallback.resident_cap = 1;

        let first = fallback.resolve('A').unwrap();
        fallback.load_face(fallback.faces[1]).unwrap(); // evicts the resolved face

        let again = fallback.resolve('A').unwrap();

        assert_eq!(again.id, first.id);
    }

    #[test]
    fn fallback_keeps_visible_generic_glyphs_ahead_of_emoji_fonts() {
        // Keep cmap coverage and metrics, but remove the outlines. This models
        // a color font the outline rasterizer cannot paint, without depending
        // on any platform's installed fonts.
        let mut blank_data = NIPAPLAY_FALLBACK_FONT.to_vec();
        let table_count = u16::from_be_bytes(blank_data[4..6].try_into().unwrap()) as usize;
        let record = (0..table_count)
            .map(|index| 12 + index * 16)
            .find(|&record| &blank_data[record..record + 4] == b"glyf")
            .unwrap();
        let offset =
            u32::from_be_bytes(blank_data[record + 8..record + 12].try_into().unwrap()) as usize;
        let length =
            u32::from_be_bytes(blank_data[record + 12..record + 16].try_into().unwrap()) as usize;
        blank_data[offset..offset + length].fill(0);

        let mut template_db = fontdb::Database::new();
        template_db.load_font_data(NIPAPLAY_FALLBACK_FONT.to_vec());
        let template = template_db.faces().next().unwrap().clone();
        for family in ["Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji"] {
            let mut db = fontdb::Database::new();
            let mut emoji = template.clone();
            emoji.id = fontdb::ID::dummy();
            emoji.families = vec![(family.into(), fontdb::Language::English_UnitedStates)];
            emoji.source = fontdb::Source::Binary(Arc::new(blank_data.clone()));
            // Put the blank face first in database order too: the generic
            // fallback must retain its existing priority over it.
            let emoji_id = db.push_face_info(emoji);
            let mut generic = template.clone();
            generic.id = fontdb::ID::dummy();
            generic.families = vec![(
                "Regression Sans".into(),
                fontdb::Language::English_UnitedStates,
            )];
            db.push_face_info(generic);
            db.set_sans_serif_family("Regression Sans");
            db.set_serif_family("Regression Sans");
            let mut fallback = SystemFontFallback::with_database(db, 1);

            assert!(fallback.face_covers(emoji_id, 'A'));
            let blank = fallback.load_face(emoji_id).unwrap();
            let key = |face: &DanmakuFontFace| GlyphCacheKey::new(face.id, 'A', 32.0, 1.0, false);
            assert!(
                rasterize_font_glyph(blank.font.as_ref(), key(&blank))
                    .fill_alpha
                    .iter()
                    .all(|&pixel| pixel == 0),
                "the competing emoji fixture must be invisible"
            );

            let selected = fallback.resolve('A').unwrap();
            assert!(
                rasterize_font_glyph(selected.font.as_ref(), key(&selected))
                    .fill_alpha
                    .iter()
                    .any(|&pixel| pixel > 0),
                "{family} must not hide a visible generic fallback glyph"
            );
        }
    }

    #[test]
    fn fallback_cap_zero_disables_caching() {
        let mut db = fontdb::Database::new();
        db.load_font_data(test_ttc());
        let mut fallback = SystemFontFallback::with_database(db, 1);
        fallback.resident_cap = 0;

        let first = fallback.resolve('A').unwrap();
        let again = fallback.resolve('A').unwrap();

        assert_eq!(again.id, first.id);
        assert!(fallback.loaded.is_empty());
        assert!(fallback.resident.is_empty());
    }
}
