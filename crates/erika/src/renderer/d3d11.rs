use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{HMODULE, HWND};
use ::windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use ::windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
    D3D_SRV_DIMENSION_TEXTURE2DARRAY, ID3DBlob,
};
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE,
    D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA, D3D11_BOX, D3D11_BUFFER_DESC,
    D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_INPUT_ELEMENT_DESC,
    D3D11_INPUT_PER_VERTEX_DATA, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_RENDER_TARGET_BLEND_DESC, D3D11_RESOURCE_MISC_SHARED, D3D11_SAMPLER_DESC,
    D3D11_SDK_VERSION, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_ARRAY_SRV, D3D11_TEX2D_SRV, D3D11_TEXTURE2D_DESC,
    D3D11_TEXTURE3D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, D3D11CreateDevice, ID3D11BlendState,
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11InputLayout, ID3D11Multithread,
    ID3D11PixelShader, ID3D11Query, ID3D11RenderTargetView, ID3D11Resource, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_COLOR_SPACE_TYPE, DXGI_FORMAT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_R8_UNORM,
    DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R32G32_FLOAT, DXGI_SAMPLE_DESC,
};
use ::windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAS_STILL_DRAWING, DXGI_HDR_METADATA_HDR10, DXGI_HDR_METADATA_TYPE_HDR10,
    DXGI_HDR_METADATA_TYPE_NONE, DXGI_PRESENT_DO_NOT_WAIT, DXGI_PRESENT_PARAMETERS,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, IDXGISwapChain3, IDXGISwapChain4,
};
use ::windows::core::{BOOL, IUnknown, Interface, PCSTR};

use crate::core::{
    ColorPrimaries, FlutterTextureKind, LumaUpscalerBackendStatus, PlatformSurface, PlayerError,
    PlayerVideoFrame, RenderFrameContext, RendererBackend, RendererRuntimeStats, Result,
    SurfaceMetrics, TransferFunction, WgpuSurfaceKind,
};
use crate::danmaku::{
    DanmakuAtlasUpdate, DanmakuGlyphAtlas, DanmakuGlyphInstance, DanmakuRenderPlan,
};
use crate::ffmpeg::{Frame, PlanarFrame, PlanarPixelFormat};
use crate::overlay::OverlayFrame;
use crate::renderer::d3d11_artcnn::D3d11ArtCnn;
use crate::renderer::gamut::{
    GamutLut, GamutLutJob, GamutLutParams, LUT_SIZE_C, LUT_SIZE_H, LUT_SIZE_I, pack_rgba16f,
};
use crate::renderer::metal::{MetalRendererConfig, VideoAlphaMode};
use crate::renderer::output::{
    ActiveOutputEncoding, OutputFallbackReason, OutputRuntimeStatus, OutputSurfaceFormat,
};
use crate::renderer::pipeline::{
    Chromaticity, LumaUpscalerMode, PrimariesCoordinates, SourceColorState, TargetColorState,
    VideoRenderPipeline, VideoUniforms, primaries_coordinates,
};
use crate::renderer::presentation::PresentationLayout;
use crate::subtitle::AssColor;

const SDR_SWAPCHAIN_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;
const HDR10_SWAPCHAIN_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R10G10B10A2_UNORM;
const SHADER_SOURCE: &[u8] = br#"
struct VsIn {
    float2 position : POSITION;
    float2 texcoord : TEXCOORD0;
};

struct VsOut {
    float4 position : SV_Position;
    float2 texcoord : TEXCOORD0;
};

struct DoviUniforms {
    float4 dovi_flags;
    float4 dovi_pivots[6];
    float4 dovi_bounds[3];
    float4 dovi_coefficients[24];
    float4 dovi_mmr[144];
    float4 dovi_nonlinear_matrix[3];
    float4 dovi_nonlinear_offset;
    float4 dovi_lms_matrix[3];
};

cbuffer VideoConstants : register(b0) {
    uint is_p010;
    uint full_range;
    uint source_transfer;
    uint target_transfer;
    uint tone_map;
    uint edr_output;
    uint input_mode;
    uint scene_linear;
    float4 nits;
    float4 luma_coefficients;
    float4 gamut_matrix_rows[3];
    float4 ipt_matrix_rows[9];
    float4 tone_map_extra;
    float4 tone_map_coeffs;
    uint gamut_lut_enabled;
    uint gamut_primaries;
    uint gamut_reserved0;
    uint gamut_reserved1;
    DoviUniforms dovi;
    // xy scales native/packed luma coordinates; zw scales native chroma.
    // D3D11VA textures can be allocation-aligned beyond the visible frame.
    float4 texture_scales;
};

Texture2D lumaTex : register(t0);
Texture2D chromaTex : register(t1);
Texture3D gamutLut : register(t2);
SamplerState videoSampler : register(s0);

float source_peak_nits() {
    return max(nits.x, 1.0);
}

float target_peak_nits() {
    return max(nits.y, 1.0);
}

float source_reference_white_nits() {
    return max(nits.z, 1.0);
}

float target_reference_white_nits() {
    return max(nits.w, 1.0);
}

float pq_eotf(float encoded) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(max(encoded, 0.0), 1.0 / m2);
    float num = max(p - c1, 0.0);
    float den = max(c2 - c3 * p, 0.000001);
    return pow(num / den, 1.0 / m1);
}

float pq_inverse_eotf(float normalized_nits) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(clamp(normalized_nits, 0.0, 1.0), m1);
    return pow((c1 + c2 * p) / max(1.0 + c3 * p, 0.000001), m2);
}

// BT.2100 HLG inverse OETF: nonlinear signal E' to scene linear light in
// [0, 1]. Mirrors the Rust reference implementation in
// `renderer/pipeline.rs` tests (`hlg_inverse_oetf`).
float hlg_inverse_oetf(float encoded) {
    const float a = 0.17883277;
    const float b = 0.28466892;
    const float c = 0.55991073;
    float e = max(encoded, 0.0);
    if (e <= 0.5) {
        return e * e / 3.0;
    }
    return (exp((e - c) / a) + b) / 12.0;
}

float3 transfer_to_source_reference_linear(float3 rgb_in) {
    float3 rgb = max(rgb_in, float3(0.0, 0.0, 0.0));
    if (source_transfer == 3u) {
        const float pq_absolute_peak_nits = 10000.0;
        return float3(pq_eotf(rgb.r), pq_eotf(rgb.g), pq_eotf(rgb.b))
            * (pq_absolute_peak_nits / source_reference_white_nits());
    }
    if (source_transfer == 4u) {
        // HLG: inverse OETF to scene linear, then the BT.2100 OOTF (system
        // gamma 1.2 at the 1000 nit nominal peak) to display linear,
        // normalized to source reference white like the PQ branch above.
        const float hlg_nominal_peak_nits = 1000.0;
        const float hlg_system_gamma = 1.2;
        float3 scene = float3(
            hlg_inverse_oetf(rgb.r),
            hlg_inverse_oetf(rgb.g),
            hlg_inverse_oetf(rgb.b)
        );
        float scene_luma = max(dot(luma_coefficients.xyz, scene), 0.000001);
        return scene * (hlg_nominal_peak_nits * pow(scene_luma, hlg_system_gamma - 1.0)
            / source_reference_white_nits());
    }
    if (source_transfer == 1u) {
        return pow(rgb, float3(2.2, 2.2, 2.2));
    }
    if (source_transfer == 2u) {
        return pow(rgb, float3(2.4, 2.4, 2.4));
    }
    return rgb;
}

float3 source_reference_to_nits(float3 rgb) {
    return max(rgb, float3(0.0, 0.0, 0.0)) * source_reference_white_nits();
}

float pq_code(float nits) {
    return pq_inverse_eotf(clamp(nits, 0.0, 10000.0) / 10000.0);
}

float nits_from_pq(float code) {
    return 10000.0 * pq_eotf(clamp(code, 0.0, 1.0));
}

