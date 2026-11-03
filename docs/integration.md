# Integrating liberika in a Native Host

> Translations: [中文](integration.zh.md) · [日本語](integration.ja.md)

This guide walks through embedding liberika in a non-Flutter host — a C/C++/Swift
app, a Win32 window, or any runtime with C FFI. It uses the **presenter** (push)
model, where liberika owns decode, timing, audio, overlays, and presentation, and
the host supplies a surface plus a per-frame `render_tick`.

Prerequisites: the C ABI ([capi_reference.md](capi_reference.md)) and a built
liberika library ([building.md](building.md)). For Flutter, use the
[`erika_flutter`](../packages/erika_flutter) plugin instead, described in
[flutter_embedding.md](flutter_embedding.md).

Two runnable references accompany this guide:
[`examples/macos_native_demo`](../examples/macos_native_demo) (AppKit +
`CAMetalLayer`) and [`examples/windows_native_demo`](../examples/windows_native_demo)
(Win32 + `HWND`). They drive the Rust `PresenterRuntime` directly; the C-ABI
calls below are the 1:1 equivalents.

## 1. Choose a handle family

Use `ErikaPresenterHandle` unless you have a reason to render yourself. The
pull-model `ErikaHandle` is for hosts that own their compositor and only want
liberika's decode/timing/state. The rest of this guide is presenter-based.

The presenter family is compiled on **macOS, iOS, tvOS, Windows, Android, and
HarmonyOS and experimental Linux**. The native host API is platform-specific; HarmonyOS Flutter hosts
normally use the plugin's ArkTS JSON bridge rather than reproducing the desktop
sample code.

## 2. The lifecycle

```
create ──▶ attach surface ──▶ open ──▶ play ──▶ (render_tick + poll_event loop)
                                                      │
                              pause / seek / set_* ◀──┤
                                                      ▼
                        detach surface ──▶ destroy
```

`open` synchronously probes the source and returns in `Ready` on success.
`play` queues playback work; observe `StateChanged` and `Error` events for its
result. HTTP probing can block, so keep all calls to one handle on a serial
worker when the UI must remain responsive. Attach the surface before playback.
`close` ends the player permanently; use `stop` to replay, or create a new handle
after closing.

## 3. Create the presenter

```c
ErikaPresenterConfig cfg = {
    .output_mode  = ErikaPresenterOutputMode_Sdr,   // AppleEdr or Android ExtendedLinear
    .edr_headroom = 1.0f,                            // requested content-headroom ceiling
    .video_alpha_mode = ErikaVideoAlphaMode_Opaque,
    .luma_upscaler = ErikaLumaUpscalerMode_Off,      // or any ErikaLumaUpscalerMode, e.g. ArtCnnC4F16
};
ErikaPresenterHandle *p = erika_presenter_create_with_config(cfg);
if (!p) { /* read erika_last_error_message() */ }
```

