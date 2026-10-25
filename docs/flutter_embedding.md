# Flutter Embedding

[中文](flutter_embedding.zh.md) | [English](flutter_embedding.md) | [日本語](flutter_embedding.ja.md)

liberika is not a Flutter video renderer. Flutter is an optional host UI.
The player owns decode, timing, native rendering, subtitles, danmaku, audio, and
HDR presentation.

## API Families

There are two C ABI entrypoint families:

- `ErikaHandle`: control and event API. Use this when the host owns its own
  presenter loop or only wants to probe/control playback.
- `ErikaPresenterHandle`: presenter-owned API. Use this when liberika should
  own `Player + renderer + audio output` and the host only supplies a
  native surface plus a display-tick callback.

Both families are declared in `crates/erika_capi/include/erika.h`.

## Apple Surface Strategies

The Apple HDR path uses a native Metal-backed surface, not Flutter Texture.
The Flutter plugin intentionally exposes two native surface strategies on
macOS, iOS, and tvOS so hosts can pick the composition model that matches their
UI.

### ErikaVideoView (Platform View)

Standard Flutter platform view backed by `NSView`/`CAMetalLayer` on macOS and
`UIView`/`CAMetalLayer` on iOS/tvOS. The plugin creates a native video view
registered as `erika_flutter/video_view`, attaches it to the presenter, and
drives rendering from a display link.

This path is useful for simple embedders and diagnostics. On macOS it is not the
recommended production path because AppKit/Flutter platform view composition can
show black flicker or other compositor artifacts.

### ErikaWindowOverlayVideoView (Window Overlay)

For the preferred HDR/EDR path, the plugin creates a window-hosted native
overlay that sits outside Flutter's platform-view compositor:

1. Dart `ErikaWindowOverlayVideoView` reserves a rectangle in the widget tree.
2. The platform plugin creates a window-level native view with a `CAMetalLayer`
   as a sibling/underlay of the Flutter host view.
3. Flutter paints the widget region transparent, leaving a hole for native video.
4. The widget tracks its position and sends geometry updates with a surface
   generation number, so stale hide calls from disposed widgets cannot affect
   newly attached surfaces.
5. Attach retry with exponential backoff handles window readiness timing.

The overlay path is the recommended path for NipaPlay and other full-player
UIs. It keeps video presentation owned by liberika/Metal while Flutter remains a
control and layout layer. On iOS/tvOS the native side uses a window plus a
sibling `UIView`/`CAMetalLayer`; on macOS it uses the host `NSWindow` plus a
sibling `NSView`/`CAMetalLayer`.

Touch events pass through both native video strategies, so Flutter controls can
remain above or around the video surface.

### ErikaTextureVideoView (Flutter Texture)

`ErikaTextureVideoView` renders through Flutter's texture registrar instead of a
platform view. On macOS the plugin allocates an IOSurface-backed
`CVPixelBuffer` pool, renders into its Metal texture directly (no per-frame CPU
readback), and publishes frames to Flutter, so regular Flutter effects —
`Opacity`, clipping, transforms, and color filters — apply to the video. On
OpenHarmony it reuses the registered external texture. On Windows it offers
explicit SDR BGRA8 output (see below). On other platforms it
falls back to `ErikaVideoView`.

When `blendMode` is not `srcOver` on macOS, the widget routes back to the native
platform view so Core Animation can blend the video against the real backdrop
(see Transparent Video and Blend Modes below).

Windows `ErikaVideoView` keeps the native HWND/swapchain path and HDR10
negotiation for ordinary opaque playback. Only explicitly selecting
`ErikaTextureVideoView` opts into SDR texture composition. `srcOver` supports
Flutter opacity/clipping/filters; `overlay` uses native Windows Composition;
other Windows blend modes are rejected.

Each changed texture frame is copied on the GPU into a completed, immutable
shared snapshot, without CPU pixel readback. Paused output is reused until the
video, subtitles, danmaku, HUD, seek generation or surface size changes. The
plugin holds only the latest and raster-acquired snapshots. Opening a shared
handle does not signal GPU sampling completion, so published textures are never
overwritten. These extra copies and GPU handoff barriers do not apply to native
opaque HWND playback, and the decoder pool retains main's existing size.

The texture API requires a matching v0.1.8 or newer native runtime. The Flutter
package pins its native version and checksums in `native_artifacts.properties`.
Missing optional texture symbols in older runtimes do not prevent native player
creation.

## Android Surface Strategies

