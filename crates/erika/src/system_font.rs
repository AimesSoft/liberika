//! The fonts a device already has, for text nothing else brings a font for.
//!
//! Subtitles and danmaku can carry fonts of their own or have one chosen by the
//! application. Everything else is drawn with what the system offers, so a
//! player needs no font of its own.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Families worth trying for text the requested font lacks, CJK-capable first.
pub(crate) const PREFERRED_FAMILIES: &[&str] = &[
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

/// One face of a font file on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemFont {
    pub path: PathBuf,
    /// The face within a font collection; 0 for a plain font file.
    pub index: u32,
}

/// The face that stands in for text nothing else covers.
pub fn fallback() -> Option<&'static SystemFont> {
    candidates().first()
}

/// Faces worth standing in for missing text, best first.
fn candidates() -> &'static [SystemFont] {
    static CANDIDATES: OnceLock<Vec<SystemFont>> = OnceLock::new();
    CANDIDATES.get_or_init(platform::candidates)
}

/// Every font on this device, read once. Reading parses each file's header, so
/// nothing asks for this until a character is missing from the fallback face.
pub(crate) fn database() -> &'static fontdb::Database {
    static DATABASE: OnceLock<fontdb::Database> = OnceLock::new();
    DATABASE.get_or_init(|| {
        let mut database = fontdb::Database::new();
        platform::load(&mut database);
        database
    })
}

#[cfg(not(any(
    target_os = "android",
    target_env = "ohos",
    target_os = "ios",
    target_os = "tvos"
)))]
mod platform {
    use super::*;

    pub(super) fn load(database: &mut fontdb::Database) {
        database.load_system_fonts();
    }

    /// macOS has fonts whose outlines are in Apple's `hvgl` table, which neither
    /// FreeType nor ab_glyph can read.
    fn has_outlines(database: &fontdb::Database, id: fontdb::ID) -> bool {
        use ab_glyph::Font;
        database
            .with_face_data(id, |data, index| {
                ab_glyph::FontRef::try_from_slice_and_index(data, index).is_ok_and(|font| {
                    font.outline(font.glyph_id('A')).is_some()
                        || font.outline(font.glyph_id('中')).is_some()
                })
            })
            .unwrap_or(false)
    }

    pub(super) fn candidates() -> Vec<SystemFont> {
        let database = database();
        let mut fonts = Vec::new();
        let families = PREFERRED_FAMILIES
            .iter()
            .map(|family| fontdb::Family::Name(family))
            .chain([fontdb::Family::SansSerif]);
        for family in families {
            let query = fontdb::Query {
                families: &[family],
                weight: fontdb::Weight::NORMAL,
                stretch: fontdb::Stretch::Normal,
                style: fontdb::Style::Normal,
            };
            let Some(id) = database
                .query(&query)
                .filter(|id| has_outlines(database, *id))
            else {
                continue;
            };
            if let Some((fontdb::Source::File(path), index)) = database.face_source(id) {
                let font = SystemFont { path, index };
                if !fonts.contains(&font) {
                    fonts.push(font);
                }
            }
        }
        fonts
    }
}

/// Android and OpenHarmony keep their fonts in known folders; libass and fontdb
/// have no way of their own to look there.
#[cfg(any(target_os = "android", target_env = "ohos"))]
const FONT_FOLDERS: &[&str] = &["/system/fonts", "/product/fonts", "/system_ext/fonts"];

#[cfg(any(target_os = "android", target_env = "ohos"))]
fn load_font_folders(database: &mut fontdb::Database) {
    for folder in FONT_FOLDERS {
        database.load_fonts_dir(folder);
    }
}

