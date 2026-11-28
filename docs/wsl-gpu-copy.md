# WSL D3D12 decoded-frame import (experimental)

> [中文](wsl-gpu-copy.zh.md) · [English](wsl-gpu-copy.md) · [日本語](wsl-gpu-copy.ja.md)

This optional path removes the **decoded NV12/P010 frame** round trip through
CPU memory on WSL. Mesa D3D12 VA-API decodes, D3D12 copies the two planes into
reusable GPU textures, and Dozen imports those textures into Vulkan for wgpu.
P010 remains 10-bit. No `av_hwframe_transfer_data` or CPU plane upload is used.

This is **GPU copy**, not direct zero-copy. Dozen 26.0.8 is non-conformant and
uses software WSI on Linux (`dzn_wsi.c` sets `sw_device = true`). Window
presentation can therefore still read back the rendered output. WSLg/RDP and
compositor copies are outside liberika's decoded-frame counters. Neither strict
end-to-end zero-copy nor physical HDR output is established by this path.

## Reproduce on Ubuntu 26.04 / WSLg x86_64

First install the ordinary [Linux build dependencies](linux.md). Additional
Mesa/bridge dependencies:

```bash
sudo apt install directx-headers-dev libva-dev libdrm-dev \
  libxcb-dri3-dev libxcb-present-dev libxcb-xfixes0-dev libxcb-sync-dev \
  libxshmfence-dev libxext-dev libxfixes-dev libxxf86vm-dev \
  libwayland-dev wayland-protocols python3-mako python3-yaml \
  bison flex zlib1g-dev libzstd-dev

# Use an unpacked upstream Mesa 26.0.8 source tree on the Linux filesystem.
bash scripts/build_wsl_interop_mesa.sh /path/to/mesa-26.0.8
export CARGO_TARGET_DIR="$HOME/.cache/erika-target"
ERIKA_LINUX_D3D12_INTEROP=1 cargo build --locked --release -p erika_capi

# The wrapper accepts any command; native presentation is needed to avoid
# Flutter's additional RGBA pixel-buffer readback.
ERIKA_LINUX_D3D12_INTEROP=1 bash scripts/run_wsl_gpu_copy.sh \
  cargo run --locked --release -p linux_native_demo -- --wayland /path/to/video.mp4
```

The script installs into `~/.local/opt/erika-wsl-mesa`, leaving system Mesa in
place. An optional second build argument changes the prefix; set
`ERIKA_WSL_MESA_PREFIX` to the same value at run time. The wrapper scopes the
driver variables to the launched application and requires hardware rendering,
hardware decoding, and successful GPU decoded-frame import. It never silently
falls back to downloading decoded frames.

`ERIKA_REQUIRE_ZERO_COPY=1` deliberately rejects this copy path. The separate
native-Linux VA-API/DRM direct-import implementation is unchanged. Intel/AMD
physical-card acceptance and native Linux NVIDIA direct NVDEC import remain
outstanding. Codec support depends on the actual hardware/VA driver.

## Synchronization and lifetime

- Match the Vulkan device LUID to a DXCore adapter. Accept only Mesa D3D12
  VA surfaces with NV12/P010 and a single opaque resource FD. Those FDs are
  **not DMA-BUFs**.
- Wait for VA decode completion, copy the planes on the GPU, wait for the
  D3D12 fence, and acquire Vulkan ownership before shader sampling.
- Retain imported textures until wgpu completes, then release external
  ownership before recycling a small pool. Resize/format changes replace it.
- Failed submission with unknown completion poisons the bridge and retains
  referenced resources until process exit instead of recycling live memory.

The Mesa patch enables shareable video resources and P010 resource export,
provides Dozen format-properties2 support needed by FFmpeg, and prevents
`vaSyncSurface` from resetting an allocator while the decoder records another
picture. It is a local experimental patch, not an upstream Mesa release.

The ordinary CPU fallback also preserves 10-bit precision on adapters without
R16/RG16 UNORM: upload original byte pairs through RG8/RGBA8 and reconstruct
them in the shader (including ArtCNN). This fallback still transfers decoded
frames through CPU memory and is not a performance substitute for GPU import.

## Measurements and acceptance, 2026-09-29

