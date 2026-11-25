import test from "node:test";
import assert from "node:assert/strict";
import { WebGpuRenderer } from "../renderer_webgpu.js";

function createMockWebGpu({
  adapterAvailable = true,
  shaderF16 = true,
  failComputeValidation = false,
  contextAvailable = true,
} = {}) {
  const log = {
    getContextCalls: [],
    computePasses: 0,
    computeDispatches: 0,
    renderPasses: 0,
    renderDraws: [],
    submits: 0,
    copyExternalCalls: 0,
    writeTextures: 0,
    writeBuffers: 0,
    destroyed: 0,
  };

  const makeObject = (kind) => ({
    kind,
    createView() {
      return { kind: `${kind}-view`, owner: this };
    },
    destroy() {
      log.destroyed++;
    },
  });

  const errorScopes = [];
  const device = {
    features: new Set(shaderF16 ? ["shader-f16"] : []),
    limits: {
      maxComputeWorkgroupStorageSize: 32768,
      maxUniformBufferBindingSize: 65536,
      maxStorageBufferBindingSize: 128 * 1024 * 1024,
      maxBufferSize: 256 * 1024 * 1024,
      maxComputeWorkgroupSizeX: 256,
      maxComputeWorkgroupSizeY: 256,
      maxComputeInvocationsPerWorkgroup: 256,
      maxTextureDimension2D: 8192,
      minUniformBufferOffsetAlignment: 256,
      maxStorageBuffersPerShaderStage: 8,
    },
    lost: new Promise(() => {}),
    queue: {
      writeBuffer() {
        log.writeBuffers++;
      },
      writeTexture() {
        log.writeTextures++;
      },
      copyExternalImageToTexture() {
        log.copyExternalCalls++;
      },
      submit() {
        log.submits++;
      },
    },
    pushErrorScope(filter) {
      errorScopes.push({ filter, error: null });
    },
    async popErrorScope() {
      const scope = errorScopes.pop();
      return scope?.error ?? null;
    },
    createSampler() {
      return makeObject("sampler");
    },
    createBuffer(desc) {
      return { ...makeObject("buffer"), size: desc.size, usage: desc.usage };
    },
    createTexture(desc) {
      return { ...makeObject("texture"), size: desc.size, format: desc.format };
    },
    createBindGroupLayout(desc) {
      return { kind: "bgl", entries: desc.entries };
    },
    createPipelineLayout(desc) {
      return { kind: "pl", layouts: desc.bindGroupLayouts };
    },
    createBindGroup(desc) {
      return { kind: "bg", layout: desc.layout, entries: desc.entries };
    },
    createShaderModule(desc) {
      return { kind: "shader", code: desc.code };
    },
    createRenderPipeline(desc) {
      return { kind: "render-pipeline", desc };
    },
    async createComputePipelineAsync(desc) {
      if (failComputeValidation) {
        const validationScope = [...errorScopes]
          .reverse()
          .find((s) => s.filter === "validation");
        if (validationScope) {
          validationScope.error = { message: "simulated WGSL validation failure" };
        }
      }
      return { kind: "compute-pipeline", desc };
    },
    createCommandEncoder() {
      return {
        beginComputePass() {
          log.computePasses++;
          return {
            setPipeline() {},
            setBindGroup() {},
            dispatchWorkgroups() {
              log.computeDispatches++;
            },
            end() {},
          };
        },
        beginRenderPass() {
          log.renderPasses++;
          return {
            setViewport() {},
            setPipeline(pipeline) {
              this._pipeline = pipeline;
            },
            setBindGroup() {},
            setVertexBuffer() {},
            draw(vertexCount, instanceCount = 1) {
              log.renderDraws.push({ vertexCount, instanceCount });
            },
            end() {},
          };
        },
        finish() {
          return { kind: "command-buffer" };
        },
      };
    },
    destroy() {
      log.destroyed++;
    },
  };

  const gpu = {
    async requestAdapter() {
      if (!adapterAvailable) return null;
      return {
        features: device.features,
        limits: device.limits,
        async requestDevice() {
          return device;
        },
      };
    },
    getPreferredCanvasFormat() {
      return "bgra8unorm";
    },
  };

  const canvas = {
    width: 1280,
    height: 720,
    getContext(kind) {
      log.getContextCalls.push(kind);
      if (kind !== "webgpu" || !contextAvailable) return null;
      return {
        configure() {},
        unconfigure() {},
        getCurrentTexture() {
          return makeObject("swapchain-texture");
        },
      };
    },
  };

  return { gpu, canvas, device, log };
}

function createFakePlanner() {
  let active = null;
  let planFrameCalls = 0;
  return {
    get planFrameCalls() {
      return planFrameCalls;
    },
    compile(mode) {
      if (mode === "off") {
        active = null;
        return null;
      }
      if (mode === "invalid") {
        throw new Error("unknown upscaler mode: invalid");
      }
      active = mode;
      return {
        mode,
        packed: mode !== "fsr1",
        half: true,
        paramsBytesSize: 1104,
        kernels: [
          {
            source: "@compute @workgroup_size(8,8,1) fn main() {}",
            weights: new Float32Array(16),
            uniform: true,
            groupX: 8,
            groupY: 8,
            ring: false,
            head: true,
            fused: true,
          },
        ],
      };
    },
    planFrame(w, h, tw, th) {
      planFrameCalls++;
      const packed = active !== "fsr1";
      return {
        outputWidth: packed ? w : tw,
        outputHeight: packed ? h : th,
        virtualWidth: packed ? w * 2 : tw,
        virtualHeight: packed ? h * 2 : th,
        packed,
        arenaBytes: 4096,
        tileWidth: w,
        tileHeight: h,
        tiles: 1,
        paramsStride: 1280,
        paramsBytes: new Uint8Array(1280),
        dispatches: new Uint32Array([0, 0, 8, 8, 1]),
      };
    },
    free() {},
  };
}

