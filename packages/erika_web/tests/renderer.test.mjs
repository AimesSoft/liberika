import test from "node:test";
import assert from "node:assert/strict";
import { Renderer } from "../renderer.js";

function createMockWebGL2() {
  let nextId = 1;
  let currentProgram = null;
  let currentFramebuffer = null;
  let currentViewport = [0, 0, 0, 0];
  let currentScissor = [0, 0, 0, 0];
  const draws = [];
  const texAllocations = [];
  const programs = [];

  const gl = {
    VERTEX_SHADER: 0x8b31,
    FRAGMENT_SHADER: 0x8b30,
    COMPILE_STATUS: 0x8b81,
    LINK_STATUS: 0x8b82,
    TEXTURE_2D: 0x0de1,
    TEXTURE_MIN_FILTER: 0x2801,
    TEXTURE_MAG_FILTER: 0x2800,
    TEXTURE_WRAP_S: 0x2802,
    TEXTURE_WRAP_T: 0x2803,
    LINEAR: 0x2601,
    CLAMP_TO_EDGE: 0x812f,
    TEXTURE0: 0x84c0,
    ARRAY_BUFFER: 0x8892,
    DYNAMIC_DRAW: 0x88e8,
    FLOAT: 0x1406,
    UNPACK_ALIGNMENT: 0x0cf5,
    FRAMEBUFFER: 0x8d40,
    COLOR_ATTACHMENT0: 0x8ce0,
    FRAMEBUFFER_COMPLETE: 0x8cd5,
    SCISSOR_TEST: 0x0c11,
    COLOR_BUFFER_BIT: 0x4000,
    BLEND: 0x0be2,
    FUNC_ADD: 0x8006,
    MAX: 0x8008,
    ONE: 1,
    ONE_MINUS_SRC_ALPHA: 0x0303,
    TRIANGLES: 0x0004,
    RGBA: 0x1908,
    RGBA8: 0x8058,
    R8: 0x8229,
    RED: 0x1903,
    UNSIGNED_BYTE: 0x1401,

    createProgram() {
      const program = { id: nextId++, uniforms: new Map(), shaders: [] };
      programs.push(program);
      return program;
    },
    createShader(type) {
      return { id: nextId++, type, source: "" };
    },
    shaderSource(shader, source) {
      shader.source = source;
    },
    compileShader() {},
    getShaderParameter() {
      return true;
    },
    getShaderInfoLog() {
      return "";
    },
    attachShader(program, shader) {
      program.shaders.push(shader);
    },
    deleteShader() {},
    linkProgram() {},
    getProgramParameter() {
      return true;
    },
    getProgramInfoLog() {
      return "";
    },
    createTexture() {
      return { id: nextId++ };
    },
    bindTexture() {},
    texParameteri() {},
    activeTexture() {},
    createFramebuffer() {
      return { id: nextId++ };
    },
    bindFramebuffer(_target, fb) {
      currentFramebuffer = fb;
    },
    framebufferTexture2D() {},
    checkFramebufferStatus() {
      return gl.FRAMEBUFFER_COMPLETE;
    },
    createBuffer() {
      return { id: nextId++ };
    },
    bindBuffer() {},
    bufferData() {},
    createVertexArray() {
      return { id: nextId++ };
    },
    bindVertexArray() {},
    enableVertexAttribArray() {},
    vertexAttribPointer() {},
    vertexAttribDivisor() {},
    pixelStorei() {},
    disable() {},
    enable() {},
    clearColor() {},
    clear() {},
    blendEquation() {},
    blendFunc() {},
    viewport(x, y, w, h) {
      currentViewport = [x, y, w, h];
    },
    scissor(x, y, w, h) {
      currentScissor = [x, y, w, h];
    },
    useProgram(program) {
      currentProgram = program;
    },
    getUniformLocation(program, name) {
      return { program, name };
    },
    uniform1i(loc, value) {
      if (loc) loc.program.uniforms.set(loc.name, [value]);
    },
    uniform1f(loc, value) {
      if (loc) loc.program.uniforms.set(loc.name, [value]);
    },
    uniform2f(loc, a, b) {
      if (loc) loc.program.uniforms.set(loc.name, [a, b]);
    },
    uniform4f(loc, a, b, c, d) {
      if (loc) loc.program.uniforms.set(loc.name, [a, b, c, d]);
    },
    texImage2D(_target, _level, internalFormat, width, height) {
      texAllocations.push({ internalFormat, width, height });
    },
    texSubImage2D() {},
    drawArrays(mode, first, count) {
      draws.push({
        kind: "arrays",
        mode,
        first,
        count,
        program: currentProgram,
        framebuffer: currentFramebuffer,
        viewport: [...currentViewport],
        scissor: [...currentScissor],
        uniforms: new Map(
          Array.from(currentProgram.uniforms.entries(), ([k, v]) => [k, [...v]]),
        ),
      });
    },
    drawArraysInstanced(mode, first, count, instanceCount) {
      draws.push({
        kind: "instanced",
        mode,
        first,
        count,
        instanceCount,
        program: currentProgram,
        framebuffer: currentFramebuffer,
        viewport: [...currentViewport],
        scissor: [...currentScissor],
        uniforms: new Map(
          Array.from(currentProgram.uniforms.entries(), ([k, v]) => [k, [...v]]),
        ),
      });
    },
    deleteTexture() {},
    deleteProgram() {},
    deleteFramebuffer() {},
    deleteBuffer() {},
    deleteVertexArray() {},
  };

  const canvas = {
    width: 0,
    height: 0,
    getContext(kind) {
      return kind === "webgl2" ? gl : null;
    },
  };
  return { canvas, gl, draws, texAllocations };
}

