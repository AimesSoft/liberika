//! `simdgroup_matrix` backend for the ArtCNN luma upscaler.
//!
//! The 3x3 convolutions are evaluated as per-tap matrix multiplies:
//! `out[CH x px] += W_tap[CH x CH] * in_tap[CH x px]`, mapped onto
//! `simdgroup_half8x8` fragments. Operands are shared across the simdgroup,
//! which removes the per-thread weight loads and address arithmetic that
//! bound the scalar backend.
//!
//! Feature maps live in plain device buffers (channel-major planes) with a
//! one-pixel zero border baked into the layout, so the hot loop has no
//! boundary branches and `simdgroup_load`/`simdgroup_store` can address tiles
//! directly. Bias is applied as an outer product (`bias_col x ones_row`) and
//! the residual add as a multiply by the identity, because fragments expose
//! no element-wise API besides `thread_elements()` (used for ReLU, where the
//! lane mapping does not matter).

use std::ffi::c_void;
use std::mem;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSRange, NSString};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandEncoder,
    MTLComputeCommandEncoder, MTLComputePipelineState, MTLDevice, MTLLibrary, MTLPixelFormat,
    MTLResource, MTLResourceOptions, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage,
};

use crate::core::{PlayerError, Result};
use crate::renderer::artcnn::{
    ArtCnnExecutionPolicy, ArtCnnModel, ArtCnnModelLayout, CONVOLUTION_TAPS as TAPS,
    FrameTokenCache, MIDDLE_LAYER_COUNT as MID_LAYERS,
};

/// Simdgroups per threadgroup; each handles one output row.
const SIMDS: usize = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct MatParams {
    weight_offset: u32,
    bias_offset: u32,
    ones_offset: u32,
    identity_offset: u32,
    relu: u32,
    add_residual: u32,
    width: u32,
    height: u32,
    row_stride: u32,
    plane_stride: u32,
}

/// Offsets into the repacked weights buffer, in halfs.
struct MatOffsets {
    conv0_w: u32,
    conv0_b: u32,
    mid_w: [u32; MID_LAYERS],
    mid_b: [u32; MID_LAYERS],
    conv6_w: u32,
    conv6_b: u32,
    ones: u32,
    identity: u32,
}

struct MatPool {
    width: usize,
    height: usize,
    row_stride: usize,
    plane_stride: usize,
    output_format: MTLPixelFormat,
    features: [Retained<ProtocolObject<dyn MTLBuffer>>; 3],
    output: Retained<ProtocolObject<dyn MTLTexture>>,
    frame_cache: FrameTokenCache,
}

pub(super) struct Resources {
    channels: usize,
    pxf: usize,
    offsets: MatOffsets,
    weights: Retained<ProtocolObject<dyn MTLBuffer>>,
    conv0_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    conv_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    conv6_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    zero_border_pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    pool: Option<MatPool>,
}

impl Resources {
    pub(super) fn allocated_bytes(&self) -> u64 {
        let pool_bytes = self.pool.as_ref().map_or(0, |pool| {
            pool.features
                .iter()
                .fold(pool.output.allocatedSize() as u64, |total, buffer| {
                    total.saturating_add(buffer.allocatedSize() as u64)
                })
        });
        (self.weights.allocatedSize() as u64).saturating_add(pool_bytes)
    }
}

fn half(payload: &[u8], index: usize) -> u16 {
    u16::from_le_bytes([payload[index * 2], payload[index * 2 + 1]])
}

