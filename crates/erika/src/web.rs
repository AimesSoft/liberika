//! Browser bindings. The Web package owns WebCodecs, libass and the GPU surface;
//! text layout and rasterization use the same implementation as native players.
use crate::danmaku::{
    DanmakuColor, DanmakuFontSelection, DanmakuItem, DanmakuLayoutConfig, DanmakuMode,
    DanmakuTimeline, DanmakuViewport, DfmLayoutEngine, DfmPreparedLayout,
};
use crate::font::SubtitleFontAttachment;
use js_sys::{Float32Array, Object, Reflect, Uint8Array};
use std::{sync::Arc, time::Duration};
use wasm_bindgen::prelude::*;

/// Share the font already embedded for danmaku with the libass WASM module.
/// Return an owned JS array so later Rust memory growth cannot invalidate it.
#[wasm_bindgen(js_name = fallbackFont)]
pub fn fallback_font() -> Uint8Array {
    Uint8Array::from(crate::NIPAPLAY_FALLBACK_FONT)
}

#[wasm_bindgen]
pub struct WebDanmaku {
    timeline: DanmakuTimeline,
    engine: DfmLayoutEngine,
    prepared: Option<DfmPreparedLayout>,
    config: DanmakuLayoutConfig,
    window: f64,
    viewport: Option<DanmakuViewport>,
    pending_size_at: Option<f64>,
    atlas_version: u64,
    atlas_size: (u32, u32, usize),
    size_deadline: Option<f64>,
    fonts: Vec<SubtitleFontAttachment>,
    font_generation: u64,
}

fn set(object: &Object, key: &str, value: JsValue) -> Result<(), JsValue> {
    Reflect::set(object, &key.into(), &value).map(|_| ())
}

