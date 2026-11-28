//! Glyphs that carry their own colors: emoji.
//!
//! Text is one color and takes it from the comment. These are pictures, drawn as
//! they are. Fonts hold them as PNG images (`sbix`, `CBDT`) or as colored vector
//! layers (`COLR`); a browser page has the browser draw them, and Apple's system
//! draws the images only it can read.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// A picture for one glyph, with premultiplied alpha, measured in ems.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColorBitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub left: f32,
    pub em_width: f32,
    pub em_height: f32,
    pub advance: f32,
}

/// Pictures are kept about this tall, and drawn at any size from there.
const PICTURE_HEIGHT: u32 = 96;

/// The private plane that stands for emoji sequences; see [`fold`].
const SEQUENCE_BASE: u32 = 0xF_0000;
const SEQUENCE_LIMIT: u32 = 0xF_FFFD;

/// Emoji sequences met so far: a flag is two characters and a family up to
/// seven, while layout goes character by character.
#[derive(Default)]
struct Sequences {
    texts: Vec<Arc<str>>,
    known: HashMap<Arc<str>, char>,
}

fn sequences() -> &'static Mutex<Sequences> {
    static SEQUENCES: OnceLock<Mutex<Sequences>> = OnceLock::new();
    SEQUENCES.get_or_init(Default::default)
}

fn intern(text: &str) -> Option<char> {
    let mut sequences = sequences().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ch) = sequences.known.get(text) {
        return Some(*ch);
    }
    let ch = char::from_u32(SEQUENCE_BASE + sequences.texts.len() as u32)
        .filter(|ch| *ch as u32 <= SEQUENCE_LIMIT)?;
    let text: Arc<str> = Arc::from(text);
    sequences.texts.push(Arc::clone(&text));
    sequences.known.insert(text, ch);
    Some(ch)
}

/// The sequence `ch` stands for, when it stands for one.
fn sequence_of(ch: char) -> Option<Arc<str>> {
    let index = (ch as u32).checked_sub(SEQUENCE_BASE)? as usize;
    let sequences = sequences().lock().unwrap_or_else(|e| e.into_inner());
    sequences.texts.get(index).cloned()
}

/// The character a font must cover for `ch` to be worth shaping.
pub(crate) fn first_char(ch: char) -> char {
    sequence_of(ch)
        .and_then(|text| text.chars().next())
        .unwrap_or(ch)
}

pub(crate) fn is_sequence(ch: char) -> bool {
    (SEQUENCE_BASE..=SEQUENCE_LIMIT).contains(&(ch as u32))
}

fn is_regional(ch: char) -> bool {
    matches!(ch as u32, 0x1F1E6..=0x1F1FF)
}

/// Where the emoji sequence that begins at `start` ends.
fn sequence_end(chars: &[char], start: usize) -> usize {
    let first = chars[start];
    let next = chars.get(start + 1).copied();
    if is_regional(first) {
        return if next.is_some_and(is_regional) {
            start + 2
        } else {
            start + 1
        };
    }
    if matches!(first, '0'..='9' | '#' | '*') {
        return match (next, chars.get(start + 2)) {
            (Some('\u{FE0F}'), Some('\u{20E3}')) => start + 3,
            (Some('\u{20E3}'), _) => start + 2,
            _ => start + 1,
        };
    }
    if !is_emoji(first) {
        return start + 1;
    }
    let mut end = start + 1;
    while let Some(ch) = chars.get(end).copied() {
        if ch == '\u{200D}' && chars.get(end + 1).copied().is_some_and(is_emoji) {
            end += 2;
        } else if ch != '\u{200D}' && is_emoji_modifier(ch) {
            end += 1;
        } else {
            break;
        }
    }
    end
}

/// `text` with each emoji sequence as one character, which layout can treat as
/// any other. Without a way to shape sequences they stay apart.
pub(crate) fn fold(text: &str) -> Cow<'_, str> {
    if !CAN_SHAPE
        || !text
            .chars()
            .any(|ch| is_regional(ch) || is_emoji_modifier(ch))
    {
        return Cow::Borrowed(text);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut folded = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let end = sequence_end(&chars, index);
        let sequence: String = chars[index..end].iter().collect();
        match (end - index > 1).then(|| intern(&sequence)).flatten() {
            Some(ch) => folded.push(ch),
            None => folded.push_str(&sequence),
        }
        index = end;
    }
    Cow::Owned(folded)
}

/// HarfBuzz comes with libass; a browser shapes for itself.
const CAN_SHAPE: bool = cfg!(any(feature = "libass", target_arch = "wasm32"));

/// The glyph of `ch` in `font`, which for a sequence takes shaping.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn glyph_id(font: &ab_glyph::FontArc, ch: char) -> ab_glyph::GlyphId {
    use ab_glyph::Font;
    match sequence_of(ch) {
        Some(text) => shaped(font, &text)
            .and_then(|glyphs| glyphs.first().copied())
            .unwrap_or(ab_glyph::GlyphId(0)),
        None => font.glyph_id(ch),
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn glyph_id(font: &ab_glyph::FontArc, ch: char) -> ab_glyph::GlyphId {
    use ab_glyph::Font;
    font.glyph_id(ch)
}

/// Whether `font` draws sequence `ch` as one picture, as a font with its flags
/// does; one that lacks them gives a glyph for each part.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn draws_as_one(font: &ab_glyph::FontArc, ch: char) -> bool {
    sequence_of(ch)
        .and_then(|text| shaped(font, &text))
        .is_some_and(|glyphs| glyphs.len() == 1 && glyphs[0].0 != 0)
}

#[cfg(all(feature = "libass", not(target_arch = "wasm32")))]
fn shaped(font: &ab_glyph::FontArc, text: &str) -> Option<Vec<ab_glyph::GlyphId>> {
    use ab_glyph::Font;
    use harfbuzz::*;
    use std::ffi::{c_int, c_uint};
    let data = font.font_data();
    let length = c_uint::try_from(data.len()).ok()?;
    let text_length = c_int::try_from(text.len()).ok()?;
    // SAFETY: the blob borrows `data` read-only, and it and everything built on
    // it are destroyed before this returns.
    unsafe {
        let blob = hb_blob_create(
            data.as_ptr().cast(),
            length,
            MEMORY_MODE_READONLY,
            std::ptr::null_mut(),
            None,
        );
        let face = hb_face_create(blob, 0);
        let hb_font = hb_font_create(face);
        let buffer = hb_buffer_create();
        hb_buffer_add_utf8(buffer, text.as_ptr().cast(), text_length, 0, text_length);
        hb_buffer_guess_segment_properties(buffer);
        hb_shape(hb_font, buffer, std::ptr::null(), 0);
        let mut count = 0;
        let infos = hb_buffer_get_glyph_infos(buffer, &mut count);
        let glyphs = std::slice::from_raw_parts(infos, count as usize)
            .iter()
            .map(|info| ab_glyph::GlyphId(info.codepoint as u16))
            .collect();
        hb_buffer_destroy(buffer);
        hb_font_destroy(hb_font);
        hb_face_destroy(face);
        hb_blob_destroy(blob);
        Some(glyphs)
    }
}

