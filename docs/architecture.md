# liberika Architecture

[中文](architecture.zh.md) | [English](architecture.md) | [日本語](architecture.ja.md)

liberika is an embeddable Rust media playback library. Host applications call into
the engine through the Rust API, a C ABI (`erika_capi`), or Flutter bindings
(`erika_flutter`). Video frames, subtitles, and danmaku stay inside the engine
and are composited in the renderer — they do not flow through the host.

The [browser backend](web.md) exposes a JavaScript/TypeScript `Player`. It
reuses Rust danmaku and patched libass in WASM, with FFmpeg container demuxing
and MKV font attachments; WebCodecs, Web Audio and WebGL2 provide decoding,
the media clock and final composition. Erika Web uses this path. The overview
below describes the native runtime; the browser guide documents its Worker
pipeline and lifecycle.

## System Overview

```text
Rust Player Core
  source abstraction ─── file + HTTP range
  FFmpeg wrappers ────── custom AVIO, probe, demux, decode, seek, audio resample
  playback engine ────── video/audio tick, clock, frame scheduler
  video decode ───────── VideoToolbox, D3D11VA, MediaCodec, AVCodec, NVDEC, VA-API, software fallback
  audio output ───────── CoreAudio, AudioQueue, WASAPI, AAudio, OHAudio, PulseAudio, ring buffer
  overlay timeline ───── subtitle + danmaku composition
  renderer core ──────── color state, render graph, tone map, scaler policy
  Metal renderer ─────── zero-copy NV12/P010, HDR/EDR, subtitle/danmaku pass
  D3D11 renderer ─────── zero-copy D3D11VA, HDR10, subtitle/danmaku pass (Windows)
  wgpu renderer ──────── cross-platform video, overlays, capture, Android scRGB, OHOS Vulkan
  presenter runtime ──── ties player + renderer + audio + overlays
  C ABI ──────────────── versioned public header, two handle families
  Flutter plugin ─────── macOS + iOS + tvOS + Windows + Android + OpenHarmony + Linux embedding
```

## HTTP sources and display timing

The HTTP Range source manages read-ahead and rewind caches with byte budgets. `http_read_ahead_bytes` defaults to 2 MiB (overridable with `ERIKA_HTTP_READAHEAD_BYTES`); `http_back_buffer_bytes` defaults to 16 MiB. C `ErikaOpenOptions` and Dart `open()` can override these per request. Stop, reopen, and close cancel the previous session's network tasks and release its resources.

The iOS Flutter bridge uses `CADisplayLink.targetTimestamp` and a mailbox that retains only the latest tick, avoiding a rendering queue backlog. `render_tick_with_timing` samples video, subtitles, and danmaku for one display target while preserving audio-master clock progression. See the [C ABI reference](capi_reference.md).

## Native Dependencies

`xtask` downloads, builds, and installs native dependencies from pinned upstream
sources into `third_party/`. The default profile is `lgpl`.

| Dependency | Version | Purpose |
|------------|---------|---------|
| FFmpeg | 8.1.2 | Demux, decode, audio resample, platform hardware decode |
| dav1d | 1.5.1 | AV1 software decode on every platform (8-bit and high bit depth) |
| libass | 0.17.5 | ASS subtitle rendering |
| FreeType | 2.14.3 | Font rasterization (libass dependency) |
| HarfBuzz | 14.2.1 | Text shaping (libass dependency) |
| FriBidi | 1.0.16 | Bidirectional text (libass dependency) |

Release targets statically link dependencies; Linux uses system FFmpeg and PulseAudio with patched libass. libass and its dependencies are enabled
by default (`features = ["libass"]`).

```sh
cargo run -p xtask -- deps build --all --profile lgpl
cargo run -p xtask -- deps status
```

## FFmpeg Integration

`erika_ffmpeg_sys` generates low-level bindings via bindgen at build time.
`erika::ffmpeg` provides safe Rust wrappers:

- **Demuxer** — owns `AVFormatContext`, optionally with a Rust-backed custom
  `AVIOContext` from `MediaSource`. Supports stream selection, reference-counted
  packets, and timestamp-based seek.
- **Decoder** — software plus VideoToolbox, D3D11VA, MediaCodec, and OpenHarmony
  AVCodec hardware backends. Software AV1 on every target selects the
  source-built `libdav1d` decoder. Hardware frames preserve color metadata for
  the renderer's platform-specific import or upload path.
- **AudioResampler** — wraps `libswresample`, converts to interleaved f32 PCM
  (default 48 kHz stereo).
