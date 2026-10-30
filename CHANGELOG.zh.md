# 更新日志

[English](CHANGELOG.md) · [日本語](CHANGELOG.ja.md)

## 0.3.0 - 2026-10-25

### 默认行为变化

- 原生字幕超采样默认启用。原生和 Web 后端的字幕与弹幕超采样现均默认开启，同时保留原有尺寸限制、原生字幕 CPU 回退及显式关闭选项。
- 软件视频解码在播放和导出时均使用帧与切片多线程（线程数由 FFmpeg 选择）；`ERIKA_VIDEO_DECODE_THREADS=1` 恢复单线程。强制软解时，4K H.264 以 3x 播放达到 72 fps、无延迟丢帧；单线程为 7 fps，15 秒内丢弃 481 帧，跳转到首帧的中位时间由 2.8 秒降至 0.43 秒。`ERIKA_HWDEC=software` 可在所有平台默认使用软解以便测量。

### 功能与修复

- 项目与仓库更名为 liberika 和 liberika-swift。`erika` Rust crate、`erika_*` C ABI 与现有 SDK 包名保持兼容。
- Release 新增 `liberika-web-0.3.0.zip`，含完整 Web 运行时、演示页和许可证，摘要列于 `SHA256SUMS`。
- presenter JSON 新增容器章节（`chapters`），宿主无需更改现有 C 结构体即可访问（#172）。
- 修复暂停时内嵌字幕送达、跨越跳转位置的长字幕，以及暂停播放中切换字幕轨后的当前字幕（#171）。
- 修复 iOS/tvOS 可选导出和诊断符号的静态链接：仅强制链接所选原生归档中存在的符号，同时仍可链接旧版锁定运行时（#170）。
- `erika_play` 跟踪日志新增失败原因，便于诊断播放启动失败。
- 将最后 4 MiB 内普通未送达的 HTTP 尾部范围视为 EOF；范围验证失败和实体变化仍作为错误处理（#169）。
- 修复弹幕包含内置回退字体未覆盖字符（如 emoji）时的内存激增。系统字体回退先以零拷贝查询字符覆盖情况，再加载字面，并以 LRU 淘汰限制常驻回退字体数量，同时保留原有回退顺序；单个未覆盖字符不再复制所有系统字体（报告的 macOS 场景约 1.65 GB）到内存。
- 删除重复的 Web 回退字体资源。libass 使用已嵌入 Rust WASM 的完整字体，省去单独字体请求及约 6.07 MiB 包数据（gzip 后 2.74 MiB）；Web 增量构建也会删除旧的独立字体文件。
- 新增浏览器后端及 `cargo run -p xtask -- web build`，将原生 Rust 弹幕引擎、打补丁的 libass/文本库和 FFmpeg 解封装器打包为 WASM。WebCodecs 解码音视频，Web Audio 驱动时钟，WebGL2 合成视频、字幕和弹幕。
- 浏览器支持 MKV 内嵌 ASS/SSA 与字体附件、外部 ASS/SSA/SRT/WebVTT、字幕与弹幕独立超采样、实时样式更新、追加弹幕文件、本地评论、轨道选择、章节和 PNG 截图。本地文件分块读取，远程源使用有界 HTTP Range 缓存。
- Web 后端已接入 Erika 浏览器应用，并在 Chrome 中验证 H.264/AAC/ASS MKV、VP9/Opus WebM、2,000 条弹幕、播放控制、截图、UI 切换及播放器清理。Web 可通过 Release 归档或源码获取；见[浏览器指南](docs/web.zh.md)。
- 修复对忽略 Range、返回整个文件和 `200` 的源站的 HTTP(S) 读取和持续预取，包括无 Content-Length 的 HLS 播放列表与分片。缓存前丢弃重播前缀，有界读取在请求长度处停止，EOF 可提供总大小。重试拒绝变化的实体验证器，避免混合文件版本；实体验证失败也会阻止同步回退和后续读取，弱 ETag 不能用于授权续传部分响应。
- 新增进程级诊断日志环，通过 `erika_log_read_json` 读取。它保存带级别和时间戳的运行诊断、先前直接写 stderr 的运行故障，以及经永不阻塞、合并重复项并限制洪泛的 `av_log` 桥接获取的 FFmpeg 警告和错误。导出时会脱敏 URL 查询串和用户信息、凭据及用户主目录。现有 stderr、logcat 和 hilog 输出不变；`ERIKA_FFMPEG_LOG_CAPTURE=0` 关闭 FFmpeg 捕获，Linux 链接系统共享 FFmpeg 时仅在 `ERIKA_FFMPEG_LOG_CAPTURE=1` 时捕获。
- 新增 `erika_presenter_get_diagnostics_json`，一次获取解码器和硬件路径、presenter 计数器、输出/HDR、资源、超分、音频、缓冲深度和时序的 JSON 快照。时序新增调度器因迟到丢弃的帧、耗尽丢帧预算后仍显示的迟到帧、呈现节奏和抖动、音频时钟漂移及纠正和跳变计数。现有结构体不变。
- `erika_presenter_invoke_json` 现为可发现的透传接口：统一分发表驱动，`listMethods` 返回方法、参数与结果，`invokeJson` 嵌套调用，`listMethods`/`getLogs` 接受 NULL 句柄，未知方法返回 `"code": "unknown_method"`。表中新增 `getDiagnosticsSnapshot`、`getLogs`、`setDebugHudEnabled`、`setOutputHeadroom`、`trackSelection`、`getDanmakuConfig`、`setSubtitleFont` 和 `setSubtitleStyle`；Android JSON 桥接对自身未实现的方法回退到该表。
- 修复 FFmpeg 在解码器打开后放弃 VideoToolbox 或 D3D11VA 时的解码器报告：硬件加速初始化失败或流不提供硬件格式时，FFmpeg 会重新协商并软解，但 liberika 过去仍报告硬件后端且无回退。现记录格式协商，播放会话发出解码器变化（`hwaccel_<backend>_to_software`，硬件恢复时为 `…_restored`），附软件像素格式和原因，计数回退并标记实际解码后端。
- 新增 `erika_export_frame` 和 `erika_frame_export_result_free`：无界面单帧导出从独立解封装器和多线程软解器返回媒体时间对应的画面（RGBA 或 PNG），可按指定边界缩放。
- 新增帧导出会话 `erika_frame_export_session_open`、`erika_frame_export_session_frame` 和 `erika_frame_export_session_close`，一次打开媒体后连续取帧。后续请求与前次之间没有关键帧时，根据解封装器 seek 索引从上次停止位置继续解码，否则跳转。4096×1744 H.264 文件中间隔 100 ms 的 60 帧，逐次 `erika_export_frame` 需 23.9 秒（60 次打开与跳转），会话需 1.0 秒（一次跳转，解码 335 帧）。JSON 方法表新增全局 `exportFrame` 和 `exportFrames`，每项现标明 `blocking` 与 `writesFiles`。
- 帧导出按视频渲染器对 SDR 显示器使用的算法将 HDR 源（PQ、HLG、Dolby Vision）映射到 SDR：目标 203 nit 的 ITU-R BT.2390 EETF、BT.2020 到 BT.709 色域映射、gamma 2.2；并在结果的 `reserved_flags` 设置 `ERIKA_FRAME_EXPORT_TONE_MAPPED`。与 SDR 显示器的播放器截图相比，4K HDR10 帧平均偏差为 0.1–0.3 / 255（此前 7–9）。
- 修复无媒体打开时，宿主通过 `erika_presenter_select_subtitle_track`（及 `Player::select_subtitle_track`）选择无字幕以清除字幕返回 `ErikaStatus 3` 的问题；现返回成功且不更改状态。

