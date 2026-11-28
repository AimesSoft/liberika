// WebGPU presentation + GPU-compute super-resolution (ArtCNN, Anime4K, FSR 1.0,
// and custom .esr / mpv hook shaders).
//
// VideoFrame stays on the GPU via copyExternalImageToTexture. Video, libass
// subtitles, single-draw instanced danmaku, and color emoji composite in one
// render pass; neural/spatial SR runs in a cached compute pass ahead of it.

const FRAME_UNIFORM_BYTES = 48;
const SUBTITLE_UNIFORM_BYTES = 32;
const SR_PARAMS_BYTES = 1104;
const DANMAKU_INSTANCE_FLOATS = 20; // 80 bytes per comment group
const DANMAKU_INSTANCE_BYTES = DANMAKU_INSTANCE_FLOATS * 4;
const BT709_LUMA = [0.2126, 0.7152, 0.0722];

const BUF = globalThis.GPUBufferUsage ?? {
  COPY_DST: 0x0008,
  VERTEX: 0x0020,
  UNIFORM: 0x0040,
  STORAGE: 0x0080,
};
const TEX = globalThis.GPUTextureUsage ?? {
  COPY_DST: 0x02,
  TEXTURE_BINDING: 0x04,
  STORAGE_BINDING: 0x08,
  RENDER_ATTACHMENT: 0x10,
};
const STAGE = globalThis.GPUShaderStage ?? {
  VERTEX: 0x1,
  FRAGMENT: 0x2,
  COMPUTE: 0x4,
};

const VIDEO_WGSL = /* wgsl */ `
struct FrameUniforms {
  resolution: vec2<f32>,
  sr_mode: u32,
  _pad0: u32,
  luma_coeff: vec4<f32>,
  canvas_resolution: vec2<f32>,
  _pad1: vec2<f32>,
};
@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(0) @binding(1) var video_tex: texture_2d<f32>;
@group(0) @binding(2) var sr_tex: texture_2d<f32>;
@group(0) @binding(3) var video_sampler: sampler;

struct VertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VertexOut {
  let unit = vec2<f32>(f32(vid & 1u), f32((vid >> 1u) & 1u));
  let ndc = vec2<f32>(unit.x * 2.0 - 1.0, 1.0 - unit.y * 2.0);
  return VertexOut(vec4<f32>(ndc, 0.0, 1.0), unit);
}

fn packed_luma_texel(virtual_coord_in: vec2<i32>, virtual_size: vec2<i32>) -> f32 {
  let virtual_coord = clamp(virtual_coord_in, vec2<i32>(0), virtual_size - vec2<i32>(1));
  let packed_coord = virtual_coord / vec2<i32>(2);
  let packed = textureLoad(sr_tex, packed_coord, 0);
  let component = u32((virtual_coord.y & 1) * 2 + (virtual_coord.x & 1));
  if (component == 0u) { return packed.r; }
  if (component == 1u) { return packed.g; }
  if (component == 2u) { return packed.b; }
  return packed.a;
}

fn sample_packed_luma(tex_coord: vec2<f32>) -> f32 {
  let packed_size = vec2<i32>(textureDimensions(sr_tex, 0));
  let virtual_size = packed_size * vec2<i32>(2);
  let sample_pos = clamp(tex_coord, vec2<f32>(0.0), vec2<f32>(1.0))
      * vec2<f32>(virtual_size) - vec2<f32>(0.5);
  let lo = vec2<i32>(floor(sample_pos));
  let frac = fract(sample_pos);
  let y0 = mix(
    packed_luma_texel(lo, virtual_size),
    packed_luma_texel(lo + vec2<i32>(1, 0), virtual_size),
    frac.x,
  );
  let y1 = mix(
    packed_luma_texel(lo + vec2<i32>(0, 1), virtual_size),
    packed_luma_texel(lo + vec2<i32>(1, 1), virtual_size),
    frac.x,
  );
  return mix(y0, y1, frac.y);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
  let original_rgb = textureSampleLevel(video_tex, video_sampler, in.uv, 0.0).rgb;
  if (frame.sr_mode == 0u) {
    return vec4<f32>(original_rgb, 1.0);
  }
  let original_luma = dot(frame.luma_coeff.xyz, original_rgb);
  var enhanced_luma: f32;
  if (frame.sr_mode == 1u) {
    enhanced_luma = sample_packed_luma(in.uv);
  } else {
    enhanced_luma = textureSampleLevel(sr_tex, video_sampler, in.uv, 0.0).r;
  }
  let rgb = clamp(
    original_rgb + vec3<f32>(enhanced_luma - original_luma),
    vec3<f32>(0.0),
    vec3<f32>(1.0),
  );
  return vec4<f32>(rgb, 1.0);
}
`;

const SUBTITLE_WGSL = /* wgsl */ `
struct FrameUniforms {
  resolution: vec2<f32>,
  sr_mode: u32,
  _pad0: u32,
  luma_coeff: vec4<f32>,
  canvas_resolution: vec2<f32>,
  _pad1: vec2<f32>,
};
struct SubUniforms {
  rect: vec4<f32>,
  color: vec4<f32>,
};
@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var<uniform> sub: SubUniforms;
@group(1) @binding(1) var mask_tex: texture_2d<f32>;
@group(1) @binding(2) var mask_sampler: sampler;

struct VertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VertexOut {
  let unit = vec2<f32>(f32(vid & 1u), f32((vid >> 1u) & 1u));
  let p = sub.rect.xy + unit * sub.rect.zw;
  let res = max(frame.resolution, vec2<f32>(1.0));
  let ndc = vec2<f32>(p.x / res.x * 2.0 - 1.0, 1.0 - p.y / res.y * 2.0);
  return VertexOut(vec4<f32>(ndc, 0.0, 1.0), unit);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
  let m = textureSampleLevel(mask_tex, mask_sampler, in.uv, 0.0).r;
  let a = sub.color.a * m;
  return vec4<f32>(sub.color.rgb * a, a);
}
`;