`erika_presenter_create()` uses defaults (SDR, no upscaler). See
[Upscaler](#upscaler) for the available modes and what runs where.

### Upscaler

`set_upscaler(mode)` selects the mode (`ErikaLumaUpscalerMode`, values grouped
by family):

| Values | Family | Content |
|---|---|---|
| 1-7 | ArtCNN: C4F16, C4F32, C4F16 DS, C4F16 DN, C4F32 DS, C4F32 DN, R8F64 | anime; DS = denoise + sharpen (degraded sources), DN = denoise; R8F64 (~926K params) is only practical on matrix units and SD/720p sources and is bundled on desktop builds only |
| 16-25 | Anime4K v3.2 Upscale CNN x2 (S, M, L, VL, UL) and Upscale+Denoise (21-25) | anime |
| 32 | `Fsr1`: AMD FSR 1.0 EASU + RCAS on luma (RCAS sharpness 0.2 stops) | any |
| 48 | `PlatformVideo`: the platform's video super resolution | live action / any |
| 49 | `MetalFxSpatial` (Apple only) | any |
| 64 | `Custom`: a network loaded at runtime | - |

Network modes upscale the decoded luma plane 2x (chroma keeps source resolution
and is reconstructed by the regular scaler). `Fsr1` and `MetalFxSpatial` scale
luma straight to the presented size; `PlatformVideo` scales the whole frame.

| Platform | What runs |
|---|---|
| Windows (D3D11) | Every network mode including `Custom` (generated compute kernels; the fastest of scalar, Vulkan cooperative matrix and D3D12 linalg is measured per network and frame size), `Fsr1`, and `PlatformVideo`: NVIDIA RTX Video Super Resolution (an RTX GPU, enabled in the NVIDIA App / Control Panel) or Intel VSR (Arc/Xe with a VSR-capable driver); SDR video only, D3D11VA-decoded NV12/P010 frames. `MetalFxSpatial` is `Inactive`. D3D12 linalg is an opt-in preview. |
| Apple (Metal: macOS, iOS, tvOS) | Every network mode including `Custom` (Metal 4 tensor ops on M5/A19, `simdgroup_matrix` on M1-M4/A14+, scalar otherwise), `Fsr1`, `MetalFxSpatial`, and `PlatformVideo` through VideoToolbox `VTLowLatencySuperResolutionScaler`: 2x only, 8-bit 4:2:0 sources between 96x96 and 960x960, macOS/iOS/tvOS 26+; otherwise `Inactive` with the reason. |
| wgpu renderers (Linux, Android, OpenHarmony, Windows wgpu fallback) | Every network mode including `Custom`, and `Fsr1` (generated WGSL kernels; fp16 where the adapter has `SHADER_F16`; 16x16 cooperative matrices on Vulkan adapters that expose them, such as NVIDIA; per-adapter defaults refined by timing a few kernel variants on the first frames). `PlatformVideo` and `MetalFxSpatial` are `Inactive`. Android/OpenHarmony hardware has not been benchmarked yet. |
| Web | No upscaler. |

A mode that cannot run reports the `Inactive` backend status and keeps native
luma sampling; the reason is in the diagnostics `upscaler` object (`reason`,
`engine`, `lastGpuMs`). On every backend the kernels compile on a background
thread: until they are ready the status is `Building` and native sampling is
used, so poll `get_upscaler_status` to drive any UI.

Measured GPU time (2x upscale of the given source; median, GPU timestamps):

| Model (1080p source) | Windows, RTX 5060 Ti (auto-selected backend) | Apple M5 Pro (Metal) | RTX 5060 Ti through wgpu/Vulkan (the Linux path) | Intel UHD 770 (D3D11) |
|---|---|---|---|---|
| ArtCNN C4F16 | 2.44 ms scalar (was 12.38 ms) | 2.65 ms tensor ops | 2.77 ms | 68 ms |
| ArtCNN C4F32 | 7.29 ms cooperative matrix (scalar 9.5 ms; was 39.4 ms) | 6.35 ms | 11.4 ms | 267 ms |
| Anime4K S / M / L / VL / UL | 0.35 / 0.78 / 0.83 / 2.07 / 4.11 ms | 1.0 / 2.8 / 2.0 / 4.96 / 7.3 ms | 0.45 / 1.04 / 1.03 / 2.85 / 5.80 ms | 8.9 / 18.7 / 22.4 / 54.5 / 173 ms |
| ArtCNN R8F64 (720p source) | 71 ms cooperative matrix (58 ms with D3D12 linalg) | 64.6 ms | 183 ms | not for playback |
| FSR1 (1080p to 4K) | 0.31 ms | 0.77 ms | 0.55 ms | 10 ms |
| MetalFX (1080p to 4K) | n/a | 0.67 ms | n/a | n/a |

The denoise/sharpen variants (ArtCNN DS/DN, Anime4K Denoise) cost the same as
their base models. R8F64 is not real time on any of these GPUs.

Windows accelerators, RTX 5060 Ti (1080p source unless noted; the selector
measures the candidates on real frames and keeps the fastest, so these only
decide the wide models):

| Model | D3D11 scalar | Vulkan cooperative matrix | D3D12 linalg (opt-in) |
|---|---|---|---|
| ArtCNN C4F16 | 2.44 ms | 3.59 ms | 3.14 ms |
| ArtCNN C4F32 | 9.5 ms | 7.29 ms | 6.53 ms |
| ArtCNN C4F32, 4K source | 39.5 ms | 27.7 ms | - |
| Anime4K UL | 4.11 ms | 7.39 ms | 20.2 ms |
| ArtCNN R8F64, 720p source | 243 ms | 71.4 ms | 58.1 ms |

Cooperative-matrix times use the default fp32 accumulation;
`ERIKA_VK_FP16_ACCUMULATION=1` gives 6.24 ms for C4F32 and 58.7 ms for R8F64
at a small precision cost.

The Vulkan spatial head for the bundled C4F16-DN/C4F32-DN graphs now uses packed
fp16 coefficients, shared inputs and accumulation on NVIDIA. It reduces the
1080p full GPU handoff interval by approximately 7–8% in the paired measurements,
with either matrix accumulation mode. Set `ERIKA_VK_FP16_HEAD=0` to retain the
fp32 head. [Implementation and measurements](investigations/sr-gpu-head-2026-11-03/README.md).

**Custom models.** `erika_presenter_load_upscaler_model(handle, name, data, len)`
loads an Erika `.esr` network (export ArtCNN ONNX with
`crates/erika/assets/artcnn/export_artcnn_esr.py`; for example R16F96, which is
not bundled) or an mpv hook shader of the FSRCNNX or Anime4K CNN dialect, then
select it with `set_upscaler(handle, ErikaLumaUpscalerMode_Custom)`;
`erika_presenter_clear_upscaler_model` removes it. Only weights are parsed;
shader code is never executed. FSRCNNX (GPL-3.0/LGPL-3.0) and RAVU/NNEDI3
(LGPL-3.0) are not bundled because liberika is MPL-2.0, so the host supplies
the file. Through the presenter JSON bridge use `loadUpscalerModelFile
{path, name?}` and `clearUpscalerModel`.

**Windows tuning (diagnostics only).**

| Variable | Effect |
|---|---|
| `ERIKA_SR_BACKEND` | `scalar`, `coopmat` or `linalg` pins the D3D11-side executor (default: measured auto-selection). Apple: `scalar`, `matmul`, `tensor`, `legacy`, `legacy-scalar`. |
| `ERIKA_SR_NO_SHADER_CACHE=1` | Disable the D3D11 kernel disk cache (`%LOCALAPPDATA%\liberika\sr-shader-cache`). |
| `ERIKA_SR_TILE_BUDGET_MB` | D3D11 intermediate tensor budget (default: chosen per network, 24-192). |
| `ERIKA_VK_FP16_ACCUMULATION=1` | fp16 accumulation in the Vulkan cooperative-matrix backend. |
| `ERIKA_WGPU_SR_AUTOTUNE=0` | wgpu (all platforms): keep the per-adapter kernel defaults instead of timing variants on the first frames. Other `ERIKA_WGPU_SR_*` variables pin individual kernel choices; see `crates/erika/src/renderer/wgpu_sr.rs`. |
| `ERIKA_SR_D3D12_LINALG=1` with `ERIKA_D3D12_AGILITY_SDK_PATH`, `ERIKA_D3D12_DXCOMPILER_PATH`, `ERIKA_D3D12_DXC_INCLUDE_PATH` | Opt in to the D3D12 SM 6.10 preview backend (Agility SDK 1.722-preview and the SM 6.10 preview DXC; Windows Developer Mode for experimental shader models; retail expected with Agility SDK 1.621, Dec 2026). See `crates/erika/src/renderer/d3d12_linalg/README.md`. |

### Subtitle and danmaku quality defaults

The native presenter enables subtitle and danmaku supersampling by default,
matching the Web backend. Text subtitles and danmaku glyphs rasterize at 2x
resolution while their displayed size, layout and video resolution stay the same.
Rust hosts can disable them independently with
`PresenterRuntime::set_subtitle_supersampling(false)` and
`DanmakuLayoutConfig { supersampling: false, ..Default::default() }`, applying
the latter through `PresenterRuntime::set_danmaku_config`.

Native subtitles retain the existing performance limits: source canvases with
either dimension above 2048 pixels or area above 2048 × 1080 pixels stay at
native resolution. An average subtitle render cost above 4 ms over 60 renders
falls back to native resolution for the current media. Danmaku glyphs larger
than 128 px also retain native resolution.

## 4. Attach a surface

liberika renders directly into a platform surface you own. Width/height are
**physical pixels**; `scale` is the DPI/backing factor.

### macOS / iOS / tvOS — `CAMetalLayer`

Create a `CAMetalLayer`, size it, then hand its pointer to liberika:

```c
// `layer` is a CAMetalLayer* (e.g. from your NSView/UIView host layer)
erika_presenter_attach_metal_layer(p, (uint64_t)(uintptr_t)layer,
                                   pixel_w, pixel_h, backing_scale);
```

On macOS the recommended arrangement is a window-hosted layer that is a sibling
/ underlay of your content view, so video stays outside the AppKit view
compositor (the same model the Flutter plugin uses — see
[flutter_embedding.md](flutter_embedding.md)).

### Windows — `HWND`

```c
HWND hwnd = /* your window */;
HINSTANCE hinst = GetModuleHandleW(NULL);
UINT dpi = GetDpiForWindow(hwnd);
double scale = dpi ? (double)dpi / 96.0 : 1.0;
RECT rc; GetClientRect(hwnd, &rc);
uint32_t w = max(1, rc.right - rc.left), h = max(1, rc.bottom - rc.top);

erika_presenter_attach_windows_hwnd(p, (uint64_t)(uintptr_t)hwnd,
                                    (uint64_t)(uintptr_t)hinst, w, h, scale);
```

`attach_windows_hwnd` is a convenience wrapper over `attach_wgpu_surface` with
kind `WindowsHwnd`. With the default presenter config the surface drives the
**native Direct3D 11** renderer (D3D11VA zero-copy, HDR10). The C ABI
`ErikaPresenterConfig` does not expose renderer selection; Rust hosts can select
`WgpuFallback` via `PlayerConfig.renderer`.

### Generic — `attach_wgpu_surface`

For X11/Wayland/Android or to be explicit about the surface kind, use
`erika_presenter_attach_wgpu_surface(p, kind, raw_window, raw_display, w, h, scale)`
with the matching `ErikaWgpuSurfaceKind` and platform handles.

### Linux X11 / Wayland

`XlibWindow` takes an X11 Window ID and Display pointer; `WaylandSurface` takes
`wl_surface*` and `wl_display*`. Both are borrowed until detach completes.
Build dependencies and examples are in [Linux integration](linux.md).

### Android extended-linear scRGB

Android `ExtendedLinear` is FP16 extended-linear scRGB, not HDR10/PQ. A native
host must provide an `ANativeWindow` from a `SurfaceView` that is directly
composited (the Flutter plugin uses Hybrid Composition) and attach it with the
probed output capabilities:

```c
ErikaSurfaceOutputCapabilities caps = {
    .extended_linear = display_and_surface_are_hdr_capable,
    .direct_composition = true,       // SurfaceView, not TextureView
    .desired_headroom = requested_headroom, // 0 = system auto
    .fallback_reason = host_probe_reason, // 0 when eligible
};
erika_presenter_attach_wgpu_surface_with_output_capabilities(
    p, ErikaWgpuSurfaceKind_AndroidNativeWindow,
    (uint64_t)(uintptr_t)native_window, 0, w, h, scale, caps);
```

The renderer activates extended-linear only when the request, display/surface
eligibility, direct composition, Vulkan backend, `Rgba16Float` support, and
post-configure `ADATASPACE_SCRGB_LINEAR` readback all succeed. Any failed
condition keeps playback on SDR and records a stable `fallback_reason` code
`0..8`; GLES and `TextureView` are always SDR paths.

`ErikaPresenterConfig.edr_headroom` is the content ceiling. A positive
`desired_headroom` is an optional surface ceiling; `0` leaves the surface in
system-auto mode. The effective wgpu target follows those ceilings and the
current display HDR/SDR ratio when that ratio is known. On API 34+, a native
host should observe `Display.registerHdrSdrRatioChangedListener` and publish
each real state change with
`erika_presenter_set_output_headroom(p, ratio, true)`. Publish
`(1.0f, false)` when the ratio becomes unavailable or the view detaches. The
Flutter plugin performs this wiring automatically. On API 35 it also calls
`SurfaceView.setDesiredHdrHeadroom` per view; it never changes the global
window.

After attach and after every resize/recovery, query
`erika_presenter_get_output_status`. Active Android scRGB reports
`AndroidExtendedLinearScRgb`, `SixteenBitFloat`, native dataspace `406913024`,
and `extended_linear_active = true`. When Android supplies a display ratio,
`active_headroom` contains that ratio and `active_headroom_known` is true;
otherwise the field is only a fallback value and `active_headroom_known` is
false. `headroom_updates` increments only for a real known-state or ratio
change. A requested mode is not evidence that the mode is active. Current
non-HDR/emulator testing covers the SDR fallback path; API 35 HDR-device
acceptance remains required for the active path.

## 5. Open and play

```c
if (erika_presenter_open(p, "/path/to/video.mkv") != ErikaStatus_Ok) { /* log */ }
erika_presenter_play(p);
```

`uri` is a local path or HTTP(S) URL. Set cache budgets with open options:

```c
ErikaOpenOptions options = {0};
options.http_read_ahead_bytes = 8 * 1024 * 1024;
options.http_back_buffer_bytes = 128 * 1024 * 1024;
erika_presenter_open_with_options(p, "https://example.com/video.mp4", &options);
```

Read-ahead defaults to 2 MiB, rewind to 16 MiB. Zero selects defaults;
`ERIKA_HTTP_READAHEAD_BYTES` can set the default read-ahead window. Budgets are
bytes; size the rewind cache for the media bitrate and desired rewind time.

HTTP(S) origins may ignore Range and return `200` with the full resource,
including responses without Content-Length. liberika discards the prefix before
the requested offset and buffers only the requested bytes; a complete response
can establish the resource length at EOF. Seeking or reconnecting to such an
origin downloads the prefix again, so servers supporting `206` remain faster
for large files. A changed validator during a retry is rejected to avoid
combining bytes from different file versions. This failure also blocks
synchronous fallback and later reads on the same source. Weak ETags permit
initial reads but cannot authorize resuming a partial response.

## 6. The render loop

Drive `render_tick` from the surface's display timer — `CADisplayLink`
(iOS/tvOS) / `CVDisplayLink` or `CADisplayLink` (macOS) / a frame scheduler on
Windows / `Choreographer` on Android. Pass the frame's **presentation time in
seconds** from a monotonic host clock; liberika uses it for vsync-quantized
scheduling, so pass an absolute timestamp, not a delta.

```c
// Once per display frame:
ErikaPresenterStats stats;
erika_presenter_render_tick(p, host_time_seconds, &stats);   // out_stats may be NULL

// Drain events the same iteration:
ErikaEvent ev;
while (erika_presenter_poll_event(p, &ev) == ErikaStatus_Ok) {
    handle_event(&ev);
}
```

On any drawable-size or scale change (window resize, monitor DPI change, device
rotation), call `erika_presenter_resize_surface(p, w, h, scale)` **before** the
next tick. The Windows demo polls `GetClientRect` + `GetDpiForWindow` each frame
and resizes when they change.

`render_tick` returns quickly; it does not block on vsync itself — your display
timer provides the cadence. If you are not on a display callback (e.g. a smoke
test), a `~16 ms` sleep per iteration approximates 60 Hz.

### Display-target sampling

Since 0.2.1, `erika_presenter_render_tick_with_timing` can sample video,
subtitles and danmaku at the display target. The signed delay is seconds from
call entry to the target, within ±0.25 seconds; NULL keeps legacy sampling.

```c
double delay = display_target_seconds - monotonic_now_seconds;
erika_presenter_render_tick_with_timing(p, display_target_seconds, &delay, &stats);
```

The iOS Flutter plugin passes `CADisplayLink.targetTimestamp` through its tick mailbox.

## 7. Handle events

`poll_event` is non-blocking and returns `NoEvent` when the queue is empty.
Dispatch on `ev.kind`:

| Kind | Meaning | Read |
|------|---------|------|
| `StateChanged` | Playback state moved | `ev.state` |
| `DurationChanged` | Duration known/updated | `ev.duration_micros` |
| `PositionChanged` | Periodic position tick | `ev.position_micros` |
| `TracksChanged` | Track list changed | re-query `erika_presenter_tracks` |
| `TrackSelectionChanged` | Selection changed | `erika_presenter_track_selection` |
| `BufferingChanged` | Buffering toggled | `ev.buffering` |
| `VideoParamsChanged` | Resolution / color metadata | `ev.video` |
| `Error` | A failure occurred | `ev.status` + `erika_last_error_message` |

## 8. Runtime control

All of these are safe to call live, between ticks:

- **Transport:** `play` / `pause` / `stop` / `seek(position_micros)` /
  `set_playback_rate(rate)`.
- **Audio:** `set_volume(0.0–1.0)`.
- **Tracks:** `erika_presenter_tracks` (counted-array idiom),
  `select_audio_track` / `select_subtitle_track` (id `-1` disables subtitles),
  `add_external_subtitle`, `remove_subtitle_track`, `set_subtitle_scale`.
- **Subtitle style:** `set_subtitle_font(family, file_path)` and
  `set_subtitle_style(ErikaSubtitleStyle)` (font, colours, metrics, attributes,
  border, alignment and margins). Both are fallbacks — an ASS script keeps its
  own styling unless the matching `override_mask` bit is set — and
  `set_subtitle_scale` still multiplies the metrics. See
  [capi_reference.md](capi_reference.md#playback-and-runtime-parameters).
- **Danmaku:** load a track (`load_danmaku_file` / `_json` or the multi-track
  `add_danmaku_track_*`), toggle (`set_danmaku_enabled`), tune via
  `set_danmaku_config`, offset tracks, set the font, set blocked words. See
  [danmaku_architecture.md](danmaku_architecture.md).
- **Upscaler:** `set_upscaler(mode)`, `load_upscaler_model` / `clear_upscaler_model` for `Custom`; inspect with `get_upscaler_status`. See [Upscaler](#upscaler).
- **Output:** inspect the actual mode and fallback counters with
  `erika_presenter_get_output_status`; Android native hosts publish dynamic
  display ratios with `erika_presenter_set_output_headroom`. `capture_frame_rgba`
  always produces an SDR RGBA8 screenshot of video + subtitle (danmaku excluded), even
  when the display output is extended-linear.
- **Off-screen host composition:** Rust
  `PresenterRuntime::capture_frame_rgba_with_danmaku(w, h)` or C
  `erika_presenter_capture_frame_rgba_with_danmaku` includes enabled danmaku
  without attaching a surface, for hosts that upload RGBA8 to their own UI
  texture. See [capture contracts and examples](capi_reference.md#diagnostics-and-capture).
  For native GPU handles without readback, use `export_frame_texture`; its
  frame loop also drives audio. See [native texture output](external_textures.md).

## 9. Teardown

```c
erika_presenter_detach_surface(p);   // stop drawing into the surface first
erika_presenter_destroy(p);          // stops playback, releases everything
```

Detach before you tear down the window/layer so liberika stops touching the
surface. `destroy` is safe on a `NULL` handle.

## 10. Threading model

A handle is **not internally synchronized**. The simplest correct design: own
the presenter on one thread — the one running the display timer — and make all
calls (`render_tick`, transport, track changes) from there. If you must call
from another thread (e.g. a UI thread issuing `seek`), serialize with your own
lock so two calls never overlap on the same handle. Error messages are
thread-local, so read `erika_last_error_message` on the thread that made the
failing call.

## Per-language notes

### C / C++

Include `erika.h`, link the library (see [building.md](building.md)), and you
are done — the ABI is plain C. In C++ wrap the handle in an RAII type that calls
`erika_presenter_destroy` in its destructor, and free returned strings /
`ErikaTrackInfo` records with the matching liberika free functions, never `delete`.

### Swift

Import the C ABI through a bridging header or a module map over `erika.h`. Cast
the `CAMetalLayer` with `unsafeBitCast(layer, to: UInt64.self)` or
`UInt64(UInt(bitPattern: ...))`. Drive `erika_presenter_render_tick` from a
`CADisplayLink`/`CVDisplayLink` callback. This is what the macOS/iOS/tvOS Flutter
Swift plugins do over the same C ABI.

### Dart FFI

Bind the symbols with `dart:ffi` (`DynamicLibrary.open` for the dylib/dll, or
process symbols for a static link). Keep all FFI calls on one isolate; marshal
strings with `toNativeUtf8`/`free`. The high-level `erika_flutter` package
already does this — prefer it unless you are building a custom embedder.

## Checklist

- [ ] Create the presenter (with the right output mode for your display).
- [ ] Attach the surface with **physical-pixel** size and the correct scale.
- [ ] For Android extended-linear, use a Hybrid-Composition `SurfaceView`, pass
  probed output capabilities, and verify `Rgba16Float + SCRGB_LINEAR` through
  `erika_presenter_get_output_status`; otherwise accept and log the SDR reason.
- [ ] On API 34+, publish display HDR/SDR ratio changes through
  `erika_presenter_set_output_headroom`; on API 35 keep desired headroom scoped
  to the individual `SurfaceView`.
- [ ] Open successfully, then play; run HTTP probes on a serial worker.
- [ ] `render_tick(absolute_time_seconds)` every display frame; drain events.
- [ ] `resize_surface` on every size/scale change.
- [ ] One thread per handle, or serialize calls.
- [ ] Free every returned string / `ErikaTrackInfo`; `detach` then `destroy`.
