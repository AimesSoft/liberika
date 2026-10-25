# liberika C ABI Reference

> Translations: [中文](capi_reference.zh.md) · [日本語](capi_reference.ja.md)

This document describes the stable C ABI exported by `erika_capi`, declared in
[`crates/erika_capi/include/erika.h`](../crates/erika_capi/include/erika.h). The
ABI is the single integration surface for every non-Rust host (C, C++, Swift,
Dart FFI, Win32, …). Rust embedders should use the `erika` crate directly; this
layer exists for FFI.

For the embedding walkthrough (surface attach, the render loop, teardown) see
[integration.md](integration.md). For the high-level engine design see
[architecture.md](architecture.md).

## Two handle families

liberika exposes two independent entry points. Pick one per integration.

| Handle | Model | Who renders | Use when |
|--------|-------|-------------|----------|
| `ErikaHandle` | **Pull** | The host | You own the render loop and pull decoded frames / drive your own compositor. |
| `ErikaPresenterHandle` | **Push** | liberika | You give liberika a native surface and call `render_tick` once per display frame. liberika owns decode, timing, audio, overlays, and presentation. |

`ErikaPresenterHandle` is the recommended path and what the Flutter plugin and
the native demos use. It is compiled on **macOS, iOS, tvOS, Windows, Android,
Linux, and HarmonyOS**. Surface attachment is platform-specific: Apple uses Metal,
Windows uses HWND/D3D11, Android/HarmonyOS use wgpu/Vulkan surfaces, and Linux
supports X11/Wayland with wgpu. The
HarmonyOS Flutter bridge uses the JSON presenter helpers because ArkTS platform
channels already exchange structured serialized values. On unsupported targets
`erika_presenter_create` may be exported but returns `NULL`; guard presenter
usage by platform and by a successful create call.

The two families do not share state; a process may use both, but a given media
session lives in exactly one handle.

### Linux Flutter frame readback

After `erika_presenter_render_tick`, the Linux texture bridge calls
`erika_presenter_copy_flutter_frame_rgba(handle, pixels, capacity, &width, &height)`
to consume the latest composited frame, including subtitles, danmaku and the
debug HUD. `pixels` is caller-owned memory, `capacity` is its size in bytes,
and success writes tightly packed RGBA8 pixels and physical dimensions.
Allocate at least the configured texture width × height × 4 bytes. The call
returns `ErikaStatus_NoEvent` when no frame is pending. An undersized buffer
returns an error and consumes that pending frame; wait for the next render tick
before retrying. No engine-owned pointer escapes, and no free helper is needed.
Serialize calls with other operations on the presenter. This path copies pixels
through CPU memory; it does not provide DMA-BUF or other zero-copy import.

## Headless GIF export

`erika_export_gif` is independent of both handle families. It constructs a
private demuxer, software decoder, scaler, GIF encoder, and muxer, so it does
not require a window or surface and does not disturb an active player.

```c
ErikaGifExportOptions options = {
    .input_uri = "/path/input.mp4",
    .output_path = "/path/output.gif",
    .start_millis = 5000,
    .end_millis = 10000,
    .frames_per_second = 10,
    .output_width = 640,
    .output_height = 360,
    .quality = ErikaGifExportQuality_High,
    .loop_count = 0,
};
ErikaGifExportResult result = {0};
ErikaStatus status = erika_export_gif(&options, &result);
```

The call is synchronous; Flutter and other UI hosts must run it on a worker
thread or isolate. `start_millis` and `end_millis` select the interval;
`output_width` and `output_height` are the exact output dimensions. Normal
quality uses bilinear scaling, while high quality uses Lanczos scaling. A
temporary sibling file is renamed into place only after successful completion.
Unless `overwrite` is true, an existing destination is rejected. `loop_count`
uses FFmpeg GIF semantics: `-1` disables looping, `0` loops forever, and a
positive value is the loop count. HTTP inputs accept the same headers and
read-ahead override as `ErikaOpenOptions`.

## Headless frame export

`erika_export_frame` is independent of both handle families too. It opens a
private demuxer and a multi-threaded software decoder, seeks to the keyframe
before `position_millis`, decodes forward and returns the picture shown at that
media time: the last frame at or before it (the first frame when the position
precedes it).

```c
ErikaFrameExportOptions options = {
    .input_uri = "/path/input.mkv",
    .position_millis = 60000,
    .max_width = 640,
    .max_height = 640,
    .format = ErikaFrameExportFormat_Png,
};
ErikaFrameExportResult result = {0};
if (erika_export_frame(&options, &result) == ErikaStatus_Ok) {
    /* result.data holds result.data_len bytes of a PNG file. */
    erika_frame_export_result_free(&result);
}
```

The call is synchronous; UI hosts must run it on a worker thread or isolate.
`max_width` and `max_height` bound the picture (0 leaves a side unbounded): the
display aspect ratio is kept and the picture is never larger than the video, so
two zeros return the video's own size. `ErikaFrameExportFormat_Rgba` returns
rows of exactly `width * 4` bytes with alpha 255; `ErikaFrameExportFormat_Png`
returns an 8-bit RGB PNG file. Colour conversion follows the frame's colour
space and range; an unlabelled YUV source is taken as BT.709 when it is HD or
larger and BT.601 otherwise. HDR sources (PQ, HLG, Dolby Vision) are tone
mapped to SDR with the arithmetic Erika's renderer uses for an SDR display
(ITU-R BT.2390 EETF against a 203-nit target, BT.2020 to BT.709 gamut mapping,
gamma 2.2), and `ERIKA_FRAME_EXPORT_TONE_MAPPED` is set in
`result.reserved_flags`; SDR sources keep the plain colour conversion.
The result reports where the picture is (`timestamp_micros`), the decoded video
size and the media duration (0 when unknown). A position at or past a known
duration fails with `ErikaStatus_PlayerError`. HTTP inputs accept the same
headers and read-ahead override as `ErikaOpenOptions`. `data` stays owned by
liberika until `erika_frame_export_result_free`, which clears the result and
may be called again.

### Taking many pictures

`erika_export_frame` opens the media and seeks for every picture. For several
pictures of one media, open an `ErikaFrameExportSession` once and ask it for
each picture; the demuxer and decoder stay open.

```c
ErikaFrameExportSessionOptions open = { .input_uri = "/path/input.mkv" };
ErikaFrameExportSession *session = NULL;
if (erika_frame_export_session_open(&open, &session) == ErikaStatus_Ok) {
    for (uint64_t ms = 60000; ms < 66000; ms += 100) {
        ErikaFrameExportRequest request = {
            .position_millis = ms, .max_width = 640, .max_height = 640,
            .format = ErikaFrameExportFormat_Png,
        };
        ErikaFrameExportResult result = {0};
        if (erika_frame_export_session_frame(session, &request, &result) == ErikaStatus_Ok) {
            erika_frame_export_result_free(&result);
        }
    }
    erika_frame_export_session_close(session);
}
```

A request after the previous one continues decoding from where the decoder
stopped when no keyframe lies between that point and the request (the demuxer's
seek index tells; without an index, when the request is within two seconds or
half the longest keyframe interval seen), and seeks to the keyframe before it
otherwise. Requests may come in any order; ascending order is the cheapest.
Results, sizing, formats and tone mapping are those of `erika_export_frame`.
A failed request leaves the session usable. Calls on one session are
serialised; all calls are synchronous. `erika_frame_export_session_close`
accepts `NULL`; no call on the session may be running or follow it.

## Conventions

### Status codes

Every fallible call returns `ErikaStatus`:

| Value | Code | Meaning |
|-------|------|---------|
| `ErikaStatus_Ok` | 0 | Success. The thread-local error is cleared. |
| `ErikaStatus_NullPointer` | 1 | A required handle or out-pointer was `NULL` (or a surface pointer was 0). |
| `ErikaStatus_InvalidUtf8` | 2 | A `const char*` argument was not valid UTF-8. |
| `ErikaStatus_PlayerError` | 3 | The engine rejected the call; read the message (see below). |
| `ErikaStatus_Panic` | 4 | A Rust panic was caught at the boundary. The call had no effect beyond what completed before the panic; the handle should be considered suspect. |
| `ErikaStatus_NoEvent` | 5 | `*_poll_event` only: the queue is empty (not an error). |

