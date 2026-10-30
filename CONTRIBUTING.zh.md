# 为 liberika 贡献

> [English](CONTRIBUTING.md) | [日本語](CONTRIBUTING.ja.md)

liberika 是 NipaPlay 的自研播放内核:一个 Rust 引擎,从 demux/decode 一路负责到 GPU
呈现、音频、字幕和弹幕,并通过 C ABI 和 Flutter 插件暴露给宿主。本文档是上手地图。

另见:[architecture.zh.md](docs/architecture.zh.md)(引擎设计)、
[building.zh.md](docs/building.zh.md)(工具链 + 原生依赖)、
[capi_reference.zh.md](docs/capi_reference.zh.md) 与
[integration.zh.md](docs/integration.zh.md)(接入面)。

## 仓库布局

```
crates/erika              核心引擎(库)
crates/erika_capi         C ABI 导出层  →  erika.h
crates/erika_ffmpeg_sys   FFmpeg bindgen 绑定
packages/erika_flutter    Flutter 插件(macOS + iOS + tvOS + Windows + Android + HarmonyOS)
examples/                 验证 / 冒烟 / demo 程序
xtask/                    原生依赖构建编排
docs/                     架构与接入文档
third_party/              构建出的原生依赖(gitignore 输出)
```

### `crates/erika` 内部

| 模块 | 职责 |
|------|------|
| `core` | 公共配置 + `RendererBackend` trait、`PlatformSurface`、`RendererBackendPreference`。 |
| `playback` | 播放引擎:视频/音频 tick、主时钟、帧调度器、`VideoDecodePreference`。 |
| `ffmpeg` | demux、decode、resample、seek;`DecoderBackend`(software / VideoToolbox / D3D11VA / MediaCodec)。 |
| `audio` | `AudioOutputBackend` trait、ring buffer、音频时钟。 |
| `renderer` | `metal`(Apple)、`d3d11`(Windows)、`wgpu`(跨平台及 Android)、`pipeline`(后端无关的色彩/tone-map/scaler 决策)。 |
| `overlay` / `subtitle` / `text` | overlay 时间线;SRT/WebVTT/ASS + libass;字体提供者。 |
| `danmaku` | Bilibili XML/JSON 解析、碰撞避让布局、glyph atlas。 |
| `presenter` | `PresenterRuntime`——串起 player + renderer + audio + overlay;`render_tick` 所驱动的对象。 |
| `source` | `MediaSource` trait——file + HTTP range。 |
| `apple` / `windows` / `android` | 平台胶水:CoreAudio/AudioQueue/VideoToolbox/Metal 互操作（macOS/iOS/tvOS）;WASAPI;AAudio/MediaCodec/native-window 互操作。 |

## 运行时模型

- **`PresenterRuntime` 拥有整个栈。** 宿主每个显示帧调一次
  `render_tick(time_seconds)`;runtime 拉取解码帧、更新 overlay(字幕 + 弹幕)、通过当前
  `RendererBackend` 渲染并呈现。弹幕 plan 生成以 `generation + media_time` 门控,与视频
  保持同步。
- **音频是主时钟。** 音频输出跑在自己的后端线程(WASAPI render 线程、CoreAudio
  回调);播放调度器把视频呈现按 vsync 量化对齐到音频时钟。
- **事件经 channel 流动。** 播放器状态变化发布到 crossbeam channel,经 `poll_event`
  呈现给宿主。
- **后端可插拔**,藏在三个 trait 之后——`RendererBackend`、`AudioOutputBackend`,以及
  `DecoderBackend` 选择——使 presenter 保持平台无关。`RendererBackendPreference`
  (`PlatformNative` vs `WgpuFallback`)和 `VideoDecodePreference` 选择具体后端。

## 新增一个平台后端

新平台通常意味着三块,各自藏在 `#[cfg(target_os = …)]` 之后:

1. **解码** —— 在 `ffmpeg.rs`(`DecoderBackend`)里加一个变体/路径,配置硬件设备
   (参考 D3D11VA 和 VideoToolbox 路径),并产出渲染器可零拷贝导入的 `PlayerVideoFrame`。
   互操作不可用时回退软解。