## 0.2.1 - 2026-10-02

### 播放与 HTTP(S)

- 恢复原生硬件解码器无法处理源时的软件回退，包括 WMV/WMA 与通过内置 dav1d 解码的 8/10-bit AV1。Windows D3D11 一致处理软件解码帧上传。
- HTTP I/O 在连接、TLS、响应头和正文读取期间均可取消。源和解封装器销毁时取消 I/O 并等待所属工作线程；stop 释放 HTTP 缓冲但保留重播能力，close 销毁会话。
- 在每个持续流分段边界强制执行预读窗口。

### Flutter 与 OpenHarmony

- Flutter 和独立 OpenHarmony 打开选项新增 `httpBackBufferBytes`。现有 SDK 桥接均将回退预算传入原生引擎；零或省略保留 16 MiB 默认值。
- iOS 在显示呈现目标时刻采样弹幕和字幕，使用合并时间戳邮箱和可选 `erika_presenter_render_tick_with_timing`；旧库保留原有 render-tick 行为。

### 实验性 Linux 支持

- 新增 X11/Wayland 上原生与 Flutter Linux 播放、PulseAudio 输出、NVDEC/VA-API 硬解和软件回退。
- 新增原生 Wayland 视频子表面、实验性 VA-API Vulkan 直接帧导入及可选 WSL D3D12 GPU 平面复制。Linux 实际 HDR 与 Intel/AMD 直接帧导入仍未验证；Linux 需源码构建，不包含在预编译 Release 归档中。
- 修复 P010 插值，保留完整信号精度。