On Android, both video widgets use the same native-view selector. SDR uses a
real `TextureView` and has been verified. wgpu selects Vulkan with a bounded
GLES fallback. Requesting `ErikaOutputMode.extendedLinear` instead creates a
`SurfaceView` through `PlatformViewLink` and Hybrid Composition so FP16 scRGB
does not pass through Flutter's texture-layer compositor. `Choreographer`
drives the surface, while lifecycle, resize, audio focus, and output fallback
remain owned by the plugin.

The FP16 extended-linear scRGB implementation is complete, including
`Rgba16Float` negotiation and `ADATASPACE_SCRGB_LINEAR` verification. Its active
path is not yet claimed as device-validated: final acceptance still requires an
API 35 HDR device. Unsupported displays, GLES, `TextureView`, missing FP16, or
dataspace verification failures continue in SDR with a queryable fallback
reason and explicit logs.

## HarmonyOS Surface Strategies

On HarmonyOS, use `ErikaVideoView`. The ArkTS plugin registers a Flutter
external texture, takes that texture's surface as an `OHNativeWindow`, and
attaches it to the presenter; wgpu then renders through Vulkan, using
`VK_OHOS_surface` for window-system integration.

Video decoding defaults to HarmonyOS AVCodec (H.264 and HEVC). AVCodec decodes
straight into a Surface, whose `OHNativeBuffer` is imported as a Vulkan
external image and resolved by a Vulkan YCbCr sampler, so decoded frames reach
the compositor with no CPU copy. Subtitles, danmaku, and overlays composite in
the same wgpu pass as every other platform.

Devices missing the required Vulkan extensions fall back to FFmpeg software
decode with CPU upload. The fallback is reported through `VideoDecoderChanged`
events and presenter diagnostics instead of failing playback. The HarmonyOS
path is validated on device; CI builds the OpenHarmony C ABI but has no
device-side run verification.

## Transparent Video and Blend Modes

liberika can present alpha-bearing video assets while keeping Flutter layout and
composition. Transparency is requested per player:

```dart
final player = ErikaPlayer(
  videoAlphaMode: ErikaVideoAlphaMode.packedAlphaRight,
);
```

`ErikaVideoAlphaMode.packedAlphaRight` expects a side-by-side encoded frame:
the left half is colour and the right half is a grayscale alpha mask. The GPU
presentation shaders (Metal, wgpu, D3D11) reconstruct a premultiplied-alpha
frame, and the video is presented at half the encoded width. Players that do
not request the mode interpret the frame as fully opaque, so existing content
is unaffected.

Blending and opacity are requested on the video widgets:

```dart
ErikaTextureVideoView(
  player: player,
  blendMode: BlendMode.overlay,
  opacity: 0.8,
)
```

Platform support differs:

| Platform and path | `blendMode` | `opacity` |
|-------------------|-------------|-----------|
| macOS, `ErikaTextureVideoView` (Flutter texture) | `srcOver` only; other modes route to the native layer | Flutter `Opacity` |
| macOS, native layer (`blendMode != srcOver`) | `overlay` supported; other values ignored | Native `CALayer` opacity |
| Windows window overlay (DirectComposition) | `srcOver`, `overlay`; other values raise a plugin error | `IDCompositionEffectGroup` opacity |
| Android / iOS / OpenHarmony | Accepted in creation parameters; not consumed | Flutter-side on texture paths |

Overlay blending is backdrop-aware: the macOS native layer composites against
the content behind the video inside the window, and the Windows blend effect
samples the same HWND, so the underlying Flutter or game content stays visible.
Transparent Windows overlay video uses an SDR BGRA8 premultiplied composition
swap chain bound to the Flutter HWND; HDR sources are tone-mapped into that
composition space inside Erika. Opaque Windows overlay video keeps the
dedicated child-HWND path used previously, and the composition content is
rebound automatically if the decoder or output device rebuilds the swap chain.

## iOS Build Path

The iOS plugin links the liberika C ABI static library into the app through a
CocoaPod script phase. By default it downloads the matching prebuilt archive;
set `ERIKA_FORCE_SOURCE_BUILD=1` (with `ERIKA_REPO_ROOT`) to build the Rust
`erika_capi` crate for the target iOS architecture instead.

## tvOS Build Path

The tvOS plugin links the liberika C ABI static library through its CocoaPod script
phase. Like iOS it downloads the prebuilt archive by default, with
`ERIKA_FORCE_SOURCE_BUILD=1` falling back to building from source. It supports
tvOS 13+, arm64 devices, and arm64/x86_64 simulators. See
[`packages/erika_flutter/README.md`](../packages/erika_flutter/README.md) for
nightly, prebuilt-bundle, and source-build options.

## macOS Build Path

The macOS pod downloads the package-pinned, checksum-verified dylib and bundles it into the app. This is also the default inside a repository checkout. Set `ERIKA_FORCE_SOURCE_BUILD=1` and, if needed, `ERIKA_REPO_ROOT` to build local changes. Architecture and library overrides are documented in the [Flutter README](../packages/erika_flutter/README.md).

