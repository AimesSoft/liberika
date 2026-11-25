#[cfg(all(feature = "wgpu", target_os = "android"))]
pub(crate) mod android_vulkan;
#[doc(hidden)]
pub mod artcnn;
#[cfg(target_os = "windows")]
pub mod d3d11;
#[cfg(target_os = "windows")]
mod d3d11_d3d12_linalg;
#[cfg(target_os = "windows")]
mod d3d11_sr;
#[cfg(target_os = "windows")]
mod d3d11_sr_select;
#[cfg(target_os = "windows")]
mod d3d11_vk_coopmat;
#[cfg(target_os = "windows")]
mod d3d11_vsr;
#[cfg(all(
    not(target_arch = "wasm32"),
    any(target_vendor = "apple", target_os = "windows", feature = "wgpu", test)
))]
pub(crate) mod danmaku_batch;
#[cfg(all(
    test,
    feature = "wgpu",
    any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        all(target_os = "linux", not(target_env = "ohos"))
    )
))]
pub(crate) mod external_texture_test;
#[cfg(not(target_arch = "wasm32"))]
mod frame;
/// CPU-side perceptual gamut LUT generation (libplacebo-compatible).
#[cfg(not(target_arch = "wasm32"))]
pub mod gamut;
#[cfg(all(feature = "wgpu", target_os = "linux", not(target_env = "ohos")))]
pub(crate) mod linux_vulkan;
#[cfg(not(target_arch = "wasm32"))]
pub mod metal;
#[cfg(all(feature = "wgpu", target_env = "ohos"))]
pub(crate) mod ohos_vulkan;
#[cfg(not(target_arch = "wasm32"))]
pub mod output;
#[cfg(not(target_arch = "wasm32"))]
pub mod pipeline;
#[cfg(target_arch = "wasm32")]
pub mod pipeline {
    pub use super::upscaler_mode::{LumaUpscalerMode, UpscalerFamily};
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod presentation;
#[doc(hidden)]
pub mod sr;
pub mod upscaler_mode;
#[cfg(all(feature = "wgpu", target_os = "linux", not(target_env = "ohos")))]
pub(crate) mod vulkan_dmabuf_export;
#[cfg(all(feature = "wgpu", target_os = "linux", not(target_env = "ohos")))]
pub(crate) mod vulkan_export;
#[cfg(feature = "wgpu")]
pub mod wgpu;
#[cfg(feature = "wgpu")]
#[doc(hidden)]
pub mod wgpu_artcnn;
#[cfg(feature = "wgpu")]
pub mod wgpu_sr;

#[cfg(not(target_arch = "wasm32"))]
pub use frame::{VideoFrameDescriptor, VideoFramePayload};