## 0.2.0 - 2026-09-23

### 播放

- HTTP(S) 预取改用一个持久流及开放区间 GET（`bytes=anchor-`），避免每 4 MiB 发请求和服务端跳转，也不下载重叠尾部。预读窗口满时工作线程停止读 socket；跳转通过 epoch 计数淘汰旧流。短响应和正文错误携带实体验证器从已到达偏移续传；反复失败后回退到有界同步请求。缓存追加保持字节连续；已知 EOF 时最后的部分读取无需等待缓冲填满。
- 打开选项通过 `http_back_buffer_bytes` 让宿主调整回退预算（使用首个保留槽，ABI 和布局不变；零保留 16 MiB 默认值）。高码率媒体应按码率配置：71 Mbps 回退 10 秒约需 89 MB，固定默认值不足。
- 同步 HTTP(S) 范围请求限制为 4 MiB，避免大窗口要求不现实的持续带宽（单个 32 MiB 请求需约 18 Mbps 才能在 ureq 的 15 秒正文期限内完成）。后台失败降级为同步获取，不终止读取；持续返回数据的源站比固定三次规则允许更多续传尝试。
- 解封装错误显示媒体源自身错误（HTTP 状态、超时），不再只显示 `Input/output error (-5)`。

### 渲染器

#### Dolby Vision 和 HDR 色调映射

