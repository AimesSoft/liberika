//! AMD FidelityFX Super Resolution 1.0 (EASU + RCAS) on luma.
//!
//! Metal port of the single-channel FSR1 in `renderer/sr/fsr1.rs`, which
//! follows `ffx_fsr1.h`:
//!
//! Copyright (c) 2021 Advanced Micro Devices, Inc. All rights reserved.
//! Permission is hereby granted, free of charge, to any person obtaining a
//! copy of this software and associated documentation files (the
//! "Software"), to deal in the Software without restriction, including
//! without limitation the rights to use, copy, modify, merge, publish,
//! distribute, sublicense, and/or sell copies of the Software, and to permit
//! persons to whom the Software is furnished to do so, subject to the
//! following conditions: The above copyright notice and this permission
//! notice shall be included in all copies or substantial portions of the
//! Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
//! EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
//! MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
//! NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
//! DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
//! OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
//! USE OR OTHER DEALINGS IN THE SOFTWARE.
//! (Full text: `assets/fsr1/LICENSE.FSR1`.)
//!
//! EASU scales straight to the presented size (any ratio, not just 2x) and
//! RCAS sharpens the result; both run in one kernel: each 16x16 threadgroup
//! evaluates EASU on its tile plus a one-pixel ring into threadgroup memory,
//! then applies RCAS from there, so the intermediate image never touches
//! device memory. Edge analysis uses the luma itself and reciprocals are
//! exact, matching the CPU reference.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandEncoder, MTLComputeCommandEncoder, MTLComputePipelineState,
    MTLDevice, MTLLibrary, MTLPixelFormat, MTLResource, MTLSize, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};

use crate::core::{PlayerError, Result};
use crate::renderer::sr::fsr1::{DEFAULT_SHARPNESS_STOPS, rcas_strength};

