# Linux 原生接入（实验性）

> [中文](linux.zh.md) · [English](linux.md) · [日本語](linux.ja.md)

Linux 支持 Rust 播放器、C ABI 与 Flutter，使用系统 FFmpeg 8.x、liberika 补丁 libass、wgpu 和 PulseAudio。PipeWire 桌面需启用 `pipewire-pulse`；WSL2 使用 WSLg 的显示与音频服务。

视频可通过 X11 / Wayland 窗口呈现；Flutter 可选 RGBA SDR 纹理或 Wayland 原生视频层。NVIDIA 使用 NVDEC，Intel/AMD 使用 VA-API，也可使用 FFmpeg 软件解码。Linux 当前从源码构建。

HDR 表面协商和 VA-API 直接导入为实验性路径。[WSL D3D12 GPU 复制](wsl-gpu-copy.md)提供另一种解码帧传输方式。设备、性能与验证结果保存在 [Linux 验证记录](investigations/linux-validation.zh.md)。

## 1. 安装依赖

以下命令适用于 **Ubuntu 26.04**。需要 Rust 1.92+ 和 **FFmpeg 8.x** 开发包；
Ubuntu 22.04/24.04 默认仓库中的旧版 FFmpeg 不满足此构建路径。

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config clang libclang-dev rustc cargo rustfmt \
  curl ca-certificates meson ninja-build patch xz-utils python3 \
  libavdevice-dev libavfilter-dev libavformat-dev libavcodec-dev \
  libswresample-dev libswscale-dev libavutil-dev \
  libfreetype-dev libharfbuzz-dev libfribidi-dev libfontconfig-dev \
  libpulse-dev libx11-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-0 \
  libvulkan-dev mesa-vulkan-drivers libegl1 libgl1-mesa-dri \
  fonts-dejavu-core pulseaudio-utils
```

桌面环境应有可用的 Vulkan 或 EGL 驱动。`pactl info` 可检查音频服务。
WSLg 已提供 PulseAudio 服务，无需再启动另一个服务。

## 2. 构建与运行

在仓库根目录运行：

```bash
# 下载并校验固定版本 libass，应用 Erika 字体回退补丁，再构建静态 PIC 库。
bash scripts/build_linux_libass.sh
cargo build --locked -p linux_native_demo -p erika_capi