RTX 5070, driver 595.79, WSLg, FFmpeg 8.0.1, Mesa 26.0.8. Fixture: 25-second
3840x2160 HEVC Main10, 60 fps, approximately 35 Mbps, AAC 48 kHz. The C ABI
benchmark displays at 1920x1080, polls at up to 120 Hz, and measures distinct
source-frame imports rather than counting repeated draws as new frames.

| 16-second sample | OpenGL/NVDEC CPU transfer baseline | WSL GPU plane copy |
|---|---:|---:|
| Hardware source frames | 960 | 960 |
| Source frames/s | 60.011 | 60.027 |
| Decoded-frame CPU transfers | 960 | 0 |
| Tick P99 | 17.954 ms | 4.149 ms |
| Maximum tick | 25.649 ms | 6.498 ms |
| Ticks over 16.67 ms | 21 | 0 |
| Audio underflow frames in measured interval | 0 | 0 |

The old baseline downconverted P010 to 8-bit; the GPU-copy result preserves
10-bit input. These are local samples, not guarantees across hardware, output
resolutions or workloads. Tick duration includes import/render submission and
does not measure Windows display latency or physical scanout.

Reproduce after building the shared library:

```bash
cc -std=c11 -O2 -Wall -Wextra -Werror \
  -Icrates/erika_capi/include examples/linux_native_demo/capi_performance.c \
  -L"$CARGO_TARGET_DIR/release" -lerika_capi -lX11 -o /tmp/erika-performance
LD_LIBRARY_PATH="$CARGO_TARGET_DIR/release" bash scripts/run_wsl_gpu_copy.sh \
  /tmp/erika-performance /path/to/4k60-p010.mp4 5 16

ERIKA_LINUX_D3D12_INTEROP=1 ERIKA_WSL_TEST_VIDEO=/path/to/video-stream-0.mp4 \
  bash scripts/run_wsl_gpu_copy.sh cargo test --locked -p erika --features wgpu \
  --lib linux_wsl_gpu_copy_matches_decoded_pixels -- --ignored --nocapture
```

Real VA decode pixel comparisons passed for 48 consecutive P010 and NV12
frames, including pool reuse, repeated readback and final-frame teardown. The
explicit CPU reference is confined to the test. The C ABI lifecycle test passed
pause, display detach/reattach, seek, resize and teardown. Shader changes also
remove invalid Function-storage layout decorations reported by current SPIR-V
validation; a subsequent real playback check reported no Vulkan validation errors.

NipaPlay 1.11.9 / Flutter 3.47.0-0.3.pre loaded the deployed bridge, reported
VA-API, successful GPU shared-frame imports and zero decoded-frame CPU fallback.
In a nested Weston GL session on the same RTX, screenshots confirmed 4K Main10
video, progress danmaku, paused seek, resumed playback near 59.9 fps, fullscreen
enter/exit and preserved picture. Thumbnail capture succeeded. Audio underflow
counts in the full application were nonzero; the C benchmark's zero-underflow
interval does not establish uninterrupted NipaPlay audio.

## 中文摘要

已实现 WSL 下 VA-API/D3D12 解码 → GPU 平面复制 → Vulkan 采样，避免每帧把
4K NV12/P010 解码平面下载到 CPU 再上传，并保留 10-bit。此路径需要显式启用
`ERIKA_LINUX_D3D12_INTEROP=1` 编译，使用上面的隔离 Mesa 构建和运行脚本。
`ERIKA_REQUIRE_GPU_FRAMES=1` 禁止解码帧 CPU 回退；严格零拷贝开关会拒绝它。

**不能称为端到端零拷贝**：GPU 内仍复制一次，Dozen 的 Linux 窗口呈现仍采用
software WSI，输出和 WSLg/RDP 合成也可能复制。CPU fallback 计数只统计解码
帧导入，不能覆盖整个显示链路。当前 WSLg 不提供所需 HDR 输出能力；Intel/AMD
实卡与原生 Linux NVIDIA 直接零拷贝也尚未验收。

表格是原生 C ABI 窗口的本机样本；实际 NipaPlay 已完成画面、暂停跳转、恢复、
全屏与缩略图检查，但整个会话有音频欠载计数，不能宣称所有场景均已无卡顿。