// libplacebo pl_smoothstep with arbitrary edge order (HLSL smoothstep has
// undefined results when edge0 >= edge1, and libplacebo's knee tuning term
// deliberately uses reversed edges).
float sstep(float edge0, float edge1, float x) {
    float t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// Simple primaries conversion (HDR10 output path); the tone-mapped path
// converts primaries inside the IPT roundtrip instead.
float3 apply_gamut_map(float3 rgb) {
    return float3(
        dot(gamut_matrix_rows[0].xyz, rgb),
        dot(gamut_matrix_rows[1].xyz, rgb),
        dot(gamut_matrix_rows[2].xyz, rgb)
    );
}

// libplacebo st2094_pick_knee evaluated on absolute PQ codes. The source
// pivot follows the scene average luminance when known and stays within
// [10%, 80%] of the range; the destination pivot rescales it into the output
// range and then adapts towards the 1:1 line (knee_adaptation 0.4).
float2 st2094_pick_knee(float src_min, float src_max, float src_avg, float dst_min, float dst_max) {
    const float knee_adaptation = 0.4;
    const float min_knee = 0.1;
    const float max_knee = 0.8;
    const float def_knee = 0.4;
    float src_knee_min = lerp(src_min, src_max, min_knee);
    float src_knee_max = lerp(src_min, src_max, max_knee);
    float dst_knee_min = lerp(dst_min, dst_max, min_knee);
    float dst_knee_max = lerp(dst_min, dst_max, max_knee);
    float fallback = lerp(src_min, src_max, def_knee);
    float src_knee = clamp(src_avg > 0.0 ? src_avg : fallback, src_knee_min, src_knee_max);
    float target = (src_knee - src_min) / max(src_max - src_min, 0.000001);
    float adapted = lerp(dst_min, dst_max, target);
    float tuning = 1.0 - sstep(max_knee, def_knee, target) * sstep(min_knee, def_knee, target);
    float adaptation = lerp(knee_adaptation, 1.0, tuning);
    float dst_knee = clamp(lerp(src_knee, adapted, adaptation), dst_knee_min, dst_knee_max);
    return float2(src_knee, dst_knee);
}

// The tone-map curve evaluated on the IPT intensity axis (PQ codes),
// mirroring libplacebo's tone-map functions. `param` is the per-operator
// curve parameter from ToneMapConfig::curve_param (0 = operator default).
float tone_map_curve_pq(float x_in, float param) {
    float src_peak = source_peak_nits();
    float dst_peak = target_peak_nits();
    float src_avg = tone_map_extra.y;
    float dst_black = tone_map_extra.z;
    float in_min = 0.0;
    float in_max = max(pq_code(src_peak), 0.000001);
    float out_min = pq_code(dst_black);
    float out_max = max(pq_code(dst_peak), 0.000001);
    float out_range = max(out_max - out_min, 0.000001);
    float x = clamp(x_in, in_min, in_max);
    if (tone_map == 0u) {
        // Clip: values within the source range pass through untouched.
        return x;
    }
    if (tone_map == 1u) {
        // Reinhard (output-relative, libplacebo pl_tone_map_reinhard).
        //
        // libplacebo evaluates this curve in the linear PL_HDR_NORM domain
        // and only the PQ-domain operators (BT.2390, spline, ST 2094-10)
        // work on encoded values. Running it on PQ codes bends the curve's
        // meaning: against a 1000->203 nit target mid-tones darken badly, and
        // the Mobius knee lands far below its intended position. Rescale to
        // linear nits here and encode back at the end.
        float in_max_nits = max(src_peak, 0.000001);
        float out_min_nits = dst_black;
        float out_range_nits = max(dst_peak - dst_black, 0.000001);
        float peak = in_max_nits / out_range_nits;
        float contrast = param > 0.0 ? param : 0.5;
        float offset = (1.0 - contrast) / max(contrast, 0.000001);
        float scale = (peak + offset) / peak;
        float t = clamp(nits_from_pq(x), 0.0, in_max_nits) / out_range_nits;
        float mapped = t / (t + offset) * scale;
        return pq_code(mapped * out_range_nits + out_min_nits);
    }
    if (tone_map == 2u) {
        // Mobius: Mobius transform with a 1:1 linear region below the knee,
        // also evaluated in linear nits (see the Reinhard note above). The
        // knee j is relative to the output range, so the linear region ends
        // at dst_black + j * (dst_peak - dst_black).
        float in_max_nits = max(src_peak, 0.000001);
        float out_min_nits = dst_black;
        float out_range_nits = max(dst_peak - dst_black, 0.000001);
        float peak = in_max_nits / out_range_nits;
        float j = param > 0.0 ? param : 0.3;
        float a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
        float b = (j * j - 2.0 * j * peak + peak) / max(peak - 1.0, 0.000001);
        float scale = (b * b + 2.0 * b * j + j * j) / (b - a);
        float t = clamp(nits_from_pq(x), 0.0, in_max_nits) / out_range_nits;
        float mapped = t > j ? scale * (t + a) / (t + b) : t;
        return pq_code(mapped * out_range_nits + out_min_nits);
    }
    if (tone_map == 3u) {
        // ITU-R BT.2390 EETF with black-point compensation (the libplacebo
        // version also compensates target black; the earlier port skipped it).
        float knee_offset = param > 0.0 ? param : 1.0;
        float max_lum = clamp(out_max / in_max, 0.0, 1.0);
        float min_lum = out_min / in_max;
        float ks = (1.0 + knee_offset) * max_lum - knee_offset;
        float bp = min(max(1.0 / max(min_lum, 0.000001), 0.0), 4.0);
        float u = x / in_max;
        if (ks < 1.0 && u > ks) {
            float tb = (u - ks) / (1.0 - ks);
            float tb2 = tb * tb;
            float tb3 = tb2 * tb;
            u = (2.0 * tb3 - 3.0 * tb2 + 1.0) * ks
              + (tb3 - 2.0 * tb2 + tb) * (1.0 - ks)
              + (-2.0 * tb3 + 3.0 * tb2) * max_lum;
        }
        if (u < 1.0) {
            u = u + min_lum * pow(1.0 - u, bp);
            float gain = max_lum < 1.0
                ? 1.0 / (1.0 + min_lum / max_lum * pow(1.0 - max_lum, bp))
                : 1.0;
            u = gain * (u - min_lum) + min_lum;
        }
        return u * in_max;
    }
    if (tone_map == 4u) {
        // Spline: perceptually linear single-pivot polynomial, the default
        // tone map of libplacebo and mpv's gpu-next renderer.
        float contrast = param > 0.0 ? param : 0.3;
        float fallback_avg = clamp(0.4 * src_peak, 100.0, 400.0);
        float effective_src_avg = src_avg > 0.0 ? src_avg : fallback_avg;
        float2 knee = st2094_pick_knee(
            in_min,
            in_max,
            pq_code(effective_src_avg),
            out_min,
            out_max
        );
        float src_pivot = knee.x;
        float dst_pivot = knee.y;
        float slope0 = (dst_pivot - out_min) / max(src_pivot - in_min, 0.000001);
        float ratio = clamp(1.5 * (in_max / out_max - 1.0), 0.2, 1.2);
        float slope = pow(slope0, (1.0 - contrast) * ratio);
        float in_min0 = in_min - src_pivot;
        float in_max0 = in_max - src_pivot;
        float out_min0 = out_min - dst_pivot;
        float out_max0 = out_max - dst_pivot;
        float pa = (out_min0 - slope * in_min0) / (in_min0 * in_min0);
        float qa = (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0 * in_max0);
        float qb = -3.0 * (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0);
        float xr = x - src_pivot;
        float mapped = xr > 0.0
            ? ((qa * xr + qb) * xr + slope) * xr
            : (pa * xr + slope) * xr;
        return mapped + dst_pivot;
    }
    if (tone_map == 5u) {
        // ITU-R BT.2446 method A: Weber-law log compression from the source
        // peak envelope and a standardized S-curve (mpv's recommended curve
        // for well-mastered content).
        float phdr = 1.0 + 32.0 * pow(src_peak / 10000.0, 1.0 / 2.4);
        float psdr = 1.0 + 32.0 * pow(dst_peak / 10000.0, 1.0 / 2.4);
        float t = pow(nits_from_pq(x) / max(src_peak, 0.000001), 1.0 / 2.4);
        t = log(1.0 + (phdr - 1.0) * t) / log(phdr);
        if (t <= 0.7399) {
            t = 1.0770 * t;
        } else if (t < 0.9909) {
            t = (-1.1510 * t + 2.7811) * t - 0.6302;
        } else {
            t = 0.5 * t + 0.5;
        }
        t = (pow(psdr, t) - 1.0) / (psdr - 1.0);
        // BT.1886 EOTF from the target black point and peak.
        float lb = pow(max(dst_black, 0.0), 1.0 / 2.4);
        float lw = pow(max(dst_peak, 0.0), 1.0 / 2.4);
        return pq_code(pow((lw - lb) * t + lb, 2.4));
    }
    // SMPTE ST 2094-10 (DolbyVision's dynamic-metadata curve): rational
    // Mobius interpolation in absolute nits; coefficients are solved per
    // frame on the CPU from the same scene pivot.
    float c1 = tone_map_coeffs.x;
    float c2 = tone_map_coeffs.y;
    float c3 = tone_map_coeffs.z;
    float x_nits = nits_from_pq(x);
    float y_nits = (c1 + c2 * x_nits) / max(1.0 + c3 * x_nits, 0.000001);
    return pq_code(clamp(y_nits, 0.0, 10000.0));
}

float3 tone_map_nits(float3 input_nits) {
    if (target_transfer == 3u) {
        // HDR10 output: convert primaries by the gamut matrix and clamp to
        // the PQ range (no tone mapping; the display does the HDR mapping).
        return clamp(apply_gamut_map(max(input_nits, float3(0.0, 0.0, 0.0)) / source_reference_white_nits())
            * source_reference_white_nits(), float3(0.0, 0.0, 0.0), float3(10000.0, 10000.0, 10000.0));
    }
    // libplacebo color map: RGB in source primaries (absolute nits) to
    // HPE-LMS, PQ-encode, IPT, map the intensity axis and apply the
    // hue-preserving chroma rule, then decode back to RGB in the target
    // primaries. The primaries conversion happens inside this roundtrip.
    float3 rgb = max(input_nits, float3(0.0, 0.0, 0.0));
    float3 lms = float3(
        dot(ipt_matrix_rows[0].xyz, rgb),
        dot(ipt_matrix_rows[1].xyz, rgb),
        dot(ipt_matrix_rows[2].xyz, rgb)
    );
    float3 lmspq = float3(pq_code(lms.r), pq_code(lms.g), pq_code(lms.b));
    float3 ipt = float3(
        dot(float3(0.4, 0.4, 0.2), lmspq),
        dot(float3(4.455, -4.851, 0.396), lmspq),
        dot(float3(0.8056, 0.3572, -1.1628), lmspq)
    );
    float i_orig = ipt.x;
    ipt.x = tone_map_curve_pq(ipt.x, tone_map_extra.x);
    // Libplacebo's chroma rule: clamp the saturation boost when brightening
    // and desaturate (by the cubic hull term) when the mapping darkens.
    float2 hull = float2(i_orig, ipt.x);
    float2 hull_c = ((hull - float2(6.0, 6.0)) * hull + float2(9.0, 9.0)) * hull;
    float ratio = min(i_orig / max(ipt.x, 0.000001), hull_c.y / max(hull_c.x, 0.000001));
    ipt.yz = ipt.yz * ratio;
    if (gamut_lut_enabled != 0u) {
        // I axis spans the target's [black, peak] in PQ codes, matching
        // libplacebo's gamut.min_luma/max_luma (tone_map_extra.z is the
        // target black in nits, the same value the LUT was generated for).
        float lut_min = pq_code(tone_map_extra.z);
        float lut_max = max(pq_code(target_peak_nits()), 0.000001);
        float lut_range = max(lut_max - lut_min, 0.000001);
        float3 pos = float3(
            clamp((ipt.x - lut_min) / lut_range, 0.0, 1.0),
            clamp(2.0 * length(ipt.yz), 0.0, 1.0),
            0.5 + 0.5 * atan2(ipt.z, ipt.y) / 3.14159265
        );
        // libplacebo's texel_scale: the lattice position must be remapped to
        // the texel-center coordinate, otherwise the low end of the chroma
        // axis (whose first texel stores zero chroma) leaks in and crushes
        // saturation.
        float3 idx = float3(
            pos.x * (47.0 / 48.0) + 0.5 / 48.0,
            pos.y * (31.0 / 32.0) + 0.5 / 32.0,
            pos.z * (255.0 / 256.0) + 0.5 / 256.0
        );
        float3 sampled = gamutLut.Sample(videoSampler, idx).xyz;
        ipt = float3(sampled.x, sampled.y - 0.5, sampled.z - 0.5);
    }
    float3 lmspq_out = float3(
        dot(float3(1.0, 0.0975689, 0.205226), ipt),
        dot(float3(1.0, -0.113876, 0.133217), ipt),
        dot(float3(1.0, 0.0326151, -0.676887), ipt)
    );
    float3 lms_out = float3(
        nits_from_pq(lmspq_out.r),
        nits_from_pq(lmspq_out.g),
        nits_from_pq(lmspq_out.b)
    );
    return float3(
        dot(ipt_matrix_rows[6].xyz, lms_out),
        dot(ipt_matrix_rows[7].xyz, lms_out),
        dot(ipt_matrix_rows[8].xyz, lms_out)
    );
}

// Hue-preserving gamut mapping: the linear gamut matrix can push highly
// saturated wide-gamut colors outside the target gamut (negative
// components). Blending those towards luma shifts hue - BT.2020 primary
// red picks up blue and turns pink. Instead blend towards the naive clip
// by an out-of-gamut smoothstep factor: slightly-out colors stay nearly
// intact, strongly-out primaries land on the pure target primary with
// their hue intact, matching mpv's perceptual gamut handling. Mirrors the
// WGSL/MSL gamut_compress and the Rust reference in pipeline.rs tests.
// Brightness overshoot (> 1) is left for the tone map.
float3 gamut_compress(float3 rgb) {
    float lo = min(rgb.r, min(rgb.g, rgb.b));
    float outness = max(-lo, 0.0);
    float k = smoothstep(0.0, 1.0, outness);
    return lerp(rgb, clamp(rgb, 0.0, 1.0), k);
}

float3 target_nits_to_reference_linear(float3 input_nits) {
    // libplacebo's encode maps [target black, target peak] onto [0, 1] where
    // 1.0 is the target reference white, so the tone-map black-point
    // compensation lands back on true black instead of lifting it.
    float black = tone_map_extra.z;
    float peak = target_peak_nits();
    float range = max(peak - black, 0.0001);
    return max(input_nits - float3(black, black, black), float3(0.0, 0.0, 0.0)) / range
        * (range / target_reference_white_nits());
}

float3 target_reference_linear_to_output(float3 rgb) {
    if (scene_linear != 0u) {
        return max(rgb, float3(0.0, 0.0, 0.0));
    }
    if (target_transfer == 3u) {
        const float pq_absolute_peak_nits = 10000.0;
        float3 out_nits = max(rgb, float3(0.0, 0.0, 0.0)) * target_reference_white_nits();
        return float3(
            pq_inverse_eotf(out_nits.r / pq_absolute_peak_nits),
            pq_inverse_eotf(out_nits.g / pq_absolute_peak_nits),
            pq_inverse_eotf(out_nits.b / pq_absolute_peak_nits)
        );
    }
    if (edr_output != 0u) {
        return max(rgb, float3(0.0, 0.0, 0.0));
    }
    if (target_transfer == 1u) {
        return pow(max(rgb, float3(0.0, 0.0, 0.0)), float3(1.0 / 2.2, 1.0 / 2.2, 1.0 / 2.2));
    }
    if (target_transfer == 2u) {
        return pow(max(rgb, float3(0.0, 0.0, 0.0)), float3(1.0 / 2.4, 1.0 / 2.4, 1.0 / 2.4));
    }
    return rgb;
}

float4 final_output(float3 rgb, float alpha) {
    float3 output_rgb;
    if (scene_linear != 0u) {
        output_rgb = max(rgb, float3(0.0, 0.0, 0.0)) * alpha;
        return float4(output_rgb, alpha);
    }
    if (target_transfer == 3u) {
        output_rgb = clamp(rgb, float3(0.0, 0.0, 0.0), float3(1.0, 1.0, 1.0)) * alpha;
        return float4(output_rgb, alpha);
    }
    if (edr_output != 0u) {
        float headroom = max(target_peak_nits() / target_reference_white_nits(), 1.0);
        output_rgb = clamp(rgb, float3(0.0, 0.0, 0.0), float3(headroom, headroom, headroom)) * alpha;
        return float4(output_rgb, alpha);
    }
    output_rgb = clamp(rgb, float3(0.0, 0.0, 0.0), float3(1.0, 1.0, 1.0)) * alpha;
    return float4(output_rgb, alpha);
}

void expand_ycbcr_range(float y_in, float2 cbcr_in, out float y, out float2 cbcr) {
    if (is_p010 != 0u) {
        // P010 stores 10-bit codes as code << 6 in a 16-bit UNORM texture.
        const float p010_scale = 65535.0 / 65472.0;
        y_in *= p010_scale;
        cbcr_in *= p010_scale;
    }
    if (full_range != 0u) {
        y = y_in;
        cbcr = cbcr_in - float2(0.5, 0.5);
        return;
    }
    if (is_p010 != 0u) {
        y = (y_in - (64.0 / 1023.0)) * (1023.0 / 876.0);
        cbcr = (cbcr_in - float2(512.0 / 1023.0, 512.0 / 1023.0)) * (1023.0 / 896.0);
        return;
    }
    y = (y_in - (16.0 / 255.0)) * (255.0 / 219.0);
    cbcr = (cbcr_in - float2(128.0 / 255.0, 128.0 / 255.0)) * (255.0 / 224.0);
}

// Dolby Vision RPU reshaping, ported from libplacebo's `pl_shader_dovi_reshape`
// (the renderer behind mpv's Dolby Vision mapping). The base-layer signal is
// reshaped per component through piecewise polynomial/MMR curves selected by
// pivot comparison, where MMR coefficients mix all three raw components.
float3 dovi_reshaped_signal(float3 sig_in) {
    float3 sig = clamp(sig_in, 0.0, 1.0);
    float result[3] = { sig.r, sig.g, sig.b };
    float4 flags = dovi.dovi_flags;
    for (uint c = 0; c < 3u; c++) {
        uint segments = uint(flags[1u + c]);
        if (segments == 0u) {
            continue;
        }
        float s = result[c];
        uint index = 0u;
        for (uint i = 0u; i < 7u; i++) {
            float4 pivot_row = dovi.dovi_pivots[2u * c + i / 4u];
            float pivot = pivot_row[i % 4u];
            if (s >= pivot) {
                index = index + 1u;
            }
        }
        float4 coeff = dovi.dovi_coefficients[8u * c + index];
        if (coeff.w < 0.5) {
            s = (coeff.z * s + coeff.y) * s + coeff.x;
        } else {
            uint base = 48u * c + uint(coeff.y);
            uint order = uint(coeff.w);
            float4 sig_x = float4(
                sig.x * sig.y,
                sig.x * sig.z,
                sig.y * sig.z,
                sig.x * sig.y * sig.z
            );
            s = coeff.x;
            s = s + dot(dovi.dovi_mmr[base].xyz, sig);
            s = s + dot(dovi.dovi_mmr[base + 1u], sig_x);
            if (order >= 2u) {
                float3 sig2 = sig * sig;
                float4 sig_x2 = sig_x * sig_x;
                s = s + dot(dovi.dovi_mmr[base + 2u].xyz, sig2);
                s = s + dot(dovi.dovi_mmr[base + 3u], sig_x2);
                if (order >= 3u) {
                    s = s + dot(dovi.dovi_mmr[base + 4u].xyz, sig2 * sig);
                    s = s + dot(dovi.dovi_mmr[base + 5u], sig_x2 * sig_x);
                }
            }
        }
        float4 bounds = dovi.dovi_bounds[c];
        result[c] = clamp(s, bounds.x, bounds.y);
    }
    return float3(result[0], result[1], result[2]);
}

// Reshaped nonlinear signal to PQ-encoded IPT via the RPU's ycc_to_rgb matrix
// and signal offsets. Applying the RPU offsets keeps integer offset codes
// exactly on sample codes (2^bits/(2^bits-1) folded in on the CPU).
float3 dovi_signal_to_pq_rgb(float3 sig) {
    float3 reshaped = dovi_reshaped_signal(sig) - dovi.dovi_nonlinear_offset.xyz;
    return float3(
        dot(dovi.dovi_nonlinear_matrix[0].xyz, reshaped),
        dot(dovi.dovi_nonlinear_matrix[1].xyz, reshaped),
        dot(dovi.dovi_nonlinear_matrix[2].xyz, reshaped)
    );
}

// Linearized BT.2020-referred HPE LMS back to linear RGB, using the composite
// of the fixed HPE inverse with the RPU's rgb_to_lms matrix (premultiplied on
// the CPU, matching libplacebo's dovi_lms2rgb).
float3 dovi_lms_to_rgb(float3 lin) {
    return float3(
        dot(dovi.dovi_lms_matrix[0].xyz, lin),
        dot(dovi.dovi_lms_matrix[1].xyz, lin),
        dot(dovi.dovi_lms_matrix[2].xyz, lin)
    );
}

VsOut vs_main(VsIn input) {
    VsOut output;
    output.position = float4(input.position, 0.0, 1.0);
    output.texcoord = input.texcoord;
    return output;
}

float packed_luma_texel(int2 virtual_coord_in, int2 virtual_size) {
    int2 virtual_coord = clamp(virtual_coord_in, int2(0, 0), virtual_size - int2(1, 1));
    int2 packed_coord = virtual_coord >> int2(1, 1);
    float4 packed = lumaTex.Load(int3(packed_coord, 0));
    uint component = uint((virtual_coord.y & 1) * 2 + (virtual_coord.x & 1));
    if (component == 0) {
        return packed.r;
    }
    if (component == 1) {
        return packed.g;
    }
    if (component == 2) {
        return packed.b;
    }
    return packed.a;
}

// Reconstruct bilinear sampling of the virtual 2W x 2H luma texture stored
// as packed TL/TR/BL/BR RGBA texels by the D3D11 ArtCNN compute pass.
float sample_packed_luma(float2 texcoord) {
    uint packed_width;
    uint packed_height;
    lumaTex.GetDimensions(packed_width, packed_height);
    int2 virtual_size = int2(packed_width, packed_height) * int2(2, 2);
    float2 sample_position = saturate(texcoord) * float2(virtual_size) - float2(0.5, 0.5);
    int2 lo = int2(floor(sample_position));
    float2 fraction = frac(sample_position);
    float y0 = lerp(
        packed_luma_texel(lo, virtual_size),
        packed_luma_texel(lo + int2(1, 0), virtual_size),
        fraction.x
    );
    float y1 = lerp(
        packed_luma_texel(lo + int2(0, 1), virtual_size),
        packed_luma_texel(lo + int2(1, 1), virtual_size),
        fraction.x
    );
    return lerp(y0, y1, fraction.y);
}

float4 ps_main(VsOut input) : SV_Target {
    uint base_input_mode = input_mode & 255u;
    bool packed_alpha = (input_mode & 256u) != 0u;
    float2 color_coord = packed_alpha
        ? float2(input.texcoord.x * 0.5, input.texcoord.y)
        : input.texcoord;
    float2 alpha_coord = float2(0.5 + input.texcoord.x * 0.5, input.texcoord.y);
    float2 luma_coord = color_coord * texture_scales.xy;
    float2 chroma_coord = color_coord * texture_scales.zw;
    float y_sample = base_input_mode == 2u
        ? sample_packed_luma(luma_coord)
        : lumaTex.Sample(videoSampler, luma_coord).r;
    float2 cbcr_sample = chromaTex.Sample(videoSampler, chroma_coord).rg;
    bool dovi_enabled = dovi.dovi_flags.x != 0.0;
    bool dovi_ycbcr_input = dovi_enabled && (base_input_mode == 0u || base_input_mode == 2u);
    float3 rgb;
    if (dovi_ycbcr_input) {
        // The base layer carries the raw 12-bit DV signal (10-bit container,
        // full range); range expansion and the YCbCr matrix are replaced by
        // the RPU reshaping + ycc_to_rgb path.
        float3 sig = float3(y_sample, cbcr_sample.x, cbcr_sample.y);
        if (is_p010 != 0u) {
            sig *= 65535.0 / 65472.0;
        }
        rgb = dovi_signal_to_pq_rgb(sig);
    } else {
        float y;
        float2 cbcr;
        expand_ycbcr_range(y_sample, cbcr_sample, y, cbcr);

        float kr = luma_coefficients.x;
        float kg = max(luma_coefficients.y, 0.000001);
        float kb = luma_coefficients.z;
        rgb.r = y + 2.0 * (1.0 - kr) * cbcr.y;
        rgb.b = y + 2.0 * (1.0 - kb) * cbcr.x;
        rgb.g = (y - kr * rgb.r - kb * rgb.b) / kg;
    }
    rgb = transfer_to_source_reference_linear(rgb);
    if (dovi_ycbcr_input) {
        rgb = dovi_lms_to_rgb(rgb);
    }
    rgb = source_reference_to_nits(rgb);
    rgb = tone_map_nits(rgb);
    rgb = target_nits_to_reference_linear(rgb);
    rgb = gamut_compress(rgb);
    rgb = target_reference_linear_to_output(rgb);
    float alpha = 1.0;
    if (packed_alpha) {
        float alpha_sample = lumaTex.Sample(
            videoSampler,
            alpha_coord * texture_scales.xy
        ).r;
        float unused_y;
        float2 unused_cbcr;
        expand_ycbcr_range(alpha_sample, float2(0.5, 0.5), unused_y, unused_cbcr);
        alpha = saturate(unused_y);
    }
    return final_output(rgb, alpha);
}

float4 encode_ps_main(VsOut input) : SV_Target {
    float3 rgb = lumaTex.Sample(videoSampler, input.texcoord).rgb;
    rgb = target_reference_linear_to_output(rgb);
    return final_output(rgb, 1.0);
}
"#;

const OVERLAY_SHADER_SOURCE: &[u8] = br#"
struct VsIn {
    float2 position : POSITION;
    float2 texcoord : TEXCOORD0;
};

struct VsOut {
    float4 position : SV_Position;
    float2 texcoord : TEXCOORD0;
};

cbuffer OverlayConstants : register(b0) {
    float4 rect;
    float4 tex_rect;
    float2 viewport;
    uint overlay_mode;
    uint reserved0;
    float4 color;
    float4 ui_nits;
};

Texture2D overlayTex : register(t0);
SamplerState overlaySampler : register(s0);

// SDR keeps its existing gamma-space composition. For HDR10, ui_nits.y is
// the scene target's reference white, so return reference-linear values and
// let the final encode pass apply PQ after fixed-function alpha blending.
float3 sdr_ui_color_to_target_output(float3 rgb) {
    if (ui_nits.y > 0.0) {
        float3 linear_rgb = pow(max(rgb, float3(0.0, 0.0, 0.0)), float3(2.2, 2.2, 2.2));
        return linear_rgb * (max(ui_nits.x, 1.0) / max(ui_nits.y, 1.0));
    }
    return rgb;
}

VsOut overlay_vs_main(VsIn input) {
    float2 pixel = rect.xy + input.texcoord * rect.zw;
    float2 safe_viewport = max(viewport, float2(1.0, 1.0));
    float2 ndc = float2(
        pixel.x / safe_viewport.x * 2.0 - 1.0,
        1.0 - pixel.y / safe_viewport.y * 2.0
    );
    VsOut output;
    output.position = float4(ndc, 0.0, 1.0);
    output.texcoord = tex_rect.xy + input.texcoord * tex_rect.zw;
    return output;
}

float4 overlay_ps_main(VsOut input) : SV_Target {
    float4 sampled = overlayTex.Sample(overlaySampler, input.texcoord);
    if (overlay_mode == 1u) {
        float3 rgb = sdr_ui_color_to_target_output(color.rgb);
        return float4(rgb, color.a * sampled.r);
    }
    sampled.rgb = sdr_ui_color_to_target_output(sampled.rgb);
    return sampled;
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct VideoVertex {
    position: [f32; 2],
    texcoord: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct D3d11VideoConstants {
    common: VideoUniforms,
    texture_scales: [f32; 4],
}

/// Overlay quad uniforms, byte-compatible with the HLSL `OverlayConstants`
/// cbuffer (80 bytes, 16-byte aligned). `ui_nits.x` is SDR UI white and
/// `ui_nits.y` is the target reference white when compositing scene-linear.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct OverlayUniforms {
    rect: [f32; 4],
    tex_rect: [f32; 4],
    viewport: [f32; 2],
    overlay_mode: u32,
    reserved0: u32,
    color: [f32; 4],
    ui_nits: [f32; 4],
}

impl OverlayUniforms {
    #[allow(clippy::too_many_arguments)]
    fn rgba_plane(
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
    ) -> Self {
        Self {
            rect: [x as f32, y as f32, width as f32, height as f32],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            viewport: [viewport_w.max(1) as f32, viewport_h.max(1) as f32],
            overlay_mode: 0,
            reserved0: 0,
            color: [1.0, 1.0, 1.0, 1.0],
            ui_nits: overlay_ui_nits(target),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn alpha_atlas(
        color_rgba: u32,
        place_x: i32,
        place_y: i32,
        place_w: u32,
        place_h: u32,
        atlas_x: u32,
        atlas_w: u32,
        atlas_h: u32,
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
    ) -> Self {
        let color = AssColor::from_libass_rgba(color_rgba);
        let aw = atlas_w.max(1) as f32;
        let ah = atlas_h.max(1) as f32;
        Self {
            rect: [
                place_x as f32,
                place_y as f32,
                place_w as f32,
                place_h as f32,
            ],
            tex_rect: [
                atlas_x as f32 / aw,
                0.0,
                place_w as f32 / aw,
                place_h as f32 / ah,
            ],
            viewport: [viewport_w.max(1) as f32, viewport_h.max(1) as f32],
            overlay_mode: 1,
            reserved0: 0,
            color: [
                f32::from(color.red) / 255.0,
                f32::from(color.green) / 255.0,
                f32::from(color.blue) / 255.0,
                f32::from(color.alpha) / 255.0,
            ],
            ui_nits: overlay_ui_nits(target),
        }
    }

    fn alpha_atlas_rect(
        color: [f32; 4],
        rect: [f32; 4],
        tex_rect: [f32; 4],
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
    ) -> Self {
        Self {
            rect,
            tex_rect,
            viewport: [viewport_w.max(1) as f32, viewport_h.max(1) as f32],
            overlay_mode: 1,
            reserved0: 0,
            color,
            ui_nits: overlay_ui_nits(target),
        }
    }
}

fn overlay_ui_nits(target: TargetColorState) -> [f32; 4] {
    if matches!(target.transfer, TransferFunction::Pq) {
        let reference_white = target.reference_white_nits.max(1.0);
        [reference_white, reference_white, 0.0, 0.0]
    } else {
        [100.0, 0.0, 0.0, 0.0]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct D3d11RendererStats {
    pub surface_width: u32,
    pub surface_height: u32,
    pub rendered_frames: u64,
    pub software_video_frames: u64,
    pub hardware_video_frames: u64,
    pub zero_copy_video_frames: u64,
    pub direct_zero_copy_video_frames: u64,
    pub shared_handle_video_frames: u64,
    pub cpu_video_frame_fallbacks: u64,
    pub hdr_source_frames: u64,
    pub hdr10_output_frames: u64,
    pub sdr_tonemap_frames: u64,
    pub hdr10_metadata_updates: u64,
    pub hdr10_metadata_failures: u64,
    pub hdr10_output_failures: u64,
    pub hdr10_output_active: bool,
    pub import_failures: u64,
    pub prepared_overlay_frames: u64,
    pub prepared_overlay_subtitle_planes: u64,
    pub overlay_alpha_atlas_uploads: u64,
    pub overlay_alpha_atlas_reuses: u64,
    pub danmaku_passes: u64,
    pub danmaku_items: u64,
    pub attached: bool,
}

struct D3d11DeviceState {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    encode_pixel_shader: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    vertex_buffer: ID3D11Buffer,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    overlay_vertex_shader: ID3D11VertexShader,
    overlay_pixel_shader: ID3D11PixelShader,
    overlay_constants: ID3D11Buffer,
    overlay_sampler: ID3D11SamplerState,
    overlay_blend: ID3D11BlendState,
}

struct AttachedSurface {
    hwnd: HWND,
    composition: bool,
    flutter_texture: bool,
    metrics: SurfaceMetrics,
    output_mode: D3d11OutputMode,
    swapchain: Option<IDXGISwapChain1>,
    output_texture: Option<ID3D11Texture2D>,
    // Immutable, GPU-complete snapshot exported to Flutter. Never render into
    // this resource again: opening its handle is not GPU consumer completion.
    published_texture: Option<ID3D11Texture2D>,
    render_target: Option<ID3D11RenderTargetView>,
    linear_render_target: Option<ID3D11RenderTargetView>,
    linear_shader_resource: Option<ID3D11ShaderResourceView>,
}

impl AttachedSurface {
    fn targets_ready(&self) -> bool {
        (if self.flutter_texture {
            self.output_texture.is_some()
        } else {
            self.swapchain.is_some()
        }) && self.render_target.is_some()
            && (!matches!(self.output_mode, D3d11OutputMode::Hdr10)
                || (self.linear_render_target.is_some() && self.linear_shader_resource.is_some()))
    }
}

struct ImportedVideoFrame {
    _frame: Option<Frame>,
    _texture: ID3D11Texture2D,
    _chroma_texture: Option<ID3D11Texture2D>,
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
    tex_rect: D3d11TexRect,
    _array_index: u32,
    frame_token: u64,
    constants: VideoUniforms,
}

struct FlutterTextureScene {
    frame_token: u64,
    generation: u64,
    upscaler: LumaUpscalerMode,
    overlay: Option<OverlayFrame>,
    danmaku: Option<DanmakuRenderPlan>,
}

impl FlutterTextureScene {
    fn new(frame_token: u64, upscaler: LumaUpscalerMode, context: RenderFrameContext<'_>) -> Self {
        Self {
            frame_token,
            generation: context.generation,
            upscaler,
            overlay: context.overlay.cloned(),
            danmaku: context.danmaku.cloned(),
        }
    }

    fn matches(
        &self,
        frame_token: u64,
        upscaler: LumaUpscalerMode,
        context: RenderFrameContext<'_>,
    ) -> bool {
        self.frame_token == frame_token
            && self.generation == context.generation
            && self.upscaler == upscaler
            && self.overlay_matches(context.overlay)
            && self.danmaku_matches(context.danmaku)
    }

    fn overlay_matches(&self, overlay: Option<&OverlayFrame>) -> bool {
        match (self.overlay.as_ref(), overlay) {
            (Some(a), Some(b)) => {
                a.viewport == b.viewport
                    && a.subtitle_planes == b.subtitle_planes
                    && a.subtitle_alpha_planes == b.subtitle_alpha_planes
            }
            (None, None) => true,
            _ => false,
        }
    }

    fn danmaku_matches(&self, danmaku: Option<&DanmakuRenderPlan>) -> bool {
        match (self.danmaku.as_ref(), danmaku) {
            (Some(a), Some(b)) => {
                a.viewport == b.viewport && a.atlas == b.atlas && a.items == b.items
            }
            (None, None) => true,
            _ => false,
        }
    }

    fn update(
        &mut self,
        frame_token: u64,
        upscaler: LumaUpscalerMode,
        context: RenderFrameContext<'_>,
    ) {
        // A new video frame need not copy unchanged subtitle/HUD pixels or
        // danmaku instances. Retain each cached buffer until its own content
        // changes; keep exact comparisons rather than lossy fingerprints.
        if !self.overlay_matches(context.overlay) {
            self.overlay = context.overlay.cloned();
        }
        if !self.danmaku_matches(context.danmaku) {
            self.danmaku = context.danmaku.cloned();
        }
        self.frame_token = frame_token;
        self.generation = context.generation;
        self.upscaler = upscaler;
    }
}

#[derive(Debug, Clone, Copy)]
struct D3d11DrawRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct D3d11TexRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl D3d11TexRect {
    const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };

    fn visible_region(
        visible_width: u32,
        visible_height: u32,
        texture_width: u32,
        texture_height: u32,
    ) -> Self {
        let texture_width = texture_width.max(1);
        let texture_height = texture_height.max(1);
        Self {
            x: 0.0,
            y: 0.0,
            width: visible_width.max(1).min(texture_width) as f32 / texture_width as f32,
            height: visible_height.max(1).min(texture_height) as f32 / texture_height as f32,
        }
    }

    fn right(self) -> f32 {
        (self.x + self.width).min(1.0)
    }

    fn bottom(self) -> f32 {
        (self.y + self.height).min(1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum D3d11OutputMode {
    #[default]
    Sdr,
    Hdr10,
}

impl D3d11OutputMode {
    fn swapchain_format(self) -> DXGI_FORMAT {
        match self {
            Self::Sdr => SDR_SWAPCHAIN_FORMAT,
            Self::Hdr10 => HDR10_SWAPCHAIN_FORMAT,
        }
    }

    fn color_space(self) -> DXGI_COLOR_SPACE_TYPE {
        match self {
            Self::Sdr => DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            Self::Hdr10 => DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
        }
    }

    fn target_color(self) -> TargetColorState {
        match self {
            Self::Sdr => TargetColorState::sdr(ColorPrimaries::Bt709),
            Self::Hdr10 => TargetColorState::hdr10(ColorPrimaries::Bt2020),
        }
    }

    fn target_color_for_source(self, source: SourceColorState) -> TargetColorState {
        match self {
            Self::Sdr if source.is_hdr() => {
                TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709)
            }
            _ => self.target_color(),
        }
    }
}

#[derive(Clone)]
struct D3d11OverlayTexture {
    _texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
}

struct D3d11OverlayDraw {
    texture: D3d11OverlayTexture,
    constants: OverlayUniforms,
}

struct D3d11DanmakuAtlasCache {
    version: u64,
    width: u32,
    height: u32,
    stride: usize,
    fill: D3d11OverlayTexture,
    outline: D3d11OverlayTexture,
}

impl D3d11DanmakuAtlasCache {
    fn can_reuse_for(&self, atlas: &DanmakuGlyphAtlas) -> bool {
        self.version == atlas.version
            && self.width == atlas.width
            && self.height == atlas.height
            && self.stride == atlas.stride
    }
}

pub struct D3d11Renderer {
    state: Option<D3d11DeviceState>,
    surface: Option<AttachedSurface>,
    current_video: Option<ImportedVideoFrame>,
    flutter_scene: Option<FlutterTextureScene>,
    danmaku_atlas_cache: Option<D3d11DanmakuAtlasCache>,
    requested_output_mode: crate::renderer::output::OutputMode,
    video_alpha_mode: VideoAlphaMode,
    upscaler_mode: LumaUpscalerMode,
    upscaler: D3d11ArtCnn,
    next_frame_token: u64,
    hdr10_output_unavailable: bool,
    /// Cached perceptual gamut LUT (3D RGBA16F SRV), keyed the same way as
    /// the wgpu/Metal caches: (source, target, target black PQ, target peak PQ).
    gamut_lut: Option<(u32, u32, u32, u32, ID3D11ShaderResourceView)>,
    /// Background generation for a cache miss; the fast `gamut_compress`
    /// path renders until the LUT lands.
    gamut_lut_job: Option<GamutLutJob>,
    stats: D3d11RendererStats,
}

impl D3d11Renderer {
    pub fn new() -> Result<Self> {
        Self::with_config(MetalRendererConfig::default())
    }

    pub fn with_config(config: MetalRendererConfig) -> Result<Self> {
        Ok(Self {
            state: None,
            surface: None,
            current_video: None,
            flutter_scene: None,
            danmaku_atlas_cache: None,
            requested_output_mode: config.output_mode,
            video_alpha_mode: config.video_alpha_mode,
            upscaler_mode: config.luma_upscaler,
            upscaler: D3d11ArtCnn::default(),
            next_frame_token: 0,
            hdr10_output_unavailable: false,
            gamut_lut: None,
            gamut_lut_job: None,
            stats: D3d11RendererStats::default(),
        })
    }

    pub fn stats(&self) -> D3d11RendererStats {
        self.stats
    }

    fn retire_current_video(&mut self) -> Result<()> {
        // Preserve main's HWND playback policy. Only compositor-owned surfaces
        // need this additional handoff barrier. Do not grow the decoder pool or
        // accumulate an unbounded queue of AVFrames on delayed/failed fences.
        if self.current_video.is_some()
            && self
                .surface
                .as_ref()
                .is_some_and(|s| s.composition || s.flutter_texture)
        {
            if let Some(state) = self.state.as_ref() {
                wait_for_gpu(&state.device, &state.context)?;
            }
        }
        self.current_video = None;
        Ok(())
    }

    fn publish_flutter_texture(&mut self) -> Result<()> {
        let surface = self.surface.as_mut().expect("surface ensured");
        if !surface.flutter_texture {
            return Ok(());
        }
        let state = self.state.as_ref().expect("device ensured");
        let source = surface.output_texture.as_ref().expect("texture ensured");
        let extent = surface.metrics.physical_extent;
        let (snapshot, _) =
            create_flutter_texture_target(&state.device, extent.width, extent.height)?;
        unsafe {
            state.context.CopyResource(&snapshot, source);
        }
        // Flush only submits work. Complete the copy before another D3D device
        // opens this immutable snapshot. The extra GPU copy is texture-only.
        wait_for_gpu(&state.device, &state.context)?;
        surface.published_texture = Some(snapshot);
        Ok(())
    }

    fn ensure_default_device(&mut self) -> Result<()> {
        if self.state.is_some() {
            return Ok(());
        }
        let (device, context) = create_default_device()?;
        self.set_device(device, context)
    }

    fn ensure_device_for_texture(&mut self, texture: &ID3D11Texture2D) -> Result<()> {
        let frame_device = unsafe { texture.GetDevice() }
            .map_err(|error| d3d_error("ID3D11Texture2D::GetDevice", error))?;
        if self
            .state
            .as_ref()
            .is_some_and(|state| state.device.as_raw() == frame_device.as_raw())
        {
            return Ok(());
        }
        let context = unsafe { frame_device.GetImmediateContext() }
            .map_err(|error| d3d_error("ID3D11Device::GetImmediateContext", error))?;
        self.retire_current_video()?;
        self.danmaku_atlas_cache = None;
        self.set_device(frame_device, context)
    }

    /// Drops the renderer state when the GPU device is gone (TDR, driver
    /// reset, adapter change) so the next call rebuilds everything through
    /// `ensure_default_device` -> `set_device`, which already re-creates the
    /// device-bound pipeline, swapchain, and targets. Without this probe a
    /// removed device poisons every later call: errors repeat forever while
    /// the host believes playback is alive.
    fn drop_device_if_removed(&mut self) {
        let removed = self
            .state
            .as_ref()
            .is_some_and(|state| unsafe { state.device.GetDeviceRemovedReason() }.is_err());
        if removed {
            self.current_video = None;
            self.danmaku_atlas_cache = None;
            self.flutter_scene = None;
            self.state = None;
            eprintln!("erika d3d11: GPU device removed; renderer will rebuild on the next frame");
        }
    }

    fn set_device(&mut self, device: ID3D11Device, context: ID3D11DeviceContext) -> Result<()> {
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }
        let state = D3d11DeviceState::new(device, context)?;
        if let Err(error) = self
            .upscaler
            .attach_device(&state.device, self.upscaler_mode)
        {
            report_upscaler_failure("pipeline_build", self.upscaler_mode, &error);
        }
        // The existing swap chain belongs to the previous device; ResizeBuffers
        // cannot migrate it. Drop it so recreate_surface_targets creates a new
        // one — COM identity changes, which the host must rebind.
        if let Some(old) = self.state.as_ref() {
            if let Some(surface) = self.surface.as_mut() {
                release_backbuffer_views(&old.context, surface);
                surface.swapchain = None;
                surface.output_texture = None;
            }
        }
        // The cached LUT SRV belongs to the previous device, and D3D11 cannot
        // bind a resource across devices. Drop it even when the key is
        // unchanged: the CPU-side `gamut_lut_job` only holds parameters, so a
        // later frame regenerates the texture on the new device and reuses it.
        self.gamut_lut = None;
        self.state = Some(state);
        self.recreate_surface_targets()?;
        Ok(())
    }

    fn recreate_surface_targets(&mut self) -> Result<()> {
        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };
        let Some(state) = self.state.as_ref() else {
            return Ok(());
        };
        trace("recreate_surface_targets: reset views");
        release_backbuffer_views(&state.context, surface);
        let output_mode = surface.output_mode;
        let width = surface.metrics.physical_extent.width.max(1);
        let height = surface.metrics.physical_extent.height.max(1);
        let format = output_mode.swapchain_format();
        let hwnd = surface.hwnd;
        let composition = surface.composition;

        if surface.flutter_texture {
            surface.swapchain = None;
            let (texture, render_target) =
                create_flutter_texture_target(&state.device, width, height)?;
            surface.output_texture = Some(texture);
            surface.render_target = Some(render_target);
            surface.output_mode = D3d11OutputMode::Sdr;
            self.stats.surface_width = surface.metrics.physical_extent.width;
            self.stats.surface_height = surface.metrics.physical_extent.height;
            self.stats.hdr10_output_active = false;
            return Ok(());
        }
        surface.output_texture = None;

        // Size and SDR↔HDR format changes keep the same IDXGISwapChain1 so a
        // DirectComposition visual's SetContent stays valid. Device changes
        // drop the chain in set_device before reaching here.
        let reused = if let Some(swapchain) = surface.swapchain.as_ref() {
            trace("recreate_surface_targets: ResizeBuffers");
            unsafe {
                swapchain
                    .ResizeBuffers(0, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
                    .is_ok()
            }
        } else {
            false
        };
        if !reused {
            if surface.swapchain.is_some() {
                trace("recreate_surface_targets: ResizeBuffers failed, creating new swapchain");
            } else {
                trace("recreate_surface_targets: create_swapchain");
            }
            surface.swapchain = None;
            surface.swapchain = Some(create_swapchain(
                &state.device,
                hwnd,
                width,
                height,
                format,
                composition,
                composition && self.video_alpha_mode.has_alpha(),
            )?);
        }
        let swapchain = surface.swapchain.as_ref().expect("swapchain ensured");
        configure_swapchain_color_space(swapchain, output_mode)?;
        if matches!(output_mode, D3d11OutputMode::Sdr) {
            let _ = clear_hdr_metadata(swapchain);
        }
        trace("recreate_surface_targets: create_render_target");
        surface.render_target = Some(create_render_target(&state.device, swapchain)?);
        if matches!(output_mode, D3d11OutputMode::Hdr10) {
            trace("recreate_surface_targets: create_linear_render_target");
            let (render_target, shader_resource) =
                create_linear_render_target(&state.device, width, height)?;
            surface.linear_render_target = Some(render_target);
            surface.linear_shader_resource = Some(shader_resource);
        }
        self.stats.surface_width = surface.metrics.physical_extent.width;
        self.stats.surface_height = surface.metrics.physical_extent.height;
        self.stats.hdr10_output_active = matches!(output_mode, D3d11OutputMode::Hdr10);
        Ok(())
    }

    fn composition_swapchain(&self) -> Option<&IDXGISwapChain1> {
        let surface = self.surface.as_ref()?;
        if !surface.composition {
            return None;
        }
        surface.swapchain.as_ref()
    }

    fn set_output_mode(&mut self, output_mode: D3d11OutputMode) -> Result<()> {
        let Some(surface) = self.surface.as_mut() else {
            self.stats.hdr10_output_active = false;
            return Ok(());
        };
        let output_mode = if surface.flutter_texture {
            D3d11OutputMode::Sdr
        } else {
            output_mode
        };
        if surface.output_mode == output_mode && surface.targets_ready() {
            self.stats.hdr10_output_active = matches!(output_mode, D3d11OutputMode::Hdr10);
            return Ok(());
        }
        surface.output_mode = output_mode;
        self.retire_current_video()?;
        self.recreate_surface_targets()
    }

    fn select_output_mode_for_source(
        &mut self,
        source: SourceColorState,
    ) -> Result<D3d11OutputMode> {
        let source_is_hdr = source.is_hdr();
        if source_is_hdr {
            self.stats.hdr_source_frames += 1;
        }
        // DirectComposition transparency is defined on the SDR premultiplied
        // swap-chain path. Packed-alpha assets are effects/UI content rather
        // than an HDR presentation plane, so keep their color and alpha in one
        // stable BGRA8 composition space.
        if self.video_alpha_mode.has_alpha()
            || self
                .surface
                .as_ref()
                .is_some_and(|surface| surface.flutter_texture)
        {
            self.set_output_mode(D3d11OutputMode::Sdr)?;
            if source_is_hdr {
                self.stats.sdr_tonemap_frames += 1;
            }
            return Ok(D3d11OutputMode::Sdr);
        }
        if matches!(source.transfer, TransferFunction::Pq)
            && self.try_enable_hdr10_output(source)?
        {
            self.stats.hdr10_output_frames += 1;
            return Ok(D3d11OutputMode::Hdr10);
        }

        self.set_output_mode(D3d11OutputMode::Sdr)?;
        if source_is_hdr {
            self.stats.sdr_tonemap_frames += 1;
        }
        Ok(D3d11OutputMode::Sdr)
    }

    fn try_enable_hdr10_output(&mut self, source: SourceColorState) -> Result<bool> {
        if self.state.is_none() || self.surface.is_none() {
            return Ok(false);
        }
        if self.hdr10_output_unavailable {
            return Ok(false);
        }

        if let Err(error) = self.set_output_mode(D3d11OutputMode::Hdr10) {
            self.stats.hdr10_output_failures += 1;
            self.hdr10_output_unavailable = true;
            trace(&format!("hdr10 output unavailable: {error}"));
            self.set_output_mode(D3d11OutputMode::Sdr)?;
            return Ok(false);
        }
        if let Err(error) = self.update_hdr10_metadata(source) {
            self.stats.hdr10_metadata_failures += 1;
            self.hdr10_output_unavailable = true;
            trace(&format!("hdr10 metadata unavailable: {error}"));
            self.set_output_mode(D3d11OutputMode::Sdr)?;
            return Ok(false);
        }
        Ok(true)
    }

    fn update_hdr10_metadata(&mut self, source: SourceColorState) -> Result<()> {
        let swapchain = self
            .surface
            .as_ref()
            .and_then(|surface| surface.swapchain.as_ref())
            .ok_or_else(|| {
                PlayerError::Renderer("d3d11: no swapchain for HDR10 metadata".into())
            })?;
        set_hdr10_metadata(swapchain, dxgi_hdr10_metadata(source))?;
        self.stats.hdr10_metadata_updates += 1;
        Ok(())
    }

    fn import_d3d11va_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        let Some(texture_ref) = frame.frame.d3d11va_texture() else {
            return Err(PlayerError::Renderer(
                "d3d11: hardware frame is not a D3D11VA texture".to_string(),
            ));
        };
        let retained_frame = frame
            .frame
            .decoded_frame()
            .ok_or_else(|| {
                PlayerError::Renderer(
                    "d3d11: D3D11VA texture is missing its decoded AVFrame backing".to_string(),
                )
            })?
            .try_clone_ref()
            .map_err(|error| {
                PlayerError::Renderer(format!("d3d11: av_frame_ref failed: {error}"))
            })?;
        let source_texture = clone_d3d11_texture(texture_ref.raw_texture())?;
        let texture = self.import_texture_for_current_device(&source_texture)?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let texture_format = D3d11VideoTextureFormat::from_dxgi(desc.Format).ok_or_else(|| {
            PlayerError::Renderer(format!(
                "d3d11: unsupported D3D11VA texture format {:?}",
                desc.Format
            ))
        })?;
        let array_index = texture_ref.array_index();
        if array_index >= desc.ArraySize {
            return Err(PlayerError::Renderer(format!(
                "d3d11: D3D11VA array index {array_index} out of bounds for {} slices",
                desc.ArraySize
            )));
        }
        let source_color = source_color_for_frame(frame);
        let output_mode = self.select_output_mode_for_source(source_color)?;
        let target_color = output_mode.target_color_for_source(source_color);
        let visible_width = texture_ref.width().max(1).min(desc.Width.max(1));
        let visible_height = texture_ref.height().max(1).min(desc.Height.max(1));

        let state = self.state.as_ref().expect("device ensured");
        let luma = create_plane_srv(state, &texture, array_index, texture_format.luma_srv())
            .map_err(|error| d3d11va_srv_error(error, &desc, array_index))?;
        let chroma = create_plane_srv(state, &texture, array_index, texture_format.chroma_srv())
            .map_err(|error| d3d11va_srv_error(error, &desc, array_index))?;
        let frame_token = self.next_frame_token;
        self.next_frame_token = self.next_frame_token.wrapping_add(1);
        self.stats.hardware_video_frames += 1;
        self.stats.zero_copy_video_frames += 1;
        self.stats.direct_zero_copy_video_frames += 1;
        let imported = ImportedVideoFrame {
            _frame: Some(retained_frame),
            _texture: texture,
            _chroma_texture: None,
            luma,
            chroma,
            width: visible_width,
            height: visible_height,
            tex_rect: D3d11TexRect::visible_region(
                visible_width,
                visible_height,
                desc.Width,
                desc.Height,
            ),
            _array_index: array_index,
            frame_token,
            constants: constants_for_frame(source_color, texture_format, target_color)
                .packed_alpha_right(self.video_alpha_mode.has_alpha()),
        };
        self.retire_current_video()?;
        self.current_video = Some(imported);
        Ok(())
    }

    fn upload_software_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        let planar = frame
            .frame
            .decoded_frame()
            .and_then(Frame::to_planar_frame)
            .ok_or_else(|| {
                PlayerError::Renderer(format!(
                    "d3d11: unsupported software video frame format {:?}",
                    frame.frame.pixel_format()
                ))
            })?;
        self.upload_planar_frame(planar, source_color_for_frame(frame))
    }

    fn upload_planar_frame(
        &mut self,
        planar: PlanarFrame,
        source_color: SourceColorState,
    ) -> Result<()> {
        self.ensure_default_device()?;
        let output_mode = self.select_output_mode_for_source(source_color)?;
        let target_color = output_mode.target_color_for_source(source_color);
        let (texture_format, bytes_per_sample) = match planar.format {
            PlanarPixelFormat::Nv12 => (D3d11VideoTextureFormat::Nv12, 1),
            PlanarPixelFormat::P010 => (D3d11VideoTextureFormat::P010, 2),
        };
        let state = self.state.as_ref().expect("device ensured");
        // Separate planes support odd visible dimensions and retain 10-bit HDR
        // samples without requiring hardware video-format texture support.
        let luma_pitch = planar
            .width
            .checked_mul(bytes_per_sample)
            .ok_or_else(|| PlayerError::Renderer("d3d11: luma row pitch overflow".into()))?;
        let chroma_pitch = planar
            .width
            .div_ceil(2)
            .checked_mul(2 * bytes_per_sample)
            .ok_or_else(|| PlayerError::Renderer("d3d11: chroma row pitch overflow".into()))?;
        let luma = create_cpu_texture(
            state,
            planar.width,
            planar.height,
            texture_format.luma_srv(),
            &planar.luma,
            luma_pitch,
        )?;
        let chroma = create_cpu_texture(
            state,
            planar.width.div_ceil(2),
            planar.height.div_ceil(2),
            texture_format.chroma_srv(),
            &planar.chroma,
            chroma_pitch,
        )?;
        let frame_token = self.next_frame_token;
        self.retire_current_video()?;
        self.current_video = Some(ImportedVideoFrame {
            _frame: None,
            _texture: luma._texture,
            _chroma_texture: Some(chroma._texture),
            luma: luma.view,
            // Each SRV retains its underlying texture after this upload returns.
            chroma: chroma.view,
            width: planar.width,
            height: planar.height,
            tex_rect: D3d11TexRect::FULL,
            _array_index: 0,
            frame_token,
            constants: constants_for_frame(source_color, texture_format, target_color)
                .packed_alpha_right(self.video_alpha_mode.has_alpha()),
        });
        self.next_frame_token = self.next_frame_token.wrapping_add(1);
        self.stats.software_video_frames += 1;
        self.stats.cpu_video_frame_fallbacks += 1;
        Ok(())
    }

    fn import_texture_for_current_device(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11Texture2D> {
        if let Some(state) = self.state.as_ref() {
            let frame_device = unsafe { texture.GetDevice() }
                .map_err(|error| d3d_error("ID3D11Texture2D::GetDevice", error))?;
            if state.device.as_raw() == frame_device.as_raw() {
                return Ok(texture.clone());
            }
            trace(
                "switching renderer to the decoder D3D11 device to avoid unsynchronized \
                 cross-device surface reuse",
            );
        }

        self.ensure_device_for_texture(texture)?;
        Ok(texture.clone())
    }

    /// Target color state overlays are composited into: the attached surface's
    /// swapchain encoding (PQ for HDR10, sRGB otherwise).
    fn overlay_target_color(&self) -> TargetColorState {
        self.surface
            .as_ref()
            .map(|surface| surface.output_mode)
            .unwrap_or_default()
            .target_color()
    }

    fn prepare_overlay_draws(
        &mut self,
        frame: Option<&OverlayFrame>,
    ) -> Result<Vec<D3d11OverlayDraw>> {
        let Some(frame) = frame else {
            return Ok(Vec::new());
        };
        if frame.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_default_device()?;
        self.stats.prepared_overlay_frames += 1;
        self.stats.prepared_overlay_subtitle_planes += frame
            .subtitle_planes
            .len()
            .saturating_add(frame.subtitle_alpha_planes.len())
            as u64;
        let viewport_w = frame.viewport.width;
        let viewport_h = frame.viewport.height;
        let target = self.overlay_target_color();
        let mut draws = Vec::new();

        for plane in &frame.subtitle_planes {
            if plane.width == 0 || plane.height == 0 {
                continue;
            }
            let expected = plane.width as usize * plane.height as usize * 4;
            if plane.rgba.len() != expected {
                return Err(PlayerError::Renderer(format!(
                    "d3d11: overlay subtitle plane has {} bytes, expected {expected} for {}x{} RGBA",
                    plane.rgba.len(),
                    plane.width,
                    plane.height
                )));
            }
            let texture = {
                let state = self.state.as_ref().expect("device ensured");
                create_cpu_texture(
                    state,
                    plane.width,
                    plane.height,
                    DXGI_FORMAT_R8G8B8A8_UNORM,
                    &plane.rgba,
                    plane.width * 4,
                )?
            };
            let (x, y, width, height) = plane.scaled_rect(viewport_w, viewport_h);
            draws.push(D3d11OverlayDraw {
                texture,
                constants: OverlayUniforms::rgba_plane(
                    x, y, width, height, viewport_w, viewport_h, target,
                ),
            });
        }

        self.append_alpha_atlas_draws(frame, viewport_w, viewport_h, target, &mut draws)?;
        Ok(draws)
    }

    fn prepare_danmaku_draws(
        &mut self,
        plan: Option<&DanmakuRenderPlan>,
    ) -> Result<Vec<D3d11OverlayDraw>> {
        let Some(plan) = plan else {
            return Ok(Vec::new());
        };
        if plan.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_default_device()?;
        let Some(atlas) = plan.atlas.as_ref() else {
            return Ok(Vec::new());
        };
        if !atlas.is_valid() {
            return Err(PlayerError::Renderer(format!(
                "d3d11: danmaku glyph atlas has fill={} outline={} bytes, expected at least {} for {}x{} stride {}",
                atlas.fill_alpha.len(),
                atlas.outline_alpha.len(),
                atlas.required_len(),
                atlas.width,
                atlas.height,
                atlas.stride
            )));
        }
        let viewport_w = plan.viewport.width;
        let viewport_h = plan.viewport.height;
        let target = self.overlay_target_color();
        let mut draws = Vec::with_capacity(plan.items.len() * 3);
        let (fill, outline) = self.prepare_danmaku_atlas_textures(atlas)?;
        for item in &plan.items {
            self.append_danmaku_glyph_draws(
                item, &fill, &outline, viewport_w, viewport_h, target, &mut draws,
            );
        }
        Ok(draws)
    }

    fn prepare_danmaku_atlas_textures(
        &mut self,
        atlas: &DanmakuGlyphAtlas,
    ) -> Result<(D3d11OverlayTexture, D3d11OverlayTexture)> {
        if let Some(cache) = &self.danmaku_atlas_cache {
            if cache.can_reuse_for(atlas) {
                self.stats.overlay_alpha_atlas_reuses += 1;
                return Ok((cache.fill.clone(), cache.outline.clone()));
            }
        }
        let incremental = self.danmaku_atlas_cache.as_ref().and_then(|cache| {
            atlas
                .incremental_update_from(cache.version, cache.width, cache.height, cache.stride)
                .map(|update| (cache.fill.clone(), cache.outline.clone(), update.clone()))
        });
        if let Some((fill, outline, update)) = incremental {
            self.update_danmaku_atlas_texture(&fill, atlas, &atlas.fill_alpha, &update);
            self.update_danmaku_atlas_texture(&outline, atlas, &atlas.outline_alpha, &update);
            if let Some(cache) = &mut self.danmaku_atlas_cache {
                cache.version = atlas.version;
            }
            self.stats.overlay_alpha_atlas_uploads += 1;
            self.stats.overlay_alpha_atlas_reuses += 1;
            return Ok((fill, outline));
        }

        let (fill, outline) = {
            let state = self.state.as_ref().expect("device ensured");
            (
                create_cpu_texture(
                    state,
                    atlas.width,
                    atlas.height,
                    DXGI_FORMAT_R8_UNORM,
                    &atlas.fill_alpha,
                    atlas.stride as u32,
                )?,
                create_cpu_texture(
                    state,
                    atlas.width,
                    atlas.height,
                    DXGI_FORMAT_R8_UNORM,
                    &atlas.outline_alpha,
                    atlas.stride as u32,
                )?,
            )
        };
        self.stats.overlay_alpha_atlas_uploads += 1;
        self.danmaku_atlas_cache = Some(D3d11DanmakuAtlasCache {
            version: atlas.version,
            width: atlas.width,
            height: atlas.height,
            stride: atlas.stride,
            fill: fill.clone(),
            outline: outline.clone(),
        });
        Ok((fill, outline))
    }

    fn update_danmaku_atlas_texture(
        &self,
        texture: &D3d11OverlayTexture,
        atlas: &DanmakuGlyphAtlas,
        pixels: &[u8],
        update: &DanmakuAtlasUpdate,
    ) {
        let offset = update.y as usize * atlas.stride + update.x as usize;
        let update_box = D3D11_BOX {
            left: update.x,
            top: update.y,
            front: 0,
            right: update.x.saturating_add(update.width),
            bottom: update.y.saturating_add(update.height),
            back: 1,
        };
        unsafe {
            self.state
                .as_ref()
                .expect("device ensured")
                .context
                .UpdateSubresource(
                    &texture._texture,
                    0,
                    Some(&update_box as *const D3D11_BOX),
                    pixels[offset..].as_ptr().cast::<c_void>(),
                    atlas.stride as u32,
                    0,
                );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_danmaku_glyph_draws(
        &self,
        item: &DanmakuGlyphInstance,
        fill_texture: &D3d11OverlayTexture,
        outline_texture: &D3d11OverlayTexture,
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
        draws: &mut Vec<D3d11OverlayDraw>,
    ) {
        if item.shadow_rgba[3] > 0.0 {
            let mut rect = item.rect;
            rect[0] += item.shadow_offset[0];
            rect[1] += item.shadow_offset[1];
            draws.push(D3d11OverlayDraw {
                texture: outline_texture.clone(),
                constants: OverlayUniforms::alpha_atlas_rect(
                    item.shadow_rgba,
                    rect,
                    item.tex_rect,
                    viewport_w,
                    viewport_h,
                    target,
                ),
            });
        }
        if item.outline_rgba[3] > 0.0 {
            draws.push(D3d11OverlayDraw {
                texture: outline_texture.clone(),
                constants: OverlayUniforms::alpha_atlas_rect(
                    item.outline_rgba,
                    item.rect,
                    item.tex_rect,
                    viewport_w,
                    viewport_h,
                    target,
                ),
            });
        }
        draws.push(D3d11OverlayDraw {
            texture: fill_texture.clone(),
            constants: OverlayUniforms::alpha_atlas_rect(
                item.color_rgba,
                item.rect,
                item.tex_rect,
                viewport_w,
                viewport_h,
                target,
            ),
        });
    }

    fn append_alpha_atlas_draws(
        &mut self,
        frame: &OverlayFrame,
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
        draws: &mut Vec<D3d11OverlayDraw>,
    ) -> Result<()> {
        let bitmaps = &frame.subtitle_alpha_planes;
        let mut atlas_width = 0usize;
        let mut atlas_height = 0usize;
        for bitmap in bitmaps {
            if bitmap.placement.width == 0 || bitmap.placement.height == 0 {
                continue;
            }
            atlas_width += bitmap.placement.width as usize;
            atlas_height = atlas_height.max(bitmap.placement.height as usize);
        }
        if atlas_width == 0 || atlas_height == 0 {
            return Ok(());
        }

        let mut pixels = vec![0u8; atlas_width * atlas_height];
        let mut cursor_x = 0usize;
        let mut placements = Vec::new();
        for (index, bitmap) in bitmaps.iter().enumerate() {
            let bw = bitmap.placement.width as usize;
            let bh = bitmap.placement.height as usize;
            if bw == 0 || bh == 0 {
                continue;
            }
            if !bitmap.is_valid() {
                return Err(PlayerError::Renderer(format!(
                    "d3d11: overlay alpha bitmap has {} bytes, expected at least {} for {}x{} stride {}",
                    bitmap.alpha.len(),
                    bitmap.required_len(),
                    bitmap.placement.width,
                    bitmap.placement.height,
                    bitmap.stride
                )));
            }
            for row in 0..bh {
                let src = row * bitmap.stride;
                let dst = row * atlas_width + cursor_x;
                pixels[dst..dst + bw].copy_from_slice(&bitmap.alpha[src..src + bw]);
            }
            placements.push((index, cursor_x));
            cursor_x += bw;
        }

        let texture = {
            let state = self.state.as_ref().expect("device ensured");
            create_cpu_texture(
                state,
                atlas_width as u32,
                atlas_height as u32,
                DXGI_FORMAT_R8_UNORM,
                &pixels,
                atlas_width as u32,
            )?
        };
        self.stats.overlay_alpha_atlas_uploads += 1;
        for (index, atlas_x) in placements {
            let bitmap = &bitmaps[index];
            draws.push(D3d11OverlayDraw {
                texture: texture.clone(),
                constants: OverlayUniforms::alpha_atlas(
                    bitmap.color_rgba,
                    bitmap.placement.x,
                    bitmap.placement.y,
                    bitmap.placement.width,
                    bitmap.placement.height,
                    atlas_x as u32,
                    atlas_width as u32,
                    atlas_height as u32,
                    viewport_w,
                    viewport_h,
                    target,
                ),
            });
        }
        Ok(())
    }

    /// Return a cached (or freshly generated) perceptual gamut LUT SRV for
    /// the given uniforms, or `None` while the fast path is active or the
    /// background generation is still pending. Callers must then mask
    /// `gamut_lut_enabled` off so the shader keeps the fast path.
    fn gamut_lut_view(
        &mut self,
        uniforms: &VideoUniforms,
    ) -> Result<Option<ID3D11ShaderResourceView>> {
        if uniforms.gamut_lut_enabled == 0 {
            return Ok(None);
        }
        let packed = uniforms._gamut_primaries;
        let source = packed >> 8;
        let target = packed & 0xff;
        // The LUT's I axis spans [target black, target peak] in PQ codes;
        // either endpoint changing invalidates the cached LUT.
        let black_pq = d3d_quantize_luma_pq(uniforms.tone_map_extra[2]);
        let peak_pq = d3d_quantize_luma_pq(uniforms.nits[1]);
        if let Some((s, t, b, p, srv)) = &self.gamut_lut {
            if *s == source && *t == target && *b == black_pq && *p == peak_pq {
                return Ok(Some(srv.clone()));
            }
        }
        let params = GamutLutParams {
            source: d3d_code_to_primaries(source),
            target: d3d_code_to_primaries(target),
            // Same target black the shader derives from tone_map_extra.z.
            min_luma: d3d_pq_code_for_lut(uniforms.tone_map_extra[2]),
            max_luma: d3d_pq_code_for_lut(uniforms.nits[1]),
        };
        let job_params = self
            .gamut_lut_job
            .as_ref()
            .map(GamutLutJob::params)
            .filter(|job_params| *job_params == params);
        if job_params.is_none() {
            // First request (or the key changed): spawn generation and keep
            // the fast path for this frame.
            self.gamut_lut_job = Some(GamutLutJob::spawn(params));
            return Ok(None);
        }
        let Some(lut) = self.gamut_lut_job.as_ref().and_then(GamutLutJob::poll) else {
            return Ok(None);
        };
        self.gamut_lut_job = None;
        let state = self.state.as_ref().expect("device ensured for gamut lut");
        // Pack (I, P+0.5, T+0.5) into RGBA16F bytes laid out I x C x H.
        let texels = pack_rgba16f(&lut.texels, 1.0);
        let initial = [D3D11_SUBRESOURCE_DATA {
            pSysMem: texels.as_ptr() as *const c_void,
            SysMemPitch: (LUT_SIZE_I * 4 * 2) as u32,
            SysMemSlicePitch: (LUT_SIZE_I * LUT_SIZE_C * 4 * 2) as u32,
        }];
        let desc = D3D11_TEXTURE3D_DESC {
            Width: LUT_SIZE_I as u32,
            Height: LUT_SIZE_C as u32,
            Depth: LUT_SIZE_H as u32,
            MipLevels: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe {
            state
                .device
                .CreateTexture3D(&desc, Some(initial.as_ptr()), Some(&mut texture))
                .map_err(|error| d3d_error("ID3D11Device::CreateTexture3D(gamut lut)", error))?;
        }
        let resource = texture
            .expect("gamut lut texture created")
            .cast::<ID3D11Resource>()
            .map_err(|error| d3d_error("cast to ID3D11Resource", error))?;
        let mut srv = None;
        unsafe {
            state
                .device
                .CreateShaderResourceView(&resource, None, Some(&mut srv))
                .map_err(|error| {
                    d3d_error("ID3D11Device::CreateShaderResourceView(gamut lut)", error)
                })?;
        }
        let srv = srv.expect("gamut lut srv created");
        self.gamut_lut = Some((source, target, black_pq, peak_pq, srv.clone()));
        Ok(Some(srv))
    }

    fn render_video(&mut self, context: RenderFrameContext<'_>) -> Result<bool> {
        if self.current_video.is_none() {
            return Ok(false);
        }
        self.ensure_default_device()?;
        self.ensure_surface_ready()?;
        let flutter_texture = self
            .surface
            .as_ref()
            .expect("surface ensured")
            .flutter_texture;
        let frame_token = self
            .current_video
            .as_ref()
            .expect("video checked")
            .frame_token;
        if flutter_texture
            && self.surface.as_ref().unwrap().published_texture.is_some()
            && self
                .flutter_scene
                .as_ref()
                .is_some_and(|scene| scene.matches(frame_token, self.upscaler_mode, context))
        {
            return Ok(true);
        }
        let overlay_draws = self.prepare_overlay_draws(context.overlay)?;
        let danmaku_draws = self.prepare_danmaku_draws(context.danmaku)?;
        let (video_width, video_height, frame_token, native_luma) = {
            let video = self.current_video.as_ref().expect("video checked");
            (
                video.width,
                video.height,
                video.frame_token,
                video.luma.clone(),
            )
        };
        let physical = self
            .surface
            .as_ref()
            .expect("surface ensured")
            .metrics
            .physical_extent;
        let logical_video_width = self.video_alpha_mode.logical_width(video_width);
        let target_rect = aspect_fit_rect(
            logical_video_width,
            video_height,
            physical.width,
            physical.height,
        );
        let upscale_requested = !self.video_alpha_mode.has_alpha()
            && self.upscaler_mode.is_enabled()
            && self.upscaler.status() == LumaUpscalerBackendStatus::Scalar
            && target_rect.width > video_width as f32;
        let upscaled_luma = if upscale_requested {
            let (device, device_context) = {
                let state = self.state.as_ref().expect("device ensured");
                (state.device.clone(), state.context.clone())
            };
            match self.upscaler.encode(
                &device,
                &device_context,
                &native_luma,
                video_width,
                video_height,
                Some(frame_token),
            ) {
                Ok(output) => output,
                Err(error) => {
                    self.upscaler.record_runtime_failure(&error);
                    report_upscaler_failure("frame_encode", self.upscaler_mode, &error);
                    None
                }
            }
        } else {
            None
        };
        let mut video_constants = {
            let video = self.current_video.as_ref().expect("video checked");
            video.constants
        };
        let gamut_lut = self.gamut_lut_view(&video_constants)?;
        if gamut_lut.is_none() {
            // Background generation pending: keep the fast gamut_compress
            // path until the LUT lands.
            video_constants.gamut_lut_enabled = 0;
        }
        let video = self.current_video.as_ref().expect("video checked");
        let state = self.state.as_ref().expect("device ensured");
        let surface = self.surface.as_ref().expect("surface ensured");
        let rtv = surface
            .render_target
            .as_ref()
            .ok_or_else(|| PlayerError::Renderer("d3d11: no render target attached".to_string()))?;
        let linear_target = if matches!(surface.output_mode, D3d11OutputMode::Hdr10) {
            Some((
                surface.linear_render_target.as_ref().ok_or_else(|| {
                    PlayerError::Renderer("d3d11: no linear render target attached".to_string())
                })?,
                surface.linear_shader_resource.as_ref().ok_or_else(|| {
                    PlayerError::Renderer("d3d11: no linear shader resource attached".to_string())
                })?,
            ))
        } else {
            None
        };
        let scene_rtv = linear_target.map_or(rtv, |(linear_rtv, _)| linear_rtv);
        unsafe {
            state.context.ClearRenderTargetView(
                scene_rtv,
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
        state.draw_video(
            video,
            video_constants,
            upscaled_luma.as_ref(),
            gamut_lut.as_ref(),
            scene_rtv,
            target_rect,
        )?;
        if !overlay_draws.is_empty() {
            // Subtitle coordinates are produced in the video-frame viewport.
            // Composite them through the same aspect-fit viewport as the video
            // so resizing the window cannot stretch the subtitle independently.
            state.draw_overlays(&overlay_draws, scene_rtv, target_rect)?;
        }
        if !danmaku_draws.is_empty() {
            state.draw_overlays(
                &danmaku_draws,
                scene_rtv,
                D3d11DrawRect {
                    x: 0.0,
                    y: 0.0,
                    width: physical.width.max(1) as f32,
                    height: physical.height.max(1) as f32,
                },
            )?;
        }
        if let Some((_, linear_srv)) = linear_target {
            state.draw_encode(
                linear_srv,
                rtv,
                physical.width,
                physical.height,
                video.constants,
            )?;
        }
        if let Some(swapchain) = surface.swapchain.as_ref() {
            present_swapchain(swapchain, "IDXGISwapChain1::Present1")?;
        }
        if flutter_texture {
            self.publish_flutter_texture()?;
            if let Some(scene) = self.flutter_scene.as_mut() {
                scene.update(frame_token, self.upscaler_mode, context);
            } else {
                self.flutter_scene = Some(FlutterTextureScene::new(
                    frame_token,
                    self.upscaler_mode,
                    context,
                ));
            }
        }
        self.stats.rendered_frames += 1;
        if !danmaku_draws.is_empty() {
            self.stats.danmaku_passes += 1;
            self.stats.danmaku_items += danmaku_draws.len() as u64;
        }
        Ok(true)
    }

    fn ensure_surface_ready(&mut self) -> Result<()> {
        if self.surface.is_none() {
            return Err(PlayerError::Renderer(
                "d3d11: no output surface attached".to_string(),
            ));
        }
        if self
            .surface
            .as_ref()
            .is_some_and(|surface| !surface.targets_ready())
        {
            self.recreate_surface_targets()?;
        }
        Ok(())
    }

    fn render_clear(&mut self, time_seconds: f64) -> Result<()> {
        trace("render_clear: ensure_default_device");
        self.ensure_default_device()?;
        trace("render_clear: ensure_surface_ready");
        self.ensure_surface_ready()?;
        if self
            .surface
            .as_ref()
            .is_some_and(|surface| surface.flutter_texture && surface.published_texture.is_some())
            && self.flutter_scene.is_none()
        {
            return Ok(());
        }
        let state = self.state.as_ref().expect("device ensured");
        let surface = self.surface.as_ref().expect("surface ensured");
        let rtv = surface
            .render_target
            .as_ref()
            .ok_or_else(|| PlayerError::Renderer("d3d11: no render target attached".to_string()))?;
        let _ = time_seconds;
        let color = [
            0.0,
            0.0,
            0.0,
            if self.video_alpha_mode.has_alpha() {
                0.0
            } else {
                1.0
            },
        ];
        unsafe {
            trace("render_clear: clear");
            state.context.ClearRenderTargetView(rtv, &color);
            trace("render_clear: present");
            if let Some(swapchain) = surface.swapchain.as_ref() {
                present_swapchain(swapchain, "IDXGISwapChain1::Present1(clear)")?;
            }
        }
        self.publish_flutter_texture()?;
        self.flutter_scene = None;
        trace("render_clear: done");
        self.stats.rendered_frames += 1;
        Ok(())
    }
}

impl RendererBackend for D3d11Renderer {
    fn attach_surface(&mut self, surface: PlatformSurface) -> Result<()> {
        let (hwnd, composition, flutter_texture, metrics) = match surface {
            PlatformSurface::Wgpu(handle) => {
                if handle.kind != WgpuSurfaceKind::WindowsHwnd {
                    return Err(PlayerError::Renderer(format!(
                        "d3d11: surface kind {:?} is not supported",
                        handle.kind
                    )));
                }
                let composition = handle.output_capabilities.direct_composition;
                if !composition && handle.raw_window == 0 {
                    return Err(PlayerError::Renderer(
                        "d3d11: Windows HWND surface handle is null".to_string(),
                    ));
                }
                (
                    HWND(handle.raw_window as *mut c_void),
                    composition,
                    false,
                    handle.metrics,
                )
            }
            PlatformSurface::FlutterTexture(handle)
                if handle.kind == FlutterTextureKind::WindowsTextureRegistrar =>
            {
                (HWND::default(), false, true, handle.metrics)
            }
            PlatformSurface::FlutterTexture(handle) => {
                return Err(PlayerError::Renderer(format!(
                    "d3d11: Flutter texture kind {:?} is not supported",
                    handle.kind
                )));
            }
            PlatformSurface::Metal(_) => {
                return Err(PlayerError::Renderer(
                    "d3d11: Metal surfaces are not supported".to_string(),
                ));
            }
        };
        self.surface = Some(AttachedSurface {
            hwnd,
            composition,
            flutter_texture,
            metrics,
            output_mode: D3d11OutputMode::Sdr,
            swapchain: None,
            output_texture: None,
            published_texture: None,
            render_target: None,
            linear_render_target: None,
            linear_shader_resource: None,
        });
        self.hdr10_output_unavailable = false;
        self.stats.attached = true;
        self.recreate_surface_targets()?;
        self.render_clear(0.0)
    }

    fn detach_surface(&mut self) -> Result<()> {
        self.retire_current_video()?;
        self.surface = None;
        self.danmaku_atlas_cache = None;
        self.stats.attached = false;
        self.stats.surface_width = 0;
        self.stats.surface_height = 0;
        Ok(())
    }

    fn resize_surface(&mut self, metrics: SurfaceMetrics) -> Result<()> {
        let Some(surface) = self.surface.as_mut() else {
            return Err(PlayerError::Renderer(
                "d3d11: no output surface attached".to_string(),
            ));
        };
        if surface.metrics.physical_extent == metrics.physical_extent {
            surface.metrics = metrics;
            return Ok(());
        }
        surface.metrics = metrics;
        self.hdr10_output_unavailable = false;
        let flutter_texture = surface.flutter_texture;
        self.recreate_surface_targets()?;
        if flutter_texture && self.current_video.is_none() {
            self.render_clear(0.0)?;
        }
        Ok(())
    }

    fn render_test_frame(&mut self, time_seconds: f64) -> Result<()> {
        // PQ code value zero is black, so the current clear-only test frame
        // can safely bypass the scene-linear intermediate target.
        self.render_clear(time_seconds)
    }

    fn upload_player_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        self.drop_device_if_removed();
        if frame.frame.d3d11va_texture().is_some() {
            let result = self.import_d3d11va_frame(frame);
            if result.is_err() {
                self.stats.import_failures += 1;
            }
            return result;
        }
        if frame.frame.has_hw_frames_context() {
            self.stats.hardware_video_frames += 1;
            return Err(PlayerError::Renderer(
                "d3d11: hardware frame is not importable as D3D11VA".to_string(),
            ));
        }
        self.upload_software_frame(frame)
    }

    fn clear_current_frame(&mut self) -> Result<()> {
        self.retire_current_video()?;
        self.danmaku_atlas_cache = None;
        if self.surface.is_some() {
            self.render_clear(0.0)?;
        }
        Ok(())
    }

    fn preserve_current_frame_for_transition(&mut self) -> Result<()> {
        Ok(())
    }

    fn render_current_frame(&mut self, context: RenderFrameContext<'_>) -> Result<bool> {
        self.drop_device_if_removed();
        self.render_video(context)
    }

    fn runtime_stats(&self) -> RendererRuntimeStats {
        RendererRuntimeStats {
            surface_width: self.stats.surface_width,
            surface_height: self.stats.surface_height,
            rendered_frames: self.stats.rendered_frames,
            attached: self.stats.attached,
            prepared_overlay_frames: self.stats.prepared_overlay_frames,
            prepared_overlay_subtitle_planes: self.stats.prepared_overlay_subtitle_planes,
            danmaku_passes: self.stats.danmaku_passes,
            danmaku_draw_items: self.stats.danmaku_items,
            overlay_alpha_atlas_uploads: self.stats.overlay_alpha_atlas_uploads,
            overlay_alpha_atlas_reuses: self.stats.overlay_alpha_atlas_reuses,
            software_video_frames: self.stats.software_video_frames,
            hardware_video_frames: self.stats.hardware_video_frames,
            zero_copy_video_frames: self.stats.zero_copy_video_frames,
            direct_zero_copy_video_frames: self.stats.direct_zero_copy_video_frames,
            shared_handle_video_frames: self.stats.shared_handle_video_frames,
            cpu_video_frame_fallbacks: self.stats.cpu_video_frame_fallbacks,
            hdr_source_frames: self.stats.hdr_source_frames,
            hdr10_output_frames: self.stats.hdr10_output_frames,
            sdr_tonemap_frames: self.stats.sdr_tonemap_frames,
            hdr10_metadata_updates: self.stats.hdr10_metadata_updates,
            hdr10_metadata_failures: self.stats.hdr10_metadata_failures,
            hdr10_output_failures: self.stats.hdr10_output_failures,
            hdr10_output_active: self.stats.hdr10_output_active,
            upscaler_mode: self.upscaler_mode,
            upscaler_backend: if self.upscaler_mode.is_enabled() && self.state.is_none() {
                LumaUpscalerBackendStatus::Building
            } else {
                self.upscaler.status()
            },
            upscaler_fallbacks: self.upscaler.fallback_count(),
            upscaled_frames: self.upscaler.upscaled_frames(),
            last_upscaler_encode_duration: self.upscaler.last_encode_duration(),
            ..Default::default()
        }
    }

    fn output_status(&self) -> OutputRuntimeStatus {
        let hdr10_active = self.stats.hdr10_output_active;
        OutputRuntimeStatus {
            requested_mode: self.requested_output_mode,
            active_encoding: if hdr10_active {
                ActiveOutputEncoding::Hdr10Pq
            } else {
                ActiveOutputEncoding::SdrSrgb
            },
            surface_format: if hdr10_active {
                OutputSurfaceFormat::TenBitUnorm
            } else {
                OutputSurfaceFormat::EightBitUnorm
            },
            native_data_space: -1,
            requested_headroom: self.requested_output_mode.headroom(),
            active_headroom: if hdr10_active { 10_000.0 / 203.0 } else { 1.0 },
            active_headroom_known: self.stats.attached,
            extended_linear_active: false,
            fallback_reason: OutputFallbackReason::None,
            fallback_count: self.stats.hdr10_output_failures,
            data_space_failures: self.stats.hdr10_output_failures,
            headroom_updates: self.stats.hdr10_metadata_updates,
            extended_linear_frames: 0,
        }
    }

    fn set_luma_upscaler(&mut self, mode: LumaUpscalerMode) {
        self.upscaler_mode = mode;
        if let Some(state) = self.state.as_ref() {
            if let Err(error) = self.upscaler.set_mode(&state.device, mode) {
                report_upscaler_failure("mode_switch", mode, &error);
            }
        }
    }

    fn composition_swapchain_ptr(&self) -> Option<*mut std::ffi::c_void> {
        Some(Interface::as_raw(self.composition_swapchain()?))
    }

    fn composition_swapchain_iunknown(&self) -> Option<*mut std::ffi::c_void> {
        let unknown: IUnknown = self.composition_swapchain()?.cast().ok()?;
        Some(unknown.into_raw())
    }

    fn windows_flutter_texture_iunknown(&self) -> Option<*mut std::ffi::c_void> {
        let surface = self.surface.as_ref()?;
        if !surface.flutter_texture {
            return None;
        }
        let unknown: IUnknown = surface.published_texture.as_ref()?.cast().ok()?;
        Some(unknown.into_raw())
    }
}

impl D3d11DeviceState {
    fn new(device: ID3D11Device, context: ID3D11DeviceContext) -> Result<Self> {
        let vertex_blob = compile_shader("vs_main", "vs_4_0")?;
        let pixel_blob = compile_shader("ps_main", "ps_4_0")?;
        let encode_pixel_blob = compile_shader("encode_ps_main", "ps_4_0")?;
        let overlay_vertex_blob =
            compile_shader_source(OVERLAY_SHADER_SOURCE, "overlay_vs_main", "vs_4_0")?;
        let overlay_pixel_blob =
            compile_shader_source(OVERLAY_SHADER_SOURCE, "overlay_ps_main", "ps_4_0")?;
        let vertex_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreateVertexShader(blob_bytes(&vertex_blob), None, Some(&mut shader))
                    .map_err(|error| d3d_error("ID3D11Device::CreateVertexShader", error))?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer("d3d11: CreateVertexShader returned null".to_string())
            })?
        };
        let pixel_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreatePixelShader(blob_bytes(&pixel_blob), None, Some(&mut shader))
                    .map_err(|error| d3d_error("ID3D11Device::CreatePixelShader", error))?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer("d3d11: CreatePixelShader returned null".to_string())
            })?
        };
        let encode_pixel_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreatePixelShader(blob_bytes(&encode_pixel_blob), None, Some(&mut shader))
                    .map_err(|error| d3d_error("ID3D11Device::CreatePixelShader(encode)", error))?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer("d3d11: CreatePixelShader(encode) returned null".to_string())
            })?
        };
        let overlay_vertex_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreateVertexShader(blob_bytes(&overlay_vertex_blob), None, Some(&mut shader))
                    .map_err(|error| {
                        d3d_error("ID3D11Device::CreateVertexShader(overlay)", error)
                    })?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer(
                    "d3d11: CreateVertexShader(overlay) returned null".to_string(),
                )
            })?
        };
        let overlay_pixel_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreatePixelShader(blob_bytes(&overlay_pixel_blob), None, Some(&mut shader))
                    .map_err(|error| {
                        d3d_error("ID3D11Device::CreatePixelShader(overlay)", error)
                    })?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer("d3d11: CreatePixelShader(overlay) returned null".to_string())
            })?
        };
        let input_layout = create_input_layout(&device, &vertex_blob)?;
        let vertex_buffer = create_vertex_buffer(&device)?;
        let constants = create_constants_buffer(&device)?;
        let sampler = create_sampler(&device)?;
        let overlay_constants = create_overlay_constants_buffer(&device)?;
        let overlay_sampler = create_sampler(&device)?;
        let overlay_blend = create_overlay_blend_state(&device)?;
        Ok(Self {
            device,
            context,
            vertex_shader,
            pixel_shader,
            encode_pixel_shader,
            input_layout,
            vertex_buffer,
            constants,
            sampler,
            overlay_vertex_shader,
            overlay_pixel_shader,
            overlay_constants,
            overlay_sampler,
            overlay_blend,
        })
    }

    /// Draw the imported video frame. `constants` is the per-frame uniform
    /// payload the caller already resolved (including any
    /// `gamut_lut_enabled` mask for a still-generating LUT); the stored
    /// `ImportedVideoFrame.constants` is left untouched so a later present
    /// can re-enable the LUT once it lands.
    fn draw_video(
        &self,
        video: &ImportedVideoFrame,
        constants: VideoUniforms,
        upscaled_luma: Option<&ID3D11ShaderResourceView>,
        gamut_lut: Option<&ID3D11ShaderResourceView>,
        render_target: &ID3D11RenderTargetView,
        target: D3d11DrawRect,
    ) -> Result<()> {
        let viewport = D3D11_VIEWPORT {
            TopLeftX: target.x,
            TopLeftY: target.y,
            Width: target.width.max(1.0),
            Height: target.height.max(1.0),
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let stride = mem::size_of::<VideoVertex>() as u32;
        let offset = 0u32;
        let vertices = video_vertices(D3d11TexRect::FULL);
        let mut common = constants;
        if upscaled_luma.is_some() {
            common = common.packed_d2s_luma_input();
        }
        let constants = D3d11VideoConstants {
            common,
            texture_scales: [
                if upscaled_luma.is_some() {
                    1.0
                } else {
                    video.tex_rect.width
                },
                if upscaled_luma.is_some() {
                    1.0
                } else {
                    video.tex_rect.height
                },
                video.tex_rect.width,
                video.tex_rect.height,
            ],
        };
        unsafe {
            self.context.UpdateSubresource(
                &self.vertex_buffer,
                0,
                None,
                vertices.as_ptr() as *const c_void,
                0,
                0,
            );
            self.context.UpdateSubresource(
                &self.constants,
                0,
                None,
                &constants as *const _ as *const c_void,
                0,
                0,
            );
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(&self.input_layout);
            self.context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(self.vertex_buffer.clone())),
                Some(&stride),
                Some(&offset),
            );
            self.context.VSSetShader(&self.vertex_shader, None);
            self.context.PSSetShader(&self.pixel_shader, None);
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            let srvs = [
                Some(upscaled_luma.cloned().unwrap_or_else(|| video.luma.clone())),
                Some(video.chroma.clone()),
                gamut_lut.cloned(),
            ];
            self.context.PSSetShaderResources(0, Some(&srvs));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context
                .OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            self.context.OMSetBlendState(None, None, u32::MAX);
            self.context.Draw(6, 0);
            self.context
                .PSSetShaderResources(0, Some(&[None, None, None]));
        }
        Ok(())
    }

    fn draw_overlays(
        &self,
        draws: &[D3d11OverlayDraw],
        render_target: &ID3D11RenderTargetView,
        target: D3d11DrawRect,
    ) -> Result<()> {
        if draws.is_empty() {
            return Ok(());
        }
        let viewport = D3D11_VIEWPORT {
            TopLeftX: target.x,
            TopLeftY: target.y,
            Width: target.width.max(1.0),
            Height: target.height.max(1.0),
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let stride = mem::size_of::<VideoVertex>() as u32;
        let offset = 0u32;
        let blend_factor = [0.0f32; 4];
        let vertices = video_vertices(D3d11TexRect::FULL);
        unsafe {
            self.context.UpdateSubresource(
                &self.vertex_buffer,
                0,
                None,
                vertices.as_ptr() as *const c_void,
                0,
                0,
            );
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(&self.input_layout);
            self.context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(self.vertex_buffer.clone())),
                Some(&stride),
                Some(&offset),
            );
            self.context.VSSetShader(&self.overlay_vertex_shader, None);
            self.context.PSSetShader(&self.overlay_pixel_shader, None);
            self.context
                .VSSetConstantBuffers(0, Some(&[Some(self.overlay_constants.clone())]));
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.overlay_constants.clone())]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.overlay_sampler.clone())]));
            self.context
                .OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            self.context
                .OMSetBlendState(&self.overlay_blend, Some(&blend_factor), u32::MAX);
            for draw in draws {
                self.context.UpdateSubresource(
                    &self.overlay_constants,
                    0,
                    None,
                    &draw.constants as *const _ as *const c_void,
                    0,
                    0,
                );
                self.context
                    .PSSetShaderResources(0, Some(&[Some(draw.texture.view.clone())]));
                self.context.Draw(6, 0);
            }
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context.OMSetBlendState(None, None, u32::MAX);
        }
        Ok(())
    }

    fn draw_encode(
        &self,
        scene: &ID3D11ShaderResourceView,
        render_target: &ID3D11RenderTargetView,
        width: u32,
        height: u32,
        mut constants: VideoUniforms,
    ) -> Result<()> {
        constants.scene_linear = 0;
        let constants = D3d11VideoConstants {
            common: constants,
            texture_scales: [1.0; 4],
        };
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width.max(1) as f32,
            Height: height.max(1) as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let stride = mem::size_of::<VideoVertex>() as u32;
        let offset = 0u32;
        let vertices = video_vertices(D3d11TexRect::FULL);
        unsafe {
            self.context.UpdateSubresource(
                &self.vertex_buffer,
                0,
                None,
                vertices.as_ptr() as *const c_void,
                0,
                0,
            );
            self.context.UpdateSubresource(
                &self.constants,
                0,
                None,
                &constants as *const _ as *const c_void,
                0,
                0,
            );
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(&self.input_layout);
            self.context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(self.vertex_buffer.clone())),
                Some(&stride),
                Some(&offset),
            );
            self.context.VSSetShader(&self.vertex_shader, None);
            self.context.PSSetShader(&self.encode_pixel_shader, None);
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            // Unbind the scene texture as an RTV before exposing it as an SRV.
            self.context
                .OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            self.context.OMSetBlendState(None, None, u32::MAX);
            self.context
                .PSSetShaderResources(0, Some(&[Some(scene.clone())]));
            self.context.Draw(6, 0);
            // The same texture becomes an RTV again on the next frame.
            self.context.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum D3d11VideoTextureFormat {
    Nv12,
    P010,
}

