# erika

Native ArkTS/HarmonyOS SDK powered by Erika. This package is independent of
Flutter and exposes the liberika presenter through an ArkTS-friendly API.

The first package target is OpenHarmony arm64. The package contains the native
N-API bridge and the matching `liberika_capi.so` runtime. A host application
provides an `XComponent` surface id through `attachSurface()` and drives
`renderTick()` from its frame scheduler.

## Installation

```sh
ohpm install erika
```

As of 2026-10-25, the public OHPM version is the approved 0.2.1 package.
This checkout documents the 0.3.0 SDK, which requires OHPM review after upload.

## Player configuration

`ErikaPlayerConfig` accepts `outputMode`, `edrHeadroom`, `upscaler`, and
`videoAlphaMode`. The alpha mode defaults to `0` (opaque); `1` decodes a frame
with color in the left half and alpha in the right half. The host surface must
support transparent composition for the background to remain visible.
Use the ArkTS wrapper, N-API bridge, and native runtime from the same package version.

## Usage

The package owns the native presenter, while the ArkTS host owns the
`XComponent` lifecycle. Attach the surface when it becomes available, resize
it when the host reports a new `SurfaceRect`, and dispose the player when the
surface is destroyed:

```ts
import { ErikaNativeResponse, ErikaPlayer } from 'erika';

class ErikaSurfaceController extends XComponentController {
  private readonly player: ErikaPlayer = new ErikaPlayer();
  private attached: boolean = false;
  private pendingUri: string = '';

  onSurfaceCreated(surfaceId: string): void {
    console.info(`Erika XComponent surface created: ${surfaceId}`);
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

  // Call this from the host's frame scheduler.
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

Use the controller with an ArkUI surface. The controller queues `open()` until
the first surface size callback has attached the native window, so the host can
call it from `onLoad()`. Call `renderFrame()` from the display-frame callback:

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

`renderTick()` returns an `ErikaNativeResponse` for diagnostics and frame
status. For audio-only playback, use `audioOnlyTick()` instead of rendering a
surface.

## HTTP options

For HTTP(S) playback, `open` accepts per-request headers, a read-ahead window,
and a rewind budget:

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

Headers are used for HEAD, Range GET, and prefetch requests. A positive
`httpReadAheadBytes` overrides `ERIKA_HTTP_READAHEAD_BYTES`; zero or omission
uses that environment variable when set, otherwise the 2 MiB default. A
positive `httpBackBufferBytes` overrides the 16 MiB rewind-budget default, which
is how much already-played data stays cached so a rewind inside it is served
without a network request; size it from the bitrate for high-bitrate media (a
-10 s skip at 71 Mbps covers ~89 MB). These options are ignored for local
files.

The package is licensed under MPL-2.0. See `THIRD_PARTY_NOTICES.md` for the
licenses of the bundled native dependencies.
