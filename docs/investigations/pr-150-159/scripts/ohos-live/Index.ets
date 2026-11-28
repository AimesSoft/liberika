import arkBridge from 'liberika_ohos.so';
import flutterBridge from 'liberika_flutter.so';
import fs from '@ohos.file.fs';
import common from '@ohos.app.ability.common';

interface Response {
  ok: boolean;
  status: number;
  error?: string;
  value?: ESObject;
}

interface Stats {
  decodedVideoFrames: number;
  renderedVideoFrames: number;
  audioClockReadFrames: number;
  renderFailures: number;
  importFailures: number;
  audioFailures: number;
}

interface EventValue { kind: number; state: number; }
interface Track { kind: number; }

class ReviewController extends XComponentController {
  private id: number = 0;
  private surfaceId: string = '';
  private width: number = 0;
  private height: number = 0;
  private clock: number = 0;
  private phase: number = 0;
  private timer: number = -1;
  private media: string = '';
  private cover: string = '';
  private lines: string[] = [];
  private logPath: string = '';
  private done: () => void = () => {};
  private flutter: boolean = false;
  private pausedDecoded: number = 0;
  private lastState: number = 0;
  private recreatedAt: number = 0;
  private pauseSampled: boolean = false;
  private pausedAudio: number = 0;

  constructor(flutter: boolean) { super(); this.flutter = flutter; }

  private label(): string { return this.flutter ? 'flutter' : 'arkts'; }

  private log(stage: string, value: string): void {
    const line: string = JSON.stringify({ bridge: this.label(), stage: stage, value: value });
    this.lines.push(line);
    console.info('ERIKA_REVIEW ' + line);
    if (this.logPath.length > 0) {
      const file: fs.File = fs.openSync(this.logPath, fs.OpenMode.CREATE | fs.OpenMode.READ_WRITE | fs.OpenMode.TRUNC);
      fs.writeSync(file.fd, this.lines.join('\n') + '\n');
      fs.closeSync(file);
    }
  }

  onSurfaceCreated(surfaceId: string): void { this.surfaceId = surfaceId; }
  onSurfaceChanged(surfaceId: string, rect: SurfaceRect): void {
    this.surfaceId = surfaceId;
    this.width = rect.surfaceWidth;
    this.height = rect.surfaceHeight;
    if (this.id > 0) {
      const status: number = this.flutter ? flutterBridge.nativeResizeSurface(this.id, this.width, this.height, 1) :
        arkBridge.nativeResizeSurface(this.id, this.width, this.height, 1);
      this.log('surface-resize', String(status));
    }
  }
  onSurfaceDestroyed(_surfaceId: string): void {
    if (this.timer >= 0) { clearInterval(this.timer); this.timer = -1; }
    if (this.id > 0) { this.destroy(this.id); this.id = 0; }
  }