// Single instanced draw for all comment groups: unions fill/outline/shadow
// masks per comment in-register, then applies comment opacity once.
const DANMAKU_WGSL = /* wgsl */ `
struct FrameUniforms {
  resolution: vec2<f32>,
  sr_mode: u32,
  _pad0: u32,
  luma_coeff: vec4<f32>,
  canvas_resolution: vec2<f32>,
  _pad1: vec2<f32>,
};
@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var<storage, read> glyphs: array<vec4<f32>>;
@group(1) @binding(1) var fill_tex: texture_2d<f32>;
@group(1) @binding(2) var outline_tex: texture_2d<f32>;
@group(1) @binding(3) var mask_sampler: sampler;

struct VertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) pixel: vec2<f32>,
  @location(1) @interpolate(flat) fill_color: vec4<f32>,
  @location(2) @interpolate(flat) outline_color: vec4<f32>,
  @location(3) @interpolate(flat) shadow: vec4<f32>,
  @location(4) @interpolate(flat) glyph_range: vec2<u32>,
};

@vertex
fn vs_main(
  @builtin(vertex_index) vid: u32,
  @location(0) rect: vec4<f32>,
  @location(1) fill_color: vec4<f32>,
  @location(2) outline_color: vec4<f32>,
  @location(3) shadow: vec4<f32>,
  @location(4) glyph_range: vec2<u32>,
) -> VertexOut {
  let unit = vec2<f32>(f32(vid & 1u), f32((vid >> 1u) & 1u));
  let pixel = rect.xy + unit * rect.zw;
  let res = max(frame.canvas_resolution, vec2<f32>(1.0));
  let ndc = vec2<f32>(pixel.x / res.x * 2.0 - 1.0, 1.0 - pixel.y / res.y * 2.0);
  return VertexOut(
    vec4<f32>(ndc, 0.0, 1.0),
    pixel,
    fill_color,
    outline_color,
    shadow,
    glyph_range,
  );
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
  var max_fill = 0.0;
  var max_outline = 0.0;
  var max_shadow = 0.0;
  let end_idx = in.glyph_range.x + in.glyph_range.y;
  for (var g = in.glyph_range.x; g < end_idx; g += 1u) {
    let box_r = glyphs[g * 2u];
    let tex_r = glyphs[g * 2u + 1u];
    if (box_r.z <= 0.0 || box_r.w <= 0.0) {
      continue;
    }
    let local = (in.pixel - box_r.xy) / box_r.zw;
    if (all(local >= vec2<f32>(0.0)) && all(local <= vec2<f32>(1.0))) {
      let uv = tex_r.xy + local * tex_r.zw;
      max_fill = max(max_fill, textureSampleLevel(fill_tex, mask_sampler, uv, 0.0).r);
      max_outline = max(max_outline, textureSampleLevel(outline_tex, mask_sampler, uv, 0.0).r);
    }
    let shadow_local = (in.pixel - (box_r.xy + in.shadow.xy)) / box_r.zw;
    if (all(shadow_local >= vec2<f32>(0.0)) && all(shadow_local <= vec2<f32>(1.0))) {
      let suv = tex_r.xy + shadow_local * tex_r.zw;
      max_shadow = max(max_shadow, textureSampleLevel(outline_tex, mask_sampler, suv, 0.0).r);
    }
  }
  let s = max_shadow * in.shadow.z;
  let a = max_fill + (1.0 - max_fill) * (max_outline + (1.0 - max_outline) * s);
  let rgb = in.fill_color.rgb * max_fill + in.outline_color.rgb * max_outline * (1.0 - max_fill);
  return vec4<f32>(rgb, a) * in.fill_color.a;
}
`;

const PICTURE_WGSL = /* wgsl */ `
struct FrameUniforms {
  resolution: vec2<f32>,
  sr_mode: u32,
  _pad0: u32,
  luma_coeff: vec4<f32>,
  canvas_resolution: vec2<f32>,
  _pad1: vec2<f32>,
};
@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var emoji_tex: texture_2d<f32>;
@group(1) @binding(1) var emoji_sampler: sampler;

struct VertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) @interpolate(flat) opacity: f32,
};

@vertex
fn vs_main(
  @builtin(vertex_index) vid: u32,
  @location(0) rect: vec4<f32>,
  @location(1) tex_rect: vec4<f32>,
  @location(2) opacity: f32,
) -> VertexOut {
  let unit = vec2<f32>(f32(vid & 1u), f32((vid >> 1u) & 1u));
  let p = rect.xy + unit * rect.zw;
  let res = max(frame.canvas_resolution, vec2<f32>(1.0));
  let ndc = vec2<f32>(p.x / res.x * 2.0 - 1.0, 1.0 - p.y / res.y * 2.0);
  return VertexOut(
    vec4<f32>(ndc, 0.0, 1.0),
    tex_rect.xy + unit * tex_rect.zw,
    opacity,
  );
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
  return textureSampleLevel(emoji_tex, emoji_sampler, in.uv, 0.0) * in.opacity;
}
`;

const PREMULTIPLIED_BLEND = {
  color: {
    srcFactor: "one",
    dstFactor: "one-minus-src-alpha",
    operation: "add",
  },
  alpha: {
    srcFactor: "one",
    dstFactor: "one-minus-src-alpha",
    operation: "add",
  },
};

function deviceCapsFrom(device) {
  const limits = device.limits || {};
  return {
    shaderF16: Boolean(device.features?.has?.("shader-f16")),
    maxComputeWorkgroupStorageSize:
      limits.maxComputeWorkgroupStorageSize ?? 16384,
    maxUniformBufferBindingSize: limits.maxUniformBufferBindingSize ?? 65536,
    maxStorageBufferBindingSize:
      limits.maxStorageBufferBindingSize ?? 128 * 1024 * 1024,
    maxBufferSize: limits.maxBufferSize ?? 256 * 1024 * 1024,
    maxComputeWorkgroupSizeX: limits.maxComputeWorkgroupSizeX ?? 256,
    maxComputeWorkgroupSizeY: limits.maxComputeWorkgroupSizeY ?? 256,
    maxComputeInvocationsPerWorkgroup:
      limits.maxComputeInvocationsPerWorkgroup ?? 256,
    maxTextureDimension2D: limits.maxTextureDimension2D ?? 8192,
    minUniformBufferOffsetAlignment:
      limits.minUniformBufferOffsetAlignment ?? 256,
    maxStorageBuffersPerShaderStage:
      limits.maxStorageBuffersPerShaderStage ?? 8,
  };
}

function requestDeviceDescriptor(adapter) {
  const requiredFeatures = [];
  if (adapter.features?.has?.("shader-f16")) {
    requiredFeatures.push("shader-f16");
  }
  const requiredLimits = {};
  const adapterLimits = adapter.limits;
  if (adapterLimits) {
    if (adapterLimits.maxComputeWorkgroupStorageSize > 16384) {
      requiredLimits.maxComputeWorkgroupStorageSize = Math.min(
        adapterLimits.maxComputeWorkgroupStorageSize,
        32768,
      );
    }
    if (adapterLimits.maxStorageBufferBindingSize > 128 * 1024 * 1024) {
      requiredLimits.maxStorageBufferBindingSize = Math.min(
        adapterLimits.maxStorageBufferBindingSize,
        256 * 1024 * 1024,
      );
    }
    if (adapterLimits.maxBufferSize > 256 * 1024 * 1024) {
      requiredLimits.maxBufferSize = Math.min(
        adapterLimits.maxBufferSize,
        256 * 1024 * 1024,
      );
    }
  }
  const desc = {};
  if (requiredFeatures.length) desc.requiredFeatures = requiredFeatures;
  if (Object.keys(requiredLimits).length) desc.requiredLimits = requiredLimits;
  return desc;
}

export class WebGpuRenderer {
  /**
   * Probes WebGPU and initializes device/pipelines BEFORE calling
   * `canvas.getContext("webgpu")` so fallback to WebGL2 leaves the canvas
   * untouched whenever WebGPU is unavailable.
   */
  static async create(
    canvas,
    {
      srPlannerFactory,
      availableModes,
      onDeviceLost,
      gpu = globalThis.navigator?.gpu,
    } = {},
  ) {
    if (!gpu?.requestAdapter) return null;
    let adapter;
    try {
      adapter =
        (await gpu.requestAdapter({ powerPreference: "high-performance" })) ||
        (await gpu.requestAdapter());
    } catch {
      return null;
    }
    if (!adapter) return null;

    let device;
    try {
      device = await adapter.requestDevice(requestDeviceDescriptor(adapter));
    } catch {
      try {
        device = await adapter.requestDevice();
      } catch {
        return null;
      }
    }
    if (!device) return null;

    const format = gpu.getPreferredCanvasFormat?.() || "bgra8unorm";
    const caps = deviceCapsFrom(device);
    let srPlanner = null;
    if (srPlannerFactory) {
      try {
        srPlanner = srPlannerFactory(JSON.stringify(caps));
      } catch {
        srPlanner = null;
      }
    }

    const renderer = new WebGpuRenderer(
      canvas,
      device,
      format,
      caps,
      srPlanner,
      availableModes || [],
      onDeviceLost,
    );

    // Compile and validate every presentation shader/pipeline inside explicit
    // error scopes BEFORE claiming canvas.getContext("webgpu"), so fallback to
    // WebGL2 still has an untouched canvas if shader or pipeline creation fails.
    device.pushErrorScope?.("out-of-memory");
    device.pushErrorScope?.("internal");
    device.pushErrorScope?.("validation");
    let initError = null;
    try {
      await renderer.initPipelines();
    } catch (error) {
      initError = error;
    }
    const scopeErrors = [];
    if (device.popErrorScope) {
      for (let i = 0; i < 3; i++) {
        const err = await device.popErrorScope();
        if (err) scopeErrors.push(err.message || String(err));
      }
    }
    const finalInitError =
      initError ||
      (scopeErrors.length ? new Error(scopeErrors.join("; ")) : null) ||
      (renderer.lost
        ? new Error("WebGPU device lost during initialization")
        : null);
    if (finalInitError) {
      renderer.destroy();
      throw finalInitError;
    }

    // Acquire canvas context ONLY after device and pipelines succeeded.
    const context = canvas.getContext("webgpu");
    if (!context) {
      renderer.destroy();
      return null;
    }
    try {
      context.configure({
        device,
        format,
        alphaMode: "opaque",
      });
    } catch (error) {
      renderer.destroy();
      // Once getContext("webgpu") returns a context, the canvas is permanently
      // bound to WebGPU and cannot obtain a WebGL2 context anymore.
      const err = new Error(
        `WebGPU canvas configuration failed: ${error?.message || error}`,
      );
      err.canvasContextClaimed = true;
      throw err;
    }
    renderer.context = context;
    renderer.ready = true;
    return renderer;
  }

