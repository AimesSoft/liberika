//! Metal RGB post-processing pass driven by a validated [`PostProcessShader`].
//!
//! When active, the video pass renders into an offscreen intermediate texture
//! sized to the presented video rectangle, and this pass applies the user
//! shader while presenting into the final drawable before subtitle and danmaku
//! overlays.

use std::ffi::c_void;
use std::mem;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLDevice, MTLLibrary, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResource, MTLSamplerAddressMode,
    MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerState, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};

use crate::core::{PlayerError, Result};
use crate::renderer::post_process::{
    PostProcessRuntimeInfo, PostProcessShader, PostProcessStatus, PostProcessUniforms,
};

struct CompiledPipeline {
    pixel_format: MTLPixelFormat,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

pub struct MetalPostProcess {
    shader: Option<Arc<PostProcessShader>>,
    compiled: Option<CompiledPipeline>,
    sampler: Option<Retained<ProtocolObject<dyn MTLSamplerState>>>,
    source_texture: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    status: PostProcessStatus,
    reason: Option<String>,
    last_gpu_duration: Duration,
}

impl Default for MetalPostProcess {
    fn default() -> Self {
        Self {
            shader: None,
            compiled: None,
            sampler: None,
            source_texture: None,
            status: PostProcessStatus::Off,
            reason: None,
            last_gpu_duration: Duration::ZERO,
        }
    }
}

impl MetalPostProcess {
    pub fn is_enabled(&self) -> bool {
        self.shader.is_some()
    }

    pub fn set_shader(&mut self, shader: Option<Arc<PostProcessShader>>) {
        let same = match (&self.shader, &shader) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.shader = shader;
        self.compiled = None;
        self.reason = None;
        self.last_gpu_duration = Duration::ZERO;
        if self.shader.is_none() {
            self.source_texture = None;
            self.status = PostProcessStatus::Off;
        } else {
            self.status = PostProcessStatus::Building;
        }
    }

    pub fn status_info(&self) -> PostProcessRuntimeInfo {
        PostProcessRuntimeInfo {
            status: self.status,
            name: self.shader.as_ref().map(|shader| shader.name.clone()),
            reason: self.reason.clone(),
            last_gpu_duration: self.last_gpu_duration,
        }
    }

    pub fn allocated_bytes(&self) -> u64 {
        self.source_texture
            .as_ref()
            .map_or(0, |tex| tex.allocatedSize() as u64)
    }

    /// Ensures the offscreen video source texture and render pipeline are ready
    /// for `(width, height, pixel_format)`. Returns `Ok(None)` when inactive or
    /// compilation failed (recording the reason so callers fall back to direct
    /// presentation).
    pub fn prepare(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        width: usize,
        height: usize,
        pixel_format: MTLPixelFormat,
    ) -> Option<(
        Retained<ProtocolObject<dyn MTLTexture>>,
        Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
        Retained<ProtocolObject<dyn MTLSamplerState>>,
    )> {
        let shader = self.shader.clone()?;
        if self
            .compiled
            .as_ref()
            .is_none_or(|compiled| compiled.pixel_format != pixel_format)
        {
            match compile_pipeline(device, &shader, pixel_format) {
                Ok(pipeline) => {
                    self.compiled = Some(CompiledPipeline {
                        pixel_format,
                        pipeline,
                    });
                    self.reason = None;
                }
                Err(error) => {
                    self.compiled = None;
                    self.status = PostProcessStatus::Inactive;
                    self.reason = Some(error.to_string());
                    return None;
                }
            }
        }
        let sampler = match self.ensure_sampler(device) {
            Ok(sampler) => sampler,
            Err(error) => {
                self.status = PostProcessStatus::Inactive;
                self.reason = Some(error.to_string());
                return None;
            }
        };
        let texture = match self.ensure_source_texture(device, width, height, pixel_format) {
            Ok(texture) => texture,
            Err(error) => {
                self.status = PostProcessStatus::Inactive;
                self.reason = Some(error.to_string());
                return None;
            }
        };
        let pipeline = self
            .compiled
            .as_ref()
            .expect("compiled pipeline exists")
            .pipeline
            .clone();
        self.status = PostProcessStatus::Active;
        self.reason = None;
        Some((texture, pipeline, sampler))
    }