#[cfg(all(not(feature = "libass"), not(target_arch = "wasm32")))]
fn shaped(_font: &ab_glyph::FontArc, _text: &str) -> Option<Vec<ab_glyph::GlyphId>> {
    None
}

#[cfg(all(feature = "libass", not(target_arch = "wasm32")))]
#[allow(non_camel_case_types)]
mod harfbuzz {
    use std::ffi::{c_char, c_int, c_uint, c_void};

    pub type hb_blob_t = c_void;
    pub type hb_face_t = c_void;
    pub type hb_font_t = c_void;
    pub type hb_buffer_t = c_void;
    pub const MEMORY_MODE_READONLY: c_int = 1;

    #[repr(C)]
    pub struct hb_glyph_info_t {
        pub codepoint: u32,
        mask: u32,
        cluster: u32,
        var1: u32,
        var2: u32,
    }

    unsafe extern "C" {
        pub fn hb_blob_create(
            data: *const c_char,
            length: c_uint,
            mode: c_int,
            user_data: *mut c_void,
            destroy: Option<unsafe extern "C" fn(*mut c_void)>,
        ) -> *mut hb_blob_t;
        pub fn hb_blob_destroy(blob: *mut hb_blob_t);
        pub fn hb_face_create(blob: *mut hb_blob_t, index: c_uint) -> *mut hb_face_t;
        pub fn hb_face_destroy(face: *mut hb_face_t);
        pub fn hb_font_create(face: *mut hb_face_t) -> *mut hb_font_t;
        pub fn hb_font_destroy(font: *mut hb_font_t);
        pub fn hb_buffer_create() -> *mut hb_buffer_t;
        pub fn hb_buffer_destroy(buffer: *mut hb_buffer_t);
        pub fn hb_buffer_add_utf8(
            buffer: *mut hb_buffer_t,
            text: *const c_char,
            text_length: c_int,
            item_offset: c_uint,
            item_length: c_int,
        );
        pub fn hb_buffer_guess_segment_properties(buffer: *mut hb_buffer_t);
        pub fn hb_shape(
            font: *mut hb_font_t,
            buffer: *mut hb_buffer_t,
            features: *const c_void,
            num_features: c_uint,
        );
        pub fn hb_buffer_get_glyph_infos(
            buffer: *mut hb_buffer_t,
            length: *mut c_uint,
        ) -> *const hb_glyph_info_t;
    }
}

/// Whether `ch` is drawn as a picture where a font has one. Everything here
/// defaults to emoji presentation or is mostly used that way.
pub(crate) fn is_emoji(ch: char) -> bool {
    is_sequence(ch)
        || matches!(
            ch as u32,
            0x1F000..=0x1FAFF
                | 0x2600..=0x27BF
                | 0x2B00..=0x2BFF
                | 0x231A..=0x231B
                | 0x23E9..=0x23F3
                | 0x25FD..=0x25FE
                | 0x2934..=0x2935
                | 0x3030
                | 0x303D
                | 0x3297
                | 0x3299
        ) && !is_emoji_modifier(ch)
}

/// Parts of an emoji sequence, which change the emoji before them. Nothing here
/// shapes sequences, so they are left out: the emoji shows in its plain form.
pub(crate) fn is_emoji_modifier(ch: char) -> bool {
    matches!(
        ch as u32,
        0x200D | 0xFE0E | 0xFE0F | 0x20E3 | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F
    )
}

/// Draws an emoji in a browser page, as `(text, height)`.
#[cfg(target_arch = "wasm32")]
pub(crate) type BrowserPainter = fn(&str, u32) -> Option<ColorBitmap>;

#[cfg(target_arch = "wasm32")]
static BROWSER_PAINTER: OnceLock<BrowserPainter> = OnceLock::new();

#[cfg(target_arch = "wasm32")]
pub(crate) fn set_browser_painter(painter: BrowserPainter) {
    let _ = BROWSER_PAINTER.set(painter);
}

/// The browser's picture of `ch`; it knows the fonts a page cannot read.
#[cfg(target_arch = "wasm32")]
pub(crate) fn render_in_browser(ch: char) -> Option<Arc<ColorBitmap>> {
    let painter = BROWSER_PAINTER.get()?;
    let text = sequence_of(ch).unwrap_or_else(|| Arc::from(ch.to_string()));
    painter(&text, PICTURE_HEIGHT).map(Arc::new)
}

/// The picture of glyph `id`, when `font` has one.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn render(font: &ab_glyph::FontArc, id: ab_glyph::GlyphId) -> Option<Arc<ColorBitmap>> {
    let bitmap = strike(font, id).or_else(|| layers::paint(font, id));
    #[cfg(target_vendor = "apple")]
    let bitmap = bitmap.or_else(|| drawn_by_system(font, id));
    bitmap.map(Arc::new)
}

/// Apple keeps some of its pictures in forms of its own: all of them on iOS and
/// tvOS (`emjc`), and the mirrored ones everywhere (`flip`).
#[cfg(target_vendor = "apple")]
fn drawn_by_system(font: &ab_glyph::FontArc, id: ab_glyph::GlyphId) -> Option<ColorBitmap> {
    use ab_glyph::Font;
    if id.0 == 0 {
        return None;
    }
    ttf_parser::RawFace::parse(font.font_data(), 0)
        .ok()?
        .table(ttf_parser::Tag::from_bytes(b"sbix"))?;
    let picture = crate::coretext::draw_emoji(id.0, PICTURE_HEIGHT)?;
    let em = PICTURE_HEIGHT as f32;
    Some(ColorBitmap {
        width: picture.width,
        height: picture.height,
        left: picture.left as f32 / em,
        em_width: picture.width as f32 / em,
        em_height: picture.height as f32 / em,
        rgba: picture.rgba,
        advance: font.h_advance_unscaled(id) / font.units_per_em().unwrap_or(1.0).max(1.0),
    })
}

/// The image a font stores for `id`, at the size nearest what is kept.
#[cfg(not(target_arch = "wasm32"))]
fn strike(font: &ab_glyph::FontArc, id: ab_glyph::GlyphId) -> Option<ColorBitmap> {
    use ab_glyph::{Font, GlyphImageFormat};
    let image = font.glyph_raster_image2(id, PICTURE_HEIGHT as u16)?;
    if !matches!(image.format, GlyphImageFormat::Png) {
        return None;
    }
    let (width, height, rgba) = decode_png(image.data)?;
    let (width, height, rgba) = shrink(width, height, rgba);
    let em = f32::from(image.pixels_per_em).max(1.0);
    let stored_height = f32::from(image.height);
    Some(ColorBitmap {
        width,
        height,
        rgba,
        left: image.origin.x / em,
        em_width: f32::from(image.width) / em,
        em_height: stored_height / em,
        advance: font.h_advance_unscaled(id) / font.units_per_em().unwrap_or(1.0).max(1.0),
    })
}