  constructor(
    canvas,
    device,
    format,
    caps,
    srPlanner,
    availableModes,
    onDeviceLost = null,
  ) {
    this.backend = "webgpu";
    this.canvas = canvas;
    this.device = device;
    this.format = format;
    this.caps = caps;
    this.srPlanner = srPlanner;
    this.availableModes = availableModes;
    this.onDeviceLost = onDeviceLost;
    this.context = null;
    this.ready = false;
    this.disposed = false;
    this.lost = false;
    device.lost?.then?.((info) => {
      if (this.disposed) return;
      this.lost = true;
      const reason = info?.reason ? ` (${info.reason})` : "";
      const detail = info?.message ? `: ${info.message}` : "";
      const message = `WebGPU device lost${reason}${detail}`;
      this.srActive = null;
      this.srBackend = "inactive";
      this.srLastError = message;
      this.srLastCacheKey = "";
      this.srFramePlanKey = "";
      this.srBindGroups = null;
      if (this.ready) {
        this.onDeviceLost?.({
          reason: info?.reason ?? "unknown",
          message,
        });
      }
    });

    this.width = 1;
    this.height = 1;
    this.x = 0;
    this.y = 0;
    this.videoWidth = 0;
    this.videoHeight = 0;
    this.hasVideo = false;
    this.videoFrameSerial = 0;

    this.uploads = 0;
    this.frames = 0;
    this.glyphs = 0;
    this.subtitleImages = [];

    this.encoder = null;
    this.pass = null;
    this.frameUniformData = new ArrayBuffer(FRAME_UNIFORM_BYTES);
    this.frameUniformF32 = new Float32Array(this.frameUniformData);
    this.frameUniformU32 = new Uint32Array(this.frameUniformData);

    // Super-resolution state.
    this.srRequestedMode = "off";
    this.srBackend = "off";
    this.srActive = null;
    this.srCache = new Map();
    this.srEpoch = 0;
    this.srCustomGeneration = 0;
    this.srFallbackCount = 0;
    this.srUpscaledFrames = 0;
    this.srLastError = null;
    this.srLastCacheKey = "";
    this.srFramePlanKey = "";
    this.srFramePlan = null;
    this.srArenaBuffer = null;
    this.srArenaCapacity = 0;
    this.srParamsBuffer = null;
    this.srParamsCapacity = 0;
    this.srOutputTexture = null;
    this.srOutputView = null;
    this.srOutputWidth = 0;
    this.srOutputHeight = 0;
    this.srBindGroups = null;
    this.srBindGroupGeneration = 0;
    this.videoBindGroup = null;
    this.danmakuBindGroup = null;
    this.pictureBindGroup = null;

    // Grow-only GPU buffers.
    this.glyphBuffer = null;
    this.glyphCapacity = 0;
    this.groupBuffer = null;
    this.groupCapacity = 0;
    this.groupScratch = new ArrayBuffer(0);
    this.groupScratchF32 = new Float32Array(0);
    this.groupScratchU32 = new Uint32Array(0);
    this.pictureBuffer = null;
    this.pictureCapacity = 0;

    this.fillWidth = 0;
    this.fillHeight = 0;
    this.emojiWidth = 0;
    this.emojiHeight = 0;
  }

  async createCheckedShaderModule(code) {
    const module = this.device.createShaderModule({ code });
    if (typeof module?.getCompilationInfo === "function") {
      const info = await module.getCompilationInfo();
      const errors = (info?.messages || []).filter((m) => m.type === "error");
      if (errors.length) {
        throw new Error(errors.map((m) => m.message).join("; "));
      }
    }
    return module;
  }

