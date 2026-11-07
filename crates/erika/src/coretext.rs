//! What only the system can draw from Apple's fonts.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

type CfType = *const c_void;

const UTF8: u32 = 0x0800_0100;
/// Premultiplied, with the bytes in R, G, B, A order.
const RGBA: u32 = 1 | (4 << 12);

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: CfType);
    fn CFStringCreateWithCString(allocator: CfType, text: *const c_char, encoding: u32) -> CfType;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> CfType;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CfType,
        info: u32,
    ) -> CfType;
}

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    fn CTFontCreateWithName(name: CfType, size: f64, matrix: *const c_void) -> CfType;
    fn CTFontDrawGlyphs(
        font: CfType,
        glyphs: *const u16,
        positions: *const Point,
        count: usize,
        context: CfType,
    );
}

/// A Core Foundation object this code owns.
struct Owned(CfType);

// SAFETY: fonts and color spaces are immutable and safe to use from any thread.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Owned {
    fn new(value: CfType) -> Option<Self> {
        (!value.is_null()).then_some(Self(value))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: this holds the only reference it was created with.
        unsafe { CFRelease(self.0) };
    }
}

/// A glyph as the system drew it, cropped to what it painted. `left` is from
/// the glyph's origin, in pixels.
pub(crate) struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub left: i32,
}

fn font_named(name: &std::ffi::CStr, size: f64) -> Option<Owned> {
    // SAFETY: the name is a C string, and the CFString lives past the call.
    unsafe {
        let name = Owned::new(CFStringCreateWithCString(
            std::ptr::null(),
            name.as_ptr(),
            UTF8,
        ))?;
        Owned::new(CTFontCreateWithName(name.0, size, std::ptr::null()))
    }
}

fn emoji_font(size: u32) -> Option<&'static Owned> {
    static FONT: OnceLock<Option<Owned>> = OnceLock::new();
    FONT.get_or_init(|| font_named(c"AppleColorEmoji", f64::from(size)))
        .as_ref()
}

/// Glyph `glyph` of Apple Color Emoji, `size` pixels to the em.
pub(crate) fn draw_emoji(glyph: u16, size: u32) -> Option<Picture> {
    let font = emoji_font(size)?;
    // Room around the origin: a picture can be wider than the em.
    let side = size as usize * 2;
    let origin = size as usize / 2;
    let mut pixels = vec![0u8; side * side * 4];
    // SAFETY: `pixels` fits the context's size and outlives it.
    unsafe {
        let space = Owned::new(CGColorSpaceCreateDeviceRGB())?;
        let context = Owned::new(CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            side,
            side,
            8,
            side * 4,
            space.0,
            RGBA,
        ))?;
        let position = Point {
            x: origin as f64,
            y: origin as f64,
        };
        CTFontDrawGlyphs(font.0, &glyph, &position, 1, context.0);
    }
    crop(&pixels, side, origin)
}

fn crop(pixels: &[u8], side: usize, origin: usize) -> Option<Picture> {
    let painted = |x: usize, y: usize| pixels[(y * side + x) * 4 + 3] != 0;
    let rows = (0..side).filter(|y| (0..side).any(|x| painted(x, *y)));
    let top = rows.clone().next()?;
    let bottom = rows.last()?;
    let columns = (0..side).filter(|x| (top..=bottom).any(|y| painted(*x, y)));
    let left = columns.clone().next()?;
    let right = columns.last()?;
    let width = right - left + 1;
    let height = bottom - top + 1;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in top..=bottom {
        let start = (y * side + left) * 4;
        rgba.extend_from_slice(&pixels[start..start + width * 4]);
    }
    Some(Picture {
        width: width as u32,
        height: height as u32,
        rgba,
        left: left as i32 - origin as i32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_is_cropped_to_what_was_painted() {
        let side = 8;
        let mut pixels = vec![0u8; side * side * 4];
        for (x, y) in [(5, 2), (6, 2), (6, 4)] {
            pixels[(y * side + x) * 4 + 3] = 255;
        }
        let picture = crop(&pixels, side, 4).unwrap();
        assert_eq!((picture.width, picture.height, picture.left), (2, 3, 1));
        assert_eq!(picture.rgba.len(), 2 * 3 * 4);
    }

    #[test]
    fn nothing_painted_is_no_picture() {
        assert!(crop(&[0; 4 * 4 * 4], 4, 2).is_none());
    }

    #[test]
    fn the_system_draws_an_emoji_in_color() {
        use ab_glyph::Font;
        let Ok(data) = std::fs::read("/System/Library/Fonts/Apple Color Emoji.ttc") else {
            return;
        };
        let font = ab_glyph::FontRef::try_from_slice_and_index(&data, 0).unwrap();
        let picture = draw_emoji(font.glyph_id('😀').0, 96).unwrap();
        assert!(picture.width > 48 && picture.height > 48);
        let colored = picture
            .rgba
            .chunks_exact(4)
            .any(|pixel| pixel[0] != pixel[1] && pixel[3] == 255);
        assert!(colored);
    }
}
