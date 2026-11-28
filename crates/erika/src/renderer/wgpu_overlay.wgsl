// WGSL port of the Metal overlay shader in `renderer/metal/apple.rs`. Draws a
// textured quad placed by pixel rect within the viewport, alpha-blended over the
// video plane. Mode 0 samples straight RGBA; mode 1 is an alpha mask tinted by
// `color` (libass coverage bitmaps).

struct OverlayUniforms {
    rect: vec4<f32>,
    tex_rect: vec4<f32>,
    viewport: vec2<f32>,
    overlay_mode: u32,
    output_encoding: u32,
    color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: OverlayUniforms;
@group(0) @binding(1) var overlay_texture: texture_2d<f32>;
@group(0) @binding(2) var overlay_sampler: sampler;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
};

@vertex
fn erika_overlay_vertex(@builtin(vertex_index) vertex_id: u32) -> VertexOut {
    // Keep this array-free for SwiftShader GLES 3.0; dynamically indexing a
    // function-local vec2 array with vertex_index can silently produce no
    // rasterized primitives on the Android emulator.
    let unit = vec2<f32>(
        f32(vertex_id & 1u),
        f32((vertex_id >> 1u) & 1u),
    );

    let pixel = uniforms.rect.xy + unit * uniforms.rect.zw;
    let ndc = vec2<f32>(
        pixel.x / max(uniforms.viewport.x, 1.0) * 2.0 - 1.0,
        1.0 - pixel.y / max(uniforms.viewport.y, 1.0) * 2.0,
    );

    return VertexOut(
        vec4<f32>(ndc, 0.0, 1.0),
        uniforms.tex_rect.xy + unit * uniforms.tex_rect.zw,
    );
}

@fragment
fn erika_overlay_fragment(in: VertexOut) -> @location(0) vec4<f32> {
    let sampled = textureSample(overlay_texture, overlay_sampler, in.tex_coord);
    var rgb = select(
        sampled.rgb,
        pow(max(sampled.rgb, vec3<f32>(0.0)), vec3<f32>(2.2)),
        uniforms.output_encoding == 1u,
    );
    if (uniforms.overlay_mode == 1u) {
        rgb = select(
            uniforms.color.rgb,
            pow(max(uniforms.color.rgb, vec3<f32>(0.0)), vec3<f32>(2.2)),
            uniforms.output_encoding == 1u,
        );
        return vec4<f32>(rgb, uniforms.color.a * sampled.r);
    }
    if (uniforms.overlay_mode == 2u) {
        // Premultiplied, so the filter does not darken an emoji's edges.
        let straight = sampled.rgb / max(sampled.a, 0.0001);
        rgb = select(
            straight,
            pow(max(straight, vec3<f32>(0.0)), vec3<f32>(2.2)),
            uniforms.output_encoding == 1u,
        );
        return vec4<f32>(rgb, uniforms.color.a * sampled.a);
    }
    return vec4<f32>(rgb, sampled.a);
}