  async initPipelines() {
    const d = this.device;
    const createRenderPipeline = (desc) =>
      d.createRenderPipelineAsync
        ? d.createRenderPipelineAsync(desc)
        : d.createRenderPipeline(desc);
    this.linearSampler = d.createSampler({
      magFilter: "linear",
      minFilter: "linear",
      addressModeU: "clamp-to-edge",
      addressModeV: "clamp-to-edge",
    });
    this.frameUniformBuffer = d.createBuffer({
      size: FRAME_UNIFORM_BYTES,
      usage: BUF.UNIFORM | BUF.COPY_DST,
    });

    // Dummy 1x1 textures keep bind groups valid before first upload / without SR.
    this.dummyRgbaTexture = this.makeTexture(
      1,
      1,
      "rgba8unorm",
      TEX.TEXTURE_BINDING | TEX.COPY_DST,
    );
    this.dummyR8Texture = this.makeTexture(
      1,
      1,
      "r8unorm",
      TEX.TEXTURE_BINDING | TEX.COPY_DST,
    );
    this.dummySrTexture = this.makeTexture(
      1,
      1,
      "rgba16float",
      TEX.TEXTURE_BINDING | TEX.STORAGE_BINDING,
    );
    this.videoTexture = this.dummyRgbaTexture;
    this.videoView = this.videoTexture.createView();
    this.fillTexture = this.dummyR8Texture;
    this.fillView = this.fillTexture.createView();
    this.outlineTexture = this.dummyR8Texture;
    this.outlineView = this.outlineTexture.createView();
    this.emojiTexture = this.dummyRgbaTexture;
    this.emojiView = this.emojiTexture.createView();
    this.srDummyView = this.dummySrTexture.createView();

    this.frameBgl = d.createBindGroupLayout({
      entries: [
        {
          binding: 0,
          visibility: STAGE.VERTEX | STAGE.FRAGMENT,
          buffer: { type: "uniform" },
        },
      ],
    });
    this.frameBindGroup = d.createBindGroup({
      layout: this.frameBgl,
      entries: [{ binding: 0, resource: { buffer: this.frameUniformBuffer } }],
    });

    // 1. Video + SR recombination pipeline
    this.videoBgl = d.createBindGroupLayout({
      entries: [
        {
          binding: 0,
          visibility: STAGE.VERTEX | STAGE.FRAGMENT,
          buffer: { type: "uniform" },
        },
        {
          binding: 1,
          visibility: STAGE.FRAGMENT,
          texture: { sampleType: "float" },
        },
        {
          binding: 2,
          visibility: STAGE.FRAGMENT,
          texture: { sampleType: "float" },
        },
        {
          binding: 3,
          visibility: STAGE.FRAGMENT,
          sampler: { type: "filtering" },
        },
      ],
    });
    const videoModule = await this.createCheckedShaderModule(VIDEO_WGSL);
    this.videoPipeline = await createRenderPipeline({
      layout: d.createPipelineLayout({ bindGroupLayouts: [this.videoBgl] }),
      vertex: { module: videoModule, entryPoint: "vs_main" },
      primitive: { topology: "triangle-strip" },
      fragment: {
        module: videoModule,
        entryPoint: "fs_main",
        targets: [{ format: this.format }],
      },
    });

    // 2. Subtitle pipeline
    this.subtitleBgl = d.createBindGroupLayout({
      entries: [
        {
          binding: 0,
          visibility: STAGE.VERTEX | STAGE.FRAGMENT,
          buffer: { type: "uniform" },
        },
        {
          binding: 1,
          visibility: STAGE.FRAGMENT,
          texture: { sampleType: "float" },
        },
        {
          binding: 2,
          visibility: STAGE.FRAGMENT,
          sampler: { type: "filtering" },
        },
      ],
    });
    const subModule = await this.createCheckedShaderModule(SUBTITLE_WGSL);
    this.subtitlePipeline = await createRenderPipeline({
      layout: d.createPipelineLayout({
        bindGroupLayouts: [this.frameBgl, this.subtitleBgl],
      }),
      vertex: { module: subModule, entryPoint: "vs_main" },
      primitive: { topology: "triangle-strip" },
      fragment: {
        module: subModule,
        entryPoint: "fs_main",
        targets: [{ format: this.format, blend: PREMULTIPLIED_BLEND }],
      },
    });

    // 3. Instanced danmaku pipeline
    this.danmakuBgl = d.createBindGroupLayout({
      entries: [
        {
          binding: 0,
          visibility: STAGE.FRAGMENT,
          buffer: { type: "read-only-storage" },
        },
        {
          binding: 1,
          visibility: STAGE.FRAGMENT,
          texture: { sampleType: "float" },
        },
        {
          binding: 2,
          visibility: STAGE.FRAGMENT,
          texture: { sampleType: "float" },
        },
        {
          binding: 3,
          visibility: STAGE.FRAGMENT,
          sampler: { type: "filtering" },
        },
      ],
    });
    const danmakuModule = await this.createCheckedShaderModule(DANMAKU_WGSL);
    this.danmakuPipeline = await createRenderPipeline({
      layout: d.createPipelineLayout({
        bindGroupLayouts: [this.frameBgl, this.danmakuBgl],
      }),
      vertex: {
        module: danmakuModule,
        entryPoint: "vs_main",
        buffers: [
          {
            arrayStride: DANMAKU_INSTANCE_BYTES,
            stepMode: "instance",
            attributes: [
              { shaderLocation: 0, offset: 0, format: "float32x4" },
              { shaderLocation: 1, offset: 16, format: "float32x4" },
              { shaderLocation: 2, offset: 32, format: "float32x4" },
              { shaderLocation: 3, offset: 48, format: "float32x4" },
              { shaderLocation: 4, offset: 64, format: "uint32x2" },
            ],
          },
        ],
      },
      primitive: { topology: "triangle-strip" },
      fragment: {
        module: danmakuModule,
        entryPoint: "fs_main",
        targets: [{ format: this.format, blend: PREMULTIPLIED_BLEND }],
      },
    });

    // 4. Instanced color emoji picture pipeline
    this.pictureBgl = d.createBindGroupLayout({
      entries: [
        {
          binding: 0,
          visibility: STAGE.FRAGMENT,
          texture: { sampleType: "float" },
        },
        {
          binding: 1,
          visibility: STAGE.FRAGMENT,
          sampler: { type: "filtering" },
        },
      ],
    });
    const pictureModule = await this.createCheckedShaderModule(PICTURE_WGSL);
    this.picturePipeline = await createRenderPipeline({
      layout: d.createPipelineLayout({
        bindGroupLayouts: [this.frameBgl, this.pictureBgl],
      }),
      vertex: {
        module: pictureModule,
        entryPoint: "vs_main",
        buffers: [
          {
            arrayStride: 36,
            stepMode: "instance",
            attributes: [
              { shaderLocation: 0, offset: 0, format: "float32x4" },
              { shaderLocation: 1, offset: 16, format: "float32x4" },
              { shaderLocation: 2, offset: 32, format: "float32" },
            ],
          },
        ],
      },
      primitive: { topology: "triangle-strip" },
      fragment: {
        module: pictureModule,
        entryPoint: "fs_main",
        targets: [{ format: this.format, blend: PREMULTIPLIED_BLEND }],
      },
    });

    // 5. SR compute bind group layouts (uniform weights vs storage weights)
    const makeSrBgl = (uniformWeights) =>
      d.createBindGroupLayout({
        entries: [
          {
            binding: 0,
            visibility: STAGE.COMPUTE,
            buffer: {
              type: "uniform",
              hasDynamicOffset: true,
              minBindingSize: SR_PARAMS_BYTES,
            },
          },
          {
            binding: 1,
            visibility: STAGE.COMPUTE,
            buffer: { type: "storage" },
          },
          {
            binding: 2,
            visibility: STAGE.COMPUTE,
            buffer: {
              type: uniformWeights ? "uniform" : "read-only-storage",
            },
          },
          {
            binding: 3,
            visibility: STAGE.COMPUTE,
            texture: { sampleType: "unfilterable-float" },
          },
          {
            binding: 4,
            visibility: STAGE.COMPUTE,
            storageTexture: {
              access: "write-only",
              format: "rgba16float",
            },
          },
        ],
      });
    this.srUniformBgl = makeSrBgl(true);
    this.srStorageBgl = makeSrBgl(false);
    this.srUniformPipelineLayout = d.createPipelineLayout({
      bindGroupLayouts: [this.srUniformBgl],
    });
    this.srStoragePipelineLayout = d.createPipelineLayout({
      bindGroupLayouts: [this.srStorageBgl],
    });

    this.pictureBindGroup = d.createBindGroup({
      layout: this.pictureBgl,
      entries: [
        { binding: 0, resource: this.emojiView },
        { binding: 1, resource: this.linearSampler },
      ],
    });
    this.rebuildVideoBindGroup();
  }

  makeTexture(width, height, format, usage) {
    return this.device.createTexture({
      size: { width: Math.max(1, width), height: Math.max(1, height) },
      format,
      usage,
    });
  }

  rebuildVideoBindGroup() {
    this.videoBindGroup = this.device.createBindGroup({
      layout: this.videoBgl,
      entries: [
        { binding: 0, resource: { buffer: this.frameUniformBuffer } },
        { binding: 1, resource: this.videoView },
        { binding: 2, resource: this.srOutputView || this.srDummyView },
        { binding: 3, resource: this.linearSampler },
      ],
    });
  }

  size(w, h, vw, vh) {
    const cw = Math.max(1, Math.round(w));
    const ch = Math.max(1, Math.round(h));
    this.canvas.width = cw;
    this.canvas.height = ch;
    const ratio = Math.min(cw / Math.max(1, vw), ch / Math.max(1, vh));
    this.width = Math.max(1, Math.round(vw * ratio));
    this.height = Math.max(1, Math.round(vh * ratio));
    this.x = Math.floor((cw - this.width) / 2);
    this.y = Math.floor((ch - this.height) / 2);
  }