test("WebGPU probe leaves OffscreenCanvas untouched when adapter is unavailable", async () => {
  const { gpu, canvas, log } = createMockWebGpu({ adapterAvailable: false });
  const renderer = await WebGpuRenderer.create(canvas, { gpu });
  assert.equal(renderer, null);
  assert.deepEqual(
    log.getContextCalls,
    [],
    "must not claim webgpu context before adapter/device succeed, or WebGL2 fallback fails",
  );
});

test("WebGpuRenderer composites video, subtitles, single-draw danmaku, emoji, and cached SR", async () => {
  const { gpu, canvas, log } = createMockWebGpu();
  const planner = createFakePlanner();
  const renderer = await WebGpuRenderer.create(canvas, {
    gpu,
    srPlannerFactory: () => planner,
    availableModes: ["off", "fsr1", "anime4k_s", "artcnn_c4f16"],
  });
  assert.ok(renderer);
  assert.equal(renderer.backend, "webgpu");
  assert.deepEqual(log.getContextCalls, ["webgpu"]);

  renderer.size(1280, 720, 640, 360);
  renderer.upload({ displayWidth: 640, displayHeight: 360 });
  assert.equal(log.copyExternalCalls, 1);

  const status = await renderer.setUpscaler("anime4k_s");
  assert.equal(status.requestedMode, "anime4k_s");
  assert.equal(status.activeBackend, "compute");
  assert.equal(status.halfFloat, true);

  const danmakuPlan = {
    width: 64,
    height: 64,
    fill: new Uint8Array(64 * 64),
    outline: new Uint8Array(64 * 64),
    glyphs: new Float32Array([
      10, 20, 30, 30, 0, 0, 0.5, 0.5,
      42, 20, 30, 30, 0.5, 0, 0.5, 0.5,
    ]),
    groups: new Float32Array([
      0, 2, 1, 1, 1, 0.8, 0, 0, 0, 1, 0, 0, 0, 0.5, 1, 1,
    ]),
    pictures: new Float32Array([100, 20, 30, 30, 0, 0, 1, 1, 0.8]),
    pictureWidth: 32,
    pictureHeight: 32,
    pictureAtlas: new Uint8Array(32 * 32 * 4),
  };

  // Frame 1: runs SR compute pass once + single render pass (video + 1 danmaku + 1 picture)
  renderer.begin();
  renderer.subtitles();
  renderer.danmaku(danmakuPlan);
  renderer.end();

  assert.equal(log.computePasses, 1);
  assert.equal(log.computeDispatches, 1);
  assert.equal(log.renderPasses, 1);
  assert.equal(log.submits, 1);
  assert.equal(renderer.getUpscalerStatus().upscaledFrames, 1);
  // 3 draws in the single render pass: video (4,1), danmaku (4,1), emoji (4,1)
  assert.deepEqual(log.renderDraws, [
    { vertexCount: 4, instanceCount: 1 },
    { vertexCount: 4, instanceCount: 1 },
    { vertexCount: 4, instanceCount: 1 },
  ]);

  // Frame 2 with the same VideoFrame (e.g. paused / slider drag): SR cache hit!
  renderer.begin();
  renderer.subtitles();
  renderer.danmaku({ ...danmakuPlan, fill: undefined, pictureAtlas: undefined });
  renderer.end();

  assert.equal(log.computePasses, 1, "same frame must hit SR output cache");
  assert.equal(planner.planFrameCalls, 1);
  assert.equal(log.renderPasses, 2);
  assert.equal(log.submits, 2);

  // Frame 3 with a new VideoFrame at the same resolution: runs compute, reuses FramePlan!
  renderer.upload({ displayWidth: 640, displayHeight: 360 });
  renderer.begin();
  renderer.end();
  assert.equal(log.computePasses, 2);
  assert.equal(
    planner.planFrameCalls,
    1,
    "steady-state frames must reuse cached FramePlan without calling WASM",
  );
  assert.equal(renderer.getUpscalerStatus().upscaledFrames, 2);

  renderer.destroy();
  assert.ok(log.destroyed > 0);
});

test("WebGpuRenderer keeps native playback healthy when SR pipeline validation fails", async () => {
  const { gpu, canvas, log } = createMockWebGpu({ failComputeValidation: true });
  const planner = createFakePlanner();
  const renderer = await WebGpuRenderer.create(canvas, {
    gpu,
    srPlannerFactory: () => planner,
    availableModes: ["off", "artcnn_c4f16"],
  });
  renderer.size(1280, 720, 640, 360);
  renderer.upload({ displayWidth: 640, displayHeight: 360 });

  await assert.rejects(
    () => renderer.setUpscaler("artcnn_c4f16"),
    /validation failure/,
  );
  const status = renderer.getUpscalerStatus();
  assert.equal(status.activeBackend, "inactive");
  assert.equal(status.fallbackCount, 1);
  assert.match(status.lastError, /validation failure/);

  // Video presentation must continue without compute dispatches or errors.
  renderer.begin();
  renderer.end();
  assert.equal(log.computePasses, 0);
  assert.equal(log.renderPasses, 1);
  assert.equal(log.submits, 1);
  renderer.destroy();
});