impl D3d11VideoTextureFormat {
    fn from_dxgi(format: DXGI_FORMAT) -> Option<Self> {
        if format == DXGI_FORMAT_NV12 {
            Some(Self::Nv12)
        } else if format == DXGI_FORMAT_P010 {
            Some(Self::P010)
        } else {
            None
        }
    }

    fn luma_srv(self) -> DXGI_FORMAT {
        match self {
            Self::Nv12 => DXGI_FORMAT_R8_UNORM,
            Self::P010 => DXGI_FORMAT_R16_UNORM,
        }
    }

    fn chroma_srv(self) -> DXGI_FORMAT {
        match self {
            Self::Nv12 => DXGI_FORMAT_R8G8_UNORM,
            Self::P010 => DXGI_FORMAT_R16G16_UNORM,
        }
    }
}

fn aspect_fit_rect(
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> D3d11DrawRect {
    let rect =
        PresentationLayout::aspect_fit(source_width, source_height, target_width, target_height)
            .presentation_rect();
    D3d11DrawRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

fn d3d_pq_code_for_lut(nits: f32) -> f32 {
    let m1 = 0.1593017578125_f32;
    let m2 = 78.84375_f32;
    let c1 = 0.8359375_f32;
    let c2 = 18.8515625_f32;
    let c3 = 18.6875_f32;
    let p = (nits / 10000.0).clamp(0.0, 1.0).powf(m1);
    ((c1 + c2 * p) / (1.0 + c3 * p).max(0.000_001)).powf(m2)
}

/// Quantize a luminance (nits) to its PQ code for the LUT cache key, matching
/// the wgpu/Metal quantizers. 16-bit PQ resolution keeps the sub-1-nit target
/// blacks of SDR targets distinct.
fn d3d_quantize_luma_pq(nits: f32) -> u32 {
    (d3d_pq_code_for_lut(nits) * 65535.0) as u32
}

fn d3d_code_to_primaries(code: u32) -> ColorPrimaries {
    match code {
        1 => ColorPrimaries::Bt2020,
        2 => ColorPrimaries::DisplayP3,
        _ => ColorPrimaries::Bt709,
    }
}

fn create_default_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    trace("create_default_device: D3D11CreateDevice");
    let feature_levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut device = None;
    let mut context = None;
    let mut selected = D3D_FEATURE_LEVEL(0);
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut selected),
            Some(&mut context),
        )
        .map_err(|error| d3d_error("D3D11CreateDevice", error))?;
    }
    let device =
        device.ok_or_else(|| PlayerError::Renderer("d3d11: device was null".to_string()))?;
    let context =
        context.ok_or_else(|| PlayerError::Renderer("d3d11: context was null".to_string()))?;
    trace("create_default_device: done");
    Ok((device, context))
}

