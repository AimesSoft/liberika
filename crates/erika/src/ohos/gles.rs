use glow::HasContext;

use crate::ohos::avcodec::{OhosAvCodecSurface, OhosNativeBufferImage};

const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

const VERTEX_SHADER: &str = r#"#version 300 es
uniform mat4 erika_transform;
out vec2 erika_uv;
void main() {
    vec2 position;
    if (gl_VertexID == 0) {
        position = vec2(-1.0, -1.0);
    } else if (gl_VertexID == 1) {
        position = vec2(3.0, -1.0);
    } else {
        position = vec2(-1.0, 3.0);
    }
    gl_Position = vec4(position, 0.0, 1.0);
    vec2 uv = position * 0.5 + 0.5;
    erika_uv = (erika_transform * vec4(uv, 0.0, 1.0)).xy;
}
"#;

const FRAGMENT_SHADER: &str = r#"#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision highp float;
uniform samplerExternalOES erika_source;
in vec2 erika_uv;
out vec4 erika_color;
void main() {
    erika_color = texture(erika_source, erika_uv);
}
"#;

pub struct OhosGlesInterop {
    device: wgpu::Device,
    program: glow::Program,
    vao: glow::VertexArray,
    framebuffer: glow::Framebuffer,
    external_texture: glow::Texture,
    _surface: std::sync::Arc<OhosAvCodecSurface>,
}

impl OhosGlesInterop {
    pub fn new(device: &wgpu::Device) -> Result<Self, String> {
        let hal_device = unsafe { device.as_hal::<wgpu::hal::gles::Api>() }
            .ok_or_else(|| "stage=ohos_gles_init reason=not_gles".to_string())?;
        let gl = hal_device.context().lock();
        let vertex = compile_shader(&gl, glow::VERTEX_SHADER, VERTEX_SHADER)?;
        let fragment = match compile_shader(&gl, glow::FRAGMENT_SHADER, FRAGMENT_SHADER) {
            Ok(fragment) => fragment,
            Err(error) => {
                unsafe { gl.delete_shader(vertex) };
                return Err(error);
            }
        };
        let program = unsafe { gl.create_program() }
            .map_err(|error| format!("stage=create_program reason={error}"))?;
        unsafe {
            gl.attach_shader(program, vertex);
            gl.attach_shader(program, fragment);
            gl.link_program(program);
            gl.delete_shader(vertex);
            gl.delete_shader(fragment);
        }
        if !unsafe { gl.get_program_link_status(program) } {
            let reason = unsafe { gl.get_program_info_log(program) };
            unsafe { gl.delete_program(program) };
            return Err(format!("stage=link_external_oes_program reason={reason}"));
        }
        let vao = unsafe { gl.create_vertex_array() }
            .map_err(|error| format!("stage=create_vao reason={error}"))?;
        let framebuffer = unsafe { gl.create_framebuffer() }
            .map_err(|error| format!("stage=create_framebuffer reason={error}"))?;
        let external_texture = create_external_texture(&gl)?;
        let surface = OhosAvCodecSurface::new_external_texture(
            external_texture.0.get(),
            GL_TEXTURE_EXTERNAL_OES,
        )?;
        let error = unsafe { gl.get_error() };
        if error != glow::NO_ERROR {
            return Err(format!(
                "stage=create_external_native_image reason=gl_error code=0x{error:x}"
            ));
        }
        drop(gl);
        drop(hal_device);
        Ok(Self {
            device: device.clone(),
            program,
            vao,
            framebuffer,
            external_texture,
            _surface: surface,
        })
    }

    pub(crate) fn avcodec_surface(&self) -> std::sync::Arc<OhosAvCodecSurface> {
        std::sync::Arc::clone(&self._surface)
    }

