// Portable scalar ArtCNN C4 compute kernels.
//
// `{FEATURE_SLICES}` is replaced with F / 4 (4 for C4F16, 8 for C4F32).
// Feature maps are tile-local RGBA16Float arrays. The final texture remains at
// source resolution: each RGBA texel stores the DCR 2x2 luma block in
// (top-left, top-right, bottom-left, bottom-right) order.

const FEATURE_SLICES: u32 = {FEATURE_SLICES}u;
const TAP_COUNT: u32 = 9u;

struct WeightWords {
    values: array<u32>,
};

struct TileParams {
    // image width, image height, clipped tile-core width, clipped tile-core height
    image_core: vec4<u32>,
    // core origin x/y, feature-domain origin x/y (core origin - radius 6)
    origins: vec4<i32>,
    // local inset, dispatch width, dispatch height, feature slice count
    dispatch: vec4<u32>,
    // weight offset, bias offset (both in half4 units), relu, add residual
    layer: vec4<u32>,
    // RGB -> Y, or planar channel weights (R normally, RG for P010 byte pairs).
    luma_coefficients: vec4<f32>,
};

@group(1) @binding(0) var<storage, read> weight_words: WeightWords;
@group(1) @binding(1) var<uniform> params: TileParams;

// conv0 resources
@group(0) @binding(0) var conv0_luma: texture_2d<f32>;
@group(0) @binding(1) var conv0_dst: texture_storage_2d_array<rgba16float, write>;
@group(0) @binding(7) var conv0_rgb: texture_2d<f32>;

// conv1..conv5 resources
@group(0) @binding(2) var mid_src: texture_2d_array<f32>;
@group(0) @binding(3) var mid_dst: texture_storage_2d_array<rgba16float, write>;
@group(0) @binding(4) var mid_residual: texture_2d_array<f32>;

// conv6 / packed DepthToSpace resources
@group(0) @binding(5) var conv6_src: texture_2d_array<f32>;
@group(0) @binding(6) var conv6_dst: texture_storage_2d<rgba16float, write>;

fn load_half4(index: u32) -> vec4<f32> {
    let word = index * 2u;
    return vec4<f32>(
        unpack2x16float(weight_words.values[word]),
        unpack2x16float(weight_words.values[word + 1u]),
    );
}

fn load_half4x4(index: u32) -> mat4x4<f32> {
    // The blob is column-major: each half4 is one input-channel column and
    // contains the four output-channel coefficients.
    return mat4x4<f32>(
        load_half4(index),
        load_half4(index + 1u),
        load_half4(index + 2u),
        load_half4(index + 3u),
    );
}

fn in_image(coord: vec2<i32>) -> bool {
    return coord.x >= 0
        && coord.y >= 0
        && coord.x < i32(params.image_core.x)
        && coord.y < i32(params.image_core.y);
}

fn local_coord(gid: vec3<u32>) -> vec2<i32> {
    let inset = i32(params.dispatch.x);
    return vec2<i32>(i32(gid.x) + inset, i32(gid.y) + inset);
}

fn global_coord(local: vec2<i32>) -> vec2<i32> {
    return params.origins.zw + local;
}

fn invocation_is_active(gid: vec3<u32>) -> bool {
    return gid.x < params.dispatch.y && gid.y < params.dispatch.z;
}

fn load_luma_or_zero(coord: vec2<i32>) -> f32 {
    if (!in_image(coord)) {
        return 0.0;
    }
    return dot(textureLoad(conv0_luma, coord, 0).rgb, params.luma_coefficients.xyz);
}

fn load_rgb_luma_or_zero(coord: vec2<i32>) -> f32 {
    if (!in_image(coord)) {
        return 0.0;
    }
    let rgb = textureLoad(conv0_rgb, coord, 0).rgb;
    return dot(rgb, params.luma_coefficients.xyz);
}

@compute @workgroup_size(8, 8, 1)
fn artcnn_conv0(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!invocation_is_active(gid)) {
        return;
    }
    let local = local_coord(gid);
    let global = global_coord(local);
    if (!in_image(global)) {
        for (var slice = 0u; slice < FEATURE_SLICES; slice++) {
            textureStore(conv0_dst, local, i32(slice), vec4<f32>(0.0));
        }
        return;
    }

    for (var slice = 0u; slice < FEATURE_SLICES; slice++) {
        var acc = load_half4(params.layer.y + slice);
        for (var tap = 0u; tap < TAP_COUNT; tap++) {
            let delta = vec2<i32>(i32(tap % 3u) - 1, i32(tap / 3u) - 1);
            let value = load_luma_or_zero(global + delta);
            let weight = load_half4(params.layer.x + slice * TAP_COUNT + tap);
            acc += weight * value;
        }
        textureStore(conv0_dst, local, i32(slice), acc);
    }
}

