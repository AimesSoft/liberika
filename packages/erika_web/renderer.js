// WebGL2 presentation. VideoFrame stays in the browser; only glyph masks cross
// the WASM boundary. A comment's masks are unioned before applying opacity once.
const quad = `const vec2 q[6]=vec2[6](vec2(0,0),vec2(1,0),vec2(0,1),vec2(0,1),vec2(1,0),vec2(1,1));`;
const vertex = `#version 300 es
precision highp float; ${quad}
uniform vec2 resolution; uniform vec4 rect; out vec2 uv;
void main(){uv=q[gl_VertexID]; vec2 p=rect.xy+uv*rect.zw; gl_Position=vec4(p/resolution*vec2(2,-2)+vec2(-1,1),0,1);}`;
const fragment = `#version 300 es
precision highp float; in vec2 uv; uniform sampler2D image; uniform vec4 color; uniform int mode; out vec4 pixel;
void main(){vec4 t=texture(image,uv); pixel=mode==0?t:vec4(color.rgb*color.a*t.r,color.a*t.r);}`;
const pictureVertex = `#version 300 es
precision highp float; ${quad}
uniform vec2 resolution; uniform vec4 rect; uniform vec4 texRect; out vec2 uv;
void main(){uv=texRect.xy+q[gl_VertexID]*texRect.zw; vec2 p=rect.xy+q[gl_VertexID]*rect.zw; gl_Position=vec4(p/resolution*vec2(2,-2)+vec2(-1,1),0,1);}`;
const pictureFragment = `#version 300 es
precision highp float; in vec2 uv; uniform sampler2D image; uniform float opacity; out vec4 pixel;
void main(){pixel=texture(image,uv)*opacity;}`;
const maskVertex = `#version 300 es
precision highp float; ${quad}
layout(location=0) in vec4 box; layout(location=1) in vec4 tex;
uniform vec2 resolution; out vec2 uv; out vec4 bounds; out vec2 ratio;
void main(){vec2 p=box.xy+q[gl_VertexID]*box.zw;uv=tex.xy+q[gl_VertexID]*tex.zw; bounds=tex;ratio=tex.zw/box.zw; gl_Position=vec4(p/resolution*vec2(2,-2)+vec2(-1,1),0,1);}`;
const maskFragment = `#version 300 es
precision highp float; in vec2 uv; in vec4 bounds; in vec2 ratio;
uniform sampler2D fillMask; uniform sampler2D outlineMask; uniform vec2 shadowOffset; out vec4 pixel;
void main(){vec2 p=uv-shadowOffset*ratio;float shadow=0.;if(all(greaterThanEqual(p,bounds.xy))&&all(lessThanEqual(p,bounds.xy+bounds.zw)))shadow=texture(outlineMask,p).r;
pixel=vec4(texture(fillMask,uv).r,texture(outlineMask,uv).r,shadow,0.);}`;
const composeFragment = `#version 300 es
precision highp float; uniform sampler2D image; uniform vec2 resolution;uniform vec2 origin;uniform vec4 fillColor;uniform vec4 outlineColor;uniform float shadowStrength;out vec4 pixel;
void main(){vec3 m=texture(image,(gl_FragCoord.xy-origin)/resolution).rgb;float s=m.b*shadowStrength;float a=m.r+(1.-m.r)*(m.g+(1.-m.g)*s);
vec3 rgb=fillColor.rgb*m.r+outlineColor.rgb*m.g*(1.-m.r);pixel=vec4(rgb,a)*fillColor.a;}`;
export class Renderer {
  constructor(canvas) {
    this.canvas = canvas;
    const gl = (this.gl = canvas.getContext("webgl2", {
      alpha: false,
      antialias: false,
      depth: false,
      stencil: false,
      preserveDrawingBuffer: false,
    }));
    if (!gl) throw new Error("WebGL2 is unavailable");
    this.basic = this.program(vertex, fragment);
    this.mask = this.program(maskVertex, maskFragment);
    this.compose = this.program(vertex, composeFragment);
    this.picture = this.program(pictureVertex, pictureFragment);
    this.video = this.texture();
    this.fill = this.texture();
    this.outline = this.texture();
    this.scratch = this.texture();
    this.sub = this.texture();
    this.emoji = this.texture();
    this.fbo = gl.createFramebuffer();
    this.instances = gl.createBuffer();
    this.vao = gl.createVertexArray();
    gl.bindVertexArray(this.vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.instances);
    for (let i = 0; i < 2; i++) {
      gl.enableVertexAttribArray(i);
      gl.vertexAttribPointer(i, 4, gl.FLOAT, false, 32, i * 16);
      gl.vertexAttribDivisor(i, 1);
    }
    gl.bindVertexArray(null);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    this.width = 0;
    this.height = 0;
    this.uploads = 0;
    this.frames = 0;
    this.glyphs = 0;
    this.scratchSize = "";
  }
  program(v, f) {
    const g = this.gl,
      p = g.createProgram();
    for (const [type, source] of [
      [g.VERTEX_SHADER, v],
      [g.FRAGMENT_SHADER, f],
    ]) {
      const s = g.createShader(type);
      g.shaderSource(s, source);
      g.compileShader(s);
      if (!g.getShaderParameter(s, g.COMPILE_STATUS))
        throw new Error(g.getShaderInfoLog(s));
      g.attachShader(p, s);
      g.deleteShader(s);
    }
    g.linkProgram(p);
    if (!g.getProgramParameter(p, g.LINK_STATUS))
      throw new Error(g.getProgramInfoLog(p));
    return p;
  }
  texture() {
    const g = this.gl,
      t = g.createTexture();
    g.bindTexture(g.TEXTURE_2D, t);
    g.texParameteri(g.TEXTURE_2D, g.TEXTURE_MIN_FILTER, g.LINEAR);
    g.texParameteri(g.TEXTURE_2D, g.TEXTURE_MAG_FILTER, g.LINEAR);
    g.texParameteri(g.TEXTURE_2D, g.TEXTURE_WRAP_S, g.CLAMP_TO_EDGE);
    g.texParameteri(g.TEXTURE_2D, g.TEXTURE_WRAP_T, g.CLAMP_TO_EDGE);
    return t;
  }
  uniform(p, n, ...v) {
    const g = this.gl,
      l = g.getUniformLocation(p, n);
    if (v.length === 1) g.uniform1f(l, v[0]);
    else if (v.length === 2) g.uniform2f(l, ...v);
    else g.uniform4f(l, ...v);
  }
  bind(t, unit = 0) {
    const g = this.gl;
    g.activeTexture(g.TEXTURE0 + unit);
    g.bindTexture(g.TEXTURE_2D, t);
  }
  size(w, h, vw, vh) {
    this.canvas.width = w;
    this.canvas.height = h;
    const ratio = Math.min(w / vw, h / vh);
    this.width = Math.max(1, Math.round(vw * ratio));
    this.height = Math.max(1, Math.round(vh * ratio));
    this.x = Math.floor((w - this.width) / 2);
    this.y = Math.floor((h - this.height) / 2);
  }
  upload(frame) {
    const g = this.gl;
    this.bind(this.video);
    g.texImage2D(g.TEXTURE_2D, 0, g.RGBA, g.RGBA, g.UNSIGNED_BYTE, frame);
    this.hasVideo = true;
  }
  begin() {
    const g = this.gl;
    g.bindVertexArray(null);
    g.bindFramebuffer(g.FRAMEBUFFER, null);
    g.disable(g.SCISSOR_TEST);
    g.viewport(0, 0, this.canvas.width, this.canvas.height);
    g.clearColor(0, 0, 0, 1);
    g.clear(g.COLOR_BUFFER_BIT);
    g.viewport(this.x, this.y, this.width, this.height);
    g.disable(g.BLEND);
    if (this.hasVideo) this.draw(this.video, [0, 0, this.width, this.height]);
    g.enable(g.BLEND);
    g.blendEquation(g.FUNC_ADD);
    g.blendFunc(g.ONE, g.ONE_MINUS_SRC_ALPHA);
    this.frames++;
  }
  draw(texture, rect, color = null) {
    const g = this.gl,
      p = this.basic;
    g.useProgram(p);
    this.uniform(p, "resolution", this.width, this.height);
    this.uniform(p, "rect", ...rect);
    this.bind(texture);
    g.uniform1i(g.getUniformLocation(p, "image"), 0);
    g.uniform1i(g.getUniformLocation(p, "mode"), color ? 1 : 0);
    if (color) this.uniform(p, "color", ...color);
    g.drawArrays(g.TRIANGLES, 0, 6);
  }
  updateSubtitles(module, head, scale) {
    const g = this.gl;
    this.clearSubtitles();
    this.subtitleImages = [];
    let count = 0;
    for (let i = head; i && count++ < 8192; i = module._erika_image_next(i)) {
      const w = module._erika_image_width(i),
        h = module._erika_image_height(i),
        stride = module._erika_image_stride(i);
      if (w <= 0 || h <= 0) continue;
      const ptr = module._erika_image_data(i),
        length = stride * (h - 1) + w;
      if (ptr + length > module.HEAPU8.length)
        throw new Error("Invalid libass bitmap");
      const packed = new Uint8Array(w * h);
      for (let y = 0; y < h; y++)
        packed.set(
          module.HEAPU8.subarray(ptr + y * stride, ptr + y * stride + w),
          y * w,
        );
      const texture = this.texture();
      this.bind(texture);
      g.texImage2D(
        g.TEXTURE_2D,
        0,
        g.R8,
        w,
        h,
        0,
        g.RED,
        g.UNSIGNED_BYTE,
        packed,
      );
      const c = module._erika_image_color(i) >>> 0;
      this.subtitleImages.push({
        texture,
        rect: [
          module._erika_image_x(i) / scale,
          module._erika_image_y(i) / scale,
          w / scale,
          h / scale,
        ],
        color: [
          (c >>> 24) / 255,
          ((c >>> 16) & 255) / 255,
          ((c >>> 8) & 255) / 255,
          1 - (c & 255) / 255,
        ],
      });
    }
  }
  clearSubtitles() {
    for (const image of this.subtitleImages || [])
      this.gl.deleteTexture(image.texture);
    this.subtitleImages = [];
  }
  subtitles() {
    for (const image of this.subtitleImages || [])
      this.draw(image.texture, image.rect, image.color);
  }
  danmaku(plan) {
    const g = this.gl,
      w = this.width,
      h = this.height;
    this.glyphs = plan.glyphs.length / 8;
    if (plan.fill) {
      for (const [texture, data] of [
        [this.fill, plan.fill],
        [this.outline, plan.outline],
      ]) {
        this.bind(texture);
        if (plan.x !== undefined)
          g.texSubImage2D(
            g.TEXTURE_2D,
            0,
            plan.x,
            plan.y,
            plan.width,
            plan.height,
            g.RED,
            g.UNSIGNED_BYTE,
            data,
          );
        else
          g.texImage2D(
            g.TEXTURE_2D,
            0,
            g.R8,
            plan.width,
            plan.height,
            0,
            g.RED,
            g.UNSIGNED_BYTE,
            data,
          );
      }
      this.uploads++;
    }
    if (plan.pictureAtlas) {
      this.bind(this.emoji);
      g.texImage2D(
        g.TEXTURE_2D,
        0,
        g.RGBA8,
        plan.pictureWidth,
        plan.pictureHeight,
        0,
        g.RGBA,
        g.UNSIGNED_BYTE,
        plan.pictureAtlas,
      );
    }
    if (!plan.glyphs.length) return this.pictures(plan);
    if (this.scratchSize !== `${w}x${h}`) {
      this.bind(this.scratch);
      g.texImage2D(
        g.TEXTURE_2D,
        0,
        g.RGBA8,
        w,
        h,
        0,
        g.RGBA,
        g.UNSIGNED_BYTE,
        null,
      );
      g.bindFramebuffer(g.FRAMEBUFFER, this.fbo);
      g.framebufferTexture2D(
        g.FRAMEBUFFER,
        g.COLOR_ATTACHMENT0,
        g.TEXTURE_2D,
        this.scratch,
        0,
      );
      if (g.checkFramebufferStatus(g.FRAMEBUFFER) !== g.FRAMEBUFFER_COMPLETE)
        throw new Error("Incomplete mask framebuffer");
      this.scratchSize = `${w}x${h}`;
    }
    g.bindBuffer(g.ARRAY_BUFFER, this.instances);
    g.bufferData(g.ARRAY_BUFFER, plan.glyphs, g.DYNAMIC_DRAW);
    for (let n = 0; n < plan.groups.length; n += 16) {
      const a = plan.groups.subarray(n, n + 16),
        start = a[0],
        count = a[1];
      let x = w,
        y = h,
        right = 0,
        bottom = 0;
      for (let j = start * 8; j < (start + count) * 8; j += 8) {
        x = Math.min(x, plan.glyphs[j]);
        y = Math.min(y, plan.glyphs[j + 1]);
        right = Math.max(right, plan.glyphs[j] + plan.glyphs[j + 2]);
        bottom = Math.max(bottom, plan.glyphs[j + 1] + plan.glyphs[j + 3]);
      }
      x = Math.max(0, Math.floor(x));
      y = Math.max(0, Math.floor(y));
      right = Math.min(w, Math.ceil(right));
      bottom = Math.min(h, Math.ceil(bottom));
      if (right <= x || bottom <= y) continue;
      g.bindFramebuffer(g.FRAMEBUFFER, this.fbo);
      g.viewport(0, 0, w, h);
      g.enable(g.SCISSOR_TEST);
      g.scissor(x, h - bottom, right - x, bottom - y);
      g.clearColor(0, 0, 0, 0);
      g.clear(g.COLOR_BUFFER_BIT);
      g.blendEquation(g.MAX);
      g.blendFunc(g.ONE, g.ONE);
      g.useProgram(this.mask);
      this.uniform(this.mask, "resolution", w, h);
      this.uniform(this.mask, "shadowOffset", a[14], a[15]);
      this.bind(this.fill, 0);
      this.bind(this.outline, 1);
      g.uniform1i(g.getUniformLocation(this.mask, "fillMask"), 0);
      g.uniform1i(g.getUniformLocation(this.mask, "outlineMask"), 1);
      g.bindVertexArray(this.vao);
      for (let j = 0; j < 2; j++)
        g.vertexAttribPointer(j, 4, g.FLOAT, false, 32, start * 32 + j * 16);
      g.drawArraysInstanced(g.TRIANGLES, 0, 6, count);
      g.bindVertexArray(null);
      g.bindFramebuffer(g.FRAMEBUFFER, null);
      g.viewport(this.x, this.y, w, h);
      g.disable(g.SCISSOR_TEST);
      g.blendEquation(g.FUNC_ADD);
      g.blendFunc(g.ONE, g.ONE_MINUS_SRC_ALPHA);
      g.useProgram(this.compose);
      this.uniform(this.compose, "resolution", w, h);
      this.uniform(this.compose, "origin", this.x, this.y);
      this.uniform(this.compose, "rect", x, y, right - x, bottom - y);
      this.uniform(this.compose, "fillColor", ...a.subarray(2, 6));
      this.uniform(this.compose, "outlineColor", ...a.subarray(6, 10));
      this.uniform(this.compose, "shadowStrength", a[5] > 0 ? a[13] / a[5] : 0);
      this.bind(this.scratch, 0);
      g.uniform1i(g.getUniformLocation(this.compose, "image"), 0);
      g.drawArrays(g.TRIANGLES, 0, 6);
    }
    this.pictures(plan);
  }
  /** Emoji go over the text of their comment, in their own colors. */
  pictures(plan) {
    if (!plan.pictures?.length) return;
    const g = this.gl,
      p = this.picture;
    g.useProgram(p);
    this.uniform(p, "resolution", this.width, this.height);
    this.bind(this.emoji, 0);
    g.uniform1i(g.getUniformLocation(p, "image"), 0);
    for (let n = 0; n < plan.pictures.length; n += 9) {
      const a = plan.pictures;
      this.uniform(p, "rect", a[n], a[n + 1], a[n + 2], a[n + 3]);
      this.uniform(p, "texRect", a[n + 4], a[n + 5], a[n + 6], a[n + 7]);
      this.uniform(p, "opacity", a[n + 8]);
      g.drawArrays(g.TRIANGLES, 0, 6);
    }
  }
  destroy() {
    this.clearSubtitles();
    const g = this.gl;
    for (const t of [
      this.video,
      this.fill,
      this.outline,
      this.scratch,
      this.sub,
      this.emoji,
    ])
      g.deleteTexture(t);
    for (const p of [this.basic, this.mask, this.compose, this.picture])
      g.deleteProgram(p);
    g.deleteFramebuffer(this.fbo);
    g.deleteBuffer(this.instances);
    g.deleteVertexArray(this.vao);
  }
}