- Dolby Vision RPU 映射使用 libplacebo 风格的分段多项式/MMR 重塑，再应用 RPU 非线性矩阵、PQ 线性化和 LMS→RGB 合成。Profile 5/8 VUI 标签强制为 BT.2020/PQ。
- 无法为硬件帧附加 RPU side data 的移动后端（MediaCodec / 通用 AvCodec）对 Profile 5 回退软解；VideoToolbox 和 D3D11VA 保持硬解。
- 不再直接拒绝 Profile 7 FEL/MEL RPU。`disable_residual_flag` 要求增强层时仍应用基础层重塑曲线、色彩矩阵与 L1 调整；此前丢弃整个 RPU 会丢失这些信息，使帧变成普通 HDR10。不可合成的残差通过新 `Frame::dovi_el_status()` 和每流一条限频 `dovi_el_not_composed` 诊断报告，与 libplacebo 区分 `nlq_active` 和“未绑定增强层的消费者不得读取这些字段”的方式一致。
- 逐帧 Dolby Vision L1 亮度（`min_pq`/`max_pq`/`avg_pq`）驱动色调映射源峰值与场景平均枢轴；静态母版显示器（L0）峰值仍决定输出模式协商及是否启用色调映射/感知色域 LUT。
- 色调映射切换到 libplacebo 风格 IPT 域色彩映射，**默认算子为 BT.2390 EETF**，另支持 spline、BT.2446 method A、SMPTE ST 2094-10、Mobius、Reinhard 和 clip。黑点补偿使用 `target black = target peak / contrast_ratio`（SDR 自动为 1000:1）。
- 当色调映射的 HDR 源压缩到更小色域时，使用 CPU 生成的 48×32×256 IPT 3D LUT（遵循 libplacebo 格点和索引映射）进行感知色域映射；后台线程生成，避免首个宽色域帧阻塞渲染线程。色度滚降目前为简化死区混合，非 libplacebo 完整的逐色相边界搜索。
- 场景自适应 HDR10：没有 DoVi L1 的软解 PQ 帧使用 CPU 测量的场景平均亮度（`luma_stats`）驱动色调映射枢轴，与 mpv 的 `--hdr-compute-peak` 相呼应。
- Metal EDR 协商读取实际呈现屏幕而非 `NSScreen.mainScreen`，Auto HDR 由该显示器能力决定。调用方必须设置 `CAMetalLayer.delegate`（示例已更新）。
- Metal 仅在帧解析到的纹理实际匹配其（source、target、target-peak）键时标记感知色域 LUT 为活动。以前在新 LUT 生成期间改变 headroom 或原色，会绑定 1×1×1 占位纹理但 `gamut_lut_enabled` 仍启用，造成黑帧直到生成完成。D3D11 在 LUT 生成期间也以相同方式屏蔽 uniform。
- ST 2094-10 色调映射在 PQ 域选择 knee，与 libplacebo 内部重缩放一致；`ToneMapConfig::curve_param` 现调节 knee 自适应（默认 0.70，即 libplacebo 的 `param_def`）。此前参数无效且 knee 在线性 nits 中选择。
- 色调映射、黑点补偿和感知色域 LUT 根据静态母版显示器 L0 峰值启用，不再根据逐帧 Dolby Vision L1 峰值，从而避免暗场景中它们一帧关闭后又恢复。
- Metal EDR / extended-linear 输出在合成到线性 drawable 前将字幕和弹幕颜色线性化；此前直接写入 gamma 编码颜色使中间调及彩色文字过亮。
- HDR10 场景亮度测量遵循帧的颜色范围：有限（TV）范围亮度平面如着色器的 `expand_ycbcr_range` 一样扩展到合法码值范围，不再按全范围测量。同时区分 `yuv420p10le`（10-bit 码值在 `[9:0]`）和 P010（左对齐 `[15:6]`）；此前两者都按 P010 处理会将软解 Main10 HDR10 测为接近黑色。
- 感知色域 LUT 的 I 轴现以 PQ 码覆盖目标 `[black, peak]`（libplacebo 的 `gamut.min_luma`/`max_luma`），不再是 `[0, peak]`；生成和三个后端采样器一致，缓存键包含目标黑点，因此 `contrast_ratio` 变化会重新生成 LUT。SDR 目标的色域映射最多移动 I 轴约 3.5%；HDR/EDR 目标不变（black = 0）。
- 退化的 ST 2094-10 锚点集（重复/非有限点）回退到恒等曲线（Clip），而非会使着色器输出黑帧的零系数。

## 0.1.8 - 2026-09-07

### 兼容性

- presenter 配置 ABI 新增 packed-alpha 视频和原生合成选项。原生嵌入方必须使用匹配的 `erika.h` 和运行时重建；不要把旧版配置结构体与 0.1.8 二进制混用。Flutter 包固定匹配的原生工件。

### C API

- 新增 `ErikaOpenOptions` 与 `erika_open_with_options` / `erika_presenter_open_with_options`，取代 `_with_headers` 对。选项结构体将请求头数组与逐请求调优组合，首先提供 `http_read_ahead_bytes` 覆盖 HTTP(S) 预读窗口（0 时若设置 `ERIKA_HTTP_READAHEAD_BYTES` 则使用它，否则默认 2 MiB；显式值优先于环境变量）。保留字段必须为零并接受验证，避免未来字段静默改变行为。

### Flutter 和 OpenHarmony

- Android、Apple、Windows 和 OpenHarmony 的 Flutter `ErikaPlayer.open` 新增 `httpReadAheadBytes`，独立 OpenHarmony SDK 也暴露匹配的打开选项。

### 弹幕

- 在滑动规划窗口间同时保持接受和拒绝的放置决策稳定，防止已丢弃弹幕中途重现或可见弹幕被迫换轨。
- 宿主提供的弹幕 ID 与内部布局身份解耦，可选 JSON ID 精确解析为 `u64`，无浮点舍入。

### 播放

- 宿主提供的 Metal layer 显式保留有限 drawable 超时（最多一秒后返回 nil）；跳过的呈现被计数并产生限频诊断。
- render tick 阻塞后恢复音频主时钟漂移，不增加音频队列深度。Presenter 反馈保留采集时间、播放意图、generation 和 output epoch；过期反馈及纯静音回调不能驱动时钟修正。每个新输出先建立进度基线。
- 视频解码在前台恢复时避免重播已排队音频；恢复音频中断期间，将 iOS 前台恢复推迟到宿主准备就绪。
- 通过有界旧速率音频桥平滑播放速度变化，同时在非默认速率保持音频主时钟同步。
- 首个音轨无法解码时按容器顺序尝试其余音轨；若均无法打开，报告全部解码器失败。
- Windows 和 macOS 新增 packed-alpha 视频呈现与预乘 GPU 输出，包括感知原生背景的 overlay 合成。
- 播放速度上限提高至 16x。