  private create(): number {
    return this.flutter ? flutterBridge.nativeCreate(0, 1, 0, 0) : arkBridge.nativeCreate(0, 1, 0, 0);
  }
  private destroy(id: number): void {
    if (this.flutter) { flutterBridge.nativeDestroy(id); } else { arkBridge.nativeDestroy(id); }
  }
  private invoke(id: number, method: string, args: string = '{}'): string {
    return this.flutter ? flutterBridge.nativeInvoke(id, method, args) : arkBridge.nativeInvoke(id, method, args);
  }
  private call(method: string, args: string = '{}'): void {
    const raw: string = this.invoke(this.id, method, args);
    this.log(method, raw);
    const result: Response = JSON.parse(raw) as Response;
    if (!result.ok) { throw new Error(method + ': ' + result.error); }
  }
  private attach(): void {
    const status: number = this.flutter ?
      flutterBridge.nativeAttachSurface(this.id, Number(this.surfaceId), this.width, this.height, 1) :
      arkBridge.nativeAttachSurface(this.id, BigInt(this.surfaceId), this.width, this.height, 1);
    this.log('surface-attach', JSON.stringify({ id: this.id, surface: this.surfaceId, width: this.width, height: this.height, status: status }));
    if (status !== 0) { throw new Error('attach failed: ' + status); }
  }
  private detach(): void {
    const status: number = this.flutter ? flutterBridge.nativeDetachSurface(this.id) : arkBridge.nativeDetachSurface(this.id);
    this.log('surface-detach', String(status));
  }
  private stats(stage: string): Stats {
    const raw: string = this.invoke(this.id, 'getPresenterStats');
    this.log(stage, raw);
    const response: Response = JSON.parse(raw) as Response;
    const stats: Stats = response.value as Stats;
    if (!response.ok || stats.renderFailures !== 0 || stats.importFailures !== 0 || stats.audioFailures !== 0) {
      throw new Error('stats report an error');
    }
    return stats;
  }
  private open(path: string): void {
    this.call('open', JSON.stringify({ uri: path }));
    this.log('tracks', this.invoke(this.id, 'tracks'));
    this.call('play');
  }
  private recreate(): void {
    const oldId: number = this.id;
    this.detach();
    this.destroy(oldId);
    let previous: number = oldId;
    for (let i: number = 0; i < 64; i++) {
      const next: number = this.create();
      if (next <= previous) { throw new Error('non-monotonic native id'); }
      this.destroy(next);
      previous = next;
    }
    this.id = this.create();
    this.log('create', JSON.stringify({ id: this.id, surface: this.surfaceId, width: this.width, height: this.height,
      error: this.flutter ? flutterBridge.nativeLastError() : arkBridge.nativeLastError() }));
    const staleInvoke: string = this.invoke(oldId, 'stop');
    const staleResult: Response = JSON.parse(staleInvoke) as Response;
    this.destroy(oldId);
    const newResult: Response = JSON.parse(this.invoke(this.id, 'getPresenterStats')) as Response;
    this.log('stale-id', JSON.stringify({ oldId: oldId, newId: this.id, staleInvoke: staleInvoke, newAlive: newResult.ok }));
    if (staleResult.ok || !newResult.ok) { throw new Error('stale ID affected recreated player'); }
    this.attach();
    this.open(this.media);
    this.recreatedAt = Date.now();
  }

  start(media: string, cover: string, logPath: string, done: () => void): void {
    this.media = media;
    this.cover = cover;
    this.logPath = logPath;
    this.done = done;
    this.log('start', this.label());
    this.id = this.create();
    this.log('initial-create', JSON.stringify({ id: this.id, surface: this.surfaceId, width: this.width, height: this.height,
      error: this.flutter ? flutterBridge.nativeLastError() : arkBridge.nativeLastError() }));
    if (this.id <= 0 || this.surfaceId.length === 0 || this.width <= 0) { throw new Error('player or surface unavailable'); }
    this.attach();
    this.open(media);
    this.clock = Date.now();
    this.timer = setInterval(() => {
      try { this.tick(); } catch (error) {
        this.log('FAILED', String(error));
        clearInterval(this.timer);
        this.timer = -1;
        this.done();
      }
    }, 33);
  }