- **SubtitleDecoder** — decodes embedded text and bitmap subtitle streams.

Hardware decode preferences fall back to software when decoder opening,
packet decoding, or renderer import fails. This applies to VideoToolbox,
D3D11VA, MediaCodec, and OpenHarmony AVCodec for every codec; it is not
restricted to AV1. MediaCodec keeps its Surface → byte-buffer → software
sequence. Import recovery rejects stale feedback and releases queued hardware
frames before replacing the decoder. A software decode/import failure remains
an error rather than starting another fallback loop.

Metal, native D3D11, and wgpu can upload software-decoded frames. Shared frame
conversion produces NV12 for 8-bit 4:2:0 and P010 for 10-bit 4:2:0; other CPU
pixel formats use swscale. D3D11 uploads luma and chroma as separate textures,
including odd-sized planes, and uses the existing color/HDR pipeline. The
native dependency plans explicitly include ASF, WMV, and WMA support. The
synthetic `testdata/software/wmv2-wmav2.asf` regression exercises all decoder
preferences, audio, seek, and replay. Windows CI additionally checks NV12 and
P010 uploads using WARP, including a byte-for-byte P010 texture readback.

Build coverage is distinct from runtime coverage: macOS and Windows CI run
the software decoding tests natively; Android, OpenHarmony, iOS and tvOS
build jobs verify compilation and linking. Playback on those devices still
requires device validation.

## Playback Engine

`PlaybackSession` opens media, selects tracks, configures decode backend, and
produces video frames and PCM audio blocks.

Decoder availability is a session invariant: when a video track is selected,
the play, seek, and video-frame-pump entry points require an active video
decoder. Destructive MediaCodec transitions, including seek reopen and
Surface-to-ByteBuffer/software fallback, first record a decoder-unavailable
reason. If the final software decoder open also fails, those entry points
return that explicit error and require the media to be reopened; they never
enter an audio-only false `Playing` state.

`VideoPlaybackEngine` adds clocked playback:

- Play, pause, stop, seek, playback rate control (SoundTouch for audio),
  EOF detection.
- `PlaybackClock` — media-time anchor with audio-master clock discipline
  (deadband correction, bounded per-frame adjustment, large-drift snap).
- `VideoFrameScheduler` — present/wait/drop decisions for decoded video frames.
- `DisplaySyncState` — vsync quantizer that carries residual frame-duration
  error across frames.

## Audio Output

Clock feedback uses `Player::capture_audio_clock` and
`update_audio_clock_observation`. An observation retains the player identity,
playback generation, command sequence, output epoch, and monotonic capture time.
The worker checks this identity against both the current intent and executed
commands, and expires feedback after 500 ms measured from capture. The same
validation protects queue and buffering decisions. The presenter invalidates
the output epoch on reset, reconfiguration, device transitions, and rate changes;
reads and output changes remain serialized by the output owner.

The engine first establishes a baseline, then requires both consumed PCM frames
and media time to advance before disciplining the shared `PlaybackClock`.
Underflow-only callbacks and frozen snapshots cannot keep pulling the clock
back. Valid large drift can snap in either direction. Delivery-time extrapolation
is limited to the PCM duration queued at capture and uses the committed rate.
Paused or first-frame-parked clocks stay parked. A running audio master remains
active during background playback and foreground video recovery.

The older `update_audio_clock(snapshot)` Rust API remains available for fresh,
synchronous feedback on the current timeline; it cannot recover the capture
identity of a previously cached snapshot. Delayed callers should use the
observation API. This does not change the C ABI or queue targets: the 250 ms rate
bridge and suppression of mixed-rate observations remain in place. Audio pumping
still runs in the presenter tick, so a blocked renderer can still cause underflow;
clock recovery does not replace independent audio delivery.

- **macOS**: CoreAudio output with ring buffer and PTS-tracking clock snapshots.
  The presenter feeds CoreAudio output snapshots back to the player worker for
  audio-master clock discipline.
- **iOS**: AudioQueue output with the same ring buffer and clock snapshot model.
- Ring buffer: interleaved f32, configurable capacity, drop-oldest overflow
  policy, volume control.

## Subtitle System

- **Parsing**: SRT, WebVTT, ASS timeline parsing. Embedded and external subtitle
  tracks. External tracks can be added/removed at runtime.