/// Repacks the scalar blob payload (layout documented in
/// `assets/artcnn/export_artcnn.py`) into the matmul layout:
/// per-tap row-major `CH x CH` matrices, bias outer-product blocks, plus the
/// shared ones-row and identity fragments.
fn repack_weights(payload: &[u8], layout: ArtCnnModelLayout) -> (Vec<u16>, MatOffsets) {
    let channels = layout.feature_count;
    let slices = channels / 4;
    let one = 0x3C00u16; // 1.0 as f16

    // The canonical offsets use half4 units; matrix packing indexes halfs.
    let source = layout.layer_offsets;
    let src_conv0_w = source.conv0_w as usize * 4;
    let src_conv0_b = source.conv0_b as usize * 4;
    // Each (out-slice, tap, in-slice) entry is a 4x4 matrix = 16 halfs.
    let mut src_mid_w = [0usize; MID_LAYERS];
    let mut src_mid_b = [0usize; MID_LAYERS];
    for layer in 0..MID_LAYERS {
        src_mid_w[layer] = source.mid_w[layer] as usize * 4;
        src_mid_b[layer] = source.mid_b[layer] as usize * 4;
    }
    let src_conv6_w = source.conv6_w as usize * 4;
    let src_conv6_b = source.conv6_b as usize * 4;

    let mut out: Vec<u16> = Vec::new();
    let take = |len: usize, out: &mut Vec<u16>| {
        let offset = out.len() as u32;
        out.resize(out.len() + len, 0);
        offset
    };

    // conv0: [tap][ch] + bias[ch]
    let conv0_w = take(TAPS * channels, &mut out);
    for s in 0..slices {
        for tap in 0..TAPS {
            for o in 0..4 {
                out[conv0_w as usize + tap * channels + s * 4 + o] =
                    half(payload, src_conv0_w + ((s * TAPS + tap) * 4) + o);
            }
        }
    }
    let conv0_b = take(channels, &mut out);
    for ch in 0..channels {
        out[conv0_b as usize + ch] = half(payload, src_conv0_b + ch);
    }

    // mid layers: [tap][out][in] row-major + bias blocks (8x8 per rb, col 0)
    let blocks = channels / 8;
    let mut mid_w = [0u32; MID_LAYERS];
    let mut mid_b = [0u32; MID_LAYERS];
    for layer in 0..MID_LAYERS {
        mid_w[layer] = take(TAPS * channels * channels, &mut out);
        for s in 0..slices {
            for tap in 0..TAPS {
                for i in 0..slices {
                    let src = src_mid_w[layer] + (((s * TAPS) + tap) * slices + i) * 16;
                    for j in 0..4 {
                        for o in 0..4 {
                            out[mid_w[layer] as usize
                                + tap * channels * channels
                                + (s * 4 + o) * channels
                                + (i * 4 + j)] = half(payload, src + j * 4 + o);
                        }
                    }
                }
            }
        }
        mid_b[layer] = take(blocks * 64, &mut out);
        for rb in 0..blocks {
            for r in 0..8 {
                out[mid_b[layer] as usize + rb * 64 + r * 8] =
                    half(payload, src_mid_b[layer] + rb * 8 + r);
            }
        }
    }

    // conv6: [tap][o][in] + bias[4]
    let conv6_w = take(TAPS * 4 * channels, &mut out);
    for tap in 0..TAPS {
        for i in 0..slices {
            let src = src_conv6_w + ((tap * slices) + i) * 16;
            for j in 0..4 {
                for o in 0..4 {
                    out[conv6_w as usize + tap * 4 * channels + o * channels + (i * 4 + j)] =
                        half(payload, src + j * 4 + o);
                }
            }
        }
    }
    let conv6_b = take(4, &mut out);
    for o in 0..4 {
        out[conv6_b as usize + o] = half(payload, src_conv6_b + o);
    }

    // ones-row fragment (row 0 = 1) and identity fragment
    let ones = take(64, &mut out);
    for c in 0..8 {
        out[ones as usize + c] = one;
    }
    let identity = take(64, &mut out);
    for d in 0..8 {
        out[identity as usize + d * 8 + d] = one;
    }

    (
        out,
        MatOffsets {
            conv0_w,
            conv0_b,
            mid_w,
            mid_b,
            conv6_w,
            conv6_b,
            ones,
            identity,
        },
    )
}

