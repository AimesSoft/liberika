//! liberika adapter for the NipaPlay DFM+ layout core.
//!
//! This module keeps the DFM+ prepare/frame-query contract intact while removing
//! the Flutter Rust Bridge handle store. The input is prepared danmaku content,
//! viewport, and user config; the output is a prepared layout plus per-media-time
//! frame positions that liberika maps into its native GPU render plan.
//!
//! Bilibili mode 7 ("advanced") comments carry their own path, duration and
//! alpha ramp in [`DanmakuSpecial`]. They bypass collision tracks: the frame
//! query interpolates their position directly instead of using a track's Y.

use rustc_hash::{FxHashMap, FxHasher};

use std::collections::HashMap;

use super::DanmakuSpecial;
use super::dfm_core::{
    filters::{FilterContext, FilterSystem},
    model::{DanmakuItem, DanmakuType, Duration, GlobalFlags},
    retainer::DanmakuRetainer,
};
use super::outline::resolve_width_px;
use std::hash::{Hash, Hasher};

const STATIC_DURATION_MS: i64 = 3800;

/// A prepared item plus the mode 7 animation data attached to it.
type PrepareEntry = (u64, bool, f32, u32, DanmakuItem, Option<DanmakuSpecial>);

#[derive(Debug, Clone)]
pub struct PrepareItem {
    pub source_id: u64,
    pub time_seconds: f64,
    pub text: String,
    pub type_code: i32,
    pub color_argb: i32,
    pub opacity: f32,
    pub is_me: bool,
    pub font_size: f32,
    pub paint_width: f64,
    pub paint_height: f64,
    /// Present only for Bilibili mode 7. Coordinates are already scaled to
    /// the viewport by `crate::danmaku::prepare_layout`.
    pub special: Option<DanmakuSpecial>,
}

