# liberika C ABI 参考手册

本文档描述 `erika_capi` 导出的稳定 C ABI，声明在
[`crates/erika_capi/include/erika.h`](../crates/erika_capi/include/erika.h)。该
ABI 是所有非 Rust 宿主（C、C++、Swift、Dart FFI、Win32……）的唯一接入面。Rust
嵌入方应直接使用 `erika` crate；本层仅为 FFI 而存在。

嵌入流程（surface attach、渲染循环、释放）见 [integration.zh.md](integration.zh.md)。
高层引擎设计见 [architecture.zh.md](architecture.zh.md)。

> 英文版：[capi_reference.md](capi_reference.md)。

## 两个 handle 族

liberika 暴露两个相互独立的入口，每个集成选其一。

| Handle | 模型 | 谁渲染 | 适用 |
|--------|------|--------|------|
| `ErikaHandle` | **拉取（Pull）** | 宿主 | 你拥有渲染循环、自己拉取解码帧 / 驱动自己的合成器。 |
| `ErikaPresenterHandle` | **推送（Push）** | liberika | 你把一个原生 surface 交给 liberika，每个显示帧调一次 `render_tick`。liberika 负责解码、时序、音频、overlay 和呈现。 |

`ErikaPresenterHandle` 是推荐路径，也是 Flutter 插件和原生 demo 所用的。它在
**macOS、iOS、tvOS、Windows、Android、Linux、HarmonyOS** 上编译。surface attach 按平台不同：
Apple 使用 Metal，Windows 使用 HWND/D3D11，Android/HarmonyOS 使用 wgpu/Vulkan surface。
Linux 通过 wgpu 支持 X11/Wayland。
HarmonyOS Flutter bridge 因 ArkTS 平台通道本身传递序列化结构化值，使用 presenter 的 JSON
辅助入口。其它不支持的平台上 `erika_presenter_create` 可能仍导出但返回 `NULL`；请同时按平台
守卫 presenter 用法，并检查 create 是否成功。

两个族不共享状态；一个进程可同时使用两者，但单个媒体会话只活在一个 handle 里。

### Linux Flutter 纹理读回

```c
ErikaStatus erika_presenter_copy_flutter_frame_rgba(
    ErikaPresenterHandle *handle, uint8_t *out_rgba, uintptr_t capacity,
    uint32_t *out_width, uint32_t *out_height);
```

每次 `erika_presenter_render_tick` 后调用 `erika_presenter_copy_flutter_frame_rgba`
可取走最近的合成帧，包含视频、字幕、弹幕和 HUD。调用方持有输出内存，容量至少为
纹理宽 × 高 × 4 字节；成功时写入紧密 RGBA8 像素与物理尺寸。不返回引擎内存，
无需调用 free。没有待取帧时返回 `ErikaStatus_NoEvent`；容量不足时返回错误，
该帧仍会被消费，应等待下一次 tick 后重试。与同一 presenter 的其它操作串行调用。
此接口经 CPU 内存复制，不提供零拷贝。

## Headless GIF 导出

`erika_export_gif` 独立于上述两个 handle 族。它创建私有的 demuxer、软件解码器、
缩放器、GIF encoder 和 muxer，不需要窗口或 surface，也不会干扰正在播放的实例。

```c
ErikaGifExportOptions options = {
    .input_uri = "/path/input.mp4",
    .output_path = "/path/output.gif",
    .start_millis = 5000,
    .end_millis = 10000,
    .frames_per_second = 10,
    .output_width = 640,
    .output_height = 360,
    .quality = ErikaGifExportQuality_High,
    .loop_count = 0,
};
ErikaGifExportResult result = {0};
ErikaStatus status = erika_export_gif(&options, &result);
```

该调用是同步的；Flutter 等 UI 宿主必须把它放到 worker thread 或 isolate 中执行。
`start_millis` 与 `end_millis` 选择导出区间；`output_width` 与 `output_height`
是精确输出尺寸。普通质量使用双线性缩放，高质量使用 Lanczos 缩放。只有完整编码成功后，临时同目录文件才会重命名为目标文件。
除非 `overwrite` 为 true，否则已有目标会被拒绝。`loop_count` 沿用 FFmpeg GIF 语义：
`-1` 不循环、`0` 无限循环、正数表示循环次数。HTTP 输入可使用与
`ErikaOpenOptions` 相同的请求头和 read-ahead 覆盖参数。

## Headless 单帧导出

`erika_export_frame` 同样独立于两个 handle 族。它创建私有的 demuxer 和多线程软件解码器，
跳到 `position_millis` 之前的关键帧后向前解码，返回该媒体时间显示的画面：时间不晚于它的最后一帧
（位置早于第一帧时为第一帧）。

```c
ErikaFrameExportOptions options = {
    .input_uri = "/path/input.mkv",
    .position_millis = 60000,
    .max_width = 640,
    .max_height = 640,
    .format = ErikaFrameExportFormat_Png,
};
ErikaFrameExportResult result = {0};
if (erika_export_frame(&options, &result) == ErikaStatus_Ok) {
    /* result.data 中是 result.data_len 字节的 PNG 文件。 */
    erika_frame_export_result_free(&result);
}
```

该调用是同步的；UI 宿主必须把它放到 worker thread 或 isolate 中执行。`max_width` 与
`max_height` 限定画面尺寸（0 表示该边不限）：保持显示宽高比，且不会大于视频本身，两者都为 0
时返回视频原尺寸。`ErikaFrameExportFormat_Rgba` 返回每行恰好 `width * 4` 字节、alpha 为 255
的像素；`ErikaFrameExportFormat_Png` 返回 8 位 RGB 的 PNG 文件。色彩转换按帧的色彩空间与范围；
未标注的 YUV 源在高清及以上按 BT.709、其余按 BT.601。HDR 源（PQ、HLG、杜比视界）会按 Erika 渲染器在 SDR 显示器上使用的
运算做色调映射到 SDR（ITU-R BT.2390 EETF，目标 203 nit，BT.2020 到 BT.709 的色域映射，gamma 2.2），
并在 `result.reserved_flags` 置位 `ERIKA_FRAME_EXPORT_TONE_MAPPED`；SDR 源仍用普通色彩转换。结果给出画面所在时间
（`timestamp_micros`）、解码后的视频尺寸与媒体时长（未知为 0）。位置不早于已知时长时返回
`ErikaStatus_PlayerError`。HTTP 输入可使用与 `ErikaOpenOptions` 相同的请求头和 read-ahead
覆盖参数。`data` 归 liberika 所有，直到调用 `erika_frame_export_result_free`；该函数会清空结果，
可重复调用。

### 连续取多帧

`erika_export_frame` 每取一张都要重新打开媒体并 seek。要从同一媒体取多张时，只打开一次
`ErikaFrameExportSession`，再逐张向它索取；demuxer 与解码器保持打开。

```c
ErikaFrameExportSessionOptions open = { .input_uri = "/path/input.mkv" };
ErikaFrameExportSession *session = NULL;
if (erika_frame_export_session_open(&open, &session) == ErikaStatus_Ok) {
    for (uint64_t ms = 60000; ms < 66000; ms += 100) {
        ErikaFrameExportRequest request = {
            .position_millis = ms, .max_width = 640, .max_height = 640,
            .format = ErikaFrameExportFormat_Png,
        };
        ErikaFrameExportResult result = {0};
        if (erika_frame_export_session_frame(session, &request, &result) == ErikaStatus_Ok) {
            erika_frame_export_result_free(&result);
        }
    }
    erika_frame_export_session_close(session);
}
```

若上一次解码位置到本次请求之间没有关键帧（demuxer 的 seek 索引可以判断；没有索引时，请求在
两秒内或最长关键帧间隔的一半以内），就接着解码；否则 seek 到请求之前的关键帧。请求可以任意顺序，
时间递增最省。结果、尺寸、格式与色调映射同 `erika_export_frame`。某次请求失败后会话仍可使用。
同一会话上的调用相互串行，所有调用都是同步的。`erika_frame_export_session_close` 接受 `NULL`；
关闭时不得有调用在执行，之后也不得再调用。

## 约定

### 状态码

每个可失败的调用都返回 `ErikaStatus`：