pub(super) fn build_resources(
    device: &ProtocolObject<dyn MTLDevice>,
    model: ArtCnnModel<'_>,
) -> Result<Resources> {
    let mode = model.mode;
    let channels = model.layout.feature_count;
    let (packed, offsets) = repack_weights(model.payload, model.layout);
    let weights = unsafe {
        device.newBufferWithBytes_length_options(
            NonNull::new(packed.as_ptr().cast::<c_void>().cast_mut())
                .expect("packed weights pointer is non-null"),
            packed.len() * 2,
            MTLResourceOptions::StorageModeShared,
        )
    }
    .ok_or_else(|| PlayerError::Renderer("newBufferWithBytes returned nil".to_string()))?;

    let pxf = ArtCnnExecutionPolicy::for_mode(mode).matrix_pixel_fragments;
    let source = MATMUL_SHADER_TEMPLATE
        .replace("{CH}", &channels.to_string())
        .replace("{PXF}", &pxf.to_string())
        .replace("{SIMDS}", &SIMDS.to_string());
    let library = device
        .newLibraryWithSource_options_error(&NSString::from_str(&source), None)
        .map_err(|error| {
            PlayerError::Renderer(format!("Metal matmul upscaler compile failed: {error}"))
        })?;
    let pipeline = |name: &str| -> Result<Retained<ProtocolObject<dyn MTLComputePipelineState>>> {
        let function = library
            .newFunctionWithName(&NSString::from_str(name))
            .ok_or_else(|| PlayerError::Renderer(format!("Metal shader missing {name}")))?;
        device
            .newComputePipelineStateWithFunction_error(&function)
            .map_err(|error| {
                PlayerError::Renderer(format!("compute pipeline {name} failed: {error}"))
            })
    };

    Ok(Resources {
        channels,
        pxf,
        offsets,
        weights,
        conv0_pipeline: pipeline("artcnn_mm_conv0")?,
        conv_pipeline: pipeline("artcnn_mm_conv")?,
        conv6_pipeline: pipeline("artcnn_mm_conv6")?,
        zero_border_pipeline: pipeline("artcnn_mm_zero_border")?,
        pool: None,
    })
}

fn ensure_pool(
    resources: &mut Resources,
    device: &ProtocolObject<dyn MTLDevice>,
    width: usize,
    height: usize,
    output_format: MTLPixelFormat,
) -> Result<bool> {
    if let Some(pool) = resources.pool.as_ref() {
        if pool.width == width && pool.height == height && pool.output_format == output_format {
            return Ok(false);
        }
    }

    let strip = resources.pxf * 8;
    let row_stride = width.div_ceil(strip) * strip + 2;
    // Layout: per 8-channel block, each padded y-row stores 8 channel rows of
    // row_stride pixels. Fragment rows are then contiguous in x, so
    // simdgroup_load/store need no transpose. plane_stride is per block.
    let plane_stride = row_stride * (height + 2) * 8;
    let buffer_len = (resources.channels / 8) * plane_stride * 2;
    let feature_buffer = || -> Result<Retained<ProtocolObject<dyn MTLBuffer>>> {
        device
            .newBufferWithLength_options(buffer_len, MTLResourceOptions::StorageModePrivate)
            .ok_or_else(|| PlayerError::Renderer("feature buffer alloc failed".to_string()))
    };

    let output_descriptor = unsafe {
        let descriptor =
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                output_format,
                width * 2,
                height * 2,
                false,
            );
        descriptor.setStorageMode(MTLStorageMode::Private);
        descriptor.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
        descriptor
    };
    let output = device
        .newTextureWithDescriptor(&output_descriptor)
        .ok_or_else(|| PlayerError::Renderer("upscaled luma texture alloc failed".to_string()))?;

    resources.pool = Some(MatPool {
        width,
        height,
        row_stride,
        plane_stride,
        output_format,
        features: [feature_buffer()?, feature_buffer()?, feature_buffer()?],
        output,
        frame_cache: FrameTokenCache::default(),
    });
    Ok(true)
}