    pub fn convert(
        &self,
        queue: &wgpu::Queue,
        image: &OhosNativeBufferImage,
        width: u32,
        height: u32,
    ) -> Result<Option<wgpu::Texture>, String> {
        if width == 0 || height == 0 {
            return Err("stage=ohos_gles_convert reason=zero_dimensions".to_string());
        }
        let output = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("erika-ohos-native-image-rgba8"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        establish_output_state(&self.device, queue, &output);
        let output_name = {
            let hal_texture = unsafe { output.as_hal::<wgpu::hal::gles::Api>() }
                .ok_or_else(|| "stage=access_output_texture reason=not_gles".to_string())?;
            match hal_texture.inner {
                wgpu::hal::gles::TextureInner::Texture { raw, .. } => raw,
                _ => {
                    return Err("stage=access_output_texture reason=not_gl_texture".to_string());
                }
            }
        };
        if !self.convert_external_texture(image, output_name, width, height)? {
            return Ok(None);
        }
        Ok(Some(output))
    }

    pub fn drain_discarded_frames(&self) -> Result<usize, String> {
        let hal_device = unsafe { self.device.as_hal::<wgpu::hal::gles::Api>() }
            .ok_or_else(|| "stage=ohos_gles_drain reason=not_gles".to_string())?;
        let _gl = hal_device.context().lock();
        self._surface.drain_discarded_external_frames()
    }

    fn convert_external_texture(
        &self,
        image: &OhosNativeBufferImage,
        output: glow::Texture,
        width: u32,
        height: u32,
    ) -> Result<bool, String> {
        let hal_device = unsafe { self.device.as_hal::<wgpu::hal::gles::Api>() }
            .ok_or_else(|| "stage=ohos_gles_convert reason=not_gles".to_string())?;
        let gl = hal_device.context().lock();
        let Some(transform) = image.update_external_texture()? else {
            return Ok(false);
        };
        unsafe {
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(self.external_texture));
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(output),
                0,
            );
        }
        let status = unsafe { gl.check_framebuffer_status(glow::FRAMEBUFFER) };
        if status != glow::FRAMEBUFFER_COMPLETE {
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, None);
            }
            return Err(format!(
                "stage=ohos_native_image_framebuffer reason=incomplete status=0x{status:x}"
            ));
        }
        unsafe {
            gl.viewport(0, 0, width as i32, height as i32);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::SCISSOR_TEST);
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            if let Some(location) = gl.get_uniform_location(self.program, "erika_source") {
                gl.uniform_1_i32(Some(&location), 0);
            }
            if let Some(location) = gl.get_uniform_location(self.program, "erika_transform") {
                gl.uniform_matrix_4_f32_slice(Some(&location), false, &transform);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            // Keep the decoder's current NativeImage buffer latched until the
            // conversion draw completes. This is still zero-copy for the source;
            // only the GPU converts YUV into liberika's compositing texture.
            gl.finish();
            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, None);
        }
        let error = unsafe { gl.get_error() };
        if error != glow::NO_ERROR {
            return Err(format!(
                "stage=ohos_native_image_draw reason=gl_error code=0x{error:x}"
            ));
        }
        Ok(true)
    }
}

impl Drop for OhosGlesInterop {
    fn drop(&mut self) {
        let Some(hal_device) = (unsafe { self.device.as_hal::<wgpu::hal::gles::Api>() }) else {
            return;
        };
        let gl = hal_device.context().lock();
        unsafe {
            gl.delete_texture(self.external_texture);
            gl.delete_framebuffer(self.framebuffer);
            gl.delete_vertex_array(self.vao);
            gl.delete_program(self.program);
        }
    }
}

fn establish_output_state(device: &wgpu::Device, queue: &wgpu::Queue, output: &wgpu::Texture) {
    let view = output.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("erika-ohos-native-image-state"),
    });
    {
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("erika-ohos-native-image-state-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    queue.submit(Some(encoder.finish()));
}

fn create_external_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    let texture = unsafe { gl.create_texture() }
        .map_err(|error| format!("stage=create_external_texture reason={error}"))?;
    unsafe {
        gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(texture));
        gl.tex_parameter_i32(
            GL_TEXTURE_EXTERNAL_OES,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            GL_TEXTURE_EXTERNAL_OES,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            GL_TEXTURE_EXTERNAL_OES,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            GL_TEXTURE_EXTERNAL_OES,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, None);
    }
    Ok(texture)
}

fn compile_shader(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::Shader, String> {
    let shader = unsafe { gl.create_shader(kind) }
        .map_err(|error| format!("stage=create_shader reason={error}"))?;
    unsafe {
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
    }
    if unsafe { gl.get_shader_compile_status(shader) } {
        Ok(shader)
    } else {
        let reason = unsafe { gl.get_shader_info_log(shader) };
        unsafe { gl.delete_shader(shader) };
        Err(format!("stage=compile_external_oes_shader reason={reason}"))
    }
}