| 值 | 码 | 含义 |
|----|----|------|
| `ErikaStatus_Ok` | 0 | 成功。清空线程局部错误。 |
| `ErikaStatus_NullPointer` | 1 | 必需的 handle 或 out 指针为 `NULL`（或 surface 指针为 0）。 |
| `ErikaStatus_InvalidUtf8` | 2 | 某个 `const char*` 参数不是合法 UTF-8。 |
| `ErikaStatus_PlayerError` | 3 | 引擎拒绝该调用；读取错误信息（见下）。 |
| `ErikaStatus_Panic` | 4 | 在边界处捕获到 Rust panic。该调用除 panic 前已完成的部分外无效；handle 应视为可疑状态。 |
| `ErikaStatus_NoEvent` | 5 | 仅 `*_poll_event`：队列为空（非错误）。 |

务必检查返回值。`Ok` 和 `NoEvent` 是仅有的非错误结果。

### panic 安全

ABI 永不让 unwind 穿越 FFI 边界。每个入口都把函数体包在 `catch_unwind` 里；panic
会变成 `ErikaStatus_Panic` 并设置错误信息。你可以跨边界调用，无需担心 C++
`noexcept`/SEH。

### 错误信息（线程局部）

任何非 `Ok`/`NoEvent` 的结果，liberika 会把一条可读信息存进**线程局部**槽。读取：

```c
char *msg = erika_last_error_message();   // 堆分配，可能为 NULL
if (msg) { fprintf(stderr, "erika: %s\n", msg); erika_string_free(msg); }
```

因为是线程局部的，要**在发起失败调用的同一线程**上读取，且在该线程下一次调用之前读取
（之后一次 `Ok` 会清空它）。`erika_last_error_message` 返回的是你拥有的副本——用
`erika_string_free` 释放。

### 字符串所有权

liberika 返回的任何 `char*` 都是堆分配、归调用方所有：

- 独立字符串（如 `erika_last_error_message`）→ 用 `erika_string_free` 释放。
- 内嵌在 `ErikaTrackInfo` 中的字符串 → 用 `erika_track_info_free(&track)`
  释放整条记录（会释放每个内部字符串）。
- 内嵌在 `ErikaDanmakuTrackInfo` 中的字符串 → 用
  `erika_danmaku_track_info_free(&track)` 释放。

切勿用 libc `free()` 释放这些；始终用配套的 liberika 释放函数，使分配在同一 allocator
上跨越 ABI。

你传入的 `const char*` 参数仅在调用期间被借用；liberika 会复制所需内容。它们必须是
NUL 结尾的 UTF-8。

### 计数数组惯用法

列表 getter（`erika_tracks`、`erika_presenter_tracks`、
`erika_presenter_danmaku_tracks`）使用调用方分配的缓冲：

```c
size_t total = 0;
erika_presenter_tracks(p, NULL, 0, &total);          // 1) 查询数量
ErikaTrackInfo *buf = calloc(total, sizeof *buf);
erika_presenter_tracks(p, buf, total, &total);        // 2) 填充
for (size_t i = 0; i < total; i++) { /* 使用 buf[i] */ }
for (size_t i = 0; i < total; i++) erika_track_info_free(&buf[i]);
free(buf);
```

`out_len` **始终**被设为可用记录的总数。最多写入 `capacity` 条；传
`capacity == 0`（配 `NULL` 缓冲）是受支持的"问数量"方式。只有实际写入的记录才持有
需要释放的字符串。

### Surface 几何与 scale

`attach_*` 和 `resize_surface` 的 `width`、`height` 单位是**物理像素**，`scale`
是 backing/DPI 因子（如 Retina 的 `2.0`、Windows 的显示器缩放）。surface 指针为 `0`
会被拒绝并返回 `NullPointer`。

### 线程

单个 handle **没有内部同步**。不要从多个线程并发调用同一个 handle；请自行串行化
（或把 handle 限定在单线程内）。presenter 的 `render_tick` 应从拥有显示定时器 /
surface 的线程驱动。不同 handle 在不同线程上互相独立。记住错误信息是线程局部的。

## `ErikaHandle` —— 拉取模型

宿主自己驱动渲染，并拉取状态/事件。

### 生命周期

```c
ErikaHandle *erika_create(void);
void         erika_destroy(ErikaHandle *handle);
char        *erika_last_error_message(void);   // 线程局部，调用方释放
void         erika_string_free(char *value);
```

`erika_create` 永不失败（返回合法 handle）。`erika_destroy(NULL)` 是 no-op。销毁
handle 会停止播放并释放全部资源。

### 播放控制

```c
ErikaStatus erika_open(ErikaHandle *handle, const char *uri);   // 文件路径或 URL
ErikaStatus erika_open_with_headers(ErikaHandle *handle, const char *uri,
                                    const ErikaHttpHeader *headers, uintptr_t header_count);
ErikaStatus erika_open_with_options(ErikaHandle *handle, const char *uri,
                                    const ErikaOpenOptions *options);
ErikaStatus erika_play(ErikaHandle *handle);
ErikaStatus erika_pause(ErikaHandle *handle);
ErikaStatus erika_stop(ErikaHandle *handle);
ErikaStatus erika_close(ErikaHandle *handle);
ErikaStatus erika_seek(ErikaHandle *handle, uint64_t position_micros);
```

`uri` 是本地文件路径或 HTTP(S) URL。`erika_open_with_headers` 用于为 HTTP(S) 播放
设置请求头；`headers` 只在调用期间读取，调用返回后即可释放。`header_count` 大于零时
`headers` 不能为 NULL。请求头会用于 HEAD、Range GET 和预取请求。
认证信息和 Cookie 不会写入 liberika 日志。`seek` 单位为微秒。

`erika_open_with_options` 是 `erika_open_with_headers` 的超集：通过 `ErikaOpenOptions`
结构体把请求头数组与逐请求调参打包在一起。`options` 传 NULL 表示全部使用默认值。
`http_read_ahead_bytes` 覆盖本次请求的 HTTP(S) 预读窗口（字节）。`0` 会优先采用进程级
环境变量 `ERIKA_HTTP_READAHEAD_BYTES`，未设置时使用 2 MiB 默认值；显式非零值优先于
该环境变量。
`http_back_buffer_bytes` 覆盖 HTTP(S) 回看预算（字节）——即播放位置之后保留多少已播
数据，落在这个范围内的回退不需要发起网络请求。`0` 使用 16 MiB 默认值；播放高码率
媒体的宿主应按码率取值（约 15 秒 × 码率，例如 71 Mbps 下 -10 秒回退覆盖 ~89 MB）。
`reserved` 中的非零值会被拒绝，以便未来增加字段时不改变旧宿主的行为。这两个参数只
影响 HTTP(S) 播放，本地文件不受影响。

```c
typedef struct ErikaOpenOptions {
  const ErikaHttpHeader *headers;
  uintptr_t header_count;
  uint64_t http_read_ahead_bytes;   /* 0 = 环境变量，否则 2 MiB */
  uint64_t http_back_buffer_bytes;  /* 0 = 16 MiB 回看预算 */
  uint64_t reserved[2];             /* 必须为零 */
} ErikaOpenOptions;
```

`open` 会同步探测媒体流并转入 `Ready`；`play` 才是异步入队。
应在宿主 UI 线程之外执行可能阻塞的 open，并串行调用同一 handle 的所有接口。
通过 `StateChanged`、`DurationChanged` 和 `Error` 事件观察后续播放变化。

### 轨道与字幕

```c
ErikaStatus erika_add_external_subtitle(ErikaHandle *, const char *uri, int64_t *out_track_id);
ErikaStatus erika_remove_subtitle_track(ErikaHandle *, int64_t track_id);
ErikaStatus erika_select_audio_track(ErikaHandle *, int64_t track_id);
ErikaStatus erika_select_subtitle_track(ErikaHandle *, int64_t track_id);
ErikaStatus erika_track_selection(ErikaHandle *, ErikaTrackSelection *out_selection);
ErikaStatus erika_tracks(ErikaHandle *, ErikaTrackInfo *out_tracks, uintptr_t capacity, uintptr_t *out_len);
void        erika_track_info_free(ErikaTrackInfo *track);
```

`erika_tracks` 遵循计数数组惯用法。`erika_track_selection` 报告当前选中的
视频/音频/字幕轨 id（`-1` 表示无）。选择字幕轨 id `-1` 即关闭字幕。

