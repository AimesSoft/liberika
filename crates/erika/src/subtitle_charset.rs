//! Charset detection and transcoding for external text subtitles.
//!
//! FFmpeg's text subtitle demuxers assume UTF-8 input. External SRT/VTT/ASS
//! files authored on Windows frequently arrive as GBK/Big5/Shift_JIS and would
//! otherwise decode to mojibake. This module sniffs the raw bytes before they
//! reach the demuxer and rewrites legacy encodings to UTF-8 in memory.

use chardetng::EncodingDetector;

use crate::source::{ByteRange, MediaSource, Result};

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
const UTF16_LE_BOM: [u8; 2] = [0xFF, 0xFE];
const UTF16_BE_BOM: [u8; 2] = [0xFE, 0xFF];

/// Maximum tolerated fraction of garbled characters (U+FFFD replacements plus
/// non-whitespace control characters) in the decoded output. Above this the
/// detector guess is considered wrong and the original bytes pass through
/// untouched. Control characters are counted because single-byte fallbacks
/// such as windows-1252 decode arbitrary binary without ever emitting a
/// replacement character.
const MAX_GARBLED_RATIO: f64 = 0.05;

/// Outcome of charset inspection for an external text subtitle payload.
pub struct CharsetInspection {
    /// Name of the encoding that drove the decision, for diagnostics.
    pub detected: &'static str,
    /// UTF-8 bytes when the input required transcoding or BOM cleanup; `None` means the
    /// original bytes should be handed to FFmpeg unchanged.
    pub utf8: Option<Vec<u8>>,
}

/// Detects the character encoding of `bytes` and transcodes to UTF-8 when
/// necessary.
///
/// Returns `None` when the input already is valid UTF-8 (with or without a
/// single BOM — FFmpeg strips it itself, so it is preserved to avoid a
/// copy) or when detection is not confident
/// enough, in which case the caller should pass the original bytes through.
/// UTF-16 with a BOM and detected legacy encodings return `Some` with the
/// UTF-8 transcoding. Repeated leading UTF-8 BOMs are collapsed to one.
pub fn detect_and_transcode(bytes: &[u8]) -> Option<Vec<u8>> {
    inspect(bytes).utf8
}

/// Same as [`detect_and_transcode`] but also reports the detected encoding
/// name so callers can log the decision.
pub fn inspect(bytes: &[u8]) -> CharsetInspection {
    if bytes.starts_with(&UTF8_BOM) {
        // Some subtitle converters prepend a BOM to text that already has
        // one. FFmpeg strips only one; preserve a single BOM for probing.
        let mut start = 0;
        while bytes[start..].starts_with(&UTF8_BOM) {
            start += UTF8_BOM.len();
        }
        return CharsetInspection {
            detected: "UTF-8 (BOM)",
            utf8: (start > UTF8_BOM.len())
                .then(|| bytes[start - UTF8_BOM.len()..].to_vec()),
        };
    }
    if bytes.starts_with(&UTF16_LE_BOM) || bytes.starts_with(&UTF16_BE_BOM) {
        let encoding = if bytes.starts_with(&UTF16_LE_BOM) {
            encoding_rs::UTF_16LE
        } else {
            encoding_rs::UTF_16BE
        };
        // `decode` sniffs and strips the BOM before decoding the remainder.
        let (text, _, _) = encoding.decode(bytes);
        return CharsetInspection {
            detected: encoding.name(),
            utf8: Some(text.into_owned().into_bytes()),
        };
    }
    if std::str::from_utf8(bytes).is_ok() {
        return CharsetInspection {
            detected: "UTF-8",
            utf8: None,
        };
    }

    let mut detector = EncodingDetector::new();
    detector.feed(bytes, true);
    // UTF-8 was already ruled out above, so exclude it from the guess.
    let encoding = detector.guess(None, false);
    let (text, _, _) = encoding.decode(bytes);
    if garbled_ratio(&text) > MAX_GARBLED_RATIO {
        return CharsetInspection {
            detected: "unknown",
            utf8: None,
        };
    }
    CharsetInspection {
        detected: encoding.name(),
        utf8: Some(text.into_owned().into_bytes()),
    }
}

fn garbled_ratio(text: &str) -> f64 {
    let mut total = 0usize;
    let mut garbled = 0usize;
    for ch in text.chars() {
        total += 1;
        let control = ch.is_control() && !matches!(ch, '\t' | '\n' | '\r');
        if ch == '\u{FFFD}' || control {
            garbled += 1;
        }
    }
    if total == 0 {
        return 1.0;
    }
    garbled as f64 / total as f64
}

/// An in-memory [`MediaSource`] carrying a transcoded subtitle payload while
/// keeping the original URI for FFmpeg format probing.
#[derive(Debug)]
pub struct TranscodedMemorySource {
    uri: String,
    bytes: Vec<u8>,
}

impl TranscodedMemorySource {
    pub fn new(uri: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            uri: uri.into(),
            bytes,
        }
    }
}