  upload(frame) {
    if (this.lost) return;
    const w = Math.max(1, frame.displayWidth || frame.codedWidth || 1);
    const h = Math.max(1, frame.displayHeight || frame.codedHeight || 1);
    if (
      this.videoTexture === this.dummyRgbaTexture ||
      this.videoWidth !== w ||
      this.videoHeight !== h
    ) {
      if (this.videoTexture !== this.dummyRgbaTexture) {
        this.videoTexture.destroy();
      }
      this.videoTexture = this.makeTexture(
        w,
        h,
        "rgba8unorm",
        TEX.TEXTURE_BINDING |
          TEX.COPY_DST |
          TEX.RENDER_ATTACHMENT,
      );
      this.videoView = this.videoTexture.createView();
      this.videoWidth = w;
      this.videoHeight = h;
      this.rebuildVideoBindGroup();
      this.srBindGroups = null;
      this.srFramePlanKey = "";
    }
    this.device.queue.copyExternalImageToTexture(
      { source: frame },
      { texture: this.videoTexture },
      { width: w, height: h },
    );
    this.hasVideo = true;
    this.videoFrameSerial++;
  }

  async loadUpscalerModel(name, bytes) {
    if (this.disposed || this.lost) {
      throw new Error("WebGPU device is unavailable");
    }
    if (!this.srPlanner) {
      throw new Error("WebGPU super-resolution planner is unavailable");
    }
    const payload =
      typeof bytes === "string"
        ? new TextEncoder().encode(bytes)
        : bytes instanceof Uint8Array
          ? bytes
          : new Uint8Array(bytes);
    const info = this.srPlanner.loadCustomModel(name || "custom", payload);
    this.srCustomGeneration++;
    // Drop cached custom pipelines, clearing srActive first if it used one.
    for (const [key, entry] of this.srCache) {
      if (key.startsWith("custom:")) {
        if (this.srActive === entry) {
          this.srActive = null;
          this.srLastCacheKey = "";
          this.srFramePlanKey = "";
          this.srBindGroups = null;
        }
        this.disposeSrEntry(entry);
        this.srCache.delete(key);
      }
    }
    if (this.srRequestedMode === "custom") {
      await this.setUpscaler("custom");
    }
    return info;
  }

  clearUpscalerModel() {
    this.srPlanner?.clearCustomModel?.();
    this.srCustomGeneration++;
    for (const [key, entry] of this.srCache) {
      if (key.startsWith("custom:")) {
        if (this.srActive === entry) {
          this.srActive = null;
          this.srLastCacheKey = "";
          this.srFramePlanKey = "";
          this.srBindGroups = null;
        }
        this.disposeSrEntry(entry);
        this.srCache.delete(key);
      }
    }
    if (this.srRequestedMode === "custom") {
      this.srActive = null;
      this.srBackend = "inactive";
      this.srLastCacheKey = "";
      this.srFramePlanKey = "";
      this.srBindGroups = null;
    }
  }

  activateSrEntry(mode, entry) {
    this.srRequestedMode = mode;
    this.srActive = entry;
    this.srBackend = "compute";
    this.srLastCacheKey = "";
    this.srFramePlanKey = "";
    this.srBindGroups = null;
  }

  disposeSrEntry(entry) {
    for (const kernel of entry.kernels) {
      kernel.weightBuffer?.destroy?.();
    }
    if (entry.planId != null) {
      this.srPlanner?.releasePlan?.(entry.planId);
    }
  }

  async setUpscaler(mode) {
    const requested = mode == null ? "off" : String(mode).trim().toLowerCase();
    const epoch = ++this.srEpoch;
    this.srLastError = null;
    if (requested === "off" || requested === "0") {
      this.srRequestedMode = "off";
      this.srBackend = "off";
      this.srActive = null;
      this.srLastCacheKey = "";
      this.srFramePlanKey = "";
      this.srBindGroups = null;
      this.srPlanner?.compile?.("off");
      return this.getUpscalerStatus();
    }
    if (this.disposed || this.lost) {
      this.srRequestedMode = requested;
      this.srBackend = "inactive";
      this.srActive = null;
      this.srLastError = this.srLastError || "WebGPU device is lost";
      throw new Error(this.srLastError);
    }
    if (!this.srPlanner) {
      this.srRequestedMode = requested;
      this.srBackend = "inactive";
      this.srActive = null;
      this.srFallbackCount++;
      this.srLastError = "WebGPU super-resolution planner is unavailable";
      throw new Error(this.srLastError);
    }

    let canonical = requested;
    if (typeof this.srPlanner.constructor?.normalizeMode === "function") {
      try {
        canonical = this.srPlanner.constructor.normalizeMode(requested);
      } catch (error) {
        if (epoch !== this.srEpoch) return this.getUpscalerStatus();
        this.srRequestedMode = requested;
        this.srActive = null;
        this.srBackend = "inactive";
        this.srFallbackCount++;
        this.srLastError = String(error);
        throw error;
      }
      if (canonical === "off") {
        this.srRequestedMode = "off";
        this.srBackend = "off";
        this.srActive = null;
        this.srLastCacheKey = "";
        this.srFramePlanKey = "";
        this.srBindGroups = null;
        this.srPlanner.compile("off");
        return this.getUpscalerStatus();
      }
    }

    const initialCacheKey =
      canonical === "custom"
        ? `custom:${this.srCustomGeneration}`
        : canonical;
    const cached = this.srCache.get(initialCacheKey);
    if (cached) {
      if (epoch !== this.srEpoch || this.disposed || this.lost) {
        return this.getUpscalerStatus();
      }
      // Refresh LRU recency so active pipelines are never evicted first.
      this.srCache.delete(initialCacheKey);
      this.srCache.set(initialCacheKey, cached);
      this.activateSrEntry(cached.mode, cached);
      return this.getUpscalerStatus();
    }

    // Keep any previous srActive rendering smoothly while async shader
    // compilation runs; srActive swaps atomically once compilation succeeds.
    this.srRequestedMode = canonical;
    this.srBackend = "building";

    let compiled;
    try {
      compiled = this.srPlanner.compile(canonical);
    } catch (error) {
      if (epoch !== this.srEpoch || this.disposed || this.lost) {
        return this.getUpscalerStatus();
      }
      this.srActive = null;
      this.srBackend = "inactive";
      this.srFallbackCount++;
      this.srLastError = String(error);
      this.srLastCacheKey = "";
      this.srFramePlanKey = "";
      this.srBindGroups = null;
      throw error;
    }
    if (!compiled) {
      this.srRequestedMode = "off";
      this.srBackend = "off";
      this.srActive = null;
      return this.getUpscalerStatus();
    }

    const cacheKey =
      compiled.mode === "custom"
        ? `custom:${this.srCustomGeneration}`
        : compiled.mode;
    const cachedByCanonical = this.srCache.get(cacheKey);
    if (cachedByCanonical) {
      if (compiled.planId != null) {
        this.srPlanner?.releasePlan?.(compiled.planId);
      }
      if (epoch !== this.srEpoch || this.disposed || this.lost) {
        return this.getUpscalerStatus();
      }
      this.srCache.delete(cacheKey);
      this.srCache.set(cacheKey, cachedByCanonical);
      this.activateSrEntry(cachedByCanonical.mode, cachedByCanonical);
      return this.getUpscalerStatus();
    }

    const d = this.device;
    d.pushErrorScope?.("out-of-memory");
    d.pushErrorScope?.("internal");
    d.pushErrorScope?.("validation");

    const builtKernels = [];
    const pipelineBySource = new Map();
    let buildError = null;
    try {
      for (const spec of compiled.kernels) {
        if (this.disposed || this.lost || epoch !== this.srEpoch) {
          break;
        }
        const weightUsage =
          (spec.uniform ? BUF.UNIFORM : BUF.STORAGE) | BUF.COPY_DST;
        const weights = spec.weights;
        const byteLength = Math.max(16, weights.byteLength);
        const weightBuffer = d.createBuffer({
          size: byteLength,
          usage: weightUsage,
        });
        d.queue.writeBuffer(weightBuffer, 0, weights);

        const sourceKey = `${spec.uniform ? "u" : "s"}:${spec.source}`;
        let pipeline = pipelineBySource.get(sourceKey);
        if (!pipeline) {
          const module = await this.createCheckedShaderModule(spec.source);
          const layout = spec.uniform
            ? this.srUniformPipelineLayout
            : this.srStoragePipelineLayout;
          const desc = {
            layout,
            compute: { module, entryPoint: "main" },
          };
          pipeline = d.createComputePipelineAsync
            ? await d.createComputePipelineAsync(desc)
            : d.createComputePipeline(desc);
          pipelineBySource.set(sourceKey, pipeline);
        }
        builtKernels.push({
          pipeline,
          weightBuffer,
          uniform: spec.uniform,
          bgl: spec.uniform ? this.srUniformBgl : this.srStorageBgl,
        });
      }
    } catch (error) {
      buildError = error;
    }

    const scopeErrors = [];
    if (d.popErrorScope && !this.disposed) {
      for (let i = 0; i < 3; i++) {
        try {
          const err = await d.popErrorScope();
          if (err) scopeErrors.push(err.message || String(err));
        } catch {
          break;
        }
      }
    }
    const finalError =
      buildError ||
      (scopeErrors.length ? new Error(scopeErrors.join("; ")) : null);

    if (finalError || this.disposed || this.lost || epoch !== this.srEpoch) {
      for (const k of builtKernels) k.weightBuffer?.destroy?.();
      if (compiled.planId != null) {
        this.srPlanner?.releasePlan?.(compiled.planId);
      }
      if (this.disposed || this.lost || epoch !== this.srEpoch) {
        return this.getUpscalerStatus();
      }
      this.srActive = null;
      this.srBackend = "inactive";
      this.srFallbackCount++;
      this.srLastError = String(finalError);
      this.srLastCacheKey = "";
      this.srFramePlanKey = "";
      this.srBindGroups = null;
      throw finalError;
    }

    const entry = {
      key: cacheKey,
      planId: compiled.planId ?? null,
      mode: compiled.mode,
      packed: compiled.packed,
      half: compiled.half,
      kernels: builtKernels,
    };
    const previous = this.srCache.get(cacheKey);
    if (previous) {
      this.srCache.delete(cacheKey);
    }
    this.srCache.set(cacheKey, entry);
    this.activateSrEntry(compiled.mode, entry);
    if (previous && previous !== entry) {
      this.disposeSrEntry(previous);
    }
    while (this.srCache.size > 4) {
      const oldestKey = this.srCache.keys().next().value;
      if (oldestKey === cacheKey) break;
      const oldest = this.srCache.get(oldestKey);
      this.srCache.delete(oldestKey);
      if (oldest && oldest !== this.srActive) {
        this.disposeSrEntry(oldest);
      }
    }
    return this.getUpscalerStatus();
  }