const TILE: usize = 16;
/// Output tile of the staged kernel (`OT` in the shader).
fn fsr_tile() -> usize {
    std::env::var("ERIKA_FSR_TILE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| (8..=56).contains(value))
        .unwrap_or(48)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FsrParams {
    /// Input / output size ratio.
    scale: [f32; 2],
    /// `0.5 * ratio - 0.5`: maps output pixel centers to input pixels.
    offset: [f32; 2],
    input_size: [i32; 2],
    output_size: [i32; 2],
    /// RCAS strength; `0` disables sharpening.
    sharpness: f32,
    _pad: [f32; 3],
}

pub struct FsrUpscaler {
    pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    tiled: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    sharpness: f32,
    output: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
}

// SAFETY: pipelines are thread-safe; the output texture is created and used
// on the render thread only.
unsafe impl Send for FsrUpscaler {}

impl FsrUpscaler {
    pub fn build(device: &ProtocolObject<dyn MTLDevice>) -> Result<Self> {
        // Half staging measured no faster and breaks the oracle tolerance.
        let stage = "float";
        let source = format!(
            "#define FSR_OT {}\ntypedef {stage} STAGE_T;\n{}",
            fsr_tile(),
            FSR1_SHADER.replacen("#include <metal_stdlib>", "", 1)
        );
        let source = format!("#include <metal_stdlib>\n{source}");
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(&source), None)
            .map_err(|error| {
                PlayerError::Renderer(format!(
                    "Metal FSR1 shader compile failed: {}",
                    error.localizedDescription()
                ))
            })?;
        let pipeline = |name: &str| {
            let function = library
                .newFunctionWithName(&NSString::from_str(name))
                .ok_or_else(|| PlayerError::Renderer(format!("Metal shader missing {name}")))?;
            device
                .newComputePipelineStateWithFunction_error(&function)
                .map_err(|error| PlayerError::Renderer(format!("FSR1 pipeline failed: {error}")))
        };
        Ok(Self {
            pipeline: pipeline("fsr1_easu_rcas")?,
            tiled: pipeline("fsr1_tiled")?,
            sharpness: rcas_strength(DEFAULT_SHARPNESS_STOPS),
            output: None,
        })
    }

    /// RCAS sharpness in stops (0 = strongest); `None` disables RCAS.
    pub fn set_sharpness_stops(&mut self, stops: Option<f32>) {
        self.sharpness = stops.map_or(0.0, rcas_strength);
    }

    pub fn allocated_bytes(&self) -> u64 {
        self.output
            .as_ref()
            .map_or(0, |texture| texture.allocatedSize() as u64)
    }

    /// Encodes EASU + RCAS from `luma` to a `width x height` texture.
    pub fn encode(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        luma: &ProtocolObject<dyn MTLTexture>,
        output_format: MTLPixelFormat,
        width: usize,
        height: usize,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        let reuse = self.output.as_ref().is_some_and(|texture| {
            texture.width() == width
                && texture.height() == height
                && texture.pixelFormat() == output_format
        });
        if !reuse {
            let descriptor = unsafe {
                let descriptor =
                    MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                        output_format,
                        width,
                        height,
                        false,
                    );
                descriptor.setStorageMode(MTLStorageMode::Private);
                descriptor.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
                descriptor
            };
            self.output = Some(
                device
                    .newTextureWithDescriptor(&descriptor)
                    .ok_or_else(|| {
                        PlayerError::Renderer("FSR1 output texture alloc failed".to_string())
                    })?,
            );
        }
        let output = self.output.clone().expect("output allocated above");
        let (in_width, in_height) = (luma.width(), luma.height());
        let ratio = [
            in_width as f32 / width as f32,
            in_height as f32 / height as f32,
        ];
        let params = FsrParams {
            scale: ratio,
            offset: [0.5 * ratio[0] - 0.5, 0.5 * ratio[1] - 0.5],
            input_size: [in_width as i32, in_height as i32],
            output_size: [width as i32, height as i32],
            sharpness: self.sharpness,
            _pad: [0.0; 3],
        };
        let Some(encoder) = command_buffer.computeCommandEncoder() else {
            return Err(PlayerError::Renderer(
                "computeCommandEncoder returned nil".to_string(),
            ));
        };
        encoder.setLabel(Some(&NSString::from_str("erika_fsr1")));
        // The staged kernel covers upscaling; any downscaling axis falls back
        // to direct texture reads.
        let tiled = ratio[0] <= 1.0 && ratio[1] <= 1.0;
        let (pipeline, tile, threads) = if tiled {
            (&self.tiled, fsr_tile(), (256, 1))
        } else {
            (&self.pipeline, TILE, (TILE, TILE))
        };
        encoder.setComputePipelineState(pipeline);
        unsafe {
            encoder.setTexture_atIndex(Some(luma), 0);
            encoder.setTexture_atIndex(Some(&output), 1);
            encoder.setBytes_length_atIndex(
                NonNull::new((&params as *const FsrParams).cast::<c_void>().cast_mut())
                    .expect("params non-null"),
                std::mem::size_of::<FsrParams>(),
                0,
            );
        }
        encoder.dispatchThreadgroups_threadsPerThreadgroup(
            MTLSize {
                width: width.div_ceil(tile),
                height: height.div_ceil(tile),
                depth: 1,
            },
            MTLSize {
                width: threads.0,
                height: threads.1,
                depth: 1,
            },
        );
        encoder.endEncoding();
        Ok(output)
    }
}

/// Fused EASU + RCAS, ported from `sr::fsr1::HLSL` (see the module docs for
/// the AMD license).
const FSR1_SHADER: &str = r#"
// AMD FidelityFX Super Resolution 1.0, single-channel port.
// Copyright (c) 2021 Advanced Micro Devices, Inc. All rights reserved.
// MIT license; see assets/fsr1/LICENSE.FSR1.
#include <metal_stdlib>
using namespace metal;

struct FsrParams {
    float2 scale;
    float2 offset;
    int2 input_size;
    int2 output_size;
    float sharpness;
    float pad0;
    float pad1;
    float pad2;
};

#define TILE 16
#define RING (TILE + 2)

inline float tap(texture2d<float, access::read> luma, int2 p, int2 size) {
    return luma.read(uint2(clamp(p, int2(0), size - 1))).r;
}

inline float rcp_or_zero(float v) { return v > 0.0f ? 1.0f / v : 0.0f; }

inline void easu_set(thread float2& dir, thread float& len, float w, float la, float lb, float lc, float ld, float le) {
    float dc = ld - lc;
    float cb = lc - lb;
    float len_x = rcp_or_zero(max(abs(dc), abs(cb)));
    float dir_x = ld - lb;
    dir.x += dir_x * w;
    len_x = saturate(abs(dir_x) * len_x);
    len += len_x * len_x * w;
    float ec = le - lc;
    float ca = lc - la;
    float len_y = rcp_or_zero(max(abs(ec), abs(ca)));
    float dir_y = le - la;
    dir.y += dir_y * w;
    len_y = saturate(abs(dir_y) * len_y);
    len += len_y * len_y * w;
}

