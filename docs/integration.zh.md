# 在原生宿主中接入 liberika

本指南讲解如何把 liberika 嵌入非 Flutter 宿主——C/C++/Swift 应用、Win32 窗口，或任何
带 C FFI 的运行时。它使用 **presenter（推送）模型**:liberika 负责解码、时序、音频、
overlay 和呈现,宿主提供一个 surface 并每帧调一次 `render_tick`。

前置:C ABI（[capi_reference.zh.md](capi_reference.zh.md)）和一个已构建好的 liberika 库
（[building.zh.md](building.zh.md)）。Flutter 请改用
[`erika_flutter`](../packages/erika_flutter) 插件,见
[flutter_embedding.zh.md](flutter_embedding.zh.md)。

> 英文版：[integration.md](integration.md)。

两个可运行参考随本指南提供:
[`examples/macos_native_demo`](../examples/macos_native_demo)（AppKit +
`CAMetalLayer`）和 [`examples/windows_native_demo`](../examples/windows_native_demo)
（Win32 + `HWND`）。它们直接驱动 Rust 的 `PresenterRuntime`;下文的 C ABI 调用是
一一对应的等价物。

## 1. 选择 handle 族

除非你有理由自己渲染,否则用 `ErikaPresenterHandle`。拉取模型的 `ErikaHandle` 适合
拥有自己合成器、只想要 liberika 解码/时序/状态的宿主。本指南其余部分都基于 presenter。

presenter 族在 **macOS、iOS、tvOS、Windows、Android、OpenHarmony 与实验性 Linux** 上编译。

## 2. 生命周期

```
create ──▶ attach surface ──▶ open ──▶ play ──▶ (render_tick + poll_event 循环)
                                                       │
                               pause / seek / set_* ◀──┤
                                                       ▼
                         detach surface ──▶ destroy
```

`open` 同步完成媒体探测，成功返回时进入 `Ready`。`play` 向工作线程投递播放命令，
结果通过 `StateChanged` 和 `Error` 事件观察。HTTP 探测可能阻塞；需要保持 UI 响应时，
将同一句柄的所有调用放到串行工作线程，并在播放前 attach surface。
`close` 结束播放器；从头重播使用 `stop`，关闭后再次播放则创建新句柄。

## 3. 创建 presenter

```c
ErikaPresenterConfig cfg = {
    .output_mode  = ErikaPresenterOutputMode_Sdr,   // AppleEdr 或 Android ExtendedLinear
    .edr_headroom = 1.0f,                            // 请求的内容 headroom 上限
    .video_alpha_mode = ErikaVideoAlphaMode_Opaque,
    .luma_upscaler = ErikaLumaUpscalerMode_Off,      // 或 ArtCnnC4F16 / C4F16Ds / C4F32
};
ErikaPresenterHandle *p = erika_presenter_create_with_config(cfg);
if (!p) { /* 读取 erika_last_error_message() */ }
```

`erika_presenter_create()` 用默认值（SDR、无超分）。神经亮度超分在 Apple 平台由
Metal 实现，在包括 Android 在内、具备 compute 能力的 Vulkan/wgpu adapter 上由
wgpu 实现，Windows 上由 D3D11 compute 实现。无 compute 的后端（主要是 GLES 3.0）会保留原生
luma sampling，并明确报告 `Inactive` 状态和回退原因。

### 高清字幕与高清弹幕的默认值

原生 presenter 默认开启高清字幕和高清弹幕，Web 后端也默认开启这两项。
文本字幕和弹幕字形以双倍分辨率采样，显示大小、排版和视频分辨率保持不变。
Rust 宿主可以分别使用 `PresenterRuntime::set_subtitle_supersampling(false)` 和
`DanmakuLayoutConfig { supersampling: false, ..Default::default() }` 关闭，
后者通过 `PresenterRuntime::set_danmaku_config` 应用。

原生字幕保留现有性能保护：源画布任一边超过 2048 像素，或面积超过
2048 × 1080 像素时保持原始采样；连续 60 次字幕渲染平均耗时超过 4 ms 时，
当前媒体回退到原始采样。弹幕中大于 128 px 的字形也保持原始采样。

## 4. Attach 一个 surface

liberika 直接渲染进你拥有的平台 surface。宽高单位是**物理像素**,`scale` 是 DPI/backing
因子。

### macOS / iOS / tvOS —— `CAMetalLayer`

创建一个 `CAMetalLayer`,设好尺寸,再把它的指针交给 liberika:

```c
// `layer` 是 CAMetalLayer*（如来自你的 NSView/UIView 宿主 layer）
erika_presenter_attach_metal_layer(p, (uint64_t)(uintptr_t)layer,
                                   pixel_w, pixel_h, backing_scale);
```

