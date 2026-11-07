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
        let ascender = f32::from(face.ascender());
        let descender = f32::from(face.descender());
        let advance = f32::from(face.glyph_hor_advance(glyph)?);
        let scale = PICTURE_HEIGHT as f32 / (ascender - descender).max(1.0);
        let width = (advance * scale).ceil().max(1.0) as u32;
        let height = PICTURE_HEIGHT;
        let base = Transform::new(scale, 0.0, 0.0, -scale, 0.0, ascender * scale);
        let mut canvas = Canvas::new(&face, width, height, base);
        face.paint_color_glyph(glyph, 0, FOREGROUND, &mut canvas)?;
        Some(ColorBitmap {
            width,
            height,
            rgba: canvas.finish(),
            left: 0.0,
            em_width: advance / units,
            em_height: (ascender - descender) / units,
            advance: advance / units,
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
            let mut path = Path {
                transform: self.transform(),
                rasterizer: Rasterizer::new(self.width as usize, self.height as usize),
                start: Point { x: 0.0, y: 0.0 },
                pen: Point { x: 0.0, y: 0.0 },
            };
            build(&mut path);
            let mut mask = vec![0.0; (self.width * self.height) as usize];
            let width = self.width as usize;
            path.rasterizer.for_each_pixel_2d(|x, y, coverage| {
                mask[y as usize * width + x as usize] = coverage.abs().min(1.0);
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
    struct Path {
        transform: Transform,
        rasterizer: Rasterizer,
        start: Point,
        pen: Point,
    }

    impl Path {
        fn map(&self, x: f32, y: f32) -> Point {
            let [x, y] = apply(self.transform, x, y);
            Point { x, y }
        }
    }

    impl OutlineBuilder for Path {
        fn move_to(&mut self, x: f32, y: f32) {
            self.pen = self.map(x, y);
            self.start = self.pen;
        }

        fn line_to(&mut self, x: f32, y: f32) {
            let to = self.map(x, y);
            self.rasterizer.draw_line(self.pen, to);
            self.pen = to;
        }

        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            let (control, to) = (self.map(x1, y1), self.map(x, y));
            self.rasterizer.draw_quad(self.pen, control, to);
            self.pen = to;
        }

        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
            let (first, second, to) = (self.map(x1, y1), self.map(x2, y2), self.map(x, y));
            self.rasterizer.draw_cubic(self.pen, first, second, to);
            self.pen = to;
        }

        fn close(&mut self) {
            self.rasterizer.draw_line(self.pen, self.start);
            self.pen = self.start;
        }
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
}