impl MediaSource for TranscodedMemorySource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        Ok(Some(self.bytes.len() as u64))
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let total = self.bytes.len() as u64;
        if range.start >= total {
            // An empty read reports EOF through `CustomAvio`, matching the
            // past-the-end behavior of `LocalFileSource`.
            return Ok(Vec::new());
        }
        let start = range.start as usize;
        let end = match range.length {
            Some(length) => range.start.saturating_add(length).min(total) as usize,
            None => self.bytes.len(),
        };
        Ok(self.bytes[start..end].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GBK_SRT: &str = "1\n00:00:01,000 --> 00:00:03,000\n简体中文外挂字幕测试，第一条对白。\n\n2\n00:00:04,000 --> 00:00:06,000\n动漫爱好者经常遇到乱码问题。\n";
    const SHIFT_JIS_SRT: &str = "1\n00:00:01,000 --> 00:00:03,000\n日本語の外部字幕テストです。\n\n2\n00:00:04,000 --> 00:00:06,000\nアニメの字幕が文字化けしないこと。\n";

    #[test]
    fn transcodes_gbk_srt_to_utf8() {
        let (encoded, _, had_errors) = encoding_rs::GBK.encode(GBK_SRT);
        assert!(!had_errors);
        assert!(std::str::from_utf8(&encoded).is_err());

        let inspection = inspect(&encoded);
        let transcoded = inspection.utf8.expect("GBK input must be transcoded");
        let text = String::from_utf8(transcoded).unwrap();
        assert!(text.contains("简体中文外挂字幕测试"));
        assert!(text.contains("乱码问题"));
        assert_eq!(inspection.detected, "GBK");
    }

    #[test]
    fn transcodes_shift_jis_srt_to_utf8() {
        let (encoded, _, had_errors) = encoding_rs::SHIFT_JIS.encode(SHIFT_JIS_SRT);
        assert!(!had_errors);
        assert!(std::str::from_utf8(&encoded).is_err());

        let transcoded =
            detect_and_transcode(&encoded).expect("Shift_JIS input must be transcoded");
        let text = String::from_utf8(transcoded).unwrap();
        assert!(text.contains("日本語の外部字幕テストです"));
        assert!(text.contains("文字化け"));
    }

    #[test]
    fn passes_plain_utf8_through() {
        assert!(detect_and_transcode(GBK_SRT.as_bytes()).is_none());
        assert_eq!(inspect(GBK_SRT.as_bytes()).detected, "UTF-8");
    }

    #[test]
    fn passes_utf8_bom_through_with_bom_preserved() {
        // Choice: the UTF-8 BOM stays in the passthrough bytes. FFmpeg's text
        // subtitle demuxers strip a UTF-8 BOM themselves, so no copy is made.
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(GBK_SRT.as_bytes());
        assert!(detect_and_transcode(&bytes).is_none());
        assert_eq!(inspect(&bytes).detected, "UTF-8 (BOM)");
    }

    #[test]
    fn collapses_repeated_leading_utf8_boms_without_changing_subtitle_text() {
        let text = "[Script Info]\nTitle: keep an interior \u{feff} character\n";
        for count in [2, 3] {
            let mut bytes = UTF8_BOM.repeat(count);
            bytes.extend_from_slice(text.as_bytes());
            let mut expected = UTF8_BOM.to_vec();
            expected.extend_from_slice(text.as_bytes());
            assert_eq!(detect_and_transcode(&bytes), Some(expected));
        }
    }

    #[test]
    fn transcodes_utf16le_bom_to_utf8_without_bom() {
        let mut bytes = UTF16_LE_BOM.to_vec();
        for unit in GBK_SRT.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let transcoded = detect_and_transcode(&bytes).expect("UTF-16LE input must be transcoded");
        assert_eq!(String::from_utf8(transcoded).unwrap(), GBK_SRT);
    }

    #[test]
    fn transcodes_utf16be_bom_to_utf8_without_bom() {
        let mut bytes = UTF16_BE_BOM.to_vec();
        for unit in SHIFT_JIS_SRT.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        let transcoded = detect_and_transcode(&bytes).expect("UTF-16BE input must be transcoded");
        assert_eq!(String::from_utf8(transcoded).unwrap(), SHIFT_JIS_SRT);
    }

    #[test]
    fn rejects_binary_garbage() {
        // PGS-like header followed by cyclic bytes: invalid as UTF-8 and full
        // of control characters under any single-byte fallback decoding.
        let mut garbage = vec![0x50, 0x47, 0x00, 0x00, 0x00, 0x01];
        garbage.extend((0u32..600).map(|value| (value % 251) as u8));
        assert!(std::str::from_utf8(&garbage).is_err());

        let inspection = inspect(&garbage);
        assert!(inspection.utf8.is_none());
        assert_eq!(inspection.detected, "unknown");
    }

    #[test]
    fn memory_source_read_range_matches_local_file_eof_semantics() {
        let mut source =
            TranscodedMemorySource::new("file:///tmp/subtitle.srt", b"hello world".to_vec());

        assert_eq!(source.uri(), "file:///tmp/subtitle.srt");
        assert_eq!(source.len().unwrap(), Some(11));
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(5),
                })
                .unwrap(),
            b"hello"
        );
        // `length: None` reads to the end.
        assert_eq!(
            source.read_range(ByteRange::suffix_from(6)).unwrap(),
            b"world"
        );
        // Over-long reads clamp to the end.
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 6,
                    length: Some(100),
                })
                .unwrap(),
            b"world"
        );
        // Reads at or past the end return empty (EOF for `CustomAvio`).
        assert!(
            source
                .read_range(ByteRange::suffix_from(11))
                .unwrap()
                .is_empty()
        );
        assert!(
            source
                .read_range(ByteRange {
                    start: 100,
                    length: Some(4),
                })
                .unwrap()
                .is_empty()
        );
    }
}