pub(super) fn encode(
    resources: &mut Resources,
    device: &ProtocolObject<dyn MTLDevice>,
    command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    luma: &ProtocolObject<dyn MTLTexture>,
    output_format: MTLPixelFormat,
    frame_token: Option<u64>,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
    let width = luma.width();
    let height = luma.height();
    let fresh_pool = ensure_pool(resources, device, width, height, output_format)?;
    let pool = resources.pool.as_ref().expect("pool built above");
    if pool.frame_cache.matches(frame_token) {
        return Ok(pool.output.clone());
    }

    if fresh_pool {
        // The zero border around each feature plane provides the zero padding
        // of the convolutions; interiors are overwritten every frame.
        let Some(blit) = command_buffer.blitCommandEncoder() else {
            return Err(PlayerError::Renderer(
                "blitCommandEncoder returned nil".to_string(),
            ));
        };
        for buffer in &pool.features {
            blit.fillBuffer_range_value(buffer, NSRange::new(0, buffer.length()), 0);
        }
        blit.endEncoding();
    }

    let Some(encoder) = command_buffer.computeCommandEncoder() else {
        return Err(PlayerError::Renderer(
            "computeCommandEncoder returned nil".to_string(),
        ));
    };
    encoder.setLabel(Some(&NSString::from_str("erika_luma_upscaler_mm")));
    unsafe { encoder.setBuffer_offset_atIndex(Some(&resources.weights), 0, 3) };

    let offsets = &resources.offsets;
    let strip = resources.pxf * 8;
    let partial_strips = width % strip != 0;
    let base_params = MatParams {
        weight_offset: 0,
        bias_offset: 0,
        ones_offset: offsets.ones,
        identity_offset: offsets.identity,
        relu: 0,
        add_residual: 0,
        width: width as u32,
        height: height as u32,
        row_stride: pool.row_stride as u32,
        plane_stride: pool.plane_stride as u32,
    };

    let pixel_grid = MTLSize {
        width: width.div_ceil(16),
        height: height.div_ceil(16),
        depth: 1,
    };
    let pixel_threadgroup = MTLSize {
        width: 16,
        height: 16,
        depth: 1,
    };
    let strip_grid = MTLSize {
        width: width.div_ceil(strip),
        height: height.div_ceil(SIMDS),
        depth: 1,
    };
    let strip_threadgroup = MTLSize {
        width: 32 * SIMDS,
        height: 1,
        depth: 1,
    };

    // conv0: luma texture -> feature buffer A (linear, kept for the residual)
    encoder.setComputePipelineState(&resources.conv0_pipeline);
    unsafe { encoder.setTexture_atIndex(Some(luma), 0) };
    unsafe { encoder.setBuffer_offset_atIndex(Some(&pool.features[0]), 0, 1) };
    set_params(
        &encoder,
        &MatParams {
            weight_offset: offsets.conv0_w,
            bias_offset: offsets.conv0_b,
            ..base_params
        },
    );
    encoder.dispatchThreadgroups_threadsPerThreadgroup(pixel_grid, pixel_threadgroup);

    // conv1..conv5 ping-pong: A->B->C->B->C->B, conv5 adds the A residual.
    let chain: [(usize, usize); MID_LAYERS] = [(0, 1), (1, 2), (2, 1), (1, 2), (2, 1)];
    for (layer, (src, dst)) in chain.iter().enumerate() {
        let last = layer == MID_LAYERS - 1;
        encoder.setComputePipelineState(&resources.conv_pipeline);
        unsafe { encoder.setBuffer_offset_atIndex(Some(&pool.features[*src]), 0, 0) };
        unsafe { encoder.setBuffer_offset_atIndex(Some(&pool.features[*dst]), 0, 1) };
        unsafe { encoder.setBuffer_offset_atIndex(Some(&pool.features[0]), 0, 2) };
        set_params(
            &encoder,
            &MatParams {
                weight_offset: offsets.mid_w[layer],
                bias_offset: offsets.mid_b[layer],
                relu: u32::from(!last),
                add_residual: u32::from(last),
                ..base_params
            },
        );
        encoder.dispatchThreadgroups_threadsPerThreadgroup(strip_grid, strip_threadgroup);

        if partial_strips {
            // Partial strips store past the interior; restore the zero border
            // before the next layer reads it as padding.
            encoder.setComputePipelineState(&resources.zero_border_pipeline);
            unsafe { encoder.setBuffer_offset_atIndex(Some(&pool.features[*dst]), 0, 1) };
            set_params(&encoder, &base_params);
            let threads = (resources.channels / 8) * (height + 2);
            encoder.dispatchThreadgroups_threadsPerThreadgroup(
                MTLSize {
                    width: threads.div_ceil(256),
                    height: 1,
                    depth: 1,
                },
                MTLSize {
                    width: 256,
                    height: 1,
                    depth: 1,
                },
            );
        }
    }

    // conv6 + DepthToSpace + clip: B -> 2x luma texture
    encoder.setComputePipelineState(&resources.conv6_pipeline);
    unsafe { encoder.setBuffer_offset_atIndex(Some(&pool.features[1]), 0, 0) };
    unsafe { encoder.setTexture_atIndex(Some(&pool.output), 0) };
    set_params(
        &encoder,
        &MatParams {
            weight_offset: offsets.conv6_w,
            bias_offset: offsets.conv6_b,
            ..base_params
        },
    );
    encoder.dispatchThreadgroups_threadsPerThreadgroup(pixel_grid, pixel_threadgroup);

    let output = pool.output.clone();
    encoder.endEncoding();
    resources
        .pool
        .as_mut()
        .expect("pool built above")
        .frame_cache
        .commit(frame_token);
    Ok(output)
}