Always check the return value. `Ok` and `NoEvent` are the only non-error
results.

### Panic safety

The ABI never unwinds across the FFI boundary. Every entry point wraps its body
in `catch_unwind`; a panic becomes `ErikaStatus_Panic` with an error message
set. You can call across the boundary without C++ `noexcept`/SEH concerns.

### Error messages (thread-local)

On any non-`Ok`/`NoEvent` result, liberika stores a human-readable message in a
**thread-local** slot. Retrieve it with:

```c
char *msg = erika_last_error_message();   // heap-allocated, may be NULL
if (msg) { fprintf(stderr, "erika: %s\n", msg); erika_string_free(msg); }
```

Because the slot is thread-local, read it **on the same thread** that made the
failing call, before making another call on that thread (a subsequent `Ok`
clears it). `erika_last_error_message` returns a copy you own — free it with
`erika_string_free`.

### String ownership

Any `char*` liberika hands back is heap-allocated and owned by the caller:

- Standalone strings (e.g. `erika_last_error_message`) → free with
  `erika_string_free`.
- Strings embedded in `ErikaTrackInfo` → free the whole record with
  `erika_track_info_free(&track)` (frees every inner string).
- Strings embedded in `ErikaDanmakuTrackInfo` → free with
  `erika_danmaku_track_info_free(&track)`.

Never `free()` these with libc; always use the matching liberika free function so
allocation crosses the ABI on the same allocator.

`const char*` arguments you pass in are borrowed for the duration of the call
only; liberika copies what it needs. They must be NUL-terminated UTF-8.

### Counted-array idiom

List getters (`erika_tracks`, `erika_presenter_tracks`,
`erika_presenter_danmaku_tracks`) use a caller-allocated buffer:

```c
size_t total = 0;
erika_presenter_tracks(p, NULL, 0, &total);          // 1) query count
ErikaTrackInfo *buf = calloc(total, sizeof *buf);
erika_presenter_tracks(p, buf, total, &total);        // 2) fill
for (size_t i = 0; i < total; i++) { /* use buf[i] */ }
for (size_t i = 0; i < total; i++) erika_track_info_free(&buf[i]);
free(buf);
```

`out_len` is **always** set to the total number of available records. At most
`capacity` records are written; passing `capacity == 0` (with a `NULL` buffer)
is the supported way to size. Only the records actually written own strings that
must be freed.

### Surface geometry and scale

`attach_*` and `resize_surface` take `width`, `height` in **physical pixels**
and a `scale` (backing/DPI factor, e.g. `2.0` on Retina, the monitor scale on
Windows). A surface pointer of `0` is rejected with `NullPointer`.

### Threading

A single handle is **not internally synchronized**. Do not call into the same
handle concurrently from multiple threads; serialize calls yourself (or confine
a handle to one thread). The presenter's `render_tick` should be driven from the
thread that owns the display timer / surface. Distinct handles on distinct
threads are independent. Remember error messages are thread-local.

## `ErikaHandle` — pull model

The host drives its own rendering and pulls state/events.

### Lifecycle

```c
ErikaHandle *erika_create(void);
void         erika_destroy(ErikaHandle *handle);
char        *erika_last_error_message(void);   // thread-local, caller frees
void         erika_string_free(char *value);
```

`erika_create` never fails (returns a valid handle). `erika_destroy(NULL)` is a
no-op. Destroying a handle stops playback and releases all resources.

### Playback control

```c
ErikaStatus erika_open(ErikaHandle *handle, const char *uri);   // file path or URL
ErikaStatus erika_open_with_headers(ErikaHandle *handle, const char *uri,
                                    const ErikaHttpHeader *headers, uintptr_t header_count);
ErikaStatus erika_open_with_options(ErikaHandle *handle, const char *uri,
                                    const ErikaOpenOptions *options);
ErikaStatus erika_play(ErikaHandle *handle);
ErikaStatus erika_pause(ErikaHandle *handle);
ErikaStatus erika_stop(ErikaHandle *handle);
ErikaStatus erika_close(ErikaHandle *handle);
ErikaStatus erika_seek(ErikaHandle *handle, uint64_t position_micros);
```

`uri` is a local filesystem path or an HTTP(S) URL. `erika_open_with_headers`
sets headers for HTTP(S) playback; `headers` is read only during the call and
may be released after it returns. When `header_count` is nonzero, `headers`
must not be `NULL`. Headers are used for HEAD, Range GET, and prefetch requests.
Authentication information and cookies are not written to liberika logs. `seek`
takes microseconds.

`erika_open_with_options` supersedes `erika_open_with_headers`: it takes an
`ErikaOpenOptions` struct that bundles the header array with per-request
tuning. A `NULL` `options` pointer means defaults. `http_read_ahead_bytes`
overrides the HTTP(S) read-ahead window in bytes for this request. `0` uses the
process-wide `ERIKA_HTTP_READAHEAD_BYTES` override when it is set, otherwise
the 2 MiB default; an explicit non-zero value supersedes the environment.
`http_back_buffer_bytes` overrides the HTTP(S) rewind budget in bytes — how
much already-played data stays cached so a rewind inside it is served without
a network request. `0` uses the 16 MiB default; hosts playing high-bitrate
media should size it from the bitrate (~15 s × bitrate, e.g. a -10 s skip at
71 Mbps covers ~89 MB).
Non-zero values in `reserved` are rejected so future fields can be added
without silently changing behavior for older hosts. Both knobs only affect
HTTP(S) playback; local files ignore them.

```c
typedef struct ErikaOpenOptions {
  const ErikaHttpHeader *headers;
  uintptr_t header_count;
  uint64_t http_read_ahead_bytes;   /* 0 = environment override, then 2 MiB */
  uint64_t http_back_buffer_bytes;  /* 0 = 16 MiB rewind budget */
  uint64_t reserved[2];             /* must be zero */
} ErikaOpenOptions;
```

`open` synchronously probes streams and transitions to `Ready`; `play` enqueues
work asynchronously. Run blocking opens off the host UI thread and serialize
all calls on the same handle. Observe `StateChanged`, `DurationChanged`, and
`Error` events for subsequent playback changes.

### Tracks and subtitles

```c
ErikaStatus erika_add_external_subtitle(ErikaHandle *, const char *uri, int64_t *out_track_id);
ErikaStatus erika_remove_subtitle_track(ErikaHandle *, int64_t track_id);
ErikaStatus erika_select_audio_track(ErikaHandle *, int64_t track_id);
ErikaStatus erika_select_subtitle_track(ErikaHandle *, int64_t track_id);
ErikaStatus erika_track_selection(ErikaHandle *, ErikaTrackSelection *out_selection);
ErikaStatus erika_tracks(ErikaHandle *, ErikaTrackInfo *out_tracks, uintptr_t capacity, uintptr_t *out_len);
void        erika_track_info_free(ErikaTrackInfo *track);
```

`erika_tracks` follows the counted-array idiom. `erika_track_selection` reports
the currently selected video/audio/subtitle track ids (`-1` for none). Selecting
the subtitle track id `-1` disables subtitles.

`ErikaTrackInfo` exposes track metadata through `codec`, `width`, `height`,
`pixel_format`, `profile`, and `level`. `bit_rate` is in bit/s, while
`frame_rate_numerator` / `frame_rate_denominator` retain a video track's rational
frame rate; `0` means the corresponding value is unknown. Frame rate is probed
in average-frame-rate, `r_frame_rate`, then FFmpeg-guessed-frame-rate order.
Bitrate prefers the track's own parameters; only a single video track with no
bitrate and known container total plus every other audio-track bitrate is
estimated as container bitrate minus audio bitrates. Neither value is an
instantaneous bitrate or rendered FPS; an estimated bitrate can include
container overhead or non-audio streams.

### State and events

```c
ErikaStatus erika_state(ErikaHandle *, ErikaState *out_state);
ErikaStatus erika_poll_event(ErikaHandle *, ErikaEvent *out_event);
```

