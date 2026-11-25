export interface DanmakuOptions {
  enabled?: boolean;
  fontSize?: number;
  opacity?: number;
  area?: number;
  duration?: number;
  delay?: number;
  speed?: number;
  supersampling?: boolean;
  blockTop?: boolean;
  blockBottom?: boolean;
  blockScroll?: boolean;
  mergeDuplicates?: boolean;
  /** Paint each comment in a stable color from the colorful sending palette. */
  randomColors?: boolean;
}
export interface Track {
  id: number;
  type: "video" | "audio" | "subtitle" | "attachment" | string;
  codec: string;
  name: string;
  language: string;
  width: number;
  height: number;
  sampleRate: number;
  channels: number;
  profile: number;
  level: number;
  bitDepth: number;
  extra: Uint8Array;
}
export type RendererPreference = "auto" | "webgpu" | "webgl2";
export type RendererBackend = "webgpu" | "webgl2";
export type UpscalerMode =
  | "off"
  | "fsr1"
  | "artcnn_c4f16"
  | "artcnn_c4f16_ds"
  | "artcnn_c4f16_dn"
  | "artcnn_c4f32"
  | "artcnn_c4f32_ds"
  | "artcnn_c4f32_dn"
  | "anime4k_s"
  | "anime4k_m"
  | "anime4k_l"
  | "anime4k_vl"
  | "anime4k_ul"
  | "anime4k_denoise_s"
  | "anime4k_denoise_m"
  | "anime4k_denoise_l"
  | "anime4k_denoise_vl"
  | "anime4k_denoise_ul"
  | "custom"
  | number;
export interface UpscalerStatus {
  requestedMode: string;
  activeBackend: "off" | "building" | "compute" | "inactive" | "unsupported";
  supported: boolean;
  halfFloat: boolean;
  fallbackCount: number;
  upscaledFrames: number;
  lastError: string | null;
  availableModes: string[];
}
export interface CustomUpscalerModelInfo {
  name: string;
  layers: number;
  macsPerPixel: number;
  parameters: number;
}
export interface MediaInfo {
  duration: number;
  tracks: Track[];
  chapters: { title: string; start: number; end: number | null }[];
  videoTrack: number;
  audioTrack: number | null;
  subtitleTrack: number | null;
  decoder: string;
  backend: "webcodecs";
  renderer: RendererBackend;
  /** A browser hint, not proof that hardware decoding was selected. */
  hardwarePreference: string;
}
export interface PlayerStats {
  time?: number;
  decodedVideo?: number;
  decodedAudio?: number;
  droppedVideo?: number;
  videoBufferedUntil?: number;
  videoQueue?: number;
  maxVideoQueue?: number;
  rendered?: number;
  glyphs?: number;
  atlasUploads?: number;
  subtitleImages?: number;
  hasVideo?: boolean;
  eof?: boolean;
  renderer?: RendererBackend;
  upscaler?: {
    mode: string;
    backend: UpscalerStatus["activeBackend"];
    halfFloat: boolean;
    upscaledFrames: number;
    fallbackCount: number;
  };
}
/** Prepare before opening a file picker or deferring playback through WASM.
 * Player takes ownership of this session and closes it on destroy(). */
export class AudioSession extends EventTarget {
  constructor();
  readonly context: AudioContext;
  readonly wanted: boolean;
  readonly playing: boolean;
  destroy(): Promise<void>;
}
/** Events: loaded(MediaInfo), stats(PlayerStats), timeupdate(number), error(Error),
 * log(string) are CustomEvents; play, pause, playblocked and ended are Events. */
export class Player extends EventTarget {
  static create(options: {
    canvas: HTMLCanvasElement;
    volume?: number;
    audioSession?: AudioSession;
    /** Stands in for text no font covers, in a package built without its own. */
    fallbackFont?: { data: ArrayBuffer | Uint8Array; index?: number };
    /** Presentation backend: "auto" prefers WebGPU and falls back to WebGL2. */
    renderer?: RendererPreference;
  }): Promise<Player>;
  private constructor();
  readonly currentTime: number;
  readonly playing: boolean;
  readonly info: MediaInfo | null;
  readonly renderer: RendererBackend;
  readonly stats: PlayerStats;
  open(source: File | Blob | string, options?: { hardwareDecode?: boolean }): Promise<MediaInfo>;
  /** Requests playback without blocking commands on browser audio permission.
   * The play event confirms the clock started; trusted input retries activation. */
  play(): Promise<void>;
  pause(): Promise<void>;
  seek(seconds: number): Promise<void>;
  selectAudio(id: number | null): Promise<void>;
  selectSubtitle(id: number | null): Promise<void>;
  addSubtitle(assSrtOrVtt: string): Promise<void>;
  addDanmaku(xmlOrJson: string): Promise<number>;
  /** Shows the viewer's own comment, bordered; `color` is 0xRRGGBB (white by default). */
  sendDanmaku(text: string, color?: number): Promise<void>;
  /** Selects a WebGPU compute super-resolution mode (ArtCNN, Anime4K, FSR 1.0, or custom). */
  setUpscaler(mode: UpscalerMode): Promise<UpscalerStatus>;
  /** Loads a custom .esr network or mpv hook shader (FSRCNNX / Anime4K GLSL) for "custom" mode. */
  loadUpscalerModel(
    name: string,
    data: ArrayBuffer | Uint8Array | string,
  ): Promise<CustomUpscalerModelInfo>;
  clearUpscalerModel(): Promise<void>;
  getUpscalerStatus(): Promise<UpscalerStatus>;
  capture(width?: number): Promise<Blob>;
  close(): Promise<void>;
  addFont(name: string, data: ArrayBuffer | Uint8Array): Promise<void>;
  configureDanmaku(options: DanmakuOptions): Promise<void>;
  configureSubtitles(options: {
    scale?: number;
    supersampling?: boolean;
  }): Promise<void>;
  setVolume(volume: number): void;
  /** Changes playback speed; the current Web Audio path also changes pitch. */
  setRate(rate: number): Promise<void>;
  destroy(): Promise<void>;
}
export function toASS(text: string): string;