inline void easu_tap(thread float& acc, thread float& aw, float2 off, float2 dir, float2 len2, float lob, float clp, float c) {
    float2 v;
    v.x = off.x * dir.x + off.y * dir.y;
    v.y = off.x * -dir.y + off.y * dir.x;
    v *= len2;
    float d2 = min(dot(v, v), clp);
    float wb = 2.0f / 5.0f * d2 - 1.0f;
    float wa = lob * d2 - 1.0f;
    wb *= wb;
    wa *= wa;
    wb = 25.0f / 16.0f * wb - (25.0f / 16.0f - 1.0f);
    float w = wb * wa;
    acc += c * w;
    aw += w;
}

inline float easu(texture2d<float, access::read> luma, int2 ip, constant FsrParams& p) {
    float2 pp = float2(ip) * p.scale + p.offset;
    float2 fp = floor(pp);
    pp -= fp;
    int2 f0 = int2(fp);
    int2 size = p.input_size;
    float b = tap(luma, f0 + int2(0, -1), size), c = tap(luma, f0 + int2(1, -1), size);
    float e = tap(luma, f0 + int2(-1, 0), size), f = tap(luma, f0, size);
    float g = tap(luma, f0 + int2(1, 0), size), h = tap(luma, f0 + int2(2, 0), size);
    float i = tap(luma, f0 + int2(-1, 1), size), j = tap(luma, f0 + int2(0, 1), size);
    float k = tap(luma, f0 + int2(1, 1), size), l = tap(luma, f0 + int2(2, 1), size);
    float n = tap(luma, f0 + int2(0, 2), size), o = tap(luma, f0 + int2(1, 2), size);
    float2 dir = float2(0.0f);
    float len = 0.0f;
    easu_set(dir, len, (1.0f - pp.x) * (1.0f - pp.y), b, e, f, g, j);
    easu_set(dir, len, pp.x * (1.0f - pp.y), c, f, g, h, k);
    easu_set(dir, len, (1.0f - pp.x) * pp.y, f, i, j, k, n);
    easu_set(dir, len, pp.x * pp.y, g, j, k, l, o);
    float dir_r = dot(dir, dir);
    bool zro = dir_r < 1.0f / 32768.0f;
    dir_r = zro ? 1.0f : precise::rsqrt(dir_r);
    dir.x = zro ? 1.0f : dir.x;
    dir *= dir_r;
    len = len * 0.5f;
    len *= len;
    float stretch = dot(dir, dir) / max(abs(dir.x), abs(dir.y));
    float2 len2 = float2(1.0f + (stretch - 1.0f) * len, 1.0f - 0.5f * len);
    float lob = 0.5f + ((1.0f / 4.0f - 0.04f) - 0.5f) * len;
    float clp = 1.0f / lob;
    float acc = 0.0f, aw = 0.0f;
    easu_tap(acc, aw, float2(0.0f, -1.0f) - pp, dir, len2, lob, clp, b);
    easu_tap(acc, aw, float2(1.0f, -1.0f) - pp, dir, len2, lob, clp, c);
    easu_tap(acc, aw, float2(-1.0f, 1.0f) - pp, dir, len2, lob, clp, i);
    easu_tap(acc, aw, float2(0.0f, 1.0f) - pp, dir, len2, lob, clp, j);
    easu_tap(acc, aw, float2(0.0f, 0.0f) - pp, dir, len2, lob, clp, f);
    easu_tap(acc, aw, float2(-1.0f, 0.0f) - pp, dir, len2, lob, clp, e);
    easu_tap(acc, aw, float2(1.0f, 1.0f) - pp, dir, len2, lob, clp, k);
    easu_tap(acc, aw, float2(2.0f, 1.0f) - pp, dir, len2, lob, clp, l);
    easu_tap(acc, aw, float2(2.0f, 0.0f) - pp, dir, len2, lob, clp, h);
    easu_tap(acc, aw, float2(1.0f, 0.0f) - pp, dir, len2, lob, clp, g);
    easu_tap(acc, aw, float2(1.0f, 2.0f) - pp, dir, len2, lob, clp, o);
    easu_tap(acc, aw, float2(0.0f, 2.0f) - pp, dir, len2, lob, clp, n);
    float mn = min(min(f, g), min(j, k));
    float mx = max(max(f, g), max(j, k));
    return clamp(acc / aw, mn, mx);
}

