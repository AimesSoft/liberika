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
  startPositionMicros?: number;
}

export interface ErikaSubtitleStyleOptions {
  fontFamily?: string;
  fontFilePath?: string;
  primaryColorRgba?: number;
  outlineColorRgba?: number;
  fontSize?: number;
  outlineWidth?: number;
  bold?: boolean;
  italic?: boolean;
  underline?: boolean;
  strikeOut?: boolean;
  spacing?: number;
  scaleXPercent?: number;
  scaleYPercent?: number;
  borderStyle?: number;
  shadowDepth?: number;
  blur?: number;
  alignment?: number;
  marginLeft?: number;
  marginRight?: number;
  marginVertical?: number;
  overrideMask?: number;
}

export interface ErikaDanmakuTrackOptions {
  name?: string;
  offsetMicros?: number;
}

export interface ErikaDanmakuConfig {
  enabled?: boolean;
  fontSize?: number;
  opacity?: number;
  displayArea?: number;
  scrollDurationSeconds?: number;
  scrollSpeedFactor?: number;
  trackGapRatio?: number;
  outlineWidth?: number;
  shadowOffsetX?: number;
  shadowOffsetY?: number;
  shadowStyle?: number;
  customFontFamily?: string;
  customFontFilePath?: string;
  mergeDuplicates?: boolean;
  allowStacking?: boolean;
  allowScrollOverwrite?: boolean;
  maxQuantity?: number;
  maxLinesPerMode?: number;
  blockTop?: boolean;
  blockBottom?: boolean;
  blockScroll?: boolean;
  blockWordsJson?: string;
  randomColors?: boolean;
}

export interface ErikaLogOptions {
  limit?: number;
  minLevel?: string;
  sinceSeq?: number;
}

export interface ErikaSendDanmakuOptions {
  color?: number;
  mode?: string;
  positionMicros?: number;
}

export interface ErikaGifExportOptions {
  inputUri: string;
  outputPath: string;
  startMillis: number;
  endMillis: number;
  framesPerSecond: number;
  outputWidth: number;
  outputHeight: number;
  quality?: number | string;
  loopCount?: number;
  overwrite?: boolean;
  httpHeaders?: ESObject;
  httpReadAheadBytes?: number;
}

export interface ErikaFrameExportOptions {
  uri: string;
  positionMillis: number;
  width?: number;
  height?: number;
  format?: string;
  outputPath?: string;
  overwrite?: boolean;
  httpHeaders?: ESObject;
  httpReadAheadBytes?: number;
}