@compute @workgroup_size(8, 8, 1)
fn artcnn_conv0_rgb(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!invocation_is_active(gid)) {
        return;
    }
    let local = local_coord(gid);
    let global = global_coord(local);
    if (!in_image(global)) {
        for (var slice = 0u; slice < FEATURE_SLICES; slice++) {
            textureStore(conv0_dst, local, i32(slice), vec4<f32>(0.0));
        }
        return;
    }

    for (var slice = 0u; slice < FEATURE_SLICES; slice++) {
        var acc = load_half4(params.layer.y + slice);
        for (var tap = 0u; tap < TAP_COUNT; tap++) {
            let delta = vec2<i32>(i32(tap % 3u) - 1, i32(tap / 3u) - 1);
            let value = load_rgb_luma_or_zero(global + delta);
            let weight = load_half4(params.layer.x + slice * TAP_COUNT + tap);
            acc += weight * value;
        }
        textureStore(conv0_dst, local, i32(slice), acc);
    }
}

@compute @workgroup_size(8, 8, 1)
fn artcnn_conv_mid(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!invocation_is_active(gid)) {
        return;
    }
    let local = local_coord(gid);
    let global = global_coord(local);
    if (!in_image(global)) {
        for (var slice = 0u; slice < FEATURE_SLICES; slice++) {
            textureStore(mid_dst, local, i32(slice), vec4<f32>(0.0));
        }
        return;
    }

    var acc: array<vec4<f32>, FEATURE_SLICES>;
    for (var out_slice = 0u; out_slice < FEATURE_SLICES; out_slice++) {
        acc[out_slice] = load_half4(params.layer.y + out_slice);
    }

    for (var in_slice = 0u; in_slice < FEATURE_SLICES; in_slice++) {
        for (var tap = 0u; tap < TAP_COUNT; tap++) {
            let delta = vec2<i32>(i32(tap % 3u) - 1, i32(tap / 3u) - 1);
            let value = textureLoad(mid_src, local + delta, i32(in_slice), 0);
            for (var out_slice = 0u; out_slice < FEATURE_SLICES; out_slice++) {
                let matrix_index = params.layer.x
                    + (((out_slice * TAP_COUNT + tap) * FEATURE_SLICES + in_slice) * 4u);
                acc[out_slice] += load_half4x4(matrix_index) * value;
            }
        }
    }

    for (var out_slice = 0u; out_slice < FEATURE_SLICES; out_slice++) {
        var value = acc[out_slice];
        if (params.layer.w != 0u) {
            value += textureLoad(mid_residual, local, i32(out_slice), 0);
        }
        if (params.layer.z != 0u) {
            value = max(value, vec4<f32>(0.0));
        }
        textureStore(mid_dst, local, i32(out_slice), value);
    }
}

@compute @workgroup_size(8, 8, 1)
fn artcnn_conv6(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (!invocation_is_active(gid)) {
        return;
    }
    let local = local_coord(gid);
    let global = global_coord(local);
    if (!in_image(global)) {
        return;
    }

    var acc = load_half4(params.layer.y);
    for (var in_slice = 0u; in_slice < FEATURE_SLICES; in_slice++) {
        for (var tap = 0u; tap < TAP_COUNT; tap++) {
            let delta = vec2<i32>(i32(tap % 3u) - 1, i32(tap / 3u) - 1);
            let value = textureLoad(conv6_src, local + delta, i32(in_slice), 0);
            let matrix_index = params.layer.x
                + ((tap * FEATURE_SLICES + in_slice) * 4u);
            acc += load_half4x4(matrix_index) * value;
        }
    }

    // Packed DCR order: R=TL, G=TR, B=BL, A=BR. Keeping this packed avoids a
    // 2W x 2H allocation while retaining fp16 precision for both NV12 and P010.
    textureStore(conv6_dst, global, clamp(acc, vec4<f32>(0.0), vec4<f32>(1.0)));
}