2. **渲染** —— 在新的 `renderer/<backend>.rs` 里实现 `RendererBackend`(`core.rs`):
   `attach_surface` / `detach_surface` / `resize_surface`、`upload_player_frame`
   (拥有导入的 GPU 表示)、`render_current_frame`(合成 overlay),以及可选的
   `capture_current_frame`、`runtime_stats`、`set_luma_upscaler`。用 `renderer::pipeline`
   做色彩/tone-map 决策,使行为与其它后端一致。D3D11 后端(`renderer/d3d11.rs`)是最近的
   完整范例。
3. **音频** —— 实现 `AudioOutputBackend`(`audio.rs`):`configure(PcmFormat)` / `start` /
   `pause` / `stop` / `push(PcmAudioFrame)` / `set_volume` / `state` / `stats`,以及用于
   A/V 同步的 `clock_snapshot`。复用 `BufferedAudioOutput` / `AudioRingBuffer` 做 ring
   buffer 管道(WASAPI 在 `windows.rs`,是参考实现)。

然后把后端接进 presenter 选择,并加一个 `<platform>_native_demo` 示例做端到端验证。

## C ABI 是一份契约

`crates/erika_capi` 是所有非 Rust 宿主的稳定接入面。改它时:

- **控制 panic。** 在支持恢复的位置使用 `catch_unwind`，并将捕获的 panic 映射为适合返回类型的错误和消息。目前部分导出没有捕获，abort 构建无法恢复；见 [panic 安全](docs/capi_reference.zh.md#panic-安全)。
- **尊重所有权。** 交出的字符串归调用方所有、由配套 `*_free` 释放;新增的要在
  [capi_reference.md](docs/capi_reference.md) 里记录。
- **同步重生成 / 手改 `erika.h`** 以匹配,并给新函数加注释。
- **⚠️ 同步 Swift 镜像结构体。** macOS/iOS/tvOS 插件在 Swift 侧手写镜像了 C 结构体(如
  `ErikaPresenterStats`)。若你改了 `erika.h` 里的结构体,必须**同时**更新
  `packages/erika_flutter` 中 macOS、iOS、tvOS 三个平台的 Swift 镜像文件;不匹配会破坏栈,可能表现为
  误导性的 autorelease-pool 崩溃,而非明显的布局错误。

## 测试

```sh
cargo test --workspace          # 单元 + 集成测试
cargo clippy --workspace
cargo fmt --all
```

- 平台相关代码以 `#[cfg]` 门控;改某个 `cfg` 分支时,保持 `macos` / `ios` / `windows` /
  `android` / `tvos` / `ohos` / `linux` / fallback 各分支都能编译。本地测不了的目标交给 CI。
- 神经超分权重会在 `tests/artcnn_upscaler.rs`（Metal）和
  `tests/wgpu_artcnn.rs`（分块 wgpu）中对照 onnxruntime 参考验证；修改 kernel 前必须
  重新核对对应路径。
- Android SDR 播放和明确的 output fallback 已验证。在 API 35 HDR 真机 active path 通过
  `Rgba16Float + SCRGB_LINEAR` 及恢复检查前，不得将 extended-linear scRGB 标记为真机已验证。
- 原生 demo 兼作冒烟测试:
  `cargo run -p macos_native_demo -- --smoke-seconds 3 "$SAMPLE"`(及 Windows 等价命令)
  打印流水线计数器——用它确认改动后硬解和零拷贝互操作仍然生效。

## 约定

- **Edition 2024**,Rust 1.92+。跑 `cargo fmt`,保持 `clippy` 干净。
- **热路径或跨 FFI 上不 `unwrap`/`panic`。** 返回 `Result`,在边界处映射为
  `ErikaStatus`。
- **与周围代码一致**——命名、注释密度、既有的 `cfg` 结构。平台胶水留在 `apple.rs` /
  `windows.rs` / `android.rs`,别散进引擎。
- **保持文档同步。** 架构/接入文档和三语 README(`README.md` + `readme/*.md`、
  `docs/*.{md,zh.md,ja.md}`)应反映用户可见的改动。基准文档为英文,翻译随后。

## Pull Request

保持改动聚焦;注明你构建/测试过的平台,以及留给 CI 的平台。在同一个 PR 里更新相关文档。
较大的特性(新后端、ABI 改动)在 PR 描述里附一段简短设计说明,有助于评审跟进线程与所有权
的影响。

## 文档维护位置

文档入口为 [docs/README.md](docs/README.md)。 SDK 用法放在各 package README，接入与内核设计放在 `docs/`，带日期的调查结果放在 `docs/investigations/`。新增主题加入文档目录，API 变更同步对应三语指南。
