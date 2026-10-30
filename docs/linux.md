# Linux native integration (experimental)

> [中文](linux.zh.md) · [English](linux.md) · [日本語](linux.ja.md)

Linux supports the Rust player, C ABI, and Flutter through system FFmpeg 8.x, liberika's patched libass, wgpu, and PulseAudio. PipeWire desktops need `pipewire-pulse`; WSL2 uses WSLg display and audio services.

Video can render into X11 / Wayland windows. Flutter offers an RGBA SDR texture or a native Wayland video layer. NVIDIA uses NVDEC, Intel/AMD use VA-API, and FFmpeg software decoding is also available. Linux currently builds from source.

HDR surface negotiation and direct VA-API import are experimental. The [WSL D3D12 GPU-copy bridge](wsl-gpu-copy.md) provides another decode-frame transfer path. Device, performance, and validation results are kept in the [Linux validation record](investigations/linux-validation.en.md).

## Build on Ubuntu 26.04

Rust 1.92+ and FFmpeg **8.x** development libraries are required. Ubuntu 22.04
and 24.04's default FFmpeg packages are too old for this build path.

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config clang libclang-dev rustc cargo rustfmt \
  curl ca-certificates meson ninja-build patch xz-utils python3 \
  libavdevice-dev libavfilter-dev libavformat-dev libavcodec-dev \
  libswresample-dev libswscale-dev libavutil-dev \
  libfreetype-dev libharfbuzz-dev libfribidi-dev libfontconfig-dev \
  libpulse-dev libx11-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-0 \
  libxcursor1 libxi6 libxrandr2 \
  libvulkan-dev mesa-vulkan-drivers libegl1 libgl1-mesa-dri \
  fonts-dejavu-core pulseaudio-utils

