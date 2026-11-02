import erikaNative from 'liberika_ohos.so';

export interface ErikaNativeResponse {
  ok: boolean;
  status: number;
  error?: string;
  value?: ESObject;
}

export interface ErikaSurfaceOptions {
  surfaceId: BigInt;
  width: number;
  height: number;
  scale?: number;
}

export interface ErikaPlayerConfig {
  outputMode?: number;
  edrHeadroom?: number;
  upscaler?: number;
  /** 0 = opaque (default), 1 = packed alpha in the right half of the frame. */
  videoAlphaMode?: number;
}

export interface ErikaOpenOptions {
  httpHeaders?: ESObject;
  httpReadAheadBytes?: number;
  httpBackBufferBytes?: number;
}

/**
 * Validates an optional byte-count open option. Omitted or zero keeps the
 * native default; negative and fractional values cannot be a byte count.
 */
function byteCountOption(value: number | undefined, name: string): number {
  const count: number = value ?? 0;
  if (!Number.isSafeInteger(count) || count < 0) {
    throw new Error(name + ' must be a non-negative safe integer');
  }
  return count;
}

/**
 * Native ArkTS wrapper around ErikaPresenterHandle.
 *
 * This package does not depend on Flutter. Hosts provide an XComponent surface
 * id to attachSurface(), then drive renderTick() from their frame scheduler or
 * enable startRendering(). Native operations are serialized on one owner thread.
 */
export class ErikaPlayer {
  private readonly nativeId: number;
  private disposed: boolean = false;

  constructor(config: ErikaPlayerConfig = {}) {
    const outputMode: number = config.outputMode ?? 0;
    const edrHeadroom: number = config.edrHeadroom ?? 1.0;
    const upscaler: number = config.upscaler ?? 0;
    const videoAlphaMode: number = config.videoAlphaMode ?? 0;
    this.nativeId = erikaNative.nativeCreate(outputMode, edrHeadroom, upscaler, videoAlphaMode) as number;
    if (this.nativeId <= 0) {
      throw new Error('Erika player creation failed: ' + (erikaNative.nativeLastError() as string));
    }
  }

  get id(): number {
    return this.nativeId;
  }

  open(uri: string, options: ErikaOpenOptions = {}): ESObject | null {
    const readAhead: number = byteCountOption(options.httpReadAheadBytes, 'httpReadAheadBytes');
    const backBuffer: number = byteCountOption(options.httpBackBufferBytes, 'httpBackBufferBytes');
    return this.invokeOrThrow('open', {
      uri: uri,
      httpHeaders: options.httpHeaders,
      httpReadAheadBytes: readAhead > 0 ? readAhead : undefined,
      httpBackBufferBytes: backBuffer > 0 ? backBuffer : undefined,
    });
  }

  play(): ESObject | null {
    return this.invokeOrThrow('play');
  }

  pause(): ESObject | null {
    return this.invokeOrThrow('pause');
  }

  stop(): ESObject | null {
    return this.invokeOrThrow('stop');
  }

  close(): ESObject | null {
    return this.invokeOrThrow('close');
  }

  seek(positionMicros: number): ESObject | null {
    return this.invokeOrThrow('seek', { positionMicros: positionMicros });
  }

  setPlaybackRate(rate: number): ESObject | null {
    return this.invokeOrThrow('setPlaybackRate', { rate: rate });
  }

  setVolume(volume: number): ESObject | null {
    return this.invokeOrThrow('setVolume', { volume: volume });
  }

  setUpscaler(mode: number): ESObject | null {
    return this.invokeOrThrow('setUpscaler', { mode: mode });
  }

  /**
   * Loads a custom super-resolution network from memory: an Erika `.esr`
   * network or an mpv hook shader (FSRCNNX / Anime4K CNN dialect). FSRCNNX is
   * not bundled for licensing reasons; the host supplies the file. Select it
   * afterwards with setUpscaler(64). Throws with the native error on failure.
   */
  loadUpscalerModel(data: ArrayBuffer | Uint8Array, name: string = 'custom'): void {
    this.ensureAlive();
    const status: number = erikaNative.nativeLoadUpscalerModel(this.nativeId, name, data) as number;
    this.checkStatus(status, 'loadUpscalerModel');
  }

