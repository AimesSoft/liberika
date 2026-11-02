# liberika Architecture

[中文](architecture.zh.md) | [English](architecture.md) | [日本語](architecture.ja.md)

liberika 是一个可嵌入的 Rust 媒体播放库。宿主应用可以通过 Rust API、C ABI (`erika_capi`) 或 Flutter 绑定 (`erika_flutter`) 调用它。视频帧、字幕和弹幕都留在引擎内部，并在渲染器里合成，原生 surface 直接呈现，Flutter texture 交给宿主合成。

浏览器通过 JavaScript / TypeScript `Player` 接入：[Web 后端](web.zh.md) 在 WASM
中复用 Rust 弹幕和补丁版 libass，使用 FFmpeg 解封装并读取 MKV 字体附件，
WebCodecs、Web Audio 和 WebGL2 分别提供解码、播放时钟及最终合成。Erika Web
已采用这条路径。下文概览描述原生运行时；浏览器的 Worker 数据流和生命周期见 Web 指南。

## 系统概览

```text
Rust Player Core
  source abstraction ─── file + HTTP range
  FFmpeg wrappers ────── custom AVIO, probe, demux, decode, seek, audio resample
  playback engine ────── video/audio tick, clock, frame scheduler
  video decode ───────── VideoToolbox, D3D11VA, MediaCodec, AVCodec, NVDEC, VA-API, software fallback
  audio output ───────── CoreAudio, AudioQueue, WASAPI, AAudio, OHAudio, PulseAudio, ring buffer
  overlay timeline ───── subtitle + danmaku composition
  renderer core ──────── color state, render graph, tone map, scaler policy
  Metal renderer ─────── zero-copy NV12/P010, HDR/EDR, subtitle/danmaku pass
  D3D11 renderer ─────── zero-copy D3D11VA, HDR10, subtitle/danmaku pass (Windows)
  wgpu renderer ──────── cross-platform video, overlays, capture, Android scRGB, OHOS Vulkan
  presenter runtime ──── ties player + renderer + audio + overlays
  C ABI ──────────────── 版本化公开头文件，两组 handle 接口族
  Flutter plugin ─────── macOS + iOS + tvOS + Windows + Android + OpenHarmony + Linux embedding
```

## HTTP 数据源与显示时间

HTTP Range 数据源使用字节预算管理预读和回看缓存。`http_read_ahead_bytes` 默认 2 MiB（可由 `ERIKA_HTTP_READAHEAD_BYTES` 设置），`http_back_buffer_bytes` 默认 16 MiB；C 的 `ErikaOpenOptions` 和 Dart `open()` 可逐次覆盖。stop、重新 open、close 会取消旧会话的网络任务并释放其资源。

iOS Flutter 桥接使用 `CADisplayLink.targetTimestamp` 作为显示目标，并通过只保留最新 tick 的 mailbox 避免渲染队列积压。`render_tick_with_timing` 让视频、字幕和弹幕按同一目标取样；音频主时钟保持原来的推进规则。接口与参数见 [C ABI 参考](capi_reference.zh.md)。

## 原生依赖

`xtask` 会从固定上游源下载、构建并安装原生依赖到 `third_party/`。默认 profile 是 `lgpl`。

| 依赖 | 版本 | 作用 |
|------|------|------|
| FFmpeg | 8.1.2 | Demux、decode、audio resample、平台硬解 |
| dav1d | 1.5.1 | 所有目标的 AV1 软解（8-bit 与高位深） |
| libass | 0.17.5 | ASS 字幕渲染 |
| FreeType | 2.14.3 | 字体栅格化（libass 依赖） |
| HarfBuzz | 14.2.1 | 文本 shaping（libass 依赖） |
| FriBidi | 1.0.16 | 双向文本处理（libass 依赖） |

发布目标的依赖静态链接；Linux 使用系统 FFmpeg、PulseAudio 和补丁版 libass。libass 及其依赖默认启用（`features = ["libass"]`）。

```sh
cargo run -p xtask -- deps build --all --profile lgpl
cargo run -p xtask -- deps status
```

## FFmpeg 集成

`erika_ffmpeg_sys` 在构建时通过 bindgen 生成底层绑定。`erika::ffmpeg` 提供安全的 Rust 封装：