# 自动选择显示协议；也可明确传入 --x11 或 --wayland。
cargo run --locked -p linux_native_demo -- /path/to/video.mkv
```

不传视频路径时显示测试图案。空格暂停/继续，左右箭头前后跳转 5 秒，Esc 退出。
输出文件为 `target/debug/linux_native_demo` 与 `target/debug/liberika_capi.so`；
发布构建加 `--release`，输出目录改为 `target/release`。

WSL 中仓库若位于 `/mnt/c`，建议把编译缓存放在 Linux 文件系统：

```bash
export CARGO_TARGET_DIR="$HOME/.cache/erika-target"
cargo run --locked -p linux_native_demo -- --wayland /mnt/c/path/to/video.mkv
```

这时所有产物位于 `$CARGO_TARGET_DIR/debug`。Windows 不能直接加载 Linux `.so`。
WSL 中可有 `/dev/dxg` 却仍使用 Mesa llvmpipe 软件渲染；请以 `vulkaninfo --summary`
或 `glxinfo -B` 显示的实际设备为准。

### WSLg 使用 GPU

在本机 Ubuntu 中运行：

```bash
bash scripts/run_wsl_gpu.sh --wayland /mnt/c/path/to/video.mkv
# X11 也已验证；不传视频文件时显示测试图案。
bash scripts/run_wsl_gpu.sh --x11
```

该启动脚本将 `GALLIUM_DRIVER=d3d12`、`WGPU_BACKEND=gl` 和
`ERIKA_REQUIRE_HARDWARE_GPU=1` 仅应用于 liberika 进程。多 GPU 机器可额外设置
`MESA_D3D12_DEFAULT_ADAPTER_NAME=NVIDIA`（也可指定 AMD / Intel）。
无需在 WSL 中安装 Linux NVIDIA 内核驱动；WSL 使用 Windows 驱动提供的 `/dev/dxg`。

GPU 路径为 `wgpu → OpenGL/EGL → Mesa D3D12 → Windows 显卡驱动`。
启动日志应显示 `name: D3D12 (NVIDIA GeForce RTX 5070)`、`backend: Gl`、
`hardware: true`；`displayBound: true` 表示窗口使用的实际适配器。
严格模式会拒绝 llvmpipe、CPU 适配器或 Microsoft Basic Render Driver，避免无声回退。
Linux 原生 Vulkan 仍使用默认路径，亦可通过 `WGPU_BACKEND=vulkan` 显式选择。

EGL 初始化绑定宿主的实际 display，并在 adapter 选择时检查窗口兼容性。
detach 成功时释放所有借用该 display 的 EGL 对象；保留一帧 CPU 像素以便暂停时重建窗口。
WSL OpenGL 路径中 NVDEC/VA-API 解码后的帧经 `av_hwframe_transfer_data` 下载，再上传到 wgpu。
该路径是硬件解码，但不是零拷贝；统计中的 `hardwareVideoFrames` 与
`cpuVideoFrameFallbacks` 会同时增加，`softwareVideoFrames` 不会因此增加。

WSL D3D 运行库会在线程退出时执行 TLS 清理；liberika 保留其加载引用直到进程退出，
避免 EGL 销毁后清理函数指向已卸载的库。音频同步采用回调消费时钟，与其它 ring-buffer
输出后端一致；WSLg/RDP 的额外设备延迟单独用于排空/倍速切换，不从此时钟扣除，
避免超过引擎的音频预取窗口而反复缓冲。远端音频设备的实际延迟仍取决于 WSLg/RDP。

WSLg 暂停时保留音频传输并输出静音，不消耗媒体 ring；恢复时直接继续消费媒体，
避免 RDP sink 的 uncork 延迟阻塞播放器。普通 Linux PulseAudio/PipeWire 仍使用 cork。
PulseAudio cork/flush 操作异步提交，不在播放器线程等待服务端回调。

完整 GPU 回归检查（需要 WSLg 的 X11、Wayland 和音频服务）：

```bash
bash scripts/check_wsl_gpu.sh
```

这会在 GPU 上执行图像读回测试、音频测试、两种窗口协议的完整 8 秒媒体播放，
并检查 C ABI 的暂停、显示连接重建、seek 与 resize。播放检查要求 10 秒内到达至少
7.9 秒媒体时间，避免只检测到渲染调用就把卡住的播放判定为成功。

## 硬件解码与显卡驱动

| 显卡 | liberika 后端 | Linux 驱动要求 |
|---|---|---|
| NVIDIA | `cuda` / NVDEC | 支持该显卡的 NVIDIA 用户态驱动、`libcuda.so.1`、`libnvcuvid.so.1` |
| Intel | `vaapi` | Intel media-driver (`iHD`)，旧显卡可使用 i965；可访问 `/dev/dri/renderD*` |
| AMD | `vaapi` | Mesa radeonsi VA-API 与可访问的 `/dev/dri/renderD*` |

默认先尝试 CUDA，再尝试 VA-API；驱动或视频格式不支持时会发出明确回退事件。
显卡代际、驱动和 FFmpeg 编译选项决定具体支持的编码、位深和分辨率。
WSL 中 Intel/AMD 的 VA-API 取决于 Mesa D3D12 VA 驱动；不能把普通 Linux 的
`/dev/dri` 路径当作 WSL 已提供的设备。本机只有 NVIDIA，且没有 `/dev/dri`。

```bash
# 要求硬解，失败即报错；不会静默使用 CPU 解码。
export ERIKA_REQUIRE_HARDWARE_DECODE=1
export ERIKA_HWDEC=auto       # auto（默认）、cuda/nvdec、vaapi、software
# 多显卡可选：
export ERIKA_CUDA_DEVICE=0
export ERIKA_VAAPI_DEVICE=/dev/dri/renderD128
```

`ERIKA_REQUIRE_HARDWARE_GPU` 管渲染设备，`ERIKA_REQUIRE_HARDWARE_DECODE` 管解码器，
两者独立。开启后者时，显卡不支持的视频会明确失败。关闭后可按日志回退软解。

## Flutter Linux 接入

使用 Linux Flutter SDK 构建宿主，依赖本地 `packages/erika_flutter`，并设置：

```bash
cargo build --locked --release -p erika_capi
export ERIKA_LIBRARY_DIR="${CARGO_TARGET_DIR:-$PWD/target}/release"
# 在 Flutter 宿主目录：
flutter build linux --release
```

插件通过 GTK `FlPixelBufferTexture` 将 GPU 合成的视频、字幕、弹幕与 HUD 交给 Flutter。
`ErikaVideoView` 和 `ErikaTextureVideoView` 均可用于 Linux；纹理支持 Flutter 遮罩与 UI 叠层。
这条兼容路径会从 GPU 读回 RGBA，4K/高帧率有额外带宽开销。Wayland 可显式使用
`ErikaWindowOverlayVideoView`：视频通过 `wl_subsurface` 放在透明 Flutter UI 下，
由 Linux 合成器合成，不经过 Flutter 的逐帧 RGBA8 回读。宿主须将视频区域留透明；
原生层仅支持不透明 `srcOver`，不支持 Flutter 对视频的裁剪或颜色滤镜。X11 继续使用纹理。
MPRIS 系统媒体控制尚未实现。显式截图仍返回紧密排列的 RGBA 像素。播放、暂停、seek、音轨/字幕切换和弹幕
走同一 C ABI，插件必须与同次构建的头文件和 `.so` 一起使用。
插件在原生调用前后保存、解绑和恢复 GTK 的 EGL/GLX 上下文，避免 `EGL_BAD_ACCESS`。
`ERIKA_DEBUG_HUD=1` 可开启诊断 HUD；安装库使用 `$ORIGIN` 查找同目录依赖。

### GPU 帧导入与 HDR 的实际范围

实验性 Vulkan 桥通过 FFmpeg 的 `AV_HWFRAME_MAP_DIRECT` 映射 VA-API 帧，
直接采样导入的 NV12/P010 VkImage；解码帧到着色器之间不分配目标平面，
也不执行 CPU/GPU 像素复制。映射持有解码帧引用直到 wgpu 完成使用，
通过 timeline semaphore 和布局/所有权转换完成获取和释放；首次使用保留外部图像内容。
需要外部显存/信号量、timeline semaphore、synchronization2 及兼容的设备/格式。
Intel/AMD 实卡导入尚未验收。FFmpeg 8 的 CUDA→Vulkan 失败清理会崩溃，因此该组合
在调用前被拒绝；普通 NVDEC 硬解仍可通过 CPU 平面传递播放。
[FFmpeg 上游修复](https://github.com/FFmpeg/FFmpeg/commit/c29d710cd5d0f80bbb56f7ec1f35d5fb7ed44d05)
已进入 FFmpeg 9，但本集成使用该版本重建仍需验证。

- `ERIKA_REQUIRE_GPU_FRAMES=1`：任何需要 CPU 上传的解码帧都报错。
- `ERIKA_REQUIRE_ZERO_COPY=1`：只接受成功的 VA-API 直接导入；软解、导入失败、WSL D3D12 与 CUDA GPU copy 都报错。
- `ERIKA_LINUX_HDR=auto`：跟随 HDR/SDR 片源；`on` 请求扩展线性输出，`off` 选择 SDR。
- `ERIKA_REQUIRE_HDR=1`：HDR 片源遇到 SDR 表面或 Flutter RGBA8 路径时报错。

`bash scripts/check_linux_zero_copy.sh` 使用测试专用的 FFmpeg Vulkan 图像，
检查图像句柄相同、NV12/P010 与参考图逐像素一致、重复重绘/截图、信号量推进、
释放后源像素不变，以及有 GPU 提交时退出。该测试已在 RTX 5070 的隔离 Dozen
驱动上通过，但它不使用 VA-API 解码器，不能替代 Intel/AMD 实卡的端到端验收。
当前换帧会等待上一帧 GPU 使用完成，吞吐/延迟尚未验收。
NVIDIA NVDEC 直接零拷贝仍未实现；CUDA 传递继续按独立的复制路径处理。
原生 Wayland 展示可避免 Flutter 的逐帧 RGBA 回读，主动截图仍会读取像素。

HDR 使用原生视频层的 FP16 scRGB；仅接受 Vulkan WSI 实际提供的
`R16G16B16A16_SFLOAT + EXTENDED_SRGB_LINEAR_EXT`，仅有 10-bit SDR 格式不算 HDR。
合成器负责将 HDR 视频与 SDR Flutter UI 合成并转换到显示器输出。
默认 12.5 headroom 是 1000/80 nit 内容目标，不是屏幕亮度测量；没有真实测量时
`activeHeadroomKnown` 仍为 false。本实现不含 HDR10+ 或 Dolby Vision 透传。

本机隔离构建的 Mesa Dozen 26.0.8 已将 RTX 5070 识别为 Vulkan 硬件设备。
本地格式查询实验进一步到达 CUDA 外部显存导入，但驱动返回 `CUDA_ERROR_NOT_SUPPORTED`。
当前 WSLg 也未提供 HDR 颜色管理/WSI 能力。这不影响已实测的 OpenGL/Vulkan 硬件渲染；
限制发生在互操作与显示链路。实验性 Mesa 构建不是发布依赖，启动脚本不会自动安装它。

## 3. Rust 与 C/C++ 接入

Rust 依赖需要启用 `erika` 的 `wgpu` feature，参考
[`examples/linux_native_demo/src/main.rs`](../examples/linux_native_demo/src/main.rs)。
C 接入使用 [`erika.h`](../crates/erika_capi/include/erika.h)，默认 C ABI 构建已启用 wgpu。
动态加载时需让加载器找到 `.so` 与当前发行版的 FFmpeg、PulseAudio、字体库依赖；
可配置 rpath 或 `LD_LIBRARY_PATH`，并用 `ldd` 检查。

`erika_presenter_attach_wgpu_surface` 参数映射如下：

| surface kind | raw_window | raw_display |
|---|---|---|
| `ErikaWgpuSurfaceKind_XlibWindow` | X11 `Window` 整数 ID | `Display*` |
| `ErikaWgpuSurfaceKind_WaylandSurface` | `wl_surface*` | `wl_display*` |

指针经 `uintptr_t` 转为 `uint64_t`。窗口与 display 由宿主持有，必须在
`detach_surface` 或 presenter 销毁完成后才能释放；窗口管理、attach、render、resize、
detach 在宿主窗口事件线程执行。每个 presenter 的调用应串行进行。
宽高为物理像素，`scale` 单独提供 DPI 比例，不乘到宽高上。

典型顺序：`create → attach → open → play → render_tick → close → detach → destroy`。
宿主驱动约 60 Hz 的 `render_tick`，处理 resize，并转发暂停、继续、seek。
可编译的 C 示例：[`capi_smoke.c`](../examples/linux_native_demo/capi_smoke.c)。

## 4. 依赖选择与许可

- Linux 默认通过 pkg-config 动态链接系统 FFmpeg 8 与 PulseAudio。
- libass 使用 liberika 补丁版本 0.17.5；系统 libass 缺少有序内存字体回退补丁，不能直接替代。
  脚本将其安装到 `third_party/dist/<rust-host-triple>/system/libass`。
  `ERIKA_LIBASS_DIR` 可指定兼容的补丁构建前缀。
- `ERIKA_FFMPEG_DIR` 可指定已有的静态 FFmpeg bundle。`ERIKA_USE_SYSTEM_LIBS=0`
  保留原有 bundle 查找路径，需要另行准备全部匹配的原生依赖；本指南仅验收系统库路径。
- 系统 FFmpeg 的编解码器与许可由发行版构建配置决定。
  设置 `ERIKA_NATIVE_PROFILE=lgpl` **不会改变系统 FFmpeg 的配置或许可**。
- 构建脚本使用本机 GNU/Linux 工具链；跨编译与 musl 不在本次验收范围内。
- PulseAudio 服务断开会报告音频错误；自动重连尚未实现。

## 5. 验证

```bash
bash scripts/check_linux.sh
# 显示会话同时提供 Wayland 时：
ERIKA_TEST_WAYLAND=1 bash scripts/check_linux.sh
```

检查覆盖单元测试、真实音频暂停恢复、头文件对齐、播放及独立 C ABI 生命周期。
需要可用的显示和 PulseAudio 服务。具体设备、版本和测量结果保存在
[Linux 验证记录](investigations/linux-validation.zh.md)。
