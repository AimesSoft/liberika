# liberika ArkTS SDK

[English](README.md) · [中文](README.zh.md) · [日本語](README.ja.md)

OHPM パッケージ名は `erika` のままで、import と `Erika*` API の名前も変わりません。

liberika を利用するネイティブ ArkTS/HarmonyOS SDK です。Flutter とは独立し、ArkTS 向け API で liberika presenter を提供します。

最初の対象は OpenHarmony arm64 です。パッケージにはネイティブ N-API ブリッジと対応する `liberika_capi.so` ランタイムが含まれます。ホストは `attachSurface()` で `XComponent` の surface ID を渡し、フレームスケジューラから `renderTick()` を呼び出します。

## インストール

```sh
ohpm install erika
```

2026-10-31 時点で OHPM の公開最新バージョンは 0.3.0 です（0.3.2 はアップロード後の審査待ち）。この文書は 0.3.2 SDK を説明します。

## プレイヤー設定

`ErikaPlayerConfig` は `outputMode`、`edrHeadroom`、`upscaler`、`videoAlphaMode` を受け取ります。alpha モードの既定値は `0`（不透明）で、`1` はフレームの左半分を色、右半分を alpha としてデコードします。背景を表示するには、ホスト surface が透明合成に対応する必要があります。ArkTS ラッパー、N-API ブリッジ、ネイティブランタイムには同じパッケージバージョンを使ってください。

## 使用方法

パッケージがネイティブ presenter を、ArkTS ホストが `XComponent` のライフサイクルを管理します。surface が利用可能になったら接続し、新しい `SurfaceRect` が通知されたらサイズを変更し、surface の破棄時にプレイヤーを解放します。

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

  // ホストのフレームスケジューラから呼び出します。
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

このコントローラを ArkUI surface と組み合わせます。最初の surface サイズコールバックでネイティブウィンドウが接続されるまで `open()` は保留されるため、ホストは `onLoad()` から呼び出せます。表示フレームのコールバックから `renderFrame()` を呼び出してください。

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

`renderTick()` は診断とフレーム状態を表す `ErikaNativeResponse` を返します。音声のみの再生では surface を描画せず `audioOnlyTick()` を使います。

## HTTP オプション

HTTP(S) 再生時、`open` にリクエストヘッダー、先読み範囲、巻き戻し用キャッシュ容量を指定できます。

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

ヘッダーは HEAD、Range GET、先読みリクエストに使われます。正の `httpReadAheadBytes` は `ERIKA_HTTP_READAHEAD_BYTES` を上書きし、ゼロまたは省略時は環境変数、未設定なら既定の 2 MiB を使います。正の `httpBackBufferBytes` は巻き戻し用キャッシュの既定値 16 MiB を上書きします。再生済みデータがキャッシュ内にあればネットワークを使わず巻き戻せます。高ビットレートのメディアではビットレートに応じて設定してください（71 Mbps で 10 秒戻すには約 89 MB）。ローカルファイルではこれらのオプションは無視されます。

このパッケージは MPL-2.0 で提供されます。同梱のネイティブ依存関係のライセンスは `THIRD_PARTY_NOTICES.md` を参照してください。
