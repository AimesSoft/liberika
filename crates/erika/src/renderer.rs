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
    pub use super::upscaler_mode::{
        LumaUpscalerMode, UpscalerCapabilities, UpscalerDescriptor, UpscalerFamily,
    };
}
#[cfg(not(target_arch = "wasm32"))]
pub mod post_process;
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
mod wgpu_post_process;
#[cfg(feature = "wgpu")]
pub mod wgpu_sr;

#[cfg(not(target_arch = "wasm32"))]
pub use frame::{VideoFrameDescriptor, VideoFramePayload};

/// Validates a runtime-supplied super-resolution network (`.esr` or supported
/// mpv CNN shader such as FSRCNNX / Anime4K CNN) without installing it on a
/// renderer.
pub fn validate_custom_upscaler_model(name: &str, bytes: &[u8]) -> crate::core::Result<()> {
    let _ = sr::catalog::load_custom(name, bytes)?;
    Ok(())
}

/// Validates a runtime-supplied RGB post-processing shader (mpv `//!HOOK MAIN`
/// GLSL or WGSL) without installing it on a renderer.
#[cfg(not(target_arch = "wasm32"))]
pub fn validate_post_process_shader(name: &str, bytes: &[u8]) -> crate::core::Result<()> {
    let _ = post_process::PostProcessShader::parse(name, bytes)?;
    Ok(())
}

/// Probes upscaler and post-processing capabilities for the platform's default
/// GPU device when no renderer session is active yet.
pub fn probe_default_upscalers() -> pipeline::UpscalerCapabilities {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    {
        return metal::probe_default_upscalers();
    }
    #[cfg(target_os = "windows")]
    {
        return d3d11::probe_default_upscalers();
    }
    #[cfg(all(
        feature = "wgpu",
        not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "windows"
        ))
    ))]
    {
        return wgpu::probe_default_upscalers();
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "windows",
        feature = "wgpu"
    )))]
    {
        pipeline::UpscalerCapabilities::default()
    }
}
