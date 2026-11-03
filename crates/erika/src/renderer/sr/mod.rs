//! Super-resolution networks shared by every GPU backend.
//!
//! [`model`] defines the backend-neutral network graph, [`catalog`] maps
//! [`LumaUpscalerMode`](crate::renderer::pipeline::LumaUpscalerMode) values to
//! bundled models, [`mpv`] imports mpv hook shaders (Anime4K, FSRCNNX),
//! [`fold`] applies exact algebraic layer fusion, and [`reference`] is the CPU
//! oracle the GPU kernels are tested against.

pub mod artcnn;
pub mod catalog;
pub mod fold;
pub mod fsr1;
pub mod hlsl;
pub mod model;
pub mod mpv;
pub mod reference;
pub mod wgsl;
