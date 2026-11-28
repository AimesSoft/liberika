/** Prepare this before an asynchronous file picker / WASM command dispatch.
 * Safari requires resume() inside the trusted input callback, even when a
 * previous resume() is still pending. One session belongs to one Player. */
export class AudioSession extends EventTarget {
  constructor({ context, gestures = globalThis.document } = {}) {
    super();
    this.context = context ?? new AudioContext({ latencyHint: "playback" });
    this.wanted = false;
    this.unlocked = false;
    this.gestures = gestures;
    this.events = ["pointerdown", "touchend", "keydown", "click"];
    this.onGesture = (event) => {
      if (event.isTrusted && (!this.unlocked || this.wanted)) this.resume();
    };
    this.onState = () => {
      if (this.context.state === "running") {
        this.unlocked = true;
        // Priming grants audio access; it must not start paused media.
        if (!this.wanted) this.context.suspend().catch(() => {});
      }
      this.dispatchEvent(new Event("statechange"));
    };
    this.context.addEventListener("statechange", this.onState);
    for (const event of this.events)
      gestures?.addEventListener(event, this.onGesture, true);
    this.context.suspend().catch(() => {});
  }
  get playing() {
    return this.wanted && this.context.state === "running";
  }
  resume() {
    if (this.context.state === "closed") return;
    if (this.context.state === "running") { this.onState(); return; }
    // Never put this promise in the media command queue. Safari can leave it
    // pending until a later gesture, which would also block seek/pause/close.
    this.context.resume().then(this.onState, (error) => {
      if (this.context.state !== "closed")
        this.dispatchEvent(new CustomEvent(
          error.name === "NotAllowedError" ? "blocked" : "error",
          { detail: error },
        ));
    });
  }
  play() {
    this.wanted = true;
    this.resume();
  }
  async pause() {
    this.wanted = false;
    if (this.context.state === "running") await this.context.suspend();
  }
  async destroy() {
    this.wanted = false;
    for (const event of this.events)
      this.gestures?.removeEventListener(event, this.onGesture, true);
    this.context.removeEventListener("statechange", this.onState);
    await this.context.close();
  }
}
