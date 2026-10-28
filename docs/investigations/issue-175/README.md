# Issue #175 validation — 2026-10-29

[Issue](https://github.com/AimesSoft/liberika/issues/175) ·
[API guide](../../external_textures.md) ·
[Standalone host](../../../examples/external_texture_host/README.md)

## Scope

The library provides an optional danmaku-inclusive RGBA capture entry point
and a generic native GPU texture output. The existing screenshot entry point
continues excluding danmaku. Native output contains video, subtitles and enabled
danmaku, requires no window surface or Flutter registrar, and exposes no wgpu
Rust types. Implemented output transports are Linux Vulkan OPAQUE_FD and Apple
Metal. Export renders directly into the immutable output texture, without CPU
pixel readback or an output texture copy. Producer completion is waited on
before returning; the consumer follows the documented ownership/lifetime rules.

The host example imports producer wgpu **29.0.3** textures into an independent
wgpu **26.0.1** device. No Dioxus, Blitz or other UI framework is added. A
version-specific host HAL patch preserves imported Vulkan image contents on
the first transition; this is an example adapter detail, not part of the
library's public interface.

## Environments and results

| Environment | Result |
|---|---|
| macOS 26.7 (25G229), arm64, Apple M5 Pro / Metal | Core **815 passed, 1 ignored**; C API **55 passed**. Cross-version host **1 passed**, with complete pixel comparisons and retained frames used after producer destruction. |
| Workstation WSL2, Ubuntu 24.04.4 x86_64; isolated Ubuntu 26.04.1 container, Rust 1.93.1, FFmpeg 8, Mesa 26.0.8, Vulkan loader/validation 1.4.341, llvmpipe | Core **755 passed, 3 ignored**; C API **55 passed**, with `LP_NUM_THREADS=4`. Cross-version Vulkan host **1 passed** under validation layers. Native C importer syntax check passed. |
| Same WSL workstation, NVIDIA RTX 5060 Ti (driver 616.56), wgpu GLES over Mesa D3D12, hardware required | Presenter danmaku pixel/screenshot regression **1 passed**. Renderer fixture capture completed 120 measured frames, matched the final reference and exited successfully. |
| Workstation Vulkan with hardware required | Correctly rejected llvmpipe. This environment exposes hardware through D3D12/OpenGL, but enumerates only a CPU Vulkan adapter. **Hardware Vulkan external-memory import was not validated.** |

[Adapter evidence](evidence/adapters.txt) distinguishes software Vulkan from
NVIDIA-backed GLES. WSLg endpoints were available, but these tests were
windowless. No Wayland surface or Dioxus UI integration is claimed.

Core coverage includes requested dimensions, visibility/opacity, playback time,
seek/generation and track offsets, unchanged screenshots, immutable retained
native frames, independent native consumers and Vulkan FD closure. C tests
cover buffer/error boundaries and frame ownership after presenter destruction.
The host test uses 320×180, 640×360 and 319×181 targets with actual danmaku and
bitmap subtitle pixels, then reuses the imports after dropping the producer.
Readback in the native path is validation instrumentation only.

Final formatting, all five API/header alignment checks (including the optional
C++ bridge check), C/C++ header syntax and macOS standalone-host clippy with
`-D warnings` passed. The engine emits existing warnings. The WSL image does
not supply clippy; no Linux clippy result is claimed.

## Renderer fixture timings

These debug-build observations use a static **64×36 NV12 source scaled to
960×540**, three warmup frames and 120 measured frames. Decoding, window
presentation and final verification readback are excluded. Native mode includes
allocation, export/import waits, host composition and resource retirement.
RGBA capture mode includes its necessary pixel readback and has no host upload.
These are not measurements of the issue author's playback workload or a
120+ FPS playback guarantee.

| Path | Mean completed frame | Fixture throughput | Report |
|---|---:|---:|---|
| M5 Pro Metal native export → wgpu 26 composition | 4.095 ms | 244.2 fps | [JSON](evidence/native-macos-metal.json) |
| WSL llvmpipe Vulkan native export → wgpu 26 composition | 9.355 ms | 106.9 fps | [JSON](evidence/native-wsl-software.json) |
| WSL RTX 5060 Ti GLES RGBA capture only | 3.998 ms | 250.1 fps | [JSON](evidence/rgba-wsl-hardware.json) |

## Reproduction

Use liberika's normal native dependencies and bootstrap the standalone host:

```sh
python3 examples/external_texture_host/prepare.py
cargo test -p erika -p erika_capi --lib
CARGO_TARGET_DIR=examples/external_texture_host/target \
  cargo test --manifest-path examples/external_texture_host/Cargo.toml --lib
CARGO_TARGET_DIR=examples/external_texture_host/target \
  cargo run --manifest-path examples/external_texture_host/Cargo.toml -- \
  --require-hardware --frames 120 --report native.json --png native.png
```

For Linux without libass, the core/C test command used was:

```sh
WGPU_BACKEND=vulkan LP_NUM_THREADS=4 \
VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation \
  cargo test -p erika -p erika_capi --no-default-features --features wgpu --lib
```

The isolated WSL container received `/dev/dxg`, read-only `/usr/lib/wsl` and
WSLg mounts, and `LD_LIBRARY_PATH=/usr/lib/wsl/lib`. Hardware capture used:

```sh
WGPU_BACKEND=gl EGL_PLATFORM=surfaceless GALLIUM_DRIVER=d3d12 \
MESA_D3D12_DEFAULT_ADAPTER_NAME=NVIDIA ERIKA_REQUIRE_HARDWARE_GPU=1 \
CARGO_TARGET_DIR=examples/external_texture_host/target \
  cargo run --manifest-path examples/external_texture_host/Cargo.toml -- \
  --capture-only --require-hardware --frames 120 --report rgba.json --png rgba.png
```

The same GPU environment also ran:

```sh
cargo test -p erika --no-default-features --features wgpu --lib \
  wgpu_headless_capture_composites_danmaku_pixels_and_preserves_screenshot_behavior
```

Use a writable XDG runtime directory owned by the container user when providing
audio/window endpoints. Audio playback was not accepted in this windowless
container; its PulseAudio connection was unavailable.

## Harness limitations observed during verification

- With unrestricted Mesa workers and many parallel presenter initializations,
  the WSL C API suite intermittently segfaulted in Vulkan loader/validation
  `set_debug_utils_object_name` while creating a device's internal compute
  pipeline during presenter construction. The [captured stack](evidence/capi-loader-stack.txt)
  is retained. Serial execution passed; setting `LP_NUM_THREADS=4` also passed
  the full suite and **10 consecutive parallel C API runs (55 tests each)**.
  This records an environment workaround, not a claim that the loader's
  concurrency failure has been fixed in liberika.
- A shared build directory between the engine workspace and the standalone
  host can overwrite cdylib/rlib artifacts resolved from different lockfiles.
  Keep their `CARGO_TARGET_DIR` values separate, as in the commands above.
- Dual-version GLES capture/upload is excluded from the example: the two
  versions do not coordinate EGL display teardown. `--capture-only` covers
  hardware capture using one renderer. Native interop uses Vulkan or Metal.
- Hardware Vulkan, other GPU/driver combinations, host-specific UI import and
  complete playback performance remain deployment-specific acceptance work.
