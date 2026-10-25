import init, { WebDanmaku, fallbackFont } from "./erika_core.js";
import createMedia from "./media.js";
import { Renderer } from "./renderer.js";
import { Source } from "./source.js";
import { supportedAudio, supportedVideo, videoPacketGate } from "./codecs.js";

let media,
  source,
  renderer,
  danmaku,
  info,
  video,
  audio,
  videoTrack,
  audioTrack,
  videoConfig,
  audioConfig,
  acceptVideoPacket;
let frames = [],
  currentFrame = null,
  time = 0,
  generation = 0,
  eof = false,
  flushed = false,
  loaded = false,
  demuxBusy = false,
  queue = Promise.resolve(),
  scheduled = false;
let packet = null,
  seekTarget = 0,
  lastStats = 0,
  decodeError = null,
  subtitleDirty = true,
  subtitleScale = 1,
  subtitleSampling = 2,
  dpr = 1,
  danmakuDelay = 0,
  hardwareDecode = true;
const stats = {
  decodedVideo: 0,
  decodedAudio: 0,
  droppedVideo: 0,
  packets: 0,
  maxVideoQueue: 0,
};
const send = (type, data = {}, transfer = []) =>
  postMessage({ type, ...data }, transfer);
const enqueue = (fn) => {
  const task = queue.then(fn);
  queue = task.catch(() => {});
  return task;
};
const check = (result) => {
  if (result < 0)
    throw new Error(
      media.sourceError || media.UTF8ToString(media._erika_error()),
    );
  return result;
};
async function call(name, types = [], values = []) {
  demuxBusy = true;
  try {
    return check(
      await media.ccall(name, "number", types, values, { async: true }),
    );
  } finally {
    demuxBusy = false;
  }
}
function bytesCall(name, bytes, ...prefix) {
  const ptr = media._malloc(bytes.length + 1);
  if (!ptr) throw new Error("WASM allocation failed");
  try {
    media.HEAPU8.set(bytes, ptr);
    media.HEAPU8[ptr + bytes.length] = 0;
    return check(
      media.ccall(
        name,
        "number",
        [...prefix.map(() => "string"), "number", "number"],
        [...prefix, ptr, bytes.length],
      ),
    );
  } finally {
    media._free(ptr);
  }
}
function disposeDecoders() {
  for (const d of [video, audio]) if (d && d.state !== "closed") d.close();
  video = audio = null;
  for (const f of frames) f.close();
  frames = [];
  currentFrame?.close();
  currentFrame = null;
}
function makeDecoders() {
  const epoch = generation;
  decodeError = null;
  acceptVideoPacket = videoPacketGate(videoTrack);
  video = new VideoDecoder({
    output(frame) {
      if (epoch !== generation) {
        frame.close();
        return;
      }
      frames.push(frame);
      frames.sort((a, b) => a.timestamp - b.timestamp);
      stats.decodedVideo++;
      stats.maxVideoQueue = Math.max(stats.maxVideoQueue, frames.length);
    },
    error(error) {
      if (epoch !== generation) return;
      decodeError = error;
      send("error", { message: `Video decoder: ${error}` });
    },
  });
  video.configure(videoConfig);
  if (audioTrack) {
    audio = new AudioDecoder({
      output(data) {
        try {
          if (epoch !== generation) return;
          const start = data.timestamp / 1e6,
            end = start + data.numberOfFrames / data.sampleRate;
          if (end <= seekTarget) return;
          const skip = Math.max(
              0,
              Math.ceil((seekTarget - start) * data.sampleRate),
            ),
            count = data.numberOfFrames - skip;
          if (count <= 0) return;
          const planes = [];
          for (let ch = 0; ch < data.numberOfChannels; ch++) {
            const plane = new Float32Array(count);
            data.copyTo(plane, {
              planeIndex: ch,
              format: "f32-planar",
              frameOffset: skip,
              frameCount: count,
            });
            planes.push(plane);
          }
          send(
            "audio",
            {
              generation: epoch,
              timestamp: Math.max(seekTarget, start),
              sampleRate: data.sampleRate,
              planes,
            },
            planes.map((p) => p.buffer),
          );
          stats.decodedAudio++;
        } finally {
          data.close();
        }
      },
      error(error) {
        if (epoch !== generation) return;
        decodeError = error;
        send("error", { message: `Audio decoder: ${error}` });
      },
    });
    audio.configure(audioConfig);
  }
}
async function selectTracks(videoId, audioId) {
  const videos = info.tracks.filter(
    (t) => t.type === "video" && (videoId == null || t.id === videoId),
  );
  let errors = [];
  videoTrack = null;
  for (const t of videos) {
    try {
      videoConfig = await supportedVideo(t, globalThis.VideoDecoder, hardwareDecode);
      videoTrack = t;
      break;
    } catch (e) {
      errors.push(String(e));
    }
  }
  if (!videoTrack) throw new Error(errors.join("; ") || "No video track");
  audioTrack = null;
  if (
    audioId != null &&
    audioId !== -1 &&
    !info.tracks.some((t) => t.type === "audio" && t.id === audioId)
  )
    throw new Error("Unknown audio track");
  const audios = info.tracks.filter(
    (t) => t.type === "audio" && (audioId == null || t.id === audioId),
  );
  errors = [];
  if (audioId !== -1)
    for (const t of audios) {
      try {
        audioConfig = await supportedAudio(t);
        audioTrack = t;
        break;
      } catch (e) {
        errors.push(String(e));
      }
    }
  if (audioId !== -1 && audios.length && !audioTrack)
    throw new Error(errors.join("; "));
}
function schedulePump() {
  if (scheduled || !loaded || eof || decodeError) return;
  scheduled = true;
  setTimeout(() => {
    scheduled = false;
    enqueue(pump).catch((e) => {
      decodeError = e;
      send("error", { message: String(e) });
    });
  }, 0);
}
async function pump() {
  if (!loaded || eof || decodeError) return;
  const deadline = performance.now() + 8;
  let loops = 0;
  while (loaded && !eof && loops++ < 64 && performance.now() < deadline) {
    if (!packet) {
      const result = await call("erika_read");
      if (!result) {
        eof = true;
        break;
      }
      stats.packets++;
      const stream = media._erika_packet_stream(),
        size = media._erika_packet_size(),
        ptr = media._erika_packet_data();
      if (stream !== videoTrack.id && stream !== audioTrack?.id) continue;
      packet = {
        stream,
        timestamp: Math.round(media._erika_packet_time()),
        duration: Math.max(0, Math.round(media._erika_packet_duration())),
        type: media._erika_packet_key() ? "key" : "delta",
        data: media.HEAPU8.slice(ptr, ptr + size),
      };
    }
    if (
      packet.timestamp / 1e6 > time + 1.5 ||
      frames.length >= 16 ||
      video.decodeQueueSize >= 8 ||
      (audio?.decodeQueueSize || 0) >= 16
    )
      return;
    const p = packet;
    packet = null;
    if (p.stream === videoTrack.id) {
      if (!acceptVideoPacket(p)) continue;
      const chunk = { timestamp: p.timestamp, type: p.type, data: p.data };
      if (p.duration > 0) chunk.duration = p.duration;
      video.decode(new EncodedVideoChunk(chunk));
    } else if (audio) {
      const chunk = { timestamp: p.timestamp, type: "key", data: p.data };
      if (p.duration > 0) chunk.duration = p.duration;
      audio.decode(new EncodedAudioChunk(chunk));
    }
  }
  if (eof && !flushed) {
    flushed = true;
    await Promise.all([video.flush(), audio?.flush()]);
    send("eof", { generation });
  } else schedulePump();
}
function draw(t) {
  if (!loaded || !renderer) return;
  time = Math.max(seekTarget, t);
  let chosen = null;
  while (frames.length && frames[0].timestamp / 1e6 <= time + 0.008) {
    if (chosen) {
      chosen.close();
      stats.droppedVideo++;
    }
    chosen = frames.shift();
  }
  if (chosen) {
    currentFrame?.close();
    currentFrame = chosen;
    renderer.upload(chosen);
  }
  if (
    !currentFrame &&
    frames.length &&
    frames[0].timestamp / 1e6 < seekTarget + 0.1
  ) {
    currentFrame = frames.shift();
    renderer.upload(currentFrame);
  }
  renderer.begin();
  if (!demuxBusy) {
    const head = media._erika_subtitle_render(
      time,
      Math.round(renderer.width * subtitleSampling),
      Math.round(renderer.height * subtitleSampling),
      subtitleScale,
    );
    if (subtitleDirty || media._erika_subtitle_changed()) {
      renderer.updateSubtitles(media, head, subtitleSampling);
      subtitleDirty = false;
    }
  }
  renderer.subtitles();
  renderer.danmaku(
    danmaku.frame(
      Math.max(0, time - danmakuDelay),
      renderer.width,
      renderer.height,
      dpr,
      performance.now(),
    ),
  );
  if (performance.now() - lastStats > 250) {
    lastStats = performance.now();
    send("stats", {
      stats: {
        ...stats,
        time,
        videoQueue: frames.length,
        videoBufferedUntil: Math.max(
          seekTarget,
          ...[currentFrame, ...frames]
            .filter(Boolean)
            .map((f) => (f.timestamp + (f.duration || 41667)) / 1e6),
        ),
        rendered: renderer.frames,
        glyphs: renderer.glyphs,
        atlasUploads: renderer.uploads,
        subtitleImages: renderer.subtitleImages?.length || 0,
        hasVideo: !!currentFrame,
        eof,
      },
    });
  }
  schedulePump();
}
const methods = {
  async init({ canvas, width, height, scale }) {
    await init();
    media = await createMedia({
      printErr: (message) => send("log", { message }),
      readSource: (offset, dest, length) =>
        source.read(media, offset, dest, length),
    });
    renderer = new Renderer(canvas);
    danmaku = new WebDanmaku();
    dpr = scale;
    renderer.size(width, height, 16, 9);
  },
  async open({ input, epoch, hardwareDecode: hardware = true }) {
    hardwareDecode = hardware;
    loaded = false;
    generation = epoch;
    for (const key of Object.keys(stats)) stats[key] = 0;
    disposeDecoders();
    source?.close();
    source = new Source();
    await source.open(input);
    media.sourceError = null;
    await call("erika_open", ["number", "string"], [source.size, source.name]);
    info = JSON.parse(media.UTF8ToString(media._erika_info()));
    for (const track of info.tracks) {
      track.extra = media.HEAPU8.slice(
        track.extraPtr,
        track.extraPtr + track.extraSize,
      );
      delete track.extraPtr;
      delete track.extraSize;
    }
    // Reuse the font embedded in Rust; each media open creates a new libass
    // library, so register it again without fetching a second packaged copy.
    bytesCall("erika_add_font", fallbackFont(), "subfont.ttf");
    await selectTracks();
    makeDecoders();
    const sub = info.tracks.find(
      (t) => t.type === "subtitle" && ["ass", "ssa"].includes(t.codec),
    );
    check(media._erika_subtitle_select(sub?.id ?? -1));
    packet = null;
    eof = false;
    flushed = false;
    time = seekTarget = 0;
    subtitleDirty = true;
    renderer.clearSubtitles();
    renderer.hasVideo = false;
    danmaku.load("[]");
    loaded = true;
    renderer.size(
      renderer.canvas.width,
      renderer.canvas.height,
      videoTrack.width,
      videoTrack.height,
    );
    schedulePump();
    return {
      ...info,
      videoTrack: videoTrack.id,
      audioTrack: audioTrack?.id ?? null,
      subtitleTrack: sub?.id ?? null,
      decoder: videoConfig.codec,
      backend: "webcodecs",
      hardwarePreference: videoConfig.hardwareAcceleration,
    };
  },
  async seek({ seconds, epoch }) {
    send("log", { message: "web seek begin" });
    generation = epoch;
    seekTarget = time = Math.max(0, seconds);
    disposeDecoders();
    packet = null;
    eof = false;
    flushed = false;
    send("log", { message: "web seek demux" });
    await call("erika_seek", ["number"], [Math.max(0, time - 5)]);
    send("log", { message: "web seek decoder" });
    makeDecoders();
    danmaku.reset();
    renderer.clearSubtitles();
    subtitleDirty = true;
    schedulePump();
  },
  async audioTrack({ id, seconds, epoch }) {
    await selectTracks(videoTrack.id, id);
    await methods.seek({ seconds, epoch });
    return audioTrack?.id ?? null;
  },
  async subtitleTrack({ id, seconds, epoch }) {
    send("log", { message: "web subtitle select" });
    check(media._erika_subtitle_select(id ?? -1));
    send("log", { message: "web subtitle selected" });
    await methods.seek({ seconds, epoch });
  },
  async subtitles({ text }) {
    bytesCall("erika_subtitle_load", new TextEncoder().encode(text));
    renderer.clearSubtitles();
    subtitleDirty = true;
  },
  async font({ name, data }) {
    bytesCall("erika_add_font", data, name);
    danmaku.add_font(name, data);
    subtitleDirty = true;
  },
  async danmaku({ text }) {
    return danmaku.append(text);
  },
  async configure({ config }) {
    if (Number.isFinite(config.delay)) danmakuDelay = Math.max(-60, Math.min(60, config.delay));
    danmaku.configure(JSON.stringify(config), performance.now());
  },
  async subtitleOptions({ scale, supersampling }) {
    if (Number.isFinite(scale))
      subtitleScale = Math.max(0.25, Math.min(4, scale));
    if (supersampling !== undefined) subtitleSampling = supersampling ? 2 : 1;
    subtitleDirty = true;
  },
  async sendDanmaku({ text, color, seconds }) {
    danmaku.send(
      text,
      Math.max(0, seconds + 0.2 - danmakuDelay),
      Number.isInteger(color) ? color & 0xffffff : undefined,
    );
  },
  async capture({ width, seconds }) {
    if (!loaded) throw new Error("No media to capture");
    draw(seconds);
    const w = Math.max(1, Math.min(8192, width || renderer.width));
    const h = Math.max(1, Math.round(w * renderer.canvas.height / renderer.canvas.width));
    const target = new OffscreenCanvas(w, h);
    target.getContext("2d").drawImage(renderer.canvas, 0, 0, w, h);
    return target.convertToBlob({ type: "image/png" });
  },
  async close({ epoch }) {
    loaded = false;
    generation = epoch;
    disposeDecoders();
    source?.close();
    source = null;
    packet = null;
    eof = flushed = false;
    media._erika_close();
    danmaku.load("[]");
    renderer.clearSubtitles();
    renderer.hasVideo = false;
    renderer.begin();
  },
  async destroy() {
    loaded = false;
    generation++;
    disposeDecoders();
    source?.close();
    media?._erika_close();
    danmaku?.free();
    renderer?.destroy();
  },
};
onmessage = ({ data }) => {
  if (data.type === "tick") {
    try {
      draw(data.time);
    } catch (e) {
      loaded = false;
      send("error", { message: String(e) });
    } finally {
      send("ticked");
    }
    return;
  }
  if (data.type === "resize") {
    dpr = data.scale;
    renderer?.size(
      data.width,
      data.height,
      videoTrack?.width || 16,
      videoTrack?.height || 9,
    );
    subtitleDirty = true;
    return;
  }
  const method = methods[data.type];
  if (!method) {
    send("reply", {
      requestId: data.requestId,
      error: `Unknown command ${data.type}`,
    });
    return;
  }
  enqueue(() => method(data)).then(
    (result) => send("reply", { requestId: data.requestId, result }),
    (error) =>
      send("reply", { requestId: data.requestId, error: String(error) }),
  );
};