  getUpscalerStatus() {
    return {
      requestedMode: this.srRequestedMode,
      activeBackend: this.srBackend,
      supported: Boolean(this.srPlanner),
      halfFloat: Boolean(this.srActive ? this.srActive.half : this.caps.shaderF16),
      fallbackCount: this.srFallbackCount,
      upscaledFrames: this.srUpscaledFrames,
      lastError: this.srLastError,
      availableModes: this.availableModes,
    };
  }

  encodeSr(encoder) {
    if (!this.hasVideo || !this.srActive || !this.srPlanner) return 0;
    const vw = this.videoWidth;
    const vh = this.videoHeight;
    if (vw <= 0 || vh <= 0) return 0;
    const targetW = this.srActive.packed ? vw : Math.max(1, this.width);
    const targetH = this.srActive.packed ? vh : Math.max(1, this.height);
    const planKey = `${this.srActive.key}|${vw}x${vh}|${targetW}x${targetH}`;
    const cacheKey = `${planKey}|${this.videoFrameSerial}`;

    if (cacheKey === this.srLastCacheKey && this.srOutputView) {
      return this.srActive.packed ? 1 : 2;
    }

    try {
      if (planKey !== this.srFramePlanKey || !this.srFramePlan) {
        const framePlan =
          this.srActive.planId != null &&
          typeof this.srPlanner.planFrameFor === "function"
            ? this.srPlanner.planFrameFor(
                this.srActive.planId,
                vw,
                vh,
                targetW,
                targetH,
                BT709_LUMA[0],
                BT709_LUMA[1],
                BT709_LUMA[2],
                true,
              )
            : this.srPlanner.planFrame(
                vw,
                vh,
                targetW,
                targetH,
                BT709_LUMA[0],
                BT709_LUMA[1],
                BT709_LUMA[2],
                true,
              );
        this.srFramePlan = framePlan;
        this.srFramePlanKey = planKey;
        this.ensureSrResources(framePlan);
      }
      const framePlan = this.srFramePlan;
      if (!this.srBindGroups) {
        this.srBindGroups = this.srActive.kernels.map((k) =>
          this.device.createBindGroup({
            layout: k.bgl,
            entries: [
              {
                binding: 0,
                resource: {
                  buffer: this.srParamsBuffer,
                  offset: 0,
                  size: SR_PARAMS_BYTES,
                },
              },
              { binding: 1, resource: { buffer: this.srArenaBuffer } },
              { binding: 2, resource: { buffer: k.weightBuffer } },
              { binding: 3, resource: this.videoView },
              { binding: 4, resource: this.srOutputView },
            ],
          }),
        );
      }
      const pass = encoder.beginComputePass();
      const dispatches = framePlan.dispatches;
      for (let i = 0; i < dispatches.length; i += 5) {
        const ki = dispatches[i];
        const offset = dispatches[i + 1];
        pass.setPipeline(this.srActive.kernels[ki].pipeline);
        pass.setBindGroup(0, this.srBindGroups[ki], [offset]);
        pass.dispatchWorkgroups(
          dispatches[i + 2],
          dispatches[i + 3],
          dispatches[i + 4],
        );
      }
      pass.end();
      this.srLastCacheKey = cacheKey;
      this.srUpscaledFrames++;
      return this.srActive.packed ? 1 : 2;
    } catch (error) {
      this.srActive = null;
      this.srBackend = "inactive";
      this.srFallbackCount++;
      this.srLastError = String(error);
      this.srLastCacheKey = "";
      this.srFramePlanKey = "";
      this.srBindGroups = null;
      return 0;
    }
  }

  ensureSrResources(framePlan) {
    const d = this.device;
    let dirtyBindGroups = false;
    const arenaSize = Math.max(16, Math.ceil(framePlan.arenaBytes));
    if (!this.srArenaBuffer || arenaSize > this.srArenaCapacity) {
      this.srArenaBuffer?.destroy?.();
      this.srArenaBuffer = d.createBuffer({
        size: arenaSize,
        usage: BUF.STORAGE,
      });
      this.srArenaCapacity = arenaSize;
      dirtyBindGroups = true;
    }
    const paramsSize = Math.max(
      SR_PARAMS_BYTES,
      framePlan.paramsBytes.byteLength,
    );
    if (!this.srParamsBuffer || paramsSize > this.srParamsCapacity) {
      this.srParamsBuffer?.destroy?.();
      this.srParamsBuffer = d.createBuffer({
        size: paramsSize,
        usage: BUF.UNIFORM | BUF.COPY_DST,
      });
      this.srParamsCapacity = paramsSize;
      dirtyBindGroups = true;
    }
    d.queue.writeBuffer(this.srParamsBuffer, 0, framePlan.paramsBytes);

    if (
      !this.srOutputTexture ||
      this.srOutputWidth !== framePlan.outputWidth ||
      this.srOutputHeight !== framePlan.outputHeight
    ) {
      this.srOutputTexture?.destroy?.();
      this.srOutputTexture = this.makeTexture(
        framePlan.outputWidth,
        framePlan.outputHeight,
        "rgba16float",
        TEX.STORAGE_BINDING | TEX.TEXTURE_BINDING,
      );
      this.srOutputView = this.srOutputTexture.createView();
      this.srOutputWidth = framePlan.outputWidth;
      this.srOutputHeight = framePlan.outputHeight;
      this.rebuildVideoBindGroup();
      dirtyBindGroups = true;
    }
    if (dirtyBindGroups) {
      this.srBindGroups = null;
    }
  }

