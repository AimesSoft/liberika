import test from "node:test";
import assert from "node:assert/strict";
import { AudioSession, Player } from "../index.js";

// Safari can keep resume() pending until it is called again in trusted input.
class Context extends EventTarget {
  state = "suspended";
  currentTime = 0;
  allowed = false;
  resumes = [];
  resume() {
    const promise = new Promise(resolve => this.resumes.push(resolve));
    if (this.allowed) {
      this.state = "running";
      this.dispatchEvent(new Event("statechange"));
      for (const resolve of this.resumes.splice(0)) resolve();
    }
    return promise;
  }
  async suspend() {
    if (this.state === "suspended") return;
    this.state = "suspended";
    this.dispatchEvent(new Event("statechange"));
  }
  async close() {
    this.state = "closed";
    for (const resolve of this.resumes.splice(0)) resolve();
  }
  createGain() { return { gain: {}, connect() {} }; }
}
class Gestures {
  handlers = new Map();
  addEventListener(type, fn) { this.handlers.set(type, fn); }
  removeEventListener(type) { this.handlers.delete(type); }
  input(type = "pointerdown", isTrusted = true) { this.handlers.get(type)?.({ isTrusted }); }
}
const flush = () => new Promise(resolve => setImmediate(resolve));

test("audio is primed synchronously by trusted input and stays paused", async () => {
  const context = new Context(), gestures = new Gestures();
  const audio = new AudioSession({ context, gestures });
  gestures.input("click", false);
  assert.equal(context.resumes.length, 0);
  context.allowed = true;
  gestures.input();
  assert.equal(audio.unlocked, true, "resume must run before the input handler returns");
  await flush();
  assert.equal(context.state, "suspended");
  assert.equal(audio.playing, false);
  audio.play();
  assert.equal(audio.playing, true);
  await audio.pause();
  gestures.input();
  assert.equal(context.state, "suspended", "unrelated input must not unpause media");
  await audio.destroy();
  assert.equal(gestures.handlers.size, 0);
});

test("pending browser activation does not block seek, pause or close", async t => {
  const previousWorker = globalThis.Worker, previousCancel = globalThis.cancelAnimationFrame;
  globalThis.Worker = class {
    postMessage(data) {
      queueMicrotask(() => this.onmessage({ data: { type: "reply", requestId: data.requestId } }));
    }
    terminate() {}
  };
  globalThis.cancelAnimationFrame = () => {};
  t.after(() => { globalThis.Worker = previousWorker; globalThis.cancelAnimationFrame = previousCancel; });
  const context = new Context(), gestures = new Gestures();
  const audio = new AudioSession({ context, gestures });
  const player = new Player({}, 1, audio);
  player.info = { duration: 1000 };
  const commands = player.play().then(() => player.seek(600));
  assert.equal(await Promise.race([commands.then(() => "ready"), new Promise(resolve => setTimeout(() => resolve("blocked"), 100))]), "ready");
  assert.equal(player.currentTime, 600);
  assert.equal(player.playing, false);
  assert.equal(audio.wanted, true);
  context.allowed = true;
  gestures.input();
  await flush();
  assert.equal(player.playing, true);
  await player.pause();
  assert.equal(player.playing, false);
  context.allowed = false;
  await player.play();
  await player.close();
  context.allowed = true;
  gestures.input();
  await flush();
  assert.equal(audio.wanted, false);
  assert.equal(player.playing, false, "a late resume must not restart a closed source");
  await player.destroy();
  assert.equal(context.state, "closed");
  assert.equal(gestures.handlers.size, 0);
});

test("web Player aligns mute/volume, subtitle delay and clearDanmaku", async t => {
  const previousWorker = globalThis.Worker, previousCancel = globalThis.cancelAnimationFrame;
  const posted = [];
  globalThis.Worker = class {
    postMessage(data) {
      posted.push(data);
      queueMicrotask(() => this.onmessage({ data: { type: "reply", requestId: data.requestId } }));
    }
    terminate() {}
  };
  globalThis.cancelAnimationFrame = () => {};
  t.after(() => { globalThis.Worker = previousWorker; globalThis.cancelAnimationFrame = previousCancel; });
  const context = new Context(), gestures = new Gestures();
  const audio = new AudioSession({ context, gestures });
  const player = new Player({}, 0.6, audio);

  assert.deepEqual(player.getVolume(), { volume: 0.6, muted: false });
  assert.equal(player.gain.gain.value, 0.6);
  player.setMuted(true);
  assert.deepEqual(player.getVolume(), { volume: 0.6, muted: true });
  assert.equal(player.gain.gain.value, 0);
  player.setVolume(0.35);
  assert.deepEqual(player.getVolume(), { volume: 0.35, muted: true });
  assert.equal(player.gain.gain.value, 0);
  player.setMuted(false);
  assert.deepEqual(player.getVolume(), { volume: 0.35, muted: false });
  assert.equal(player.gain.gain.value, 0.35);

  await player.setSubtitleDelay(-1.25);
  assert.equal(player.getSubtitleDelay(), -1.25);
  assert.equal(posted.at(-1).type, "subtitleOptions");
  assert.equal(posted.at(-1).delay, -1.25);
  await player.setSubtitleDelay(9999);
  assert.equal(player.getSubtitleDelay(), 600);
  assert.throws(() => player.setSubtitleDelay(NaN));
  await player.configureSubtitles({ scale: 1.5, delay: 0.5 });
  assert.equal(player.getSubtitleDelay(), 0.5);

  await player.clearDanmaku();
  assert.equal(posted.at(-1).type, "clearDanmaku");

  await player.destroy();
});
