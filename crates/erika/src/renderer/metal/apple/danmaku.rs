#[cfg(test)]
mod gpu_tests {
    use super::super::*;
    use crate::renderer::danmaku_batch::test_support;

    // Exercise the same shader, buffer packing, atlas upload and blend state as
    // the live player, then read GPU pixels (not a CPU copy of the shader).
    fn render(
        renderer: &mut MetalRendererImpl,
        plan: &DanmakuRenderPlan,
        background: [f32; 4],
    ) -> Vec<[f32; 4]> {
        let (w, h) = (32, 24);
        let format = renderer.drawable_pixel_format;
        unsafe {
            let descriptor =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    metal_pixel_format(format),
                    w,
                    h,
                    false,
                );
            descriptor.setStorageMode(MTLStorageMode::Private);
            descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            let texture = renderer
                .device
                .newTextureWithDescriptor(&descriptor)
                .unwrap();
            let command = renderer.queue.commandBuffer().unwrap();
            let pass = MTLRenderPassDescriptor::new();
            let attachment = pass.colorAttachments().objectAtIndexedSubscript(0);
            attachment.setTexture(Some(&texture));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: background[0] as f64,
                green: background[1] as f64,
                blue: background[2] as f64,
                alpha: background[3] as f64,
            });
            let encoder = command.renderCommandEncoderWithDescriptor(&pass).unwrap();
            renderer
                .draw_danmaku_plan(
                    &encoder,
                    &command,
                    plan,
                    VideoPresentationLayout::aspect_fit(w as u32, h as u32, w as u32, h as u32),
                    TargetColorState::sdr(ColorPrimaries::Bt709),
                )
                .unwrap();
            encoder.endEncoding();
            let bpp = if format == MetalDrawablePixelFormat::Bgra8Unorm {
                4
            } else {
                8
            };
            let bytes = w * h * bpp;
            let buffer = renderer
                .device
                .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)
                .unwrap();
            let blit = command.blitCommandEncoder().unwrap();
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                &texture, 0, 0, MTLOrigin { x: 0, y: 0, z: 0 }, MTLSize { width: w, height: h, depth: 1 },
                &buffer, 0, w * bpp, bytes);
            blit.endEncoding();
            command.commit();
            command.waitUntilCompleted();
            assert_eq!(command.status(), MTLCommandBufferStatus::Completed);
            let raw = std::slice::from_raw_parts(buffer.contents().as_ptr().cast::<u8>(), bytes);
            raw.chunks_exact(bpp)
                .map(|p| {
                    if bpp == 4 {
                        [
                            p[2] as f32 / 255.0,
                            p[1] as f32 / 255.0,
                            p[0] as f32 / 255.0,
                            p[3] as f32 / 255.0,
                        ]
                    } else {
                        std::array::from_fn(|i| {
                            half_to_f32(u16::from_le_bytes([p[2 * i], p[2 * i + 1]]))
                        })
                    }
                })
                .collect()
        }
    }

    fn renderer(edr: bool) -> MetalRendererImpl {
        let mut r = MetalRendererImpl::new(MetalRendererConfig::default())
            .expect("Metal device required for pixel regression");
        // This test renders offscreen, so configure the target explicitly.
        if edr {
            r.output_mode = MetalOutputMode::apple_edr(4.0);
            r.drawable_pixel_format = MetalDrawablePixelFormat::Rgba16Float;
        }
        r
    }

    #[test]
    fn danmaku_gpu_group_opacity() {
        for edr in [false, true] {
            let mut r = renderer(edr);
            test_support::check_opacity(|p, bg| render(&mut r, p, bg));
            test_support::check_boundaries(|p, bg| render(&mut r, p, bg));
            test_support::check_overlap(|p, bg| render(&mut r, p, bg));
            test_support::check_target_alpha(|p, bg| render(&mut r, p, bg));
        }
    }
}
