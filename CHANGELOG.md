# Changelog

## Unreleased

- Fixed a danmaku memory spike when comments contain characters the embedded
  fallback font does not cover, such as emoji. The danmaku system font
  fallback now checks character coverage with zero-copy lookups before
  loading a face and keeps a bounded set of fallback faces resident with LRU
  eviction while preserving the existing font fallback order, so a single
  uncovered character no longer copies every system font (about 1.65 GB in
  the reported macOS case) into memory.
- Enabled native subtitle supersampling by default. Subtitle and danmaku
  supersampling now default to on in both native and Web backends, with the
  existing size limits, native subtitle CPU fallback and explicit opt-out
  controls preserved.
- Removed the duplicate Web fallback-font asset. libass now uses the complete
  font already embedded in the Rust WASM, eliminating a separate font request
  and about 6.07 MiB of package data (2.74 MiB with gzip). Incremental Web builds
  also remove the old standalone font file.
- Added a browser backend and `cargo run -p xtask -- web build` to bundle the
  native Rust danmaku engine, patched libass/text libraries and an FFmpeg
  demuxer as WASM. WebCodecs decodes audio/video, Web Audio drives the clock,
  and WebGL2 composites video, subtitles and danmaku.
- Added browser MKV playback with embedded ASS/SSA and font attachments,
  external ASS/SSA/SRT/WebVTT, independent subtitle/danmaku supersampling,
  live style updates, appended danmaku files, local comments, track selection,
  chapters and PNG capture. Local files use chunked reads; remote sources use
  a bounded HTTP Range cache.
- Integrated the Web backend into Erika's browser application and verified it
  in Chrome with H.264/AAC/ASS MKV, VP9/Opus WebM, 2,000 comments, playback
  controls, screenshots, UI transitions and player cleanup. Web distribution
  currently uses source builds; see the [browser guide](docs/web.md).
- Fixed HTTP(S) reads and persistent prefetch for origins that ignore Range
  and return `200` with the whole file, including HLS playlists and segments
  without Content-Length. Replayed prefixes are discarded before caching,
  bounded reads stop at the requested length, and EOF can supply the total size.
  Retries reject changed entity validators instead of mixing file versions.
  Entity validation failures also block synchronous fallback and later reads;
  weak ETags cannot authorize resuming a partial response.
- Added a process-wide diagnostics log ring, read with the new
  `erika_log_read_json`. It keeps operational diagnostics (with a level and
  timestamp), operational failures that previously went to bare stderr, and
  FFmpeg warnings and errors through an `av_log` bridge that never blocks,
  folds repeats and rate-limits floods. Exports are redacted (URL query strings
  and user info, credentials, home directories). Existing stderr, logcat and
  hilog output is unchanged; `ERIKA_FFMPEG_LOG_CAPTURE=0` turns FFmpeg capture
  off, and Linux builds against a shared system FFmpeg capture only with
  `ERIKA_FFMPEG_LOG_CAPTURE=1`.
- Added `erika_presenter_get_diagnostics_json`, a one-call JSON snapshot of the
  decoder and hardware path, presenter counters, output/HDR, resources,
  upscaler, audio, buffer depths and timing. Timing adds counters that did not
  exist before: frames the scheduler dropped for being late, late frames shown
  after the drop budget ran out, presentation cadence and jitter, and audio
  clock drift, corrections and snaps. Existing structs are unchanged.
- `erika_presenter_invoke_json` is now a discoverable pass-through: one
  dispatch table backs it, `listMethods` returns that table with arguments and
  results, `invokeJson` nests a call, `listMethods`/`getLogs` accept a NULL
  handle, and unknown methods return `"code": "unknown_method"`. The table adds
  `getDiagnosticsSnapshot`, `getLogs`, `setDebugHudEnabled`,
  `setOutputHeadroom`, `trackSelection`, `getDanmakuConfig`,
  `setSubtitleFont` and `setSubtitleStyle`; Android's JSON bridge falls back to
  it for methods it does not implement itself.
