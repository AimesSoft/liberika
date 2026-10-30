# Contributing to liberika

> [中文](CONTRIBUTING.zh.md) | [日本語](CONTRIBUTING.ja.md)

liberika is the in-house playback core for NipaPlay: a Rust engine that owns
everything from demux/decode to GPU presentation, audio, subtitles, and danmaku,
exposed to hosts through a C ABI and a Flutter plugin. This document is the
orientation map for working on it.

See also: [architecture.md](docs/architecture.md) (engine design),
[building.md](docs/building.md) (toolchain + native deps),
[capi_reference.md](docs/capi_reference.md) and
[integration.md](docs/integration.md) (the embedding surface).

## Repository layout

```
crates/erika              Core engine (library)
crates/erika_capi         C ABI export layer  →  erika.h
crates/erika_ffmpeg_sys   FFmpeg bindgen bindings
packages/erika_flutter    Flutter plugin (macOS + iOS + tvOS + Windows + Android + HarmonyOS)
examples/                 Validation / smoke / demo binaries
xtask/                    Native dependency build orchestration
docs/                     Architecture and integration docs
third_party/              Built native deps (gitignored output)
```

### Inside `crates/erika`

| Module | Responsibility |
|--------|----------------|
| `core` | Public config + the `RendererBackend` trait, `PlatformSurface`, `RendererBackendPreference`. |
| `playback` | Playback engine: video/audio tick, master clock, frame scheduler, `VideoDecodePreference`. |
| `ffmpeg` | Demux, decode, resample, seek; `DecoderBackend` (software / VideoToolbox / D3D11VA / MediaCodec). |
| `audio` | `AudioOutputBackend` trait, ring buffer, audio clock. |
| `renderer` | `metal` (Apple), `d3d11` (Windows), `wgpu` (cross-platform and Android), `pipeline` (backend-agnostic color/tone-map/scaler decisions). |
| `overlay` / `subtitle` / `text` | Overlay timeline; SRT/WebVTT/ASS + libass; font providers. |
| `danmaku` | Bilibili XML/JSON parsing, collision-avoidance layout, glyph atlas. |
| `presenter` | `PresenterRuntime` — ties player + renderer + audio + overlays; what `render_tick` drives. |
| `source` | `MediaSource` trait — file + HTTP range. |
| `apple` / `windows` / `android` | Platform glue: CoreAudio/AudioQueue/VideoToolbox/Metal interop (macOS/iOS/tvOS); WASAPI; AAudio/MediaCodec/native-window interop. |

## Runtime model

- **`PresenterRuntime` owns the stack.** The host calls `render_tick(time_seconds)`
  once per display frame; the runtime pumps decoded frames, updates the overlay
  (subtitle + danmaku), renders through the active `RendererBackend`, and
  presents. Danmaku plan generation is gated on `generation + media_time` so it
  stays synchronized with video.
- **Audio is the master clock.** Audio output runs on its own backend thread
  (WASAPI render thread, CoreAudio callback); the playback scheduler quantizes
  video presentation to vsync against the audio clock.
- **Events flow over a channel.** Player state changes are published on a
  crossbeam channel and surfaced to hosts via `poll_event`.
- **Backends are pluggable** behind three traits — `RendererBackend`,
  `AudioOutputBackend`, and the `DecoderBackend` selection — so the presenter
  stays platform-agnostic. `RendererBackendPreference` (`PlatformNative` vs
  `WgpuFallback`) and `VideoDecodePreference` choose concrete backends.

## Adding a platform backend

A new platform generally means three pieces, each behind `#[cfg(target_os = …)]`:

1. **Decode** — add a variant/path in `ffmpeg.rs` (`DecoderBackend`) that
   configures the hardware device (cf. the D3D11VA and VideoToolbox paths) and
   produces a `PlayerVideoFrame` the renderer can import zero-copy. Fall back to
   software decode when interop isn't available.