    /// Encodes the post-processing draw call onto an active render encoder.
    pub fn encode(
        encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
        source: &ProtocolObject<dyn MTLTexture>,
        sampler: &ProtocolObject<dyn MTLSamplerState>,
        uniforms: &PostProcessUniforms,
    ) {
        unsafe {
            encoder.setRenderPipelineState(pipeline);
            encoder.setFragmentTexture_atIndex(Some(source), 0);
            encoder.setFragmentSamplerState_atIndex(Some(sampler), 0);
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(
                    (uniforms as *const PostProcessUniforms)
                        .cast::<c_void>()
                        .cast_mut(),
                )
                .expect("uniform pointer is non-null"),
                mem::size_of::<PostProcessUniforms>(),
                0,
            );
            encoder.setFragmentBytes_length_atIndex(
                NonNull::new(
                    (uniforms as *const PostProcessUniforms)
                        .cast::<c_void>()
                        .cast_mut(),
                )
                .expect("uniform pointer is non-null"),
                mem::size_of::<PostProcessUniforms>(),
                0,
            );
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
        }
    }

    fn ensure_sampler(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
    ) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>> {
        if let Some(sampler) = self.sampler.as_ref() {
            return Ok(sampler.clone());
        }
        let descriptor = MTLSamplerDescriptor::new();
        descriptor.setMinFilter(MTLSamplerMinMagFilter::Linear);
        descriptor.setMagFilter(MTLSamplerMinMagFilter::Linear);
        descriptor.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
        descriptor.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
        let sampler = device
            .newSamplerStateWithDescriptor(&descriptor)
            .ok_or_else(|| {
                PlayerError::Renderer("Metal post-process sampler creation returned nil".into())
            })?;
        self.sampler = Some(sampler.clone());
        Ok(sampler)
    }

    fn ensure_source_texture(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        width: usize,
        height: usize,
        pixel_format: MTLPixelFormat,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        let width = width.max(1);
        let height = height.max(1);
        if let Some(existing) = self.source_texture.as_ref()
            && existing.width() == width
            && existing.height() == height
            && existing.pixelFormat() == pixel_format
        {
            return Ok(existing.clone());
        }
        let descriptor = unsafe {
            let desc =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    pixel_format,
                    width,
                    height,
                    false,
                );
            desc.setStorageMode(MTLStorageMode::Private);
            desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            desc
        };
        let texture = device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| {
                PlayerError::Renderer(format!(
                    "Metal post-process texture alloc failed for {width}x{height} {pixel_format:?}"
                ))
            })?;
        self.source_texture = Some(texture.clone());
        Ok(texture)
    }
}