// Tiled variant for upscaling (input/output ratio <= 1): the input footprint
// of a 32x32 output tile plus its one-pixel ring is staged in threadgroup
// memory once, so EASU's 12 taps per evaluation become on-chip reads.
#define OT FSR_OT
#define ER (OT + 2)
#define IT (OT + 8)

inline float easu_staged(threadgroup const STAGE_T* src, int2 base, int2 ip, constant FsrParams& p) {
    float2 pp = float2(ip) * p.scale + p.offset;
    float2 fp = floor(pp);
    pp -= fp;
    int2 f0 = int2(fp) - base;
    threadgroup const STAGE_T* r0 = src + (f0.y - 1) * IT + f0.x;
    threadgroup const STAGE_T* r1 = r0 + IT;
    threadgroup const STAGE_T* r2 = r1 + IT;
    threadgroup const STAGE_T* r3 = r2 + IT;
    float b = r0[0], c = r0[1];
    float e = r1[-1], f = r1[0], g = r1[1], h = r1[2];
    float i = r2[-1], j = r2[0], k = r2[1], l = r2[2];
    float n = r3[0], o = r3[1];
    float2 dir = float2(0.0f);
    float len = 0.0f;
    easu_set(dir, len, (1.0f - pp.x) * (1.0f - pp.y), b, e, f, g, j);
    easu_set(dir, len, pp.x * (1.0f - pp.y), c, f, g, h, k);
    easu_set(dir, len, (1.0f - pp.x) * pp.y, f, i, j, k, n);
    easu_set(dir, len, pp.x * pp.y, g, j, k, l, o);
    float dir_r = dot(dir, dir);
    bool zro = dir_r < 1.0f / 32768.0f;
    dir_r = zro ? 1.0f : precise::rsqrt(dir_r);
    dir.x = zro ? 1.0f : dir.x;
    dir *= dir_r;
    len = len * 0.5f;
    len *= len;
    float stretch = dot(dir, dir) / max(abs(dir.x), abs(dir.y));
    float2 len2 = float2(1.0f + (stretch - 1.0f) * len, 1.0f - 0.5f * len);
    float lob = 0.5f + ((1.0f / 4.0f - 0.04f) - 0.5f) * len;
    float clp = 1.0f / lob;
    // Offsets are literal, so with the projections pre-scaled each tap's
    // rotated, stretched offset costs a few FMAs.
    float2 ax = dir * len2.x;
    float2 ay = float2(-dir.y, dir.x) * len2.y;
    float bx = -dot(pp, ax);
    float by = -dot(pp, ay);
    float acc = 0.0f, aw = 0.0f;
#define EASU_TAP(KX, KY, C) { \
        float vx = bx + (KX) * ax.x + (KY) * ax.y; \
        float vy = by + (KX) * ay.x + (KY) * ay.y; \
        float d2 = min(vx * vx + vy * vy, clp); \
        float wb = 2.0f / 5.0f * d2 - 1.0f; \
        float wa = lob * d2 - 1.0f; \
        wb *= wb; \
        wa *= wa; \
        float w = (25.0f / 16.0f * wb - (25.0f / 16.0f - 1.0f)) * wa; \
        acc += (C) * w; \
        aw += w; }
    EASU_TAP(0.0f, -1.0f, b)
    EASU_TAP(1.0f, -1.0f, c)
    EASU_TAP(-1.0f, 1.0f, i)
    EASU_TAP(0.0f, 1.0f, j)
    EASU_TAP(0.0f, 0.0f, f)
    EASU_TAP(-1.0f, 0.0f, e)
    EASU_TAP(1.0f, 1.0f, k)
    EASU_TAP(2.0f, 1.0f, l)
    EASU_TAP(2.0f, 0.0f, h)
    EASU_TAP(1.0f, 0.0f, g)
    EASU_TAP(1.0f, 2.0f, o)
    EASU_TAP(0.0f, 2.0f, n)
#undef EASU_TAP
    float mn = min(min(f, g), min(j, k));
    float mx = max(max(f, g), max(j, k));
    return clamp(acc / aw, mn, mx);
}