2. **Render** — implement `RendererBackend` (`core.rs`) in a new
   `renderer/<backend>.rs`:
   `attach_surface` / `detach_surface` / `resize_surface`, `upload_player_frame`
   (own the imported GPU representation), `render_current_frame` (composite the
   overlay), and optionally `capture_current_frame`, `runtime_stats`,
   `set_luma_upscaler`. Use `renderer::pipeline` for color/tone-map decisions so
   behavior matches the other backends. The D3D11 backend (`renderer/d3d11.rs`)
   is the most recent worked example.
3. **Audio** — implement `AudioOutputBackend` (`audio.rs`):
   `configure(PcmFormat)` / `start` / `pause` / `stop` / `push(PcmAudioFrame)` /
   `set_volume` / `state` / `stats`, and `clock_snapshot` for A/V sync. Reuse
   `BufferedAudioOutput` / `AudioRingBuffer` for the ring-buffer plumbing
   (WASAPI in `windows.rs` is the reference).

Then wire the backend into presenter selection and add a `<platform>_native_demo`
example for end-to-end validation.

## The C ABI is a contract

`crates/erika_capi` is the stable surface for every non-Rust host. When you
change it:

- **Contain panics.** Use `catch_unwind` where recovery is supported, and map caught
  panics to the return type and an error message. Some exports do not currently
  catch panics, and aborting builds cannot recover; see [panic safety](docs/capi_reference.md#panic-safety).
- **Honor ownership.** Strings handed out are caller-owned and freed via the
  matching `*_free`; document new ones in [capi_reference.md](docs/capi_reference.md).
- **Regenerate / hand-edit `erika.h`** to match, and annotate new functions.
- **⚠️ Sync the Swift mirror structs.** The macOS/iOS/tvOS plugins hand-mirror C
  structs (e.g. `ErikaPresenterStats`) on the Swift side. If you change a struct
  in `erika.h`, update the macOS, iOS, and tvOS Swift mirror files in `packages/erika_flutter`;
  a mismatch corrupts the stack and can surface as a misleading autorelease-pool
  crash rather than an obvious layout error.

## Testing

```sh
cargo test --workspace          # unit + integration tests
cargo clippy --workspace
cargo fmt --all
```

- Platform-specific code is `#[cfg]`-gated; when you touch a `cfg` branch, keep
  the `macos` / `ios` / `windows` / `android` / `tvos` / `ohos` / `linux` / fallback arms all compiling. CI builds the
  targets you can't test locally.
- The neural upscaler weights are verified against onnxruntime references in
  `tests/artcnn_upscaler.rs` (Metal) and `tests/wgpu_artcnn.rs` (tiled wgpu);
  don't change kernels without re-checking both applicable paths.
- Android SDR playback and explicit output fallback are verified. Do not label
  extended-linear scRGB device-validated until the API 35 HDR-device active path
  passes `Rgba16Float + SCRGB_LINEAR` acceptance and recovery checks.
- The native demos double as smoke tests:
  `cargo run -p macos_native_demo -- --smoke-seconds 3 "$SAMPLE"` (and the
  Windows equivalent) print pipeline counters — use them to confirm hardware
  decode and zero-copy interop still engage after a change.

## Conventions

- **Edition 2024**, Rust 1.92+. Run `cargo fmt` and keep `clippy` clean.
- **No `unwrap`/`panic` on the hot path or across FFI.** Return `Result` and map
  to `ErikaStatus` at the boundary.
- **Match the surrounding code** — naming, comment density, and the existing
  `cfg` structure. Platform glue stays in `apple.rs` / `windows.rs` /
  `android.rs`, not spread through the engine.
- **Keep docs in sync.** Architecture/integration docs and the trilingual
  READMEs (`README.md` + `readme/*.md`, `docs/*.{md,zh.md,ja.md}`) should reflect
  user-visible changes. The base doc is English; translations follow.

## Pull requests

Keep changes focused; note the platforms you built/tested and any platforms left
to CI. Update the relevant docs in the same PR. For larger features (a new
backend, an ABI change), a short design note in the PR description helps reviewers
follow the threading and ownership implications.

## Documentation ownership

Documentation starts at [docs/README.md](docs/README.md). Keep SDK usage in its package README, implementation/design in `docs/`, and dated investigation results in `docs/investigations/`. Add new topics to the documentation index and update matching translations with API changes.