## Minimal Presenter Flow

```c
ErikaPresenterHandle *presenter = erika_presenter_create();
erika_presenter_attach_metal_layer(
    presenter,
    (uint64_t)cametal_layer,
    width,
    height,
    backing_scale);
erika_presenter_open(presenter, "/path/to/media.mp4");
erika_presenter_play(presenter);

// On every display tick:
ErikaPresenterStats stats;
erika_presenter_render_tick(presenter, host_time_seconds, &stats);

// On resize:
erika_presenter_resize_surface(presenter, width, height, backing_scale);

// On dispose:
erika_presenter_detach_surface(presenter);
erika_presenter_destroy(presenter);
```

## Flutter Texture Path

Flutter Texture is a lower-capability compatibility path.

Useful for:
- SDR fallback.
- Platforms where native view composition is not ready.
- Transparent video that should participate in Flutter's compositor
  (`ErikaTextureVideoView` on macOS, Windows, and OpenHarmony).
- Test surfaces or constrained embedding environments.

It is not the preferred HDR/EDR route because video enters Flutter's
compositor. On Apple the surface is an IOSurface-backed `CVPixelBuffer`: the
host registers a Flutter texture, selects the frame's Metal texture with
`erika_presenter_attach_flutter_texture` and
`erika_presenter_set_flutter_texture_buffer` before every render tick, and
Flutter composites the same buffer without a CPU readback. The host keeps
ownership of the pixel buffers.

## wgpu and Android

The Apple HDR path remains native Metal, and Windows uses a native Direct3D 11
renderer (D3D11VA zero-copy decode, HDR10 output). On Android, wgpu is the active
renderer: Vulkan imports MediaCodec Surface frames through AHardwareBuffer, and
software frames have an explicit CPU-upload fallback. Video, subtitles,
danmaku, capture, and ArtCNN compute share this path. Vulkan can negotiate FP16
extended-linear scRGB; GLES and failed capability negotiation explicitly fall
back to SDR. Android SDR is verified, while the API 35 HDR-device active-path
acceptance remains pending. Linux provides experimental X11/Wayland and Flutter integration.

## Dart API

Installation, player controls, HTTP cache options, events and view examples are
maintained in the [Flutter SDK guide](../packages/erika_flutter/README.md).

## Media Track Information

`tracks()` returns an `ErikaTrackInfo` for every embedded or external track. A video track
provides `codec`, `width`, `height`, `pixelFormat`, `profile`, `level`, `bitRate`,
`frameRateNumerator`, and `frameRateDenominator`; audio tracks additionally provide
`sampleRate`, `channels`, and `sampleFormat`.

- `bitRate` is in bit/s. liberika prefers the video track's own codec parameters; only when there is
  exactly one video track with no declared bitrate and the container bitrate plus every other
  media track's bitrate (audio, subtitle, other video) are known does it estimate video bitrate as
  container bitrate minus those. It is `null` when
  unavailable, is not an instantaneous runtime bitrate, and an estimate can include container
  overhead or non-audio streams.
- `frameRateNumerator` / `frameRateDenominator` retain the rational value, preventing values
  such as `30000/1001` from being truncated. The probe order is average frame rate,
  `r_frame_rate`, then FFmpeg's guessed frame rate. `framesPerSecond` is a Dart convenience
  getter; for variable-frame-rate media it remains an average, declared, or guessed value.
- `TracksChanged` and `TrackSelectionChanged` events include the complete `trackList`. Hosts may
  also call `tracks()` again after either event to obtain a current snapshot.

```dart
player.events.listen((event) {
  if (event.kind == ErikaEventKind.tracksChanged) {
    for (final track in event.trackList) {
      if (track.kind == ErikaTrackKind.video && track.selected) {
        print(track.toMap());
        break;
      }
    }
  }
});
```

## Native Debug HUD

`setDebugHudEnabled(true)` makes liberika draw a diagnostic HUD in the native video composition. It
does not render through Dart or alter the Flutter widget hierarchy. It is off by
default and intended for development, performance analysis, and on-device diagnosis.

The low-frequency HUD snapshot includes track codec/resolution/bitrate/frame rate, playback
position and rate, decoded and rendered FPS, hardware/software decode route, zero-copy/fallback
counters, CPU/GPU render times, audio queue and underflow, HDR output negotiation, and danmaku
item count. FPS is derived from adjacent sampling windows; frame and failure counters are
cumulative for the presenter lifetime. The HUD is excluded from `screenshot()` off-screen captures.

