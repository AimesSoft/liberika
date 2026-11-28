import test from "node:test";
import assert from "node:assert/strict";
import { readdir, readFile } from "node:fs/promises";
import { WebGpuRenderer } from "../renderer_webgpu.js";

function createMockWebGpu({
  adapterAvailable = true,
  shaderF16 = true,
  failComputeValidation = false,
  failRenderValidation = false,
  failRenderShaderCompile = false,
  contextAvailable = true,
  failContextConfigure = false,
  deferComputePipeline = false,
} = {}) {
  const destroyedObjects = new Set();
  const createdWeightBuffers = [];
  const pendingComputeResolvers = [];
  let resolveDeviceLost;
  const lostPromise = new Promise((resolve) => {
    resolveDeviceLost = resolve;
  });

  const log = {
    getContextCalls: [],
    computePasses: 0,
    computeDispatches: 0,
    renderPasses: 0,
    renderDraws: [],
    renderDrawDetails: [],
    bufferWrites: [],
    submits: 0,
    copyExternalCalls: 0,
    writeTextures: 0,
    writeBuffers: 0,
    destroyed: 0,
    boundDestroyedResource: false,
    destroyedObjects,
    createdWeightBuffers,
  };

  const makeObject = (kind) => ({
    kind,
    createView() {
      return { kind: `${kind}-view`, owner: this };
    },
    destroy() {
      if (!destroyedObjects.has(this)) {
        destroyedObjects.add(this);
        log.destroyed++;
      }
    },
  });

  const checkBindGroupAlive = (bg) => {
    if (!bg?.entries) return;
    for (const entry of bg.entries) {
      const res = entry.resource;
      const owner = res?.owner || res?.buffer || res;
      if (destroyedObjects.has(owner)) {
        log.boundDestroyedResource = true;
        throw new Error(`BindGroup references destroyed ${owner.kind}`);
      }
    }
  };

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
    lost: lostPromise,
    queue: {
      writeBuffer(buffer, offset, data, dataOffset = 0, size) {
        log.writeBuffers++;
        let copy = null;
        if (data instanceof ArrayBuffer) {
          const byteLen = size ?? data.byteLength - dataOffset;
          copy = new Uint8Array(data.slice(dataOffset, dataOffset + byteLen));
        } else if (ArrayBuffer.isView(data)) {
          const elemOffset = data.byteOffset + dataOffset;
          const byteLen = size ?? data.byteLength - dataOffset;
          copy = new Uint8Array(data.buffer.slice(elemOffset, elemOffset + byteLen));
        }
        log.bufferWrites.push({ buffer, offset, bytes: copy });
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
      const obj = { ...makeObject("buffer"), size: desc.size, usage: desc.usage };
      // SR weight buffers are created with COPY_DST and size >= 64 (or uniform/storage).
      createdWeightBuffers.push(obj);
      return obj;
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
      return {
        kind: "shader",
        code: desc.code,
        async getCompilationInfo() {
          if (failRenderShaderCompile && desc.code.includes("vs_main")) {
            return {
              messages: [
                { type: "error", message: "simulated WGSL shader compile failure" },
              ],
            };
          }
          return { messages: [] };
        },
      };
    },
    createRenderPipeline(desc) {
      return { kind: "render-pipeline", desc };
    },
    async createRenderPipelineAsync(desc) {
      if (failRenderValidation) {
        const validationScope = [...errorScopes]
          .reverse()
          .find((s) => s.filter === "validation");
        if (validationScope) {
          validationScope.error = {
            message: "simulated render pipeline validation failure",
          };
        }
      }
      return { kind: "render-pipeline", desc };
    },
    async createComputePipelineAsync(desc) {
      if (deferComputePipeline) {
        await new Promise((resolve) => pendingComputeResolvers.push(resolve));
      }
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
            setBindGroup(_slot, bg) {
              checkBindGroupAlive(bg);
            },
            dispatchWorkgroups() {
              log.computeDispatches++;
            },
            end() {},
          };
        },
        beginRenderPass() {
          log.renderPasses++;
          return {
            _viewport: null,
            setViewport(x, y, w, h, minDepth, maxDepth) {
              this._viewport = [x, y, w, h, minDepth, maxDepth];
            },
            setPipeline(pipeline) {
              this._pipeline = pipeline;
            },
            setBindGroup(_slot, bg) {
              checkBindGroupAlive(bg);
            },
            setVertexBuffer(_slot, buf) {
              if (destroyedObjects.has(buf)) {
                log.boundDestroyedResource = true;
                throw new Error("VertexBuffer was destroyed");
              }
            },
            draw(vertexCount, instanceCount = 1) {
              log.renderDraws.push({ vertexCount, instanceCount });
              log.renderDrawDetails.push({
                pipeline: this._pipeline,
                viewport: this._viewport ? [...this._viewport] : null,
                vertexCount,
                instanceCount,
              });
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
      if (!destroyedObjects.has(this)) {
        destroyedObjects.add(this);
        log.destroyed++;
      }
      resolveDeviceLost({ reason: "destroyed", message: "device destroyed" });
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
        configure() {
          if (failContextConfigure) {
            throw new Error("simulated swapchain configure failure");
          }
        },
        unconfigure() {},
        getCurrentTexture() {
          return makeObject("swapchain-texture");
        },
      };
    },
  };

  return {
    gpu,
    canvas,
    device,
    log,
    triggerDeviceLost(info = { reason: "unknown", message: "GPU process crashed" }) {
      resolveDeviceLost(info);
    },
    flushPendingComputePipelines() {
      while (pendingComputeResolvers.length) {
        pendingComputeResolvers.shift()();
      }
    },
    get pendingComputeCount() {
      return pendingComputeResolvers.length;
    },
  };
}

