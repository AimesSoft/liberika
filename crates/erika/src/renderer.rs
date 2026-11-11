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
#[cfg(any(target_vendor = "apple", target_os = "windows", feature = "wgpu", test))]
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
mod frame;
/// CPU-side perceptual gamut LUT generation (libplacebo-compatible).
pub mod gamut;
#[cfg(all(feature = "wgpu", target_os = "linux", not(target_env = "ohos")))]
pub(crate) mod linux_vulkan;
pub mod metal;
#[cfg(all(feature = "wgpu", target_env = "ohos"))]
pub(crate) mod ohos_vulkan;
pub mod output;
pub mod pipeline;
pub(crate) mod presentation;
#[doc(hidden)]
pub mod sr;
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

pub use frame::{VideoFrameDescriptor, VideoFramePayload};
