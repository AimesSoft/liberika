//! SDR readback of the retained video frame, independent of the live surface.
use super::*;
use crate::core::RendererFrameCapture;
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_USAGE_STAGING,
};

impl D3d11Renderer {
    pub(super) fn capture_rgba(
        &mut self,
        context: RenderFrameContext<'_>,
        width: u32,
        height: u32,
    ) -> Result<Option<RendererFrameCapture>> {
        let Some(video) = self.current_video.as_ref() else {
            return Ok(None);
        };
        if width == 0 || height == 0 {
            return Err(PlayerError::Renderer(
                "capture size must be non-zero".into(),
            ));
        }
        let row_bytes = (width as usize)
            .checked_mul(4)
            .ok_or_else(|| PlayerError::Renderer("d3d11: capture row size overflow".into()))?;
        let byte_len = row_bytes
            .checked_mul(height as usize)
            .ok_or_else(|| PlayerError::Renderer("d3d11: capture size overflow".into()))?;
        let target_rect = aspect_fit_rect(
            self.video_alpha_mode.logical_width(video.width),
            video.height,
            width,
            height,
        );
        // Live HDR uniforms produce linear light for the HDR composition pass.
        // Build an SDR pipeline from the original metadata instead of changing
        // those uniforms or the attached swapchain's color space.
        let format = if video.constants.is_p010 != 0 {
            D3d11VideoTextureFormat::P010
        } else {
            D3d11VideoTextureFormat::Nv12
        };
        let mut constants = constants_for_frame(
            video.source_color,
            format,
            D3d11OutputMode::Sdr.target_color_for_source(video.source_color),
        )
        .packed_alpha_right(self.video_alpha_mode.has_alpha());
        let gamut_lut = self.gamut_lut_view(&constants)?;
        if gamut_lut.is_none() {
            constants.gamut_lut_enabled = 0;
        }
        let overlay_target = D3d11OutputMode::Sdr.target_color();
        let overlays = self.prepare_overlay_draws_for_target(context.overlay, overlay_target)?;
        let danmaku = context.danmaku.filter(|plan| {
            plan.generation == context.generation
                && plan.viewport.width == width
                && plan.viewport.height == height
        });
        let danmaku = self.prepare_danmaku_draws_for_target(danmaku, overlay_target)?;
        let state = self.state.as_ref().expect("current video has a device");
        let (texture, target, staging) = capture_targets(&state.device, width, height)?;
        let fence = create_d3d11_event_query(&state.device)?;
        // Allocate before submitting work, and before mapping the staging
        // texture, so neither allocation failure nor panic leaves it mapped.
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(byte_len).map_err(|error| {
            PlayerError::Renderer(format!("d3d11: capture allocation: {error}"))
        })?;
        rgba.resize(byte_len, 0);
        unsafe {
            state.context.ClearRenderTargetView(
                &target,
                &[
                    0.0,
                    0.0,
                    0.0,
                    if self.video_alpha_mode.has_alpha() {
                        0.0
                    } else {
                        1.0
                    },
                ],
            );
        }
        let draw_result = (|| {
            state.draw_video(
                self.current_video
                    .as_ref()
                    .expect("video retained during capture"),
                constants,
                None,
                gamut_lut.as_ref(),
                &target,
                target_rect,
            )?;
            state.draw_overlays(&overlays, &target, target_rect)?;
            state.draw_danmaku(
                &danmaku,
                &target,
                D3d11DrawRect {
                    x: 0.0,
                    y: 0.0,
                    width: width as f32,
                    height: height as f32,
                },
            )
        })();
        unsafe {
            state.context.OMSetRenderTargets(None, None);
            if draw_result.is_ok() {
                state.context.CopyResource(&staging, &texture);
            }
            state.context.End(&fence);
            state.context.Flush();
        }
        // Even a timed-out readback must retain the decoder frame until its
        // last GPU reader finishes. This fence also covers earlier live draws.
        self.current_video_fence = Some(fence);
        draw_result?;
        read_capture(state, &staging, row_bytes, &mut rgba, self.handoff_timeout)?;
        if self.video_alpha_mode.has_alpha() {
            // Composition shaders use premultiplied RGB. CPU captures (and
            // PNG encoders) consume straight RGBA after all overlays blend.
            for pixel in rgba.chunks_exact_mut(4) {
                let alpha = u32::from(pixel[3]);
                if alpha != 0 && alpha != 255 {
                    for channel in &mut pixel[..3] {
                        *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                    }
                }
            }
        }
        Ok(Some(RendererFrameCapture {
            width,
            height,
            rgba,
        }))
    }
}

impl Drop for D3d11Renderer {
    fn drop(&mut self) {
        let (Some(state), Some(fence), Some(video)) = (
            self.state.as_ref(),
            self.current_video_fence.as_ref(),
            self.current_video.as_mut(),
        ) else {
            return;
        };
        if video._frame.is_none() {
            return;
        }
        // Normal teardown retires the frame first. If its bounded wait failed,
        // destruction must not return a still-sampled AVFrame to the decoder
        // pool. On a removed device no GPU read can continue. Otherwise retain
        // this one frame on the exceptional failure path instead of blocking
        // the owner indefinitely or allowing decoder reuse while it is read.
        if wait_for_fence(&state.context, fence, self.handoff_timeout).is_err()
            && unsafe { state.device.GetDeviceRemovedReason() }.is_ok()
            && let Some(frame) = video._frame.take()
        {
            mem::forget(frame);
            crate::trace::diagnostic_at(
                crate::trace::LogLevel::Warn,
                "erika d3d11: retaining an unfinished GPU frame during renderer destruction",
            );
        }
    }
}

fn capture_targets(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView, ID3D11Texture2D)> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let mut texture = None;
    let mut target = None;
    let mut staging = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| d3d_error("CreateTexture2D(capture)", error))?;
        let texture = texture
            .ok_or_else(|| PlayerError::Renderer("d3d11: capture texture was null".into()))?;
        device
            .CreateRenderTargetView(&texture, None, Some(&mut target))
            .map_err(|error| d3d_error("CreateRenderTargetView(capture)", error))?;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .map_err(|error| d3d_error("CreateTexture2D(capture staging)", error))?;
        Ok((
            texture,
            target.ok_or_else(|| PlayerError::Renderer("d3d11: capture target was null".into()))?,
            staging
                .ok_or_else(|| PlayerError::Renderer("d3d11: capture staging was null".into()))?,
        ))
    }
}

fn read_capture(
    state: &D3d11DeviceState,
    staging: &ID3D11Texture2D,
    row_bytes: usize,
    rgba: &mut [u8],
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    loop {
        let result = unsafe {
            state.context.Map(
                staging,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        };
        match result {
            Ok(()) => break,
            Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                if started.elapsed() >= timeout {
                    return Err(PlayerError::Renderer("d3d11: capture GPU readback timed out; retaining the current decoder frame".into()));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(d3d_error("Map(capture staging)", error)),
        }
    }
    if mapped.pData.is_null() || (mapped.RowPitch as usize) < row_bytes {
        unsafe { state.context.Unmap(staging, 0) };
        return Err(PlayerError::Renderer(
            "d3d11: invalid capture staging layout".into(),
        ));
    }
    for (row, output) in rgba.chunks_exact_mut(row_bytes).enumerate() {
        unsafe {
            output.copy_from_slice(std::slice::from_raw_parts(
                mapped
                    .pData
                    .cast::<u8>()
                    .add(row * mapped.RowPitch as usize),
                row_bytes,
            ));
        }
    }
    unsafe { state.context.Unmap(staging, 0) };
    Ok(())
}

#[cfg(test)]
mod tests;