### Windows 渲染

- 修复不安全 D3D11 帧复用造成的 DWM 崩溃和视频条纹，保留原生不透明播放的 HDR 能力路径，并修正填充尺寸。
- 新增显式 SDR Flutter 纹理路径和同步 GPU 纹理交接。播放器关闭现会先等待弹幕工作线程结束再卸载 DLL。

### SDK 分发

- 新增 Swift SDK 二进制归档，将原生、Flutter、OpenHarmony 和 Swift 包发布统一到原生版本 tag。

## 0.1.7 - 2026-08-16

### Flutter 包分发

- 为 `erika_flutter` 首次 pub.dev 发布准备包内许可证、更新日志、元数据、示例和独立包测试。
- Android、Apple、Windows、OpenHarmony 默认使用已验证且固定版本的原生 Release 归档；源码构建需显式设置 `ERIKA_FORCE_SOURCE_BUILD=1`。
- 新增按 ABI 分发的 Flutter Android 运行时归档，使应用只下载请求的架构且不包含原生嵌入方静态库。
- 新增 Release 级 `SHA256SUMS` 清单，覆盖所有发布的原生归档。
- 新增 Android、iOS、macOS、tvOS、Windows、OpenHarmony 的隔离 GitHub Actions 消费者，确保包构建不依赖 monorepo。

### OpenHarmony 包分发

- 将独立原生 ArkTS 包 `erika` 发布至 OHPM，面向 OpenHarmony arm64（API 18+），包含 `XComponent` 表面生命周期、`renderTick()` 调度、播放控制、事件及截图。

## 0.1.6 - 2026-08-14

### 渲染与播放

- 为退化动画源新增 ArtCNN C4F16 DS 降噪/锐化亮度超分，覆盖 Rust、C、Flutter、Metal 和 wgpu 接口。
- Android 和 macOS presenter 渲染移出应用 UI/主线程，降低 Android AHardwareBuffer 转换带宽，并防止跨设备复用 D3D11 帧。
- Apple 平台新增根据源选择 SDR/EDR 输出，Android、Windows 和 OpenHarmony 接口暴露资源状态。

### 接入与行为

- 默认关闭弹幕滚动覆盖，密集播放时保留已稳定的弹幕。
- 恢复 Windows 系统媒体控制构建，并使 OpenHarmony Flutter 插件的 ArkTS 声明显式化。

## 0.1.5 - 2026-08-03

### 系统媒体控制和后台音频

- 在 iOS、tvOS、macOS、Android、Windows 和 OpenHarmony 新增 Now Playing 元数据、播放状态、时间线、播放/暂停/跳转及上一项/下一项导航控制。
- iOS 和 Android 新增可选后台音频播放，暂停视频解码；播放工作线程屏障有界，生命周期回调不会无限期阻塞应用线程。
- 打开没有 `ErikaMediaMetadata` 的媒体时清除过期元数据，确保暂停/播放顺序确定，并隔离多个 Flutter 引擎的 Android 媒体回调。
- 缓存 Android 封面，原生播放状态未变化时避免重建媒体通知。

### 平台支持和发布工件

- 新增 tvOS Flutter 插件和 Apple TV 设备、arm64/x86_64 模拟器的原生目标，包含 Metal 呈现、Apple 音频、VideoToolbox、字幕、弹幕、截图及 HTTP 源。
- 自动 GitHub Release 新增 `erika-capi-tvos.zip`，包含 tvOS 设备和通用模拟器 `erika_capi.xcframework`。
- OpenHarmony 新增可选预编译包：插件 CMake 可下载指定 tag 的 `liberika_capi.so`，放在 `liberika_flutter.so` 旁，失败时回退源码构建。
- macOS、iOS 和 tvOS CI/Release 统一使用 macOS 26 runner；macOS x64 与 arm64 一起交叉构建后合成 universal 归档。
- 修复原生依赖构建中的 tvOS 平台检测，并接受有效零长度 HTTP 资源，不再误判为截断。

