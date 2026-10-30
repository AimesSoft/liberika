# liberika ArkTS SDK

[English](README.md) · [中文](README.zh.md) · [日本語](README.ja.md)

OHPM 包名仍为 `erika`；导入方式和 `Erika*` API 名称保持不变。

基于 liberika 的原生 ArkTS/HarmonyOS SDK。该包独立于 Flutter，通过适合 ArkTS 的 API 提供 liberika presenter。

包的首个目标平台为 OpenHarmony arm64。包内含原生 N-API 桥接和配套的 `liberika_capi.so` 运行库。宿主通过 `attachSurface()` 提供 `XComponent` surface ID，并从帧调度器调用 `renderTick()`。

## 安装

```sh
ohpm install erika
```

截至 2026-10-30，OHPM 公开的最新版本为 0.3.0；当前文档描述 0.3.0 SDK。

## 播放器配置

`ErikaPlayerConfig` 接受 `outputMode`、`edrHeadroom`、`upscaler` 和 `videoAlphaMode`。alpha 模式默认为 `0`（不透明）；`1` 将帧的左半部分解码为颜色、右半部分解码为 alpha。要显示背景，宿主 surface 必须支持透明合成。ArkTS 封装、N-API 桥和原生运行库须使用同一包版本。

## 使用

包负责原生 presenter，ArkTS 宿主负责 `XComponent` 生命周期。surface 可用时进行绑定，宿主报告新的 `SurfaceRect` 时调整尺寸，surface 销毁时释放播放器：

```ts
import { ErikaNativeResponse, ErikaPlayer } from 'erika';

class ErikaSurfaceController extends XComponentController {
  private readonly player: ErikaPlayer = new ErikaPlayer();
  private attached: boolean = false;
  private pendingUri: string = '';

  onSurfaceCreated(surfaceId: string): void {
    console.info(`liberika XComponent surface created: ${surfaceId}`);
  }

  onSurfaceChanged(surfaceId: string, rect: SurfaceRect): void {
    if (!this.attached) {
      this.player.attachSurface({
        surfaceId: BigInt(surfaceId),
        width: rect.surfaceWidth,
        height: rect.surfaceHeight,
        scale: 1.0,
      });
      this.attached = true;
    } else {
      this.player.resizeSurface(rect.surfaceWidth, rect.surfaceHeight);
    }
    this.startPendingUri();
  }

  onSurfaceDestroyed(_surfaceId: string): void {
    if (this.attached) {
      this.player.detachSurface();
      this.attached = false;
    }
    this.player.dispose();
  }

  open(uri: string): void {
    this.pendingUri = uri;
    this.startPendingUri();
  }

  // 从宿主的帧调度器调用。
  renderFrame(timeSeconds: number): ErikaNativeResponse {
    return this.player.renderTick(timeSeconds);
  }

  private startPendingUri(): void {
    if (!this.attached || this.pendingUri.length === 0) {
      return;
    }
    const uri: string = this.pendingUri;
    this.pendingUri = '';
    this.player.open(uri);
    this.player.play();
  }
}
```

将控制器与 ArkUI surface 一起使用。控制器会将 `open()` 排队，直到首次 surface 尺寸回调绑定原生窗口，因此宿主可以从 `onLoad()` 调用它。请在显示帧回调中调用 `renderFrame()`：

```ts
@Entry
@Component
struct VideoPage {
  private readonly surfaceController: ErikaSurfaceController =
    new ErikaSurfaceController();

  build() {
    XComponent({
      id: 'erika-surface',
      type: XComponentType.SURFACE,
      controller: this.surfaceController,
    })
      .width('100%')
      .height('100%')
      .onLoad(() => {
        this.surfaceController.open('https://example.com/video.mp4');
      });
  }
}
```

`renderTick()` 返回 `ErikaNativeResponse`，用于诊断和获取帧状态。纯音频播放使用 `audioOnlyTick()`，无需渲染 surface。

## HTTP 选项

播放 HTTP(S) 时，`open` 接受请求头、预读窗口和回退缓存预算：

```ts
this.player.open('https://example.com/video.mp4', {
  httpHeaders: {
    'Authorization': 'Bearer token',
    'Referer': 'https://example.com/',
  },
  httpReadAheadBytes: 16 * 1024 * 1024,
  httpBackBufferBytes: 89 * 1024 * 1024,
});
```

请求头用于 HEAD、Range GET 和预取请求。正数 `httpReadAheadBytes` 覆盖 `ERIKA_HTTP_READAHEAD_BYTES`；为零或省略时使用该环境变量，未设置则默认为 2 MiB。正数 `httpBackBufferBytes` 覆盖 16 MiB 的默认回退缓存预算：已播放的数据会保留在缓存中，缓存范围内回退无需网络请求。高码率媒体应按码率设定（71 Mbps 回退 10 秒约需 89 MB）。本地文件忽略这些选项。

本包采用 MPL-2.0 许可。随包原生依赖的许可见 `THIRD_PARTY_NOTICES.md`。