macOS 上推荐的安排是一个 window-hosted layer,作为内容视图的 sibling / underlay,让
视频留在 AppKit 视图合成器之外(与 Flutter 插件同款模型,见
[flutter_embedding.zh.md](flutter_embedding.zh.md)）。

### Windows —— `HWND`

```c
HWND hwnd = /* 你的窗口 */;
HINSTANCE hinst = GetModuleHandleW(NULL);
UINT dpi = GetDpiForWindow(hwnd);
double scale = dpi ? (double)dpi / 96.0 : 1.0;
RECT rc; GetClientRect(hwnd, &rc);
uint32_t w = max(1, rc.right - rc.left), h = max(1, rc.bottom - rc.top);

erika_presenter_attach_windows_hwnd(p, (uint64_t)(uintptr_t)hwnd,
                                    (uint64_t)(uintptr_t)hinst, w, h, scale);
```

`attach_windows_hwnd` 是 `attach_wgpu_surface` + kind `WindowsHwnd` 的便捷封装。在默认
presenter 配置下，该 surface 驱动**原生 Direct3D 11** 渲染器（D3D11VA 零拷贝、HDR10）。
C ABI 的 `ErikaPresenterConfig` 目前不提供渲染器选择；Rust 宿主可通过
`PlayerConfig.renderer` 选择 `WgpuFallback`。

### 通用 —— `attach_wgpu_surface`

对 X11/Wayland/Android,或想显式指定 surface 类型,用
`erika_presenter_attach_wgpu_surface(p, kind, raw_window, raw_display, w, h, scale)`,
配对应的 `ErikaWgpuSurfaceKind` 和平台句柄。

### Linux X11 / Wayland

`XlibWindow` 对应 X11 Window ID 与 Display 指针，`WaylandSurface` 对应
`wl_surface*` 与 `wl_display*`。这些句柄由宿主持有，detach 完成后再释放。
构建依赖与示例在 [Linux 接入](linux.zh.md)。

### Android extended-linear scRGB

Android `ExtendedLinear` 是 FP16 extended-linear scRGB，不是 HDR10/PQ。原生宿主必须从
直接合成的 `SurfaceView` 取得 `ANativeWindow`（Flutter 插件使用 Hybrid Composition），
并把探测到的输出能力传入 attach：

```c
ErikaSurfaceOutputCapabilities caps = {
    .extended_linear = display_and_surface_are_hdr_capable,
    .direct_composition = true,       // SurfaceView，而非 TextureView
    .desired_headroom = requested_headroom, // 0 = 系统 auto
    .fallback_reason = host_probe_reason, // eligible 时为 0
};
erika_presenter_attach_wgpu_surface_with_output_capabilities(
    p, ErikaWgpuSurfaceKind_AndroidNativeWindow,
    (uint64_t)(uintptr_t)native_window, 0, w, h, scale, caps);
```

只有请求模式、显示器/surface eligibility、direct composition、Vulkan backend、
`Rgba16Float` 支持和 configure 后的 `ADATASPACE_SCRGB_LINEAR` 回读全部成功时，renderer
才会激活 extended-linear。任一条件失败都会保持 SDR 播放，并记录稳定的 `0..8`
`fallback_reason`；GLES 与 `TextureView` 始终是 SDR 路径。

`ErikaPresenterConfig.edr_headroom` 是内容上限。正数 `desired_headroom` 是可选的 surface
上限；`0` 表示由系统自动决定。wgpu 的有效 target 会受这些上限和当前显示器 HDR/SDR
ratio 共同约束。API 34+ 的原生宿主应监听
`Display.registerHdrSdrRatioChangedListener`，并在状态真实变化时调用
`erika_presenter_set_output_headroom(p, ratio, true)`；ratio 不可用或 view detach 时发布
`(1.0f, false)`。Flutter 插件会自动完成这条链路。API 35 上它还会按 view 调用
`SurfaceView.setDesiredHdrHeadroom`，不会修改全局 Window。

attach 后以及每次 resize/recovery 后都应查询 `erika_presenter_get_output_status`。Android
scRGB 真正激活时会报告 `AndroidExtendedLinearScRgb`、`SixteenBitFloat`、native dataspace
`406913024` 和 `extended_linear_active = true`。Android 提供显示器 ratio 时，
`active_headroom` 为该 ratio，且 `active_headroom_known = true`；否则该字段只是 fallback
值，`active_headroom_known = false`。只有 known 状态或 ratio 真实变化时，
`headroom_updates` 才增长。请求模式本身不能证明实际模式已激活。当前非 HDR/模拟器测试
覆盖 SDR 回退；active 路径仍需 API 35 HDR 真机验收。

## 5. 打开并播放

```c
if (erika_presenter_open(p, "/path/to/video.mkv") != ErikaStatus_Ok) { /* 记录 */ }
erika_presenter_play(p);
```

`uri` 是本地路径或 HTTP(S) URL。使用打开选项设置缓存预算：

```c
ErikaOpenOptions options = {0};
options.http_read_ahead_bytes = 8 * 1024 * 1024;
options.http_back_buffer_bytes = 128 * 1024 * 1024;
erika_presenter_open_with_options(p, "https://example.com/video.mp4", &options);
```

预读默认为 2 MiB，回退缓存为 16 MiB；传 0 采用默认值。进程级
`ERIKA_HTTP_READAHEAD_BYTES` 可设置默认预读窗口。预算按字节计算，回退时间取决于码率。

HTTP(S) 源可以忽略 Range、返回包含完整文件的 `200`，也可以不提供 Content-Length。
liberika 会丢弃请求偏移之前的字节，只缓存所需数据；完整响应到达 EOF 后可以得到文件长度。
这类源在跳转或重连时需要重新下载前缀，因此支持 `206` 的服务器读取大文件更快。
重试期间若资源校验标识发生变化，会拒绝该响应，并阻止同一源的同步回退和后续读取，
避免拼接不同版本的文件。弱 ETag 不影响首次读取，但不能用于恢复未完成的响应。

## 6. 渲染循环

从 surface 的显示定时器驱动 `render_tick`——`CADisplayLink`（iOS/tvOS）/ `CVDisplayLink`
或 `CADisplayLink`（macOS）/ Windows 的帧调度器 / Android `Choreographer`。传该帧的
**呈现时间(秒)**,取自单调宿主时钟;liberika 用它做 vsync 量化调度,所以传绝对时间戳,
不是增量。

```c
// 每个显示帧:
ErikaPresenterStats stats;
erika_presenter_render_tick(p, host_time_seconds, &stats);   // out_stats 可为 NULL

// 同一次迭代里把事件抽干:
ErikaEvent ev;
while (erika_presenter_poll_event(p, &ev) == ErikaStatus_Ok) {
    handle_event(&ev);
}
```

任何 drawable 尺寸或 scale 变化时(窗口缩放、显示器 DPI 改变、设备旋转),在下一次 tick
**之前**调 `erika_presenter_resize_surface(p, w, h, scale)`。Windows demo 每帧轮询
`GetClientRect` + `GetDpiForWindow`,变化时 resize。

`render_tick` 很快返回;它自身不在 vsync 上阻塞——节奏由你的显示定时器提供。若你不在
显示回调里(如冒烟测试),每次迭代 `~16 ms` 的 sleep 可近似 60 Hz。

### 显示目标时间采样

0.2.1 起可用 `erika_presenter_render_tick_with_timing` 在显示目标时刻同步采样视频、
字幕和弹幕。delay 是从调用入口到显示目标的秒数，可为负数，范围 ±0.25 秒；
传 NULL 保留原有采样行为。

```c
double delay = display_target_seconds - monotonic_now_seconds;
erika_presenter_render_tick_with_timing(p, display_target_seconds, &delay, &stats);
```

iOS Flutter 插件将 `CADisplayLink.targetTimestamp` 经 tick mailbox 传入此接口。

## 7. 处理事件

`poll_event` 非阻塞,队列空时返回 `NoEvent`。按 `ev.kind` 分发:

| Kind | 含义 | 读取 |
|------|------|------|
| `StateChanged` | 播放状态变化 | `ev.state` |
| `DurationChanged` | 时长已知/更新 | `ev.duration_micros` |
| `PositionChanged` | 周期性位置 tick | `ev.position_micros` |
| `TracksChanged` | 轨道列表变化 | 重新查询 `erika_presenter_tracks` |
| `TrackSelectionChanged` | 选择变化 | `erika_presenter_track_selection` |
| `BufferingChanged` | 缓冲切换 | `ev.buffering` |
| `VideoParamsChanged` | 分辨率 / 色彩元数据 | `ev.video` |
| `Error` | 发生失败 | `ev.status` + `erika_last_error_message` |

## 8. 运行时控制

以下都可在两次 tick 之间安全实时调用:

- **传输:** `play` / `pause` / `stop` / `seek(position_micros)` /
  `set_playback_rate(rate)`。
- **音频:** `set_volume(0.0–1.0)`。
- **轨道:** `erika_presenter_tracks`(计数数组惯用法)、`select_audio_track` /
  `select_subtitle_track`(id `-1` 关闭字幕)、`add_external_subtitle`、
  `remove_subtitle_track`、`set_subtitle_scale`。
- **字幕样式:** `set_subtitle_font(family, file_path)` 与
  `set_subtitle_style(ErikaSubtitleStyle)`（字体、颜色、度量、字形属性、描边、
  对齐与边距）。两者都是回退值——未置起对应的 `override_mask` 位时 ASS 脚本保留
  自带样式；字号与描边仍会再乘 `set_subtitle_scale`。见
  [capi_reference.zh.md](capi_reference.zh.md#播放与运行时参数)。
- **弹幕:** 加载一条轨(`load_danmaku_file` / `_json` 或多轨 `add_danmaku_track_*`)、
  开关(`set_danmaku_enabled`)、用 `set_danmaku_config` 调参、偏移轨、设字体、设屏蔽词。
  见 [danmaku_architecture.md](danmaku_architecture.md)。
- **超分:** `set_upscaler(mode)`;用 `get_upscaler_status` 检视。
- **输出:** 用 `erika_presenter_get_output_status` 检视实际模式和 fallback counters。
  Android 原生宿主用 `erika_presenter_set_output_headroom` 发布动态显示 ratio。
  `capture_frame_rgba` 始终输出视频 + 字幕（不含弹幕）的 SDR RGBA8 截图，即使显示输出是
  extended-linear。
- **宿主离屏合成:** Rust 的 `PresenterRuntime::capture_frame_rgba_with_danmaku(w, h)`
  或 C 的 `erika_presenter_capture_frame_rgba_with_danmaku` 可在不绑定 surface 时
  捕获视频、字幕和已启用的弹幕，供宿主上传到自己的 UI 纹理。
  见 [捕获约定与示例](capi_reference.zh.md)。
  不读回 CPU 的原生 GPU 句柄使用 `export_frame_texture`，其帧循环同时驱动音频；
  见 [原生纹理输出](external_textures.zh.md)。

## 9. 释放

```c
erika_presenter_detach_surface(p);   // 先停止往 surface 绘制
erika_presenter_destroy(p);          // 停止播放,释放一切
```

在拆掉窗口/layer 之前先 detach,让 liberika 停止触碰 surface。`destroy` 对 `NULL` handle
安全。

## 10. 线程模型

handle **没有内部同步**。最简单的正确设计:在一个线程上拥有 presenter——就是跑显示
定时器的那个——并从那里发起所有调用(`render_tick`、传输、轨道变更)。若必须从另一个
线程调用(如 UI 线程发 `seek`),用你自己的锁串行化,使同一 handle 上两次调用绝不重叠。
错误信息是线程局部的,所以在发起失败调用的线程上读 `erika_last_error_message`。

## 分语言注意点

### C / C++

include `erika.h`,链接库(见 [building.zh.md](building.zh.md)),就这么简单——ABI 是
纯 C。C++ 里把 handle 包进一个在析构时调 `erika_presenter_destroy` 的 RAII 类型,并
用配套的 liberika 释放函数释放返回的字符串 / `ErikaTrackInfo`,绝不用 `delete`。

### Swift

通过 bridging header 或对 `erika.h` 的 module map 引入 C ABI。用
`unsafeBitCast(layer, to: UInt64.self)` 或 `UInt64(UInt(bitPattern: ...))` 转
`CAMetalLayer`。从 `CADisplayLink`/`CVDisplayLink` 回调驱动
`erika_presenter_render_tick`。macOS/iOS/tvOS 的 Flutter Swift 插件就是在同一 C ABI 上这么做。

### Dart FFI

用 `dart:ffi` 绑定符号（`DynamicLibrary.open` 加载 dylib/dll,或静态链接用进程符号）。
把所有 FFI 调用放在一个 isolate;用 `toNativeUtf8`/`free` 编排字符串。高层
`erika_flutter` 包已经做好了这些——除非你在做自定义嵌入,否则优先用它。

## 检查清单

- [ ] 创建 presenter(用与你显示器匹配的输出模式)。
- [ ] 用**物理像素**尺寸和正确的 scale attach surface。
- [ ] Android extended-linear 必须使用 Hybrid Composition 的 `SurfaceView`，传入探测后的
  output capabilities，并用 `erika_presenter_get_output_status` 验证
  `Rgba16Float + SCRGB_LINEAR`；否则接受并记录 SDR reason。
- [ ] API 34+ 通过 `erika_presenter_set_output_headroom` 发布显示器 HDR/SDR ratio 变化；
  API 35 的 desired headroom 只作用于单个 `SurfaceView`。
- [ ] 同步 open 后调用 play；通过事件观察播放状态和错误。
- [ ] 每个显示帧 `render_tick(absolute_time_seconds)`;抽干事件。
- [ ] 每次尺寸/scale 变化都 `resize_surface`。
- [ ] 每个 handle 一个线程,或串行化调用。
- [ ] 释放每个返回的字符串 / `ErikaTrackInfo`;先 `detach` 再 `destroy`。
- [ ] API 35 HDR 真机的旋转/恢复、多 player 和 SDR 截图检查通过前，不宣称 Android
  extended-linear 已完成真机验证。