`erika_poll_event` is non-blocking: it returns `NoEvent` when the queue is
empty. Drain it in your loop. See [Events](#events).

### Surface attach (host-managed)

```c
ErikaStatus erika_attach_metal_layer(ErikaHandle *, uint64_t raw_layer, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_attach_wgpu_surface(ErikaHandle *, ErikaWgpuSurfaceKind kind,
                                      uint64_t raw_window, uint64_t raw_display,
                                      uint32_t w, uint32_t h, double scale);
ErikaStatus erika_attach_wgpu_surface_with_output_capabilities(
                                      ErikaHandle *, ErikaWgpuSurfaceKind kind,
                                      uint64_t raw_window, uint64_t raw_display,
                                      uint32_t w, uint32_t h, double scale,
                                      ErikaSurfaceOutputCapabilities capabilities);
ErikaStatus erika_attach_flutter_texture(ErikaHandle *, ErikaFlutterTextureKind kind,
                                         int64_t texture_id, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_detach_surface(ErikaHandle *);
```

`raw_layer` is a `CAMetalLayer*` cast to `uint64_t`. For `erika_attach_wgpu_surface`,
`raw_window`/`raw_display` are the platform window/display handles for the given
`kind` (e.g. `HWND` + `HINSTANCE` for `WindowsHwnd`, `xcb`/Xlib window + display
for `XlibWindow`). `erika_attach_flutter_texture` registers an external texture
id with a platform texture registrar. The `_with_output_capabilities` variant
is required when an Android native host wants to declare a directly composited,
HDR-eligible `SurfaceView`; the shorter function supplies all-false/default
capabilities and therefore cannot activate Android extended-linear output.

## `ErikaPresenterHandle` — push model

liberika owns the full stack; the host supplies a surface and calls `render_tick`.
**macOS / iOS / tvOS / Windows / Android / HarmonyOS.**

### Lifecycle and configuration

```c
ErikaPresenterHandle *erika_presenter_create(void);
ErikaPresenterHandle *erika_presenter_create_with_config(ErikaPresenterConfig config);
ErikaPresenterHandle *erika_presenter_create_with_output_mode(int32_t output_mode, float edr_headroom);
ErikaPresenterHandle *erika_presenter_create_with_output_mode_and_alpha(int32_t output_mode, float edr_headroom,
                                                                        int32_t video_alpha_mode);
void                  erika_presenter_destroy(ErikaPresenterHandle *handle);
```

Since v0.1.8, `ErikaPresenterConfig` has four fields (16 bytes); v0.1.7 used
three fields (12 bytes). This by-value structure is not binary-compatible
across those versions. Rebuild C/C++ callers and manual Swift/FFI mirrors
with the matching header and native library, and initialize `video_alpha_mode`
to `0` for opaque video.

`ErikaPresenterConfig` selects the output mode (`Sdr`, Apple `AppleEdr`, or
Android `ExtendedLinear`), the requested EDR/scRGB content-headroom ceiling,
the initial luma upscaler, and the `video_alpha_mode`
(`ErikaVideoAlphaMode`). Android `ExtendedLinear` means FP16 extended-linear
scRGB, not HDR10/PQ. `create_with_output_mode` and
`create_with_output_mode_and_alpha` are shorthands; `create` uses defaults
(SDR, opaque video, no upscaler). A `NULL` return means creation failed —
check `erika_last_error_message`.

### Playback and runtime parameters

```c
ErikaStatus erika_presenter_open(ErikaPresenterHandle *, const char *uri);
ErikaStatus erika_presenter_open_with_headers(ErikaPresenterHandle *, const char *uri,
                                              const ErikaHttpHeader *headers,
                                              uintptr_t header_count);
ErikaStatus erika_presenter_open_with_options(ErikaPresenterHandle *, const char *uri,
                                              const ErikaOpenOptions *options);
ErikaStatus erika_presenter_play(ErikaPresenterHandle *);
ErikaStatus erika_presenter_pause(ErikaPresenterHandle *);
ErikaStatus erika_presenter_stop(ErikaPresenterHandle *);
ErikaStatus erika_presenter_close(ErikaPresenterHandle *);
ErikaStatus erika_presenter_seek(ErikaPresenterHandle *, uint64_t position_micros);
ErikaStatus erika_presenter_set_playback_rate(ErikaPresenterHandle *, double rate);
ErikaStatus erika_presenter_set_volume(ErikaPresenterHandle *, double volume);   // 0.0–1.0
ErikaStatus erika_presenter_set_upscaler(ErikaPresenterHandle *, int32_t mode);  // ErikaLumaUpscalerMode
ErikaStatus erika_presenter_set_subtitle_scale(ErikaPresenterHandle *, double scale);
ErikaStatus erika_presenter_set_subtitle_font(ErikaPresenterHandle *, const char *family, const char *file_path);
ErikaStatus erika_presenter_set_subtitle_style(ErikaPresenterHandle *, ErikaSubtitleStyle style);
ErikaStatus erika_presenter_set_output_headroom(ErikaPresenterHandle *, float headroom, bool known);
```

`set_playback_rate(1.0)` is normal speed. `erika_presenter_open_with_options`
is the push-model counterpart of `erika_open_with_options` and accepts the same
`ErikaOpenOptions` (headers plus `http_read_ahead_bytes` /
`http_back_buffer_bytes`; see
[`erika_open_with_options`](#erikahandle--pull-model)). `set_upscaler` switches the neural
luma upscaler at runtime (see [`erika_presenter_get_upscaler_status`](#diagnostics-and-capture));
Metal, D3D11 feature level 11+, and compute-capable wgpu renderers execute
ArtCNN, while backends without compute retain native luma sampling and report
an explicit `Inactive` fallback.

`set_subtitle_font` and `set_subtitle_style` set the subtitle *fallback* look.
An empty family or path clears that half of the selection. Colours are
`0xRRGGBBAA`, defaulting to opaque white text (`0xFFFFFFFF`) and
half-transparent black outline (`0x0000007F`). `font_size` (default `48`,
clamped to `8..400`) and `outline_width` (default `2`, clamped to `0..32`) are in
ASS script units and are still multiplied by `set_subtitle_scale`. A container
ASS script keeps its own styling; these only fill in what the script leaves
open, what the system cannot resolve, and the look of plain-text (SRT/WebVTT)
subtitles.

`ErikaSubtitleStyle` carries the full look, and out-of-range metrics are clamped
rather than rejected:

```c
typedef struct ErikaSubtitleStyle {
  const char *font_family;      /* NULL or empty keeps the platform default */
  const char *font_file_path;
  uint32_t primary_color_rgba;  /* 0xRRGGBBAA */
  uint32_t outline_color_rgba;
  double font_size;             /* 8..400   */
  double outline_width;         /* 0..32    */
  bool bold, italic, underline, strike_out;
  double spacing;               /* -100..100 */
  double scale_x_percent;       /* 1..1000, 100 = unscaled */
  double scale_y_percent;
  int32_t border_style;         /* 1 outline+shadow, 3 opaque box */
  double shadow_depth;          /* 0..32 */
  double blur;                  /* 0..100 */
  int32_t alignment;            /* 1..9, numpad layout, 2 = bottom centre */
  int32_t margin_left, margin_right, margin_vertical;
  uint32_t override_mask;
} ErikaSubtitleStyle;
```

`override_mask` decides which of those fields stop being fallbacks and instead
replace what ASS dialogue events request, through libass' selective style
override. It is a bitmask of the `ERIKA_SUBTITLE_OVERRIDE_*` macros in
`erika.h`, which mirror libass' `ASS_OVERRIDE_BIT_*` values:

| Macro | Replaces |
| --- | --- |
| `ERIKA_SUBTITLE_OVERRIDE_FONT_SIZE_FIELDS` | `font_size`, `spacing`, `scale_x_percent`, `scale_y_percent` |
| `ERIKA_SUBTITLE_OVERRIDE_FONT_NAME` | `font_family` |
| `ERIKA_SUBTITLE_OVERRIDE_COLORS` | `primary_color_rgba`, `outline_color_rgba` |
| `ERIKA_SUBTITLE_OVERRIDE_ATTRIBUTES` | `bold`, `italic`, `underline`, `strike_out` |
| `ERIKA_SUBTITLE_OVERRIDE_BORDER` | `border_style`, `outline_width`, `shadow_depth` |
| `ERIKA_SUBTITLE_OVERRIDE_ALIGNMENT` | `alignment` |
| `ERIKA_SUBTITLE_OVERRIDE_MARGINS` | `margin_left`, `margin_right`, `margin_vertical` |
| `ERIKA_SUBTITLE_OVERRIDE_BLUR` | `blur` |
| `ERIKA_SUBTITLE_OVERRIDE_ALL` | every field above |

`0` leaves every field a fallback, and unknown bits are dropped. Sizes stay
resolution-independent when overridden — a `font_size` of `48` lands on the same
pixels whatever `PlayResY` a script declares. Margins do not: libass leaves
override margins in the script's own units, so on a container ASS track they
scale with that script's `PlayResY`, while on plain-text subtitles (whose script
liberika generates at the frame size) they are pixels.

`ErikaHttpHeader` is defined as:

```c
typedef struct ErikaHttpHeader {
  const char *name;
  const char *value;
} ErikaHttpHeader;
```

For example:

```c
ErikaHttpHeader headers[] = {
    {"Authorization", "Bearer token"},
    {"Referer", "https://example.com/"},
};
erika_presenter_open_with_headers(presenter, "https://example.com/video.mp4",
                                  headers, 2);
```

Headers apply only to HTTP(S) sources; local files and Android `content://`
sources ignore them. The caller must keep the URI, header names, and values
valid for the duration of the call; the API does not take ownership of these
strings.

Headers the player derives itself are rejected rather than merged, because the
underlying HTTP client appends duplicates instead of replacing them: `Range`,
`Host`, `Content-Length`, `Transfer-Encoding`, and `Connection` (matched
case-insensitively) all fail the call with `ERIKA_STATUS_PLAYER_ERROR`. A header
name that is not a valid HTTP token, or a value containing characters that are
not allowed in a field value, fails the same way — validation happens during
`open`, not on the first range request.

Headers are attached to the media source only. External subtitle tracks and
danmaku sidecar files are still fetched without them, so a token that
authenticates the video does not yet authenticate those URLs.

`set_output_headroom` publishes the display's current HDR/SDR ratio. Android
API 34+ hosts should call it from
`Display.registerHdrSdrRatioChangedListener`, passing `known = true` for a valid
ratio and `(1.0f, false)` when the measurement becomes unavailable. Values are
sanitized to `1.0..10000.0`. wgpu ignores duplicate state, updates subsequent
frame targets without reattaching the surface, and increments
`headroom_updates` only for a real known-state or ratio change. The effective
content target is bounded by the configured content ceiling, any positive
surface `desired_headroom`, and the known display ratio. Other renderers may
ignore this advisory update.

### Tracks and subtitles

```c
ErikaStatus erika_presenter_add_external_subtitle(ErikaPresenterHandle *, const char *uri, int64_t *out_track_id);
ErikaStatus erika_presenter_remove_subtitle_track(ErikaPresenterHandle *, int64_t track_id);
ErikaStatus erika_presenter_select_audio_track(ErikaPresenterHandle *, int64_t track_id);
ErikaStatus erika_presenter_select_subtitle_track(ErikaPresenterHandle *, int64_t track_id);
ErikaStatus erika_presenter_track_selection(ErikaPresenterHandle *, ErikaTrackSelection *out_selection);
ErikaStatus erika_presenter_tracks(ErikaPresenterHandle *, ErikaTrackInfo *out_tracks, uintptr_t capacity, uintptr_t *out_len);
```

Same semantics as the `ErikaHandle` track functions.

In-memory subtitle fonts let a host register font data directly instead of
relying on the bundled fallback font or system font providers:

```c
ErikaStatus erika_presenter_register_subtitle_memory_font(ErikaPresenterHandle *, const uint8_t *data, uintptr_t data_len, uint64_t *out_font_id);
ErikaStatus erika_presenter_select_subtitle_memory_fonts(ErikaPresenterHandle *, const uint64_t *font_ids, uintptr_t font_count);
ErikaStatus erika_presenter_clear_subtitle_memory_fonts(ErikaPresenterHandle *);
ErikaStatus erika_presenter_get_subtitle_memory_font_status(ErikaPresenterHandle *, ErikaSubtitleMemoryFontStatus *out_status);
ErikaStatus erika_presenter_get_subtitle_memory_font_info(ErikaPresenterHandle *, uint64_t font_id, ErikaSubtitleMemoryFontInfo *out_info);
void erika_subtitle_memory_font_status_free(ErikaSubtitleMemoryFontStatus *status);
void erika_subtitle_memory_font_info_free(ErikaSubtitleMemoryFontInfo *info);
```

Registered fonts count against a total byte limit. `select_subtitle_memory_fonts`
replaces the active selection with the given already-registered ids (invalid or
duplicate ids are rejected); `clear_subtitle_memory_fonts` drops both the
selection and the registered fonts. The `*_free` helpers release the returned
status/info buffers.

### Danmaku (bullet comments)

```c
ErikaStatus erika_presenter_load_danmaku_file(ErikaPresenterHandle *, const char *uri);
ErikaStatus erika_presenter_load_danmaku_json(ErikaPresenterHandle *, const char *json);
ErikaStatus erika_presenter_add_danmaku_track_file(ErikaPresenterHandle *, const char *uri, const char *name, int64_t offset_micros, uint64_t *out_track_id);
ErikaStatus erika_presenter_add_danmaku_track_json(ErikaPresenterHandle *, const char *json, const char *name, int64_t offset_micros, uint64_t *out_track_id);
ErikaStatus erika_presenter_remove_danmaku_track(ErikaPresenterHandle *, uint64_t track_id);
ErikaStatus erika_presenter_set_danmaku_track_enabled(ErikaPresenterHandle *, uint64_t track_id, bool enabled);
ErikaStatus erika_presenter_set_danmaku_track_offset(ErikaPresenterHandle *, uint64_t track_id, int64_t offset_micros);
ErikaStatus erika_presenter_set_danmaku_global_offset(ErikaPresenterHandle *, int64_t offset_micros);
ErikaStatus erika_presenter_danmaku_tracks(ErikaPresenterHandle *, ErikaDanmakuTrackInfo *out_tracks, uintptr_t capacity, uintptr_t *out_len);
void erika_danmaku_track_info_free(ErikaDanmakuTrackInfo *track);
ErikaStatus erika_presenter_clear_danmaku(ErikaPresenterHandle *);
ErikaStatus erika_presenter_set_danmaku_enabled(ErikaPresenterHandle *, bool enabled);
ErikaStatus erika_presenter_set_debug_hud_enabled(ErikaPresenterHandle *, bool enabled);
ErikaStatus erika_presenter_set_danmaku_config(ErikaPresenterHandle *, ErikaDanmakuConfig config);
ErikaStatus erika_presenter_set_danmaku_config_ptr(ErikaPresenterHandle *, const ErikaDanmakuConfig *config);
ErikaStatus erika_presenter_get_danmaku_config(ErikaPresenterHandle *, ErikaDanmakuConfig *out_config);
ErikaStatus erika_presenter_set_danmaku_font(ErikaPresenterHandle *, const char *family, const char *file_path);
ErikaStatus erika_presenter_set_danmaku_block_words_json(ErikaPresenterHandle *, const char *json);
```

`load_danmaku_*` replaces the active danmaku with a single anonymous track;
`add_danmaku_track_*` builds a multi-track list (each with a name and time
offset). Input is Bilibili XML (`*_file`, by path/URL) or JSON (`*_json`,
inline). `offset_micros` shifts a track's timeline; the global offset shifts all
tracks. `set_danmaku_config` / `_ptr` apply the full `ErikaDanmakuConfig` (the
`_ptr` variant avoids passing the struct by value); `get_danmaku_config` reads
it back. See [danmaku_architecture.md](danmaku_architecture.md) for the layout
engine. `set_danmaku_block_words_json` takes a JSON array of strings to filter.

Inline JSON may be an item array or an object containing a `comments`,
`danmaku`, or `items` array. Each item accepts these fields (aliases in
parentheses):

- `content` (`text`, `c`): text; an item with missing or blank text is skipped.
- `time` (`t`): presentation time in seconds, defaulting to `0`.
- `type` (`mode`, `y`): `scroll`/`1`, `bottom`/`4`, `top`/`5`, `reverse`/`6`,
  or `special`/`7`. Numeric `type_code` / `mode_code` are also accepted.
- `color` (`r`): decimal RGB, `#RRGGBB`, or `rgb(r,g,b)`.
- `font_size` (`fontSize`, `size`, `s`), `opacity` (`alpha`, `a`), and `is_me`
  (`isMe`, `self`, `mine`): optional style and self-danmaku metadata.
- `id`: an optional unsigned 64-bit integer or decimal string. When omitted,
  liberika assigns the item's position in the complete input. The session creates
  a separate internal layout identity, so hosts do not need to synthesize a
  business ID to keep tracks stable across planner windows.

Unknown fields are ignored, so standard maps carrying source fields such as
`cid` or `danmakuId` may be passed through unchanged.

`set_debug_hud_enabled` is off by default. When enabled, the Presenter draws a
native diagnostic HUD in the video composition. It shows track
technical metadata, playback state, decoded/rendered FPS, decode and zero-copy
route counters, render/audio state, HDR output, and danmaku item count. The HUD
only appears when a video frame exists, does not require host-side stats polling,
and does not populate `ErikaPresenterStats`. Off-screen `capture_frame_rgba`
captures exclude the HUD.

### Surface and presentation

```c
ErikaStatus erika_presenter_attach_metal_layer(ErikaPresenterHandle *, uint64_t raw_layer, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_attach_wgpu_surface(ErikaPresenterHandle *, ErikaWgpuSurfaceKind kind, uint64_t raw_window, uint64_t raw_display, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_attach_wgpu_surface_with_output_capabilities(ErikaPresenterHandle *, ErikaWgpuSurfaceKind kind, uint64_t raw_window, uint64_t raw_display, uint32_t w, uint32_t h, double scale, ErikaSurfaceOutputCapabilities capabilities);
ErikaStatus erika_presenter_attach_windows_hwnd(ErikaPresenterHandle *, uint64_t hwnd, uint64_t hinstance, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_resize_surface(ErikaPresenterHandle *, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_detach_surface(ErikaPresenterHandle *);
```

Use `attach_metal_layer` on macOS/iOS/tvOS (a `CAMetalLayer*`), and
`attach_windows_hwnd` on Windows (it is a convenience wrapper over
`attach_wgpu_surface` with kind `WindowsHwnd`, passing `HWND` + `HINSTANCE`).
The renderer backend bound to the surface (native Metal, native Direct3D 11, or
wgpu) is decided by the presenter configuration, not by the attach call. Call
`resize_surface` whenever the drawable size or scale changes.

For Android extended-linear, pass an `AndroidNativeWindow` from a
Hybrid-Composition `SurfaceView` through the `_with_output_capabilities`
function. Set `extended_linear` only after the display/surface HDR probe, set
`direct_composition = true` only for the direct `SurfaceView`, and preserve any
host probe failure in `fallback_reason`. `desired_headroom = 0` means system
auto; a positive value is a surface ceiling and is suitable for API 35
per-`SurfaceView` `setDesiredHdrHeadroom`. liberika still verifies Vulkan,
`Rgba16Float`, and `ADATASPACE_SCRGB_LINEAR` itself; failure of any condition
falls back to SDR and remains queryable.

### Flutter texture surfaces

```c
ErikaStatus erika_presenter_attach_flutter_texture(ErikaPresenterHandle *, ErikaFlutterTextureKind kind,
                                                   int64_t texture_id, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_set_flutter_texture_buffer(ErikaPresenterHandle *, uint64_t raw_texture,
                                                       uint32_t w, uint32_t h);
```

`attach_flutter_texture` binds the presenter to a texture-registrar surface
identified by `texture_id` (Apple `MacOsTextureRegistrar`/`IosTextureRegistrar`
today). The host owns the pixel buffers; before **every** `render_tick` it
selects the GPU target for the next frame with
`set_flutter_texture_buffer`, passing an `id<MTLTexture>` pointer cast to
`uint64_t` that must use `BGRA8Unorm` and match the declared `w`×`h`. The
texture is only borrowed for the duration of the frame — the host keeps
ownership and may reuse or free it once `render_tick` returns. This is the
surface the Flutter plugin's `ErikaTextureVideoView` uses on macOS.

### Windows DirectComposition swap chain

```c
ErikaStatus erika_presenter_windows_composition_swapchain_iunknown(ErikaPresenterHandle *, void **out_swapchain);
```

Windows only. When the presenter was attached through
`attach_wgpu_surface_with_output_capabilities` with `direct_composition = true`
(and the presenter plays with a transparent video alpha mode or an overlay
blend), liberika creates a premultiplied-alpha composition swap chain for the
target HWND. This getter returns it as an **AddRef'd `IUnknown*`**: the caller
owns the returned COM reference and must `Release` it. Re-fetch it after
decoder/device loss — liberika recreates the swap chain and exposes the new
object; the same pointer means nothing was rebuilt.

### Windows Flutter texture

```c
ErikaStatus erika_presenter_windows_flutter_texture_iunknown(ErikaPresenterHandle *, void **out_texture);
```

Windows only; this symbol is not exported on other platforms. Returns the latest completed, immutable SDR Flutter GPU frame as an **AddRef'd `IUnknown*`**. The caller owns the reference and must `Release` it. Keep the reference while consuming the frame; its texture contents remain unchanged throughout its lifetime. A null output pointer returns `NullPointer`. With a valid presenter but no completed Flutter texture, it returns `PlayerError` and clears the output pointer. Drive rendering before requesting a frame.

### Render loop and events

```c
ErikaStatus erika_presenter_render_tick(ErikaPresenterHandle *, double time_seconds, ErikaPresenterStats *out_stats);
ErikaStatus erika_presenter_get_stats(ErikaPresenterHandle *, ErikaPresenterStats *out_stats);
ErikaStatus erika_presenter_poll_event(ErikaPresenterHandle *, ErikaEvent *out_event);
```

Call `render_tick` once per display frame (e.g. from `CADisplayLink`,
`CVDisplayLink`, or a Windows frame scheduler). `time_seconds` is the host
display clock for the frame in seconds; it drives the idle test pattern
animation, so pass the presentation timestamp, not wall-clock deltas. If
`out_stats` is non-`NULL` it is filled with a snapshot of pipeline counters.
`poll_event` is non-blocking and returns `NoEvent` when idle.

`audio_only_tick` advances audio output without rendering, for hosts that have
no surface attached:

```c
ErikaStatus erika_presenter_audio_only_tick(ErikaPresenterHandle *, ErikaPresenterStats *out_stats);
```

`get_stats` fills the same `ErikaPresenterStats` snapshot without rendering a
frame. Use it when the host samples counters on a different cadence from the
display loop; it does not advance presentation.

### Display-target rendering

```c
ErikaStatus erika_presenter_render_tick_with_timing( ErikaPresenterHandle *handle, double time_seconds, const double *presentation_delay_seconds, ErikaPresenterStats *out_stats);
```

The optional delay is signed seconds from call entry to the display target (±0.25 s). NULL keeps legacy sampling. Video, subtitles and danmaku sample the same target without changing the playback clock.

### JSON bridge

For embedders whose platform channel already serializes structured arguments
(the HarmonyOS ArkTS plugin, for example), the same presenter surface is
available as JSON:

```c
char *erika_presenter_invoke_json(ErikaPresenterHandle *, const char *method,
                                  const char *arguments_json);
char *erika_presenter_render_tick_json(ErikaPresenterHandle *, double time_seconds);
char *erika_presenter_poll_event_json(ErikaPresenterHandle *);
```

Every returned string is owned by liberika and must be released with
`erika_string_free`. `poll_event_json` returns `NULL` — not an envelope — when
no event is pending, so a `NULL` return is the idle case, not an error.

All three wrap their result in an envelope:

```json
{ "ok": true,  "status": 0, "value": <result> }
{ "ok": false, "status": 3, "error": "<message>" }
```

`arguments_json` must be a JSON object. `method` selects an entry of the
dispatch table in `crates/erika_capi/src/presenter_json.rs` (`JSON_METHODS`).
The table mirrors the C entry points — playback (`open`, `play`, `pause`,
`stop`, `close`, `seek`, `setPlaybackRate`, `setVolume`), tracks and subtitles
(`tracks`, `trackSelection`, `selectAudioTrack`, `selectSubtitleTrack`,
`addExternalSubtitle`, `removeSubtitleTrack`, `setSubtitleScale`,
`setSubtitleFont`, `setSubtitleStyle`, the subtitle memory-font calls), the
danmaku family (including `getDanmakuConfig`), output and upscaler
(`setUpscaler`, `getUpscalerStatus`, `setOutputHeadroom`, `getOutputStatus`,
`setDebugHudEnabled`), counters (`getPresenterStats`, `getResourceStatus`), and
the diagnostics below (`getDiagnosticsSnapshot`, `getLogs`).

`erika_presenter_invoke_json` doubles as the generic pass-through into the
engine:

- `listMethods` returns the table itself — `{schema: 1, fields, methods: [{name, scope,
  summary, arguments: [{name, type, required}], returns, blocking, writesFiles}]}` — so a host (or an
  agent behind it) can discover and forward capabilities without code changes.
  Registering a capability means adding a `match` arm in `invoke` and an entry
  in `JSON_METHODS`; a unit test fails if the two disagree.
- Each entry also carries `blocking` (the call returns when the work is done,
  on the calling thread, and the work grows with its input: media length, file
  size) and `writesFiles` (the call creates files on disk and its result lists
  their paths); `fields` at the top level states both. `exportFrame` and
  `exportFrames` take pictures from a media with the frame export above: with
  `outputPath` / `outputDir` they write PNG or RGBA files and return the paths,
  and `exportFrame` without `outputPath` returns the bytes base64 encoded.
- Methods whose `scope` is `global` (`listMethods`, `getLogs`, `exportFrame`,
  `exportFrames`) accept a `NULL`
  handle.
- `invokeJson` (`{method, arguments}`) calls another table method and returns
  its full envelope as `value`. Platform plugins that unwrap envelopes
  natively (Linux, HarmonyOS, Android) use it to forward Dart's
  `ErikaPlayer.invokeJson` unchanged.
- An unknown method returns `{"ok": false, "status": 3, "code":
  "unknown_method", "method": "<name>", "error": "unsupported Erika presenter
  method: <name>"}`; malformed `arguments_json` adds `"code":
  "invalid_arguments"`. Neither aborts.

The bridge calls the same functions documented above and carries no extra
capability. Hosts that can pass structs across their platform channel may
prefer the typed entry points.

### Diagnostics and capture

```c
ErikaStatus erika_presenter_get_upscaler_status(ErikaPresenterHandle *, ErikaUpscalerStatus *out_status);
ErikaStatus erika_presenter_get_output_status(ErikaPresenterHandle *, ErikaOutputStatus *out_status);
ErikaStatus erika_presenter_get_resource_status(ErikaPresenterHandle *, ErikaPresenterResourceStatus *out_status);
ErikaStatus erika_presenter_capture_frame_rgba(ErikaPresenterHandle *, uint32_t width, uint32_t height,
                                               uint8_t *out_rgba, uintptr_t out_capacity);
```

`get_upscaler_status` reports the requested upscaler mode, the active backend
(off / inactive / building / scalar / simdgroup-matrix), the fallback count,
upscaled frame count, and recent encode/GPU timings in microseconds.

`get_output_status` returns the actual negotiated output, not merely the
request. The 13 fields are:

| Field | Meaning |
|-------|---------|
| `requested_mode` | `ErikaPresenterOutputMode` requested at creation. |
| `active_encoding` | Actual `SdrSrgb`, `AppleEdr`, `AndroidExtendedLinearScRgb`, or `Hdr10Pq` encoding. |
| `surface_format` | Actual 8-bit UNORM, 10-bit UNORM, or 16-bit float surface class. |
| `native_data_space` | Android `ANativeWindow` dataspace; `406913024` is `SCRGB_LINEAR`, `-1` means unavailable/not applicable. |
| `requested_headroom` | Sanitized requested content-headroom ceiling, at least `1.0`. |
| `active_headroom` | Current display HDR/SDR ratio when known; otherwise an effective-content fallback value. |
| `active_headroom_known` | Whether `active_headroom` came from an authoritative platform ratio. Android sets this true when the API 34+ ratio is available. |
| `extended_linear_active` | A floating-point extended-linear presentation path is active; use `active_encoding` to distinguish Apple EDR from Android scRGB. |
| `fallback_reason` | Stable `ErikaOutputFallbackReason` code explaining why the requested mode is not active. |
| `fallback_count` | Number of recorded output fallback transitions/failures. |
| `data_space_failures` | Dataspace/output-color-space validation failure count. |
| `headroom_updates` | Real runtime headroom state changes; duplicate ratio/known publications do not increment it. |
| `extended_linear_frames` | Frames presented through an active extended-linear path. |

The fallback values are ABI-stable; append new reasons, never renumber `0..8`:

| Code | Enum | Stable label | Meaning |
|------|------|--------------|---------|
| 0 | `None` | `none` | No fallback. |
| 1 | `DisplayHdrUnsupported` | `display_hdr_unsupported` | Display/surface HDR capability probe failed. |
| 2 | `HybridCompositionRequired` | `hybrid_composition_required` | Android surface is not direct `SurfaceView` composition. |
| 3 | `WgpuBackendNotVulkan` | `wgpu_backend_not_vulkan` | Active wgpu backend is not Vulkan (for example GLES). |
| 4 | `Rgba16FloatSurfaceFormatUnavailable` | `rgba16float_surface_format_unavailable` | Surface capabilities do not expose `Rgba16Float`. |
| 5 | `NativeWindowDataSpaceApiUnavailable` | `native_window_dataspace_api_unavailable` | `ANativeWindow_*DataSpace` API is unavailable (including API 26/27). |
| 6 | `ScrgbDataSpaceVerificationFailed` | `scrgb_dataspace_verification_failed` | `SCRGB_LINEAR` set/readback did not verify. |
| 7 | `SurfaceConfigureFailed` | `surface_configure_failed` | Requested output surface configuration failed. |
| 8 | `LegacyAppleEdrUnsupported` | `legacy_apple_edr_unsupported` | Apple EDR was requested on a backend that does not implement it. |

`capture_frame_rgba` is a **screenshot**: it renders the current composited
frame (video + subtitle) off-screen into a caller-allocated RGBA8 buffer at the
requested `width`×`height` (independent of the display surface size). Danmaku
is deliberately not included — screenshots represent the video rather than
transient on-screen comments. `out_capacity` must be at least `width*height*4`.
It returns `PlayerError` when no frame is available yet. Metal and wgpu
(including Android) implement capture; the current D3D11 backend does not.
Capture always uses an SDR RGBA8 offscreen target and tone-maps
HDR/extended-linear content, so the returned
bytes are SDR even when the display output is Apple EDR, HDR10, or Android
extended-linear scRGB.

```c
uint32_t w = 1920, h = 1080;
uint8_t *rgba = malloc((size_t)w * h * 4);
if (erika_presenter_capture_frame_rgba(p, w, h, rgba, (uintptr_t)w * h * 4) == ErikaStatus_Ok) {
    /* rgba holds w*h tightly-packed RGBA8 pixels — encode to PNG, etc. */
}
free(rgba);
```

`get_resource_status` reports memory budgeting for diagnostics: current device
allocation, recommended working set, per-bucket GPU byte estimates (video
frames, overlay and danmaku atlases, danmaku vertex buffers, upscaler), the
renderer-tracked total, presenter CPU-side danmaku atlas bytes, the drawable
count, and how many times the output mode was switched. All values are
snapshots from the same runtime state as `get_output_status`.

### Agent diagnostics (JSON)

```c
ErikaStatus erika_presenter_get_diagnostics_json(ErikaPresenterHandle *, char **out_json);
ErikaStatus erika_log_read_json(uint32_t limit, int32_t min_level, uint64_t since_seq,
                                char **out_json);
```

New diagnostic data is returned as JSON documents, never as new fields on the
structs above (Apple plugins mirror those structs by hand, so growing them would
force a lock-step library/plugin upgrade). Resolve both symbols optionally and
treat a missing symbol as "not supported". On `Ok`, `*out_json` holds a
caller-owned UTF-8 document to free with `erika_string_free`; it is `NULL` on
any other status. Each document has a `schema` number (currently `1`); fields
are only added within a schema. Values that are not known are `null`, never a
made-up zero. Durations are milliseconds with microsecond precision.

#### Diagnostics snapshot

`erika_presenter_get_diagnostics_json` gathers everything in one call. It only
reads presenter fields, renderer/audio counters and lock-free playback
counters: it neither decodes nor renders and never waits on the playback
worker (on macOS, a release build measured a median of 0.3 ms per call
including JSON encoding during playback). Same threading rule as every other
handle call.

| Section | Contents |
|---------|----------|
| `schema`, `capturedAtUnixMs` | Document version and wall-clock capture time. |
| `playback` | `state`, `playing`, `mediaTimeSeconds`, `durationSeconds`, `playbackRate`, `generation`, `audioOnly` (background video suspension), `buffering` `{active, events, currentMs, totalMs}`. |
| `media` | Selected `video` (`codec`, `profile`, `level`, `width`, `height`, `pixelFormat`, `frameRate`, `bitRate`), `audio` (`codec`, `sampleRate`, `channels`, `sampleFormat`, `bitRate`) and `subtitle` tracks, or `null`. |
| `decoder` | Latest decoder transition, queryable instead of event-only: `requestedBackend`, `activeBackend`, `previousBackend`, `hardware`, `fellBack`, `fallbackCount`, `stage`, `reason`, `codec`, `pixelFormat`, `lineSizes`. `null` before the first transition. |
| `video` | Decoded/rendered frames, hardware/software/zero-copy/direct-zero-copy/shared-handle frames, `cpuFallbackFrames`, `importFailures`, `renderFailures`, `backpressureDrops`, `rejectedImportRoute`. |
| `presenter` | Surface attachment and size, last tick/pump/audio/subtitle/video/clock/danmaku-plan/render/GPU durations, test/overlay/subtitle/danmaku counters. |
| `output` | Requested mode, active encoding, surface format, dataspace, requested/active headroom (`null` when unknown), fallback reason label and counters. |
| `hdr` | HDR source frames, HDR10 output frames, SDR tone-mapped frames, metadata updates/failures, output failures, `outputActive`. |
| `resources` | Same fields as `ErikaPresenterResourceStatus`. |
| `upscaler` | Requested mode and active backend as labels, fallbacks, upscaled frames, last encode time. |
| `audio` | Output state, configured/started, volume, pushed frames, failures, queued frames and `queuedMs`, read/written/underflow frames, ring `ringDroppedFrames`, device `recovery` state and counts. |
| `buffers` | `playback` queue depths published by the worker (`demuxPackets`, `pendingVideoPackets`, `decodedVideoFrames`, `decodedAudioFrames`, `decodedSubtitleFrames` as `{queued, capacity}`, `demuxEof`, `updatedAgoMs`; `null` before open), presenter frame channels, `audioOutputQueuedMs`. |
| `timing.scheduler` | `leadTimeMs`, `dropToleranceMs`, `maxConsecutiveDrops`; `presentedFrames`; `lateDroppedFrames` (discarded for being later than the drop tolerance — previously uncounted); `forcedLatePresents` (late beyond tolerance but shown because the consecutive-drop budget was spent); `maxDropBurst`, `maxDroppedLateMs`, `lastDropAgoMs`, `seekPrerollDroppedFrames`; `lateByMs` window `{samples, meanMs, p50Ms, p95Ms, maxMs}` over the last 128 presented frames; `lateHistogram` (`le10ms` … `gt120ms`). |
| `timing.presentation` | Measured where liberika renders, not at the display's vsync: `ticks` and `tickIntervalMs`; `uploadedFrames`, `newFramesPresented`, `supersededBeforePresent`; `frameIntervalMs` and `jitterMs` (absolute deviation from the source frame interval divided by the playback rate); `longFrames` (on screen longer than `longFrameFactor` = 1.5 source intervals); `lastPresentAgoMs`. |
| `timing.clock` | Audio-master discipline, previously trace-only: mode, sync config (`deadbandMs`, `maxCorrectionPerFrameMs`, `snapThresholdMs`, `audioLeadTimeMs`), `observations`, `corrections`, `snaps`, `forwardCorrections`, `backwardCorrections`, `lastDriftMs` (positive: audio ahead of the playback clock), `maxAbsDriftMs`, `appliedTotalMs`, `lastCorrectionAgoMs`, `lastSnapAgoMs`, `absDriftMs` window. |

Counters in `timing` and `buffers.playback` restart when the presenter opens
new media.

#### Log ring

`erika_log_read_json` exports a process-wide in-memory ring (at most 2000
entries / 512 KiB, entry text capped at 4 KiB). It needs no handle and may be
called from any thread. It collects:

- every operational diagnostic liberika already wrote to stderr / logcat /
  hilog (those outputs are unchanged);
- operational failures that used to be bare `eprintln!` (audio
  configure/push/start/pause/reset, renderer clear, subtitle render and
  decoder open, WASAPI, D3D11 device removal, upscaler disablement);
- FFmpeg `av_log` messages at warning level and above. The callback is
  process-wide and shared by every player: it formats into a stack buffer,
  never blocks (contended messages are counted as `droppedContended`), folds
  consecutive repeats that differ only in numbers or addresses into one entry
  with `count`, and admits at most 50 new FFmpeg entries per second (the rest
  are `rateLimited`). FFmpeg's own stderr output is still produced.
  `ERIKA_FFMPEG_LOG_CAPTURE=0` disables capture; with a shared distro FFmpeg
  (Linux system libraries) capture is off unless `ERIKA_FFMPEG_LOG_CAPTURE=1`,
  because other libraries in the process may own the callback.

Arguments: `limit` newest entries after filtering (`0` = every retained
entry); `min_level` an `ErikaLogLevel` (`Trace` 0 … `Error` 4) or `-1` for all;
`since_seq` only entries with `seq >= since_seq` (pass the previous `nextSeq`
to read incrementally).

```json
{
  "schema": 1,
  "capacity": { "entries": 2000, "bytes": 524288, "entryTextBytes": 4096 },
  "retained": 15, "retainedBytes": 6011, "oldestSeq": 1, "nextSeq": 16,
  "evicted": 0, "rateLimited": 0, "droppedContended": 0,
  "ffmpegCapture": "active", "redacted": true,
  "entries": [
    { "seq": 3, "unixMs": 1761134400123, "level": "error", "source": "ffmpeg",
      "event": null, "count": 1,
      "message": "[mpeg4 @ 0x…] Failed setup for format videotoolbox_vld: hwaccel initialisation returned error." },
    { "seq": 4, "unixMs": 1761134400140, "level": "warn", "source": "erika",
      "event": "video_decoder", "count": 2, "lastUnixMs": 1761134400410,
      "data": { "event": "video_decoder", "stage": "open_avcodec_to_software" } }
  ]
}
```

Entries are oldest first. Structured diagnostics come back parsed as `data`,
other lines as `message`. `ffmpegCapture` is `not_installed` (FFmpeg not used
yet), `active`, `disabled`, or `shared_library_opt_in_required`.

Levels: call sites that know the severity pass it; FFmpeg levels map
PANIC/FATAL/ERROR → `error`, WARNING → `warn`; libass maps 0–1 → `error`,
2 → `warn`. Other structured diagnostics get a default from their `event` and
`stage` names: `error` for fatal/panic, `warn` for failures, timeouts,
unavailable/lost/rejected/missing resources, stalls and fallbacks (including
`*_to_software` and `*_to_bytebuffer` routes), `info` otherwise.

Redaction happens on export: URL user info and everything after `?`/`#`,
credential-like `key=value`/`key: value` pairs and JSON keys (`token`,
`api_key`, `access_token`, `authorization`, `cookie`, `X-Emby-Token`, …),
`Bearer`/`Basic` credentials, the current user's home directory (`~`), and
other `/Users/<user>`, `/home/<user>`, `C:\Users\<user>` path segments. The
ring lives only in process memory and is read on demand.

## Enums

| Enum | Values |
|------|--------|
| `ErikaState` | `Idle` `Opening` `Ready` `Playing` `Paused` `Stopped` `Closed` `Error` |
| `ErikaEventKind` | `None` `StateChanged` `DurationChanged` `PositionChanged` `TracksChanged` `BufferingChanged` `VideoParamsChanged` `SurfaceAttached` `SurfaceDetached` `Error` `TrackSelectionChanged` `VideoDecoderChanged` `AudioOutputChanged` |
| `ErikaTrackKind` | `Video` `Audio` `Subtitle` |
| `ErikaTrackSource` | `Embedded` `External` |
| `ErikaWgpuSurfaceKind` | `Unknown` `MacOsNsView` `MacOsCaMetalLayer` `IosUiView` `WindowsHwnd` `XlibWindow` `WaylandSurface` `AndroidNativeWindow` `OhosNativeWindow` |
| `ErikaFlutterTextureKind` | `Unknown` `MacOsTextureRegistrar` `IosTextureRegistrar` `AndroidSurfaceTexture` `WindowsTextureRegistrar` `LinuxTextureRegistrar` |
| `ErikaVideoAlphaMode` | `Opaque` `PackedAlphaRight` |
| `ErikaPresenterOutputMode` | `Sdr` `AppleEdr` `ExtendedLinear` `Auto` |
| `ErikaActiveOutputEncoding` | `SdrSrgb` `AppleEdr` `AndroidExtendedLinearScRgb` `Hdr10Pq` `LinuxExtendedLinearScRgb` |
| `ErikaOutputSurfaceFormat` | `EightBitUnorm` `TenBitUnorm` `SixteenBitFloat` |
| `ErikaOutputFallbackReason` | `None` `DisplayHdrUnsupported` `HybridCompositionRequired` `WgpuBackendNotVulkan` `Rgba16FloatSurfaceFormatUnavailable` `NativeWindowDataSpaceApiUnavailable` `ScrgbDataSpaceVerificationFailed` `SurfaceConfigureFailed` `LegacyAppleEdrUnsupported` |
| `ErikaLumaUpscalerMode` | `Off` `ArtCnnC4F16` `ArtCnnC4F32` `ArtCnnC4F16Ds` |
| `ErikaUpscalerBackendStatus` | `Off` `Inactive` `Building` `Scalar` `SimdgroupMatrix` |

## Structs

- **`ErikaPresenterConfig`** `{ int32 output_mode; float edr_headroom; int32 luma_upscaler; int32 video_alpha_mode; }` —
  passed by value to `create_with_config`; `video_alpha_mode` is an
  `ErikaVideoAlphaMode` (`Opaque` default, `PackedAlphaRight` for side-by-side
  colour/alpha assets).
- **`ErikaSurfaceOutputCapabilities`** `{ bool extended_linear; bool direct_composition; float desired_headroom; int32 fallback_reason; }` — host-side Android display/surface probe supplied at attach time; `desired_headroom == 0` selects system auto.
- **`ErikaUpscalerStatus`** — requested mode, active backend, fallback count,
  upscaled frames, last encode/GPU micros.
- **`ErikaOutputStatus`** — the 13-field negotiated output snapshot documented
  under [Diagnostics and capture](#diagnostics-and-capture).
- **`ErikaPresenterResourceStatus`** — device/recommended working set bytes,
  per-bucket GPU byte estimates, renderer-tracked total, drawable count, and
  output-mode switch count; filled by `get_resource_status`.
- **`ErikaSubtitleMemoryFontStatus`** — registered/selected count and total
  bytes for in-memory subtitle fonts; free with
  `erika_subtitle_memory_font_status_free`.
- **`ErikaSubtitleMemoryFontFace`** / **`ErikaSubtitleMemoryFontInfo`** —
  per-font face details and aggregate info; free with
  `erika_subtitle_memory_font_info_free`.
- **`ErikaDanmakuConfig`** — full danmaku layout/appearance config (font size,
  opacity, display area, scroll timing, collision/stacking flags, blocked modes,
  shadow style). `font_size` is a NipaPlay/Flutter *logical* size; liberika
  multiplies by the surface scale for glyph pixels.
- **`ErikaDanmakuTrackInfo`** `{ id, enabled, offset_micros, item_count, char *name, char *source }` —
  free with `erika_danmaku_track_info_free`.
- **`ErikaVideoParams`** `{ width, height, primaries, transfer }` — color
  metadata reported via `VideoParamsChanged`.
- **`ErikaTrackCounts`** / **`ErikaTrackSelection`** — per-kind counts / selected
  ids (`-1` = none).
- **`ErikaTrackInfo`** — full per-track metadata; the six `char*` fields are
  owned by the caller (free via `erika_track_info_free`). Video tracks additionally
  expose `bit_rate` (bit/s) and `frame_rate_numerator` /
  `frame_rate_denominator` (`0` = unknown).
- **`ErikaEvent`** — a tagged union-by-struct: `kind` selects which fields are
  meaningful (`state`, `duration_micros`, `position_micros`, `buffering`,
  `video`, `tracks`); `status` carries the code for `Error` events.
- **`ErikaPresenterStats`** — pipeline counters: decoded/rendered frames,
  pushed audio frames, overlay/danmaku frames, hardware vs software vs zero-copy
  frame counts, HDR source/HDR10-output/SDR-tonemap counts, audio-clock
  read/queued/underflow frames, and last render timings.

## Events

Poll on each loop iteration and dispatch on `kind`:

```c
ErikaEvent ev;
while (erika_presenter_poll_event(p, &ev) == ErikaStatus_Ok) {
    switch (ev.kind) {
        case ErikaEventKind_StateChanged:    /* ev.state */                 break;
        case ErikaEventKind_DurationChanged: /* ev.duration_micros */        break;
        case ErikaEventKind_PositionChanged: /* ev.position_micros */        break;
        case ErikaEventKind_TracksChanged:   /* re-query erika_*_tracks */   break;
        case ErikaEventKind_BufferingChanged:/* ev.buffering */              break;
        case ErikaEventKind_VideoParamsChanged: /* ev.video */               break;
        case ErikaEventKind_Error:           /* ev.status + last_error */    break;
        default: break;
    }
}
```

The event queue is bounded and drained by polling; a host that stops polling
will simply stop observing state. `position_micros` is emitted periodically
during playback.

## Minimal presenter integration (C)

```c
#include "erika.h"

ErikaPresenterHandle *p = erika_presenter_create();
erika_presenter_attach_metal_layer(p, (uint64_t)layer, w, h, scale);  // or attach_windows_hwnd
if (erika_presenter_open(p, "/path/to/video.mkv") != ErikaStatus_Ok) {
    char *m = erika_last_error_message(); /* log */ erika_string_free(m);
}
erika_presenter_play(p);

// Per display frame:
ErikaPresenterStats stats;
erika_presenter_render_tick(p, host_time_seconds, &stats);
ErikaEvent ev;
while (erika_presenter_poll_event(p, &ev) == ErikaStatus_Ok) { /* dispatch */ }

// Teardown:
erika_presenter_detach_surface(p);
erika_presenter_destroy(p);
```

See [integration.md](integration.md) for the per-platform surface and
display-timer details, and the runnable
[`macos_native_demo`](../examples/macos_native_demo) /
[`windows_native_demo`](../examples/windows_native_demo) examples.