export interface ErikaFramesExportOptions {
  uri: string;
  positionsMillis: Array<number>;
  outputDir: string;
  width?: number;
  height?: number;
  format?: string;
  overwrite?: boolean;
  httpHeaders?: ESObject;
  httpReadAheadBytes?: number;
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
    const startPosition: number = byteCountOption(options.startPositionMicros, 'startPositionMicros');
    return this.invokeOrThrow('open', {
      uri: uri,
      httpHeaders: options.httpHeaders,
      httpReadAheadBytes: readAhead > 0 ? readAhead : undefined,
      httpBackBufferBytes: backBuffer > 0 ? backBuffer : undefined,
      startPositionMicros: startPosition > 0 ? startPosition : undefined,
    });
  }

  setOpenStartPosition(positionMicros: number): ESObject | null {
    const position: number = byteCountOption(positionMicros, 'positionMicros');
    return this.invokeOrThrow('setOpenStartPosition', { positionMicros: position });
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

  setMuted(muted: boolean): ESObject | null {
    return this.invokeOrThrow('setMuted', { muted: muted });
  }

  getVolume(): ESObject | null {
    return this.invokeOrThrow('volume');
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

  loadUpscalerModelFile(path: string, name?: string): ESObject | null {
    return this.invokeOrThrow('loadUpscalerModelFile', {
      path: path,
      name: name,
    });
  }

  /** Unloads the network loaded by loadUpscalerModel(). */
  clearUpscalerModel(): ESObject | null {
    return this.invokeOrThrow('clearUpscalerModel');
  }

  getUpscalerStatus(): ESObject | null {
    return this.invokeOrThrow('getUpscalerStatus');
  }

  setDebugHudEnabled(enabled: boolean): ESObject | null {
    return this.invokeOrThrow('setDebugHudEnabled', { enabled: enabled });
  }

  setOutputHeadroom(headroom: number, known: boolean): ESObject | null {
    return this.invokeOrThrow('setOutputHeadroom', {
      headroom: headroom,
      known: known,
    });
  }

  getOutputStatus(): ESObject | null {
    return this.invokeOrThrow('getOutputStatus');
  }

  getPresenterStats(): ESObject | null {
    return this.invokeOrThrow('getPresenterStats');
  }

  getResourceStatus(): ESObject | null {
    return this.invokeOrThrow('getResourceStatus');
  }

  getDiagnosticsSnapshot(): ESObject | null {
    return this.invokeOrThrow('getDiagnosticsSnapshot');
  }

  getLogs(options: ErikaLogOptions = {}): ESObject | null {
    return this.invokeOrThrow('getLogs', options);
  }

  listMethods(): ESObject | null {
    return this.invokeOrThrow('listMethods');
  }

  /**
   * Calls any method in liberika's JSON dispatch table and returns the raw
   * response envelope `{ ok, status, value?, error? }` without throwing on
   * native errors.
   */
  invokeJson(method: string, args: ESObject = {}): ErikaNativeResponse {
    if (method.length === 0) {
      throw new Error('method must not be empty');
    }
    return this.invokeRaw(method, args);
  }

  tracks(): ESObject | null {
    return this.invokeOrThrow('tracks');
  }

  trackSelection(): ESObject | null {
    return this.invokeOrThrow('trackSelection');
  }

  chapters(): ESObject | null {
    return this.invokeOrThrow('chapters');
  }

  containerInfo(): ESObject | null {
    return this.invokeOrThrow('container');
  }

  selectAudioTrack(trackId: number | null): ESObject | null {
    return this.invokeOrThrow('selectAudioTrack', { trackId: trackId ?? -1 });
  }

  selectSubtitleTrack(trackId: number | null): ESObject | null {
    return this.invokeOrThrow('selectSubtitleTrack', { trackId: trackId ?? -1 });
  }

  addExternalSubtitle(uri: string): ESObject | null {
    return this.invokeOrThrow('addExternalSubtitle', { uri: uri });
  }

  removeSubtitleTrack(trackId: number): ESObject | null {
    return this.invokeOrThrow('removeSubtitleTrack', { trackId: trackId });
  }

  setSubtitleScale(scale: number): ESObject | null {
    return this.invokeOrThrow('setSubtitleScale', { scale: scale });
  }

  setSubtitleDelay(delayMicros: number): ESObject | null {
    return this.invokeOrThrow('setSubtitleDelay', { delayMicros: delayMicros });
  }

  getSubtitleDelay(): ESObject | null {
    return this.invokeOrThrow('subtitleDelay');
  }

  subtitleText(): ESObject | null {
    return this.invokeOrThrow('subtitleText');
  }

  setSubtitleFont(fontFamily?: string, fontFilePath?: string): ESObject | null {
    return this.invokeOrThrow('setSubtitleFont', {
      fontFamily: fontFamily,
      fontFilePath: fontFilePath,
    });
  }

  setSubtitleStyle(style: ErikaSubtitleStyleOptions): ESObject | null {
    return this.invokeOrThrow('setSubtitleStyle', style);
  }

  selectSubtitleMemoryFonts(fontIds: Array<number>): ESObject | null {
    return this.invokeOrThrow('selectSubtitleMemoryFonts', { fontIds: fontIds });
  }

  clearSubtitleMemoryFonts(): ESObject | null {
    return this.invokeOrThrow('clearSubtitleMemoryFonts');
  }

  getSubtitleMemoryFontStatus(): ESObject | null {
    return this.invokeOrThrow('getSubtitleMemoryFontStatus');
  }

  loadDanmakuFile(uri: string): ESObject | null {
    return this.invokeOrThrow('loadDanmakuFile', { uri: uri });
  }

  loadDanmakuJson(json: string): ESObject | null {
    return this.invokeOrThrow('loadDanmakuJson', { json: json });
  }

  addDanmakuTrackFile(uri: string, options: ErikaDanmakuTrackOptions = {}): ESObject | null {
    return this.invokeOrThrow('addDanmakuTrackFile', {
      uri: uri,
      name: options.name,
      offsetMicros: options.offsetMicros,
    });
  }

  addDanmakuTrackJson(json: string, options: ErikaDanmakuTrackOptions = {}): ESObject | null {
    return this.invokeOrThrow('addDanmakuTrackJson', {
      json: json,
      name: options.name,
      offsetMicros: options.offsetMicros,
    });
  }

  removeDanmakuTrack(trackId: number): ESObject | null {
    return this.invokeOrThrow('removeDanmakuTrack', { trackId: trackId });
  }

  setDanmakuTrackEnabled(trackId: number, enabled: boolean): ESObject | null {
    return this.invokeOrThrow('setDanmakuTrackEnabled', {
      trackId: trackId,
      enabled: enabled,
    });
  }

  setDanmakuTrackOffset(trackId: number, offsetMicros: number): ESObject | null {
    return this.invokeOrThrow('setDanmakuTrackOffset', {
      trackId: trackId,
      offsetMicros: offsetMicros,
    });
  }

  setDanmakuGlobalOffset(offsetMicros: number): ESObject | null {
    return this.invokeOrThrow('setDanmakuGlobalOffset', { offsetMicros: offsetMicros });
  }

  danmakuTracks(): ESObject | null {
    return this.invokeOrThrow('danmakuTracks');
  }

  clearDanmaku(): ESObject | null {
    return this.invokeOrThrow('clearDanmaku');
  }

  setDanmakuEnabled(enabled: boolean): ESObject | null {
    return this.invokeOrThrow('setDanmakuEnabled', { enabled: enabled });
  }

  setDanmakuConfig(config: ErikaDanmakuConfig): ESObject | null {
    return this.invokeOrThrow('setDanmakuConfig', config);
  }

  getDanmakuConfig(): ESObject | null {
    return this.invokeOrThrow('getDanmakuConfig');
  }

  sendDanmaku(text: string, options: ErikaSendDanmakuOptions = {}): ESObject | null {
    return this.invokeOrThrow('sendDanmaku', {
      text: text,
      color: options.color,
      mode: options.mode,
      positionMicros: options.positionMicros,
    });
  }

  exportGif(options: ErikaGifExportOptions): ESObject | null {
    return this.invokeOrThrow('exportGif', options);
  }

  exportFrame(options: ErikaFrameExportOptions): ESObject | null {
    return this.invokeOrThrow('exportFrame', options);
  }

  exportFrames(options: ErikaFramesExportOptions): ESObject | null {
    return this.invokeOrThrow('exportFrames', options);
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

  captureFrame(width: number, height: number, includeDanmaku: boolean = false): Uint8Array | null {
    this.ensureAlive();
    return erikaNative.nativeCaptureFrame(this.nativeId, width, height, includeDanmaku) as Uint8Array | null;
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