- **libass renderer**: Statically linked, enabled by default. Accepts ASS
  scripts, calls `ass_render_frame`, imports alpha planes into liberika's overlay
  system. macOS uses the CoreText font provider and Windows DirectWrite; every
  other target runs without a system font provider (the vendored libass build
  disables fontconfig), so liberika's bundled Droid Sans Fallback — registered as
  an in-memory font on all platforms — is the family libass resolves by default.
- **Subtitle style**: A custom font family and font file, colours, metrics
  (size, outline, shadow, blur, spacing, scale), attributes, border style,
  alignment and margins act as fallbacks that fill in what a script leaves
  open, plus the styling of plain-text subtitles; the subtitle scale multiplies
  the metrics. An `override_mask` promotes chosen fields to libass' selective
  style override so they also replace what ASS dialogue events request, with the
  override metrics renormalized so they land on the same pixels whatever
  `PlayResY` a script declares.
- **SubtitleRendererCore**: Renderer-facing boundary that tracks changed/unchanged
  frames to avoid redundant GPU uploads.

## Danmaku System

The danmaku subsystem implements the NipaPlay DFM+ layout algorithm natively in
Rust. See `docs/danmaku_architecture.md` for the full design.

- **Input**: Bilibili XML, JSON, JSON-lines parsing.
- **DanmakuSession**: Multi-track management with per-track enable/disable,
  per-track offset, global offset.
- **DFM+ layout core**: Prepare/frame-query separation. Prepare processes the
  entire track once (measurement, filtering, duplicate merge, collision avoidance,
  lane allocation). Frame query returns positioned items for a given media time.
- **Text rasterizer**: Glyph atlas with fill and outline alpha masks, version
  tracking for GPU texture reuse.
- **Render plan**: `DanmakuRenderPlan` carries glyph instances with screen rects,
  atlas tex rects, colors, outline, shadow. Metal and wgpu renderers draw
  instanced quads from the atlas.

## Renderer

### Metal Renderer (macOS/iOS/tvOS)

The primary renderer for Apple platforms:

- Zero-copy CVPixelBuffer → MTLTexture import via `CVMetalTextureCache`.
- YCbCr sampling, transfer decode, gamut mapping (BT.2020→BT.709, Display P3→BT.709).
- Tone mapping: libplacebo-style IPT-domain color map with BT.2390 EETF
  default (plus spline, BT.2446 method A, ST 2094-10, Mobius, Reinhard, clip)
  over absolute nits.
- SDR output (`BGRA8Unorm`) and Apple EDR output (`RGBA16Float` with EDR
  headroom).
