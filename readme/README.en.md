[中文](../README.md) | [English](README.en.md) | [日本語](README.ja.md)

# liberika

> "GOOD! I'm liberika, the fifth player kernel in NipaPlay after mdk, video player, libmpv, and media kit."
> "Even counting you, there are only four player kernels!"

**The in-house playback core of NipaPlay.** Written in Rust, embeddable, handling everything from decode to render.

> Named after the detective **Furude liberika** from *Umineko When They Cry*.
> [NipaPlay](https://github.com/AimesSoft/NipaPlay-Reload) takes its name from **Furude Rika**'s catchphrase "nipah~☆" in *Higurashi When They Cry* — the community simply calls her "Rika".
> One is the player the audience sees; the other is the engine behind the curtain. Two sides of the same coin, from the same universe.

The host application provides a rendering surface and sends playback commands — decoding, timing, video rendering, subtitles, danmaku, and audio output are handled entirely inside Erika. Native video surfaces present directly; Flutter textures can participate in host composition.

Current release: **0.3.1**. Start with the [documentation index](../docs/README.en.md).

**Play MKV in the browser with its ASS subtitles and attached fonts.** The current
source includes a [Web / WASM backend](../docs/web.md), integrated into Erika Web.
WebCodecs decodes audio/video; libass in WASM renders subtitle fonts, positioning,
outlines and effects; Rust danmaku and subtitles share the playback clock and
WebGL2 compositor. Sites can keep their own UI and serve the original file with
HTTP Range, without first extracting subtitles or burning them into video on
the server.

## Features

- Hardware decoding with software fallback: VideoToolbox, D3D11VA, MediaCodec, AVCodec, and experimental Linux NVDEC / VA-API.
- Metal, D3D11 and wgpu rendering with subtitles and danmaku, HDR/EDR, Dolby Vision mapping and ArtCNN luma upscaling.
- Local files and HTTP(S), custom headers, background read-ahead and a bounded rewind cache.
- Audio-master synchronization, pause, seek, playback rate, media tracks and external subtitles.
- Rust, C ABI, Flutter, SwiftPM and native OpenHarmony ArkTS integration.
- Browser JavaScript / TypeScript integration: MKV demuxing, ASS font attachments, unified subtitle/danmaku composition and independent text supersampling.

## Integration

| Host | Install / entry point | Guide |
|---|---|---|
| Browser / JavaScript / TypeScript | Download `liberika-web-0.3.1.zip` or build from source | [Web integration and deployment](../docs/web.md) |
| Flutter | `flutter pub add erika_flutter` (0.3.1) | [Plugin guide](../packages/erika_flutter/README.md) |
| Swift / Apple | Add `https://github.com/AimesSoft/liberika-swift` in Xcode, from 0.3.1 | [liberika-swift](https://github.com/AimesSoft/liberika-swift) |
| OpenHarmony / ArkTS | `ohpm install erika` | [ArkTS SDK](../packages/erika_ohos/README.md) |
| C / C++ | Matching C ABI bundle and `erika.h` | [Native integration](../docs/integration.md) |
| Rust | Git dependency pinned to `v0.3.1` | [Build guide](../docs/building.md) |
| Linux | System FFmpeg 8 and patched libass; source build | [Linux guide](../docs/linux.md) |

As of 2026-10-31, OHPM lists 0.3.0 as the latest public version; 0.3.1 awaits review after upload.
Native archives are available from [GitHub Releases](https://github.com/AimesSoft/liberika/releases/tag/v0.3.1).

### Browser

```sh
# Install Emscripten, wasm-bindgen and the other build prerequisites first.
cargo run -p xtask -- web build
python3 scripts/serve_web.py
```

Open `http://127.0.0.1:18765` for the demo. Deploy the runtime from `target/web/`
and use `Player.create({ canvas })` and `player.open(fileOrURL)` in your own UI.
The Release includes `liberika-web-0.3.1.zip` with a demo `index.html`.
Extract the whole archive and serve it over HTTPS or localhost; source builds
are also available. The Web package has not been published to npm.
Audio/video codecs depend on browser WebCodecs support. See the [browser guide](../docs/web.md)
for prerequisites, a complete example and the Erika integration.

### Flutter

```dart
import 'package:erika_flutter/erika_flutter.dart';

final player = ErikaPlayer();
await player.open('/path/to/video.mp4');
await player.play();

// Add to the widget tree. Apple full-player UIs can use the native overlay.
ErikaVideoView(player: player)
```

Use `ErikaTextureVideoView` for Flutter clipping and color filters, or a native
video surface for Apple EDR and Windows HDR10. The plugin guide covers the
platform-specific choices and lifecycle.

### Rust

```toml
[dependencies]
erika = { git = "https://github.com/AimesSoft/liberika", tag = "v0.3.1" }
```

```rust
use erika::{MediaRequest, Player, PlayerConfig};

let player = Player::new(PlayerConfig::default());
player.open(MediaRequest::new("/path/to/video.mp4"))?;
player.play()?;
```

Prepare native dependencies before building. `Player` exposes controls and frame
subscriptions; `PresenterRuntime` owns rendering and audio. C hosts use
`ErikaHandle` or `ErikaPresenterHandle`, declared in [erika.h](../crates/erika_capi/include/erika.h).

## Platforms

| Platform | Decode | Render | Audio | Distribution |
|---|---|---|---|---|
| macOS 11+ | VideoToolbox / software | Metal | CoreAudio | Native bundles, Flutter, SwiftPM |
| iOS 13+ | VideoToolbox / software | Metal | AudioQueue | XCFramework, Flutter, SwiftPM |
| tvOS 13+ | VideoToolbox / software | Metal | AudioQueue | XCFramework, Flutter, SwiftPM |
| Windows 10+ | D3D11VA / software | D3D11 | WASAPI | x64 / ARM64 bundles, Flutter |
| Android 8+ | MediaCodec / software | wgpu Vulkan / GLES | AAudio | Four ABI bundles, Flutter |
| OpenHarmony API 18+ | AVCodec / software | wgpu | OHAudio | arm64 bundle, Flutter, OHPM |
| Linux (experimental) | NVDEC / VA-API / software | wgpu X11 / Wayland | PulseAudio / pipewire-pulse | Rust, C ABI and Flutter source builds |
| Browser | WebCodecs | WebGL2 / Canvas | Web Audio | Release Web archive or source-built JS / WASM package |

The [platform matrix](../docs/platform_matrix.zh.md) summarizes surface and HDR
paths. The browser backend currently outputs SDR and has been verified in Chrome.
It requires HTTPS or localhost, WebCodecs video/audio decoding, OffscreenCanvas,
WebGL2 and Web Audio.

## Development

```sh
# Apple / Windows / Android / OpenHarmony native dependencies
cargo run -p xtask -- deps build --all --profile lgpl
cargo build -p erika_capi
```

Cross builds require the target triple. Linux uses the system-library path.
See [building](../docs/building.md), [releasing](../docs/releasing.md) and
[contributing](../CONTRIBUTING.md).

## Repository

| Directory | Purpose |
|---|---|
| `crates/erika` | Playback, decode, clocks, subtitles, danmaku and rendering |
| `crates/erika_capi` | C ABI and public header |
| `crates/erika_ffmpeg_sys` | FFmpeg bindings |
| `packages/erika_flutter` | Flutter plugin |
| `packages/erika_ohos` | OpenHarmony ArkTS SDK |
| `packages/erika_web` | Browser Player, Worker, WebGL2 compositor and TypeScript types |
| `examples` | Native, Flutter, ArkTS and browser player examples |
| `xtask` | Native dependencies and complete Web package builds |
| `docs` | Guides and design; historical investigation and validation records in `investigations/` |

Version history: [CHANGELOG](../CHANGELOG.md).

## License

Rust workspace: [MPL-2.0](../LICENSE). Native dependency profiles and notices are
covered in the [build guide](../docs/building.md#license-profiles) and
[third-party notices](../packaging/THIRD_PARTY_NOTICES.md).