  /** Unloads the network loaded by loadUpscalerModel(). */
  clearUpscalerModel(): ESObject | null {
    return this.invokeOrThrow('clearUpscalerModel');
  }

  attachSurface(options: ErikaSurfaceOptions): void {
    const scale: number = options.scale ?? 1.0;
    const status: number = erikaNative.nativeAttachSurface(
      this.nativeId,
      options.surfaceId,
      options.width,
      options.height,
      scale,
    ) as number;
    this.checkStatus(status, 'attachSurface');
  }

  resizeSurface(width: number, height: number, scale: number = 1.0): void {
    const status: number = erikaNative.nativeResizeSurface(
      this.nativeId,
      width,
      height,
      scale,
    ) as number;
    this.checkStatus(status, 'resizeSurface');
  }

  detachSurface(): void {
    const status: number = erikaNative.nativeDetachSurface(this.nativeId) as number;
    this.checkStatus(status, 'detachSurface');
  }

  renderTick(timeSeconds: number): ErikaNativeResponse {
    this.ensureAlive();
    const raw: string = erikaNative.nativeRenderTick(this.nativeId, timeSeconds) as string;
    return JSON.parse(raw) as ErikaNativeResponse;
  }

  /** Native monotonic frame clock; events are delivered on ArkUI through N-API. */
  startRendering(onEvent: (event: ErikaNativeResponse) => void): void {
    this.ensureAlive();
    erikaNative.nativeStartFrames(this.nativeId, (raw: string): void => {
      if (!this.disposed) onEvent(JSON.parse(raw) as ErikaNativeResponse);
    });
  }

  /** Returns to host-driven renderTick()/pollEvent(). */
  stopRendering(): void {
    this.ensureAlive();
    erikaNative.nativeStopFrames(this.nativeId);
  }

  /** Enables native owner/slow-frame hilog diagnostics for this module. */
  setFrameTrace(enabled: boolean): void {
    this.ensureAlive();
    erikaNative.nativeSetFrameTrace(enabled);
  }

  audioOnlyTick(): void {
    this.ensureAlive();
    const status: number = erikaNative.nativeAudioOnlyTick(this.nativeId) as number;
    this.checkStatus(status, 'audioOnlyTick');
  }

  pollEvent(): ESObject | null {
    this.ensureAlive();
    const raw: ESObject = erikaNative.nativePollEvent(this.nativeId);
    if (raw === null || raw === undefined) {
      return null;
    }
    return JSON.parse(raw as string) as ESObject;
  }

  captureFrame(width: number, height: number): Uint8Array | null {
    this.ensureAlive();
    return erikaNative.nativeCaptureFrame(this.nativeId, width, height) as Uint8Array | null;
  }

  registerSubtitleMemoryFont(bytes: Uint8Array): number {
    this.ensureAlive();
    const result: ESObject = erikaNative.nativeRegisterSubtitleMemoryFont(this.nativeId, bytes);
    const status: number = Number((result as Array<ESObject>)[0]);
    this.checkStatus(status, 'registerSubtitleMemoryFont');
    return Number((result as Array<ESObject>)[1]);
  }

  dispose(): void {
    if (this.disposed) {
      return;
    }
    erikaNative.nativeDestroy(this.nativeId);
    this.disposed = true;
  }

  private invokeOrThrow(method: string, args: ESObject = {}): ESObject | null {
    const response: ErikaNativeResponse = this.invokeRaw(method, args);
    if (!response.ok) {
      throw new Error(response.error ?? ('Erika native method failed: ' + method));
    }
    return response.value ?? null;
  }

  private invokeRaw(method: string, args: ESObject): ErikaNativeResponse {
    this.ensureAlive();
    const raw: string = erikaNative.nativeInvoke(
      this.nativeId,
      method,
      JSON.stringify(args),
    ) as string;
    return JSON.parse(raw) as ErikaNativeResponse;
  }

  private checkStatus(status: number, operation: string): void {
    if (status !== 0) {
      throw new Error(operation + ' failed (' + status + '): ' +
        (erikaNative.nativeLastError() as string));
    }
  }

  private ensureAlive(): void {
    if (this.disposed) {
      throw new Error('ErikaPlayer has been disposed');
    }
  }
}
