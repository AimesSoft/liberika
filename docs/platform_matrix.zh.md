# liberika 平台能力矩阵

> [中文](platform_matrix.zh.md) · [English](platform_matrix.md) · [日本語](platform_matrix.ja.md)

以下平台以 0.3.1 为基线；浏览器后端已接入 Erika Web，并随 Release 提供独立 Web 归档。
Linux 目前为实验性源码构建目标。

| 平台 | 最低版本 | 解码 / 渲染 | 音频 | 分发与构建 |
|---|---|---|---|---|
| macOS | 11 | VideoToolbox / 软解 + Metal | CoreAudio | arm64、x64、universal；Flutter / SwiftPM |
| iOS | 13 | VideoToolbox / 软解 + Metal | AudioQueue | 真机与模拟器 XCFramework；Flutter / SwiftPM |
| tvOS | 13 | VideoToolbox / 软解 + Metal | AudioQueue | 真机与模拟器 XCFramework；Flutter / SwiftPM |
| Windows | 10 | D3D11VA / 软解 + D3D11 | WASAPI | x64 / ARM64；Flutter |
| Android | 8 / API 26 | MediaCodec / 软解 + wgpu | AAudio | 四个 ABI；Flutter |
| OpenHarmony | API 18 | AVCodec / 软解 + wgpu | OHAudio | arm64；Flutter / ArkTS OHPM |
| Linux（实验性） | FFmpeg 8.x | NVDEC / VA-API / 软解 + wgpu | PulseAudio / pipewire-pulse | Rust / C ABI / Flutter 源码构建，无预编译包 |
| 浏览器 | HTTPS / localhost；WebCodecs 视频与音频、OffscreenCanvas、WebGL2、Web Audio | WebCodecs + WebGL2 / Canvas | Web Audio | Release 预编译归档或源码构建 JS / WASM；尚未发布到 npm |

原生平台提供 C ABI Presenter；浏览器通过 JavaScript / TypeScript `Player` 接入。
硬件解码能力取决于编码格式、设备及驱动；浏览器会在运行时检查编码配置支持。

## 浏览器播放

浏览器中的 FFmpeg WASM 负责解封装，libass 使用 MKV 内嵌 ASS/SSA 和字体附件绘制
字幕，Rust DFM 引擎绘制弹幕；字幕和弹幕共用播放时钟与 WebGL2 合成器，支持独立
双倍采样。外部字幕支持 ASS/SSA、SRT、WebVTT。服务器提供原文件与 HTTP Range。

当前输出 SDR，音视频由浏览器 WebCodecs 解码；HDR/EDR、Dolby Vision、ArtCNN、
位图字幕和内嵌非 ASS 字幕转换尚未移植。完整要求、已知限制与构建部署步骤见
[浏览器指南](web.zh.md)。

## 视频表面与 HDR

| 平台 | 原生呈现 | Flutter 纹理与合成 |
|---|---|---|
| Apple | CAMetalLayer，支持 EDR | macOS IOSurface / Metal 纹理用于 Flutter 裁剪和滤镜 |
| Windows | HWND / D3D11，支持 HDR10；透明视频可用 DirectComposition | 共享 D3D11 SDR 纹理；overlay 混合切换到原生层 |
| Android | SDR TextureView；scRGB 使用 SurfaceView、Vulkan 与 FP16/data space 协商 | TextureView 兼容路径为 SDR |
| OpenHarmony | OHNativeWindow，wgpu / AVCodec；OHNativeBuffer 导入 | Flutter 外部纹理 |
| Linux | X11 / Wayland；Wayland 可用原生视频层 | RGBA SDR 纹理需要读回；原生层位于透明 Flutter UI 下 |

Android scRGB 和 Linux HDR 需要表面实际支持相应编码。查询
`erika_presenter_get_output_status` 获取当前输出、headroom 与回退原因。

Linux VA-API → Vulkan 直接导入和 WSL GPU 复制路径为实验性实现。纹理合成、
硬件解码和解码帧零拷贝是不同的路径，选择步骤在 [Linux 指南](linux.zh.md)。

## CI 与验证

原生构建覆盖六个预编译平台；Linux native workflow 使用 Xvfb、Mesa 和 PulseAudio
运行源码播放检查。Flutter consumer workflow 检查六个预编译平台。
HDR 输出和平台生命周期的设备实验保存在 [验证记录目录](investigations/README.md)。

Web workflow 构建完整 WASM 包并运行独立播放器回归。Erika Web 应用已在 macOS 的
Chrome 155.0.8059.85 验证 H.264/AAC/ASS/字体附件 MKV、VP9/Opus WebM、2000 条
弹幕、实时参数、章节、截图与播放器生命周期；Safari / Firefox 尚未验证。