  begin() {
    if (this.lost || !this.context) return;
    if (this.pass) {
      this.pass.end();
      this.pass = null;
    }
    const encoder = (this.encoder = this.device.createCommandEncoder());
    const srMode = this.encodeSr(encoder);

    this.frameUniformF32[0] = this.width;
    this.frameUniformF32[1] = this.height;
    this.frameUniformU32[2] = srMode;
    this.frameUniformU32[3] = 0;
    this.frameUniformF32[4] = BT709_LUMA[0];
    this.frameUniformF32[5] = BT709_LUMA[1];
    this.frameUniformF32[6] = BT709_LUMA[2];
    this.frameUniformF32[7] = 0;
    this.frameUniformF32[8] = this.canvas.width;
    this.frameUniformF32[9] = this.canvas.height;
    this.frameUniformF32[10] = 0;
    this.frameUniformF32[11] = 0;
    this.device.queue.writeBuffer(
      this.frameUniformBuffer,
      0,
      this.frameUniformData,
    );

    const view = this.context.getCurrentTexture().createView();
    const pass = (this.pass = encoder.beginRenderPass({
      colorAttachments: [
        {
          view,
          clearValue: { r: 0, g: 0, b: 0, a: 1 },
          loadOp: "clear",
          storeOp: "store",
        },
      ],
    }));
    pass.setViewport(this.x, this.y, this.width, this.height, 0, 1);
    if (this.hasVideo) {
      pass.setPipeline(this.videoPipeline);
      pass.setBindGroup(0, this.videoBindGroup);
      pass.draw(4);
    }
    this.frames++;
  }

  updateSubtitles(module, head, scale) {
    this.clearSubtitles();
    if (this.lost) return;
    this.subtitleImages = [];
    let count = 0;
    for (let i = head; i && count++ < 8192; i = module._erika_image_next(i)) {
      const w = module._erika_image_width(i);
      const h = module._erika_image_height(i);
      const stride = module._erika_image_stride(i);
      if (w <= 0 || h <= 0) continue;
      const ptr = module._erika_image_data(i);
      const length = stride * (h - 1) + w;
      if (ptr + length > module.HEAPU8.length) {
        throw new Error("Invalid libass bitmap");
      }
      const packed = new Uint8Array(w * h);
      for (let y = 0; y < h; y++) {
        packed.set(
          module.HEAPU8.subarray(ptr + y * stride, ptr + y * stride + w),
          y * w,
        );
      }
      const texture = this.makeTexture(
        w,
        h,
        "r8unorm",
        TEX.TEXTURE_BINDING | TEX.COPY_DST,
      );
      this.device.queue.writeTexture(
        { texture },
        packed,
        { bytesPerRow: w, rowsPerImage: h },
        { width: w, height: h },
      );
      const c = module._erika_image_color(i) >>> 0;
      const uniformData = new Float32Array([
        module._erika_image_x(i) / scale,
        module._erika_image_y(i) / scale,
        w / scale,
        h / scale,
        (c >>> 24) / 255,
        ((c >>> 16) & 255) / 255,
        ((c >>> 8) & 255) / 255,
        1 - (c & 255) / 255,
      ]);
      const uniformBuffer = this.device.createBuffer({
        size: SUBTITLE_UNIFORM_BYTES,
        usage: BUF.UNIFORM | BUF.COPY_DST,
      });
      this.device.queue.writeBuffer(uniformBuffer, 0, uniformData);
      const bindGroup = this.device.createBindGroup({
        layout: this.subtitleBgl,
        entries: [
          { binding: 0, resource: { buffer: uniformBuffer } },
          { binding: 1, resource: texture.createView() },
          { binding: 2, resource: this.linearSampler },
        ],
      });
      this.subtitleImages.push({ texture, uniformBuffer, bindGroup });
    }
  }

  clearSubtitles() {
    for (const image of this.subtitleImages || []) {
      image.texture?.destroy?.();
      image.uniformBuffer?.destroy?.();
    }
    this.subtitleImages = [];
  }

  subtitles() {
    if (!this.pass || !this.subtitleImages?.length) return;
    this.pass.setViewport(this.x, this.y, this.width, this.height, 0, 1);
    this.pass.setPipeline(this.subtitlePipeline);
    this.pass.setBindGroup(0, this.frameBindGroup);
    for (const image of this.subtitleImages) {
      this.pass.setBindGroup(1, image.bindGroup);
      this.pass.draw(4);
    }
  }

  ensureBuffer(prop, capProp, requiredBytes, usage) {
    const size = Math.max(16, Math.ceil(requiredBytes / 4) * 4);
    if (!this[prop] || size > this[capProp]) {
      this[prop]?.destroy?.();
      const nextCap = Math.max(size, this[capProp] * 2 || 256);
      this[prop] = this.device.createBuffer({ size: nextCap, usage });
      this[capProp] = nextCap;
      return true;
    }
    return false;
  }