function createFakePlanner() {
  let nextPlanId = 1;
  const plans = new Map();
  let activePlanId = null;
  let planFrameCalls = 0;
  const releasedPlanIds = [];

  const makeFrame = (mode, w, h, tw, th) => {
    planFrameCalls++;
    const packed = mode !== "fsr1";
    const kernelCount = mode === "artcnn_c4f16" ? 3 : 1;
    const dispatches = new Uint32Array(kernelCount * 5);
    for (let k = 0; k < kernelCount; k++) {
      dispatches[k * 5 + 0] = k;
      dispatches[k * 5 + 1] = k * 1280;
      dispatches[k * 5 + 2] = 8;
      dispatches[k * 5 + 3] = 8;
      dispatches[k * 5 + 4] = 1;
    }
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
      paramsBytes: new Uint8Array(kernelCount * 1280),
      dispatches,
    };
  };

  return {
    get planFrameCalls() {
      return planFrameCalls;
    },
    get releasedPlanIds() {
      return releasedPlanIds;
    },
    compile(mode) {
      if (mode === "off") {
        activePlanId = null;
        return null;
      }
      if (mode === "invalid") {
        throw new Error("unknown upscaler mode: invalid");
      }
      const planId = nextPlanId++;
      plans.set(planId, mode);
      activePlanId = planId;
      const kernelCount = mode === "artcnn_c4f16" ? 3 : 1;
      const kernels = Array.from({ length: kernelCount }, (_, i) => ({
        source: `@compute @workgroup_size(8,8,1) fn main() { /* ${mode}:${i} */ }`,
        weights: new Float32Array(16),
        uniform: true,
        groupX: 8,
        groupY: 8,
        ring: false,
        head: i === kernelCount - 1,
        fused: kernelCount === 1,
      }));
      return {
        planId,
        mode,
        packed: mode !== "fsr1",
        half: true,
        paramsBytesSize: 1104,
        kernels,
      };
    },
    releasePlan(planId) {
      plans.delete(planId);
      releasedPlanIds.push(planId);
      if (activePlanId === planId) activePlanId = null;
    },
    planFrame(w, h, tw, th) {
      const mode = plans.get(activePlanId);
      if (!mode) throw new Error("no active plan");
      return makeFrame(mode, w, h, tw, th);
    },
    planFrameFor(planId, w, h, tw, th) {
      const mode = plans.get(planId);
      if (!mode) throw new Error(`unknown planId ${planId}`);
      return makeFrame(mode, w, h, tw, th);
    },
    free() {
      plans.clear();
    },
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

test("WebGPU probe validates presentation shaders and pipelines before claiming canvas context", async () => {
  for (const opts of [
    { failRenderValidation: true },
    { failRenderShaderCompile: true },
  ]) {
    const { gpu, canvas, log } = createMockWebGpu(opts);
    await assert.rejects(() => WebGpuRenderer.create(canvas, { gpu }));
    assert.deepEqual(
      log.getContextCalls,
      [],
      "must not claim canvas context when shader compilation or pipeline validation fails",
    );
  }
});

test("WebGPU marks context.configure failure as canvasContextClaimed instead of returning null", async () => {
  const { gpu, canvas, log } = createMockWebGpu({ failContextConfigure: true });
  await assert.rejects(
    () => WebGpuRenderer.create(canvas, { gpu }),
    (err) => {
      assert.equal(err.canvasContextClaimed, true);
      assert.match(err.message, /configure/i);
      return true;
    },
  );
  assert.deepEqual(log.getContextCalls, ["webgpu"]);
});

test("WebGPU reports unexpected device.lost via onDeviceLost and ignores intentional destroy", async () => {
  const lostEvents = [];
  const { gpu, canvas, log, triggerDeviceLost } = createMockWebGpu();
  const renderer = await WebGpuRenderer.create(canvas, {
    gpu,
    srPlannerFactory: () => createFakePlanner(),
    onDeviceLost: (info) => lostEvents.push(info),
  });
  await renderer.setUpscaler("fsr1");
  triggerDeviceLost({ reason: "unknown", message: "driver reset" });
  await Promise.resolve();
  assert.equal(lostEvents.length, 1);
  assert.match(lostEvents[0].message, /driver reset/);
  assert.equal(renderer.lost, true);
  assert.equal(renderer.getUpscalerStatus().activeBackend, "inactive");
  // Further frames must be no-ops after device loss.
  renderer.begin();
  renderer.end();
  assert.equal(log.submits, 0);
  renderer.destroy();

  // Intentional destroy must NOT trigger onDeviceLost.
  const destroyEvents = [];
  const second = createMockWebGpu();
  const renderer2 = await WebGpuRenderer.create(second.canvas, {
    gpu: second.gpu,
    onDeviceLost: (info) => destroyEvents.push(info),
  });
  renderer2.destroy();
  await Promise.resolve();
  assert.equal(destroyEvents.length, 0);
});

test("WebGPU rebuilds danmakuBindGroup even when atlas resizes on a glyph-less frame", async () => {
  const { gpu, canvas, log } = createMockWebGpu();
  const renderer = await WebGpuRenderer.create(canvas, { gpu });
  renderer.size(1280, 720, 640, 360);

  const glyphFrame = {
    glyphs: new Float32Array([10, 20, 30, 30, 0, 0, 0.5, 0.5]),
    groups: new Float32Array([
      0, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0.5, 1, 1,
    ]),
    pictures: new Float32Array(0),
  };

  // Frame 1: initial 64x64 atlas with glyphs builds danmakuBindGroup.
  renderer.begin();
  renderer.danmaku({
    ...glyphFrame,
    width: 64,
    height: 64,
    fill: new Uint8Array(64 * 64),
    outline: new Uint8Array(64 * 64),
  });
  renderer.end();

  // Frame 2: atlas resizes to 128x128 on a frame with ZERO glyphs (destroys 64x64 textures).
  renderer.begin();
  renderer.danmaku({
    width: 128,
    height: 128,
    fill: new Uint8Array(128 * 128),
    outline: new Uint8Array(128 * 128),
    glyphs: new Float32Array(0),
    groups: new Float32Array(0),
    pictures: new Float32Array(0),
  });
  renderer.end();

  // Frame 3: glyphs return without another atlas upload — must NOT bind destroyed 64x64 textures!
  renderer.begin();
  renderer.danmaku(glyphFrame);
  renderer.end();
  assert.equal(log.boundDestroyedResource, false);
  assert.equal(log.submits, 3);
  renderer.destroy();
});

test("WebGpuRenderer keeps video and subtitles in the picture while danmaku spans letterbox bars", async () => {
  const { gpu, canvas, log } = createMockWebGpu();
  const renderer = await WebGpuRenderer.create(canvas, { gpu });

  // Pillarbox: 1600x720 canvas with 1280x720 video leaves 160px bars on each side.
  renderer.size(1600, 720, 1280, 720);
  renderer.upload({ displayWidth: 1280, displayHeight: 720 });
  renderer.subtitleImages = [{ bindGroup: { entries: [] } }];

  renderer.begin();
  renderer.subtitles();
  renderer.danmaku({
    width: 64,
    height: 64,
    fill: new Uint8Array(64 * 64),
    outline: new Uint8Array(64 * 64),
    glyphs: new Float32Array([
      12, 30, 80, 24, 0, 0, 0.2, 0.2,
      1500, 30, 80, 24, 0.2, 0, 0.2, 0.2,
    ]),
    groups: new Float32Array([
      0, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0.5, 1, 1,
      1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0.5, 1, 1,
    ]),
    pictures: new Float32Array([1510, 80, 32, 32, 0, 0, 1, 1, 1]),
    pictureWidth: 32,
    pictureHeight: 32,
    pictureAtlas: new Uint8Array(32 * 32 * 4),
  });
  renderer.end();

  const frameWrite = log.bufferWrites.findLast((w) => w.buffer === renderer.frameUniformBuffer);
  assert.ok(frameWrite);
  const frameF32 = new Float32Array(
    frameWrite.bytes.buffer,
    frameWrite.bytes.byteOffset,
    frameWrite.bytes.byteLength / 4,
  );
  assert.deepEqual([frameF32[0], frameF32[1]], [1280, 720], "picture resolution stays aspect-fit");
  assert.deepEqual([frameF32[8], frameF32[9]], [1600, 720], "canvas resolution covers pillarbox bars");

  const videoDraw = log.renderDrawDetails.find((d) => d.pipeline === renderer.videoPipeline);
  const subDraw = log.renderDrawDetails.find((d) => d.pipeline === renderer.subtitlePipeline);
  const danmakuDraw = log.renderDrawDetails.find((d) => d.pipeline === renderer.danmakuPipeline);
  const pictureDraw = log.renderDrawDetails.find((d) => d.pipeline === renderer.picturePipeline);
  assert.deepEqual(videoDraw.viewport, [160, 0, 1280, 720, 0, 1]);
  assert.deepEqual(subDraw.viewport, [160, 0, 1280, 720, 0, 1]);
  assert.deepEqual(danmakuDraw.viewport, [0, 0, 1600, 720, 0, 1]);
  assert.equal(danmakuDraw.instanceCount, 2, "glyphs inside left and right bars must not be clipped");
  assert.deepEqual(pictureDraw.viewport, [0, 0, 1600, 720, 0, 1]);

  // Letterbox with emoji-only danmaku: must still switch viewport to full canvas.
  log.renderDrawDetails.length = 0;
  renderer.size(1280, 1000, 1280, 720);
  renderer.subtitleImages = [];
  renderer.begin();
  renderer.danmaku({
    glyphs: new Float32Array(0),
    groups: new Float32Array(0),
    pictures: new Float32Array([40, 20, 32, 32, 0, 0, 1, 1, 1]),
  });
  renderer.end();

  const letterboxVideo = log.renderDrawDetails.find((d) => d.pipeline === renderer.videoPipeline);
  const letterboxEmoji = log.renderDrawDetails.find((d) => d.pipeline === renderer.picturePipeline);
  assert.deepEqual(letterboxVideo.viewport, [0, 140, 1280, 720, 0, 1]);
  assert.deepEqual(letterboxEmoji.viewport, [0, 0, 1280, 1000, 0, 1]);
  renderer.destroy();
});

test("WebGPU SR cache refreshes LRU recency on hits and disposes evicted weight buffers without leaks", async () => {
  const { gpu, canvas, log } = createMockWebGpu();
  const planner = createFakePlanner();
  const renderer = await WebGpuRenderer.create(canvas, {
    gpu,
    srPlannerFactory: () => planner,
  });

  // Fill cache to capacity (4): A, B, C, D.
  await renderer.setUpscaler("fsr1");
  const entryA = renderer.srActive;
  await renderer.setUpscaler("anime4k_s");
  const entryB = renderer.srActive;
  await renderer.setUpscaler("anime4k_m");
  await renderer.setUpscaler("anime4k_l");

  // Cache hit on A refreshes A to most-recently-used; B is now oldest.
  await renderer.setUpscaler("fsr1");
  assert.equal(renderer.srActive, entryA);

  // Inserting 5th mode E must evict B (not A!), destroy B's weight buffers, and release B's plan.
  await renderer.setUpscaler("anime4k_vl");
  assert.equal(renderer.srCache.has("fsr1"), true);
  assert.equal(renderer.srCache.has("anime4k_s"), false);
  assert.equal(log.destroyedObjects.has(entryB.kernels[0].weightBuffer), true);
  assert.equal(log.destroyedObjects.has(entryA.kernels[0].weightBuffer), false);
  assert.ok(planner.releasedPlanIds.includes(entryB.planId));

  // Verify A is still alive and never binds destroyed buffers when rendered.
  renderer.size(1280, 720, 640, 360);
  renderer.upload({ displayWidth: 640, displayHeight: 360 });
  await renderer.setUpscaler("fsr1");
  renderer.begin();
  renderer.end();
  assert.equal(log.boundDestroyedResource, false);

  renderer.destroy();
  // Every single cached entry's buffers must be destroyed on teardown.
  assert.equal(log.destroyedObjects.has(entryA.kernels[0].weightBuffer), true);
});

test("WebGPU resize and render during async SR compile stay consistent with active plan", async () => {
  const mock = createMockWebGpu({ deferComputePipeline: true });
  const planner = createFakePlanner();
  const renderer = await WebGpuRenderer.create(mock.canvas, {
    gpu: mock.gpu,
    srPlannerFactory: () => planner,
  });
  renderer.size(1280, 720, 640, 360);
  renderer.upload({ displayWidth: 640, displayHeight: 360 });

  // Compile anime4k_s (1 kernel).
  const p1 = renderer.setUpscaler("anime4k_s");
  await new Promise((r) => setTimeout(r, 0));
  mock.flushPendingComputePipelines();
  await p1;
  renderer.begin();
  renderer.end();
  assert.equal(mock.log.computeDispatches, 1);

  // Start compiling artcnn_c4f16 (3 kernels) asynchronously without resolving yet.
  const p2 = renderer.setUpscaler("artcnn_c4f16");
  // Wait microtasks so compile() ran and createComputePipelineAsync is pending.
  await new Promise((r) => setTimeout(r, 0));
  assert.ok(mock.pendingComputeCount > 0);

  // Resize AND render a new frame while artcnn_c4f16 is still compiling:
  // must continue planning/dispatching with anime4k_s's planId (1 kernel), NOT artcnn's 3 kernels!
  renderer.size(1920, 1080, 640, 360);
  renderer.upload({ displayWidth: 640, displayHeight: 360 });
  renderer.begin();
  renderer.end();
  assert.equal(mock.log.computeDispatches, 2);
  assert.equal(mock.log.boundDestroyedResource, false);

  // Finish compiling artcnn_c4f16 (3 sequential kernels).
  while (mock.pendingComputeCount > 0) {
    mock.flushPendingComputePipelines();
    await new Promise((r) => setTimeout(r, 0));
  }
  const status = await p2;
  assert.equal(status.requestedMode, "artcnn_c4f16");
  assert.equal(status.activeBackend, "compute");

  // Next frame uses artcnn_c4f16 (3 dispatches) at the resized 1920x1080 viewport.
  renderer.begin();
  renderer.end();
  assert.equal(mock.log.computeDispatches, 5);
  renderer.destroy();
});

test("WebGPU destroy() during pending async setUpscaler disposes in-flight weight buffers cleanly", async () => {
  const mock = createMockWebGpu({ deferComputePipeline: true });
  const planner = createFakePlanner();
  const renderer = await WebGpuRenderer.create(mock.canvas, {
    gpu: mock.gpu,
    srPlannerFactory: () => planner,
  });

  const beforeBuffers = mock.log.createdWeightBuffers.length;
  const compilePromise = renderer.setUpscaler("artcnn_c4f16");
  await new Promise((r) => setTimeout(r, 0));
  assert.ok(mock.pendingComputeCount > 0);

  // Destroy renderer while compute pipeline compilation is still pending.
  renderer.destroy();
  while (mock.pendingComputeCount > 0) {
    mock.flushPendingComputePipelines();
    await new Promise((r) => setTimeout(r, 0));
  }
  await compilePromise;

  const newBuffers = mock.log.createdWeightBuffers.slice(beforeBuffers);
  assert.ok(newBuffers.length > 0);
  for (const buf of newBuffers) {
    assert.equal(
      mock.log.destroyedObjects.has(buf),
      true,
      "weight buffer created before destroy() must be disposed",
    );
  }
});

test("scripts/web_npm_package.py RUNTIME includes every local JS module imported by packages/erika_web", async () => {
  const pkgDir = new URL("../", import.meta.url);
  const scriptText = await readFile(
    new URL("../../../scripts/web_npm_package.py", import.meta.url),
    "utf8",
  );
  const runtimeMatch = scriptText.match(/RUNTIME\s*=\s*\[([\s\S]*?)\]/);
  assert.ok(runtimeMatch, "RUNTIME list not found in scripts/web_npm_package.py");
  const runtime = new Set(
    Array.from(runtimeMatch[1].matchAll(/['"]([^'"]+)['"]/g), (m) => m[1]),
  );

  const files = (await readdir(pkgDir)).filter((f) => f.endsWith(".js"));
  assert.ok(files.includes("renderer_webgpu.js"));
  for (const file of files) {
    assert.ok(
      runtime.has(file),
      `packages/erika_web/${file} is missing from scripts/web_npm_package.py RUNTIME`,
    );
    const src = await readFile(new URL(file, pkgDir), "utf8");
    for (const m of src.matchAll(/\bfrom\s+["']\.\/([^"']+)["']/g)) {
      assert.ok(
        runtime.has(m[1]),
        `${file} imports ./${m[1]} which is missing from scripts/web_npm_package.py RUNTIME`,
      );
    }
  }
});