- Fixed decoder reporting when FFmpeg drops VideoToolbox or D3D11VA after the
  decoder opened: when the hwaccel fails to initialise (or is not offered for
  the stream), FFmpeg renegotiates and decodes in software, while liberika kept
  reporting the hardware backend with no fallback. The format negotiation is
  now recorded; the playback session emits a decoder change
  (`hwaccel_<backend>_to_software`, or `…_restored` when the hardware returns)
  with the software pixel format and the reason, counts the fallback, and tags
  frames with the backend that actually decoded them.
- Added `erika_export_frame` and `erika_frame_export_result_free`: a headless
  single-frame export that returns the picture shown at a media time as RGBA or
  PNG, scaled within optional bounds, from its own demuxer and multi-threaded
  software decoder.
- Added frame export sessions: `erika_frame_export_session_open`,
  `erika_frame_export_session_frame` and `erika_frame_export_session_close`
  open a media once and take pictures from it one after another. A request
  after the previous one continues decoding from where the decoder stopped
  when no keyframe lies between (the demuxer's seek index tells), and seeks
  otherwise. 60 pictures 100 ms apart from a 4096x1744 H.264 file took 23.9 s
  through one `erika_export_frame` call each (60 opens and seeks) and 1.0 s
  through a session (one seek, 335 frames decoded). The JSON
  method table gains `exportFrame` and `exportFrames` (scope `global`); each
  table entry now says whether it is `blocking` and whether it `writesFiles`.
- Frame exports tone map HDR sources (PQ, HLG, Dolby Vision) to SDR with the
  arithmetic the video renderer applies for an SDR display (ITU-R BT.2390 EETF
  against a 203-nit target, BT.2020 to BT.709 gamut mapping, gamma 2.2) instead
  of FFmpeg's plain conversion, and set `ERIKA_FRAME_EXPORT_TONE_MAPPED` in
  `reserved_flags` of the result. Against a screenshot of the player on an SDR
  display, a 4K HDR10 frame differs by 0.1 to 0.3 of 255 on average (7 to 9
  before).
- Software video decoders now use frame and slice threading (FFmpeg picks the
  count) for playback as well as exports; `ERIKA_VIDEO_DECODE_THREADS=1`
  restores single-threaded decoding. Forced to software, 4K H.264 at 3x
  presented 72 fps with no late drops where one thread presented 7 fps and
  dropped 481 frames in 15 s, and the time from a seek to the first picture
  fell from 2.8 s to 0.43 s (median). `ERIKA_HWDEC=software` makes software
  decoding the default on every platform, for measuring it.
- Fixed `erika_presenter_select_subtitle_track` (and `Player::select_subtitle_track`)
  failing with `ErikaStatus 3` when asked for no subtitle while no media was
  open, which is how a host clears the subtitle before opening: it now succeeds
  and changes nothing.

## 0.2.1 - 2026-10-02

### Playback and HTTP(S)

- Restored software decoding fallback when a native hardware decoder cannot
  handle the source, including WMV/WMA and 8/10-bit AV1 through bundled dav1d.
  Windows D3D11 now handles software-decoded frame uploads consistently.
- HTTP I/O is cancellable during connection, TLS, response headers, and body
  reads. Source and demux teardown cancel I/O and join owned workers; stop
  releases HTTP buffers while preserving replay, and close destroys the session.
- Enforced the read-ahead window at every persistent-stream stripe boundary.

### Flutter and OpenHarmony

- Added `httpBackBufferBytes` to Flutter and standalone OpenHarmony open
  options. All existing SDK bridges forward the rewind budget to the native
  engine; zero or omission keeps the 16 MiB default.
- iOS samples danmaku and subtitles at the display presentation target, using
  a coalescing timestamp mailbox and optional `erika_presenter_render_tick_with_timing`.
  Older libraries retain the legacy render-tick behavior.

### Experimental Linux support

- Added native and Flutter Linux playback on X11/Wayland, PulseAudio output,
  NVDEC/VA-API hardware decoding, and software fallback.
- Added native Wayland video subsurfaces, experimental direct VA-API Vulkan
  frame import, and opt-in WSL D3D12 GPU plane copies. Physical Linux HDR and
  Intel/AMD direct frame import remain unverified; Linux requires a source
  build and is not included in the prebuilt release archives.
- Fixed P010 interpolation to retain full signal precision.

## 0.2.0 - 2026-09-23

### Playback

- HTTP(S) prefetch now uses one persistent stream with an open-ended GET
  (`bytes=anchor-`), avoiding a request and server seek on every 4 MiB piece
  without downloading overlapping tails. The worker stops reading its socket
  when the read-ahead window fills; seeks retire the old stream through an
  epoch counter. Short responses and body errors resume at the reached offset
  with the entity validator replayed; repeated failures fall back to bounded
  synchronous requests. Cache appends preserve byte continuity, and final
  partial reads return at the known EOF without waiting for a full buffer.
- The rewind budget is host-tunable via `http_back_buffer_bytes` on the open
  options (the first reserved slot — ABI and layout unchanged; zero keeps the
  16 MiB default). Hosts playing high-bitrate media should size it from the
  bitrate: a -10 s skip at 71 Mbps covers ~89 MB, which the fixed default
  could not hold.
- Capped synchronous HTTP(S) range requests at 4 MiB so a large window no longer demands
  unrealistic sustained bandwidth (a single 32 MiB request needed ~18 Mbps to
  survive ureq's 15 s body deadline); failed background work degrades to
  synchronous fetches instead of killing the read, and an origin that keeps
  delivering bytes licenses more resume attempts than the flat three-attempt
  rule.
- Surface the media source's own error (HTTP status, timeout) in demux errors
  rather than a bare `Input/output error (-5)`.

### Renderer

#### Dolby Vision and HDR tone mapping

- Dolby Vision RPU mapping via libplacebo-style piecewise polynomial/MMR
  reshaping, followed by the RPU nonlinear matrix, PQ linearization, and
  LMS→RGB composite. Profile 5/8 VUI tags are forced to BT.2020/PQ.
- Profile 5 falls back to software decode on mobile backends (MediaCodec /
  generic AvCodec) that cannot attach RPU side data to hardware frames;
  VideoToolbox and D3D11VA keep hardware decode.
- Profile 7 FEL/MEL RPUs are no longer rejected outright. An RPU whose
  `disable_residual_flag` asks for an enhancement layer still applies its
  base-layer reshaping curves, color matrices, and L1 trims — dropping the
  whole RPU threw those away and left the frame as plain HDR10. The
  un-composable residual is reported through the new `Frame::dovi_el_status()`
  and one throttled `dovi_el_not_composed` diagnostic per stream, matching
  libplacebo's split between `nlq_active` and "consumers that have not bound an
  enhancement layer must not look at these fields".
- Per-frame Dolby Vision L1 brightness (`min_pq`/`max_pq`/`avg_pq`) drives the
  tone-map source peak and scene-average pivot; the static mastering-display
  (L0) peak still decides output-mode negotiation and whether tone mapping /
  the perceptual gamut LUT are enabled.
- Tone mapping switched to a libplacebo-style IPT-domain color map with
  **BT.2390 EETF as the default** operator, plus spline, BT.2446 method A,
  SMPTE ST 2094-10, Mobius, Reinhard, and clip. Black-point compensation uses
  `target black = target peak / contrast_ratio` (auto 1000:1 for SDR).
- Perceptual gamut mapping via a CPU-generated 48×32×256 IPT 3D LUT
  (following libplacebo's lattice and index mapping) when a tone-mapped HDR
  source is compressed into a smaller gamut; generated on a background thread
  so the first wide-gamut frame does not stall the render thread. Chroma
  rolloff is currently a simplified dead-zone blend rather than libplacebo's
  full per-hue boundary search.
- Scene-adaptive HDR10: software-decoded PQ frames without DoVi L1 get a
  CPU-measured scene-average luminance (`luma_stats`) that drives the
  tone-map pivot, mirroring mpv's `--hdr-compute-peak`.
- Metal EDR negotiation reads the presenting screen rather than
  `NSScreen.mainScreen`, and Auto HDR is decided from that display's
  capability. Callers must set `CAMetalLayer.delegate` (examples updated).
- Metal: the perceptual gamut LUT is only marked active when the texture
  resolved for the frame actually matches its (source, target, target-peak)
  key. A headroom or primaries change while a new LUT generates used to bind
  the 1x1x1 placeholder with `gamut_lut_enabled` still set, rendering black
  frames until generation finished. D3D11 now masks the uniform the same way
  while its LUT is still generating.
- ST 2094-10 tone mapping picks its knee in the PQ domain, matching
  libplacebo's internal rescale, and `ToneMapConfig::curve_param` now tunes
  its knee adaptation (default 0.70, libplacebo's `param_def`). Previously the
  parameter had no effect and the knee was selected in linear nits.
- The tone map, its black-point compensation, and the perceptual gamut LUT are
  enabled from the static mastering-display (L0) peak rather than the
  per-frame Dolby Vision L1 peak, so dark scenes no longer drop them for a
  frame and then restore them.
- Metal EDR / extended-linear output linearizes subtitle and danmaku colors
  before compositing into the linear drawable; previously they were written
  gamma-encoded, making mid-tones and colored text too bright.
- HDR10 scene-luma measurement honors the frame's color range: limited (TV)
  range luma planes are expanded over the legal code span like the shaders'
  `expand_ycbcr_range`, instead of being measured as full range. The
  measurement also distinguishes `yuv420p10le` (10-bit code in bits `[9:0]`)
  from P010 (left-aligned in `[15:6]`); treating both as P010 measured
  software Main10 HDR10 as near-black.
- The perceptual gamut LUT's I axis now spans the target's `[black, peak]` in
  PQ codes (libplacebo's `gamut.min_luma`/`max_luma`) instead of
  `[0, peak]`; generation and all three backends' samplers agree, and the
  cache key includes the target black, so a `contrast_ratio` change
  regenerates the LUT. SDR targets shift their gamut mapping by up to ~3.5%
  of the I axis; HDR/EDR targets are unchanged (black = 0).
- A degenerate ST 2094-10 anchor set (duplicated/non-finite points) falls back
  to the identity curve (Clip) instead of zero coefficients, which the shader
  would have rendered as a black frame.

## 0.1.8 - 2026-09-07

### Compatibility

- The presenter configuration ABI now includes packed-alpha video and native
  compositing options. Native embedders must rebuild against the matching
  `erika.h` and runtime; do not mix configuration structs from older releases
  with the 0.1.8 binaries. Flutter packages pin matching native artifacts.

### C API

- Added `ErikaOpenOptions` and `erika_open_with_options` /
  `erika_presenter_open_with_options`, superseding the `_with_headers` pair.
  The options struct bundles the header array with per-request tuning, starting
  with `http_read_ahead_bytes` to override the HTTP(S) read-ahead window
  (0 uses `ERIKA_HTTP_READAHEAD_BYTES` when set, otherwise the 2 MiB default;
  an explicit value supersedes the environment variable). Reserved fields must
  be zero and are validated so future fields cannot silently change behavior.

### Flutter and OpenHarmony

- Added `httpReadAheadBytes` to Flutter `ErikaPlayer.open` on Android, Apple
  platforms, Windows, and OpenHarmony, and exposed matching open options in the
  standalone OpenHarmony SDK.

### Danmaku

- Kept both accepted and rejected placement decisions stable across sliding
  planner windows, preventing dropped comments from reappearing mid-flight or
  forcing visible comments onto another lane.
- Decoupled host-provided danmaku IDs from internal layout identity and parse
  optional JSON IDs as exact `u64` values without floating-point rounding.

### Playback

- Host-provided Metal layers explicitly retain the finite drawable timeout
  (up to one second before returning nil); skipped presents are counted with
  throttled diagnostics.
- Recover audio-master clock drift after a blocked render tick without increasing
  audio queue depth. Presenter feedback retains its capture time, playback intent,
  generation, and output epoch; stale feedback and silence-only callbacks cannot
  drive clock correction. Each new output first establishes a progress baseline.
- Avoid replaying already queued audio when video decoding resumes in the
  foreground, and defer iOS foreground resume until the host is ready while
  recovering audio interruptions.
- Smooth playback-rate changes with a bounded old-rate audio bridge while
  preserving audio-master synchronization at non-default rates.
- When the first audio stream cannot be decoded, try the remaining audio
  streams in container order and report all decoder failures if none can be
  opened.
- Added packed-alpha video presentation with premultiplied GPU output on
  Windows and macOS, including native backdrop-aware overlay composition.
- Raised the playback-rate ceiling to 16x.

### Windows rendering

- Fixed DWM crashes and striped video caused by unsafe D3D11 frame reuse, kept
  native opaque playback on its HDR-capable path, and corrected fill sizing.
- Added an explicit SDR Flutter texture path and synchronized GPU texture
  handoff. Player shutdown now joins the danmaku worker before unloading its DLL.

### SDK distribution

- Added a Swift SDK binary archive and unified native, Flutter, OpenHarmony,
  and Swift package releases behind the native version tag.

## 0.1.7 - 2026-08-16

### Flutter package distribution

- Prepared `erika_flutter` for its first pub.dev release with package-local
  licensing, changelog, metadata, examples, and standalone package tests.
- Made verified, version-pinned native release bundles the default on Android,
  Apple platforms, Windows, and OpenHarmony; source builds are now explicitly
  selected with `ERIKA_FORCE_SOURCE_BUILD=1`.
- Added per-ABI Flutter Android runtime archives so app builds download only
  the requested architecture and omit the native-embedder static library.
- Added a release-level `SHA256SUMS` manifest covering every published native
  archive.
- Added isolated GitHub Actions consumers for Android, iOS, macOS, tvOS,
  Windows, and OpenHarmony so package builds cannot depend on the monorepo.

### OpenHarmony package distribution

- Published the independent native ArkTS package `erika` to OHPM for
  OpenHarmony arm64 (API 18+), with `XComponent` surface lifecycle,
  `renderTick()` scheduling, playback controls, events, and screenshots.

## 0.1.6 - 2026-08-14

### Rendering and playback

- Added the ArtCNN C4F16 DS denoising/sharpening luma upscaler for degraded
  anime sources across the Rust, C, Flutter, Metal, and wgpu interfaces.
- Moved Android and macOS presenter rendering off the application UI/main
  thread, reduced Android AHardwareBuffer conversion bandwidth, and prevented
  D3D11 frame reuse across devices.
- Added source-aware SDR/EDR output selection on Apple platforms and exposed
  resource status through the Android, Windows, and OpenHarmony integrations.

### Integration and behavior

- Disabled danmaku scroll overwrite by default, preserving already-stable
  comments during dense playback.
- Restored the Windows system-media-controls build and made the OpenHarmony
  Flutter plugin's ArkTS declarations explicit.

## 0.1.5 - 2026-08-03

### System media controls and background audio

- Added Now Playing metadata, playback state, timeline, play/pause/seek, and
  previous/next navigation controls across iOS, tvOS, macOS, Android, Windows,
  and OpenHarmony.
- Added opt-in background audio playback for iOS and Android while suspending
  video decoding, with bounded playback-worker barriers so lifecycle callbacks
  cannot block an application thread indefinitely.
- Cleared stale metadata when opening media without `ErikaMediaMetadata`, made
  pause/play sequencing deterministic, and isolated Android media callbacks
  across multiple Flutter engines.
- Cached Android artwork and avoided rebuilding media notifications when native
  playback state has not changed.

### Platform support and release artifacts

- Added the tvOS Flutter plugin and native target support for Apple TV devices
  and arm64/x86_64 simulators, including Metal presentation, Apple audio and
  VideoToolbox integration, subtitles, danmaku, screenshots, and HTTP sources.
- Added `erika-capi-tvos.zip`, containing a tvOS device and universal simulator
  `erika_capi.xcframework`, to the automated GitHub Release workflow.
- Added opt-in OpenHarmony prebuilt consumption: the plugin CMake build can
  download the tagged `liberika_capi.so`, stage it beside
  `liberika_flutter.so`, and fall back to a source build on failure.
- Unified macOS, iOS, and tvOS CI/release builds on macOS 26 runners; macOS x64
  is cross-built alongside arm64 before the universal archive is assembled.
- Fixed tvOS platform detection during native dependency builds and accepted
  valid zero-length HTTP resources without misclassifying them as truncated.

### Subtitle memory fonts

- Added an in-memory subtitle font registry across Rust, the C API, Dart, and
  every native Flutter embedding. Hosts can inspect font faces, select ordered
  fallback sets, replace font data, and clear the registry without writing font
  files to disk.
- Preserved ordered fallback in the bundled libass build, shared TTC/OTC data
  across faces, and invalidated renderer caches when a same-path font is
  replaced so active subtitle tracks immediately use the new data.
- Kept registry and selected-font generations separate, replayed libass codec
  private data after renderer recreation, and added OpenHarmony and tvOS C API
  coverage for the complete memory-font surface.

### Playback recovery

- Added explicit buffering transitions when audio and video input starves. The
  playback clock now freezes during a stall and reanchors to recovered media
  time instead of running ahead and causing a prolonged frame drop or black
  screen after data resumes.

## 0.1.4 - 2026-07-30

### Compatibility notes

- **`outline_width` is now a profile, not a multiplier.** `0` is off, `1` fine,
  `2` normal, `3` thick; values above `3` clamp to thick. Normal and thick
  reproduce exactly the widths the old continuous multiplier produced at `1.0`
  and `2.0`, so a host that used to send `outline_width: 1.0` must now send
  `2.0` to keep the same stroke. Left unmigrated, outlines render thinner —
  at the default font size the rasterized radius halves from 2 px to 1 px.
- **Danmaku font size is now interpreted as pixels per em.** `ab_glyph`'s
  `PxScale` is an ascent-to-descent height, and the previous code passed the
  em size straight into it, so text rendered smaller than requested by the
  font's own height/em ratio. Text now matches the requested size, which means
  existing users see larger danmaku on upgrade: the ratio is 1.0 for STHeiti
  and Hiragino Sans GB, but 1.4 for PingFang SC (the default macOS face), so
  the same configuration can render up to 40% larger and fit fewer tracks.
  Hosts that want the previous look should lower their configured font size.
- Scroll duration now scales with the viewport's logical width (×0.9 at 640 pt
  up to ×1.3 at 1920 pt and wider) so a danmaku crosses wide windows in a
  comparable amount of visual time. The same configuration therefore scrolls
  more slowly on a large window than it did before.
- Danmaku screenshots no longer include danmaku; `capture_*` composites video
  and subtitles only. The debug HUD is also excluded from captures.

### Platform support and release artifacts

- Added an OpenHarmony player backend and Flutter plugin with AVCodec H.264/HEVC
  hardware decoding, OHNativeBuffer/Vulkan zero-copy presentation, WGPU
  composition, subtitles, danmaku, audio, diagnostics, and RGBA screenshots.
  The release includes `erika-capi-openharmony-arm64.zip` with the C API runtime
  and Flutter native bridge.
- Added native Windows ARM64 dependency and `erika_capi` builds, plus the new
  `erika-capi-windows-arm64.zip` release archive. Windows x64 and ARM64 CI run
  on matching GitHub-hosted architectures.
- Added selectable macOS arm64, x86_64, and universal builds and corresponding
  architecture-specific release archives.
- Fixed Android source builds on Apple Silicon by selecting the universal
  `glslc` shipped in the NDK's `darwin-x86_64` tools directory.
- Re-enabled FFmpeg's optimized x86 assembly for shipped x86_64 builds. Source
  builds for these targets now require NASM.

### Playback, rendering, and window integration

- Published the shared playback clock directly and made frame snapshots sample
  time, state, and generation atomically. Danmaku, subtitles, and rendering now
  use the same time base without a separate forward-only display clock.
- Paused seeks now decode and present the requested preview frame while keeping
  the media clock frozen. Seek preroll, EOF, immediate pause-after-seek, and
  resume startup races no longer produce clock rollback, worker spin, or a
  prolonged black frame.
- Separated audio and video demux demand so audio backpressure cannot stall video
  packet scanning, while retaining a bounded decoded-audio queue. Switching or
  adding subtitles no longer resets audio/video demux selection.
- Reworked macOS presentation around `CVDisplayLink`, coalesced slow frames, and
  retargeted the display link after screen changes. Reduced redundant overlay
  attachment and GPU resource work during resize and window migration.
- Added multi-`FlutterView` and secondary-window targeting to the desktop overlay
  API. macOS and Windows overlays now follow a player between host windows while
  preserving surface, visibility, generation, and aspect-fit state.
- Improved Windows D3D11 overlay lifetime, non-blocking presentation, and resume
  seek handling. Subtitles are composited in the video viewport while danmaku
  remains in the full-window viewport.

### Danmaku and subtitles

- Aligned danmaku collision bounds with rasterized outlines, corrected
  shadow/outline/text layer ordering, stabilized track preference, and limited
  high-density overlap to overflow tracks.
- Kept the previous danmaku plan moving while asynchronous relayout completes;
  paint-only settings reuse layout, visibility changes apply immediately, and
  stale plans cannot reappear after danmaku is disabled.
- Added incremental glyph-atlas uploads and reusable Metal instance buffers, and
  reduced per-frame DFM allocation and candidate traversal overhead.
- Added charset detection and transcoding for external text subtitles, including
  GBK, Big5, Shift_JIS, and UTF-16, with UTF-8 passthrough and guarded fallback
  for low-confidence or binary input.
- libass now registers liberika's bundled Droid Sans Fallback on every platform,
  not just iOS/Android, and targets without a system font provider default to
  that family instead of an unresolvable `Arial`.
- Added subtitle fallback and selective override styling: custom font family and
  file, RGBA colours, metrics, text attributes, border, alignment, margins, and
  blur. New entry points are `erika_presenter_set_subtitle_font`,
  `erika_presenter_set_subtitle_style`, and `ErikaPlayer.setSubtitleStyle`.

### Networking and media diagnostics

- Added `erika_open_with_headers` and `erika_presenter_open_with_headers`.
  `ErikaPlayer.open` accepts `httpHeaders` on Android, iOS, macOS, and Windows,
  and applies them to the probe, ranged reads, retries, and prefetch requests.
  The original open functions remain compatible and use an empty header list.
- Rejects caller-supplied transport headers managed by liberika and invalid HTTP
  field names or values. External subtitle and danmaku sidecars remain on the
  headerless path and do not inherit media request headers.
- Hardened HTTP range input against ignored `Range` requests, incorrect response
  offsets, transient request/body failures, partial responses, duplicate
  prefetch downloads, and false EOF. Retries are bounded across the complete
  fetch operation.
- Added track bitrate and rational frame-rate metadata across Rust, C, and Dart,
  plus an opt-in native debug HUD for decoder, renderer, GPU, audio, output/HDR,
  and danmaku diagnostics.

### Audio, colour, and codec updates

- Normalized surround-to-stereo downmix matrices to prevent clipping.
- Added smooth per-callback volume ramps on all audio backends and WASAPI device
  loss recovery with observable recovery state and bounded backoff.
- Added BT.2100 HLG decoding on Metal, WGPU, and D3D11, and correct PQ encoding
  for D3D11 overlays on HDR10 output.
- Upgraded FFmpeg to 8.1.2, improved Darwin AV1 hardware decode/import handling,
  and preserved the last frame during seek loading instead of flashing black.

## 0.1.3 - 2026-07-17

### Android playback and packaging

- Added the complete MediaCodec, AHardwareBuffer/wgpu, AAudio, Flutter
  PlatformView, SAF/content-source, subtitle, danmaku, screenshot, SDR/HDR,
  diagnostics, and recovery paths.
- `ERIKA_PREBUILT=1` now stages `liberika_capi.so` and `libc++_shared.so` from
  the tagged Android release archive for the requested Flutter ABIs, with an
  explicit source-build fallback.

### Breaking C API surface-size semantics

The `width` and `height` arguments passed to
`erika_presenter_attach_metal_layer`, `erika_presenter_attach_wgpu_surface`,
`erika_presenter_attach_wgpu_surface_with_output_capabilities`,
`erika_presenter_attach_windows_hwnd`, and `erika_presenter_resize_surface`
now mean the exact drawable extent in physical pixels.

Previously, native renderers multiplied those values by `scale`. The `scale`
argument is now independent and affects logical UI content such as danmaku; it
never changes the surface extent. Direct C API hosts that currently pass logical
dimensions must convert them to physical pixels before calling these functions.
The in-tree macOS, iOS, Windows, and Android Flutter embeddings and examples
have already been updated.

### Playback command dispatch

`play` is queued asynchronously and no longer waits indefinitely for the
playback worker. Hosts must observe `StateChanged` and `Error` events for the
authoritative result of the transition.