  danmaku(plan) {
    if (this.lost) return;
    const d = this.device;
    this.glyphs = plan.glyphs.length / 8;

    if (plan.fill) {
      if (plan.x === undefined) {
        if (
          this.fillTexture === this.dummyR8Texture ||
          this.fillWidth !== plan.width ||
          this.fillHeight !== plan.height
        ) {
          if (this.fillTexture !== this.dummyR8Texture) {
            this.fillTexture.destroy();
            this.outlineTexture.destroy();
          }
          const usage =
            TEX.TEXTURE_BINDING | TEX.COPY_DST;
          this.fillTexture = this.makeTexture(
            plan.width,
            plan.height,
            "r8unorm",
            usage,
          );
          this.outlineTexture = this.makeTexture(
            plan.width,
            plan.height,
            "r8unorm",
            usage,
          );
          this.fillView = this.fillTexture.createView();
          this.outlineView = this.outlineTexture.createView();
          this.fillWidth = plan.width;
          this.fillHeight = plan.height;
          // Invalidate immediately even if this frame has zero glyphs so the
          // next glyph frame never binds the destroyed textures.
          this.danmakuBindGroup = null;
        }
      }
      const origin = { x: plan.x ?? 0, y: plan.y ?? 0 };
      const layout = { bytesPerRow: plan.width, rowsPerImage: plan.height };
      const size = { width: plan.width, height: plan.height };
      d.queue.writeTexture({ texture: this.fillTexture, origin }, plan.fill, layout, size);
      d.queue.writeTexture(
        { texture: this.outlineTexture, origin },
        plan.outline,
        layout,
        size,
      );
      this.uploads++;
    }

    if (plan.pictureAtlas) {
      if (
        this.emojiTexture === this.dummyRgbaTexture ||
        this.emojiWidth !== plan.pictureWidth ||
        this.emojiHeight !== plan.pictureHeight
      ) {
        if (this.emojiTexture !== this.dummyRgbaTexture) {
          this.emojiTexture.destroy();
        }
        this.emojiTexture = this.makeTexture(
          plan.pictureWidth,
          plan.pictureHeight,
          "rgba8unorm",
          TEX.TEXTURE_BINDING | TEX.COPY_DST,
        );
        this.emojiView = this.emojiTexture.createView();
        this.emojiWidth = plan.pictureWidth;
        this.emojiHeight = plan.pictureHeight;
        this.pictureBindGroup = d.createBindGroup({
          layout: this.pictureBgl,
          entries: [
            { binding: 0, resource: this.emojiView },
            { binding: 1, resource: this.linearSampler },
          ],
        });
      }
      d.queue.writeTexture(
        { texture: this.emojiTexture },
        plan.pictureAtlas,
        {
          bytesPerRow: plan.pictureWidth * 4,
          rowsPerImage: plan.pictureHeight,
        },
        { width: plan.pictureWidth, height: plan.pictureHeight },
      );
    }

    if (this.pass && plan.glyphs.length && plan.groups.length) {
      const glyphRealloc = this.ensureBuffer(
        "glyphBuffer",
        "glyphCapacity",
        plan.glyphs.byteLength,
        BUF.STORAGE | BUF.COPY_DST,
      );
      d.queue.writeBuffer(this.glyphBuffer, 0, plan.glyphs);
      if (glyphRealloc || !this.danmakuBindGroup) {
        this.danmakuBindGroup = d.createBindGroup({
          layout: this.danmakuBgl,
          entries: [
            { binding: 0, resource: { buffer: this.glyphBuffer } },
            { binding: 1, resource: this.fillView },
            { binding: 2, resource: this.outlineView },
            { binding: 3, resource: this.linearSampler },
          ],
        });
      }

      const maxGroups = plan.groups.length / 16;
      const neededBytes = maxGroups * DANMAKU_INSTANCE_BYTES;
      if (this.groupScratch.byteLength < neededBytes) {
        this.groupScratch = new ArrayBuffer(Math.max(neededBytes, 4096));
        this.groupScratchF32 = new Float32Array(this.groupScratch);
        this.groupScratchU32 = new Uint32Array(this.groupScratch);
      }
      const f32 = this.groupScratchF32;
      const u32 = this.groupScratchU32;
      const w = this.canvas.width;
      const h = this.canvas.height;
      let instanceCount = 0;

      for (let n = 0; n < plan.groups.length; n += 16) {
        const start = plan.groups[n] | 0;
        const count = plan.groups[n + 1] | 0;
        if (count <= 0) continue;
        const sx = plan.groups[n + 14];
        const sy = plan.groups[n + 15];
        let minX = w;
        let minY = h;
        let maxX = 0;
        let maxY = 0;
        const end = (start + count) * 8;
        for (let j = start * 8; j < end; j += 8) {
          const gx = plan.glyphs[j];
          const gy = plan.glyphs[j + 1];
          const gw = plan.glyphs[j + 2];
          const gh = plan.glyphs[j + 3];
          minX = Math.min(minX, gx, gx + sx);
          minY = Math.min(minY, gy, gy + sy);
          maxX = Math.max(maxX, gx + gw, gx + gw + sx);
          maxY = Math.max(maxY, gy + gh, gy + gh + sy);
        }
        const rx = Math.max(0, Math.floor(minX));
        const ry = Math.max(0, Math.floor(minY));
        const rr = Math.min(w, Math.ceil(maxX));
        const rb = Math.min(h, Math.ceil(maxY));
        if (rr <= rx || rb <= ry) continue;

        const base = instanceCount * DANMAKU_INSTANCE_FLOATS;
        f32[base + 0] = rx;
        f32[base + 1] = ry;
        f32[base + 2] = rr - rx;
        f32[base + 3] = rb - ry;
        f32[base + 4] = plan.groups[n + 2];
        f32[base + 5] = plan.groups[n + 3];
        f32[base + 6] = plan.groups[n + 4];
        f32[base + 7] = plan.groups[n + 5];
        f32[base + 8] = plan.groups[n + 6];
        f32[base + 9] = plan.groups[n + 7];
        f32[base + 10] = plan.groups[n + 8];
        f32[base + 11] = plan.groups[n + 9];
        f32[base + 12] = sx;
        f32[base + 13] = sy;
        f32[base + 14] =
          plan.groups[n + 5] > 0 ? plan.groups[n + 13] / plan.groups[n + 5] : 0;
        f32[base + 15] = 0;
        u32[base + 16] = start >>> 0;
        u32[base + 17] = count >>> 0;
        u32[base + 18] = 0;
        u32[base + 19] = 0;
        instanceCount++;
      }

      if (instanceCount > 0) {
        const uploadBytes = instanceCount * DANMAKU_INSTANCE_BYTES;
        this.ensureBuffer(
          "groupBuffer",
          "groupCapacity",
          uploadBytes,
          BUF.VERTEX | BUF.COPY_DST,
        );
        d.queue.writeBuffer(this.groupBuffer, 0, this.groupScratch, 0, uploadBytes);
        this.pass.setViewport(0, 0, w, h, 0, 1);
        this.pass.setPipeline(this.danmakuPipeline);
        this.pass.setBindGroup(0, this.frameBindGroup);
        this.pass.setBindGroup(1, this.danmakuBindGroup);
        this.pass.setVertexBuffer(0, this.groupBuffer, 0, uploadBytes);
        this.pass.draw(4, instanceCount);
      }
    }

    this.pictures(plan);
  }

  pictures(plan) {
    if (!this.pass || !plan.pictures?.length) return;
    const w = this.canvas.width;
    const h = this.canvas.height;
    if (!w || !h) return;
    const count = (plan.pictures.length / 9) | 0;
    if (count <= 0) return;
    this.ensureBuffer(
      "pictureBuffer",
      "pictureCapacity",
      plan.pictures.byteLength,
      BUF.VERTEX | BUF.COPY_DST,
    );
    this.device.queue.writeBuffer(this.pictureBuffer, 0, plan.pictures);
    this.pass.setViewport(0, 0, w, h, 0, 1);
    this.pass.setPipeline(this.picturePipeline);
    this.pass.setBindGroup(0, this.frameBindGroup);
    this.pass.setBindGroup(1, this.pictureBindGroup);
    this.pass.setVertexBuffer(0, this.pictureBuffer, 0, plan.pictures.byteLength);
    this.pass.draw(4, count);
  }

  end() {
    if (!this.encoder) return;
    if (this.pass) {
      this.pass.end();
      this.pass = null;
    }
    const commands = this.encoder.finish();
    this.encoder = null;
    if (!this.lost) {
      this.device.queue.submit([commands]);
    }
  }

  destroy() {
    this.disposed = true;
    this.lost = true;
    this.srEpoch++;
    if (this.pass) {
      try {
        this.pass.end();
      } catch {}
      this.pass = null;
    }
    this.encoder = null;
    this.clearSubtitles();
    for (const entry of this.srCache.values()) {
      this.disposeSrEntry(entry);
    }
    this.srCache.clear();
    this.srActive = null;
    this.srPlanner?.free?.();
    this.srPlanner = null;

    const textures = new Set([
      this.videoTexture,
      this.fillTexture,
      this.outlineTexture,
      this.emojiTexture,
      this.srOutputTexture,
      this.dummyRgbaTexture,
      this.dummyR8Texture,
      this.dummySrTexture,
    ]);
    for (const tex of textures) tex?.destroy?.();

    for (const buf of [
      this.frameUniformBuffer,
      this.glyphBuffer,
      this.groupBuffer,
      this.pictureBuffer,
      this.srArenaBuffer,
      this.srParamsBuffer,
    ]) {
      buf?.destroy?.();
    }

    try {
      this.context?.unconfigure?.();
    } catch {}
    this.context = null;
    this.device?.destroy?.();
  }
}