fn create_swapchain(
    device: &ID3D11Device,
    hwnd: HWND,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    composition: bool,
    premultiplied_alpha: bool,
) -> Result<IDXGISwapChain1> {
    trace("create_swapchain: cast IDXGIDevice");
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|error| d3d_error("ID3D11Device::cast<IDXGIDevice>", error))?;
    trace("create_swapchain: get adapter");
    let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }
        .map_err(|error| d3d_error("IDXGIDevice::GetAdapter", error))?;
    trace("create_swapchain: get factory");
    let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }
        .map_err(|error| d3d_error("IDXGIAdapter::GetParent<IDXGIFactory2>", error))?;
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width.max(1),
        Height: height.max(1),
        Format: format,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: if premultiplied_alpha {
            DXGI_ALPHA_MODE_PREMULTIPLIED
        } else {
            DXGI_ALPHA_MODE_IGNORE
        },
        Flags: 0,
    };
    let swapchain = if composition {
        trace("create_swapchain: CreateSwapChainForComposition");
        unsafe { factory.CreateSwapChainForComposition(device, &desc, None) }
            .map_err(|error| d3d_error("IDXGIFactory2::CreateSwapChainForComposition", error))?
    } else {
        trace("create_swapchain: CreateSwapChainForHwnd");
        unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None) }
            .map_err(|error| d3d_error("IDXGIFactory2::CreateSwapChainForHwnd", error))?
    };
    trace("create_swapchain: done");
    Ok(swapchain)
}