/// The folder a font file named by the system is in.
#[cfg(any(target_os = "android", target_env = "ohos"))]
fn locate(file: &str, index: u32) -> Option<SystemFont> {
    FONT_FOLDERS.iter().find_map(|folder| {
        let path = std::path::Path::new(folder).join(file);
        path.is_file().then_some(SystemFont { path, index })
    })
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;

    /// Android 15 keeps the current list in the first; earlier versions only
    /// have the second.
    const LISTS: &[&str] = &["/system/etc/font_fallback.xml", "/system/etc/fonts.xml"];
    const LANGUAGES: &[&str] = &["zh-Hans", "zh-Hant", "ja", "ko"];
    /// For a system whose list names none of [`LANGUAGES`].
    const KNOWN_FILES: &[(&str, u32)] = &[
        ("NotoSansCJK-Regular.ttc", 2),
        ("NotoSansSC-Regular.otf", 0),
        ("DroidSansFallback.ttf", 0),
        ("DroidSansFallbackFull.ttf", 0),
        ("Roboto-Regular.ttf", 0),
    ];

    pub(super) fn load(database: &mut fontdb::Database) {
        load_font_folders(database);
    }

    pub(super) fn candidates() -> Vec<SystemFont> {
        let mut fonts = Vec::new();
        for list in LISTS {
            let Ok(xml) = std::fs::read_to_string(list) else {
                continue;
            };
            for language in LANGUAGES {
                let font = super::font_list::for_language(&xml, language)
                    .and_then(|(file, index)| locate(&file, index));
                if let Some(font) = font
                    && !fonts.contains(&font)
                {
                    fonts.push(font);
                }
            }
        }
        for (file, index) in KNOWN_FILES {
            if let Some(font) = locate(file, *index)
                && !fonts.contains(&font)
            {
                fonts.push(font);
            }
        }
        fonts
    }
}

#[cfg(target_env = "ohos")]
mod platform {
    use super::*;

    const KNOWN_FILES: &[(&str, u32)] = &[
        ("HarmonyOS_Sans_SC.ttf", 0),
        ("HarmonyOS_Sans_SC_Regular.ttf", 0),
        ("NotoSansCJK-Regular.ttc", 2),
        ("HarmonyOS_Sans.ttf", 0),
    ];

    pub(super) fn load(database: &mut fontdb::Database) {
        load_font_folders(database);
    }

    pub(super) fn candidates() -> Vec<SystemFont> {
        KNOWN_FILES
            .iter()
            .filter_map(|(file, index)| locate(file, *index))
            .collect()
    }
}

/// Nothing here stands in for the embedded face on iOS and tvOS: Chinese is only
/// in PingFang, whose outlines are in Apple's `hvgl` table, which nothing here
/// can read. The system's other fonts still cover what that face lacks.
#[cfg(any(target_os = "ios", target_os = "tvos"))]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;

    type CfType = *const c_void;

    const PATH_BYTES: usize = 4096;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: CfType);
        fn CFURLGetFileSystemRepresentation(
            url: CfType,
            resolve: u8,
            buffer: *mut u8,
            length: isize,
        ) -> u8;
        fn CFArrayGetCount(array: CfType) -> isize;
        fn CFArrayGetValueAtIndex(array: CfType, index: isize) -> CfType;
    }

    #[link(name = "CoreText", kind = "framework")]
    unsafe extern "C" {
        static kCTFontURLAttribute: CfType;
        fn CTFontCollectionCreateFromAvailableFonts(options: CfType) -> CfType;
        fn CTFontCollectionCreateMatchingFontDescriptors(collection: CfType) -> CfType;
        fn CTFontDescriptorCopyAttribute(descriptor: CfType, attribute: CfType) -> CfType;
    }

    /// A Core Foundation object this code owns.
    struct Owned(CfType);

    impl Owned {
        fn new(value: CfType) -> Option<Self> {
            (!value.is_null()).then_some(Self(value))
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }

    fn path_of(url: &Owned) -> Option<PathBuf> {
        let mut buffer = vec![0u8; PATH_BYTES];
        let converted = unsafe {
            CFURLGetFileSystemRepresentation(url.0, 1, buffer.as_mut_ptr(), PATH_BYTES as isize)
        };
        if converted == 0 {
            return None;
        }
        let length = buffer.iter().position(|byte| *byte == 0)?;
        buffer.truncate(length);
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer)))
    }

    pub(super) fn candidates() -> Vec<SystemFont> {
        Vec::new()
    }

    pub(super) fn load(database: &mut fontdb::Database) {
        let Some(collection) =
            Owned::new(unsafe { CTFontCollectionCreateFromAvailableFonts(std::ptr::null()) })
        else {
            return;
        };
        let Some(descriptors) =
            Owned::new(unsafe { CTFontCollectionCreateMatchingFontDescriptors(collection.0) })
        else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        for index in 0..unsafe { CFArrayGetCount(descriptors.0) } {
            let descriptor = unsafe { CFArrayGetValueAtIndex(descriptors.0, index) };
            let url = Owned::new(unsafe {
                CTFontDescriptorCopyAttribute(descriptor, kCTFontURLAttribute)
            });
            if let Some(path) = url.as_ref().and_then(path_of)
                && seen.insert(path.clone())
            {
                let _ = database.load_font_file(path);
            }
        }
    }
}