#[derive(Debug, Clone)]
pub struct PrepareRequest<'a> {
    pub items: Vec<PrepareItem>,
    pub width: f64,
    pub height: f64,
    pub font_size: f64,
    pub display_area: f64,
    pub scroll_duration_seconds: f64,
    pub allow_stacking: bool,
    pub allow_scroll_overwrite: bool,
    pub merge_danmaku: bool,
    pub max_quantity: Option<u32>,
    pub max_lines_per_type: Option<u32>,
    pub track_gap_ratio: f64,
    pub outline_width: f64,
    pub preferred_tracks: &'a HashMap<u64, usize>,
    pub block_words: Vec<String>,
    pub block_top: bool,
    pub block_bottom: bool,
    pub block_scroll: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedLayout {
    pub width: f64,
    pub height: f64,
    pub scroll_duration_seconds: f64,
    pub static_duration_seconds: f64,
    /// Longest lifetime of any item, including mode 7 comments whose duration
    /// can exceed both the scroll and the static duration. Frame queries use
    /// this as their look-behind window.
    pub max_duration_seconds: f64,
    pub items: Vec<PreparedItem>,
    pub item_times: Vec<f64>,
    pub track_count: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedItem {
    pub source_id: u64,
    pub time_seconds: f64,
    pub text: String,
    pub type_code: i32,
    pub color_argb: i32,
    pub opacity: f32,
    pub is_me: bool,
    pub font_size: f64,
    pub font_size_multiplier: f64,
    pub count_text: Option<String>,
    pub duplicate_count: u32,
    pub track_index: i32,
    pub y_position: f64,
    pub width: f64,
    pub height: f64,
    pub scroll_speed: f64,
    pub is_filtered: bool,
    pub duration_seconds: f64,
    pub is_scroll: bool,
    pub centered_x: f64,
    /// Mode 7 animation data, already scaled to the viewport.
    pub special: Option<DanmakuSpecial>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrameItem {
    pub item_index: usize,
    pub track_index: i32,
    pub x: f64,
    pub y: f64,
    pub offstage_x: f64,
    /// Per-frame alpha multiplier: the mode 7 alpha ramp, or 1.0 otherwise.
    pub alpha: f32,
}

pub fn prepare_layout(request: PrepareRequest) -> Result<PreparedLayout, String> {
    let width = request.width.max(1.0) as f32;
    let height = request.height.max(1.0) as f32;
    let font_size = request.font_size.max(1.0) as f32;
    let display_area = request.display_area.clamp(0.1, 1.0) as f32;
    let scroll_dur_secs = request.scroll_duration_seconds.max(1.0);
    let scroll_dur_ms = (scroll_dur_secs * 1000.0) as i64;
    let global_flags = GlobalFlags::default();
    let outline_width = request.outline_width.max(0.0) as f32;
    let outline_px = resolve_width_px(font_size, outline_width);

    let mut items = request
        .items
        .into_iter()
        .enumerate()
        .map(|(i, raw)| {
            let danmaku_type = DanmakuType::from_code(raw.type_code);
            let dur_ms = if danmaku_type.is_scroll() {
                scroll_dur_ms
            } else if let Some(special) = &raw.special {
                (special.duration_seconds.max(0.1) * 1000.0) as i64
            } else {
                STATIC_DURATION_MS
            };
            let mut item = DanmakuItem::new(
                (raw.time_seconds * 1000.0) as i64,
                raw.text,
                raw.color_argb as u32,
                raw.font_size.max(1.0),
                danmaku_type,
                dur_ms,
            );
            item.index = i as u32;
            item.alpha = (raw.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
            // A bare Special type without its JSON payload has nothing to draw
            // at any time. Keep the historical "unsupported" behaviour.
            if danmaku_type == DanmakuType::Special && raw.special.is_none() {
                item.is_filtered = true;
                item.filter_param = 99;
            }
            if raw.paint_width > 0.0 && raw.paint_height > 0.0 {
                item.paint_width = raw.paint_width as f32 + outline_px * 2.0;
                item.paint_height = raw.paint_height as f32;
                if item.danmaku_type.is_scroll() {
                    let distance = width + item.paint_width;
                    item.step_x = distance / item.duration_ms as f32;
                }
                item.flags.measure = global_flags.measure_flag;
            }
            (
                raw.source_id,
                raw.is_me,
                raw.font_size.max(1.0),
                1u32,
                item,
                raw.special,
            )
        })
        .collect::<Vec<PrepareEntry>>();

    for (_, _, _, _, item, _) in &mut items {
        item.measure_with_outline(width, height, &global_flags, outline_px);
    }

    if request.merge_danmaku {
        merge_duplicate_items(&mut items);
    }

    let mut filter_sys = FilterSystem::default();
    if let Some(q) = request.max_quantity {
        filter_sys.max_quantity = Some(q);
    }
    if let Some(max) = request.max_lines_per_type {
        for ty in [
            DanmakuType::ScrollRL,
            DanmakuType::ScrollLR,
            DanmakuType::FixTop,
            DanmakuType::FixBottom,
        ] {
            filter_sys.max_lines.insert(ty, max);
        }
    }
    if request.block_scroll {
        filter_sys.blocked_types.insert(DanmakuType::ScrollRL);
        filter_sys.blocked_types.insert(DanmakuType::ScrollLR);
    }
    if request.block_top {
        filter_sys.blocked_types.insert(DanmakuType::FixTop);
    }
    if request.block_bottom {
        filter_sys.blocked_types.insert(DanmakuType::FixBottom);
    }
    filter_sys.duplicate_merge = request.merge_danmaku;
    filter_sys.set_block_words(&request.block_words);

    let scroll_duration = Duration::new(scroll_dur_ms);
    let mut ctx = FilterContext {
        timer_ms: 0,
        index_in_screen: 0,
        screen_size: items.len(),
        frame_elapsed_ms: 0,
        global_flags,
        scroll_duration,
    };
    for (i, (_, _, _, _, item, _)) in items.iter_mut().enumerate() {
        if item.is_filtered {
            continue;
        }
        ctx.index_in_screen = i;
        filter_sys.filter_primary(item, &ctx);
    }

    let track_gap_ratio = request.track_gap_ratio.clamp(0.0, 2.0) as f32;
    let mut retainer = DanmakuRetainer::new(2.0, track_gap_ratio);
    let type_order = [
        DanmakuType::ScrollRL,
        DanmakuType::ScrollLR,
        DanmakuType::FixTop,
        DanmakuType::FixBottom,
    ];
    let mut type_indices: [Vec<usize>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for (i, (_, _, _, _, item, _)) in items.iter().enumerate() {
        if item.is_filtered {
            continue;
        }
        match item.danmaku_type {
            DanmakuType::ScrollRL => type_indices[0].push(i),
            DanmakuType::ScrollLR => type_indices[1].push(i),
            DanmakuType::FixTop => type_indices[2].push(i),
            DanmakuType::FixBottom => type_indices[3].push(i),
            // Mode 7 owns its coordinates; it never joins a collision track.
            DanmakuType::Special => {}
        }
    }

    for (type_idx, _) in type_order.iter().enumerate() {
        for &i in &type_indices[type_idx] {
            let is_me = items[i].1;
            let preferred_track = request.preferred_tracks.get(&items[i].0).copied();
            let (placed, displaced_index) = retainer.fix_with_options_and_preferred_track(
                &mut items[i].4,
                width,
                height,
                &global_flags,
                display_area,
                is_me,
                request.allow_stacking,
                request.allow_scroll_overwrite,
                preferred_track,
            );
            if !placed {
                items[i].4.is_filtered = true;
                items[i].4.filter_param = 99;
                continue;
            }
            if exceeds_max_line(
                &items[i].4,
                request.max_lines_per_type,
                track_gap_ratio,
                height,
                display_area,
            ) {
                items[i].4.is_filtered = true;
                items[i].4.filter_param = 99;
                continue;
            }
            for &displaced in &displaced_index {
                if displaced < items.len() && !items[displaced].4.is_filtered {
                    items[displaced].4.is_filtered = true;
                    items[displaced].4.filter_param = 99;
                }
            }
        }
    }

    let mut prepared_items = Vec::with_capacity(items.len());
    for (source_id, is_me, raw_font_size, duplicate_count, item, special) in &mut items {
        if item.is_filtered {
            continue;
        }
        let type_code = item.danmaku_type as i32;
        let is_scroll = item.danmaku_type.is_scroll();
        let centered_x = if is_scroll {
            0.0
        } else {
            (width as f64 - item.paint_width as f64) / 2.0
        };
        prepared_items.push(PreparedItem {
            source_id: *source_id,
            time_seconds: item.time_ms as f64 / 1000.0,
            text: std::mem::take(&mut item.text),
            type_code,
            color_argb: item.text_color as i32,
            opacity: item.alpha as f32 / 255.0,
            is_me: *is_me,
            font_size: f64::from(*raw_font_size),
            font_size_multiplier: 1.0,
            count_text: None,
            duplicate_count: *duplicate_count,
            track_index: track_index_from_y(item.y, item.paint_height, track_gap_ratio),
            y_position: item.y as f64,
            width: item.paint_width as f64,
            height: item.paint_height as f64,
            scroll_speed: item.step_x as f64 * 1000.0,
            is_filtered: item.is_filtered,
            duration_seconds: item.duration_ms as f64 / 1000.0,
            is_scroll,
            centered_x,
            special: special.take(),
        });
    }

    prepared_items.sort_by(|a, b| {
        a.time_seconds
            .partial_cmp(&b.time_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let item_times = prepared_items.iter().map(|i| i.time_seconds).collect();
    let static_dur_secs = STATIC_DURATION_MS as f64 / 1000.0;
    let max_duration_seconds = prepared_items
        .iter()
        .fold(scroll_dur_secs.max(static_dur_secs), |max, item| {
            max.max(item.duration_seconds)
        });
    Ok(PreparedLayout {
        width: width as f64,
        height: height as f64,
        scroll_duration_seconds: scroll_dur_secs,
        static_duration_seconds: static_dur_secs,
        max_duration_seconds,
        items: prepared_items,
        item_times,
        track_count: ((height * display_area) / (font_size * 1.2 * 1.25)).max(1.0) as i32,
    })
}

fn track_index_from_y(y: f32, height: f32, track_gap_ratio: f32) -> i32 {
    if !y.is_finite() || y < 0.0 {
        return 0;
    }
    let track_height = (height + height * track_gap_ratio).max(1.0);
    ((y - 2.0).max(0.0) / track_height).round().max(0.0) as i32
}

pub fn frame_candidate_count(layout: &PreparedLayout, current_time: f64) -> usize {
    let start_idx = lower_bound(
        &layout.item_times,
        current_time - layout.max_duration_seconds,
    );
    let end_idx = upper_bound(&layout.item_times, current_time);
    end_idx.saturating_sub(start_idx)
}

pub fn for_each_frame_item(
    layout: &PreparedLayout,
    current_time: f64,
    mut visit: impl FnMut(FrameItem),
) {
    let width = layout.width;
    let window_start = current_time - layout.max_duration_seconds;
    let start_idx = lower_bound(&layout.item_times, window_start);
    let end_idx = upper_bound(&layout.item_times, current_time);

    for i in start_idx..end_idx {
        let item = &layout.items[i];
        let elapsed = current_time - item.time_seconds;
        if elapsed < 0.0 {
            continue;
        }
        let is_special = item.special.is_some();
        // Mode 7 uses the [start, end) lifetime of its own animation; other
        // comments keep the inclusive endpoint they have always used.
        let expired = if is_special {
            elapsed >= item.duration_seconds
        } else {
            !item.is_scroll && elapsed > item.duration_seconds
        };
        if expired {
            continue;
        }
        let (x, y, alpha, offstage_x) = if item.is_scroll {
            let speed = item.scroll_speed;
            if item.type_code == DanmakuType::ScrollLR as i32 {
                (
                    speed * elapsed - item.width,
                    item.y_position,
                    1.0,
                    -item.width,
                )
            } else {
                (
                    width - speed * elapsed,
                    item.y_position,
                    1.0,
                    width + item.width,
                )
            }
        } else if let Some(special) = &item.special {
            let duration = special.duration_seconds.max(0.001);
            let progress = (elapsed / duration).clamp(0.0, 1.0);
            let x = special.x1 + (special.x2 - special.x1) * progress;
            let y = special.y1 + (special.y2 - special.y1) * progress;
            let alpha = (special.alpha_from
                + (special.alpha_to - special.alpha_from) * progress as f32)
                .clamp(0.0, 1.0);
            (x, y, alpha, width)
        } else {
            (item.centered_x, item.y_position, 1.0, width)
        };
        if item.is_scroll && x < -item.width {
            continue;
        }
        if y < 0.0 {
            continue;
        }
        visit(FrameItem {
            item_index: i,
            track_index: item.track_index,
            x,
            y,
            offstage_x,
            alpha,
        });
    }
}

fn merge_duplicate_items(items: &mut [PrepareEntry]) {
    let mut merge_map: FxHashMap<u64, (usize, u32)> = FxHashMap::default();
    let mut dup_indices = Vec::new();
    for (i, (_, _, _, _, item, special)) in items.iter().enumerate() {
        // Mode 7 comments can share the same text while animating along
        // different paths; merging by text would silently drop animations.
        if special.is_some() {
            continue;
        }
        let text_hash = fxhash_str(&item.text);
        match merge_map.get_mut(&text_hash) {
            Some((first_idx, count)) => {
                if item.text == items[*first_idx].4.text {
                    dup_indices.push(i);
                    *count += 1;
                }
            }
            None => {
                merge_map.insert(text_hash, (i, 1));
            }
        }
    }
    for idx in dup_indices {
        items[idx].4.is_filtered = true;
        items[idx].4.filter_param = 99;
    }
    for &(first_idx, count) in merge_map.values() {
        if count > 1 {
            items[first_idx].3 = count;
            items[first_idx].4.text.push_str(&format!(" x{count}"));
        }
    }
}

fn fxhash_str(s: &str) -> u64 {
    let mut hasher = FxHasher::default();
    s.hash(&mut hasher);
    hasher.finish()
}

fn exceeds_max_line(
    item: &DanmakuItem,
    max_lines_per_type: Option<u32>,
    track_gap_ratio: f32,
    height: f32,
    display_area: f32,
) -> bool {
    let Some(max_lines) = max_lines_per_type else {
        return false;
    };
    if max_lines == 0 {
        return true;
    }
    let track_height = (item.paint_height + item.paint_height * track_gap_ratio).max(1.0);
    let row = match item.danmaku_type {
        DanmakuType::FixBottom => {
            let effective_height = height * display_area;
            ((effective_height - item.y) / track_height).ceil().max(1.0) as u32 - 1
        }
        _ => ((item.y - 2.0) / track_height).floor().max(0.0) as u32,
    };
    row >= max_lines
}

fn lower_bound(times: &[f64], target: f64) -> usize {
    times.partition_point(|&time| time < target)
}

fn upper_bound(times: &[f64], target: f64) -> usize {
    times.partition_point(|&time| time <= target)
}