function makeGroup(start, count) {
  return [
    start,
    count,
    1, 1, 1, 1,
    0, 0, 0, 1,
    0, 0, 0, 0.5,
    1, 1,
  ];
}

test("video and subtitles stay in the picture while danmaku spans pillarbox bars", () => {
  const { canvas, draws, texAllocations } = createMockWebGL2();
  const renderer = new Renderer(canvas);
  renderer.size(1600, 720, 1280, 720);
  assert.equal(renderer.width, 1280);
  assert.equal(renderer.height, 720);
  assert.equal(renderer.x, 160);
  assert.equal(renderer.y, 0);

  renderer.hasVideo = true;
  renderer.begin();
  renderer.subtitleImages = [
    { texture: { id: 999 }, rect: [10, 20, 200, 40], color: [1, 1, 1, 1] },
  ];
  renderer.subtitles();

  assert.equal(draws.length, 2);
  // Video and subtitle draws stay in the aspect-fit picture viewport.
  for (const draw of draws) {
    assert.deepEqual(draw.viewport, [160, 0, 1280, 720]);
    assert.deepEqual(draw.uniforms.get("resolution"), [1280, 720]);
  }

  // Two glyphs placed wholly inside the left (x < 160) and right (x > 1440) bars.
  renderer.danmaku({
    glyphs: new Float32Array([
      12, 30, 80, 24, 0, 0, 0.1, 0.1,
      1500, 30, 80, 24, 0.1, 0, 0.1, 0.1,
    ]),
    groups: new Float32Array([...makeGroup(0, 1), ...makeGroup(1, 1)]),
    pictures: new Float32Array(0),
  });

  assert.equal(renderer.scratchSize, "1600x720");
  assert.ok(
    texAllocations.some((t) => t.width === 1600 && t.height === 720),
    "scratch framebuffer must match the full canvas size",
  );

  const maskDraws = draws.filter((d) => d.program === renderer.mask);
  const composeDraws = draws.filter((d) => d.program === renderer.compose);
  assert.equal(maskDraws.length, 2, "glyphs inside left and right bars must not be clipped");
  assert.equal(composeDraws.length, 2);

  for (const draw of maskDraws) {
    assert.equal(draw.framebuffer, renderer.fbo);
    assert.deepEqual(draw.viewport, [0, 0, 1600, 720]);
    assert.deepEqual(draw.uniforms.get("resolution"), [1600, 720]);
  }
  for (const draw of composeDraws) {
    assert.equal(draw.framebuffer, null);
    assert.deepEqual(draw.viewport, [0, 0, 1600, 720]);
    assert.deepEqual(draw.uniforms.get("resolution"), [1600, 720]);
    assert.deepEqual(draw.uniforms.get("origin"), [0, 0]);
  }
  assert.deepEqual(composeDraws[0].uniforms.get("rect"), [12, 30, 80, 24]);
  assert.deepEqual(composeDraws[1].uniforms.get("rect"), [1500, 30, 80, 24]);
  renderer.destroy();
});

test("emoji-only danmaku uses the full canvas viewport across letterbox bars", () => {
  const { canvas, draws } = createMockWebGL2();
  const renderer = new Renderer(canvas);
  renderer.size(1280, 1000, 1280, 720);
  assert.equal(renderer.x, 0);
  assert.equal(renderer.y, 140);

  renderer.hasVideo = true;
  renderer.begin();
  renderer.danmaku({
    glyphs: new Float32Array(0),
    groups: new Float32Array(0),
    pictures: new Float32Array([40, 20, 32, 32, 0, 0, 1, 1, 0.9]),
  });

  const pictureDraws = draws.filter((d) => d.program === renderer.picture);
  assert.equal(pictureDraws.length, 1);
  assert.equal(pictureDraws[0].framebuffer, null);
  assert.deepEqual(pictureDraws[0].viewport, [0, 0, 1280, 1000]);
  assert.deepEqual(pictureDraws[0].uniforms.get("resolution"), [1280, 1000]);
  assert.deepEqual(pictureDraws[0].uniforms.get("rect"), [40, 20, 32, 32]);
  renderer.destroy();
});