- **Demuxer**：持有 `AVFormatContext`，可选使用来自 `MediaSource` 的 Rust 后端自定义 `AVIOContext`。支持流选择、引用计数 packet 和基于时间戳的 seek。
- **Decoder**：软件，以及 VideoToolbox、D3D11VA、MediaCodec、OpenHarmony AVCodec 硬件后端。硬件帧保留 BT.2020/PQ 元数据，并携带平台原生句柄（Apple 上是 `CVPixelBufferRef`）供渲染器零拷贝导入。
- **AudioResampler**：封装 `libswresample`，输出 interleaved f32 PCM（默认 48 kHz stereo）。
- **SubtitleDecoder**：解码内嵌文本和位图字幕流。

## 播放引擎

`PlaybackSession` 负责打开媒体、选择轨道、配置解码后端，并产出视频帧和 PCM 音频块。

解码器可用性是 session invariant：只要选中了视频轨，play、seek 和 video-frame pump 入口就必须有活动的视频 decoder。MediaCodec seek reopen 以及 Surface→ByteBuffer/software fallback 等破坏性切换会先记录 decoder unavailable reason；若最终 software decoder 也打开失败，这些入口会返回该明确错误并要求重新 open 媒体，绝不会进入只播音频的假 `Playing` 状态。

`VideoPlaybackEngine` 增加时钟驱动播放：

- 播放、暂停、停止、seek、倍速控制（音频经 SoundTouch）、EOF 检测。
- `PlaybackClock`：带音频主时钟约束的 media-time 锚点（deadband 校正、逐帧有界调整、大漂移 snap）。
- `VideoFrameScheduler`：为解码后的视频帧决定 present / wait / drop。
- `DisplaySyncState`：携带残余帧时长误差的 vsync 量化器。

## 音频输出

时钟反馈通过 `Player::capture_audio_clock` 和
`update_audio_clock_observation` 传递。观测保留 player 身份、播放 generation、
命令序号、输出 epoch 和单调采样时间。worker 同时核对当前意图与已执行命令，
并从采样时刻计算 500 ms 有效期；队列与缓冲决策使用相同的有效性检查。
presenter 在输出重置、重新配置、设备状态切换和倍速切换时使旧 epoch 失效；
输出拥有者仍需串行执行采样与输出变更。

引擎先建立观测基线，随后要求已消耗 PCM 帧数与媒体时间同时推进，才校正共享的
`PlaybackClock`。纯静音欠载回调与冻结快照不能持续拉回时钟；有效的大幅漂移
允许向前或向后重锚。投递延迟补偿以采样时已排队的 PCM 时长为上限，并使用已提交
的倍速。暂停或等待首帧而停驻的时钟保持停驻；后台播放和前台视频恢复期间，
仍在运行的音频主时钟继续生效。

原有 Rust API `update_audio_clock(snapshot)` 保留给当前时间线的即时同步反馈，
无法还原缓存快照的原始采样身份；延迟反馈应使用新的观测 API。C ABI 与队列目标
不变，保留 250 ms 倍速桥接及过渡期间抑制混合倍速反馈的契约。
音频喂送仍在 presenter tick 中执行，渲染阻塞仍可能造成欠载；时钟恢复不能替代
音频喂送与渲染的独立调度。

- **macOS**：CoreAudio 输出，带 ring buffer 和 PTS 跟踪的 clock snapshot。presenter 会把输出快照回传给 player worker 做音频主时钟约束。
- **iOS**：AudioQueue 输出，使用同样的 ring buffer 和 clock snapshot 模型。
- Ring buffer：interleaved f32、容量可配、溢出丢最旧、支持音量控制。

## 字幕系统

- **Parsing**：SRT、WebVTT、ASS 时间线解析。支持内嵌和外部字幕轨，外部轨可在运行时增删。
- **libass renderer**：静态链接且默认启用。接收 ASS 脚本，调用 `ass_render_frame`，把 alpha plane 导入 liberika 的 overlay 系统。macOS 使用 CoreText 字体提供者，Windows 用 DirectWrite；其余目标运行时没有任何系统字体提供者（vendored libass 关闭了 fontconfig），因此 liberika 内嵌并在所有平台注册为内存字体的 Droid Sans Fallback 就是 libass 默认解析到的字体族。
- **字幕样式**：自定义字体族与字体文件、颜色、度量（字号、描边、阴影、模糊、字距、缩放）、字形属性、描边样式、对齐与边距都作为回退值，补足脚本未指定的部分以及纯文本字幕的样式；度量还会再乘上字幕缩放。`override_mask` 会把选中的字段提升为 libass 的 selective style override，从而覆盖 ASS 对白自带的样式；覆盖用的度量经过重新归一，无论脚本声明何种 `PlayResY` 都落在同样的像素上。
- **SubtitleRendererCore**：面向 renderer 的边界层，用 changed/unchanged frame 跟踪避免重复 GPU 上传。

