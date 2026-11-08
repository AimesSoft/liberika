// One instanced draw for all comment strips. Glyph records live in an unfiltered
// RGBA32Float texture so this path also works on GLES 3 without fragment storage.
struct Uniforms {
    viewport: vec2<f32>,
    output_encoding: u32,
    glyph_width: u32,
};
@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var glyph_data: texture_2d<f32>;
@group(0) @binding(2) var fill_texture: texture_2d<f32>;
@group(0) @binding(3) var outline_texture: texture_2d<f32>;
@group(0) @binding(4) var mask_sampler: sampler;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) pixel: vec2<f32>,
    @location(1) @interpolate(flat) glyph_range: vec2<u32>,
    @location(2) @interpolate(flat) opacity: f32,
};

@vertex
fn danmaku_vertex(
    @builtin(vertex_index) vertex_id: u32,
    @location(0) rect: vec4<f32>,
    @location(1) glyph_range: vec2<u32>,
    @location(2) opacity: f32,
) -> VertexOut {
    // Array-free for the GLES 3 / SwiftShader vertex-index path.
    let unit = vec2<f32>(f32(vertex_id & 1u), f32((vertex_id >> 1u) & 1u));
    let pixel = rect.xy + unit * rect.zw;
    let ndc = vec2<f32>(
        pixel.x / max(uniforms.viewport.x, 1.0) * 2.0 - 1.0,
        1.0 - pixel.y / max(uniforms.viewport.y, 1.0) * 2.0,
    );
    return VertexOut(vec4<f32>(ndc, 0.0, 1.0), pixel, glyph_range, opacity);
}

fn glyph_field(glyph: u32, field: u32) -> vec4<f32> {
    let index = glyph * 6u + field;
    return textureLoad(glyph_data, vec2<i32>(
        i32(index % uniforms.glyph_width), i32(index / uniforms.glyph_width),
    ), 0);
}

fn coverage(mask: texture_2d<f32>, pixel: vec2<f32>, rect: vec4<f32>, uv: vec4<f32>) -> f32 {
    if (any(rect.zw <= vec2<f32>(0.0))) {
        return 0.0;
    }
    let local = (pixel - rect.xy) / rect.zw;
    if (any(local < vec2<f32>(0.0)) || any(local >= vec2<f32>(1.0))) {
        return 0.0;
    }
    return textureSampleLevel(mask, mask_sampler, uv.xy + local * uv.zw, 0.0).r;
}

fn over(color: vec4<f32>, mask: f32, below: vec4<f32>) -> vec4<f32> {
    // Keep identical to wgpu_overlay.wgsl, not Metal's EDR color transform.
    let rgb = select(color.rgb, pow(max(color.rgb, vec3<f32>(0.0)), vec3<f32>(2.2)),
        uniforms.output_encoding == 1u);
    let alpha = color.a * mask;
    return vec4<f32>(rgb * alpha, alpha) + below * (1.0 - alpha);
}

@fragment
fn danmaku_fragment(in: VertexOut) -> @location(0) vec4<f32> {
    var fill = vec4<f32>(0.0);
    var outline = vec4<f32>(0.0);
    var shadow = vec4<f32>(0.0);
    let end = in.glyph_range.x + in.glyph_range.y;
    for (var glyph = in.glyph_range.x; glyph < end; glyph += 1u) {
        let rect = glyph_field(glyph, 0u);
        let uv = glyph_field(glyph, 1u);
        let offset = glyph_field(glyph, 5u).xy;
        fill = over(glyph_field(glyph, 2u), coverage(fill_texture, in.pixel, rect, uv), fill);
        outline = over(glyph_field(glyph, 3u), coverage(outline_texture, in.pixel, rect, uv), outline);
        shadow = over(glyph_field(glyph, 4u), coverage(outline_texture, in.pixel,
            vec4<f32>(rect.xy + offset, rect.zw), uv), shadow);
    }
    let composed = fill + (outline + shadow * (1.0 - outline.a)) * (1.0 - fill.a);
    // The render target uses straight-alpha blending. Apply comment opacity only
    // after its glyphs and effects have become a single premultiplied layer.
    return vec4<f32>(composed.rgb / max(composed.a, 0.000001), composed.a * in.opacity);
}