  private tick(): void {
    const elapsed: number = (Date.now() - this.clock) / 1000;
    const raw: string = this.flutter ? flutterBridge.nativeRenderTick(this.id, elapsed) : arkBridge.nativeRenderTick(this.id, elapsed);
    const response: Response = JSON.parse(raw) as Response;
    if (!response.ok) { throw new Error('render: ' + response.error); }
    for (let i: number = 0; i < 32; i++) {
      const event: string | null = this.flutter ? flutterBridge.nativePollEvent(this.id) : arkBridge.nativePollEvent(this.id);
      if (event === null) { break; }
      this.log('event', event);
      const result: Response = JSON.parse(event) as Response;
      const value: EventValue = result.value as EventValue;
      if (value.kind === 1) { this.lastState = value.state; }
    }
    if (this.phase === 0 && elapsed >= 4) {
      const stats: Stats = this.stats('playing');
      if (stats.renderedVideoFrames === 0 || stats.audioClockReadFrames === 0) { throw new Error('no video or audio playback'); }
      this.call('pause'); this.phase++;
    }
    if (this.phase === 1 && elapsed >= 5 && !this.pauseSampled) {
      const stats: Stats = this.stats('pause-settled');
      if (this.lastState !== 4) { throw new Error('pause did not settle'); }
      this.pausedDecoded = stats.decodedVideoFrames;
      this.pausedAudio = stats.audioClockReadFrames;
      this.pauseSampled = true;
    }
    if (this.phase === 1 && elapsed >= 6) {
      const stats: Stats = this.stats('paused');
      if (stats.decodedVideoFrames !== this.pausedDecoded || stats.audioClockReadFrames !== this.pausedAudio) {
        throw new Error('settled pause continued decoding or consuming audio');
      }
      this.call('seek', '{"positionMicros":10000000}'); this.phase++;
    }
    if (this.phase === 2 && elapsed >= 7) {
      const stats: Stats = this.stats('paused-eof');
      this.log('paused-eof-state', String(this.lastState));
      if (stats.decodedVideoFrames !== this.pausedDecoded || this.lastState !== 4) { throw new Error('paused EOF state or decoding changed'); }
      this.call('seek', '{"positionMicros":3500000}'); this.phase++;
    }
    if (this.phase === 3 && elapsed >= 8) { this.call('play'); this.call('setPlaybackRate', '{"rate":1.25}'); this.phase++; }
    if (this.phase === 4 && elapsed >= 11) { this.stats('resumed'); this.detach(); this.phase++; }
    if (this.phase === 5 && elapsed >= 12) { this.attach(); this.phase++; }
    if (this.phase === 6 && elapsed >= 16) {
      this.stats('surface-reconnected');
      this.detach(); this.destroy(this.id); this.id = this.create(); this.attach();
      this.open(this.cover);
      const trackResponse: Response = JSON.parse(this.invoke(this.id, 'tracks')) as Response;
      const tracks: Track[] = trackResponse.value as Track[];
      if (tracks.length !== 1 || tracks[0].kind !== 1) { throw new Error('cover exposed as video'); }
      this.phase++;
    }
    if (this.phase === 7 && elapsed >= 22) {
      const stats: Stats = this.stats('cover-audio');
      if (stats.audioClockReadFrames === 0 || stats.decodedVideoFrames !== 0) { throw new Error('cover MP3 did not play as audio only'); }
      this.recreate(); this.phase++;
    }
    if (this.phase === 8 && Date.now() - this.recreatedAt >= 5000) {
      const stats: Stats = this.stats('recreated-playback');
      if (stats.renderedVideoFrames === 0 || stats.audioClockReadFrames === 0) { throw new Error('recreated player did not play'); }
      this.call('pause');
      clearInterval(this.timer);
      this.timer = -1;
      this.log('COMPLETE', this.label());
      this.done();
    }
  }
}

@Entry
@Component
struct Index {
  @State status: string = 'Ready';
  private arkController: ReviewController = new ReviewController(false);
  private flutterController: ReviewController = new ReviewController(true);
  private media: string = '';
  private cover: string = '';
  private directory: string = '';

  aboutToAppear(): void {
    const context: common.UIAbilityContext = getContext(this) as common.UIAbilityContext;
    this.directory = context.filesDir;
    this.media = this.copyRaw(context, 'bbb-av-10s.mp4');
    this.cover = this.copyRaw(context, 'cover-5s.mp3');
    setTimeout(() => {
      this.status = 'ArkTS running';
      this.arkController.start(this.media, this.cover, this.directory + '/arkts-review.jsonl', () => {
        this.status = 'Flutter bridge running';
        this.flutterController.start(this.media, this.cover, this.directory + '/flutter-review.jsonl', () => {
          this.status = 'Finished — inspect logs';
        });
      });
    }, 1500);
  }

  private copyRaw(context: common.UIAbilityContext, name: string): string {
    const path: string = context.filesDir + '/' + name;
    const bytes: Uint8Array = context.resourceManager.getRawFileContentSync(name);
    const file: fs.File = fs.openSync(path, fs.OpenMode.CREATE | fs.OpenMode.READ_WRITE | fs.OpenMode.TRUNC);
    fs.writeSync(file.fd, bytes.buffer);
    fs.closeSync(file);
    return path;
  }

  build() {
    Column() {
      Text('Erika PR review — ' + this.status).fontSize(16).height(48).fontColor(Color.White)
      Text('ArkTS production bridge').fontColor(Color.White)
      XComponent({ id: 'review-arkts', type: XComponentType.SURFACE, controller: this.arkController })
        .width('100%').height('38%')
      Text('Flutter production bridge').fontColor(Color.White)
      XComponent({ id: 'review-flutter', type: XComponentType.SURFACE, controller: this.flutterController })
        .width('100%').height('38%')
    }.width('100%').height('100%').backgroundColor(Color.Black)
  }
}
