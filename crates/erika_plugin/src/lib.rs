//! The playback half of liberika's JavaScript plugin interface: the
//! `player` and `danmaku` objects plugins call, for any player built on
//! liberika. It needs nothing native, so a host can run plugins wherever it
//! runs; `erika::plugin` adds the conversions to and from danmaku timelines.
//!
//! A host runs each plugin in a JavaScript context of its own and defines a
//! `__host` object there before [`PRELUDE`]:
//!
//! - `__host.has(permission)` tells whether the plugin declared a
//!   permission, such as [`PLAYER_CONTROL`];
//! - `__host.read(name)` returns what the host last told the context, by
//!   name: `"player"` (a [`PlayerState`]), `"danmaku.offset"` (seconds);
//! - `__host.call(name, args)` queues a call for the host.
//!
//! The host reads the queued calls back, parses each with [`Call::parse`] and
//! applies it to its player. Nothing here waits for the host: the calls work
//! the same in an embedded engine and in a system web view.

use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};

/// The `player` and `danmaku` objects, NipaPlay's plugin interface with
/// rates and offsets added; `danmaku.checkSimilarity` and
/// `danmaku.pairSimilarity` run in the context itself.
pub const PRELUDE: &str = include_str!("prelude.js");

/// The permission playing, pausing, seeking and the rate need.
pub const PLAYER_CONTROL: &str = "player.control";
/// The permission every `danmaku` call needs.
pub const DANMAKU_MODIFY: &str = "danmaku.modify";

/// A call a plugin queued.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Play,
    Pause,
    /// Seconds from the start.
    Seek(f64),
    /// The playback rate, 1 for normal speed.
    SetRate(f64),
    ShowDanmaku,
    HideDanmaku,
    /// From 0 to 1.
    SetDanmakuOpacity(f64),
    /// A block-list entry: plain text, or `name/pattern/`.
    AddFilter(String),
    RemoveFilter(String),
    /// The comments shown from now on, in place of those loaded.
    ReplaceDanmaku(Vec<Comment>),
    /// Seconds the comments are shown later than their time; negative is
    /// earlier.
    SetDanmakuOffset(f64),
}

impl Call {
    /// The call `name` with `args`, as the prelude queues it; nothing for a
    /// call this interface does not know or arguments it cannot use.
    pub fn parse(name: &str, args: &[Value]) -> Option<Self> {
        let number = |index: usize| args.get(index).and_then(number_value);
        let text = |index: usize| args.get(index).and_then(text_value);
        Some(match name {
            "playerPlay" => Call::Play,
            "playerPause" => Call::Pause,
            "playerSeek" => Call::Seek(number(0)?.max(0.0)),
            "playerSetRate" => Call::SetRate(number(0).filter(|rate| *rate > 0.0)?),
            "danmakuShow" => Call::ShowDanmaku,
            "danmakuHide" => Call::HideDanmaku,
            "danmakuSetOpacity" => Call::SetDanmakuOpacity(number(0)?.clamp(0.0, 1.0)),
            // NipaPlay's `addFilter(filterId, pattern)`: the pattern counts.
            "danmakuAddFilter" => Call::AddFilter(text(1).or_else(|| text(0))?),
            "danmakuRemoveFilter" => Call::RemoveFilter(text(0)?),
            "danmakuReplace" => Call::ReplaceDanmaku(replacement(args.first()?)?),
            "danmakuSetOffset" => Call::SetDanmakuOffset(number(0)?),
            _ => return None,
        })
    }

    /// The permission the plugin must have declared for the call.
    pub fn permission(&self) -> &'static str {
        match self {
            Call::Play | Call::Pause | Call::Seek(_) | Call::SetRate(_) => PLAYER_CONTROL,
            _ => DANMAKU_MODIFY,
        }
    }
}

fn number_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|number| number.is_finite())
}

fn text_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
    .filter(|text| !text.trim().is_empty())
}

/// `danmaku.replace`'s argument: `{ count, comments }`, a list of comments,
/// or either of them as JSON text.
fn replacement(value: &Value) -> Option<Vec<Comment>> {
    let list = match value {
        Value::String(text) => return replacement(&serde_json::from_str(text).ok()?),
        Value::Array(list) => list,
        Value::Object(object) => object.get("comments")?.as_array()?,
        _ => return None,
    };
    Some(list.iter().filter_map(Comment::from_value).collect())
}

/// What `player.getState()` gives.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    /// Seconds played, and the length of the media.
    pub position: f64,
    pub duration: f64,
    pub status: PlayerStatus,
    pub has_video: bool,
    pub rate: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayerStatus {
    #[default]
    Idle,
    Playing,
    Paused,
}

/// A comment as `danmakuLoaded` gives it and `danmaku.replace` takes it:
/// seconds, text, `scroll`, `top` or `bottom`, and `rgb(r,g,b)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub time: f64,
    pub content: String,
    pub kind: CommentKind,
    pub color: [u8; 3],
    /// Sent by the one watching.
    pub is_me: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    Scroll,
    Top,
    Bottom,
}

