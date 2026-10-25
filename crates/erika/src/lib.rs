#[cfg(target_os = "android")]
pub mod android;
#[cfg(not(target_arch = "wasm32"))]
pub mod apple;
#[cfg(not(target_arch = "wasm32"))]
pub mod audio;
#[cfg(not(target_arch = "wasm32"))]
pub mod core;
pub mod danmaku;
pub mod plugin;
#[cfg(not(target_arch = "wasm32"))]
pub mod debug_hud;
#[cfg(not(target_arch = "wasm32"))]
pub mod diagnostics;
#[cfg(not(target_arch = "wasm32"))]
pub mod export;
#[cfg(not(target_arch = "wasm32"))]
mod export_color;
#[cfg(not(target_arch = "wasm32"))]
pub mod opening;
#[cfg(not(target_arch = "wasm32"))]
pub mod ffmpeg;
pub mod font;
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
pub mod linux;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod luma_stats;
#[cfg(target_env = "ohos")]
pub mod ohos;
#[cfg(not(target_arch = "wasm32"))]
pub mod overlay;
#[cfg(not(target_arch = "wasm32"))]
pub mod playback;
#[cfg(not(target_arch = "wasm32"))]
pub mod presenter;
#[cfg(not(target_arch = "wasm32"))]
pub mod renderer;
#[cfg(not(target_arch = "wasm32"))]
pub mod source;
#[cfg(not(target_arch = "wasm32"))]
pub mod subtitle;
#[cfg(not(target_arch = "wasm32"))]
pub mod subtitle_charset;
pub mod text;
#[cfg(target_arch = "wasm32")]
pub mod web;
#[cfg(target_os = "windows")]
pub mod windows;

mod trace;

pub(crate) const NIPAPLAY_FALLBACK_FONT: &[u8] = include_bytes!("../assets/subfont.ttf");

#[cfg(not(target_arch = "wasm32"))]
pub use core::*;