bash scripts/build_linux_libass.sh
cargo build --locked -p linux_native_demo -p erika_capi
cargo run --locked -p linux_native_demo -- --wayland /path/to/video.mkv
```

Use `--x11` to select X11, or omit either flag for automatic selection. No media
argument displays a test pattern. Space pauses/resumes, arrows seek by five
seconds, and Esc exits. Build artifacts are in `target/debug`; add `--release`
for `target/release`. On WSL, set `CARGO_TARGET_DIR="$HOME/.cache/erika-target"`
to keep build output on the Linux filesystem. `pactl info` should find a server;
WSLg already provides one. A Vulkan/EGL driver is required; `/dev/dxg` alone
does not establish hardware acceleration (check `vulkaninfo --summary`).

### GPU rendering on WSLg

```bash
bash scripts/run_wsl_gpu.sh --wayland /path/to/video.mkv
# X11 is also supported:
bash scripts/run_wsl_gpu.sh --x11 /path/to/video.mkv
```

The launcher scopes `GALLIUM_DRIVER=d3d12`, `WGPU_BACKEND=gl`, and
`ERIKA_REQUIRE_HARDWARE_GPU=1` to Erika. Optionally select a card with
`MESA_D3D12_DEFAULT_ADAPTER_NAME=NVIDIA` (or AMD/Intel). WSL uses the Windows
GPU driver through `/dev/dxg`; a Linux NVIDIA kernel driver is not needed.
The pipeline is wgpu → EGL/OpenGL → Mesa D3D12 → Windows GPU driver.
Check the `adapter_selected` log with `displayBound: true` for the actual
window device; this machine reports `D3D12 (NVIDIA GeForce RTX 5070)` and
`backend: Gl`. Strict mode rejects CPU/software adapters instead of silently
falling back. Native Linux Vulkan remains available (`WGPU_BACKEND=vulkan`).

EGL is initialized on the actual host display and adapter selection checks
surface compatibility. Successful detach releases that display's EGL objects;
one CPU frame is retained so a paused image survives detach/reattach. NVDEC/VA-API
frames on the WSL OpenGL path are downloaded using FFmpeg and uploaded to wgpu.
This is hardware decode with a CPU transfer; statistics report these separately.

liberika retains WSL's D3D runtime until process exit to keep its thread-local
cleanup callbacks valid after EGL teardown. Audio uses the callback consumption
clock, consistent with the other ring-based outputs. WSLg/RDP device latency is
reported separately for draining/rate transitions; subtracting it from this
clock can starve the bounded audio prefetch window. End-device latency still
depends on the WSLg/RDP sink.

On WSLg, pause keeps the remote audio transport running with silence without
consuming the media ring. Resume continues media consumption without waiting
for the RDP sink to uncork. Ordinary Linux PulseAudio/PipeWire sessions still
cork on pause. PulseAudio cork/flush operations are submitted asynchronously.

Run `bash scripts/check_wsl_gpu.sh` for GPU readback/pixel tests, real audio,
X11/Wayland playback and C ABI lifecycle checks. Playback must reach at least
7.9 seconds of the 8-second fixture within 10 seconds, not merely issue draws.
The RTX 5070 passes all 27 wgpu tests, including readback and thread teardown.

## Integration

Linux defaults to CUDA/NVDEC, then VA-API. Set `ERIKA_HWDEC=cuda`, `vaapi`, or
`software` to select a backend. `ERIKA_CUDA_DEVICE=0` selects a CUDA ordinal;
`ERIKA_VAAPI_DEVICE=/dev/dri/renderD128` selects a render node. NVIDIA needs its
user-space CUDA/NVDEC libraries; Intel needs media-driver/iHD (or i965 on older
cards); AMD needs Mesa radeonsi VA-API. Supported codecs depend on the card and
driver. WSL Intel/AMD additionally depend on the Mesa D3D12 VA driver, not a
presumed `/dev/dri` node. This NVIDIA WSL machine has no `/dev/dri`.

Set `ERIKA_REQUIRE_HARDWARE_DECODE=1` to reject software fallback. This is separate
from the rendering adapter check `ERIKA_REQUIRE_HARDWARE_GPU=1`. Unsupported
videos fail explicitly in strict mode; otherwise transitions are logged.

For Flutter, build `erika_capi --release`, point `ERIKA_LIBRARY_DIR` at the folder
containing `liberika_capi.so`, and use a Linux Flutter SDK with a path dependency
on `packages/erika_flutter`. `ErikaVideoView` and `ErikaTextureVideoView` use GTK's
SDR pixel buffer texture. On Wayland, use `ErikaWindowOverlayVideoView` with a
transparent Flutter video rectangle to present video, subtitles, danmaku and HUD
directly beneath Flutter. The native path avoids per-frame RGBA readback, supports
opaque `srcOver` only, and does not provide Flutter clipping/color filters.
X11 retains the texture path. MPRIS is not implemented. Bundle matching headers
and library. Explicit screenshots still return tightly packed RGBA bytes.
The plugin saves, unbinds and restores GTK's EGL/GLX context around native calls
to avoid `EGL_BAD_ACCESS`. Set `ERIKA_DEBUG_HUD=1` to show the diagnostic HUD.
Installed plugins resolve sibling shared libraries through `$ORIGIN`.

### GPU frame import and HDR

The experimental Vulkan bridge maps VA-API frames through FFmpeg with
`AV_HWFRAME_MAP_DIRECT`, then samples the imported NV12/P010 VkImages directly.
This path allocates no destination plane and performs no CPU or GPU pixel copy
between decoder and shader. It requires matching device/format
support, external-memory/semaphore extensions, timeline semaphores and
synchronization2. Physical Intel/AMD import remains unverified. CUDA/Vulkan
transfer is rejected with FFmpeg 8: its import-failure cleanup can crash
([upstream fix](https://github.com/FFmpeg/FFmpeg/commit/c29d710cd5d0f80bbb56f7ec1f35d5fb7ed44d05)).
FFmpeg 9 contains the fix; rebuilding this integration with it still needs
validation. Ordinary NVDEC decode remains available with CPU plane transfer.

`ERIKA_REQUIRE_GPU_FRAMES=1` rejects any CPU decoded-plane upload.
`ERIKA_REQUIRE_ZERO_COPY=1` accepts only the direct VA-API path and rejects
software frames, failed imports, WSL D3D12 copies and CUDA GPU copies. The zero-copy counters
increase only after a successful direct import. Use native Wayland presentation
to also avoid Flutter's RGBA readback; explicit screenshots still read pixels.

Each imported picture retains its decoder reference until wgpu finishes using
both image aliases. Timeline waits and ownership/layout transitions precede
sampling; release returns the images to the external owner before recycling.
The vendored Vulkan HAL preserves the external image's initialized layout on
first use. Current frame replacement waits for outstanding GPU work; this is a
correctness-first implementation, not a throughput/latency acceptance result.

`bash scripts/check_linux_zero_copy.sh` enables test-only FFmpeg Vulkan fixtures.
It checks exact image-handle identity, NV12/P010 pixels against CPU-uploaded
reference images, repeated redraw/screenshot, semaphore advancement, unchanged
producer pixels after release, and teardown with submitted GPU work. This passed
on the RTX 5070 through isolated Mesa Dozen. The fixture is not a VA-API decoder;
physical Intel/AMD decode-to-display zero-copy remains unverified. NVIDIA NVDEC
direct zero-copy is not implemented; its CUDA transfer remains a separate copy path.

Set `ERIKA_LINUX_HDR=auto` to follow HDR/SDR source changes (`on` requests extended
linear output; `off` selects SDR). Only a Vulkan WSI that advertises
`R16G16B16A16_SFLOAT + EXTENDED_SRGB_LINEAR_EXT` is accepted for HDR. A 10-bit SDR
format alone is insufficient. The compositor combines the HDR video subsurface
with SDR Flutter UI and controls final display conversion. `ERIKA_REQUIRE_HDR=1`
rejects HDR sources on SDR-only surfaces or the RGBA8 Flutter texture path.
The default 12.5 scRGB headroom is a content target (1000/80 nits), not a measured
display capability; `activeHeadroomKnown` remains false without a real measurement.
HDR10+ and Dolby Vision passthrough are not implemented by this work.

On the tested WSL system, isolated Mesa Dozen 26.0.8 recognizes the RTX 5070 as a
Vulkan device. A local format-properties experiment reached CUDA external memory
import, which returned `CUDA_ERROR_NOT_SUPPORTED`. WSLg also did not expose HDR
color-management/WSI capabilities. This is a specific interop/display limitation;
hardware OpenGL and Vulkan rendering both work. The experimental Mesa build is
not a required dependency and is not installed by liberika's launcher. The optional
WSL GPU-copy bridge uses Mesa D3D12 VA-API instead of CUDA external-memory import;
see its [build steps, performance results and presentation limits](wsl-gpu-copy.md).

Rust consumers must enable the `wgpu` feature. See the
[native demo](../examples/linux_native_demo/src/main.rs). C consumers use
[`erika.h`](../crates/erika_capi/include/erika.h) and the shared library; see
[the C smoke example](../examples/linux_native_demo/capi_smoke.c).

For `erika_presenter_attach_wgpu_surface`, `XlibWindow` takes an X11 `Window` ID
and `Display*`; `WaylandSurface` takes `wl_surface*` and `wl_display*`. Cast
pointers through `uintptr_t` to `uint64_t`. Keep both host-owned handles alive
until detach or presenter destruction finishes. Attach/render/resize/detach on
the window event-loop thread, serialize calls per presenter, and drive
`render_tick` once per display frame. Width/height are physical pixels; scale is
an independent DPI factor. Resolve `.so` dependencies using rpath or
`LD_LIBRARY_PATH` and inspect them with `ldd`.

Linux uses system FFmpeg shared libraries and a static, PIC, **patched libass
0.17.5**, with system font libraries. The script verifies the upstream archive
SHA256 and applies liberika's ordered memory-font fallback patch. Stock libass
does not preserve that behavior. `ERIKA_LIBASS_DIR` can point to an equivalent
patched prefix. `ERIKA_FFMPEG_DIR` selects an existing static FFmpeg bundle;
`ERIKA_USE_SYSTEM_LIBS=0` preserves the original bundle lookup path but requires
all matching dependencies to be prepared separately. This guide validates the
system-library path and native GNU/Linux builds, not cross-compilation or musl.
The distro's FFmpeg codecs and licensing apply; `ERIKA_NATIVE_PROFILE=lgpl`
does not change its configuration. PulseAudio disconnections report an error;
automatic reconnection is not implemented.

## Validation

```bash
bash scripts/check_linux.sh
# Include Wayland when the display session provides it:
ERIKA_TEST_WAYLAND=1 bash scripts/check_linux.sh
```

The checks cover unit tests, real audio pause/resume, header alignment, playback
and a separately linked C ABI lifecycle test. A display and PulseAudio service
are required. Device versions and measured results are in the
[Linux validation records](investigations/linux-validation.en.md).