### 字幕内存字体

- Rust、C API、Dart 和所有原生 Flutter 嵌入新增内存字幕字体注册表。宿主可检查字体 face、选择有序回退集、替换字体数据及清空注册表，无需写入磁盘字体文件。
- 内置 libass 保留有序回退，在 face 间共享 TTC/OTC 数据；同路径字体替换时使渲染缓存失效，使活动字幕轨立即使用新数据。
- 分别维护注册表与所选字体 generation，渲染器重建后重放 libass codec private data，并新增 OpenHarmony 与 tvOS C API 完整内存字体接口覆盖。

### 播放恢复

- 音视频输入饥饿时新增显式缓冲状态切换。停滞时冻结播放时钟，恢复后重新锚定到媒体时间，避免时钟超前导致数据恢复后长时间丢帧或黑屏。

## 0.1.4 - 2026-07-30

### 兼容性说明

- **`outline_width` 现为档位而非乘数。** `0` 关闭、`1` 细、`2` 普通、`3` 粗；大于 `3` 截至粗。普通和粗分别精确复现旧连续乘数 `1.0` 和 `2.0` 的宽度，旧宿主若传 `outline_width: 1.0`，现需传 `2.0` 保持笔画；不迁移会变细，默认字号的栅格化半径从 2 px 减半为 1 px。
- **弹幕字号现按每 em 像素解释。** `ab_glyph` 的 `PxScale` 是上升线到下降线高度；旧代码直接传入 em 大小，使文本比请求尺寸小一个字体自身高度/em 比例。现在文本匹配请求字号，升级后既有用户看到的弹幕变大：STHeiti 和 Hiragino Sans GB 比例为 1.0，而 PingFang SC（macOS 默认字体）为 1.4，因此同配置最多放大 40%，容纳的轨道更少。需要旧外观的宿主应降低配置字号。
- 滚动时长随视口逻辑宽度缩放（640 pt 时 ×0.9，1920 pt 及更宽时至 ×1.3），使弹幕在宽窗口中穿过画面的视觉时间相近。因此同配置在大窗口中比过去滚动更慢。
- 弹幕截图不再包含弹幕；`capture_*` 只合成视频和字幕，也不包括调试 HUD。

### 平台支持和发布工件

- 新增 OpenHarmony 播放器后端和 Flutter 插件，包含 AVCodec H.264/HEVC 硬解、OHNativeBuffer/Vulkan 零拷贝呈现、WGPU 合成、字幕、弹幕、音频、诊断及 RGBA 截图。Release 包含 C API 运行时和 Flutter 原生桥接的 `erika-capi-openharmony-arm64.zip`。
- 新增原生 Windows ARM64 依赖及 `erika_capi` 构建，以及 `erika-capi-windows-arm64.zip`；Windows x64 和 ARM64 CI 在匹配架构的 GitHub 托管机器运行。
- 新增可选 macOS arm64、x86_64、universal 构建及对应架构归档。
- 修复 Apple Silicon 上 Android 源码构建，选择 NDK `darwin-x86_64` 工具目录提供的 universal `glslc`。
- 已发布 x86_64 构建重新启用 FFmpeg 优化 x86 汇编；这些目标的源码构建现需 NASM。

### 播放、渲染和窗口集成

- 直接发布共享播放时钟，使帧快照原子采样时间、状态与 generation；弹幕、字幕和渲染使用同一时间基准，不再有独立只能向前的显示时钟。
- 暂停时跳转现解码并呈现所请求预览帧，同时保持媒体时钟冻结。跳转预滚、EOF、跳转后立即暂停及恢复启动竞争不再造成时钟回退、工作线程空转或长时间黑帧。
- 分离音频与视频解封装需求，避免音频背压阻塞视频包扫描，同时保持有界解码音频队列。切换或添加字幕不再重置音视频解封装选择。
- macOS 呈现改用 `CVDisplayLink`、合并慢帧并在屏幕变化后重定向 display link，减少缩放和窗口迁移时重复的 overlay 附着和 GPU 资源操作。
- 桌面 overlay API 新增多 `FlutterView` 和次级窗口目标；macOS 和 Windows overlay 可跟随播放器在宿主窗口间移动，并保持 surface、可见性、generation 和 aspect-fit 状态。
- 改进 Windows D3D11 overlay 生命周期、非阻塞呈现及恢复跳转处理。字幕合成在视频视口内，弹幕保留全窗口视口。

