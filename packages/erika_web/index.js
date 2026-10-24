import { AudioSession } from "./audio.js";
export { AudioSession } from "./audio.js";

/** liberika browser backend: one player owns demux, WebCodecs, libass, danmaku
 * and WebGL presentation. Serve this package unchanged over HTTPS/localhost. */
export class Player extends EventTarget {
  static async create({ canvas, volume = 1, audioSession } = {}) {
    if (!canvas?.transferControlToOffscreen)
      throw new Error(
        "An HTML canvas with OffscreenCanvas support is required",
      );
    const player = new Player(canvas, volume, audioSession);
    const offscreen = canvas.transferControlToOffscreen();
    try {
      await player.request("init", { canvas: offscreen, ...player.size() }, [
        offscreen,
      ]);
      player.observe();
      return player;
    } catch (e) {
      await player.destroy();
      throw e;
    }
  }
  constructor(canvas, volume, audioSession) {
    super();
    this.canvas = canvas;
    this.worker = new Worker(new URL("./worker.js", import.meta.url), {
      type: "module",
    });
    this.pending = new Map();
    this.nextId = 1;
    this.generation = 0;
    this.sources = new Set();
    this.rate = 1;
    this.playing = false;
    this.disposed = false;
    this.position = 0;
    this.info = null;
    this.stats = {};
    this.audioBufferedUntil = 0;
    this.eof = false;
    this.audio = audioSession ?? new AudioSession();
    this.context = this.audio.context;
    this.audio.addEventListener("statechange", () => {
      if (this.disposed || this.playing === this.audio.playing) return;
      if (!this.audio.playing) this.position = this.currentTime;
      this.playing = this.audio.playing;
      this.dispatchEvent(new Event(this.playing ? "play" : "pause"));
    });
    this.audio.addEventListener("error", (event) => this.fail(event.detail));
    this.audio.addEventListener("blocked", () => this.dispatchEvent(new Event("playblocked")));
    this.gain = this.context.createGain();
    this.gain.gain.value = Math.max(0, Math.min(1, volume));
    this.gain.connect(this.context.destination);
    this.resetClock(0);
    this.worker.onmessage = (event) => this.message(event.data);
    this.worker.onerror = (event) => {
      this.broken = true;
      this.fail(new Error(event.message));
    };
  }
  size() {
    const r = this.canvas.getBoundingClientRect(),
      scale = Math.min(3, devicePixelRatio || 1);
    return {
      width: Math.max(1, Math.min(8192, Math.round(r.width * scale))),
      height: Math.max(1, Math.min(8192, Math.round(r.height * scale))),
      scale,
    };
  }
  observe() {
    this.observer = new ResizeObserver(() =>
      this.worker.postMessage({ type: "resize", ...this.size() }),
    );
    this.observer.observe(this.canvas);
    const tick = () => {
      if (this.disposed) return;
      const time = this.currentTime;
      if (!this.tickInFlight) {
        this.tickInFlight = true;
        this.worker.postMessage({ type: "tick", time });
      }
      this.dispatchEvent(new CustomEvent("timeupdate", { detail: time }));
      if (
        this.playing && this.eof &&
        this.info?.duration > 0 &&
        time >= this.info.duration - 0.025
      ) {
        this.pause();
        this.position = this.info.duration;
        this.dispatchEvent(new Event("ended"));
      }
      this.raf = requestAnimationFrame(tick);
    };
    this.raf = requestAnimationFrame(tick);
  }
  request(type, args = {}, transfer = []) {
    if (this.disposed) return Promise.reject(new Error("Player is disposed"));
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.worker.postMessage({ ...args, type, requestId: id }, transfer);
    });
  }
  message(data) {
    if (data.type === "ticked") {
      this.tickInFlight = false;
      return;
    }
    if (data.type === "reply") {
      const pending = this.pending.get(data.requestId);
      this.pending.delete(data.requestId);
      if (pending) {
        if (data.error) pending.reject(new Error(data.error));
        else pending.resolve(data.result);
      }
      return;
    }
    if (data.type === "audio") {
      if (data.generation === this.generation) this.scheduleAudio(data);
      return;
    }
    if (data.type === "stats") {
      this.stats = data.stats;
      this.dispatchEvent(new CustomEvent("stats", { detail: data.stats }));
      return;
    }
    if (data.type === "eof" && data.generation === this.generation) {
      this.eof = true;
      return;
    }
    if (data.type === "error") {
      this.fail(new Error(data.message));
      return;
    }
    if (data.type === "log")
      this.dispatchEvent(new CustomEvent("log", { detail: data.message }));
  }
  fail(error) {
    this.pause();
    for (const p of this.pending.values()) p.reject(error);
    this.pending.clear();
    this.dispatchEvent(new CustomEvent("error", { detail: error }));
  }
  resetClock(seconds) {
    for (const source of this.sources) {
      try {
        source.stop();
      } catch {}
      source.disconnect();
    }
    this.sources.clear();
    this.position = seconds;
    this.stats = {};
    this.mediaAnchor = seconds;
    this.audioAnchor = this.context.currentTime + 0.1;
    this.audioBufferedUntil = seconds;
    this.eof = false;
  }
  scheduleAudio({ timestamp, sampleRate, planes }) {
    if (!planes.length || !planes[0].length) return;
    const buffer = this.context.createBuffer(
      planes.length,
      planes[0].length,
      sampleRate,
    );
    planes.forEach((p, i) => buffer.copyToChannel(p, i));
    const source = this.context.createBufferSource();
    source.buffer = buffer;
    source.playbackRate.value = this.rate;
    source.connect(this.gain);
    let start = this.audioAnchor + (timestamp - this.mediaAnchor) / this.rate;
    if (start < this.context.currentTime) {
      const shift = this.context.currentTime + 0.04 - start;
      this.audioAnchor += shift;
      start += shift;
    }
    this.sources.add(source);
    source.onended = () => {
      this.sources.delete(source);
      source.disconnect();
    };
    source.start(start);
    this.audioBufferedUntil = Math.max(
      this.audioBufferedUntil,
      timestamp + buffer.duration,
    );
  }
  get currentTime() {
    if (!this.playing) return this.position;
    let clock = this.context.currentTime;
    const stamp =
      this.info?.audioTrack != null
        ? this.context.getOutputTimestamp?.()
        : null;
    if (stamp?.contextTime > 0 && stamp.performanceTime > 0) {
      clock = Math.min(
        clock,
        stamp.contextTime +
          Math.max(0, performance.now() - stamp.performanceTime) / 1000,
      );
    }
    let time = Math.max(
      this.mediaAnchor,
      this.mediaAnchor + (clock - this.audioAnchor) * this.rate,
    );
    if (this.info)
      time = Math.min(
        time,
        Math.max(
          this.audioBufferedUntil,
          this.stats.videoBufferedUntil || this.mediaAnchor,
        ),
      );
    if (this.info?.duration > 0) time = Math.min(time, this.info.duration);
    return time;
  }
  async open(input, options = {}) {
    await this.pause();
    this.generation++;
    this.resetClock(0);
    this.info = null;
    this.stats = {};
    const info = await this.request("open", { input, epoch: this.generation, hardwareDecode: options.hardwareDecode !== false });
    this.info = info;
    this.dispatchEvent(new CustomEvent("loaded", { detail: info }));
    return info;
  }
  async play() {
    if (!this.info) throw new Error("Open media before playing");
    this.audio.play();
  }
  async pause() {
    this.position = this.currentTime;
    this.playing = false;
    await this.audio.pause();
    this.dispatchEvent(new Event("pause"));
  }
  async seek(seconds) {
    if (!Number.isFinite(seconds) || seconds < 0)
      throw new Error("Invalid seek time");
    const playing = this.audio.wanted;
    await this.pause();
    this.generation++;
    this.resetClock(Math.min(seconds, this.info?.duration || seconds));
    await this.request("seek", {
      seconds: this.position,
      epoch: this.generation,
    });
    if (playing) await this.play();
  }
  async selectAudio(id) {
    const playing = this.audio.wanted,
      seconds = this.currentTime;
    await this.pause();
    this.generation++;
    this.resetClock(seconds);
    const selected = await this.request("audioTrack", {
      id: id ?? -1,
      seconds,
      epoch: this.generation,
    });
    this.info.audioTrack = selected;
    if (playing) await this.play();
  }
  async selectSubtitle(id) {
    const playing = this.audio.wanted,
      seconds = this.currentTime;
    await this.pause();
    this.generation++;
    this.resetClock(seconds);
    await this.request("subtitleTrack", {
      id,
      seconds,
      epoch: this.generation,
    });
    this.info.subtitleTrack = id;
    if (playing) await this.play();
  }
  addSubtitle(text) {
    return this.request("subtitles", { text: toASS(text) });
  }
  addDanmaku(text) {
    return this.request("danmaku", { text });
  }
  sendDanmaku(text, color) {
    return this.request("sendDanmaku", { text, color, seconds: this.currentTime });
  }
  capture(width) {
    return this.request("capture", { width, seconds: this.currentTime });
  }
  async close() {
    await this.pause();
    this.generation++;
    this.resetClock(0);
    this.info = null;
    this.stats = {};
    await this.request("close", { epoch: this.generation });
  }
  configureDanmaku(config) {
    return this.request("configure", { config });
  }
  configureSubtitles(config) {
    return this.request("subtitleOptions", config);
  }
  addFont(name, data) {
    return this.request("font", { name, data: new Uint8Array(data) });
  }
  setVolume(volume) {
    if (!Number.isFinite(volume)) throw new Error("Invalid volume");
    this.gain.gain.value = Math.max(0, Math.min(1, volume));
  }
  async setRate(rate) {
    if (!Number.isFinite(rate) || rate < 0.25 || rate > 4)
      throw new Error("Playback rate must be between 0.25 and 4");
    const time = this.currentTime;
    this.rate = rate;
    await this.seek(time);
  }
  async destroy() {
    if (this.disposed) return;
    cancelAnimationFrame(this.raf);
    this.observer?.disconnect();
    await this.pause();
    try {
      if (!this.broken) await this.request("destroy");
    } finally {
      this.disposed = true;
      this.worker.terminate();
      for (const p of this.pending.values())
        p.reject(new Error("Player disposed"));
      this.pending.clear();
      await this.audio.destroy();
    }
  }
}
// Keep full ASS scripts intact. Plain SRT/WebVTT use libass too, with literal
// braces escaped so subtitle text cannot accidentally become ASS overrides.
export function toASS(text) {
  if (/^\s*\[Script Info\]/im.test(text)) return text;
  const stamp = (value) => {
    const parts = value.replace(",", ".").split(":").map(Number);
    const seconds = parts.reduce((a, b) => a * 60 + b, 0);
    if (!Number.isFinite(seconds))
      throw new Error("Invalid subtitle timestamp");
    return `${Math.floor(seconds / 3600)}:${String(Math.floor(seconds / 60) % 60).padStart(2, "0")}:${(seconds % 60).toFixed(2).padStart(5, "0")}`;
  };
  const events = [];
  for (const block of text.replace(/\r/g, "").split(/\n\s*\n/)) {
    const lines = block.split("\n"),
      i = lines.findIndex((l) => l.includes("-->"));
    if (i < 0) continue;
    const match = /^\s*([\d:.,]+)\s+-->\s+([\d:.,]+)/.exec(lines[i]);
    if (!match) continue;
    const body = lines
      .slice(i + 1)
      .join("\\N")
      .replace(/<[^>]*>/g, "")
      .replace(/[{}]/g, "");
    events.push(
      `Dialogue: 0,${stamp(match[1])},${stamp(match[2])},Default,,0,0,0,,${body}`,
    );
  }
  if (!events.length)
    throw new Error("No ASS, SRT or WebVTT subtitle cues found");
  return `[Script Info]\nScriptType: v4.00+\nPlayResX: 1280\nPlayResY: 720\nWrapStyle: 0\n[V4+ Styles]\nFormat: Name,Fontname,Fontsize,PrimaryColour,SecondaryColour,OutlineColour,BackColour,Bold,Italic,Underline,StrikeOut,ScaleX,ScaleY,Spacing,Angle,BorderStyle,Outline,Shadow,Alignment,MarginL,MarginR,MarginV,Encoding\nStyle: Default,sans-serif,36,&H00FFFFFF,&H00FFFFFF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,2,1,2,20,20,30,1\n[Events]\nFormat: Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text\n${events.join("\n")}\n`;
}