#[wasm_bindgen]
impl WebDanmaku {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        let config = DanmakuLayoutConfig::default();
        Self {
            timeline: DanmakuTimeline::default(),
            engine: DfmLayoutEngine::new(DanmakuTimeline::default(), config.clone()),
            config,
            prepared: None,
            window: -100.0,
            viewport: None,
            pending_size_at: None,
            atlas_version: 0,
            atlas_size: (0, 0, 0),
            size_deadline: None,
            fonts: Vec::new(),
            font_generation: 0,
        }
    }
    pub fn load(&mut self, text: &str) -> Result<usize, JsValue> {
        self.timeline =
            DanmakuTimeline::parse_auto(text).map_err(|e| JsValue::from_str(&e.to_string()))?;
        self.prepared = None;
        self.engine.invalidate_placement_history();
        Ok(self.timeline.len())
    }
    /// Append a track without dropping the already mounted comments.
    pub fn append(&mut self, text: &str) -> Result<usize, JsValue> {
        let track =
            DanmakuTimeline::parse_auto(text).map_err(|e| JsValue::from_str(&e.to_string()))?;
        let count = track.len();
        let mut items = self.timeline.items().to_vec();
        let base = items.iter().map(|i| i.id).max().unwrap_or(0);
        items.extend(
            track
                .items()
                .iter()
                .cloned()
                .enumerate()
                .map(|(n, mut item)| {
                    item.id = base + n as u64 + 1;
                    item
                }),
        );
        self.timeline =
            DanmakuTimeline::new(items).map_err(|e| JsValue::from_str(&e.to_string()))?;
        self.prepared = None;
        Ok(count)
    }
    /// Show the viewer's own comment, bordered, in `color` (`0xRRGGBB`;
    /// white when omitted).
    pub fn send(&mut self, text: &str, seconds: f64, color: Option<u32>) -> Result<(), JsValue> {
        if !seconds.is_finite() || text.trim().is_empty() {
            return Ok(());
        }
        let color = color.map_or(DanmakuColor::WHITE, |rgb| {
            DanmakuColor::rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
        });
        let mut items = self.timeline.items().to_vec();
        items.push(DanmakuItem {
            id: items.iter().map(|i| i.id).max().unwrap_or(0) + 1,
            pts: Duration::from_secs_f64(seconds.max(0.0)),
            text: text.into(),
            mode: DanmakuMode::Scroll,
            font_size: 25.0,
            color,
            opacity: 1.0,
            is_self: true,
        });
        self.timeline =
            DanmakuTimeline::new(items).map_err(|e| JsValue::from_str(&e.to_string()))?;
        self.prepared = None;
        Ok(())
    }
    pub fn add_font(&mut self, name: &str, data: &[u8]) -> Result<(), JsValue> {
        if data.len() > crate::font::MAX_MEMORY_SUBTITLE_FONT_BYTES {
            return Err("Font exceeds 32 MiB".into());
        }
        if self.fonts.iter().map(|f| f.data.len()).sum::<usize>() + data.len()
            > crate::font::MAX_MEMORY_SUBTITLE_FONT_TOTAL_BYTES
        {
            return Err("Fonts exceed 128 MiB".into());
        }
        self.fonts.push(SubtitleFontAttachment::new(
            name,
            None,
            Vec::new(),
            Arc::<[u8]>::from(data),
        ));
        self.font_generation += 1;
        self.engine.set_font_selection(DanmakuFontSelection::new(
            self.font_generation,
            self.fonts.clone(),
        ));
        self.prepared = None;
        Ok(())
    }
    pub fn configure(&mut self, json: &str, now_ms: f64) -> Result<(), JsValue> {
        let v: serde_json::Value =
            serde_json::from_str(json).map_err(|e| JsValue::from_str(&e.to_string()))?;
        let old = self.config.clone();
        for (key, slot) in [
            ("fontSize", &mut self.config.font_size),
            ("opacity", &mut self.config.opacity),
            ("area", &mut self.config.display_area),
            ("duration", &mut self.config.scroll_duration_seconds),
            ("speed", &mut self.config.scroll_speed_factor),
        ] {
            if let Some(n) = v.get(key).and_then(|n| n.as_f64()) {
                if n.is_finite() {
                    *slot = n as f32;
                }
            }
        }
        for (key, slot) in [
            ("enabled", &mut self.config.enabled),
            ("supersampling", &mut self.config.supersampling),
            ("blockTop", &mut self.config.block_top),
            ("blockBottom", &mut self.config.block_bottom),
            ("blockScroll", &mut self.config.block_scroll),
            ("mergeDuplicates", &mut self.config.merge_duplicates),
            ("randomColors", &mut self.config.random_colors),
        ] {
            if let Some(b) = v.get(key).and_then(|b| b.as_bool()) {
                *slot = b;
            }
        }
        self.config.font_size = self.config.font_size.clamp(1.0, 256.0);
        self.config.opacity = self.config.opacity.clamp(0.0, 1.0);
        self.config.scroll_duration_seconds = self.config.scroll_duration_seconds.clamp(1.0, 120.0);
        self.config.scroll_speed_factor = self.config.scroll_speed_factor.clamp(0.1, 10.0);
        if old.font_size != self.config.font_size {
            let deadline = *self.size_deadline.get_or_insert(now_ms + 500.0);
            self.pending_size_at = Some((now_ms + 120.0).min(deadline));
        }
        let mut paint = old.clone();
        paint.font_size = self.config.font_size;
        paint.opacity = self.config.opacity;
        paint.enabled = self.config.enabled;
        paint.random_colors = self.config.random_colors;
        if paint != self.config {
            self.prepared = None;
            self.pending_size_at = None;
        }
        if let Some(p) = &mut self.prepared {
            p.apply_paint_config(&self.config);
        }
        Ok(())
    }
    pub fn reset(&mut self) {
        self.prepared = None;
        self.engine.invalidate_placement_history();
    }
    pub fn frame(
        &mut self,
        seconds: f64,
        width: u32,
        height: u32,
        scale: f32,
        now_ms: f64,
    ) -> Result<JsValue, JsValue> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err("Invalid media time".into());
        }
        let media_time = Duration::try_from_secs_f64(seconds)
            .map_err(|_| JsValue::from_str("Media time exceeds supported range"))?;
        let viewport = DanmakuViewport::with_scale(width.min(8192), height.min(8192), scale);
        let size_due = self
            .pending_size_at
            .is_some_and(|deadline| now_ms >= deadline);
        if self.prepared.is_none()
            || self.viewport != Some(viewport)
            || size_due
            || seconds < self.window
            || seconds > self.window + 5.0
        {
            self.engine.set_config(self.config.clone());
            self.engine.recycle_raster_caches_if_needed();
            let lookback = (self.config.scroll_duration_seconds as f64 * 1.3
                / self.config.scroll_speed_factor as f64)
                .max(4.0);
            let start = media_time.saturating_sub(Duration::from_secs_f64(lookback));
            let end = media_time.saturating_add(Duration::from_secs(20));
            self.engine.sync_timeline(&self.timeline.window(start, end));
            self.prepared = Some(self.engine.prepare(viewport, 1));
            self.window = seconds;
            self.viewport = Some(viewport);
            self.pending_size_at = None;
            self.size_deadline = None;
        }
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| JsValue::from_str("No prepared layout"))?;
        let frame = prepared.frame_layout(media_time, 1);
        let plan = self.engine.rasterizer_clone().render_plan_scaled(
            &frame,
            self.config.font_size / prepared.font_size().max(1.0),
            prepared.atlas(),
        );
        let result = Object::new();
        let mut glyphs = Vec::<f32>::with_capacity(plan.items.len() * 8);
        let mut groups = Vec::<f32>::new();
        let mut last = None;
        for item in &plan.items {
            // A group shares one paint; the border of the viewer's own
            // comment is white beside its colored text.
            let style = (
                item.item_id,
                item.color_rgba,
                item.outline_rgba,
                item.shadow_rgba,
                item.shadow_offset,
            );
            if last != Some(style) {
                groups.extend_from_slice(&[(glyphs.len() / 8) as f32, 0.0]);
                groups.extend_from_slice(&item.color_rgba);
                groups.extend_from_slice(&item.outline_rgba);
                groups.extend_from_slice(&item.shadow_rgba);
                groups.extend_from_slice(&item.shadow_offset);
                last = Some(style);
            }
            let count = groups.len() - 15;
            groups[count] += 1.0;
            glyphs.extend_from_slice(&item.rect);
            glyphs.extend_from_slice(&item.tex_rect);
        }
        set(
            &result,
            "glyphs",
            Float32Array::from(glyphs.as_slice()).into(),
        )?;
        set(
            &result,
            "groups",
            Float32Array::from(groups.as_slice()).into(),
        )?;
        if let Some(atlas) = plan.atlas {
            if atlas.version != self.atlas_version {
                if let Some(update) = atlas.incremental_update_from(
                    self.atlas_version,
                    self.atlas_size.0,
                    self.atlas_size.1,
                    self.atlas_size.2,
                ) {
                    set(&result, "x", update.x.into())?;
                    set(&result, "y", update.y.into())?;
                    set(&result, "width", update.width.into())?;
                    set(&result, "height", update.height.into())?;
                    for (name, data) in [
                        ("fill", &atlas.fill_alpha),
                        ("outline", &atlas.outline_alpha),
                    ] {
                        let mut region =
                            Vec::with_capacity(update.width as usize * update.height as usize);
                        for y in update.y..update.y + update.height {
                            let start = y as usize * atlas.stride + update.x as usize;
                            region.extend_from_slice(&data[start..start + update.width as usize]);
                        }
                        set(&result, name, Uint8Array::from(region.as_slice()).into())?;
                    }
                } else {
                    set(&result, "width", atlas.width.into())?;
                    set(&result, "height", atlas.height.into())?;
                    set(
                        &result,
                        "fill",
                        Uint8Array::from(atlas.fill_alpha.as_slice()).into(),
                    )?;
                    set(
                        &result,
                        "outline",
                        Uint8Array::from(atlas.outline_alpha.as_slice()).into(),
                    )?;
                }
                self.atlas_size = (atlas.width, atlas.height, atlas.stride);
                self.atlas_version = atlas.version;
            }
        }
        Ok(result.into())
    }
}
impl Default for WebDanmaku {
    fn default() -> Self {
        Self::new()
    }
}
