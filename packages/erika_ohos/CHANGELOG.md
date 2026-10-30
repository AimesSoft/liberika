# Changelog

## 0.4.0 - 2026-10-31

- Fixed text subtitles rendering as solid rectangles by enabling libass and
  building its text dependencies (#180).
- Native runtime 0.4.0: opening at a position, the media's end reported,
  a subtitle delay and the current subtitle text through `invokeJson`,
  danmaku capture and native frame texture export (#175).

## 0.3.0 - 2026-10-25

### Default behavior changes

- Enabled subtitle and danmaku supersampling by default; explicit opt-out
  settings remain available.
- Software video decoding now uses FFmpeg frame/slice threading by default;
  `ERIKA_VIDEO_DECODE_THREADS=1` restores single-threaded decoding.

### Playback and SDK updates

- Added native diagnostics, redacted logs, discoverable JSON invocation and
  headless frame export, including sessions and HDR-to-SDR tone mapping.
- Exposed container chapters through presenter JSON (`chapters`, #172).
- Fixed paused seek and subtitle-track switching, including active long cues
  (#171), and kept pending pause intent during asynchronous playback changes.
- Added playback-start failure reasons to traces and handle ordinary HTTP tail
  failures as EOF without masking range/entity verification errors (#169).
- Bounded system fallback-font memory while preserving visible glyphs (#173).
- Improved wgpu compatibility with GLES varying limits and avoided unsupported
  cooperative-matrix discovery on HarmonyOS emulators.
- Updated to the matching 0.3.0 native runtime. The project is now named
  liberika; the ArkTS/OHPM package remains `erika`. iOS/tvOS optional-symbol
  linking is fixed in the shared native release (#170).


## 0.2.1

- Added `httpBackBufferBytes` to `ErikaOpenOptions`, forwarding the HTTP(S)
  rewind budget through the presenter JSON bridge.
- Updated to the matching v0.2.1 native runtime with software decoding fallback
  and cancellable HTTP source and demux teardown.

## 0.1.9

- Published the ArkTS/OHPM package with the matching v0.1.9 native runtime.
- Added `videoAlphaMode` propagation through the ArkTS wrapper and N-API
  bridge while preserving opaque playback as the default.
- Synchronized the OpenHarmony wrapper header with the C API and validated
  HAR installation and consumer HAP builds.

## 0.1.8

- Updated the native runtime with audio-clock recovery after render stalls,
  smooth playback-rate transitions, audio-track startup fallback, and stable
  danmaku placement across planner windows.

- Added HTTP headers and per-open read-ahead tuning to `ErikaPlayer.open`.

## 0.1.7

- Initial ArkTS/OHPM package scaffold.
- Added native N-API presenter bridge for OpenHarmony arm64.
- Added surface attachment, playback control, render ticks, events, screenshots,
  and subtitle memory-font registration.