## 弹幕系统

弹幕子系统用 Rust 原生实现了 NipaPlay DFM+ 的布局算法。完整设计见 `docs/danmaku_architecture.md`。

- **输入**：Bilibili XML、JSON、JSON-lines 解析。
- **DanmakuSession**：多轨管理，支持按轨启用/禁用、按轨 offset、全局 offset。
- **DFM+ layout core**：prepare / frame-query 分离。prepare 一次性处理整条轨道（测量、过滤、重复合并、碰撞避让、轨道分配），frame query 返回某一 media time 下的位置结果。
- **Text rasterizer**：带 fill/outline alpha mask 的 glyph atlas，并通过 version 跟踪 GPU 纹理复用。
- **Render plan**：`DanmakuRenderPlan` 携带 glyph instances，包含屏幕 rect、atlas tex rect、颜色、outline、shadow。Metal 和 wgpu 渲染器从 atlas 画实例化 quad。

## 渲染器

### Metal Renderer（macOS/iOS/tvOS）

Apple 平台的主渲染器：

- 通过 `CVMetalTextureCache` 零拷贝导入 CVPixelBuffer → MTLTexture。
- YCbCr 采样、transfer decode、gamut mapping（BT.2020→BT.709、Display P3→BT.709）。
- Tone mapping：Mobius、Reinhard、clip，支持绝对 nits。
- SDR 输出（`BGRA8Unorm`）与 Apple EDR 输出（`RGBA16Float` + EDR headroom）。
- 亮度超分（`LumaUpscalerMode`）：网络类模式（ArtCNN、Anime4K、`Custom`）以 Metal compute 跑在解码后的 Y plane 上，并与 render pass 使用同一 command buffer。色度保持原分辨率。网络输出会按解码帧缓存，重复 vsync tick 直接复用结果。kernel 在后台线程编译，编译完成前播放会先以未放大状态继续。各引擎族（通用网络引擎、FSR1、MetalFX spatial、VideoToolbox）见[超分](#超分)。
- 字幕 overlay：RGBA plane 上传与 alpha blending。
- 弹幕：来自 atlas 的 instanced glyph quad 绘制（shadow → outline → fill）。
- 呈现布局保持源宽高比。

### Direct3D 11 Renderer（Windows）

Windows 平台的原生渲染器（`renderer/d3d11.rs`）：

- 零拷贝 D3D11VA 解码纹理互操作：解码出的 `ID3D11Texture2D` 表面共享进渲染设备，不经过 CPU。
- YCbCr 采样与色彩空间转换（HLSL shader），与 Metal 保持同一流水线模型。
- 在零拷贝 D3D11VA luma plane 上通过生成的 compute kernel 执行神经超分与平台超分，包含实测后端选择、RTX/Intel VSR，以及可选的 Vulkan / D3D12 加速器 sidecar（见[超分](#超分)）。放大后的亮度是普通的 2x `R16_FLOAT` 纹理，按解码帧缓存。无法运行的模式会明确报告 `Inactive` 回退并给出原因。
- HDR10 输出：`R10G10B10A2_UNORM` swapchain + `DXGI_HDR_METADATA_HDR10`，并提供 SDR（`BGRA8`）回退。
- 字幕 overlay alpha-atlas 上传与 blending；来自 atlas 的 instanced 弹幕 glyph quad 绘制。
- window-hosted swapchain，由 `render_tick` 驱动。

### wgpu Renderer（跨平台）

面向可移植性的第二渲染后端：

- 真正的 `wgpu` 依赖与设备/表面/pipeline 创建。
- NV12/P010 视频帧上传和 WGSL YCbCr 转换 shader。
- Android 通过 AHardwareBuffer/Vulkan 导入 MediaCodec Surface；原生互操作不可用时明确切到 ByteBuffer/CPU upload。
- 公共 `renderer/frame.rs` 边界统一携带尺寸/色彩元数据，以及 FFmpeg 解码帧或独立的 prepared AHardwareBuffer。MediaCodec Surface 的 AVFrame 会在 playback worker 上、decoder callback context 仍存活时释放；presenter 与 GPU recovery 不再持有 decoder-owned AVFrame。
- 色彩空间转换、tone mapping（与 Metal 保持同一流水线模型）。
- 超分辨率（`renderer/wgpu_sr.rs` 与 `renderer/sr/wgsl.rs`）：支持全部网络模式（ArtCNN 1-7、Anime4K 16-25、`Custom`）以及 `Fsr1`，与 D3D11、Metal 共用同一份 `SrModel` IR。网络输出是源分辨率的 `Rgba16Float` texture，存放 packed 2x2 depth-to-space 四元组；FSR1 输出呈现分辨率的 luma texture。既支持原生 luma plane，也支持 Android 已转换的 nonlinear RGB texture，并通过 `rgb + (Y_sr - Y)` 保持色度。`PlatformVideo` 与 `MetalFxSpatial` 为 `Inactive`。GLES 3.0 不尝试 compute，而是明确报告 `Inactive` 和 `native_luma_sampling` 回退。
- 字幕/弹幕合成、截图，以及可用于无头验证的离屏 render target。即使显示 surface 是 extended-linear，截图也始终离屏渲染到 SDR RGBA8 target，避免把未映射的 scRGB 值当成 SDR 像素输出。
- 表面句柄模型覆盖 macOS NSView、iOS UIView、Windows HWND、X11/Wayland、Android native window、OpenHarmony `OHNativeWindow`。
- OpenHarmony 把 AVCodec 的 Surface 输出作为 `OHNativeBuffer` 支撑的 Vulkan 外部图像导入，并用 Vulkan YCbCr sampler 在 GPU 上完成 YUV 转换，解码帧无需 CPU 拷贝即可进入 wgpu 合成器。缺少所需 Vulkan 扩展的设备回退到软解 + CPU 上传，回退过程通过诊断事件上报。
- Android 提供有界 Vulkan/GLES backend recovery，以及 import、能力、质量降级和 device failure 的结构化日志。其高 headroom 输出是 FP16 **extended-linear scRGB**，不是 HDR10/PQ：renderer 使用 `Rgba16Float`、Vulkan extended-sRGB-linear color space，并在每次 configure/reconfigure 后验证 `ANativeWindow` 的 `ADATASPACE_SCRGB_LINEAR`（`0x18410000`）。Android scRGB 使用 BT.709 primaries，`1.0 = 80 nit`，不会输出 PQ 或 HDR10 static metadata。
- Extended-linear 只有在显式请求 `ExtendedLinear`、显示器/surface 支持 HDR、使用 Flutter Hybrid Composition 承载 `SurfaceView`、wgpu 后端为 Vulkan、surface 支持 `Rgba16Float`，且 `SCRGB_LINEAR` dataspace 回读成功时才会激活。任一条件缺失都会立即选择正常 SDR surface，并记录稳定的 `0..8` fallback reason；因此 GLES 与 `TextureView` 都是 SDR 路径。
- API 34+ 上，Android 宿主通过 `Display.registerHdrSdrRatioChangedListener` 观察显示器，并把真实变化经 `erika_presenter_set_output_headroom` 发布给 liberika。wgpu 无需重新 attach surface，就会让后续帧使用更新后的有效内容 headroom，并同步可查询状态。ratio 可用时 `activeHeadroomKnown` 为 true；只有 known 标志或 ratio 确实变化时，`headroomUpdates` 才增长。
- Flutter extended-linear player 未显式传 `edrHeadroom` 时，内容上限默认为 4x，同时给 `SurfaceView` 传 `0` 表示系统 auto desired headroom。显式数值会成为内容上限，并在 API 35 上作为 per-`SurfaceView` desired headroom；liberika 不修改全局 Window。
- 模拟器/非 HDR 设备覆盖已经验证明确的 SDR 回退分支；在完成 API 35 HDR 真机上的 `Rgba16Float + SCRGB_LINEAR` 验收前，不把 active extended-linear 路径描述为“已通过真机验证”。

### 超分

网络类模式共用位于 `crates/erika/src/renderer/sr/` 的平台中立网络 IR：

- `model.rs`：网络图（单通道亮度输入上的卷积，每个输入有各自的 kernel，CReLU 变换、残差、ReLU/SiLU/PReLU，以及 2x depth-to-space 头）及其 `.esr` 序列化。
- `catalog.rs` 把模式映射到内置模型；`mpv.rs` 导入 Anime4K / FSRCNNX 的 mpv hook shader（只取权重，从不执行 shader 代码）；`artcnn.rs` 降级转换旧版 ArtCNN blob；`fsr1.rs` 是 FSR 1.0。
- `fold.rs` 做精确的优化：合并同级 pass，并把线性层代数折叠进相邻层（ArtCNN 的 conv0 折进 conv1、conv5 折进 conv6），用九组边界类别权重保证图像边缘仍然精确。ArtCNN 的 MAC 减少 26-33%。
- `reference.rs` 是 CPU oracle，所有 GPU 后端都以它为测试基准。
- `hlsl.rs` 是 D3D11 kernel 生成器，`wgsl.rs` 是 wgpu 的。

Direct3D 11（`renderer/d3d11_sr.rs` 及相邻文件）：

- `d3d11_sr.rs` 是执行器：逐层 kernel，权重以字面索引放在 constant buffer 中，groupshared 暂存输入，融合 epilogue/head，边缘用一次 ring dispatch，在 192 MB 预算内分块，后台 FXC 编译加磁盘缓存，并用 timestamp query 统计 GPU 耗时。
- `d3d11_sr_select.rs` 按网络实测标量与各加速器，选择最快者，并发出 `luma_upscaler_backend_selection` 诊断事件，列出实测的候选。
- `d3d11_vsr.rs` 通过 video processor 厂商扩展驱动 NVIDIA RTX Video / Intel VSR。
- Sidecar：`d3d11_vk_coopmat.rs` 与 `vk_coopmat/`（Vulkan cooperative matrix），以及 `d3d11_d3d12_linalg.rs` 与 `d3d12_linalg/`（可选的 D3D12 SM 6.10 预览）。

Metal（`renderer/metal/`）：`upscaler_network.rs` 是通用网络引擎（M5/A19 用 Metal 4 tensor ops，其余为 `simdgroup_matrix` 或标量），`upscaler_fsr.rs`、`upscaler_metalfx.rs`、`upscaler_vt.rs` 分别对应 FSR1、MetalFX spatial 与 VideoToolbox；`upscaler.rs` 负责选择与状态。手写的 ArtCNN kernel 仍可通过 `ERIKA_SR_BACKEND=legacy` 选用。

wgpu（`renderer/wgpu_sr.rs`、`renderer/sr/wgsl.rs`）：`wgsl.rs` 从 `SrModel` IR 生成 WGSL，并以 `reference.rs` 校验；`wgpu_sr.rs` 是执行器，负责后台编译（`Building` 状态）、在 scratch 预算内分块和模型缓存。生成的 kernel 使用 workgroup memory 暂存输入、融合 bias/residual/激活 epilogue 与 depth-to-space head、用一次 perimeter dispatch 处理折叠层的边界类别，在适配器具备 `SHADER_F16` 时使用 fp16，在声明支持的 Vulkan 适配器（NVIDIA）上使用 16x16 cooperative matrix。每个适配器的默认值会在最初几帧通过计时少量 kernel 变体来校准（进程内缓存；`ERIKA_WGPU_SR_AUTOTUNE=0` 可关闭）。FSR1 是融合的 EASU+RCAS kernel。`wgpu_artcnn.rs`（旧版 ArtCNN 执行器）仅保留用于回归对比。


### Render Pipeline

`renderer::pipeline` 会在任何后端消费之前，先在 Rust 里描述渲染决策：

- `SourceColorState` / `TargetColorState`：primaries、transfer、range。
- `VideoRenderPipeline`：gamut matrix、tone map operator、transfer functions。
- `renderer::output`：由原生 renderer 与 wgpu 共用的请求模式、实际 encoding、surface format、dataspace/headroom 状态和稳定 fallback 诊断。
- HDR metadata：mastering display、content light level、nominal peak nits。

## Presenter Runtime

`PresenterRuntime` 把 Player、MetalRenderer、OverlayTimeline、DanmakuEngine 和音频输出串起来。宿主提供原生 surface，并从显示定时器驱动 `render_tick`。

- 推送视频帧，更新 overlay（字幕 + 弹幕），渲染并 present。
- 弹幕 plan 生成与视频帧通过 generation + media_time gate 保持同步。
- seek、选轨、fallback、open/close 等 decoder transition 使用 quiesce/ACK 屏障：停发帧、释放 renderer cache、清 receiver、完成切换后再恢复。playback generation 跨 reopen 单调递增，旧 generation 或旧 MediaCodec route 的 import feedback 会被明确丢弃。
- 运行时支持倍速、音量、轨道选择、字幕/弹幕配置。

## C ABI

主头文件为 `crates/erika_capi/include/erika.h`，各包的副本须与其一致。两个 `erika_presenter_windows_*_iunknown` getter 仅在 Windows 导出；共用头文件在所有平台都保留其声明。

`erika_capi` 提供两组 handle family：

- **`ErikaHandle`**：播放器控制与事件轮询，渲染由宿主管理。
- **`ErikaPresenterHandle`**：liberika 持有完整栈，宿主只需提供 surface 并调用 `render_tick`。

覆盖范围包括：create/destroy、open/play/pause/stop/seek、轨道选择、字幕轨增删、弹幕轨管理（add/remove/enable/offset/config）、surface attach/detach/resize、事件轮询、音量、播放速率、神经亮度超分切换、超分诊断，以及 `erika_presenter_get_output_status` 返回的 13 字段输出状态快照。

Header：`crates/erika_capi/include/erika.h`

## Flutter Plugin

`packages/erika_flutter` 提供 macOS、iOS、tvOS、Windows、Android 和 HarmonyOS 的 Flutter embedding：

- **Dart**：`ErikaPlayer`（命令 + 事件）、`ErikaWindowOverlayVideoView`（推荐的 window-hosted native surface——Apple 上是 Metal，Windows 上是 D3D11 swapchain）、`ErikaVideoView`（兼容 platform view）。
- **macOS Swift plugin**：加载 `liberika_capi.dylib`，创建 `NSWindow` overlay 或 `NSView`/`CAMetalLayer` platform view，并通过 display link 驱动 `render_tick`。
- **iOS Swift plugin**：静态链接 `liberika_capi.a`，创建 `UIWindow` overlay 或 `UIView`/`CAMetalLayer` platform view，并沿用同一 presenter 模型。
- **tvOS Swift plugin**：静态链接 `liberika_capi.a`，在 Apple TV 的 `UIView`/`CAMetalLayer` platform view 中呈现，并沿用同一 presenter 模型；支持 tvOS 真机和 arm64/x86_64 模拟器。
- **Windows C++ plugin**（`ErikaFlutterPluginCApi`）：通过 CMake（`build_erika_runtime.cmake`，cargo target `x86_64-pc-windows-msvc`）构建并链接 `erika_capi.dll`，host 一个 window-level D3D11 swapchain，并由帧调度器驱动 `render_tick`。
- **Android Kotlin/JNI plugin**：为 Android ABI 构建 Rust runtime，每个 player 持有独立原生 surface。SDR 使用 `TextureView`；请求 extended-linear 时，通过 Flutter Hybrid Composition 使用 `SurfaceView`。插件协调 Activity surface lifecycle、音频焦点、noisy-route、HDR eligibility/headroom，并只在存在活跃 player 时驱动共享帧调度器。
- **HarmonyOS ArkTS/N-API plugin**：通过 Hvigor/CMake 为 `aarch64-unknown-linux-ohos` 构建 `liberika_capi.so`，注册 Flutter 外部纹理，并把该纹理的 `OHNativeWindow` attach 给 presenter。音频经 OHAudio 以交错 f32 PCM 输出。

Embedding 模型和 HDR 策略见 `docs/flutter_embedding.md`。

## Platform Support

| Platform | Decode | Render | Audio | Status |
|----------|--------|--------|-------|--------|
| macOS 11+ | VideoToolbox | Metal | CoreAudio | Available |
| iOS 13+ | VideoToolbox | Metal | AudioQueue | Available |
| tvOS 13+ (Apple TV) | VideoToolbox | Metal | AudioQueue | Available |
| Windows 10+ | D3D11VA | Direct3D 11 | WASAPI | Available |
| Linux | NVDEC / VA-API / FFmpeg | wgpu X11/Wayland (SDR) | PulseAudio / PipeWire-Pulse | 实验性，见 [Linux 接入](linux.zh.md) |
| Android 8+ | MediaCodec / software | wgpu Vulkan + GLES fallback | AAudio | Available；SDR 已验证，extended-linear scRGB 等待 API 35 HDR 真机验收 |
| HarmonyOS API 18+ | AVCodec（H.264/HEVC）/ software | wgpu Vulkan，`OHNativeBuffer` 零拷贝导入 | OHAudio | Available；已在真机验证，CI 构建 OpenHarmony C ABI 但无设备侧运行验证 |