inline float rcas(float b, float d, float e, float f, float h, float sharpness) {
    float mn4 = min(min(b, d), min(f, h));
    float mx4 = max(max(b, d), max(f, h));
    float hit_min = min(mn4, e) / max(4.0f * mx4, 1.0f / 65536.0f);
    float hit_max = (1.0f - max(mx4, e)) / min(4.0f * mn4 - 4.0f, -1.0f / 65536.0f);
    float lobe = max(-(0.25f - 1.0f / 16.0f), min(max(-hit_min, hit_max), 0.0f)) * sharpness;
    return (lobe * (b + d + f + h) + e) / (4.0f * lobe + 1.0f);
}

kernel void fsr1_tiled(
    texture2d<float, access::read> luma [[texture(0)]],
    texture2d<float, access::write> dst [[texture(1)]],
    constant FsrParams& p [[buffer(0)]],
    uint2 gid [[threadgroup_position_in_grid]],
    uint tid [[thread_index_in_threadgroup]])
{
    threadgroup STAGE_T src[IT * IT];
    threadgroup STAGE_T lds[ER * ER];
    const int threads = 256;
    int2 origin = int2(gid) * OT - 1;
    int2 first = clamp(origin, int2(0), p.output_size - 1);
    // Taps reach one pixel left/up of floor(pp) for the first evaluation.
    int2 base = int2(floor(float2(first) * p.scale + p.offset)) - 1;
    for (int index = int(tid); index < IT * IT; index += threads) {
        int2 q = base + int2(index % IT, index / IT);
        src[index] = STAGE_T(luma.read(uint2(clamp(q, int2(0), p.input_size - 1))).r);
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (int index = int(tid); index < ER * ER; index += threads) {
        int2 q = clamp(origin + int2(index % ER, index / ER), int2(0), p.output_size - 1);
        lds[index] = STAGE_T(easu_staged(src, base, q, p));
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (int index = int(tid); index < OT * OT; index += threads) {
        int2 local = int2(index % OT, index / OT);
        int2 pix = int2(gid) * OT + local;
        if (any(pix >= p.output_size)) {
            continue;
        }
        int center = (local.y + 1) * ER + local.x + 1;
        float e = lds[center];
        float v = p.sharpness <= 0.0f
            ? e
            : rcas(lds[center - ER], lds[center - 1], e, lds[center + 1], lds[center + ER], p.sharpness);
        dst.write(float4(v), uint2(pix));
    }
}

kernel void fsr1_easu_rcas(
    texture2d<float, access::read> luma [[texture(0)]],
    texture2d<float, access::write> dst [[texture(1)]],
    constant FsrParams& p [[buffer(0)]],
    uint2 gid [[threadgroup_position_in_grid]],
    uint2 lid [[thread_position_in_threadgroup]])
{
    threadgroup float lds[RING * RING];
    int2 origin = int2(gid) * TILE - 1;
    uint lin = lid.y * TILE + lid.x;
    for (uint index = lin; index < RING * RING; index += TILE * TILE) {
        int2 q = origin + int2(index % RING, index / RING);
        // RCAS reads the EASU image clamp-to-edge.
        lds[index] = easu(luma, clamp(q, int2(0), p.output_size - 1), p);
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    int2 pix = int2(gid) * TILE + int2(lid);
    if (any(pix >= p.output_size)) {
        return;
    }
    uint center = (lid.y + 1) * RING + lid.x + 1;
    float e = lds[center];
    if (p.sharpness <= 0.0f) {
        dst.write(float4(e), uint2(pix));
        return;
    }
    float b = lds[center - RING], d = lds[center - 1], f = lds[center + 1], h = lds[center + RING];
    float mn4 = min(min(b, d), min(f, h));
    float mx4 = max(max(b, d), max(f, h));
    float hit_min = min(mn4, e) / max(4.0f * mx4, 1.0f / 65536.0f);
    float hit_max = (1.0f - max(mx4, e)) / min(4.0f * mn4 - 4.0f, -1.0f / 65536.0f);
    float lobe = max(-(0.25f - 1.0f / 16.0f), min(max(-hit_min, hit_max), 0.0f)) * p.sharpness;
    dst.write(float4((lobe * (b + d + f + h) + e) / (4.0f * lobe + 1.0f)), uint2(pix));
}
"#;