/// RGBA with premultiplied alpha.
#[cfg(not(target_arch = "wasm32"))]
fn decode_png(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().ok()?;
    let mut pixels = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut pixels).ok()?;
    pixels.truncate(frame.buffer_size());
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return None;
    }
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u32::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
    Some((frame.width, frame.height, pixels))
}

/// Halves a picture until it is near [`PICTURE_HEIGHT`]: some fonts store only
/// one large size, and the atlas keeps every emoji shown.
#[cfg(not(target_arch = "wasm32"))]
fn shrink(mut width: u32, mut height: u32, mut rgba: Vec<u8>) -> (u32, u32, Vec<u8>) {
    while height >= PICTURE_HEIGHT * 2 && width >= 2 {
        let (next_width, next_height) = (width / 2, height / 2);
        let mut next = vec![0; (next_width * next_height * 4) as usize];
        for y in 0..next_height as usize {
            for x in 0..next_width as usize {
                for channel in 0..4 {
                    let at = |dx: usize, dy: usize| {
                        u32::from(rgba[((y * 2 + dy) * width as usize + x * 2 + dx) * 4 + channel])
                    };
                    next[(y * next_width as usize + x) * 4 + channel] =
                        ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8;
                }
            }
        }
        (width, height, rgba) = (next_width, next_height, next);
    }
    (width, height, rgba)
}

/// `COLR`: a glyph painted from outlines, solid colors and gradients.
#[cfg(not(target_arch = "wasm32"))]
mod layers {
    use super::{ColorBitmap, PICTURE_HEIGHT};
    use ab_glyph::Font;
    use ab_glyph_rasterizer::{Point, Rasterizer};
    use ttf_parser::colr::{ClipBox, CompositeMode, Paint, Painter};
    use ttf_parser::{Face, GlyphId, OutlineBuilder, RgbaColor, Transform};

    const FOREGROUND: RgbaColor = RgbaColor {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 255,
    };