- Luma upscaler (`LumaUpscalerMode`): network modes (ArtCNN, Anime4K, `Custom`)
  run as Metal compute on the decoded Y plane, encoded on the same command
  buffer ahead of the render pass. Chroma keeps its source resolution. The
  network output is cached per decoded frame so repeated vsync ticks of the
  same frame skip the compute. Kernels compile on a background thread, so
  playback continues unscaled until they are ready. Engine families (generic
  network engine, FSR1, MetalFX spatial, VideoToolbox) are listed under
  [Super resolution](#super-resolution).
- Subtitle overlay: RGBA plane upload and alpha blending.
- Danmaku: Instanced glyph quad drawing from atlas (shadow → outline → fill passes).
- Presentation layout preserves source aspect ratio.

### Direct3D 11 Renderer (Windows)

The native renderer for Windows (`renderer/d3d11.rs`):

- Zero-copy D3D11VA decode-texture interop: decoded `ID3D11Texture2D` surfaces
  are shared into the render device, no CPU round-trip.
- YCbCr sampling and color space conversion (HLSL shaders), same pipeline model
  as Metal.
- Neural and platform super resolution on the zero-copy D3D11VA luma plane
  through generated compute kernels, with measured backend selection, RTX/Intel
  VSR and optional Vulkan / D3D12 accelerator sidecars (see
  [Super resolution](#super-resolution)). The upscaled luma is a plain 2x
  `R16_FLOAT` texture, cached per decoded frame. A mode that cannot run reports
  an explicit `Inactive` fallback with the reason.
- HDR10 output via an `R10G10B10A2_UNORM` swapchain with `DXGI_HDR_METADATA_HDR10`,
  with SDR (`BGRA8`) fallback.
- Subtitle overlay alpha-atlas upload and blending; instanced danmaku glyph
  quad drawing from the atlas.
- Window-hosted swapchain driven by `render_tick`.

### wgpu Renderer (cross-platform)

Second renderer backend for portability:

- Real `wgpu` dependency with device/surface/pipeline creation. The workspace
  vendors a small `wgpu-hal` patch (`[patch.crates-io]` in the root
  `Cargo.toml`) that routes the OHOS NDK surface to `VK_OHOS_surface`.
- NV12/P010 video frame upload and WGSL YCbCr conversion shader.
- Android MediaCodec Surface import through AHardwareBuffer/Vulkan, with an
  explicit ByteBuffer/CPU-upload path when native interop is unavailable.
- The shared `renderer/frame.rs` boundary carries geometry/color metadata plus
  either a decoded FFmpeg frame or an independent prepared AHardwareBuffer.
  MediaCodec Surface AVFrames are released on the playback worker while their
  decoder callback context is still alive; presenter and GPU recovery never
  retain the decoder-owned AVFrame.
- Color space conversion, tone mapping (same pipeline model as Metal).
- Super resolution (`renderer/wgpu_sr.rs` with `renderer/sr/wgsl.rs`): every
  network mode (ArtCNN 1-7, Anime4K 16-25, `Custom`) plus `Fsr1`, from the same
  `SrModel` IR as D3D11 and Metal. Network output is a source-sized
  `Rgba16Float` texture holding the packed 2x2 depth-to-space quad; FSR1 writes
  a presented-size luma texture. It accepts both native luma planes and
  Android's converted nonlinear RGB texture while preserving chroma as
  `rgb + (Y_sr - Y)`. `PlatformVideo` and `MetalFxSpatial` are `Inactive`.
  GLES 3.0 reports `Inactive` with a structured `native_luma_sampling`
  fallback instead of attempting compute.
- Subtitle/danmaku compositing, frame capture, and offscreen headless tests.
  Capture always renders to an SDR RGBA8 target, including when the display
  surface is extended-linear, so screenshots never expose unclamped scRGB
  values as if they were SDR pixels.
- Surface handle model covers macOS NSView, iOS UIView, Windows HWND,
  X11/Wayland, Android native windows, OpenHarmony `OHNativeWindow`.
- OpenHarmony imports AVCodec Surface output as an `OHNativeBuffer`-backed
  Vulkan external image and resolves YUV on the GPU with a Vulkan YCbCr
  sampler, so decoded frames reach the wgpu compositor without a CPU copy.
  Devices without the required Vulkan extensions fall back to software decode
  and CPU upload, and the fallback is reported through the diagnostics events.
- Android has bounded Vulkan/GLES backend recovery and explicit import,
  capability, quality-reduction, and device-failure diagnostics. Its
  high-headroom output is FP16 **extended-linear scRGB**, not HDR10/PQ: the
  renderer uses `Rgba16Float`, Vulkan's extended-sRGB-linear color space, and
  verifies `ADATASPACE_SCRGB_LINEAR` (`0x18410000`) on the `ANativeWindow` after
  every configure/reconfigure. Android scRGB uses BT.709 primaries with
  `1.0 = 80 nit`; it does not emit PQ or HDR10 static metadata.
- Extended-linear activation requires an explicit `ExtendedLinear` request, an
  HDR-capable display/surface, a `SurfaceView` hosted with Flutter Hybrid
  Composition, the Vulkan wgpu backend, `Rgba16Float` surface support, and a
  successful `SCRGB_LINEAR` dataspace readback. A missing condition selects the
  normal SDR surface immediately and records one of the stable fallback reason
  codes `0..8`; GLES and `TextureView` are therefore SDR paths.
- On API 34+, the Android host observes the display with
  `Display.registerHdrSdrRatioChangedListener` and publishes real changes
  through `erika_presenter_set_output_headroom`. wgpu updates the effective
  content headroom used by subsequent frames and the queryable output status
  without reattaching the surface. When the ratio is available,
  `activeHeadroomKnown` is true; `headroomUpdates` grows only when the known
  flag or ratio actually changes.
- A Flutter extended-linear player with no explicit `edrHeadroom` uses a 4x
  content ceiling while passing `0` to the `SurfaceView` as system-auto desired
  headroom. An explicit value becomes the content ceiling and, on API 35, the
  per-`SurfaceView` desired headroom; liberika never changes the global window.
- Emulator/non-HDR coverage verifies the explicit SDR fallback path. Active
  `Rgba16Float + SCRGB_LINEAR` presentation still requires acceptance on an
  API 35 HDR device before it is described as device-validated.

### Super resolution

Network modes share a backend-neutral network IR in `crates/erika/src/renderer/sr/`:

- `model.rs`: the network graph (convolutions on a 1-channel luma input with
  per-input kernels, CReLU transforms, residuals, ReLU/SiLU/PReLU, and a 2x
  depth-to-space head) and its `.esr` serialization.
- `catalog.rs` maps a mode to its bundled model; `mpv.rs` imports Anime4K /
  FSRCNNX mpv hook shaders (weights only, shader code is never executed);
  `artcnn.rs` lowers the legacy ArtCNN blobs; `fsr1.rs` is FSR 1.0.
- `fold.rs` applies exact optimizations: merging sibling passes and
  algebraically folding linear layers into their neighbours (ArtCNN conv0 into
  conv1 and conv5 into conv6) with nine border-class weight sets so the image
  border stays exact. ArtCNN needs 26-33% fewer MACs.
- `reference.rs` is the CPU oracle every GPU backend is tested against.
- `hlsl.rs` is the D3D11 kernel generator; `wgsl.rs` is the wgpu one.

Direct3D 11 (`renderer/d3d11_sr.rs` and neighbours):

- `d3d11_sr.rs` is the executor: per-layer kernels with constant-buffer weights
  at literal indices, groupshared input staging, a fused epilogue/head, one ring
  dispatch for the border, tiling within a 192 MB budget, background FXC
  compilation with a disk cache, and timestamp queries for GPU time.
- `d3d11_sr_select.rs` measures scalar against the accelerators per network and
  chooses the fastest; it emits the `luma_upscaler_backend_selection` diagnostic
  event with the measured candidates.
- `d3d11_vsr.rs` drives NVIDIA RTX Video / Intel VSR through the video
  processor vendor extension.
- Sidecars: `d3d11_vk_coopmat.rs` with `vk_coopmat/` (Vulkan cooperative
  matrix) and `d3d11_d3d12_linalg.rs` with `d3d12_linalg/` (opt-in D3D12 SM 6.10
  preview).

The Windows Vulkan sidecar loads `vulkan-1.dll` by its absolute system-directory
path, independent of DLLs bundled by the host application. A missing runtime or
unsupported device uses the existing scalar D3D11 compute path. The
`luma_upscaler_vulkan_init` diagnostics identify the loader path and adapter/driver;
`luma_upscaler_backend_selection` includes unavailable-accelerator reasons alongside
the measured candidates. Acceleration remains capability-based across GPU vendors.

Metal (`renderer/metal/`): `upscaler_network.rs` is the generic network engine
(Metal 4 tensor ops on M5/A19, `simdgroup_matrix`, or scalar), with
`upscaler_fsr.rs`, `upscaler_metalfx.rs` and `upscaler_vt.rs` for FSR1, MetalFX
spatial and VideoToolbox; `upscaler.rs` handles selection and status. The
hand-written ArtCNN kernels remain selectable with `ERIKA_SR_BACKEND=legacy`.

wgpu (`renderer/wgpu_sr.rs`, `renderer/sr/wgsl.rs`): `wgsl.rs` generates WGSL
from the `SrModel` IR and is validated against `reference.rs`; `wgpu_sr.rs` is
the executor, with background compilation (`Building` status), tiling within a
scratch budget and a model cache. Generated kernels use workgroup-memory input
staging, a fused bias/residual/activation epilogue and depth-to-space head, one
perimeter dispatch for the border classes of folded layers, fp16 where the
adapter has `SHADER_F16`, and 16x16 cooperative matrices on Vulkan adapters that
advertise them (NVIDIA). Per-adapter defaults are refined by timing a few kernel
variants on the first frames (cached in-process; `ERIKA_WGPU_SR_AUTOTUNE=0`
disables). FSR1 is a fused EASU+RCAS kernel. `wgpu_artcnn.rs` (the legacy
ArtCNN executor) remains only for regression comparisons.


### Render Pipeline

`renderer::pipeline` describes rendering decisions in Rust before any backend
consumes them:

- `SourceColorState` / `TargetColorState` — primaries, transfer, range.
- `VideoRenderPipeline` — gamut matrix, tone map operator, transfer functions.
- `renderer::output` — requested mode, active encoding, surface format,
  dataspace/headroom state, and stable fallback diagnostics shared by the
  native renderers and wgpu.
- HDR metadata: mastering display, content light level, nominal peak nits.

## Presenter Runtime

`PresenterRuntime` ties together Player, MetalRenderer, OverlayTimeline,
DanmakuEngine, and audio output. The host supplies a native surface and drives
`render_tick` from a display timer.

- Pumps video frames, updates overlay (subtitle + danmaku), renders, presents.
- Decoder-changing operations use a quiesce/ACK barrier, discard renderer and
  receiver state, perform the transition, then resume. Playback generations
  remain monotonic across reopen and stale import feedback is gated by both
  generation and the exact MediaCodec route.
- Danmaku plan generation is time-synchronized with video frames using
  generation + media_time gating.
- Supports playback rate, volume, track selection, subtitle/danmaku
  configuration at runtime.

## C ABI

The canonical header is `crates/erika_capi/include/erika.h`; package copies must match it. The two `erika_presenter_windows_*_iunknown` getters are exported only on Windows, even though the shared header declares them on all platforms.

`erika_capi` exposes two handle families:

- **`ErikaHandle`** — player control and event polling. The host owns rendering.
- **`ErikaPresenterHandle`** — liberika owns the full stack. The host provides a
  surface and calls `render_tick`.

Covers: create/destroy, open/play/pause/stop/seek, track selection, subtitle
track add/remove, danmaku track management (add/remove/enable/offset/config),
surface attach/detach/resize, event polling, volume, playback rate, neural
luma upscaler switching, upscaler diagnostics, and the 13-field output status
snapshot returned by `erika_presenter_get_output_status`.

Header: `crates/erika_capi/include/erika.h`

## Flutter Plugin

`packages/erika_flutter` provides macOS, iOS, tvOS, Windows, Android, HarmonyOS, and Linux
Flutter embedding:

- **Dart**: `ErikaPlayer` (commands + events), `ErikaWindowOverlayVideoView`
  (recommended window-hosted native surface — Metal on Apple, D3D11 swapchain on
  Windows), and `ErikaVideoView` (compatibility platform view).
- **macOS Swift plugin**: Loads `liberika_capi.dylib`, creates either
  `NSWindow`-hosted overlay or `NSView`/`CAMetalLayer` platform view surfaces,
  and drives `render_tick` from a display link.
- **iOS Swift plugin**: Links `liberika_capi.a` statically, creates either
  `UIWindow`-hosted overlay or `UIView`/`CAMetalLayer` platform view surfaces,
  and uses the same presenter model.
- **tvOS Swift plugin**: Links `liberika_capi.a` statically, presents through a
  `UIView`/`CAMetalLayer` platform view on Apple TV, and uses the same presenter
  model; supports tvOS devices and arm64/x86_64 simulators.
- **Windows C++ plugin** (`ErikaFlutterPluginCApi`): builds and links
  `erika_capi.dll` via CMake (`build_erika_runtime.cmake`, cargo target
  `x86_64-pc-windows-msvc`), hosts a window-level D3D11 swapchain, and drives
  `render_tick` from a frame scheduler.
- **Android Kotlin/JNI plugin**: builds the Rust runtime for Android ABIs and
  gives each player an independent native surface. SDR uses `TextureView`;
  requested extended-linear output uses `SurfaceView` through Flutter Hybrid
  Composition. The plugin coordinates Activity surface lifecycle, audio focus,
  noisy-route policy, HDR eligibility/headroom, and drives presentation from a
  shared frame scheduler only while players are active.
- **HarmonyOS ArkTS/N-API plugin**: builds `liberika_capi.so` for
  `aarch64-unknown-linux-ohos` through Hvigor/CMake, registers a Flutter
  external texture, and attaches that texture's `OHNativeWindow` to the
  presenter. Audio goes through OHAudio as interleaved f32 PCM.

See `docs/flutter_embedding.md` for the embedding model and HDR strategy.

## Platform Support

| Platform | Decode | Render | Audio | Status |
|----------|--------|--------|-------|--------|
| macOS 11+ | VideoToolbox | Metal | CoreAudio | Available |
| iOS 13+ | VideoToolbox | Metal | AudioQueue | Available |
| tvOS 13+ (Apple TV) | VideoToolbox | Metal | AudioQueue | Available |
| Windows 10+ | D3D11VA | Direct3D 11 | WASAPI | Available |
| Linux | NVDEC / VA-API / FFmpeg | wgpu X11/Wayland | PulseAudio / PipeWire-Pulse | Experimental |
| Android 8+ | MediaCodec / software | wgpu Vulkan with GLES fallback | AAudio | Available; SDR validated, extended-linear scRGB implementation awaits API 35 HDR-device acceptance |
| HarmonyOS API 18+ | AVCodec (H.264/HEVC) / software | wgpu Vulkan, `OHNativeBuffer` zero-copy import | OHAudio | Available; validated on device, CI builds the OpenHarmony C ABI but has no device-side run verification |
