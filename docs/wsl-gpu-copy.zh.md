# WSL D3D12 解码帧导入（实验性）

> [中文](wsl-gpu-copy.zh.md) · [English](wsl-gpu-copy.md) · [日本語](wsl-gpu-copy.ja.md)

这一可选路径消除了 WSL 上**解码后的 NV12/P010 帧**经 CPU 内存往返：Mesa D3D12 VA-API 解码，D3D12 将两个平面复制到可复用 GPU 纹理，Dozen 将纹理导入 Vulkan 供 wgpu 使用。P010 保留 10-bit 精度，不使用 `av_hwframe_transfer_data` 或 CPU 平面上传。

这是 **GPU 复制**，并非直接零拷贝。Dozen 26.0.8 尚不符合规范，Linux 上使用 software WSI（`dzn_wsi.c` 设置 `sw_device = true`），因此窗口呈现仍可能读回渲染结果。WSLg/RDP 与合成器的复制不计入 liberika 的解码帧计数器。该路径未证明严格的端到端零拷贝或实际 HDR 输出。

## 在 Ubuntu 26.04 / WSLg x86_64 上复现

先安装常规 [Linux 构建依赖](linux.zh.md)，再安装 Mesa/桥接依赖：

```bash
sudo apt install directx-headers-dev libva-dev libdrm-dev \
  libxcb-dri3-dev libxcb-present-dev libxcb-xfixes0-dev libxcb-sync-dev \
  libxshmfence-dev libxext-dev libxfixes-dev libxxf86vm-dev \
  libwayland-dev wayland-protocols python3-mako python3-yaml \
  bison flex zlib1g-dev libzstd-dev

# 在 Linux 文件系统中使用解压后的上游 Mesa 26.0.8 源码。
bash scripts/build_wsl_interop_mesa.sh /path/to/mesa-26.0.8
export CARGO_TARGET_DIR="$HOME/.cache/erika-target"
ERIKA_LINUX_D3D12_INTEROP=1 cargo build --locked --release -p erika_capi

# 包装脚本可运行任意命令；原生呈现可避免 Flutter 额外的 RGBA 像素缓冲读回。
ERIKA_LINUX_D3D12_INTEROP=1 bash scripts/run_wsl_gpu_copy.sh \
  cargo run --locked --release -p linux_native_demo -- --wayland /path/to/video.mp4
```

脚本安装到 `~/.local/opt/erika-wsl-mesa`，保留系统 Mesa。可用第二个构建参数更改前缀；运行时将 `ERIKA_WSL_MESA_PREFIX` 设置为相同值。包装脚本仅为启动的应用设置驱动变量，并要求硬件渲染、硬件解码和成功的 GPU 解码帧导入，不会静默回退到下载解码帧。

`ERIKA_REQUIRE_ZERO_COPY=1` 会有意拒绝这条复制路径。独立的原生 Linux VA-API/DRM 直接导入实现不受影响。Intel/AMD 实卡验收和原生 Linux NVIDIA 直接 NVDEC 导入仍待完成。支持的编解码器取决于实际硬件和 VA 驱动。

## 同步与生命周期

- 将 Vulkan 设备 LUID 与 DXCore 适配器匹配。仅接受 NV12/P010、单个不透明资源 FD 的 Mesa D3D12 VA 表面；这些 FD **不是 DMA-BUF**。
- 等待 VA 解码完成，在 GPU 上复制平面，等待 D3D12 fence，并在着色器采样前取得 Vulkan 所有权。
- 保留导入纹理直到 wgpu 完成，再释放外部所有权并回收到小型池；尺寸或格式变化时更换池。
- 提交失败且完成状态未知时，桥接进入不可复用状态，并保留引用的资源直到进程退出，避免回收仍在使用的内存。

Mesa 补丁启用可共享视频资源与 P010 资源导出，提供 FFmpeg 所需的 Dozen format-properties2 支持，并防止 `vaSyncSurface` 在解码器记录另一帧时重置分配器。这是本地实验补丁，并非上游 Mesa 版本。

没有 R16/RG16 UNORM 的适配器上，普通 CPU 回退也保留 10-bit 精度：通过 RG8/RGBA8 上传原始字节对并在着色器中重建（包括 ArtCNN）。该回退仍经 CPU 内存传输解码帧，性能上不能替代 GPU 导入。

## 测量与验收，2026-09-29

RTX 5070、驱动 595.79、WSLg、FFmpeg 8.0.1、Mesa 26.0.8。素材为 25 秒 3840×2160 HEVC Main10、60 fps、约 35 Mbps、AAC 48 kHz。C ABI 基准以 1920×1080 显示，最高 120 Hz 轮询，统计不同源帧的导入，不把重复绘制视为新帧。

| 16 秒样本 | OpenGL/NVDEC CPU 传输基线 | WSL GPU 平面复制 |
|---|---:|---:|
| 硬件源帧 | 960 | 960 |
| 源帧/秒 | 60.011 | 60.027 |
| 解码帧 CPU 传输 | 960 | 0 |
| Tick P99 | 17.954 ms | 4.149 ms |
| 最大 tick | 25.649 ms | 6.498 ms |
| 超过 16.67 ms 的 tick | 21 | 0 |
| 测量区间音频欠载帧 | 0 | 0 |

旧基线将 P010 降为 8-bit；GPU 复制保留 10-bit 输入。这些是本机样本，不能保证其他硬件、输出分辨率或负载获得相同结果。Tick 时间包含导入和渲染提交，不衡量 Windows 显示延迟或物理扫描输出。

构建共享库后复现：

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

真实 VA 解码像素比较覆盖连续 48 帧 P010 和 NV12，包括池复用、重复读回与末帧销毁，均通过。CPU 参考路径仅用于测试。C ABI 生命周期测试通过了暂停、显示 detach/reattach、跳转、缩放和销毁。着色器修改还移除了当前 SPIR-V 校验报告的无效 Function 存储布局修饰；后续真实播放检查未报告 Vulkan 校验错误。

NipaPlay 1.11.9 / Flutter 3.47.0-0.3.pre 加载已部署桥接后报告 VA-API、成功导入 GPU 共享帧、解码帧 CPU 回退为零。同一 RTX 的嵌套 Weston GL 会话中，截图确认了 4K Main10 视频、进度弹幕、暂停跳转、接近 59.9 fps 的恢复播放、全屏进出及画面保持；缩略图捕获成功。完整应用会话中音频欠载计数非零，C 基准区间的零欠载不能证明 NipaPlay 音频连续无中断。
