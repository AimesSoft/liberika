# Linux 验证记录

2026-09-29，Ubuntu 26.04 / WSLg，x86_64。原记录中的版本、测试数与设备信息保留如下。

## 5. 验证

在有 X11 与音频服务的会话中：

```bash
bash scripts/check_linux.sh
# 若同时有 Wayland（例如 WSLg），也验证 Wayland 播放：
ERIKA_TEST_WAYLAND=1 bash scripts/check_linux.sh
```

脚本验证核心/C ABI 单元测试、真实音频暂停恢复与重配置、头文件对齐、X11 播放、
可选 Wayland 播放，并编译独立 C 程序加载 `.so` 检查播放、暂停、seek、resize 与销毁。
新增的 `Linux native` workflow 在 Ubuntu 26.04 容器中使用 Xvfb、Mesa 软件渲染与
PulseAudio null sink 运行 X11 路径；提交后需以实际 CI 结果为准。

本地验收环境：Ubuntu 26.04 / WSL2 / WSLg，x86_64，Rust 1.93.1，FFmpeg 8.0.1，
Mesa llvmpipe 以及 Mesa D3D12 / NVIDIA GeForce RTX 5070。独立 Linux 桌面与 ARM64 仍需另行验收。

2026-09-29 本地验证记录（上游基线 `97bbb84`）：

| 检查 | 结果 |
|---|---|
| `erika` 核心单元测试 | 657 通过，1 项音频集成测试默认忽略 |
| `erika_capi` 单元测试 | 41 通过 |
| 单独启用真实 PulseAudio 集成测试 | 1 通过 |
| C API / package header 对齐 | 5 通过，三份头文件 SHA256 一致 |
| X11 / Wayland 视频与音频 smoke | 均通过，渲染和音频错误数均为 0 |
| 独立 C 程序链接 `.so`，暂停/seek/resize/销毁 | 通过，渲染和音频错误数均为 0 |
| 格式、shell 语法、diff 空白、动态依赖解析 | 通过，无缺失动态库 |
| RTX 5070 X11 / Wayland 视频与音频 smoke | 均通过，实际 adapter 为 D3D12 RTX 5070，错误数均为 0 |
| RTX 5070 C ABI 暂停、关闭 display 后重新 attach、seek、resize | 通过，暂停画面保留，音频恢复正常 |
| GPU 严格模式下强制 llvmpipe | 按预期拒绝启动 |
| RTX 5070 GPU 图像读回 / wgpu 测试 | 27 通过，包含像素值比较与线程退出 |
| RTX 5070 NVDEC H.264 / HEVC Main10 / AV1 | 均通过，严格硬解；错误数均为 0 |
| 无效 VA-API 设备 + 严格硬解模式 | 明确失败，未回退软解 |
| 原生 Flutter texture C API 长暂停（12.5 秒） | 200 硬解帧、0 软解帧、0 渲染/音频错误，恢复后音频时钟继续推进 |
| 最终 presenter 定向单元测试 | 24 通过 |
| NipaPlay 1.10.12 Linux release 实际 UI | 播放、数分钟暂停后恢复、seek、全屏进入/退出、中文字幕和进度弹幕通过 |
| NipaPlay 原始 RGBA 截图及缩略图保存 | 427×240、409920 字节，保存成功 |
| 安装包动态链接与版本 | 从安装目录解析 liberika，SHA256 与 release 构建一致 |

原生 Wayland 视频层更新后另行验证：NipaPlay 1.11.9 / Flutter 3.47.0-0.3.pre
的 Rust 与 Flutter release 构建成功，Weston 截图确认透明控件、视频、中文字幕及
进度弹幕正确合成；暂停、全屏进入/退出和缩略图捕获通过。独立原生层 probe 在
OpenGL 与实验性 Dozen Vulkan 均完成暂停、缩放、detach/reattach、seek，暂停画面保留。
最终定向检查：29 项 wgpu 测试（含 2 项 HDR 协商）、5 项 API/头文件测试、C/C++
严格警告检查、2 项禁止 CPU 帧绕过严格模式的测试均通过；C ABI 生命周期播放
得到 316 帧、0 渲染/音频错误。完整 NipaPlay 会话的远端音频 underflow 计数非零，
尚不能据此保证原生 Linux 桌面的音频流畅度。上述结果不构成物理 HDR 或解码零拷贝验收。

## 原接入指南的环境说明

Linux 现在可以从源码构建 Rust 播放器与 `liberika_capi.so`，通过 X11 或 Wayland
显示视频，通过 PulseAudio 输出音频。PipeWire 桌面需启用 `pipewire-pulse`；WSL2
使用 WSLg 提供的显示与音频服务。

当前范围：NVDEC/VA-API 硬件解码、FFmpeg 软件解码、wgpu 呈现、Linux Flutter
纹理插件及 Wayland 原生视频层、字幕/弹幕、音频播放、暂停/恢复、seek、窗口缩放。
已实现 HDR 表面协商，但真实显示器 HDR 输出与直接零拷贝尚未验收通过。
尚未提供 Linux 预编译发布包。
X11 当前使用 screen 0。原生 ARM64 构建路径已准备，但验收仅覆盖 x86_64。
WSLg 的 RTX 5070 已实测 NVDEC（H.264、HEVC 10-bit、AV1）与 Mesa D3D12 / OpenGL 渲染。
Intel/AMD 的 VA-API 路径已实现，尚无对应显卡的实机验收。
新增可选 [WSL D3D12 GPU 复制路径](../wsl-gpu-copy.md)：本机 RTX 5070 已实测
4K60 Main10，解码帧 CPU 往返为零。GPU 内仍有复制，Dozen 的 Linux 窗口输出
仍用 software WSI，不能称为端到端零拷贝。链接包含构建步骤及完整验收边界。