fn set_params(encoder: &ProtocolObject<dyn MTLComputeCommandEncoder>, params: &MatParams) {
    unsafe {
        encoder.setBytes_length_atIndex(
            NonNull::new((params as *const MatParams).cast::<c_void>().cast_mut())
                .expect("params pointer is non-null"),
            mem::size_of::<MatParams>(),
            4,
        );
    }
}

/// `{CH}`, `{PXF}`, `{SIMDS}` are substituted before compilation.
const MATMUL_SHADER_TEMPLATE: &str = r#"
#include <metal_stdlib>
#include <metal_simdgroup_matrix>
using namespace metal;

#define UNROLL _Pragma("clang loop unroll(full)")

constant constexpr uint CH = {CH};
constant constexpr uint NB = CH / 8;
constant constexpr uint PXF = {PXF};
constant constexpr uint SIMDS = {SIMDS};

struct MatParams {
    uint weight_offset;
    uint bias_offset;
    uint ones_offset;
    uint identity_offset;
    uint relu;
    uint add_residual;
    uint width;
    uint height;
    uint row_stride;
    uint plane_stride;
};

// element index of (ch, padded_y, padded_x): ch * plane_stride + y * row_stride + x

kernel void artcnn_mm_conv0(
    texture2d<half, access::read> luma [[texture(0)]],
    device half* dst [[buffer(1)]],
    constant half* weights [[buffer(3)]],
    constant MatParams& p [[buffer(4)]],
    uint2 gid [[thread_position_in_grid]])
{
    if (gid.x >= p.width || gid.y >= p.height) {
        return;
    }
    half acc[CH];
    for (uint c = 0; c < CH; ++c) {
        acc[c] = weights[p.bias_offset + c];
    }
    uint tap = 0;
    for (int dy = -1; dy <= 1; ++dy) {
        for (int dx = -1; dx <= 1; ++dx, ++tap) {
            int2 coord = int2(gid) + int2(dx, dy);
            bool outside = coord.x < 0 || coord.y < 0 || coord.x >= int(p.width) || coord.y >= int(p.height);
            half value = outside ? 0.0h : luma.read(uint2(coord)).x;
            for (uint c = 0; c < CH; ++c) {
                acc[c] += weights[p.weight_offset + tap * CH + c] * value;
            }
        }
    }
    uint base = (gid.y + 1) * 8u * p.row_stride + gid.x + 1;
    for (uint kb = 0; kb < NB; ++kb) {
        for (uint c = 0; c < 8; ++c) {
            dst[kb * p.plane_stride + base + c * p.row_stride] = acc[kb * 8u + c];
        }
    }
}

kernel void artcnn_mm_conv(
    device const half* src [[buffer(0)]],
    device half* dst [[buffer(1)]],
    device const half* residual [[buffer(2)]],
    device const half* weights [[buffer(3)]],
    constant MatParams& p [[buffer(4)]],
    uint2 tgid [[threadgroup_position_in_grid]],
    uint simd_id [[simdgroup_index_in_threadgroup]])
{
    uint y = tgid.y * SIMDS + simd_id;
    if (y >= p.height) {
        return;
    }
    uint x0 = tgid.x * (PXF * 8u);

    // acc[rb][f] accumulates output channels rb*8.. for pixel fragment f.
    simdgroup_half8x8 acc[NB][PXF];
    {
        simdgroup_half8x8 ones;
        simdgroup_load(ones, weights + p.ones_offset, 8);
        UNROLL for (uint rb = 0; rb < NB; ++rb) {
            simdgroup_half8x8 bias_block;
            simdgroup_load(bias_block, weights + p.bias_offset + rb * 64u, 8);
            UNROLL for (uint f = 0; f < PXF; ++f) {
                acc[rb][f] = make_filled_simdgroup_matrix<half, 8, 8>(0.0h);
                simdgroup_multiply_accumulate(acc[rb][f], bias_block, ones, acc[rb][f]);
            }
        }
    }

    for (uint tap = 0; tap < 9; ++tap) {
        int dy = int(tap / 3) - 1;
        int dx = int(tap % 3) - 1;
        device const half* wtap = weights + p.weight_offset + tap * CH * CH;
        uint row = uint(int(y) + 1 + dy) * 8u * p.row_stride;
        for (uint kb = 0; kb < NB; ++kb) {
            simdgroup_half8x8 w[NB];
            UNROLL for (uint rb = 0; rb < NB; ++rb) {
                simdgroup_load(w[rb], wtap + (rb * 8u) * CH + kb * 8u, CH);
            }
            device const half* xbase = src + kb * p.plane_stride + row;
            UNROLL for (uint f = 0; f < PXF; ++f) {
                uint px = uint(int(x0 + f * 8u) + 1 + dx);
                simdgroup_half8x8 x;
                simdgroup_load(x, xbase + px, p.row_stride);
                UNROLL for (uint rb = 0; rb < NB; ++rb) {
                    simdgroup_multiply_accumulate(acc[rb][f], w[rb], x, acc[rb][f]);
                }
            }
        }
    }

    if (p.add_residual != 0) {
        simdgroup_half8x8 identity;
        simdgroup_load(identity, weights + p.identity_offset, 8);
        uint row = (y + 1) * 8u * p.row_stride;
        UNROLL for (uint rb = 0; rb < NB; ++rb) {
            device const half* rbase = residual + rb * p.plane_stride + row;
            UNROLL for (uint f = 0; f < PXF; ++f) {
                simdgroup_half8x8 r;
                simdgroup_load(r, rbase + x0 + f * 8u + 1, p.row_stride);
                simdgroup_multiply_accumulate(acc[rb][f], r, identity, acc[rb][f]);
            }
        }
    }
    if (p.relu != 0) {
        UNROLL for (uint rb = 0; rb < NB; ++rb) {
            UNROLL for (uint f = 0; f < PXF; ++f) {
                thread auto& elems = acc[rb][f].thread_elements();
                elems[0] = max(elems[0], 0.0h);
                elems[1] = max(elems[1], 0.0h);
            }
        }
    }

    uint orow = (y + 1) * 8u * p.row_stride;
    UNROLL for (uint rb = 0; rb < NB; ++rb) {
        device half* obase = dst + rb * p.plane_stride + orow;
        UNROLL for (uint f = 0; f < PXF; ++f) {
            simdgroup_store(acc[rb][f], obase + x0 + f * 8u + 1, p.row_stride);
        }
    }
}

