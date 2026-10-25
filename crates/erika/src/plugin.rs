//! The JavaScript plugin interface for players built on this crate: the
//! `erika_plugin` crate, with its comments read from and made into danmaku
//! timelines.

pub use erika_plugin::*;

use crate::danmaku::{DanmakuItem, DanmakuMode, DanmakuTimeline};

/// A comment as plugins see it, of a timeline's item.
pub fn comment(item: &DanmakuItem) -> Comment {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Comment {
        time: item.pts.as_secs_f64(),
        content: item.text.clone(),
        kind: match item.mode {
            DanmakuMode::Top => CommentKind::Top,
            DanmakuMode::Bottom => CommentKind::Bottom,
            _ => CommentKind::Scroll,
        },
        color: [
            channel(item.color.red),
            channel(item.color.green),
            channel(item.color.blue),
        ],
        is_me: item.is_self,
    }
}

/// The comments of `timeline`, in its order.
pub fn comments(timeline: &DanmakuTimeline) -> Vec<Comment> {
    timeline.items().iter().map(comment).collect()
}

/// A timeline of `comments`, as the danmaku JSON reader makes it.
pub fn timeline(comments: &[Comment]) -> crate::danmaku::Result<DanmakuTimeline> {
    DanmakuTimeline::from_json(&comments_json(comments).to_string())
}

/// The comments of `timeline` `list` lets through.
pub fn filter(list: &BlockList, timeline: &DanmakuTimeline) -> DanmakuTimeline {
    if list.is_empty() {
        return timeline.clone();
    }
    let kept: Vec<DanmakuItem> = timeline
        .items()
        .iter()
        .filter(|item| !list.blocks(&item.text))
        .cloned()
        .collect();
    DanmakuTimeline::new(kept).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_round_trip_through_a_timeline() {
        let comments = vec![
            Comment {
                time: 1.5,
                content: "第一".into(),
                kind: CommentKind::Scroll,
                color: [255, 255, 255],
                is_me: false,
            },
            Comment {
                time: 2.0,
                content: "我的".into(),
                kind: CommentKind::Bottom,
                color: [0, 128, 255],
                is_me: true,
            },
        ];
        let timeline = timeline(&comments).unwrap();
        assert_eq!(super::comments(&timeline), comments);
        let hidden = filter(&BlockList::new(["我的"]), &timeline);
        assert_eq!(super::comments(&hidden), comments[..1].to_vec());
    }
}
