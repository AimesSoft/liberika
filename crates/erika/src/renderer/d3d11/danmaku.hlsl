// Shader model 4: glyph data is an ordinary unfiltered RGBA32Float texture.
// Keep the six-texel record in sync with renderer::danmaku_batch.
cbuffer DanmakuConstants : register(b0) {
    float2 viewport;
    uint glyph_texture_width;
    uint reserved;
    float4 ui_nits;
};

Texture2D<float> fillAtlas : register(t0);
Texture2D<float> outlineAtlas : register(t1);
Texture2D<float4> glyphData : register(t2);
SamplerState atlasSampler : register(s0);

struct VsIn {
    float4 rect : RECT;
    uint2 glyph_range : GLYPHS;
    float opacity : OPACITY;
    uint vertex_id : SV_VertexID;
};

struct VsOut {
    float4 position : SV_Position;
    float2 pixel : TEXCOORD0;
    nointerpolation uint2 glyph_range : TEXCOORD1;
    nointerpolation float opacity : TEXCOORD2;
};

VsOut danmaku_vs_main(VsIn input) {
    // Same winding as the existing overlay quad, without a per-vertex buffer.
    const float2 corners[6] = {
        float2(0.0, 1.0), float2(0.0, 0.0), float2(1.0, 1.0),
        float2(1.0, 1.0), float2(0.0, 0.0), float2(1.0, 0.0)
    };
    float2 pixel = input.rect.xy + corners[input.vertex_id] * input.rect.zw;
    float2 safe_viewport = max(viewport, float2(1.0, 1.0));
    VsOut output;
    output.position = float4(pixel.x / safe_viewport.x * 2.0 - 1.0,
                             1.0 - pixel.y / safe_viewport.y * 2.0, 0.0, 1.0);
    output.pixel = pixel;
    output.glyph_range = input.glyph_range;
    output.opacity = input.opacity;
    return output;
}

float4 glyph_field(uint glyph_index, uint field) {
    uint texel = glyph_index * 6u + field;
    return glyphData.Load(int3(texel % glyph_texture_width, texel / glyph_texture_width, 0));
}

// SDR uses the existing gamma-space composition; HDR uses the same reference-
// linear conversion as the subtitle/emoji path, before the final PQ encode.
float3 convert_rgb(float3 rgb) {
    if (ui_nits.y > 0.0) {
        return pow(max(rgb, float3(0.0, 0.0, 0.0)), float3(2.2, 2.2, 2.2))
            * (max(ui_nits.x, 1.0) / max(ui_nits.y, 1.0));
    }
    return rgb;
}

float mask_at(Texture2D<float> atlas, float2 pixel, float4 rect, float4 tex_rect) {
    float2 local = (pixel - rect.xy) / max(rect.zw, float2(0.0001, 0.0001));
    // An instance may cover several glyphs, blank space or shifted shadows.
    // Clamp addressing is only an atlas edge policy, not a glyph-bounds test.
    if (any(local < 0.0) || any(local >= 1.0)) {
        return 0.0;
    }
    return atlas.SampleLevel(atlasSampler, tex_rect.xy + local * tex_rect.zw, 0.0);
}

float4 layer_over(float4 below, float4 color, float mask) {
    float alpha = color.a * mask;
    return float4(convert_rgb(color.rgb) * alpha, alpha) + below * (1.0 - alpha);
}

float4 danmaku_ps_main(VsOut input) : SV_Target {
    float4 fill = 0.0;
    float4 outline = 0.0;
    float4 shadow = 0.0;
    [loop]
    for (uint i = 0u; i < input.glyph_range.y; ++i) {
        uint glyph = input.glyph_range.x + i;
        float4 rect = glyph_field(glyph, 0u);
        float4 tex_rect = glyph_field(glyph, 1u);
        float4 color = glyph_field(glyph, 2u);
        float4 outline_color = glyph_field(glyph, 3u);
        float4 shadow_color = glyph_field(glyph, 4u);
        float2 shadow_offset = glyph_field(glyph, 5u).xy;
        fill = layer_over(fill, color, mask_at(fillAtlas, input.pixel, rect, tex_rect));
        outline = layer_over(outline, outline_color, mask_at(outlineAtlas, input.pixel, rect, tex_rect));
        shadow = layer_over(shadow, shadow_color,
                            mask_at(outlineAtlas, input.pixel - shadow_offset, rect, tex_rect));
    }
    // All fills cover all outlines, and all outlines cover all shadows, even
    // where neighboring glyphs overlap. Only the completed comment is faded.
    float4 composed = fill + (outline + shadow * (1.0 - outline.a)) * (1.0 - fill.a);
    if (composed.a <= 0.0) {
        discard;
    }
    // Existing fixed-function blending expects straight RGB and source-over
    // alpha. Never multiply RGB or each individual effect by comment opacity.
    return float4(composed.rgb / max(composed.a, 0.000001), composed.a * input.opacity);
}