kernel void artcnn_mm_conv6(
    device const half* src [[buffer(0)]],
    texture2d<half, access::write> dst [[texture(0)]],
    constant half* weights [[buffer(3)]],
    constant MatParams& p [[buffer(4)]],
    uint2 gid [[thread_position_in_grid]])
{
    if (gid.x >= p.width || gid.y >= p.height) {
        return;
    }
    half4 acc = half4(
        weights[p.bias_offset],
        weights[p.bias_offset + 1],
        weights[p.bias_offset + 2],
        weights[p.bias_offset + 3]);
    uint tap = 0;
    for (int dy = -1; dy <= 1; ++dy) {
        for (int dx = -1; dx <= 1; ++dx, ++tap) {
            uint base = uint(int(gid.y) + 1 + dy) * 8u * p.row_stride + uint(int(gid.x) + 1 + dx);
            constant half* w = weights + p.weight_offset + tap * 4u * CH;
            for (uint kb = 0; kb < NB; ++kb) {
                device const half* values = src + kb * p.plane_stride + base;
                for (uint c8 = 0; c8 < 8; ++c8) {
                    uint c = kb * 8u + c8;
                    half value = values[c8 * p.row_stride];
                    acc += half4(w[c], w[CH + c], w[2u * CH + c], w[3u * CH + c]) * value;
                }
            }
        }
    }
    acc = clamp(acc, 0.0h, 1.0h);
    // DepthToSpace DCR: channel c maps to subpixel (dx, dy) = (c % 2, c / 2).
    uint2 out = gid * 2;
    dst.write(half4(acc.x), out);
    dst.write(half4(acc.y), out + uint2(1, 0));
    dst.write(half4(acc.z), out + uint2(0, 1));
    dst.write(half4(acc.w), out + uint2(1, 1));
}

// Restores the right zero border after partial-strip stores clobbered it.
kernel void artcnn_mm_zero_border(
    device half* plane [[buffer(1)]],
    constant MatParams& p [[buffer(4)]],
    uint gid [[thread_position_in_grid]])
{
    uint rows = p.height + 2;
    uint kb = gid / rows;
    uint yy = gid % rows;
    if (kb >= NB) {
        return;
    }
    device half* block = plane + kb * p.plane_stride + yy * 8u * p.row_stride;
    for (uint c = 0; c < 8; ++c) {
        for (uint xx = p.width + 1; xx < p.row_stride; ++xx) {
            block[c * p.row_stride + xx] = 0.0h;
        }
    }
}
"#;