fn compile_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    shader: &PostProcessShader,
    pixel_format: MTLPixelFormat,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
    let (source, frag_entry) = shader.to_msl()?;
    let library = device
        .newLibraryWithSource_options_error(&NSString::from_str(&source), None)
        .map_err(|error| {
            PlayerError::Renderer(format!(
                "Metal post-process shader compile failed for {:?}: {}",
                shader.name,
                error.localizedDescription()
            ))
        })?;
    let vertex = library
        .newFunctionWithName(&NSString::from_str("erika_post_vs"))
        .ok_or_else(|| {
            PlayerError::Renderer("Metal post-process library missing erika_post_vs".to_string())
        })?;
    let fragment = library
        .newFunctionWithName(&NSString::from_str(&frag_entry))
        .ok_or_else(|| {
            PlayerError::Renderer(format!(
                "Metal post-process library missing fragment entry {frag_entry:?}"
            ))
        })?;
    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setLabel(Some(&NSString::from_str("Erika Post-Process Pipeline")));
    descriptor.setVertexFunction(Some(&*vertex));
    descriptor.setFragmentFunction(Some(&*fragment));
    let attachments = descriptor.colorAttachments();
    let attachment = unsafe { attachments.objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(pixel_format);
    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|error| {
            PlayerError::Renderer(format!(
                "Metal post-process pipeline create failed for {:?}: {}",
                shader.name,
                error.localizedDescription()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_metal::{
        MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
        MTLCreateSystemDefaultDevice, MTLLoadAction, MTLOrigin, MTLRegion, MTLRenderPassDescriptor,
        MTLSize, MTLStoreAction,
    };

    const CRT_STANDARD: &str = r#"//!DESC CRT Standard
//!HOOK MAIN
//!BIND HOOKED

const float PI = 3.141592653589793;

vec4 hook() {
    vec2 uv = HOOKED_pos;
    vec2 px = HOOKED_pt;

    vec2 centered = uv * 2.0 - 1.0;
    float r2 = dot(centered, centered);
    float curvature = 0.12;
    vec2 distorted = centered * (1.0 + curvature * r2);
    vec2 duv = distorted * 0.5 + 0.5;

    float inside = step(0.0, duv.x) * step(duv.x, 1.0) *
        step(0.0, duv.y) * step(duv.y, 1.0);
    vec4 color = HOOKED_tex(clamp(duv, 0.0, 1.0));
    color.rgb *= inside;

    float y = duv.y / px.y;
    float line = mod(floor(y), 2.0);
    float scan = mix(1.0, 0.85, line);
    color.rgb *= scan;

    float x = duv.x / px.x;
    float tri = x * (2.0 * PI / 3.0);
    vec3 mask = vec3(
        sin(tri) * 0.5 + 0.5,
        sin(tri + 2.094395102393195) * 0.5 + 0.5,
        sin(tri + 4.1887902047863905) * 0.5 + 0.5
    );
    const float maskStrength = 0.35;
    color.rgb *= mix(vec3(1.0 - maskStrength), vec3(1.0), mask);

    float vignette = 1.0 - smoothstep(0.5, 1.1, length(distorted));
    color.rgb *= mix(1.0, vignette, 0.45);

    color.rgb *= 1.08;
    return vec4(clamp(color.rgb, 0.0, 1.0), color.a);
}
"#;

    #[test]
    fn metal_executes_crt_post_process_shader() {
        let Some(device) = MTLCreateSystemDefaultDevice() else {
            return;
        };
        let queue = device.newCommandQueue().expect("command queue");
        let shader = Arc::new(PostProcessShader::parse_str("crt_standard", CRT_STANDARD).unwrap());
        let mut post = MetalPostProcess::default();
        post.set_shader(Some(shader));

        let width = 64usize;
        let height = 64usize;
        let (_, pipeline, sampler) = post
            .prepare(&device, width, height, MTLPixelFormat::BGRA8Unorm)
            .unwrap_or_else(|| panic!("metal post-process failed: {:?}", post.status_info()));
        assert_eq!(post.status_info().status, PostProcessStatus::Active);

        let src_desc = unsafe {
            let desc =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    width,
                    height,
                    false,
                );
            desc.setStorageMode(MTLStorageMode::Shared);
            desc.setUsage(MTLTextureUsage::ShaderRead);
            desc
        };
        let src = device.newTextureWithDescriptor(&src_desc).expect("src");
        let white = vec![200u8; width * height * 4];
        unsafe {
            src.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize {
                        width,
                        height,
                        depth: 1,
                    },
                },
                0,
                NonNull::new(white.as_ptr().cast::<c_void>().cast_mut()).unwrap(),
                width * 4,
            );
        }

        let dst_desc = unsafe {
            let desc =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    width,
                    height,
                    false,
                );
            desc.setStorageMode(MTLStorageMode::Shared);
            desc.setUsage(MTLTextureUsage::RenderTarget);
            desc
        };
        let dst = device.newTextureWithDescriptor(&dst_desc).expect("dst");

        let uniforms = PostProcessUniforms::new(
            (width as u32, height as u32),
            [0.0, 0.0, width as f32, height as f32],
            (width as u32, height as u32),
            0.0,
            0,
        );

        let cmd = queue.commandBuffer().expect("cmd");
        let pass = MTLRenderPassDescriptor::new();
        let att = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        att.setTexture(Some(&*dst));
        att.setLoadAction(MTLLoadAction::Clear);
        att.setStoreAction(MTLStoreAction::Store);
        att.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        });
        let encoder = cmd.renderCommandEncoderWithDescriptor(&pass).expect("enc");
        MetalPostProcess::encode(&encoder, &pipeline, &src, &sampler, &uniforms);
        encoder.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();

        let mut out = vec![0u8; width * height * 4];
        unsafe {
            dst.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(out.as_mut_ptr().cast::<c_void>()).unwrap(),
                width * 4,
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize {
                        width,
                        height,
                        depth: 1,
                    },
                },
                0,
            );
        }
        // Center pixel should be bright; extreme corner is warped outside [0, 1] -> black.
        let center_offset = ((height / 2) * width + (width / 2)) * 4;
        let center_luma = u32::from(out[center_offset])
            + u32::from(out[center_offset + 1])
            + u32::from(out[center_offset + 2]);
        let corner_luma = u32::from(out[0]) + u32::from(out[1]) + u32::from(out[2]);
        assert!(center_luma > 250, "center too dark: {center_luma}");
        assert_eq!(corner_luma, 0, "CRT curvature corner should clip to black");
    }
}
