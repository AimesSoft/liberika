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
export interface MediaInfo {
  duration: number;
  tracks: Track[];
  chapters: { title: string; start: number; end: number | null }[];
  videoTrack: number;
  audioTrack: number | null;
  subtitleTrack: number | null;
  decoder: string;
  backend: "webcodecs";
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
  }): Promise<Player>;
  private constructor();
  readonly currentTime: number;
  readonly playing: boolean;
  readonly info: MediaInfo | null;
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