fn configure_swapchain_color_space(
    swapchain: &IDXGISwapChain1,
    output_mode: D3d11OutputMode,
) -> Result<()> {
    let swapchain3: IDXGISwapChain3 = swapchain
        .cast()
        .map_err(|error| d3d_error("IDXGISwapChain1::cast<IDXGISwapChain3>", error))?;
    let color_space = output_mode.color_space();
    if matches!(output_mode, D3d11OutputMode::Hdr10) {
        let support = unsafe { swapchain3.CheckColorSpaceSupport(color_space) }
            .map_err(|error| d3d_error("IDXGISwapChain3::CheckColorSpaceSupport(HDR10)", error))?;
        if support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32 == 0 {
            return Err(PlayerError::Renderer(
                "d3d11: HDR10 swapchain color space is not presentable".to_string(),
            ));
        }
    }
    unsafe { swapchain3.SetColorSpace1(color_space) }
        .map_err(|error| d3d_error("IDXGISwapChain3::SetColorSpace1", error))
}

fn set_hdr10_metadata(
    swapchain: &IDXGISwapChain1,
    metadata: DXGI_HDR_METADATA_HDR10,
) -> Result<()> {
    let swapchain4: IDXGISwapChain4 = swapchain
        .cast()
        .map_err(|error| d3d_error("IDXGISwapChain1::cast<IDXGISwapChain4>", error))?;
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&metadata as *const DXGI_HDR_METADATA_HDR10).cast::<u8>(),
            mem::size_of::<DXGI_HDR_METADATA_HDR10>(),
        )
    };
    unsafe { swapchain4.SetHDRMetaData(DXGI_HDR_METADATA_TYPE_HDR10, Some(bytes)) }
        .map_err(|error| d3d_error("IDXGISwapChain4::SetHDRMetaData(HDR10)", error))
}