    pub(super) fn paint(font: &ab_glyph::FontArc, id: ab_glyph::GlyphId) -> Option<ColorBitmap> {
        // Color fonts are single files; ab_glyph does not say which face it read.
        let face = Face::parse(font.font_data(), 0).ok()?;
        let glyph = GlyphId(id.0);
        if !face.is_color_glyph(glyph) {
            return None;
        }
        let units = f32::from(face.units_per_em()).max(1.0);
        let advance = f32::from(face.glyph_hor_advance(glyph)?);
        let bounds = measure_bounds(&face, glyph)?;
        let frame = CanvasFrame::for_bounds(bounds, units)?;
        let mut canvas = Canvas::new(&face, frame.width, frame.height, frame.transform);
        face.paint_color_glyph(glyph, 0, FOREGROUND, &mut canvas)?;
        let rgba = canvas.finish();
        crop_bitmap(
            &rgba,
            frame.width,
            frame.height,
            frame.origin_x,
            units * frame.scale,
            advance / units,
        )
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(super) struct Bounds {
        pub(super) min_x: f32,
        pub(super) min_y: f32,
        pub(super) max_x: f32,
        pub(super) max_y: f32,
    }

    impl Bounds {
        fn empty() -> Self {
            Self {
                min_x: f32::INFINITY,
                min_y: f32::INFINITY,
                max_x: f32::NEG_INFINITY,
                max_y: f32::NEG_INFINITY,
            }
        }

        fn include(&mut self, x: f32, y: f32) {
            self.min_x = self.min_x.min(x);
            self.min_y = self.min_y.min(y);
            self.max_x = self.max_x.max(x);
            self.max_y = self.max_y.max(y);
        }

        fn union(&mut self, other: Self) {
            self.min_x = self.min_x.min(other.min_x);
            self.min_y = self.min_y.min(other.min_y);
            self.max_x = self.max_x.max(other.max_x);
            self.max_y = self.max_y.max(other.max_y);
        }

        fn intersect(self, other: Self) -> Option<Self> {
            let bounds = Self {
                min_x: self.min_x.max(other.min_x),
                min_y: self.min_y.max(other.min_y),
                max_x: self.max_x.min(other.max_x),
                max_y: self.max_y.min(other.max_y),
            };
            bounds.valid()
        }

        fn valid(self) -> Option<Self> {
            (self.min_x.is_finite()
                && self.min_y.is_finite()
                && self.max_x.is_finite()
                && self.max_y.is_finite()
                && self.max_x > self.min_x
                && self.max_y > self.min_y)
                .then_some(self)
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum ShapeBounds {
        Unbounded,
        Empty,
        Rect(Bounds),
    }

    impl ShapeBounds {
        fn intersect(self, other: Self) -> Self {
            match (self, other) {
                (Self::Empty, _) | (_, Self::Empty) => Self::Empty,
                (Self::Unbounded, other) => other,
                (this, Self::Unbounded) => this,
                (Self::Rect(a), Self::Rect(b)) => a.intersect(b).map_or(Self::Empty, Self::Rect),
            }
        }
    }

    struct OutlineBounds {
        transform: Transform,
        bounds: Bounds,
    }

    impl OutlineBounds {
        fn push(&mut self, x: f32, y: f32) {
            let [x, y] = apply(self.transform, x, y);
            self.bounds.include(x, y);
        }
    }

    impl OutlineBuilder for OutlineBounds {
        fn move_to(&mut self, x: f32, y: f32) {
            self.push(x, y);
        }

        fn line_to(&mut self, x: f32, y: f32) {
            self.push(x, y);
        }

        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            self.push(x1, y1);
            self.push(x, y);
        }

        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
            self.push(x1, y1);
            self.push(x2, y2);
            self.push(x, y);
        }

        fn close(&mut self) {}
    }

    struct BoundsPainter<'a, 'b> {
        face: &'b Face<'a>,
        transforms: Vec<Transform>,
        shape: ShapeBounds,
        clips: Vec<ShapeBounds>,
        painted: Bounds,
    }

    impl<'a, 'b> BoundsPainter<'a, 'b> {
        fn new(face: &'b Face<'a>) -> Self {
            Self {
                face,
                transforms: vec![Transform::default()],
                shape: ShapeBounds::Unbounded,
                clips: Vec::new(),
                painted: Bounds::empty(),
            }
        }

        fn transform(&self) -> Transform {
            *self.transforms.last().expect("the base transform stays")
        }

        fn active_clip(&self) -> ShapeBounds {
            self.clips.last().copied().unwrap_or(ShapeBounds::Unbounded)
        }
    }

    impl<'a> Painter<'a> for BoundsPainter<'a, '_> {
        fn outline_glyph(&mut self, glyph: GlyphId) {
            let mut collector = OutlineBounds {
                transform: self.transform(),
                bounds: Bounds::empty(),
            };
            self.face.outline_glyph(glyph, &mut collector);
            self.shape = collector
                .bounds
                .valid()
                .map_or(ShapeBounds::Empty, ShapeBounds::Rect);
        }

        fn paint(&mut self, _paint: Paint<'a>) {
            if let ShapeBounds::Rect(bounds) = self.shape.intersect(self.active_clip()) {
                self.painted.union(bounds);
            }
        }

        fn push_clip(&mut self) {
            let clip = self.shape.intersect(self.active_clip());
            self.clips.push(clip);
            self.shape = ShapeBounds::Unbounded;
        }

        fn push_clip_box(&mut self, clipbox: ClipBox) {
            let transform = self.transform();
            let mut box_bounds = Bounds::empty();
            for (x, y) in [
                (clipbox.x_min, clipbox.y_min),
                (clipbox.x_max, clipbox.y_min),
                (clipbox.x_max, clipbox.y_max),
                (clipbox.x_min, clipbox.y_max),
            ] {
                let [tx, ty] = apply(transform, x, y);
                box_bounds.include(tx, ty);
            }
            let shape = box_bounds
                .valid()
                .map_or(ShapeBounds::Empty, ShapeBounds::Rect);
            let clip = shape.intersect(self.active_clip());
            self.clips.push(clip);
            self.shape = ShapeBounds::Unbounded;
        }

        fn pop_clip(&mut self) {
            self.clips.pop();
            self.shape = ShapeBounds::Unbounded;
        }

        fn push_layer(&mut self, _mode: CompositeMode) {}

        fn pop_layer(&mut self) {}

        fn push_transform(&mut self, transform: Transform) {
            let combined = Transform::combine(self.transform(), transform);
            self.transforms.push(combined);
        }

        fn pop_transform(&mut self) {
            if self.transforms.len() > 1 {
                self.transforms.pop();
            }
        }
    }

    fn measure_bounds(face: &Face<'_>, glyph: GlyphId) -> Option<Bounds> {
        let mut painter = BoundsPainter::new(face);
        face.paint_color_glyph(glyph, 0, FOREGROUND, &mut painter)?;
        painter.painted.valid()
    }

    #[derive(Clone, Copy, Debug)]
    pub(super) struct CanvasFrame {
        pub(super) width: u32,
        pub(super) height: u32,
        pub(super) origin_x: f32,
        pub(super) scale: f32,
        pub(super) transform: Transform,
    }

    impl CanvasFrame {
        pub(super) fn for_bounds(bounds: Bounds, units: f32) -> Option<Self> {
            let span_x = bounds.max_x - bounds.min_x;
            let span_y = bounds.max_y - bounds.min_y;
            if !(span_x > 0.0 && span_y > 0.0 && units > 0.0) {
                return None;
            }
            let scale = (PICTURE_HEIGHT as f32 / units)
                .min(PICTURE_HEIGHT as f32 / span_y)
                .min(PICTURE_HEIGHT as f32 * 2.0 / span_x);
            if !(scale.is_finite() && scale > 0.0) {
                return None;
            }
            // Leave a 1px margin around the measured font bounds so antialiased
            // edges never clip even when artwork exceeds `hhea.ascender`.
            let origin_x = (bounds.min_x * scale).floor() - 1.0;
            let right_x = (bounds.max_x * scale).ceil() + 1.0;
            let origin_y = (-bounds.max_y * scale).floor() - 1.0;
            let bottom_y = (-bounds.min_y * scale).ceil() + 1.0;
            let width = (right_x - origin_x).max(1.0) as u32;
            let height = (bottom_y - origin_y).max(1.0) as u32;
            Some(Self {
                width,
                height,
                origin_x,
                scale,
                transform: Transform::new(scale, 0.0, 0.0, -scale, -origin_x, -origin_y),
            })
        }
    }

    pub(super) fn crop_bitmap(
        rgba: &[u8],
        width: u32,
        height: u32,
        origin_x: f32,
        px_per_em: f32,
        advance: f32,
    ) -> Option<ColorBitmap> {
        let (w, h) = (width as usize, height as usize);
        if w == 0 || h == 0 || rgba.len() < w * h * 4 {
            return None;
        }
        let painted = |x: usize, y: usize| rgba[(y * w + x) * 4 + 3] != 0;
        let top = (0..h).find(|&y| (0..w).any(|x| painted(x, y)))?;
        let bottom = (0..h).rev().find(|&y| (0..w).any(|x| painted(x, y)))?;
        let left = (0..w).find(|&x| (top..=bottom).any(|y| painted(x, y)))?;
        let right = (0..w)
            .rev()
            .find(|&x| (top..=bottom).any(|y| painted(x, y)))?;
        let crop_w = right - left + 1;
        let crop_h = bottom - top + 1;
        let mut cropped = Vec::with_capacity(crop_w * crop_h * 4);
        for y in top..=bottom {
            let start = (y * w + left) * 4;
            cropped.extend_from_slice(&rgba[start..start + crop_w * 4]);
        }
        let em = px_per_em.max(1.0);
        Some(ColorBitmap {
            width: crop_w as u32,
            height: crop_h as u32,
            rgba: cropped,
            left: (origin_x + left as f32) / em,
            em_width: crop_w as f32 / em,
            em_height: crop_h as f32 / em,
            advance,
        })
    }

    type Pixels = Vec<[f32; 4]>;

    struct Canvas<'a, 'b> {
        face: &'b Face<'a>,
        width: u32,
        height: u32,
        transforms: Vec<Transform>,
        /// What `paint` fills: the last outline.
        shape: Vec<f32>,
        clips: Vec<Vec<f32>>,
        layers: Vec<Pixels>,
    }

    impl<'a, 'b> Canvas<'a, 'b> {
        fn new(face: &'b Face<'a>, width: u32, height: u32, base: Transform) -> Self {
            let size = (width * height) as usize;
            Self {
                face,
                width,
                height,
                transforms: vec![base],
                shape: vec![1.0; size],
                clips: Vec::new(),
                layers: vec![vec![[0.0; 4]; size]],
            }
        }

        fn transform(&self) -> Transform {
            *self.transforms.last().expect("the base transform stays")
        }

        fn rasterize(&self, build: impl FnOnce(&mut Path)) -> Vec<f32> {
            let width = self.width as usize;
            let height = self.height as usize;
            // `ab_glyph_rasterizer` accumulates scanlines across a flat buffer
            // without horizontal clipping: edges at `x < 0` wrap into the
            // previous row (or get dropped on row 0) and edges at `x >= width`
            // spill into the next row. Shift x by +1 into a `width + 3` buffer
            // (1 left guard + `width` pixels + 2 right sentinels) and project
            // out-of-bounds edges onto `[0, width]` so every scanline's winding
            // sum closes cleanly inside its own row.
            let stride = width + 3;
            let mut path = Path {
                transform: self.transform(),
                rasterizer: Rasterizer::new(stride, height),
                width: self.width as f32,
                height: self.height as f32,
                start: Point { x: 0.0, y: 0.0 },
                pen: Point { x: 0.0, y: 0.0 },
            };
            build(&mut path);
            let mut mask = vec![0.0; width * height];
            path.rasterizer.for_each_pixel(|index, coverage| {
                let x = index % stride;
                if x >= 1 && x <= width {
                    let y = index / stride;
                    mask[y * width + (x - 1)] = coverage.abs().min(1.0);
                }
            });
            mask
        }

        fn clip(&mut self) {
            let mut clip = self.shape.clone();
            if let Some(outer) = self.clips.last() {
                for (pixel, outer) in clip.iter_mut().zip(outer) {
                    *pixel *= outer;
                }
            }
            self.clips.push(clip);
            self.shape.fill(1.0);
        }

        fn finish(mut self) -> Vec<u8> {
            let pixels = self.layers.pop().expect("the first layer stays");
            let mut rgba = Vec::with_capacity(pixels.len() * 4);
            for pixel in pixels {
                rgba.extend(pixel.map(|channel| (channel.clamp(0.0, 1.0) * 255.0 + 0.5) as u8));
            }
            rgba
        }
    }

    impl<'a> Painter<'a> for Canvas<'a, '_> {
        fn outline_glyph(&mut self, glyph: GlyphId) {
            let face = self.face;
            self.shape = self.rasterize(|path| {
                face.outline_glyph(glyph, path);
            });
        }

        fn paint(&mut self, paint: Paint<'a>) {
            let shader = Shader::new(&paint, self.transform());
            let width = self.width as usize;
            let clip = self.clips.last();
            let target = self.layers.last_mut().expect("the first layer stays");
            for (index, pixel) in target.iter_mut().enumerate() {
                let coverage = self.shape[index] * clip.map_or(1.0, |clip| clip[index]);
                if coverage <= 0.0 {
                    continue;
                }
                let x = (index % width) as f32 + 0.5;
                let y = (index / width) as f32 + 0.5;
                let color = shader.at(x, y);
                let alpha = color[3] * coverage;
                for channel in 0..3 {
                    pixel[channel] = color[channel] * alpha + pixel[channel] * (1.0 - alpha);
                }
                pixel[3] = alpha + pixel[3] * (1.0 - alpha);
            }
        }

        fn push_clip(&mut self) {
            self.clip();
        }

        fn push_clip_box(&mut self, clipbox: ClipBox) {
            self.shape = self.rasterize(|path| {
                path.move_to(clipbox.x_min, clipbox.y_min);
                path.line_to(clipbox.x_max, clipbox.y_min);
                path.line_to(clipbox.x_max, clipbox.y_max);
                path.line_to(clipbox.x_min, clipbox.y_max);
                path.close();
            });
            self.clip();
        }

        fn pop_clip(&mut self) {
            self.clips.pop();
            self.shape.fill(1.0);
        }

        fn push_layer(&mut self, _mode: CompositeMode) {
            self.layers
                .push(vec![[0.0; 4]; (self.width * self.height) as usize]);
        }

        /// Every layer goes over the one below: emoji fonts rarely ask for more.
        fn pop_layer(&mut self) {
            let Some(top) = self.layers.pop() else {
                return;
            };
            let target = self.layers.last_mut().expect("the first layer stays");
            for (pixel, over) in target.iter_mut().zip(top) {
                for channel in 0..4 {
                    pixel[channel] = over[channel] + pixel[channel] * (1.0 - over[3]);
                }
            }
        }

        fn push_transform(&mut self, transform: Transform) {
            let combined = Transform::combine(self.transform(), transform);
            self.transforms.push(combined);
        }

        fn pop_transform(&mut self) {
            self.transforms.pop();
        }
    }

    /// Turns outline commands into pixels on the canvas.
    pub(super) struct Path {
        transform: Transform,
        rasterizer: Rasterizer,
        width: f32,
        height: f32,
        start: Point,
        pen: Point,
    }

    impl Path {
        fn map(&self, x: f32, y: f32) -> Point {
            let [x, y] = apply(self.transform, x, y);
            Point { x, y }
        }

        fn in_bounds(&self, points: &[Point]) -> bool {
            points.iter().all(|p| {
                p.x.is_finite()
                    && p.y.is_finite()
                    && p.x >= 0.0
                    && p.x <= self.width
                    && p.y >= 0.0
                    && p.y <= self.height
            })
        }

        fn shifted(p: Point) -> Point {
            Point {
                x: p.x + 1.0,
                y: p.y,
            }
        }

        fn lerp_point(t: f32, a: Point, b: Point) -> Point {
            Point {
                x: a.x + (b.x - a.x) * t,
                y: a.y + (b.y - a.y) * t,
            }
        }

        fn draw_clipped_line(&mut self, mut p0: Point, mut p1: Point) {
            if !p0.x.is_finite() || !p0.y.is_finite() || !p1.x.is_finite() || !p1.y.is_finite() {
                return;
            }
            if (p0.y - p1.y).abs() <= f32::EPSILON {
                return;
            }
            let h = self.height;
            if (p0.y <= 0.0 && p1.y <= 0.0) || (p0.y >= h && p1.y >= h) {
                return;
            }
            let clip_y = |a: Point, b: Point, y: f32| -> Point {
                let t = (y - a.y) / (b.y - a.y);
                Point {
                    x: a.x + (b.x - a.x) * t,
                    y,
                }
            };
            if p0.y < 0.0 {
                p0 = clip_y(p0, p1, 0.0);
            } else if p0.y > h {
                p0 = clip_y(p0, p1, h);
            }
            if p1.y < 0.0 {
                p1 = clip_y(p1, p0, 0.0);
            } else if p1.y > h {
                p1 = clip_y(p1, p0, h);
            }
            if (p0.y - p1.y).abs() <= f32::EPSILON {
                return;
            }

            let w = self.width;
            let emit = |rasterizer: &mut Rasterizer, a: Point, b: Point| {
                if (a.y - b.y).abs() > f32::EPSILON {
                    rasterizer.draw_line(
                        Point {
                            x: a.x.clamp(0.0, w) + 1.0,
                            y: a.y,
                        },
                        Point {
                            x: b.x.clamp(0.0, w) + 1.0,
                            y: b.y,
                        },
                    );
                }
            };

            let dx = p1.x - p0.x;
            if dx.abs() <= f32::EPSILON
                || (p0.x <= 0.0 && p1.x <= 0.0)
                || (p0.x >= w && p1.x >= w)
                || (p0.x >= 0.0 && p0.x <= w && p1.x >= 0.0 && p1.x <= w)
            {
                emit(&mut self.rasterizer, p0, p1);
                return;
            }

            let mut ts = [0.0f32; 4];
            let mut len = 0usize;
            ts[len] = 0.0;
            len += 1;
            for bound in [0.0, w] {
                let t = (bound - p0.x) / dx;
                if t > 0.0 && t < 1.0 {
                    ts[len] = t;
                    len += 1;
                }
            }
            ts[len] = 1.0;
            len += 1;
            ts[..len].sort_by(f32::total_cmp);

            let mut prev = p0;
            for &t in &ts[1..len] {
                let next = if t == 1.0 {
                    p1
                } else {
                    Point {
                        x: p0.x + dx * t,
                        y: p0.y + (p1.y - p0.y) * t,
                    }
                };
                emit(&mut self.rasterizer, prev, next);
                prev = next;
            }
        }

        fn draw_clipped_quad(&mut self, p0: Point, p1: Point, p2: Point) {
            if self.in_bounds(&[p0, p1, p2]) {
                self.rasterizer
                    .draw_quad(Self::shifted(p0), Self::shifted(p1), Self::shifted(p2));
                return;
            }
            if (p0.y <= 0.0 && p1.y <= 0.0 && p2.y <= 0.0)
                || (p0.y >= self.height && p1.y >= self.height && p2.y >= self.height)
            {
                return;
            }
            if (p0.x <= 0.0 && p1.x <= 0.0 && p2.x <= 0.0)
                || (p0.x >= self.width && p1.x >= self.width && p2.x >= self.width)
            {
                self.draw_clipped_line(p0, p2);
                return;
            }
            let devx = p0.x - 2.0 * p1.x + p2.x;
            let devy = p0.y - 2.0 * p1.y + p2.y;
            let devsq = devx * devx + devy * devy;
            if !devsq.is_finite() || devsq < 0.333 {
                self.draw_clipped_line(p0, p2);
                return;
            }
            let n = (1 + (3.0 * devsq).sqrt().sqrt().floor() as usize).min(1024);
            let mut p = p0;
            let nrecip = (n as f32).recip();
            let mut t = 0.0;
            for _ in 0..n - 1 {
                t += nrecip;
                let pn =
                    Self::lerp_point(t, Self::lerp_point(t, p0, p1), Self::lerp_point(t, p1, p2));
                self.draw_clipped_line(p, pn);
                p = pn;
            }
            self.draw_clipped_line(p, p2);
        }

        fn draw_clipped_cubic(&mut self, p0: Point, p1: Point, p2: Point, p3: Point, depth: u8) {
            if self.in_bounds(&[p0, p1, p2, p3]) {
                self.rasterizer.draw_cubic(
                    Self::shifted(p0),
                    Self::shifted(p1),
                    Self::shifted(p2),
                    Self::shifted(p3),
                );
                return;
            }
            if (p0.y <= 0.0 && p1.y <= 0.0 && p2.y <= 0.0 && p3.y <= 0.0)
                || (p0.y >= self.height
                    && p1.y >= self.height
                    && p2.y >= self.height
                    && p3.y >= self.height)
            {
                return;
            }
            if (p0.x <= 0.0 && p1.x <= 0.0 && p2.x <= 0.0 && p3.x <= 0.0)
                || (p0.x >= self.width
                    && p1.x >= self.width
                    && p2.x >= self.width
                    && p3.x >= self.width)
            {
                self.draw_clipped_line(p0, p3);
                return;
            }
            const FLATNESS_SQUARED: f32 = 0.35 * 0.35;
            const MAX_DEPTH: u8 = 16;
            let dist = |a: Point, b: Point| (b.x - a.x).hypot(b.y - a.y);
            let longlen = dist(p0, p1) + dist(p1, p2) + dist(p2, p3);
            let shortlen = dist(p0, p3);
            let flatness_squared = longlen * longlen - shortlen * shortlen;
            if depth < MAX_DEPTH
                && flatness_squared.is_finite()
                && flatness_squared > FLATNESS_SQUARED
            {
                let p01 = Self::lerp_point(0.5, p0, p1);
                let p12 = Self::lerp_point(0.5, p1, p2);
                let p23 = Self::lerp_point(0.5, p2, p3);
                let pa = Self::lerp_point(0.5, p01, p12);
                let pb = Self::lerp_point(0.5, p12, p23);
                let mp = Self::lerp_point(0.5, pa, pb);
                self.draw_clipped_cubic(p0, p01, pa, mp, depth + 1);
                self.draw_clipped_cubic(mp, pb, p23, p3, depth + 1);
            } else {
                self.draw_clipped_line(p0, p3);
            }
        }
    }

    impl OutlineBuilder for Path {
        fn move_to(&mut self, x: f32, y: f32) {
            self.pen = self.map(x, y);
            self.start = self.pen;
        }

        fn line_to(&mut self, x: f32, y: f32) {
            let to = self.map(x, y);
            self.draw_clipped_line(self.pen, to);
            self.pen = to;
        }

        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            let (control, to) = (self.map(x1, y1), self.map(x, y));
            self.draw_clipped_quad(self.pen, control, to);
            self.pen = to;
        }

        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
            let (first, second, to) = (self.map(x1, y1), self.map(x2, y2), self.map(x, y));
            self.draw_clipped_cubic(self.pen, first, second, to, 0);
            self.pen = to;
        }