/// Reads Android's font list: `<family lang="zh-Hans"><font index="2">file…`.
#[cfg(any(target_os = "android", test))]
mod font_list {
    /// The file and face the list gives for `language`.
    pub(super) fn for_language(xml: &str, language: &str) -> Option<(String, u32)> {
        let xml = without_comments(xml);
        xml.split("<family").skip(1).find_map(|family| {
            let (attributes, body) = family.split_once('>')?;
            let languages = attribute(attributes, "lang")?;
            languages
                .split([',', ' '])
                .any(|tag| tag == language)
                .then(|| first_font(body))
                .flatten()
        })
    }

    fn without_comments(xml: &str) -> String {
        let mut text = String::with_capacity(xml.len());
        let mut rest = xml;
        while let Some((before, after)) = rest.split_once("<!--") {
            text.push_str(before);
            rest = after.split_once("-->").map_or("", |(_, after)| after);
        }
        text.push_str(rest);
        text
    }

    fn attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
        let key = format!("{name}=\"");
        let start = attributes.find(&key)? + key.len();
        attributes[start..].split('"').next()
    }

    /// The family's first sans-serif face: a `fallbackFor` one stands in for
    /// another style, such as serif.
    fn first_font(family: &str) -> Option<(String, u32)> {
        let family = family.split("</family>").next()?;
        family.split("<font").skip(1).find_map(|font| {
            let (attributes, text) = font.split_once('>')?;
            if attributes.contains("fallbackFor=") {
                return None;
            }
            let file = text
                .split(['<', ' ', '\n', '\t'])
                .find(|part| !part.is_empty())?;
            let index =
                attribute(attributes, "index").map_or(Some(0), |index| index.parse().ok())?;
            Some((file.to_string(), index))
        })
    }

    #[cfg(test)]
    mod tests {
        use super::for_language;

        const LIST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<familyset version="23">
    <!-- <family lang="zh-Hans"><font>Commented.ttf</font></family> -->
    <family name="sans-serif">
        <font weight="400" style="normal">Roboto-Regular.ttf
          <axis tag="wdth" stylevalue="100" />
        </font>
    </family>
    <family lang="zh-Hans">
        <font weight="400" style="normal" index="2" fallbackFor="serif"
              postScriptName="NotoSerifCJKjp-Regular">NotoSerifCJK-Regular.ttc</font>
        <font weight="400" style="normal" index="2"
              postScriptName="NotoSansCJKjp-Regular">NotoSansCJK-Regular.ttc</font>
    </family>
    <family lang="zh-Hant,zh-Bopo">
        <font weight="400" style="normal" index="3">NotoSansCJK-Regular.ttc</font>
    </family>
    <family lang="ko">
        <font weight="400" style="normal">Vendor Korean.ttf</font>
    </family>
</familyset>
"#;

        #[test]
        fn a_language_finds_its_first_sans_serif_face() {
            assert_eq!(
                for_language(LIST, "zh-Hans"),
                Some(("NotoSansCJK-Regular.ttc".to_string(), 2))
            );
        }

        #[test]
        fn a_family_may_serve_several_languages() {
            assert_eq!(
                for_language(LIST, "zh-Bopo"),
                Some(("NotoSansCJK-Regular.ttc".to_string(), 3))
            );
        }

        #[test]
        fn a_face_without_an_index_is_the_first_of_its_file() {
            assert_eq!(for_language(LIST, "ko"), Some(("Vendor".to_string(), 0)));
        }

        #[test]
        fn a_language_nobody_lists_finds_nothing() {
            assert_eq!(for_language(LIST, "ja"), None);
        }
    }
}