fn clear_hdr_metadata(swapchain: &IDXGISwapChain1) -> Result<()> {
    let swapchain4: IDXGISwapChain4 = swapchain
        .cast()
        .map_err(|error| d3d_error("IDXGISwapChain1::cast<IDXGISwapChain4>", error))?;
    unsafe { swapchain4.SetHDRMetaData(DXGI_HDR_METADATA_TYPE_NONE, None) }
        .map_err(|error| d3d_error("IDXGISwapChain4::SetHDRMetaData(None)", error))
}

fn release_backbuffer_views(context: &ID3D11DeviceContext, surface: &mut AttachedSurface) {
    // ResizeBuffers / creating a new chain requires every RTV and SRV of the
    // current back buffer to be gone, including those bound on the immediate
    // context.
    unsafe {
        context.OMSetRenderTargets(None, None);
        context.PSSetShaderResources(0, Some(&[None, None]));
    }
    surface.render_target = None;
    surface.output_texture = None;
    surface.published_texture = None;
    surface.linear_render_target = None;
    surface.linear_shader_resource = None;
}

fn create_d3d11_event_query(device: &ID3D11Device) -> Result<ID3D11Query> {
    let desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut query = None;
    unsafe {
        device
            .CreateQuery(&desc, Some(&mut query))
            .map_err(|error| d3d_error("ID3D11Device::CreateQuery(video retirement)", error))?;
    }
    query.ok_or_else(|| PlayerError::Renderer("d3d11: video retirement query was null".to_string()))
}

fn d3d11_event_query_complete(context: &ID3D11DeviceContext, query: &ID3D11Query) -> Result<bool> {
    let mut complete = BOOL::default();
    unsafe {
        context
            .GetData(
                query,
                Some((&mut complete as *mut BOOL).cast::<c_void>()),
                mem::size_of::<BOOL>() as u32,
                D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
            )
            .map_err(|error| d3d_error("ID3D11DeviceContext::GetData(video retirement)", error))?;
    }
    Ok(complete.as_bool())
}

fn wait_for_gpu(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<()> {
    let query = create_d3d11_event_query(device)?;
    unsafe {
        context.End(&query);
        context.Flush();
    }
    let started = Instant::now();
    while !d3d11_event_query_complete(context, &query)? {
        if started.elapsed() >= Duration::from_millis(100) {
            return Err(PlayerError::Renderer(
                "d3d11: compositor GPU handoff timed out; retaining the current decoder frame"
                    .into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

fn create_flutter_texture_target(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: SDR_SWAPCHAIN_FORMAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| d3d_error("ID3D11Device::CreateTexture2D(Flutter)", error))?;
    }
    let texture = texture.ok_or_else(|| {
        PlayerError::Renderer("d3d11: Flutter output texture was null".to_string())
    })?;
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| d3d_error("ID3D11Texture2D::cast<ID3D11Resource>(Flutter)", error))?;
    let mut render_target = None;
    unsafe {
        device
            .CreateRenderTargetView(&resource, None, Some(&mut render_target))
            .map_err(|error| d3d_error("ID3D11Device::CreateRenderTargetView(Flutter)", error))?;
    }
    Ok((
        texture,
        render_target.ok_or_else(|| {
            PlayerError::Renderer("d3d11: Flutter render target view was null".to_string())
        })?,
    ))
}

fn create_render_target(
    device: &ID3D11Device,
    swapchain: &IDXGISwapChain1,
) -> Result<ID3D11RenderTargetView> {
    trace("create_render_target: GetBuffer");
    let back_buffer: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0) }
        .map_err(|error| d3d_error("IDXGISwapChain1::GetBuffer", error))?;
    trace("create_render_target: cast resource");
    let resource: ID3D11Resource = back_buffer
        .cast()
        .map_err(|error| d3d_error("ID3D11Texture2D::cast<ID3D11Resource>", error))?;
    let mut view = None;
    unsafe {
        trace("create_render_target: CreateRenderTargetView");
        device
            .CreateRenderTargetView(&resource, None, Some(&mut view))
            .map_err(|error| d3d_error("ID3D11Device::CreateRenderTargetView", error))?;
    }
    let view =
        view.ok_or_else(|| PlayerError::Renderer("d3d11: render target was null".to_string()))?;
    trace("create_render_target: done");
    Ok(view)
}

fn create_linear_render_target(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11RenderTargetView, ID3D11ShaderResourceView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| d3d_error("ID3D11Device::CreateTexture2D(linear target)", error))?;
    }
    let texture = texture.ok_or_else(|| {
        PlayerError::Renderer("d3d11: linear render target texture was null".to_string())
    })?;
    let resource: ID3D11Resource = texture.cast().map_err(|error| {
        d3d_error(
            "ID3D11Texture2D::cast<ID3D11Resource>(linear target)",
            error,
        )
    })?;
    let mut render_target = None;
    let mut shader_resource = None;
    unsafe {
        device
            .CreateRenderTargetView(&resource, None, Some(&mut render_target))
            .map_err(|error| {
                d3d_error("ID3D11Device::CreateRenderTargetView(linear target)", error)
            })?;
        device
            .CreateShaderResourceView(&resource, None, Some(&mut shader_resource))
            .map_err(|error| {
                d3d_error(
                    "ID3D11Device::CreateShaderResourceView(linear target)",
                    error,
                )
            })?;
    }
    Ok((
        render_target.ok_or_else(|| {
            PlayerError::Renderer("d3d11: linear render target view was null".to_string())
        })?,
        shader_resource.ok_or_else(|| {
            PlayerError::Renderer("d3d11: linear shader resource view was null".to_string())
        })?,
    ))
}