`ErikaTrackInfo` 的 `codec`、`width`、`height`、`pixel_format`、`profile`、`level` 是轨道
元数据。`bit_rate` 的单位为 bit/s，`frame_rate_numerator` / `frame_rate_denominator` 是视频
轨的有理帧率；`0` 表示对应值未知。帧率按平均帧率、`r_frame_rate`、FFmpeg 估算帧率的顺序
探测。码率优先用轨自身参数；只在单视频轨缺少码率、容器总码率与所有其它音频轨码率均已知时，
才以“容器总码率减去音频轨码率”估算。两者都不是播放中的瞬时码率或实时渲染 FPS，码率估算值
还可能包含封装开销或其它非音频流。

### 状态与事件

```c
ErikaStatus erika_state(ErikaHandle *, ErikaState *out_state);
ErikaStatus erika_poll_event(ErikaHandle *, ErikaEvent *out_event);
```

`erika_poll_event` 非阻塞：队列空时返回 `NoEvent`。在循环里把它抽干。见
[事件](#事件)。

### Surface attach（宿主管理）

```c
ErikaStatus erika_attach_metal_layer(ErikaHandle *, uint64_t raw_layer, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_attach_wgpu_surface(ErikaHandle *, ErikaWgpuSurfaceKind kind,
                                      uint64_t raw_window, uint64_t raw_display,
                                      uint32_t w, uint32_t h, double scale);
ErikaStatus erika_attach_wgpu_surface_with_output_capabilities(
                                      ErikaHandle *, ErikaWgpuSurfaceKind kind,
                                      uint64_t raw_window, uint64_t raw_display,
                                      uint32_t w, uint32_t h, double scale,
                                      ErikaSurfaceOutputCapabilities capabilities);
ErikaStatus erika_attach_flutter_texture(ErikaHandle *, ErikaFlutterTextureKind kind,
                                         int64_t texture_id, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_detach_surface(ErikaHandle *);
```

`raw_layer` 是 `CAMetalLayer*` 转成的 `uint64_t`。对 `erika_attach_wgpu_surface`，
`raw_window`/`raw_display` 是该 `kind` 对应的平台窗口/显示句柄（如 `WindowsHwnd`
的 `HWND` + `HINSTANCE`，`XlibWindow` 的 xcb/Xlib window + display）。
`erika_attach_flutter_texture` 把一个外部纹理 id 注册进平台 texture registrar。
Android 原生宿主若要声明直接合成、具备 HDR eligibility 的 `SurfaceView`，必须使用
`_with_output_capabilities` 变体；短函数传入全 false/default capabilities，因此不能激活
Android extended-linear 输出。

## `ErikaPresenterHandle` —— 推送模型

liberika 拥有完整栈；宿主提供 surface 并调用 `render_tick`。
**macOS / iOS / tvOS / Windows / Android / HarmonyOS / Linux。**

### 生命周期与配置

```c
ErikaPresenterHandle *erika_presenter_create(void);
ErikaPresenterHandle *erika_presenter_create_with_config(ErikaPresenterConfig config);
ErikaPresenterHandle *erika_presenter_create_with_output_mode(int32_t output_mode, float edr_headroom);
ErikaPresenterHandle *erika_presenter_create_with_output_mode_and_alpha(int32_t output_mode, float edr_headroom,
                                                                        int32_t video_alpha_mode);
void                  erika_presenter_destroy(ErikaPresenterHandle *handle);
```

从 v0.1.8 起，`ErikaPresenterConfig` 为四字段、16 字节；v0.1.7 为三字段、
12 字节。此结构体按值传递，两个版本不具备二进制兼容性。C/C++ 调用方及手写
Swift/FFI 镜像必须使用配套头文件和原生库重新编译，不透明视频应将
`video_alpha_mode` 初始化为 `0`。

`ErikaPresenterConfig` 选择输出模式（`Sdr`、Apple `AppleEdr`、Android
`ExtendedLinear`）、请求的 EDR/scRGB 内容 headroom 上限、初始亮度超分，以及
`video_alpha_mode`（`ErikaVideoAlphaMode`）。Android
`ExtendedLinear` 是 FP16 extended-linear scRGB，不是 HDR10/PQ。
`create_with_output_mode` 与 `create_with_output_mode_and_alpha` 是简写；`create` 用默认值
（SDR、不透明视频、无超分）。返回 `NULL` 表示
创建失败——检查 `erika_last_error_message`。

### 播放与运行时参数

```c
ErikaStatus erika_presenter_open(ErikaPresenterHandle *, const char *uri);
ErikaStatus erika_presenter_open_with_headers(ErikaPresenterHandle *, const char *uri,
                                              const ErikaHttpHeader *headers,
                                              uintptr_t header_count);
ErikaStatus erika_presenter_open_with_options(ErikaPresenterHandle *, const char *uri,
                                              const ErikaOpenOptions *options);
ErikaStatus erika_presenter_play(ErikaPresenterHandle *);
ErikaStatus erika_presenter_pause(ErikaPresenterHandle *);
ErikaStatus erika_presenter_stop(ErikaPresenterHandle *);
ErikaStatus erika_presenter_close(ErikaPresenterHandle *);
ErikaStatus erika_presenter_seek(ErikaPresenterHandle *, uint64_t position_micros);
ErikaStatus erika_presenter_set_playback_rate(ErikaPresenterHandle *, double rate);
ErikaStatus erika_presenter_set_volume(ErikaPresenterHandle *, double volume);   // 0.0–1.0
ErikaStatus erika_presenter_set_upscaler(ErikaPresenterHandle *, int32_t mode);  // ErikaLumaUpscalerMode
ErikaStatus erika_presenter_set_subtitle_scale(ErikaPresenterHandle *, double scale);
ErikaStatus erika_presenter_set_subtitle_font(ErikaPresenterHandle *, const char *family, const char *file_path);
ErikaStatus erika_presenter_set_subtitle_style(ErikaPresenterHandle *, ErikaSubtitleStyle style);
ErikaStatus erika_presenter_set_output_headroom(ErikaPresenterHandle *, float headroom, bool known);
```

`set_playback_rate(1.0)` 为正常速度。`erika_presenter_open_with_options` 是
`erika_open_with_options` 的推送模型对应版本，接受同样的
`ErikaOpenOptions`（请求头加 `http_read_ahead_bytes` / `http_back_buffer_bytes`；见
[`erika_open_with_options`](#erikahandle--拉取模型)）。`set_upscaler` 在运行时切换神经亮度超分（见
[`erika_presenter_get_upscaler_status`](#诊断与截图)）。Metal 与具备 compute 能力的
wgpu renderer，以及 feature level 11.0+ 的 D3D11 renderer 会执行 ArtCNN；其他后端保留原生 luma sampling，并明确报告
`Inactive` 回退。

`set_subtitle_font` 与 `set_subtitle_style` 设置字幕的**回退**外观。family 或
path 传空即清除该项。颜色为 `0xRRGGBBAA`，默认不透明白色文字（`0xFFFFFFFF`）与
半透明黑色描边（`0x0000007F`）。`font_size`（默认 `48`，钳位 `8..400`）与
`outline_width`（默认 `2`，钳位 `0..32`）以 ASS 脚本单位计，且仍会再乘上
`set_subtitle_scale`。容器内的 ASS 脚本保留自带样式，这些值只用于补足脚本未指定的
部分、系统解析不到的字体，以及纯文本（SRT/WebVTT）字幕的外观。

`ErikaSubtitleStyle` 承载完整外观，越界的度量会被钳位而不是拒绝：

```c
typedef struct ErikaSubtitleStyle {
  const char *font_family;      /* NULL 或空则回到平台默认 */
  const char *font_file_path;
  uint32_t primary_color_rgba;  /* 0xRRGGBBAA */
  uint32_t outline_color_rgba;
  double font_size;             /* 8..400   */
  double outline_width;         /* 0..32    */
  bool bold, italic, underline, strike_out;
  double spacing;               /* -100..100 */
  double scale_x_percent;       /* 1..1000，100 为原始比例 */
  double scale_y_percent;
  int32_t border_style;         /* 1 描边+阴影，3 不透明底框 */
  double shadow_depth;          /* 0..32 */
  double blur;                  /* 0..100 */
  int32_t alignment;            /* 1..9，小键盘布局，2 为底部居中 */
  int32_t margin_left, margin_right, margin_vertical;
  uint32_t override_mask;
} ErikaSubtitleStyle;
```

`override_mask` 决定哪些字段不再只是回退值，而是通过 libass 的 selective style
override 覆盖 ASS 对白自带的样式。它是 `erika.h` 中 `ERIKA_SUBTITLE_OVERRIDE_*`
宏的位掩码，与 libass 的 `ASS_OVERRIDE_BIT_*` 一一对应：

| 宏 | 覆盖的字段 |
| --- | --- |
| `ERIKA_SUBTITLE_OVERRIDE_FONT_SIZE_FIELDS` | `font_size`、`spacing`、`scale_x_percent`、`scale_y_percent` |
| `ERIKA_SUBTITLE_OVERRIDE_FONT_NAME` | `font_family` |
| `ERIKA_SUBTITLE_OVERRIDE_COLORS` | `primary_color_rgba`、`outline_color_rgba` |
| `ERIKA_SUBTITLE_OVERRIDE_ATTRIBUTES` | `bold`、`italic`、`underline`、`strike_out` |
| `ERIKA_SUBTITLE_OVERRIDE_BORDER` | `border_style`、`outline_width`、`shadow_depth` |
| `ERIKA_SUBTITLE_OVERRIDE_ALIGNMENT` | `alignment` |
| `ERIKA_SUBTITLE_OVERRIDE_MARGINS` | `margin_left`、`margin_right`、`margin_vertical` |
| `ERIKA_SUBTITLE_OVERRIDE_BLUR` | `blur` |
| `ERIKA_SUBTITLE_OVERRIDE_ALL` | 以上全部 |

传 `0` 则全部保持回退语义，未知位会被丢弃。覆盖时字号仍与分辨率无关——`font_size`
为 `48` 时，无论脚本声明何种 `PlayResY` 都落在同样的像素上。边距则不然：libass 让
覆盖用的 margin 保持脚本自身的单位，因此在容器内的 ASS 轨上它随该脚本的 `PlayResY`
缩放，而在纯文本字幕（liberika 按画面尺寸生成脚本）上就是像素。

`ErikaHttpHeader` 定义如下：

```c
typedef struct ErikaHttpHeader {
  const char *name;
  const char *value;
} ErikaHttpHeader;
```

例如：

```c
ErikaHttpHeader headers[] = {
    {"Authorization", "Bearer token"},
    {"Referer", "https://example.com/"},
};
erika_presenter_open_with_headers(presenter, "https://example.com/video.mp4",
                                  headers, 2);
```

请求头仅对 HTTP(S) source 生效；本地文件和 Android `content://` source 会忽略它们。
调用方负责保证 URI、header 名称和值在调用期间保持有效，接口不会接管这些字符串的所有权。

播放器自己生成的请求头会被拒绝而不是合并——底层 HTTP client 是追加而非替换重复的
header：`Range`、`Host`、`Content-Length`、`Transfer-Encoding`、`Connection`
（大小写不敏感匹配）都会让调用返回 `ERIKA_STATUS_PLAYER_ERROR`。不符合 HTTP token
规则的 header 名，或包含非法字符的 header 值，同样会失败——校验发生在 `open`，
而不是第一次 range 请求时。

请求头只作用于媒体 source。外挂字幕轨道和弹幕 sidecar 文件仍然不带这些请求头拉取，
因此能通过视频认证的 token 目前无法用于这些 URL。

`set_output_headroom` 用于发布显示器当前 HDR/SDR ratio。Android API 34+ 宿主应从
`Display.registerHdrSdrRatioChangedListener` 调用它：有效 ratio 传 `known = true`，测量
不可用时传 `(1.0f, false)`。数值会清洗到 `1.0..10000.0`。wgpu 会忽略重复状态、无需
重新 attach surface 即更新后续帧 target，并且只有 known 状态或 ratio 真实变化时才增加
`headroom_updates`。有效内容 target 受配置的内容上限、正数 surface
`desired_headroom` 和已知显示 ratio 共同约束。其他 renderer 可以忽略这个 advisory 更新。

### 轨道与字幕

```c
ErikaStatus erika_presenter_add_external_subtitle(ErikaPresenterHandle *, const char *uri, int64_t *out_track_id);
ErikaStatus erika_presenter_remove_subtitle_track(ErikaPresenterHandle *, int64_t track_id);
ErikaStatus erika_presenter_select_audio_track(ErikaPresenterHandle *, int64_t track_id);
ErikaStatus erika_presenter_select_subtitle_track(ErikaPresenterHandle *, int64_t track_id);
ErikaStatus erika_presenter_track_selection(ErikaPresenterHandle *, ErikaTrackSelection *out_selection);
ErikaStatus erika_presenter_tracks(ErikaPresenterHandle *, ErikaTrackInfo *out_tracks, uintptr_t capacity, uintptr_t *out_len);
```

语义同 `ErikaHandle` 的轨道函数。

### 音频、资源与内存字体接口

```c
ErikaStatus erika_presenter_audio_only_tick( ErikaPresenterHandle *handle, ErikaPresenterStats *out_stats);
ErikaStatus erika_presenter_clear_subtitle_memory_fonts(ErikaPresenterHandle *handle);
ErikaStatus erika_presenter_get_resource_status( ErikaPresenterHandle *handle, ErikaPresenterResourceStatus *out_status);
ErikaStatus erika_presenter_get_subtitle_memory_font_info( ErikaPresenterHandle *handle, uint64_t font_id, ErikaSubtitleMemoryFontInfo *out_info);
ErikaStatus erika_presenter_get_subtitle_memory_font_status( ErikaPresenterHandle *handle, ErikaSubtitleMemoryFontStatus *out_status);
ErikaStatus erika_presenter_register_subtitle_memory_font( ErikaPresenterHandle *handle, const uint8_t *data, uintptr_t data_len, uint64_t *out_font_id);
ErikaStatus erika_presenter_select_subtitle_memory_fonts( ErikaPresenterHandle *handle, const uint64_t *font_ids, uintptr_t font_count);
void erika_subtitle_memory_font_info_free(ErikaSubtitleMemoryFontInfo *info);
void erika_subtitle_memory_font_status_free( ErikaSubtitleMemoryFontStatus *status);
```

`audio_only_tick` 用于无视频表面的音频调度；`get_resource_status` 返回渲染器资源快照。
内存字体先注册取得 ID，再按顺序选择回退字体；状态与信息内的分配用对应 `_free` 释放。

`ErikaSubtitleMemoryFontFace` 包含 face index、字体族 JSON、PostScript 名称、weight、italic 与 monospaced。`ErikaSubtitleMemoryFontInfo.faces` 和内嵌字符串随 `erika_subtitle_memory_font_info_free` 一并释放。

### 弹幕

```c
ErikaStatus erika_presenter_load_danmaku_file(ErikaPresenterHandle *, const char *uri);
ErikaStatus erika_presenter_load_danmaku_json(ErikaPresenterHandle *, const char *json);
ErikaStatus erika_presenter_add_danmaku_track_file(ErikaPresenterHandle *, const char *uri, const char *name, int64_t offset_micros, uint64_t *out_track_id);
ErikaStatus erika_presenter_add_danmaku_track_json(ErikaPresenterHandle *, const char *json, const char *name, int64_t offset_micros, uint64_t *out_track_id);
ErikaStatus erika_presenter_remove_danmaku_track(ErikaPresenterHandle *, uint64_t track_id);
ErikaStatus erika_presenter_set_danmaku_track_enabled(ErikaPresenterHandle *, uint64_t track_id, bool enabled);
ErikaStatus erika_presenter_set_danmaku_track_offset(ErikaPresenterHandle *, uint64_t track_id, int64_t offset_micros);
ErikaStatus erika_presenter_set_danmaku_global_offset(ErikaPresenterHandle *, int64_t offset_micros);
ErikaStatus erika_presenter_danmaku_tracks(ErikaPresenterHandle *, ErikaDanmakuTrackInfo *out_tracks, uintptr_t capacity, uintptr_t *out_len);
ErikaStatus erika_presenter_clear_danmaku(ErikaPresenterHandle *);
ErikaStatus erika_presenter_set_danmaku_enabled(ErikaPresenterHandle *, bool enabled);
ErikaStatus erika_presenter_set_debug_hud_enabled(ErikaPresenterHandle *, bool enabled);
ErikaStatus erika_presenter_set_danmaku_config(ErikaPresenterHandle *, ErikaDanmakuConfig config);
ErikaStatus erika_presenter_set_danmaku_config_ptr(ErikaPresenterHandle *, const ErikaDanmakuConfig *config);
ErikaStatus erika_presenter_get_danmaku_config(ErikaPresenterHandle *, ErikaDanmakuConfig *out_config);
ErikaStatus erika_presenter_set_danmaku_font(ErikaPresenterHandle *, const char *family, const char *file_path);
ErikaStatus erika_presenter_set_danmaku_block_words_json(ErikaPresenterHandle *, const char *json);
```

`load_danmaku_*` 用单条匿名轨替换当前弹幕；`add_danmaku_track_*` 构建多轨列表
（每轨带名字和时间偏移）。输入为 Bilibili XML（`*_file`，按路径/URL）或 JSON
（`*_json`，内联）。`offset_micros` 偏移某条轨的时间线；全局 offset 偏移所有轨。
`set_danmaku_config` / `_ptr` 应用完整 `ErikaDanmakuConfig`（`_ptr` 变体避免按值
传结构体）；`get_danmaku_config` 读回。布局引擎见
[danmaku_architecture.md](danmaku_architecture.md)。`set_danmaku_block_words_json`
接受一个字符串 JSON 数组用于过滤。

内联 JSON 的根可以是 item 数组，也可以是包含 `comments`、`danmaku` 或 `items`
数组的对象。每个 item 使用以下字段；括号内是兼容别名：

- `content`（`text`、`c`）：正文；缺失或空白的 item 会被跳过。
- `time`（`t`）：出现时间，单位为秒，默认 `0`。
- `type`（`mode`、`y`）：`scroll`/`1`、`bottom`/`4`、`top`/`5`、
  `reverse`/`6` 或 `special`/`7`。也可使用数值 `type_code` / `mode_code`。
- `color`（`r`）：十进制 RGB、`#RRGGBB` 或 `rgb(r,g,b)`。
- `font_size`（`fontSize`、`size`、`s`）、`opacity`（`alpha`、`a`）、
  `is_me`（`isMe`、`self`、`mine`）：可选样式和自发标记。
- `id`：可选的无符号 64 位整数或十进制字符串。省略时按整份输入中的顺序分配；
  session 会另外生成布局内部身份，因此调用方无需为了滑窗轨道稳定而合成业务 ID。

未知字段会被忽略，因此包含 `cid`、`danmakuId` 等数据源字段的标准 map 可以直接传入。

`set_debug_hud_enabled` 默认关闭。开启后 Presenter 在原生视频合成中绘制诊断 HUD，显示轨道
技术信息、播放状态、实时解码/渲染 FPS、解码/零拷贝路径、渲染和音频状态、
HDR 输出以及弹幕数量。HUD 只在已有视频帧时显示；其统计不要求调用方轮询，也不会写入
`ErikaPresenterStats`。`capture_frame_rgba` 的离屏截图不包含 HUD。

### Surface 与呈现

```c
ErikaStatus erika_presenter_attach_metal_layer(ErikaPresenterHandle *, uint64_t raw_layer, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_attach_wgpu_surface(ErikaPresenterHandle *, ErikaWgpuSurfaceKind kind, uint64_t raw_window, uint64_t raw_display, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_attach_wgpu_surface_with_output_capabilities(ErikaPresenterHandle *, ErikaWgpuSurfaceKind kind, uint64_t raw_window, uint64_t raw_display, uint32_t w, uint32_t h, double scale, ErikaSurfaceOutputCapabilities capabilities);
ErikaStatus erika_presenter_attach_windows_hwnd(ErikaPresenterHandle *, uint64_t hwnd, uint64_t hinstance, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_resize_surface(ErikaPresenterHandle *, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_detach_surface(ErikaPresenterHandle *);
```

macOS/iOS/tvOS 用 `attach_metal_layer`（`CAMetalLayer*`），Windows 用
`attach_windows_hwnd`（它是 `attach_wgpu_surface` + kind `WindowsHwnd` 的便捷封装，
传 `HWND` + `HINSTANCE`）。绑定到 surface 的渲染后端（原生 Metal、原生 Direct3D 11
或 wgpu）由 presenter 配置决定，而非 attach 调用决定。drawable 尺寸或 scale 变化时
调 `resize_surface`。

Android extended-linear 应把 Flutter Hybrid Composition `SurfaceView` 对应的
`AndroidNativeWindow` 传给 `_with_output_capabilities`。只有在显示器/surface HDR 探测成功
后才设置 `extended_linear`，只有直接 `SurfaceView` 才设置
`direct_composition = true`，并把宿主探测失败保留在 `fallback_reason`。
`desired_headroom = 0` 表示系统 auto；正数是 surface 上限，可用于 API 35 per-`SurfaceView`
`setDesiredHdrHeadroom`。liberika 仍会自行
验证 Vulkan、`Rgba16Float` 和 `ADATASPACE_SCRGB_LINEAR`；任何一项失败都会回退 SDR，
且原因可查询。

### Flutter texture surface

```c
ErikaStatus erika_presenter_attach_flutter_texture(ErikaPresenterHandle *, ErikaFlutterTextureKind kind,
                                                   int64_t texture_id, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_set_flutter_texture_buffer(ErikaPresenterHandle *, uint64_t raw_texture,
                                                       uint32_t w, uint32_t h);
```

`attach_flutter_texture` 把 presenter 绑定到由 `texture_id` 标识的
texture-registrar surface（目前为 Apple 的
`MacOsTextureRegistrar`/`IosTextureRegistrar`）。pixel buffer 由宿主持有；在**每次**
`render_tick` 之前，用 `set_flutter_texture_buffer` 选定下一帧的 GPU 目标，传入
强制转换为 `uint64_t` 的 `id<MTLTexture>` 指针，且必须是 `BGRA8Unorm` 格式并与声明的
`w`×`h` 一致。该纹理只在当帧内被借用——宿主保持所有权，可在 `render_tick`
返回后复用或释放。Flutter 插件在 macOS 上的 `ErikaTextureVideoView`
使用的就是这个 surface。

### Windows DirectComposition swap chain

```c
ErikaStatus erika_presenter_windows_composition_swapchain_iunknown(ErikaPresenterHandle *, void **out_swapchain);
```

仅限 Windows。当 presenter 通过
`attach_wgpu_surface_with_output_capabilities`（`direct_composition = true`）attach，
且以透明视频 alpha 模式或 overlay 混合播放时，liberika 会为目标 HWND 创建预乘 alpha 的
composition swap chain。此 getter 以 **AddRef 后的 `IUnknown*`** 返回它：调用者拥有返回的
COM 引用，必须自行 `Release`。解码器/设备丢失后应重新获取——liberika 会重建 swap chain
并暴露新对象；指针未变说明没有重建。

### Windows Flutter 纹理

```c
ErikaStatus erika_presenter_windows_flutter_texture_iunknown(ErikaPresenterHandle *, void **out_texture);
```

仅限 Windows，其他平台不导出此符号。返回最新已完成且不可变的 SDR Flutter GPU 帧，类型为 **已 AddRef 的 `IUnknown*`**。调用方拥有该引用，必须 `Release`；使用帧期间应保留引用，纹理内容在其整个生命周期内不会改变。输出指针为空时返回 `NullPointer`；presenter 有效但尚无已完成 Flutter 纹理时返回 `PlayerError`，并将输出置空。应先驱动渲染再请求帧。

### 渲染循环与事件

```c
ErikaStatus erika_presenter_render_tick(ErikaPresenterHandle *, double time_seconds, ErikaPresenterStats *out_stats);
ErikaStatus erika_presenter_get_stats(ErikaPresenterHandle *, ErikaPresenterStats *out_stats);
ErikaStatus erika_presenter_poll_event(ErikaPresenterHandle *, ErikaEvent *out_event);
```

每个显示帧调一次 `render_tick`（如来自 `CADisplayLink`、`CVDisplayLink` 或 Windows
帧调度器）。`time_seconds` 是该帧的宿主显示时钟（秒）——liberika 用它做 vsync 量化调度，
所以传**呈现时间戳**，不是 wall-clock 增量。若 `out_stats` 非 `NULL`，会填入流水线
计数器快照。`poll_event` 非阻塞，空闲时返回 `NoEvent`。

`get_stats` 填入同样的 `ErikaPresenterStats` 快照，但不渲染帧。当宿主采样计数器的
节奏与显示循环不同时用它；它不推进呈现。

### 按显示目标时间渲染

```c
ErikaStatus erika_presenter_render_tick_with_timing( ErikaPresenterHandle *handle, double time_seconds, const double *presentation_delay_seconds, ErikaPresenterStats *out_stats);
```

可选 delay 是从调用入口到显示目标的秒数，范围 ±0.25 秒，传 NULL 保留旧采样行为。视频、字幕和弹幕使用同一目标快照，不改变播放时钟。

### JSON 桥

对于平台通道本身就在序列化结构化参数的嵌入方（例如 HarmonyOS ArkTS 插件），
同一套 presenter 接口也以 JSON 形式提供：

```c
char *erika_presenter_invoke_json(ErikaPresenterHandle *, const char *method,
                                  const char *arguments_json);
char *erika_presenter_render_tick_json(ErikaPresenterHandle *, double time_seconds);
char *erika_presenter_poll_event_json(ErikaPresenterHandle *);
```

返回的字符串归 liberika 所有，必须用 `erika_string_free` 释放。`poll_event_json`
在无事件时返回 `NULL`——不是信封——所以 `NULL` 表示空闲而非错误。

三者的返回值都包在同一层信封里：

```json
{ "ok": true,  "status": 0, "value": <result> }
{ "ok": false, "status": 3, "error": "<message>" }
```

`arguments_json` 必须是 JSON 对象。`method` 选择
`crates/erika_capi/src/presenter_json.rs` 中分派表（`JSON_METHODS`）的一项。分派表
与 C 入口对应：播放（`open`、`play`、`pause`、`stop`、`close`、`seek`、
`setPlaybackRate`、`setVolume`）、轨道与字幕（`tracks`、`trackSelection`、
`selectAudioTrack`、`selectSubtitleTrack`、`addExternalSubtitle`、
`removeSubtitleTrack`、`setSubtitleScale`、`setSubtitleFont`、`setSubtitleStyle`、
字幕内存字体系列）、弹幕系列（含 `getDanmakuConfig`）、输出与超分（`setUpscaler`、
`getUpscalerStatus`、`setOutputHeadroom`、`getOutputStatus`、`setDebugHudEnabled`）、
计数（`getPresenterStats`、`getResourceStatus`），以及下文的诊断
（`getDiagnosticsSnapshot`、`getLogs`）。

`erika_presenter_invoke_json` 同时是进入内核的通用透传通道：

- `listMethods` 返回分派表本身——`{schema: 1, fields, methods: [{name, scope, summary,
  arguments: [{name, type, required}], returns, blocking, writesFiles}]}`——宿主（或其后的智能体）无需改代码
  就能发现并转发能力。登记一项能力 = 在 `invoke` 加一个 `match` 分支，并在
  `JSON_METHODS` 加一项；两者不一致时单元测试会失败。
- 每一项还带 `blocking`（调用在调用线程上做完才返回，耗时随输入增长：媒体长度、文件大小）和
  `writesFiles`（调用会在磁盘上创建文件，结果列出路径）；顶层 `fields` 写明这两个字段。
  `exportFrame`、`exportFrames` 用上面的单帧导出从媒体取画面：给出 `outputPath` / `outputDir` 时写
  PNG 或 RGBA 文件并返回路径，`exportFrame` 不给 `outputPath` 时返回 base64 编码的字节。
- `scope` 为 `global` 的方法（`listMethods`、`getLogs`、`exportFrame`、`exportFrames`）接受 `NULL` 句柄。
- `invokeJson`（`{method, arguments}`）调用表中另一方法，并把它的完整信封作为
  `value` 返回。会在原生侧拆信封的平台插件（Linux、鸿蒙、Android）用它原样转发 Dart
  的 `ErikaPlayer.invokeJson`。
- 未知方法返回 `{"ok": false, "status": 3, "code": "unknown_method", "method":
  "<名称>", "error": "unsupported Erika presenter method: <名称>"}`；`arguments_json`
  格式错误时带 `"code": "invalid_arguments"`。两者都不会中止。

这层桥调用的就是上面这些函数，没有额外能力。能在平台通道上传结构体的宿主也可以用
类型化入口。

### 诊断与截图

```c
ErikaStatus erika_presenter_get_upscaler_status(ErikaPresenterHandle *, ErikaUpscalerStatus *out_status);
ErikaStatus erika_presenter_get_output_status(ErikaPresenterHandle *, ErikaOutputStatus *out_status);
ErikaStatus erika_presenter_capture_frame_rgba(ErikaPresenterHandle *, uint32_t width, uint32_t height,
                                               uint8_t *out_rgba, uintptr_t out_capacity);
```

`get_upscaler_status` 报告请求的超分模式、当前后端（off / inactive / building /
scalar / simdgroup-matrix）、fallback 次数、超分帧数，以及最近的 encode/GPU 耗时
（微秒）。

`get_output_status` 返回实际协商到的输出，而不只是请求值。13 个字段如下：

| 字段 | 含义 |
|------|------|
| `requested_mode` | 创建时请求的 `ErikaPresenterOutputMode`。 |
| `active_encoding` | 实际的 `SdrSrgb`、`AppleEdr`、`AndroidExtendedLinearScRgb`、`Hdr10Pq` 或 `LinuxExtendedLinearScRgb` encoding。 |
| `surface_format` | 实际 8-bit UNORM、10-bit UNORM 或 16-bit float surface class。 |
| `native_data_space` | Android `ANativeWindow` dataspace；`406913024` 为 `SCRGB_LINEAR`，`-1` 表示不可用/不适用。 |
| `requested_headroom` | 清洗后的请求内容 headroom 上限，最小 `1.0`。 |
| `active_headroom` | 已知时为当前显示器 HDR/SDR ratio；未知时为有效内容 fallback 值。 |
| `active_headroom_known` | `active_headroom` 是否来自权威平台 ratio；API 34+ ratio 可用时 Android 设为 true。 |
| `extended_linear_active` | FP extended-linear 呈现路径是否激活；用 `active_encoding` 区分 Apple EDR 与 Android scRGB。 |
| `fallback_reason` | 解释请求模式为何未激活的稳定 `ErikaOutputFallbackReason`。 |
| `fallback_count` | 已记录的输出回退 transition/failure 次数。 |
| `data_space_failures` | dataspace/输出色彩空间验证失败次数。 |
| `headroom_updates` | 运行时 headroom 状态真实变化次数；重复发布相同 ratio/known 不增长。 |
| `extended_linear_frames` | 通过 active extended-linear 路径呈现的帧数。 |

Fallback 数值是稳定 ABI；新增原因只能追加，不能重排 `0..8`：

| 码 | 枚举 | 稳定 label | 含义 |
|----|------|------------|------|
| 0 | `None` | `none` | 无回退。 |
| 1 | `DisplayHdrUnsupported` | `display_hdr_unsupported` | 显示器/surface HDR 能力探测失败。 |
| 2 | `HybridCompositionRequired` | `hybrid_composition_required` | Android surface 不是直接合成的 `SurfaceView`。 |
| 3 | `WgpuBackendNotVulkan` | `wgpu_backend_not_vulkan` | 当前 wgpu backend 不是 Vulkan（如 GLES）。 |
| 4 | `Rgba16FloatSurfaceFormatUnavailable` | `rgba16float_surface_format_unavailable` | Surface capabilities 没有 `Rgba16Float`。 |
| 5 | `NativeWindowDataSpaceApiUnavailable` | `native_window_dataspace_api_unavailable` | `ANativeWindow_*DataSpace` API 不可用（含 API 26/27）。 |
| 6 | `ScrgbDataSpaceVerificationFailed` | `scrgb_dataspace_verification_failed` | `SCRGB_LINEAR` set/readback 未通过验证。 |
| 7 | `SurfaceConfigureFailed` | `surface_configure_failed` | 请求的输出 surface configure 失败。 |
| 8 | `LegacyAppleEdrUnsupported` | `legacy_apple_edr_unsupported` | 在未实现 Apple EDR 的 backend 上请求了该模式。 |

`capture_frame_rgba` 是**截图**：把当前合成帧（视频 + 字幕，不含弹幕）离屏渲染进调用方
分配的 RGBA8 缓冲，按请求的 `width`×`height`（与显示 surface 尺寸无关）。
`out_capacity` 至少为 `width*height*4`。尚无可用帧时返回 `PlayerError`。Metal 与
wgpu（包括 Android）已实现截图；当前 D3D11 backend 尚未实现。截图始终使用 SDR RGBA8
离屏 target，并对 HDR/extended-linear 内容 tone-map，因此即使显示输出是 Apple EDR、
HDR10 或 Android extended-linear scRGB，返回字节仍是 SDR。

```c
uint32_t w = 1920, h = 1080;
uint8_t *rgba = malloc((size_t)w * h * 4);
if (erika_presenter_capture_frame_rgba(p, w, h, rgba, (uintptr_t)w * h * 4) == ErikaStatus_Ok) {
    /* rgba 为 w*h 紧排的 RGBA8 像素——可编码为 PNG 等 */
}
free(rgba);
```

### 智能体诊断（JSON）

```c
ErikaStatus erika_presenter_get_diagnostics_json(ErikaPresenterHandle *, char **out_json);
ErikaStatus erika_log_read_json(uint32_t limit, int32_t min_level, uint64_t since_seq,
                                char **out_json);
```

新的诊断数据一律以 JSON 文档返回，不给上面的结构体加字段（Apple 插件手写镜像这些
结构体，加字段会迫使动态库与插件锁步升级）。两个符号都应按可选方式加载，缺失即视为
“不支持”。返回 `Ok` 时 `*out_json` 是调用方所有的 UTF-8 文档，用 `erika_string_free`
释放；其它状态下为 `NULL`。每个文档带 `schema`（当前为 `1`），同一 schema 内只增字段。
取不到的值为 `null`，不会填假的 0。时长单位为毫秒（微秒精度）。

#### 诊断快照

`erika_presenter_get_diagnostics_json` 一次取全。它只读 presenter 字段、渲染/音频计数
和无锁的播放计数：不解码、不渲染，也从不等待播放线程（macOS release 构建播放中实测
单次中位数约 0.3 ms，含 JSON 编码）。线程规则与其它句柄调用相同。

| 段 | 内容 |
|----|------|
| `schema`、`capturedAtUnixMs` | 文档版本与采集时刻（墙钟）。 |
| `playback` | `state`、`playing`、`mediaTimeSeconds`、`durationSeconds`、`playbackRate`、`generation`、`audioOnly`（后台暂停视频解码）、`buffering` `{active, events, currentMs, totalMs}`。 |
| `media` | 选中的 `video`（`codec`、`profile`、`level`、`width`、`height`、`pixelFormat`、`frameRate`、`bitRate`）、`audio`（`codec`、`sampleRate`、`channels`、`sampleFormat`、`bitRate`）、`subtitle`，没有则为 `null`。 |
| `decoder` | 最近一次解码器切换，可直接查询而不再只靠事件：`requestedBackend`、`activeBackend`、`previousBackend`、`hardware`、`fellBack`、`fallbackCount`、`stage`、`reason`、`codec`、`pixelFormat`、`lineSizes`。首次切换前为 `null`。 |
| `video` | 解码/渲染帧、硬件/软件/零拷贝/直接零拷贝/共享句柄帧、`cpuFallbackFrames`、`importFailures`、`renderFailures`、`backpressureDrops`、`rejectedImportRoute`。 |
| `presenter` | surface 挂载与尺寸，最近一次 tick/pump/音频/字幕/视频/时钟/弹幕规划/渲染/GPU 耗时，测试帧/叠加/字幕/弹幕计数。 |
| `output` | 请求模式、实际编码、surface 格式、dataspace、请求/实际余量（未知为 `null`）、回退原因标签与计数。 |
| `hdr` | HDR 源帧、HDR10 输出帧、色调映射到 SDR 的帧、元数据更新/失败、输出失败、`outputActive`。 |
| `resources` | 与 `ErikaPresenterResourceStatus` 相同。 |
| `upscaler` | 请求模式与实际后端（标签）、回退次数、超分帧数、最近编码耗时。 |
| `audio` | 输出状态、configured/started、音量、推入帧、失败数、队列帧与 `queuedMs`、读/写/欠载帧、环形缓冲 `ringDroppedFrames`、设备恢复 `recovery`。 |
| `buffers` | 播放线程发布的队列深度 `playback`（`demuxPackets`、`pendingVideoPackets`、`decodedVideoFrames`、`decodedAudioFrames`、`decodedSubtitleFrames`，形如 `{queued, capacity}`；`demuxEof`、`updatedAgoMs`；open 前为 `null`）、presenter 帧通道、`audioOutputQueuedMs`。 |
| `timing.scheduler` | `leadTimeMs`、`dropToleranceMs`、`maxConsecutiveDrops`；`presentedFrames`；`lateDroppedFrames`（因超过丢帧容忍度被丢弃，过去没有计数）；`forcedLatePresents`（超过容忍度但连续丢帧额度用尽而照常显示）；`maxDropBurst`、`maxDroppedLateMs`、`lastDropAgoMs`、`seekPrerollDroppedFrames`；最近 128 个已呈现帧的 `lateByMs` 窗口 `{samples, meanMs, p50Ms, p95Ms, maxMs}`；`lateHistogram`（`le10ms` … `gt120ms`）。 |
| `timing.presentation` | 在 liberika 渲染处测得，并非显示器真实 vsync：`ticks` 与 `tickIntervalMs`；`uploadedFrames`、`newFramesPresented`、`supersededBeforePresent`；`frameIntervalMs` 与 `jitterMs`（相对源帧间隔÷倍速的绝对偏差）；`longFrames`（在屏时长超过 `longFrameFactor` = 1.5 个源帧间隔）；`lastPresentAgoMs`。 |
| `timing.clock` | 音频主时钟纠偏（过去只写 trace）：模式、同步参数（`deadbandMs`、`maxCorrectionPerFrameMs`、`snapThresholdMs`、`audioLeadTimeMs`）、`observations`、`corrections`、`snaps`、`forwardCorrections`、`backwardCorrections`、`lastDriftMs`（正值：音频领先播放时钟）、`maxAbsDriftMs`、`appliedTotalMs`、`lastCorrectionAgoMs`、`lastSnapAgoMs`、`absDriftMs` 窗口。 |

`timing` 与 `buffers.playback` 中的计数在 presenter 打开新媒体时清零。

#### 日志环形缓冲

`erika_log_read_json` 导出进程级内存环形缓冲（最多 2000 条 / 512 KiB，单条正文上限
4 KiB）。无需句柄，任意线程可调用。收集内容：

- liberika 原本写到 stderr / logcat / hilog 的全部运维诊断（这些输出保持不变）；
- 过去是裸 `eprintln!` 的运维失败（音频 configure/push/start/pause/reset、渲染器
  clear、字幕渲染与解码器打开、WASAPI、D3D11 设备移除、超分被禁用）；
- 警告及以上的 FFmpeg `av_log` 消息。回调是进程级的，所有播放器共享：在栈缓冲里格式化，
  从不阻塞（争用时计入 `droppedContended`），只差数字/地址的连续重复合并为一条并计
  `count`，FFmpeg 新条目每秒最多 50 条（其余计入 `rateLimited`）。FFmpeg 自身的 stderr
  输出照旧。`ERIKA_FFMPEG_LOG_CAPTURE=0` 关闭捕获；使用发行版共享 FFmpeg（Linux 系统库）
  时默认不捕获，需 `ERIKA_FFMPEG_LOG_CAPTURE=1`，因为进程里其它库可能占用该回调。

参数：`limit` 过滤后最新的条数（`0` = 全部保留条目）；`min_level` 为 `ErikaLogLevel`
（`Trace` 0 … `Error` 4）或 `-1` 表示全部；`since_seq` 只取 `seq >= since_seq` 的条目
（传上次的 `nextSeq` 即增量读取）。

```json
{
  "schema": 1,
  "capacity": { "entries": 2000, "bytes": 524288, "entryTextBytes": 4096 },
  "retained": 15, "retainedBytes": 6011, "oldestSeq": 1, "nextSeq": 16,
  "evicted": 0, "rateLimited": 0, "droppedContended": 0,
  "ffmpegCapture": "active", "redacted": true,
  "entries": [
    { "seq": 3, "unixMs": 1761134400123, "level": "error", "source": "ffmpeg",
      "event": null, "count": 1,
      "message": "[mpeg4 @ 0x…] Failed setup for format videotoolbox_vld: hwaccel initialisation returned error." },
    { "seq": 4, "unixMs": 1761134400140, "level": "warn", "source": "erika",
      "event": "video_decoder", "count": 2, "lastUnixMs": 1761134400410,
      "data": { "event": "video_decoder", "stage": "open_avcodec_to_software" } }
  ]
}
```

条目按时间从旧到新。结构化诊断解析为 `data`，其它行为 `message`。`ffmpegCapture` 取值
`not_installed`（尚未用到 FFmpeg）、`active`、`disabled`、
`shared_library_opt_in_required`。

级别：已知严重程度的调用点显式传入；FFmpeg 的 PANIC/FATAL/ERROR → `error`、
WARNING → `warn`；libass 0–1 → `error`、2 → `warn`。其它结构化诊断按 `event`/`stage`
名称给默认级别：fatal/panic 为 `error`；失败、超时、资源不可用/丢失/被拒/缺失、卡住与
回退（包括 `*_to_software`、`*_to_bytebuffer` 路径）为 `warn`；其余为 `info`。

脱敏在导出时进行：URL 的用户信息以及 `?`/`#` 之后的全部内容、凭据类 `key=value` /
`key: value` 与 JSON 键（`token`、`api_key`、`access_token`、`authorization`、
`cookie`、`X-Emby-Token` 等）、`Bearer`/`Basic` 凭据、当前用户主目录（替换为 `~`），以及
其它 `/Users/<user>`、`/home/<user>`、`C:\Users\<user>` 路径段。缓冲只存在于进程内存，
按需读取。

## 枚举

| 枚举 | 取值 |
|------|------|
| `ErikaState` | `Idle` `Opening` `Ready` `Playing` `Paused` `Stopped` `Closed` `Error` |
| `ErikaEventKind` | `None` `StateChanged` `DurationChanged` `PositionChanged` `TracksChanged` `BufferingChanged` `VideoParamsChanged` `VideoDecoderChanged` `AudioOutputChanged` `SurfaceAttached` `SurfaceDetached` `Error` `TrackSelectionChanged` |
| `ErikaTrackKind` | `Video` `Audio` `Subtitle` |
| `ErikaTrackSource` | `Embedded` `External` |
| `ErikaWgpuSurfaceKind` | `Unknown` `MacOsNsView` `MacOsCaMetalLayer` `IosUiView` `WindowsHwnd` `XlibWindow` `WaylandSurface` `AndroidNativeWindow` `OhosNativeWindow` |
| `ErikaFlutterTextureKind` | `Unknown` `MacOsTextureRegistrar` `IosTextureRegistrar` `AndroidSurfaceTexture` `WindowsTextureRegistrar` `LinuxTextureRegistrar` |
| `ErikaVideoAlphaMode` | `Opaque` `PackedAlphaRight` |
| `ErikaPresenterOutputMode` | `Auto` `Sdr` `AppleEdr` `ExtendedLinear` |
| `ErikaActiveOutputEncoding` | `SdrSrgb` `AppleEdr` `AndroidExtendedLinearScRgb` `Hdr10Pq` `LinuxExtendedLinearScRgb` |
| `ErikaOutputSurfaceFormat` | `EightBitUnorm` `TenBitUnorm` `SixteenBitFloat` |
| `ErikaOutputFallbackReason` | `None` `DisplayHdrUnsupported` `HybridCompositionRequired` `WgpuBackendNotVulkan` `Rgba16FloatSurfaceFormatUnavailable` `NativeWindowDataSpaceApiUnavailable` `ScrgbDataSpaceVerificationFailed` `SurfaceConfigureFailed` `LegacyAppleEdrUnsupported` |
| `ErikaLumaUpscalerMode` | `Off` `ArtCnnC4F16` `ArtCnnC4F32` `ArtCnnC4F16Ds` |
| `ErikaUpscalerBackendStatus` | `Off` `Inactive` `Building` `Scalar` `SimdgroupMatrix` |

## 结构体

- **`ErikaPresenterConfig`** `{ int32 output_mode; float edr_headroom; int32 luma_upscaler; int32 video_alpha_mode; }` —
  按值传给 `create_with_config`；`video_alpha_mode` 是 `ErikaVideoAlphaMode`
  （默认 `Opaque`，左右分区颜色/alpha 素材用 `PackedAlphaRight`）。
- **`ErikaSurfaceOutputCapabilities`** `{ bool extended_linear; bool direct_composition; float desired_headroom; int32 fallback_reason; }` —— attach 时传入的 Android 宿主显示器/surface 探测结果；`desired_headroom == 0` 表示系统 auto。
- **`ErikaUpscalerStatus`** —— 请求模式、当前后端、fallback 次数、超分帧数、最近
  encode/GPU 微秒。
- **`ErikaOutputStatus`** —— 上文“诊断与截图”说明的 13 字段协商输出快照。
- **`ErikaDanmakuConfig`** —— 完整弹幕布局/外观配置（字号、不透明度、显示区域、滚动
  时序、碰撞/堆叠开关、屏蔽模式、阴影样式）。`font_size` 是 NipaPlay/Flutter 的*逻辑*
  字号；liberika 会乘以 surface scale 得到 glyph 像素。
- **`ErikaDanmakuTrackInfo`** `{ id, enabled, offset_micros, item_count, char *name, char *source }` —
  用 `erika_danmaku_track_info_free` 释放。
- **`ErikaVideoParams`** `{ width, height, primaries, transfer }` —— 通过
  `VideoParamsChanged` 上报的色彩元数据。
- **`ErikaTrackCounts`** / **`ErikaTrackSelection`** —— 各类轨道计数 / 选中 id
  （`-1` = 无）。
- **`ErikaTrackInfo`** —— 完整的每轨元数据；六个 `char*` 字段归调用方所有（用
  `erika_track_info_free` 释放）。视频轨还包含 `bit_rate`（bit/s）和
  `frame_rate_numerator` / `frame_rate_denominator`（`0` = unknown）。
- **`ErikaEvent`** —— 用结构体表达的 tagged union：`kind` 决定哪些字段有意义
  （`state`、`duration_micros`、`position_micros`、`buffering`、`video`、`tracks`）；
  `Error` 事件由 `status` 携带状态码。
- **`ErikaPresenterStats`** —— 流水线计数器：解码/渲染帧数、推送音频帧、overlay/弹幕
  帧、硬解 vs 软解 vs 零拷贝帧数、HDR 源/HDR10 输出/SDR tonemap 计数、音频时钟
  read/queued/underflow 帧数，以及最近渲染耗时。

## 事件

每次循环迭代轮询并按 `kind` 分发：

```c
ErikaEvent ev;
while (erika_presenter_poll_event(p, &ev) == ErikaStatus_Ok) {
    switch (ev.kind) {
        case ErikaEventKind_StateChanged:    /* ev.state */                 break;
        case ErikaEventKind_DurationChanged: /* ev.duration_micros */        break;
        case ErikaEventKind_PositionChanged: /* ev.position_micros */        break;
        case ErikaEventKind_TracksChanged:   /* 重新查询 erika_*_tracks */    break;
        case ErikaEventKind_BufferingChanged:/* ev.buffering */              break;
        case ErikaEventKind_VideoParamsChanged: /* ev.video */               break;
        case ErikaEventKind_Error:           /* ev.status + last_error */    break;
        default: break;
    }
}
```

事件队列有界，靠轮询抽干；停止轮询的宿主只是停止观察状态。`position_micros` 在播放
中周期性发出。

## 最小 presenter 集成（C）

```c
#include "erika.h"

ErikaPresenterHandle *p = erika_presenter_create();
erika_presenter_attach_metal_layer(p, (uint64_t)layer, w, h, scale);  // 或 attach_windows_hwnd
if (erika_presenter_open(p, "/path/to/video.mkv") != ErikaStatus_Ok) {
    char *m = erika_last_error_message(); /* 记录 */ erika_string_free(m);
}
erika_presenter_play(p);

// 每个显示帧:
ErikaPresenterStats stats;
erika_presenter_render_tick(p, host_time_seconds, &stats);
ErikaEvent ev;
while (erika_presenter_poll_event(p, &ev) == ErikaStatus_Ok) { /* 分发 */ }

// 释放:
erika_presenter_detach_surface(p);
erika_presenter_destroy(p);
```

各平台的 surface 与显示定时器细节见 [integration.zh.md](integration.zh.md)，以及可运行的
[`macos_native_demo`](../examples/macos_native_demo) /
[`windows_native_demo`](../examples/windows_native_demo) 示例。