impl Comment {
    /// A comment in any of the shapes plugins write; nothing without text.
    pub fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let field = |names: &[&str]| names.iter().find_map(|name| object.get(*name));
        let content = field(&["content", "text", "c"]).and_then(text_value)?;
        let kind = match field(&["type", "mode"]) {
            Some(Value::Number(code)) => match code.as_u64() {
                Some(4) | Some(2) => CommentKind::Bottom,
                Some(5) | Some(1) => CommentKind::Top,
                _ => CommentKind::Scroll,
            },
            Some(Value::String(name)) => match name.trim().to_ascii_lowercase().as_str() {
                "top" | "5" => CommentKind::Top,
                "bottom" | "4" => CommentKind::Bottom,
                _ => CommentKind::Scroll,
            },
            _ => CommentKind::Scroll,
        };
        Some(Comment {
            time: field(&["time", "t"]).and_then(number_value).unwrap_or(0.0).max(0.0),
            content,
            kind,
            color: field(&["color", "r"]).and_then(color_value).unwrap_or([255; 3]),
            is_me: field(&["isMe", "is_me"]).and_then(Value::as_bool).unwrap_or(false),
        })
    }

    pub fn to_value(&self) -> Value {
        let [red, green, blue] = self.color;
        let mut object = Map::new();
        object.insert("time".into(), json!(self.time));
        object.insert("content".into(), json!(self.content));
        object.insert(
            "type".into(),
            json!(match self.kind {
                CommentKind::Scroll => "scroll",
                CommentKind::Top => "top",
                CommentKind::Bottom => "bottom",
            }),
        );
        object.insert("color".into(), json!(format!("rgb({red},{green},{blue})")));
        if self.is_me {
            object.insert("isMe".into(), json!(true));
        }
        Value::Object(object)
    }
}

fn color_value(value: &Value) -> Option<[u8; 3]> {
    let rgb = |decimal: u64| {
        let decimal = decimal as u32;
        [(decimal >> 16) as u8, (decimal >> 8) as u8, decimal as u8]
    };
    match value {
        Value::Number(number) => number.as_u64().map(rgb),
        Value::String(text) => {
            let text = text.trim();
            if let Some(hex) = text.strip_prefix('#') {
                return u64::from_str_radix(hex, 16).ok().map(rgb);
            }
            if let Some(inner) = text.strip_prefix("rgb(").and_then(|t| t.strip_suffix(')')) {
                let parts: Vec<u8> = inner
                    .split(',')
                    .filter_map(|part| part.trim().parse().ok())
                    .collect();
                return (parts.len() >= 3).then(|| [parts[0], parts[1], parts[2]]);
            }
            text.parse().ok().map(rgb)
        }
        _ => None,
    }
}

/// The comments as `danmakuLoaded`'s `danmaku` and the danmaku JSON
/// reader take them.
pub fn comments_json(comments: &[Comment]) -> Value {
    Value::Array(comments.iter().map(Comment::to_value).collect())
}

/// Entries that hide comments: plain text hides a comment containing it,
/// `name/pattern/` one a regular expression matches. An entry whose
/// pattern does not compile counts as plain text.
#[derive(Debug, Clone, Default)]
pub struct BlockList {
    words: Vec<String>,
    patterns: Vec<Regex>,
}

impl BlockList {
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut list = BlockList::default();
        for entry in entries {
            let entry = entry.as_ref().trim();
            if entry.is_empty() {
                continue;
            }
            match pattern_of(entry).and_then(|pattern| Regex::new(pattern).ok()) {
                Some(regex) => list.patterns.push(regex),
                None => list.words.push(entry.to_string()),
            }
        }
        list
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.patterns.is_empty()
    }

    pub fn blocks(&self, text: &str) -> bool {
        self.words.iter().any(|word| text.contains(word.as_str()))
            || self.patterns.iter().any(|pattern| pattern.is_match(text))
    }
}

/// The pattern of `name/pattern/`.
fn pattern_of(entry: &str) -> Option<&str> {
    let inner = entry.strip_suffix('/')?;
    let (_, pattern) = inner.split_once('/')?;
    (!pattern.is_empty()).then_some(pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_parse_as_the_prelude_queues_them() {
        assert_eq!(Call::parse("playerSeek", &[json!("12.5")]), Some(Call::Seek(12.5)));
        assert_eq!(Call::parse("playerSetRate", &[json!(0)]), None);
        assert_eq!(
            Call::parse("danmakuAddFilter", &[json!("id"), json!("剧透")]),
            Some(Call::AddFilter("剧透".into()))
        );
        let replaced = Call::parse(
            "danmakuReplace",
            &[json!({ "count": 1, "comments": [{ "time": 3, "content": "好", "type": "top", "color": "rgb(255,0,0)" }] })],
        );
        let Some(Call::ReplaceDanmaku(comments)) = replaced else {
            panic!("a replacement");
        };
        assert_eq!(comments[0].kind, CommentKind::Top);
        assert_eq!(comments[0].color, [255, 0, 0]);
        assert_eq!(Call::Play.permission(), PLAYER_CONTROL);
        assert_eq!(Call::HideDanmaku.permission(), DANMAKU_MODIFY);
        assert_eq!(Call::parse("unknown", &[]), None);
    }

    #[test]
    fn block_lists_hide_words_and_patterns() {
        let list = BlockList::new(["剧透", "刷屏/[哈]{5,}/", "坏/[/"]);
        assert!(list.blocks("前方剧透"));
        assert!(list.blocks("哈哈哈哈哈哈"));
        assert!(!list.blocks("哈哈"));
        // A pattern that does not compile is plain text.
        assert!(list.blocks("坏/[/"));
        assert!(!list.blocks("好看"));
    }
}