### 弹幕和字幕

- 弹幕碰撞边界对齐栅格化描边，修正阴影/描边/文字层次，稳定轨道偏好，将高密度重叠限制在溢出轨道。
- 异步重新布局期间保持旧弹幕计划运行；仅绘制设置复用布局，可见性变化立即生效，禁用弹幕后过期计划不再重现。
- 新增增量字形图集上传和可复用 Metal 实例缓冲，减少逐帧 DFM 分配与候选遍历开销。
- 外部文本字幕新增字符集检测与转码，包含 GBK、Big5、Shift_JIS、UTF-16，UTF-8 直接通过，低置信度或二进制输入使用受保护回退。
- libass 在所有平台注册 liberika 内置 Droid Sans Fallback，不再仅限 iOS/Android；没有系统字体提供器的目标默认使用该字体族，而非无法解析的 `Arial`。
- 新增字幕回退和选择性覆盖样式：自定义字体族与文件、RGBA 颜色、度量、文字属性、边框、对齐、边距及模糊。新入口为 `erika_presenter_set_subtitle_font`、`erika_presenter_set_subtitle_style` 和 `ErikaPlayer.setSubtitleStyle`。

### 网络与媒体诊断

- 新增 `erika_open_with_headers` 与 `erika_presenter_open_with_headers`。Android、iOS、macOS、Windows 的 `ErikaPlayer.open` 接受 `httpHeaders`，用于探测、范围读取、重试和预取；原有 open 函数兼容并使用空头列表。
- 拒绝调用方提供由 liberika 管理的传输头及无效 HTTP 字段名或值。外部字幕和弹幕 sidecar 仍使用无请求头路径，不继承媒体请求头。
- 加固 HTTP 范围输入，应对忽略 `Range`、错误响应偏移、临时请求/正文失败、部分响应、重复预取下载和错误 EOF；完整获取操作内的重试有界。
- Rust、C、Dart 新增轨道码率及有理数帧率元数据，并新增可选原生调试 HUD，显示解码器、渲染器、GPU、音频、输出/HDR 与弹幕诊断。

### 音频、色彩和编解码更新

- 规范化环绕声到立体声的下混矩阵，防止削波。
- 所有音频后端新增每回调平滑音量渐变，WASAPI 新增设备丢失恢复、可观察恢复状态与有界退避。
- Metal、WGPU、D3D11 新增 BT.2100 HLG 解码，并修正 HDR10 输出中 D3D11 overlay 的 PQ 编码。
- FFmpeg 升级至 8.1.2，改进 Darwin AV1 硬解/导入处理；跳转加载期间保留最后一帧，避免闪黑。

## 0.1.3 - 2026-07-17

### Android 播放和打包

- 新增完整 MediaCodec、AHardwareBuffer/wgpu、AAudio、Flutter PlatformView、SAF/content 源、字幕、弹幕、截图、SDR/HDR、诊断和恢复路径。
- `ERIKA_PREBUILT=1` 现从指定 tag 的 Android Release 归档为所需 Flutter ABI 准备 `liberika_capi.so` 与 `libc++_shared.so`，并支持显式源码构建回退。

### 不兼容的 C API 表面尺寸语义

传给 `erika_presenter_attach_metal_layer`、`erika_presenter_attach_wgpu_surface`、`erika_presenter_attach_wgpu_surface_with_output_capabilities`、`erika_presenter_attach_windows_hwnd` 和 `erika_presenter_resize_surface` 的 `width`、`height` 现表示物理像素中精确的 drawable 尺寸。

此前原生渲染器会将这些值乘以 `scale`。`scale` 现独立影响弹幕等逻辑 UI 内容，绝不改变 surface 尺寸。当前传逻辑尺寸的直接 C API 宿主必须在调用前转换为物理像素。仓库内 macOS、iOS、Windows、Android Flutter 嵌入及示例均已更新。

### 播放命令分发

`play` 异步入队，不再无限期等待播放工作线程。宿主必须监听 `StateChanged` 和 `Error` 事件以获取状态切换的权威结果。
