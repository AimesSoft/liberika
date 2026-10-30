# Linux validation records

2026-09-29, Ubuntu 26.04 / WSLg, x86_64. The original versions, test counts and device details follow.

## Validation

```bash
# Needs X11 and a PulseAudio-compatible server.
bash scripts/check_linux.sh
# Also exercise Wayland when a compositor is available:
ERIKA_TEST_WAYLAND=1 bash scripts/check_linux.sh
```

This runs unit tests, the real audio pause/resume/reconfigure test, header
alignment, playback smoke tests, and a separately linked C executable covering
pause, seek, resize and teardown. The `Linux native` workflow uses an Ubuntu 26.04 container,
Xvfb, Mesa software rendering and a PulseAudio null sink; remote CI results must be checked after
submission. Local validation uses x86_64 Ubuntu 26.04 on WSLg, Rust 1.93.1,
FFmpeg 8.0.1, llvmpipe and an RTX 5070 through Mesa D3D12. X11, Wayland, the
C ABI pause/display recreation/seek/resize path, and rejection of CPU fallback
have passed locally. ARM64 and standalone desktop testing remain outstanding.
See the [Chinese guide](../linux.zh.md) for more detail.

The local acceptance record from 2026-09-29 used upstream baseline `97bbb84`:

| Check | Result |
|---|---|
| `erika` core unit tests | 657 passed; one audio integration test ignored by default |
| `erika_capi` unit tests | 41 passed |
| Real PulseAudio integration test enabled separately | 1 passed |
| C API / package header alignment | 5 passed; SHA256 of all three headers identical |
| X11 / Wayland video and audio smoke | Both passed; zero render and audio errors |
| Standalone C program linked to `.so`: pause / seek / resize / teardown | Passed; zero render and audio errors |
| Formatting, shell syntax, diff whitespace, dynamic dependency resolution | Passed; no missing dynamic libraries |
| RTX 5070 X11 / Wayland video and audio smoke | Both passed; actual adapter D3D12 RTX 5070; zero errors |
| RTX 5070 C ABI pause, close display and reattach, seek, resize | Passed; paused picture retained and audio resumed normally |
| Force llvmpipe under strict GPU mode | Startup rejected as expected |
| RTX 5070 GPU image readback / wgpu tests | 27 passed, including pixel comparison and thread exit |
| RTX 5070 NVDEC H.264 / HEVC Main10 / AV1 | All passed with strict hardware decoding; zero errors |
| Invalid VA-API device with strict hardware decode | Explicit failure; no software fallback |
| Native Flutter texture C API long pause (12.5 seconds) | 200 hardware frames, 0 software frames, 0 render/audio errors; audio clock continued after resume |
| Final focused presenter unit tests | 24 passed |
| NipaPlay 1.10.12 Linux release actual UI | Playback, resume after several minutes paused, seek, fullscreen enter/exit, Chinese subtitles and progress danmaku passed |
| NipaPlay raw RGBA screenshot and thumbnail saving | 427×240, 409920 bytes; saved successfully |
| Installed package dynamic linking and version | liberika resolved from the installation directory; SHA256 matched the release build |

The installed NipaPlay 1.10.12 release also passed Chinese SRT display, progress
danmaku, RGBA thumbnail capture, seeking, fullscreen transitions and resuming
after a pause of several minutes. The native texture C API test passed a
12.5-second pause with 200 hardware frames, no software frames and no audio or
render errors. The final focused presenter suite passed 24 tests. NVDEC H.264,
HEVC Main10 and AV1 fixtures passed; invalid VA-API devices fail in strict mode.
These results do not establish physical Intel/AMD or native desktop coverage.

After the native Wayland video-layer update, Rust and Flutter release builds also succeeded with NipaPlay 1.11.9 / Flutter
3.47.0-0.3.pre. Weston screenshots verify transparent Flutter controls over
video, Chinese SRT and progress danmaku. Pause, fullscreen enter/exit and
thumbnail capture worked in NipaPlay. A separate native-view probe completed
pause, resize, detach/reattach and seek on both OpenGL and experimental Dozen
Vulkan, preserving the paused picture. The updated checks passed 29 wgpu tests
(including two HDR negotiation tests), five API/header tests, C/C++ warnings as
errors, two strict CPU-frame rejection cases, and C ABI lifecycle playback
(316 video frames; zero render/audio errors). Remote audio underflow counters
in the full NipaPlay session were nonzero; these checks do not establish smooth
native-desktop audio, physical HDR output, or decoder zero-copy.

## Environment notes from the original integration guide

Linux supports source builds of the Rust engine and `liberika_capi.so`, with
NVDEC/VA-API hardware decoding, FFmpeg software decoding, wgpu SDR presentation
on X11/Wayland, a Linux Flutter texture plugin, subtitles/danmaku and PulseAudio
output (including PipeWire-Pulse and WSLg). Wayland also has a native video
subsurface beneath transparent Flutter UI. HDR surface negotiation is implemented;
physical HDR output and direct zero-copy have not passed hardware validation.
Prebuilt Linux releases are not available yet.
X11 currently uses screen 0. The native ARM64 build path was prepared, but local acceptance covered only x86_64.
WSLg hardware rendering on the RTX 5070 is validated via Mesa D3D12 / OpenGL;
NVDEC H.264, HEVC 10-bit and AV1 decoding have also passed real-device playback.
Intel/AMD VA-API is implemented but has not been tested on those physical cards.
An opt-in [WSL D3D12 GPU-copy bridge](../wsl-gpu-copy.md) now removes decoded-frame
CPU transfers on this RTX 5070, with measured 4K60 Main10 playback. A GPU copy remains, so this is not end-to-end zero-copy; Dozen's Linux presentation still uses software WSI. The linked guide contains build steps and full acceptance limits.