        fn close(&mut self) {
            self.draw_clipped_line(self.pen, self.start);
            self.pen = self.start;
        }
    }

    #[cfg(test)]
    pub(super) fn rasterize_rect_for_test(
        width: u32,
        height: u32,
        x_min: f32,
        y_min: f32,
        x_max: f32,
        y_max: f32,
    ) -> Vec<f32> {
        let stride = width as usize + 3;
        let mut path = Path {
            transform: Transform::default(),
            rasterizer: Rasterizer::new(stride, height as usize),
            width: width as f32,
            height: height as f32,
            start: Point { x: 0.0, y: 0.0 },
            pen: Point { x: 0.0, y: 0.0 },
        };
        path.move_to(x_min, y_min);
        path.line_to(x_max, y_min);
        path.line_to(x_max, y_max);
        path.line_to(x_min, y_max);
        path.close();
        let mut mask = vec![0.0; (width * height) as usize];
        path.rasterizer.for_each_pixel(|index, coverage| {
            let x = index % stride;
            if x >= 1 && x <= width as usize {
                let y = index / stride;
                mask[y * width as usize + (x - 1)] = coverage.abs().min(1.0);
            }
        });
        mask
    }

    /// The color of a paint at a point on the canvas, not premultiplied.
    enum Shader {
        Solid([f32; 4]),
        Linear {
            inverse: Option<Transform>,
            start: [f32; 2],
            axis: [f32; 2],
            stops: Vec<(f32, [f32; 4])>,
        },
        Radial {
            inverse: Option<Transform>,
            center: [f32; 2],
            radius: f32,
            step: [f32; 2],
            growth: f32,
            stops: Vec<(f32, [f32; 4])>,
        },
    }

    fn color(color: RgbaColor) -> [f32; 4] {
        [color.red, color.green, color.blue, color.alpha].map(|channel| f32::from(channel) / 255.0)
    }

    fn sorted(stops: impl Iterator<Item = (f32, [f32; 4])>) -> Vec<(f32, [f32; 4])> {
        let mut stops: Vec<_> = stops.collect();
        stops.sort_by(|a, b| a.0.total_cmp(&b.0));
        stops
    }

    fn sample(stops: &[(f32, [f32; 4])], t: f32) -> [f32; 4] {
        let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
            return [0.0; 4];
        };
        if t <= first.0 {
            return first.1;
        }
        if t >= last.0 {
            return last.1;
        }
        for pair in stops.windows(2) {
            let ((from, a), (to, b)) = (pair[0], pair[1]);
            if t <= to {
                let mix = if to > from {
                    (t - from) / (to - from)
                } else {
                    1.0
                };
                return std::array::from_fn(|channel| a[channel] + (b[channel] - a[channel]) * mix);
            }
        }
        last.1
    }

    fn apply(transform: Transform, x: f32, y: f32) -> [f32; 2] {
        let Transform { a, b, c, d, e, f } = transform;
        [a * x + c * y + e, b * x + d * y + f]
    }

    fn invert(transform: Transform) -> Option<Transform> {
        let Transform { a, b, c, d, e, f } = transform;
        let determinant = a * d - b * c;
        if determinant.abs() < 1e-9 {
            return None;
        }
        Some(Transform::new(
            d / determinant,
            -b / determinant,
            -c / determinant,
            a / determinant,
            (c * f - d * e) / determinant,
            (b * e - a * f) / determinant,
        ))
    }

    impl Shader {
        fn new(paint: &Paint<'_>, transform: Transform) -> Self {
            let inverse = invert(transform);
            match paint {
                Paint::Solid(solid) => Self::Solid(color(*solid)),
                Paint::LinearGradient(gradient) => {
                    let stops = sorted(
                        gradient
                            .stops(0, &[])
                            .map(|stop| (stop.stop_offset, color(stop.color))),
                    );
                    // The gradient runs along p0-p1, turned square to p0-p2.
                    let start = [gradient.x0, gradient.y0];
                    let along = [gradient.x1 - gradient.x0, gradient.y1 - gradient.y0];
                    let across = [gradient.x2 - gradient.x0, gradient.y2 - gradient.y0];
                    let normal = [across[1], -across[0]];
                    let length = (normal[0] * normal[0] + normal[1] * normal[1]).max(1e-6);
                    let dot = along[0] * normal[0] + along[1] * normal[1];
                    let axis = [normal[0] * dot / length, normal[1] * dot / length];
                    Self::Linear {
                        inverse,
                        start,
                        axis,
                        stops,
                    }
                }
                Paint::RadialGradient(gradient) => Self::Radial {
                    inverse,
                    center: [gradient.x0, gradient.y0],
                    radius: gradient.r0,
                    step: [gradient.x1 - gradient.x0, gradient.y1 - gradient.y0],
                    growth: gradient.r1 - gradient.r0,
                    stops: sorted(
                        gradient
                            .stops(0, &[])
                            .map(|stop| (stop.stop_offset, color(stop.color))),
                    ),
                },
                // A turn around a point: its average reads the same at this size.
                Paint::SweepGradient(gradient) => {
                    let stops: Vec<_> = gradient
                        .stops(0, &[])
                        .map(|stop| color(stop.color))
                        .collect();
                    let count = stops.len().max(1) as f32;
                    Self::Solid(std::array::from_fn(|channel| {
                        stops.iter().map(|stop| stop[channel]).sum::<f32>() / count
                    }))
                }
            }
        }

        fn at(&self, x: f32, y: f32) -> [f32; 4] {
            let unmapped = |inverse: &Option<Transform>| {
                inverse.map_or([x, y], |inverse| apply(inverse, x, y))
            };
            match self {
                Self::Solid(color) => *color,
                Self::Linear {
                    inverse,
                    start,
                    axis,
                    stops,
                } => {
                    let [x, y] = unmapped(inverse);
                    let length = (axis[0] * axis[0] + axis[1] * axis[1]).max(1e-6);
                    let t = ((x - start[0]) * axis[0] + (y - start[1]) * axis[1]) / length;
                    sample(stops, t)
                }
                Self::Radial {
                    inverse,
                    center,
                    radius,
                    step,
                    growth,
                    stops,
                } => {
                    let [x, y] = unmapped(inverse);
                    let (dx, dy) = (x - center[0], y - center[1]);
                    let a = step[0] * step[0] + step[1] * step[1] - growth * growth;
                    let b = dx * step[0] + dy * step[1] + radius * growth;
                    let c = dx * dx + dy * dy - radius * radius;
                    let t = if a.abs() < 1e-6 {
                        if b.abs() < 1e-6 { 0.0 } else { c / (2.0 * b) }
                    } else {
                        let root = (b * b - a * c).max(0.0).sqrt();
                        ((b + root) / a).max((b - root) / a)
                    };
                    sample(stops, t)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_are_told_from_text_and_from_their_modifiers() {
        for ch in ['🎉', '😂', '✨', '⭐', '❤'] {
            assert!(is_emoji(ch), "{ch}");
        }
        for ch in ['A', '中', 'あ', '한', '。'] {
            assert!(!is_emoji(ch), "{ch}");
        }
        for ch in ['\u{200D}', '\u{FE0F}', '🏻'] {
            assert!(is_emoji_modifier(ch) && !is_emoji(ch), "{ch:?}");
        }
    }

    fn folded_length(text: &str) -> usize {
        fold(text).chars().count()
    }

    #[test]
    fn sequences_fold_into_one_character_each() {
        assert_eq!(folded_length("🇨🇳"), 1);
        assert_eq!(folded_length("🇨🇳🇯🇵"), 2);
        assert_eq!(folded_length("👨\u{200D}👩\u{200D}👧"), 1);
        assert_eq!(folded_length("👍🏻"), 1);
        assert_eq!(folded_length("❤\u{FE0F}"), 1);
        assert_eq!(folded_length("1\u{FE0F}\u{20E3}"), 1);
        assert_eq!(folded_length("国旗🇨🇳国旗"), 5);
    }

    #[test]
    fn text_without_sequences_stays_as_it_is() {
        assert!(matches!(fold("纯文字 😂 ABC"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_sequence_keeps_its_character() {
        let first = fold("🇨🇳").chars().next().unwrap();
        assert_eq!(fold("🇨🇳").chars().next(), Some(first));
        assert!(is_emoji(first) && is_sequence(first));
        assert_eq!(first_char(first), '🇨');
    }

    #[test]
    fn large_pictures_halve_towards_the_kept_size() {
        let (width, height, rgba) = shrink(400, 200, vec![255; 400 * 200 * 4]);
        assert_eq!((width, height), (200, 100));
        assert_eq!(rgba.len(), 200 * 100 * 4);
        assert!(rgba.iter().all(|channel| *channel == 255));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn colr_canvas_frame_keeps_artwork_above_hhea_ascender_inside_canvas() {
        // Matches Windows 11 Segoe UI Emoji metrics (upem 2048, hhea ascender 1491,
        // while emoji outlines extend up to ~1886 above the baseline).
        let bounds = layers::Bounds {
            min_x: 246.0,
            min_y: -328.0,
            max_x: 2542.0,
            max_y: 1886.0,
        };
        let frame = layers::CanvasFrame::for_bounds(bounds, 2048.0).unwrap();
        let corners = [
            (bounds.min_x, bounds.min_y),
            (bounds.max_x, bounds.min_y),
            (bounds.max_x, bounds.max_y),
            (bounds.min_x, bounds.max_y),
        ];
        for (x, y) in corners {
            let t = frame.transform;
            let px = t.a * x + t.c * y + t.e;
            let py = t.b * x + t.d * y + t.f;
            assert!(
                px >= 1.0 && px <= frame.width as f32 - 1.0,
                "x={x} mapped outside canvas: px={px}, width={}",
                frame.width
            );
            assert!(
                py >= 1.0 && py <= frame.height as f32 - 1.0,
                "y={y} mapped outside canvas: py={py}, height={}",
                frame.height
            );
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn colr_crop_trims_asymmetric_margins_and_preserves_em_metrics() {
        let (w, h) = (100u32, 100u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        // Paint a 40x60 box offset near the top-right to mimic asymmetric padding.
        for y in 4..64 {
            for x in 20..60 {
                let idx = ((y * w + x) * 4) as usize;
                rgba[idx] = 255;
                rgba[idx + 1] = 200;
                rgba[idx + 2] = 0;
                rgba[idx + 3] = 255;
            }
        }
        let bitmap = layers::crop_bitmap(&rgba, w, h, 10.0, 80.0, 1.25).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (40, 60));
        assert!((bitmap.left - (10.0 + 20.0) / 80.0).abs() < 1e-5);
        assert!((bitmap.em_width - 40.0 / 80.0).abs() < 1e-5);
        assert!((bitmap.em_height - 60.0 / 80.0).abs() < 1e-5);
        assert!((bitmap.advance - 1.25).abs() < 1e-5);
        // Top and bottom rows of the cropped picture both contain painted pixels.
        assert!((0..bitmap.width).any(|x| bitmap.rgba[(x * 4 + 3) as usize] != 0));
        assert!((0..bitmap.width).any(|x| bitmap.rgba
            [(((bitmap.height - 1) * bitmap.width + x) * 4 + 3) as usize]
            != 0));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn colr_rasterize_handles_clip_box_extending_outside_canvas() {
        // A COLR v1 ClipBox is often larger than the tight painted outline bounds,
        // so its edges can lie at x < 0, x > width, y < 0, or y > height.
        for (x_min, y_min, x_max, y_max) in [
            (-10.0, -5.0, 50.0, 45.0),
            (-0.4, 0.3, 40.4, 39.7),
            (-100.0, -100.0, 200.0, 200.0),
        ] {
            let mask = layers::rasterize_rect_for_test(40, 40, x_min, y_min, x_max, y_max);
            for y in 1..39 {
                for x in 0..40 {
                    let coverage = mask[y * 40 + x];
                    assert!(
                        (coverage - 1.0).abs() < 1e-4,
                        "rect ({x_min},{y_min})-({x_max},{y_max}) at ({x},{y}) got coverage {coverage}"
                    );
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_colr_emoji_renders_without_top_clipping_and_centered() {
        let Ok(data) = std::fs::read(r"C:\Windows\Fonts\seguiemj.ttf") else {
            return;
        };
        let font = ab_glyph::FontArc::try_from_vec(data).unwrap();
        for text in [
            "😀", "😂", "🎉", "🔥", "👍", "✨", "❤", "❤️", "😭", "💀", "🥰", "⭐",
        ] {
            let folded = fold(text);
            let ch = folded.chars().next().unwrap();
            let bitmap = render(&font, glyph_id(&font, ch))
                .unwrap_or_else(|| panic!("{text}: missing COLR emoji bitmap"));
            assert!(
                bitmap.em_height >= 0.75 && bitmap.em_height <= 1.25,
                "{text}: unexpected em_height={}",
                bitmap.em_height
            );
            // Painted pixels reach both top and bottom rows after tight cropping,
            // and the top row is not a flat wall of solid alpha across the full width.
            let top_solid = (0..bitmap.width)
                .filter(|&x| bitmap.rgba[(x * 4 + 3) as usize] >= 250)
                .count();
            assert!(
                (top_solid as f32) < bitmap.width as f32 * 0.65,
                "{text}: top row looks flat-clipped ({top_solid}/{} solid pixels)",
                bitmap.width
            );
            // Solid opaque core: guard against out-of-bounds clip-box rasterization
            // turning emoji translucent or hollow.
            let opaque_px = bitmap
                .rgba
                .chunks_exact(4)
                .filter(|pixel| pixel[3] >= 250)
                .count();
            assert!(
                opaque_px * 100 >= (bitmap.width * bitmap.height) as usize * 25,
                "{text}: too few opaque pixels ({opaque_px}/{}), bitmap looks translucent or hollow",
                bitmap.width * bitmap.height
            );
            // Alpha-weighted vertical centroid sits near the middle of the cropped bitmap.
            let mut sum_y = 0.0f64;
            let mut sum_a = 0.0f64;
            for y in 0..bitmap.height {
                for x in 0..bitmap.width {
                    let a = f64::from(bitmap.rgba[((y * bitmap.width + x) * 4 + 3) as usize]);
                    sum_y += (y as f64 + 0.5) * a;
                    sum_a += a;
                }
            }
            let centroid = sum_y / sum_a / f64::from(bitmap.height);
            assert!(
                (0.35..=0.65).contains(&centroid),
                "{text}: vertical centroid {centroid:.3} is off-center"
            );
        }
    }
}