fn present_swapchain(swapchain: &IDXGISwapChain1, operation: &'static str) -> Result<()> {
    let params = DXGI_PRESENT_PARAMETERS {
        DirtyRectsCount: 0,
        pDirtyRects: ptr::null_mut(),
        pScrollRect: ptr::null_mut(),
        pScrollOffset: ptr::null_mut(),
    };
    let status = unsafe { swapchain.Present1(0, DXGI_PRESENT_DO_NOT_WAIT, &params) };
    // Rendering shares the Windows platform thread with WM_MOVING. Never wait
    // for a full DXGI queue here: dropping one presentation keeps window
    // interaction responsive while decode/audio continue normally.
    if status == DXGI_ERROR_WAS_STILL_DRAWING {
        return Ok(());
    }
    status.ok().map_err(|error| d3d_error(operation, error))
}

fn create_plane_srv(
    state: &D3d11DeviceState,
    texture: &ID3D11Texture2D,
    array_index: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11ShaderResourceView> {
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| d3d_error("ID3D11Texture2D::cast<ID3D11Resource>", error))?;
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: array_index,
                ArraySize: 1,
            },
        },
    };
    let mut view = None;
    unsafe {
        state
            .device
            .CreateShaderResourceView(&resource, Some(&desc), Some(&mut view))
            .map_err(|error| {
                d3d_error(
                    "ID3D11Device::CreateShaderResourceView(D3D11VA plane)",
                    error,
                )
            })?;
    }
    view.ok_or_else(|| PlayerError::Renderer("d3d11: shader resource view was null".to_string()))
}

fn create_cpu_texture(
    state: &D3d11DeviceState,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    data: &[u8],
    bytes_per_row: u32,
) -> Result<D3d11OverlayTexture> {
    if width == 0 || height == 0 || bytes_per_row == 0 {
        return Err(PlayerError::Renderer(
            "d3d11: CPU texture dimensions must be non-zero".to_string(),
        ));
    }
    let required_len = (bytes_per_row as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| PlayerError::Renderer("d3d11: CPU texture size overflow".into()))?;
    if data.len() < required_len {
        return Err(PlayerError::Renderer(format!(
            "d3d11: CPU texture has {} bytes, expected at least {required_len}",
            data.len()
        )));
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let subresource = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr() as *const c_void,
        SysMemPitch: bytes_per_row,
        SysMemSlicePitch: bytes_per_row.saturating_mul(height),
    };
    let mut texture = None;
    unsafe {
        state
            .device
            .CreateTexture2D(&desc, Some(&subresource), Some(&mut texture))
            .map_err(|error| d3d_error("ID3D11Device::CreateTexture2D(cpu)", error))?;
    }
    let texture =
        texture.ok_or_else(|| PlayerError::Renderer("d3d11: CPU texture was null".to_string()))?;
    let view = create_texture2d_srv(state, &texture, format)?;
    Ok(D3d11OverlayTexture {
        _texture: texture,
        view,
    })
}

fn create_texture2d_srv(
    state: &D3d11DeviceState,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
) -> Result<ID3D11ShaderResourceView> {
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| d3d_error("ID3D11Texture2D::cast<ID3D11Resource>", error))?;
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: ::windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };
    let mut view = None;
    unsafe {
        state
            .device
            .CreateShaderResourceView(&resource, Some(&desc), Some(&mut view))
            .map_err(|error| d3d_error("ID3D11Device::CreateShaderResourceView(overlay)", error))?;
    }
    view.ok_or_else(|| PlayerError::Renderer("d3d11: overlay SRV was null".to_string()))
}

fn create_input_layout(device: &ID3D11Device, vertex_blob: &ID3DBlob) -> Result<ID3D11InputLayout> {
    const POSITION: &[u8] = b"POSITION\0";
    const TEXCOORD: &[u8] = b"TEXCOORD\0";
    let elements = [
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(POSITION.as_ptr()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(TEXCOORD.as_ptr()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 8,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
    ];
    let mut layout = None;
    unsafe {
        device
            .CreateInputLayout(&elements, blob_bytes(vertex_blob), Some(&mut layout))
            .map_err(|error| d3d_error("ID3D11Device::CreateInputLayout", error))?;
    }
    layout.ok_or_else(|| PlayerError::Renderer("d3d11: input layout was null".to_string()))
}

fn create_vertex_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let vertices = video_vertices(D3d11TexRect::FULL);
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: mem::size_of_val(&vertices) as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: ::windows::Win32::Graphics::Direct3D11::D3D11_BIND_VERTEX_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: vertices.as_ptr() as *const c_void,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    create_buffer(
        device,
        &desc,
        Some(&data),
        "ID3D11Device::CreateBuffer(vertex)",
    )
}

fn video_vertices(tex_rect: D3d11TexRect) -> [VideoVertex; 6] {
    let left = tex_rect.x;
    let right = tex_rect.right();
    let top = tex_rect.y;
    let bottom = tex_rect.bottom();
    [
        VideoVertex {
            position: [-1.0, -1.0],
            texcoord: [left, bottom],
        },
        VideoVertex {
            position: [-1.0, 1.0],
            texcoord: [left, top],
        },
        VideoVertex {
            position: [1.0, -1.0],
            texcoord: [right, bottom],
        },
        VideoVertex {
            position: [1.0, -1.0],
            texcoord: [right, bottom],
        },
        VideoVertex {
            position: [-1.0, 1.0],
            texcoord: [left, top],
        },
        VideoVertex {
            position: [1.0, 1.0],
            texcoord: [right, top],
        },
    ]
}

fn create_constants_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: mem::size_of::<D3d11VideoConstants>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    create_buffer(device, &desc, None, "ID3D11Device::CreateBuffer(constants)")
}

fn create_overlay_constants_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: mem::size_of::<OverlayUniforms>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    create_buffer(
        device,
        &desc,
        None,
        "ID3D11Device::CreateBuffer(overlay constants)",
    )
}

fn create_overlay_blend_state(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    let target = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: D3D11_BLEND_SRC_ALPHA,
        DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ONE,
        DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut render_targets = [D3D11_RENDER_TARGET_BLEND_DESC::default(); 8];
    render_targets[0] = target;
    let desc = D3D11_BLEND_DESC {
        AlphaToCoverageEnable: false.into(),
        IndependentBlendEnable: false.into(),
        RenderTarget: render_targets,
    };
    let mut state = None;
    unsafe {
        device
            .CreateBlendState(&desc, Some(&mut state))
            .map_err(|error| d3d_error("ID3D11Device::CreateBlendState(overlay)", error))?;
    }
    state.ok_or_else(|| PlayerError::Renderer("d3d11: overlay blend state was null".to_string()))
}

fn create_buffer(
    device: &ID3D11Device,
    desc: &D3D11_BUFFER_DESC,
    data: Option<&D3D11_SUBRESOURCE_DATA>,
    operation: &'static str,
) -> Result<ID3D11Buffer> {
    let mut buffer = None;
    unsafe {
        device
            .CreateBuffer(desc, data.map(|data| data as *const _), Some(&mut buffer))
            .map_err(|error| d3d_error(operation, error))?;
    }
    buffer.ok_or_else(|| PlayerError::Renderer(format!("d3d11: {operation} returned null")))
}

fn create_sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: ::windows::Win32::Graphics::Direct3D11::D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: ::windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: ::windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: ::windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: ::windows::Win32::Graphics::Direct3D11::D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };
    let mut sampler = None;
    unsafe {
        device
            .CreateSamplerState(&desc, Some(&mut sampler))
            .map_err(|error| d3d_error("ID3D11Device::CreateSamplerState", error))?;
    }
    sampler.ok_or_else(|| PlayerError::Renderer("d3d11: sampler was null".to_string()))
}

fn compile_shader(entry: &'static str, target: &'static str) -> Result<ID3DBlob> {
    compile_shader_source(SHADER_SOURCE, entry, target)
}

fn compile_shader_source(
    source: &'static [u8],
    entry: &'static str,
    target: &'static str,
) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let entry = nul(entry);
    let target = nul(target);
    unsafe {
        let result = D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            0,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(error) = result {
            let message = errors
                .as_ref()
                .map(blob_to_string)
                .unwrap_or_else(|| error.message());
            return Err(PlayerError::Renderer(format!(
                "d3d11: D3DCompile({}/{}) failed: {message}",
                String::from_utf8_lossy(&entry[..entry.len() - 1]),
                String::from_utf8_lossy(&target[..target.len() - 1])
            )));
        }
    }
    code.ok_or_else(|| PlayerError::Renderer("d3d11: D3DCompile returned null".to_string()))
}

fn clone_d3d11_texture(raw: *mut c_void) -> Result<ID3D11Texture2D> {
    if raw.is_null() {
        return Err(PlayerError::Renderer(
            "d3d11: D3D11VA texture pointer is null".to_string(),
        ));
    }
    let borrowed = unsafe { ID3D11Texture2D::from_raw_borrowed(&raw) }.ok_or_else(|| {
        PlayerError::Renderer("d3d11: failed to borrow D3D11VA texture".to_string())
    })?;
    Ok(borrowed.clone())
}

fn source_color_for_frame(frame: &PlayerVideoFrame) -> SourceColorState {
    SourceColorState::new(
        frame.frame.color_primaries(),
        frame.frame.transfer_function(),
    )
    .range(frame.frame.color_range())
    .matrix(frame.frame.matrix_coefficients())
    .hdr_metadata(frame.frame.hdr_metadata())
    .dovi(frame.frame.dovi_metadata())
    .measured_scene_avg_nits(frame.scene_avg_nits)
}

fn constants_for_frame(
    source: SourceColorState,
    texture_format: D3d11VideoTextureFormat,
    target: TargetColorState,
) -> VideoUniforms {
    let pipeline = VideoRenderPipeline::new(source, target);
    let uniforms = VideoUniforms::from_pipeline(
        &pipeline,
        matches!(texture_format, D3d11VideoTextureFormat::P010),
        false,
    );
    if matches!(target.transfer, TransferFunction::Pq) {
        uniforms.scene_linear_output()
    } else {
        uniforms
    }
}

fn dxgi_hdr10_metadata(source: SourceColorState) -> DXGI_HDR_METADATA_HDR10 {
    let mastering = source
        .hdr_metadata
        .and_then(|metadata| metadata.mastering_display);
    let content_light = source
        .hdr_metadata
        .and_then(|metadata| metadata.content_light);
    let default_primaries = primaries_coordinates(ColorPrimaries::Bt2020);
    let primaries = mastering
        .and_then(|metadata| metadata.display_primaries)
        .map(|primaries| PrimariesCoordinates {
            red: primaries[0],
            green: primaries[1],
            blue: primaries[2],
            white: mastering
                .and_then(|metadata| metadata.white_point)
                .unwrap_or(default_primaries.white),
        })
        .unwrap_or(default_primaries);
    let max_mastering = mastering
        .and_then(|metadata| metadata.max_luminance_nits)
        .unwrap_or(source.nominal_peak_nits.max(1000.0));
    let min_mastering = mastering
        .and_then(|metadata| metadata.min_luminance_nits)
        .unwrap_or(0.005);
    let max_cll = content_light
        .map(|metadata| metadata.max_content_light_level_nits)
        .unwrap_or_else(|| max_mastering.round().max(1.0) as u32);
    let max_fall = content_light
        .map(|metadata| metadata.max_frame_average_light_level_nits)
        .unwrap_or_else(|| (max_mastering * 0.4).round().max(1.0) as u32);

    DXGI_HDR_METADATA_HDR10 {
        RedPrimary: dxgi_chromaticity(primaries.red),
        GreenPrimary: dxgi_chromaticity(primaries.green),
        BluePrimary: dxgi_chromaticity(primaries.blue),
        WhitePoint: dxgi_chromaticity(primaries.white),
        MaxMasteringLuminance: nits_u32(max_mastering),
        MinMasteringLuminance: min_mastering_luminance(min_mastering),
        MaxContentLightLevel: clamp_u16(max_cll),
        MaxFrameAverageLightLevel: clamp_u16(max_fall),
    }
}

fn dxgi_chromaticity(value: Chromaticity) -> [u16; 2] {
    [chromaticity_coord(value.x), chromaticity_coord(value.y)]
}

fn chromaticity_coord(value: f32) -> u16 {
    if value.is_finite() {
        (value.clamp(0.0, 1.0) * 50_000.0)
            .round()
            .clamp(0.0, u16::MAX as f32) as u16
    } else {
        0
    }
}

fn nits_u32(value: f32) -> u32 {
    if value.is_finite() {
        value.round().clamp(1.0, u32::MAX as f32) as u32
    } else {
        1000
    }
}

fn min_mastering_luminance(value: f32) -> u32 {
    if value.is_finite() {
        (value.max(0.0) * 10_000.0)
            .round()
            .clamp(0.0, u32::MAX as f32) as u32
    } else {
        50
    }
}

fn clamp_u16(value: u32) -> u16 {
    value.min(u16::MAX as u32) as u16
}

fn nul(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

fn blob_to_string(blob: &ID3DBlob) -> String {
    String::from_utf8_lossy(blob_bytes(blob)).into_owned()
}

fn trace(message: &str) {
    if std::env::var_os("ERIKA_D3D11_TRACE").is_some() {
        eprintln!("erika d3d11: {message}");
    }
}

fn report_upscaler_failure(stage: &str, mode: LumaUpscalerMode, error: &PlayerError) {
    eprintln!("Erika D3D11 ArtCNN disabled after {stage} failure: {error}");
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "luma_upscaler",
            "stage": stage,
            "renderer": "d3d11",
            "requestedMode": format!("{mode:?}"),
            "activeBackend": "inactive",
            "reason": error.to_string(),
            "fallback": "native_luma_sampling",
        })
        .to_string(),
    );
}

fn d3d11va_srv_error(
    error: PlayerError,
    desc: &D3D11_TEXTURE2D_DESC,
    array_index: u32,
) -> PlayerError {
    PlayerError::Renderer(format!(
        "{error}; texture desc format={:?} bind_flags=0x{:x} misc_flags=0x{:x} usage={:?} array_size={} mip_levels={} sample_count={} array_index={}",
        desc.Format,
        desc.BindFlags,
        desc.MiscFlags,
        desc.Usage,
        desc.ArraySize,
        desc.MipLevels,
        desc.SampleDesc.Count,
        array_index
    ))
}

