# Changelog

## Unreleased

- Fixed OpenHarmony builds shipping the debug text subtitle renderer, which
  displayed embedded ASS and other text subtitles as solid rectangles. CI,
  release packaging and Flutter source builds now include libass and its text
  dependencies, and validate the linked renderer before packaging (#180).

## 0.3.0 - 2026-10-25

### Default behavior changes

- Enabled subtitle and danmaku supersampling by default; explicit opt-out
  settings remain available.
- Software video decoding now uses FFmpeg frame/slice threading by default;
  `ERIKA_VIDEO_DECODE_THREADS=1` restores single-threaded decoding.

### Playback and SDK updates

- Fixed subtitles during paused seeks and track switches, including long cues
  spanning the seek position (#171).
- Added container chapters through `invokeJson('chapters')` (#172).
- iOS/tvOS force-link only optional entry points actually present in the
  selected native archive, retaining compatibility with older runtimes (#170).
- Added playback-start failure reasons to native traces, fixed ordinary HTTP
  tail read failures becoming playback errors (#169), and bounded system-font
  fallback memory without hiding previously visible symbols (#173).
- Renamed the project to liberika; the `erika_flutter` package name is unchanged.


- Added `ErikaPlayer.getDiagnosticsSnapshot()`, `getLogs()` and
  `invokeJson()`. They return JSON maps from the native engine for automated
  troubleshooting; `invokeJson('listMethods')` lists every native method.
  macOS, iOS and tvOS resolve the new native symbols optionally, Linux,
  HarmonyOS and Android use the shared JSON bridge, and Windows (or an older
  native library) throws `UnsupportedError`. Requires native artifacts from
  the matching release.
- Added `ErikaPlayer.exportFrame()`, which returns the picture shown at a
  position of a media URI as RGBA or PNG through the native headless frame
  export, off the UI thread (a native worker on macOS, a background isolate on
  the FFI platforms).
- Large JSON documents from `getLogs()`, `getDiagnosticsSnapshot()` and
  `invokeJson()` are decoded on a background isolate.
- On macOS and iOS `getLogs()`, `getDiagnosticsSnapshot()` and `invokeJson()`
  run on a background queue and answer on the main thread; while one of them
  runs inside the player the plugin's main-thread timers skip a tick instead of
  waiting for the player's lock, and methods with scope `global` run without it.
- Added `ErikaFrameExportSession` and `ErikaPlayer.exportFrames()`: the media is
  opened once and many pictures are taken from it (a native worker queue on
  macOS, a background isolate per session on the FFI platforms).
  `ErikaExportedFrame.toneMapped` says whether an HDR source was tone mapped to
  SDR. `ERIKA_FLUTTER_LIBRARY` points the FFI exports at a library under test.
- The iOS and tvOS podspecs retain available export, diagnostics and JSON
  functions for the plugin's and Dart's lookups in the running executable.
- The macOS build phase checks that an `ERIKA_MACOS_CAPI_DYLIB` override has
  every architecture being built and says which library it bundles.

## 0.2.1

- Added `httpBackBufferBytes` to `open`, forwarding the HTTP(S) rewind budget
  through Android, Apple platforms, Windows, and OpenHarmony so high-bitrate
  media can keep more than the 16 MiB default cached behind the reader.
- Added experimental Linux playback on X11/Wayland, including a native Wayland
  video layer. Linux requires a separately built `liberika_capi.so`.
- Improved iOS danmaku/subtitle timing by sampling at the display presentation
  target and coalescing pending render ticks.
- Restored native software decoding fallback and fixed HTTP worker cancellation
  and resource teardown. Updated packages use matching v0.2.1 native artifacts.

## 0.2.0

- Added headless GIF export through the native liberika runtime.
- Added Dolby Vision RPU mapping and a libplacebo-style HDR tone/gamut pipeline.
- Improved HTTP(S) streaming with persistent prefetch, resumable range reads,
  and a host-tunable rewind cache.
- Updated the native artifacts and checksums for the matching v0.2.0 release.

## 0.1.9

- Published the Flutter package with the matching v0.1.9 native artifacts.
- Fixed OpenHarmony packed-alpha playback by forwarding `videoAlphaMode`
  through the Dart, ArkTS, and N-API layers.
- Kept the C API, platform bindings, and prebuilt runtime checksums aligned
  across Android, Apple platforms, Windows, and OpenHarmony.

## 0.1.8

- Fixed persistent audio/video drift after rendering stalls and overlapping
  queued audio during foreground recovery.
- Fixed Windows DWM crashes, frame reuse, video fill sizing, and DLL shutdown;
  native opaque playback retains HDR, with an explicit SDR Flutter texture path.
- Smoothed playback-rate transitions and improved iOS foreground/interruption
  recovery, audio-track startup fallback, and danmaku continuity.
- Updated native artifacts and the presenter ABI together; native integrations
  must use the matching headers and runtime.

- Added per-open HTTP read-ahead tuning through `httpReadAheadBytes` on
  Android, Apple platforms, Windows, and OpenHarmony.
- Added a packed-alpha video mode that stores color and alpha side by side,
  reconstructs premultiplied transparency in the GPU renderer, and propagates
  the mode through the C ABI and Flutter platform integrations.
- Added a macOS `ErikaTextureVideoView` backed by IOSurface and Metal for
  Flutter-composited opacity, clipping, transforms, and color filters without
  per-frame CPU pixel readback.
- Added native macOS opacity and overlay compositing for transparent video
  platform views when backdrop-aware blending is required.
- Added Windows DirectComposition presentation for transparent video, with a
  premultiplied-alpha swap chain attached directly to the Flutter HWND and
  native overlay blending/opacity instead of a covering popup window.
- Raised the native playback-rate ceiling to 16× for short-form video effects.

## 0.1.7

- Published `erika_flutter` as a standalone pub.dev package with package-local
  license, changelog, metadata, and runnable iOS and macOS examples.
- Made verified, version-pinned native bundles the default for Android, Apple
  platforms, Windows, and OpenHarmony.
- Split Flutter Android runtimes by ABI so app builds download only the selected
  architecture and omit native-embedder static libraries.
- Added explicit `ERIKA_FORCE_SOURCE_BUILD=1` source builds without silent
  fallback when a prebuilt download or checksum fails.
- Added isolated package and cross-platform consumer validation in GitHub
  Actions.

## 0.1.6

- Added the ArtCNN C4F16 DS denoising and sharpening upscaler.
- Added source-aware SDR and EDR output selection on Apple platforms.
- Moved Android and macOS presentation work off the application UI thread.
- Exposed renderer resource status on Android, Windows, and OpenHarmony.
- Restored Windows system media controls and tightened the OpenHarmony bridge.

See the [repository changelog](https://github.com/AimesSoft/liberika/blob/main/CHANGELOG.md)
for native engine and earlier release details.