For a custom UI, use `getPresenterStats()` to retrieve the latest native display-tick snapshot. It
does not drive the HUD, and its freshness depends on an attached surface and active display loop.

## Diagnostics for Hosts and Agents

Three methods expose the engine's own view for automated troubleshooting
(stutter, corruption, decode failures, A/V drift):

- `getDiagnosticsSnapshot()` returns one JSON map (schema 1): decoder and
  hardware path with fallbacks, presenter counters, output/HDR, resources,
  upscaler, audio, buffer depths, and timing — scheduler late drops,
  presentation cadence and jitter, clock drift and corrections. Unknown values
  are `null`.
- `getLogs({limit, minLevel, sinceSeq})` reads the process-wide, redacted log
  ring: liberika's operational diagnostics plus FFmpeg warnings and errors.
- `invokeJson(method, [arguments])` calls any entry of the native JSON method
  table and returns its envelope (`{ok, status, value | error, code?}`);
  `invokeJson('listMethods')` lists the table, so new engine capabilities need
  no Dart change.

Field reference: [capi_reference.md](capi_reference.md), "Agent diagnostics
(JSON)" and "JSON bridge". When the platform plugin or the bundled native
library lacks a method, the call throws `UnsupportedError`. Status in this
release: macOS, iOS and tvOS resolve the new symbols optionally; Linux,
HarmonyOS and Android route through the shared JSON table; Windows throws
`UnsupportedError`.

On macOS and iOS the plugin runs these three calls on a background queue and
answers on the main thread, so a call that takes long (`addExternalSubtitle` of a
large file, `exportFrame`) does not occupy it. While a presenter method runs,
the plugin's main-thread timers (stats, event polling, macOS rendering) skip a
tick instead of waiting for the player's lock; methods whose `scope` is `global`
(`exportFrame`, `exportFrames`, `getLogs`, `listMethods`) run without that lock.
`listMethods` marks each method `blocking` (returns when the work is done, and
the work grows with its input) and `writesFiles` (creates files; the result
lists their paths).

## Pictures Without a Player

`ErikaPlayer.exportFrame(uri, position: ...)` returns the picture shown at a
media time as RGBA or PNG from its own demuxer and multi-threaded software
decoder; no player is involved. HDR sources (PQ, HLG, Dolby Vision) are tone
mapped to SDR with the arithmetic the player uses for an SDR display, and
`ErikaExportedFrame.toneMapped` says so. For several pictures of one media open
an `ErikaFrameExportSession` (or call `ErikaPlayer.exportFrames`): the media is
opened once and requests in ascending time order continue decoding instead of
seeking again. The work runs off the UI thread: a native worker queue on macOS,
a background isolate per session on the FFI platforms (iOS, tvOS, Android,
Windows, Linux, HarmonyOS). Close a session with `close()`. Set
`ERIKA_FLUTTER_LIBRARY` to a library path to run the FFI exports against a
build under test.

## Neural Upscaler Status

`setUpscaler` requests a mode; the kernels are compiled on a background thread,
so the host should poll `getUpscalerStatus` to drive its UI:

| `activeBackend` | Meaning |
|-----------------|---------|
| `off` | No mode requested. |
| `building` | Kernels compiling (first use of a mode); frames render unscaled until ready. |
| `inactive` | Mode requested but not applied — kernels not ready (and not compiling), or the backend recorded a fallback/failure. |
| `scalar` | Running on the Metal scalar or wgpu compute backend. |
| `simdgroupMatrix` | Running on the `simdgroup_matrix` backend (Apple Silicon default). |

The upscaler only engages when the drawable shows the video larger than its
source resolution, so a 1080p source in a 1080p (or smaller) view stays
`inactive`. C4F16 is the real-time recommendation; C4F16 DS targets heavily
compressed or noisy sources at the same compute cost. On Apple, C4F32 generally
needs an M-Pro/Max-class GPU at 1080p input; on Android, both models use Vulkan
compute and GLES reports an explicit `inactive` fallback. See
`docs/architecture.md` for the renderer-side design.

## Ownership Rule

Flutter owns layout and controls. liberika owns the video plane, subtitle plane,
danmaku plane, audio, and timing. The plugin bridges commands and events through
a `MethodChannel`; rendering never passes through Dart.

## Linux Surface Strategies

`ErikaVideoView` and `ErikaTextureVideoView` publish SDR RGBA pixel buffers to
Flutter after GPU readback. Wayland can use `ErikaWindowOverlayVideoView` as a
native plane below transparent Flutter UI; this avoids per-frame texture
readback but does not support Flutter clipping or color filters on the video.
Build the runtime locally and set `ERIKA_LIBRARY_DIR`; see [Linux integration](linux.md).