fn d3d_error(operation: &'static str, error: ::windows::core::Error) -> PlayerError {
    PlayerError::Renderer(format!("d3d11: {operation} failed: {}", error.message()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn software_test_renderer() -> D3d11Renderer {
        use ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;

        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .expect("create WARP device");
        }
        let mut renderer = D3d11Renderer::new().unwrap();
        renderer
            .set_device(device.unwrap(), context.unwrap())
            .unwrap();
        renderer
    }

    #[test]
    fn software_p010_upload_preserves_odd_dimensions_and_sample_precision() {
        use ::windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_STAGING,
        };
        let mut renderer = software_test_renderer();
        let luma: Vec<u8> = [64u16, 128, 256, 512, 1000, 1023, 940, 128, 64]
            .into_iter()
            .flat_map(|value| (value << 6).to_le_bytes())
            .collect();
        let chroma: Vec<u8> = [64u16, 960, 128, 896, 512, 512, 960, 64]
            .into_iter()
            .flat_map(|value| (value << 6).to_le_bytes())
            .collect();
        renderer
            .upload_planar_frame(
                PlanarFrame {
                    format: PlanarPixelFormat::P010,
                    width: 3,
                    height: 3,
                    luma: luma.clone(),
                    chroma: chroma.clone(),
                },
                SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq),
            )
            .unwrap();
        let video = renderer.current_video.as_ref().unwrap();
        assert_eq!(video.constants.is_p010, 1);
        assert_eq!(video.tex_rect, D3d11TexRect::FULL);
        let state = renderer.state.as_ref().unwrap();
        for (view, width, height, format, expected, pitch) in [
            (&video.luma, 3, 3, DXGI_FORMAT_R16_UNORM, luma, 6usize),
            (
                &video.chroma,
                2,
                2,
                DXGI_FORMAT_R16G16_UNORM,
                chroma,
                8usize,
            ),
        ] {
            unsafe {
                let texture: ID3D11Texture2D = view.GetResource().unwrap().cast().unwrap();
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                assert_eq!(
                    (desc.Width, desc.Height, desc.Format),
                    (width, height, format)
                );
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                let mut staging = None;
                state
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut staging))
                    .unwrap();
                let staging = staging.unwrap();
                state.context.CopyResource(&staging, &texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                state
                    .context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .unwrap();
                let mut actual = Vec::new();
                for row in 0..height as usize {
                    actual.extend_from_slice(std::slice::from_raw_parts(
                        mapped
                            .pData
                            .cast::<u8>()
                            .add(row * mapped.RowPitch as usize),
                        pitch,
                    ));
                }
                state.context.Unmap(&staging, 0);
                assert_eq!(actual, expected);
            }
        }
        assert_eq!(renderer.runtime_stats().software_video_frames, 1);
        assert_eq!(renderer.runtime_stats().hdr_source_frames, 1);
    }

    #[test]
    fn software_wmv_uploads_to_native_d3d11() {
        use crate::ffmpeg::{DecoderBackend, DecoderOutputFrame, Demuxer, StreamSelection};
        use crate::renderer::VideoFramePayload;
        let mut renderer = software_test_renderer();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/software/wmv2-wmav2.asf");
        let mut demuxer = Demuxer::open_path(path).unwrap();
        demuxer
            .set_stream_selection(StreamSelection::only([0]))
            .unwrap();
        let mut decoder = demuxer.open_decoder(0).unwrap();
        let mut uploads = 0;
        while let Some(packet) = demuxer.read_packet().unwrap() {
            decoder.send_packet(&packet).unwrap();
            while let DecoderOutputFrame::Frame(frame) = decoder.receive_frame().unwrap() {
                let player_frame = PlayerVideoFrame {
                    frame: VideoFramePayload::from_decoded(frame).unwrap(),
                    decode_backend: DecoderBackend::Software,
                    pts: None,
                    media_time: Duration::ZERO,
                    late_by: None,
                    generation: 0,
                    scene_avg_nits: None,
                };
                renderer.upload_player_frame(&player_frame).unwrap();
                let video = renderer.current_video.as_ref().unwrap();
                assert_eq!((video.width, video.height), (160, 90));
                assert_eq!(video.constants.is_p010, 0);
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { video._texture.GetDesc(&mut desc) };
                assert_eq!(desc.Format, DXGI_FORMAT_R8_UNORM);
                assert_eq!((desc.Width, desc.Height), (160, 90));
                uploads += 1;
            }
        }
        assert_eq!(uploads, 20);
        assert_eq!(renderer.runtime_stats().software_video_frames, uploads);
        assert_eq!(renderer.runtime_stats().hardware_video_frames, 0);
        assert_eq!(renderer.runtime_stats().zero_copy_video_frames, 0);
        renderer.clear_current_frame().unwrap();
        assert!(renderer.current_video.is_none());
    }

    #[test]
    fn flutter_scene_ignores_clock_ticks_but_detects_output_changes() {
        use crate::overlay::OverlayViewport;
        use crate::subtitle::SubtitleBitmapPlane;
        let mut overlay = OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(8, 8),
            subtitle_planes: vec![SubtitleBitmapPlane::new(0, 0, 1, 1, vec![255; 4])],
            subtitle_alpha_planes: vec![],
            subtitle_changed: true,
        };
        let scene = FlutterTextureScene::new(
            7,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).overlay(Some(&overlay)),
        );
        overlay.pts = Duration::from_secs(2);
        overlay.subtitle_changed = false;
        let context = RenderFrameContext::new(Duration::from_secs(2), 1).overlay(Some(&overlay));
        assert!(scene.matches(7, LumaUpscalerMode::Off, context));
        assert!(!scene.matches(8, LumaUpscalerMode::Off, context));
        assert!(!scene.matches(7, LumaUpscalerMode::ArtCnnC4F16, context));
        assert!(!scene.matches(
            7,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 2).overlay(Some(&overlay))
        ));
        overlay.subtitle_planes[0].rgba[0] = 0; // Includes HUD pixel changes.
        assert!(!scene.matches(
            7,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).overlay(Some(&overlay))
        ));
        assert!(!scene.matches(
            7,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1)
        ));
    }

    #[test]
    fn flutter_scene_reuses_unchanged_cpu_buffers_across_video_frames() {
        use crate::danmaku::DanmakuViewport;
        use crate::overlay::OverlayViewport;
        use crate::subtitle::{SubtitleAlphaBitmap, SubtitleBitmapPlacement, SubtitleBitmapPlane};
        let mut overlay = OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(128, 64),
            subtitle_planes: vec![SubtitleBitmapPlane::new(
                0,
                0,
                128,
                64,
                vec![255; 128 * 64 * 4],
            )],
            subtitle_alpha_planes: vec![SubtitleAlphaBitmap::new(
                SubtitleBitmapPlacement::new(0, 0, 128, 64),
                128,
                0xffffffff,
                vec![255; 128 * 64],
            )],
            subtitle_changed: true,
        };
        let mut plan = DanmakuRenderPlan::empty(Duration::ZERO, 1, DanmakuViewport::new(128, 64));
        plan.items.push(DanmakuGlyphInstance {
            item_id: 1,
            rect: [0.0, 0.0, 1.0, 1.0],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            color_rgba: [1.0; 4],
            outline_rgba: [0.0; 4],
            shadow_rgba: [0.0; 4],
            shadow_offset: [0.0; 2],
        });
        let mut scene = FlutterTextureScene::new(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1)
                .overlay(Some(&overlay))
                .danmaku(Some(&plan)),
        );
        let rgba = scene.overlay.as_ref().unwrap().subtitle_planes[0]
            .rgba
            .as_ptr();
        let alpha = scene.overlay.as_ref().unwrap().subtitle_alpha_planes[0]
            .alpha
            .as_ptr();
        let items = scene.danmaku.as_ref().unwrap().items.as_ptr();
        for token in 2..=121 {
            overlay.pts = Duration::from_millis(token * 10);
            overlay.subtitle_changed = false;
            plan.media_time = overlay.pts;
            let context = RenderFrameContext::new(overlay.pts, 1)
                .overlay(Some(&overlay))
                .danmaku(Some(&plan));
            assert!(!scene.matches(token, LumaUpscalerMode::Off, context));
            scene.update(token, LumaUpscalerMode::Off, context);
            assert!(scene.matches(token, LumaUpscalerMode::Off, context));
            assert_eq!(
                scene.overlay.as_ref().unwrap().subtitle_planes[0]
                    .rgba
                    .as_ptr(),
                rgba
            );
            assert_eq!(
                scene.overlay.as_ref().unwrap().subtitle_alpha_planes[0]
                    .alpha
                    .as_ptr(),
                alpha
            );
            assert_eq!(scene.danmaku.as_ref().unwrap().items.as_ptr(), items);
        }

        // A real subtitle/HUD change still replaces the cached pixels even
        // when the video frame is paused and subtitle_changed is false.
        overlay.subtitle_planes[0].rgba[0] = 17;
        overlay.subtitle_alpha_planes[0].alpha[0] = 23;
        let context = RenderFrameContext::new(overlay.pts, 1)
            .overlay(Some(&overlay))
            .danmaku(Some(&plan));
        assert!(!scene.matches(121, LumaUpscalerMode::Off, context));
        scene.update(121, LumaUpscalerMode::Off, context);
        assert!(scene.matches(121, LumaUpscalerMode::Off, context));
        assert_eq!(
            scene.overlay.as_ref().unwrap().subtitle_planes[0].rgba[0],
            17
        );
        assert_eq!(
            scene.overlay.as_ref().unwrap().subtitle_alpha_planes[0].alpha[0],
            23
        );
        assert_eq!(scene.danmaku.as_ref().unwrap().items.as_ptr(), items);

        let rgba = scene.overlay.as_ref().unwrap().subtitle_planes[0]
            .rgba
            .as_ptr();
        plan.items[0].rect[0] = 2.0;
        let context = RenderFrameContext::new(overlay.pts, 1)
            .overlay(Some(&overlay))
            .danmaku(Some(&plan));
        assert!(!scene.matches(121, LumaUpscalerMode::Off, context));
        scene.update(121, LumaUpscalerMode::Off, context);
        assert!(scene.matches(121, LumaUpscalerMode::Off, context));
        assert_eq!(
            scene.overlay.as_ref().unwrap().subtitle_planes[0]
                .rgba
                .as_ptr(),
            rgba
        );
        assert_eq!(scene.danmaku.as_ref().unwrap().items[0].rect[0], 2.0);

        scene.update(
            121,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 2),
        );
        assert!(scene.overlay.is_none() && scene.danmaku.is_none());
        assert!(scene.matches(
            121,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 2)
        ));
    }

    #[test]
    fn flutter_publication_is_sdr_immutable_and_stable_while_idle() {
        use crate::core::FlutterTextureHandle;
        let mut renderer = D3d11Renderer::new().unwrap();
        // WARP keeps this GPU/API contract test runnable on headless CI too.
        let mut device = None;
        let mut device_context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut device_context),
            )
            .unwrap();
        }
        renderer
            .set_device(device.unwrap(), device_context.unwrap())
            .unwrap();
        renderer
            .attach_surface(PlatformSurface::FlutterTexture(FlutterTextureHandle::new(
                FlutterTextureKind::WindowsTextureRegistrar,
                1,
                4,
                4,
                1.0,
            )))
            .unwrap();
        let first = renderer
            .surface
            .as_ref()
            .unwrap()
            .published_texture
            .as_ref()
            .unwrap()
            .clone();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            first.GetDesc(&mut desc);
        }
        assert_eq!(desc.Format, SDR_SWAPCHAIN_FORMAT);
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        assert!(matches!(
            renderer.select_output_mode_for_source(source).unwrap(),
            D3d11OutputMode::Sdr
        ));
        assert_eq!(renderer.stats.hdr10_metadata_failures, 0);
        renderer.render_clear(1.0).unwrap();
        assert_eq!(
            first.as_raw(),
            renderer
                .surface
                .as_ref()
                .unwrap()
                .published_texture
                .as_ref()
                .unwrap()
                .as_raw()
        );

        let state = renderer.state.as_ref().unwrap();
        unsafe {
            state.context.ClearRenderTargetView(
                renderer
                    .surface
                    .as_ref()
                    .unwrap()
                    .render_target
                    .as_ref()
                    .unwrap(),
                &[1.0, 0.0, 0.0, 1.0],
            );
        }
        renderer.publish_flutter_texture().unwrap();
        let next = renderer
            .surface
            .as_ref()
            .unwrap()
            .published_texture
            .as_ref()
            .unwrap()
            .clone();
        assert_ne!(first.as_raw(), next.as_raw());
        assert_eq!(
            read_bgra_pixel(renderer.state.as_ref().unwrap(), &first),
            [0, 0, 0, 255]
        );
        assert_eq!(
            read_bgra_pixel(renderer.state.as_ref().unwrap(), &next),
            [0, 0, 255, 255]
        );
        renderer
            .resize_surface(SurfaceMetrics::new(8, 6, 1.0))
            .unwrap();
        unsafe {
            first.GetDesc(&mut desc);
        }
        assert_eq!((desc.Width, desc.Height), (4, 4));
        let mut resized = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            renderer
                .surface
                .as_ref()
                .unwrap()
                .published_texture
                .as_ref()
                .unwrap()
                .GetDesc(&mut resized);
        }
        assert_eq!((resized.Width, resized.Height), (8, 6));
    }

    fn read_bgra_pixel(state: &D3d11DeviceState, texture: &ID3D11Texture2D) -> [u8; 4] {
        use ::windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_STAGING,
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe {
            state
                .device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
        }
        let staging = staging.unwrap();
        unsafe {
            state.context.CopyResource(&staging, texture);
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            state
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let bytes = *(mapped.pData as *const [u8; 4]);
            state.context.Unmap(&staging, 0);
            bytes
        }
    }

    #[test]
    fn flutter_scene_detects_danmaku_motion_and_visibility_changes() {
        use crate::danmaku::DanmakuViewport;
        let mut plan = DanmakuRenderPlan::empty(Duration::ZERO, 1, DanmakuViewport::new(8, 8));
        plan.items.push(DanmakuGlyphInstance {
            item_id: 1,
            rect: [0.0, 0.0, 1.0, 1.0],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            color_rgba: [1.0; 4],
            outline_rgba: [0.0; 4],
            shadow_rgba: [0.0; 4],
            shadow_offset: [0.0; 2],
        });
        let scene = FlutterTextureScene::new(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).danmaku(Some(&plan)),
        );
        plan.media_time = Duration::from_secs(1);
        assert!(scene.matches(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).danmaku(Some(&plan))
        ));
        plan.items[0].rect[0] = 2.0;
        assert!(!scene.matches(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).danmaku(Some(&plan))
        ));
        plan.items.clear();
        assert!(!scene.matches(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).danmaku(Some(&plan))
        ));
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.0001,
            "expected {actual} to be close to {expected}"
        );
    }

    #[test]
    fn video_texture_format_maps_plane_srv_formats() {
        assert_eq!(
            D3d11VideoTextureFormat::from_dxgi(DXGI_FORMAT_NV12)
                .unwrap()
                .luma_srv(),
            DXGI_FORMAT_R8_UNORM
        );
        assert_eq!(
            D3d11VideoTextureFormat::from_dxgi(DXGI_FORMAT_P010)
                .unwrap()
                .chroma_srv(),
            DXGI_FORMAT_R16G16_UNORM
        );
    }

    #[test]
    fn d3d11_tex_rect_crops_padded_decoder_texture() {
        let rect = D3d11TexRect::visible_region(1920, 1080, 2048, 1088);

        assert_close(rect.width, 1920.0 / 2048.0);
        assert_close(rect.height, 1080.0 / 1088.0);
        let vertices = video_vertices(rect);
        assert_close(vertices[0].texcoord[1], 1080.0 / 1088.0);
        assert_close(vertices[2].texcoord[0], 1920.0 / 2048.0);
    }

    #[test]
    fn d3d11_surface_extent_is_already_physical() {
        let metrics = SurfaceMetrics::new(960, 540, 1.5);

        assert_eq!(metrics.physical_size(), (960, 540));
        assert_eq!(metrics.content_scale, 1.5);
    }

    #[test]
    fn d3d11_renderer_config_initializes_upscaler_mode() {
        let renderer = D3d11Renderer::with_config(MetalRendererConfig {
            output_mode: crate::renderer::metal::MetalOutputMode::Sdr,
            luma_upscaler: LumaUpscalerMode::ArtCnnC4F16,
            video_alpha_mode: VideoAlphaMode::Opaque,
        })
        .unwrap();

        let stats = renderer.runtime_stats();
        assert_eq!(stats.upscaler_mode, LumaUpscalerMode::ArtCnnC4F16);
        assert_eq!(stats.upscaler_backend, LumaUpscalerBackendStatus::Building);
    }

    #[test]
    fn d3d11_video_shader_compiles() {
        compile_shader("vs_main", "vs_4_0").unwrap();
        compile_shader("ps_main", "ps_4_0").unwrap();
        compile_shader("encode_ps_main", "ps_4_0").unwrap();
    }

    #[test]
    fn d3d11_overlay_shader_compiles() {
        compile_shader_source(OVERLAY_SHADER_SOURCE, "overlay_vs_main", "vs_4_0").unwrap();
        compile_shader_source(OVERLAY_SHADER_SOURCE, "overlay_ps_main", "ps_4_0").unwrap();
    }

    #[test]
    fn renderer_stats_distinguish_direct_and_shared_zero_copy() {
        let mut renderer = D3d11Renderer::new().unwrap();
        renderer.stats.hardware_video_frames = 3;
        renderer.stats.zero_copy_video_frames = 3;
        renderer.stats.direct_zero_copy_video_frames = 1;
        renderer.stats.shared_handle_video_frames = 2;
        renderer.stats.hdr_source_frames = 3;
        renderer.stats.hdr10_output_frames = 2;
        renderer.stats.sdr_tonemap_frames = 1;
        renderer.stats.hdr10_output_active = true;

        let stats = renderer.runtime_stats();
        assert_eq!(stats.hardware_video_frames, 3);
        assert_eq!(stats.zero_copy_video_frames, 3);
        assert_eq!(stats.direct_zero_copy_video_frames, 1);
        assert_eq!(stats.shared_handle_video_frames, 2);
        assert_eq!(stats.hdr_source_frames, 3);
        assert_eq!(stats.hdr10_output_frames, 2);
        assert_eq!(stats.sdr_tonemap_frames, 1);
        assert!(stats.hdr10_output_active);
    }

    #[test]
    fn hdr10_output_mode_targets_bt2020_pq() {
        let source = SourceColorState::new(ColorPrimaries::DisplayP3, TransferFunction::Pq);
        let target = D3d11OutputMode::Hdr10.target_color_for_source(source);

        assert_eq!(target.primaries, ColorPrimaries::Bt2020);
        assert_eq!(target.transfer, TransferFunction::Pq);
        assert_eq!(target.peak_nits, 10_000.0);
        assert_eq!(target.reference_white_nits, 203.0);
    }

    #[test]
    fn hdr10_video_constants_request_scene_linear_output() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let hdr10 = constants_for_frame(
            source,
            D3d11VideoTextureFormat::P010,
            D3d11OutputMode::Hdr10.target_color(),
        );
        let sdr = constants_for_frame(
            source,
            D3d11VideoTextureFormat::P010,
            D3d11OutputMode::Sdr.target_color(),
        );

        assert_eq!(hdr10.scene_linear, 1);
        assert_eq!(sdr.scene_linear, 0);
    }

    #[test]
    fn hlsl_evaluates_reinhard_and_mobius_in_linear_nits() {
        // libplacebo runs these two curves in the linear PL_HDR_NORM domain
        // and only the PQ-domain operators work on encoded values; evaluating
        // them on PQ codes moved the Mobius knee far below its intended
        // position. The HLSL is compiled by the D3D11 runtime, not by the Rust
        // build, so this pins the source shape rather than executing it.
        let source = std::str::from_utf8(SHADER_SOURCE).unwrap();
        for branch in ["if (tone_map == 1u)", "if (tone_map == 2u)"] {
            let start = source.find(branch).expect("branch");
            let rest = &source[start..];
            let end = rest.find("\n    }").expect("branch end");
            let body = &rest[..end];
            assert!(
                body.contains("float out_range_nits = max(dst_peak - dst_black, 0.000001);"),
                "{branch} must scale in linear nits"
            );
            assert!(
                body.contains("nits_from_pq(x)"),
                "{branch} must decode the PQ code before the curve"
            );
            assert!(
                body.contains("return pq_code("),
                "{branch} must re-encode the result"
            );
        }
    }

    #[test]
    fn hlsl_gamut_lut_samples_the_target_black_to_peak_axis() {
        // libplacebo's LUT I axis is [target black, target peak]; a backend
        // that samples `ipt.x / peak` again would diverge from the generated
        // LUT. The lattice position must also be remapped to the texel-center
        // coordinate (libplacebo's `texel_scale`), or the zero-chroma texel at
        // the low end of the C axis crushes saturation.
        let source = std::str::from_utf8(SHADER_SOURCE).unwrap();
        assert!(source.contains("float lut_min = pq_code(tone_map_extra.z);"));
        assert!(source.contains("clamp((ipt.x - lut_min) / lut_range, 0.0, 1.0)"));
        assert!(source.contains("pos.y * (31.0 / 32.0) + 0.5 / 32.0"));
        assert!(source.contains("pos.x * (47.0 / 48.0) + 0.5 / 48.0"));
    }

    #[test]
    fn overlay_uniforms_enable_scene_linear_only_for_hdr10() {
        let hdr10 =
            OverlayUniforms::rgba_plane(0, 0, 1, 1, 1, 1, D3d11OutputMode::Hdr10.target_color());
        let sdr =
            OverlayUniforms::rgba_plane(0, 0, 1, 1, 1, 1, D3d11OutputMode::Sdr.target_color());

        assert_eq!(mem::size_of::<OverlayUniforms>(), 80);
        assert_eq!(hdr10.ui_nits[0], 203.0);
        assert_eq!(hdr10.ui_nits[1], 203.0);
        assert_eq!(sdr.ui_nits[1], 0.0);
    }

    #[test]
    fn dxgi_hdr10_metadata_uses_source_mastering_values() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .hdr_metadata(Some(crate::renderer::pipeline::HdrMetadata::new(
                Some(crate::renderer::pipeline::MasteringDisplayMetadata {
                    display_primaries: Some([
                        Chromaticity::new(0.708, 0.292),
                        Chromaticity::new(0.170, 0.797),
                        Chromaticity::new(0.131, 0.046),
                    ]),
                    white_point: Some(Chromaticity::new(0.3127, 0.3290)),
                    min_luminance_nits: Some(0.005),
                    max_luminance_nits: Some(1000.0),
                }),
                Some(crate::renderer::pipeline::ContentLightMetadata {
                    max_content_light_level_nits: 4000,
                    max_frame_average_light_level_nits: 450,
                }),
            )));

        let metadata = dxgi_hdr10_metadata(source);

        assert_eq!(metadata.RedPrimary, [35400, 14600]);
        assert_eq!(metadata.GreenPrimary, [8500, 39850]);
        assert_eq!(metadata.BluePrimary, [6550, 2300]);
        assert_eq!(metadata.WhitePoint, [15635, 16450]);
        assert_eq!(metadata.MaxMasteringLuminance, 1000);
        assert_eq!(metadata.MinMasteringLuminance, 50);
        assert_eq!(metadata.MaxContentLightLevel, 4000);
        assert_eq!(metadata.MaxFrameAverageLightLevel, 450);
    }
}
