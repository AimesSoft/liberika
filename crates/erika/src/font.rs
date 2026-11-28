//! Font data shared by the native and browser text backends.
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleFontAttachment {
    pub name: String,
    pub mime_type: Option<String>,
    pub families: Vec<String>,
    pub data: Arc<[u8]>,
}

pub const MAX_MEMORY_SUBTITLE_FONT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_MEMORY_SUBTITLE_FONT_TOTAL_BYTES: usize = 128 * 1024 * 1024;

impl SubtitleFontAttachment {
    pub fn new(
        name: impl Into<String>,
        mime_type: Option<String>,
        families: Vec<String>,
        data: impl Into<Arc<[u8]>>,
    ) -> Self {
        Self {
            name: name.into(),
            mime_type,
            families,
            data: data.into(),
        }
    }

    pub fn byte_len(&self) -> usize {
        self.data.len()
    }
}

/// The face that stands in for text nothing else covers, where the device's
/// fonts are out of reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackFont {
    pub data: Arc<[u8]>,
    /// The face within a font collection; 0 for a plain font file.
    pub index: u32,
}

impl FallbackFont {
    /// The names libass may find this face by, separated as liberika's patch
    /// reads its default family. Which name libass files a face under depends
    /// on the font, so all of them go.
    pub fn families(&self) -> Option<String> {
        let face = ttf_parser::Face::parse(&self.data, self.index).ok()?;
        let mut names = Vec::new();
        for name in face.names() {
            if matches!(
                name.name_id,
                ttf_parser::name_id::FAMILY
                    | ttf_parser::name_id::FULL_NAME
                    | ttf_parser::name_id::TYPOGRAPHIC_FAMILY
            ) && let Some(text) = name.to_string()
                && !names.contains(&text)
            {
                names.push(text);
            }
        }
        (!names.is_empty()).then(|| names.join("\u{1f}"))
    }
}

static GIVEN_FALLBACK: RwLock<Option<FallbackFont>> = RwLock::new(None);

/// Give the player its fallback face. A browser page cannot read the device's
/// fonts, so whoever serves it hands one over.
pub fn set_fallback_font(font: Option<FallbackFont>) {
    *GIVEN_FALLBACK
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = font;
}

fn given_fallback_font() -> Option<FallbackFont> {
    GIVEN_FALLBACK
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The font built into this library, if it was built with one. A host serving
/// a page can hand this over, so that the two need only one copy between them.
pub fn embedded_fallback_font() -> Option<&'static [u8]> {
    #[cfg(any(test, feature = "embedded-fallback-font"))]
    return Some(crate::NIPAPLAY_FALLBACK_FONT);
    #[cfg(not(any(test, feature = "embedded-fallback-font")))]
    None
}

/// The face given to the player, else the one built in, if any.
pub(crate) fn fallback_font() -> Option<FallbackFont> {
    given_fallback_font().or_else(|| {
        embedded_fallback_font().map(|data| FallbackFont {
            data: Arc::from(data),
            index: 0,
        })
    })
}

/// Whether the player was given a face, which then goes before the system's.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn has_given_fallback_font() -> bool {
    given_fallback_font().is_some()
}
