[English](readme/README.en.md) | [日本語](readme/README.ja.md)

# liberika

> 「GOOD！我是liberika，是NipaPlay里继mdk、video player、libmpv、media kit之后的第五个播放器内核。」
> 「即便算上你，也只有四个播放器内核！」

**NipaPlay 的自研播放内核。** Rust 实现，可嵌入，从解码到渲染一手包办。

> 名字取自《海猫鸣泣之时》的侦探 **古戸ヱリカ**。
> 而 [NipaPlay](https://github.com/AimesSoft/NipaPlay-Reload) 来自《寒蝉鸣泣之时》古手梨花的口癖「にぱー☆」——社区里大家都叫她「梨花」。
> 一个是台前的播放器，一个是幕后的引擎。同出一脉，互为表里。

宿主应用只需提供一个渲染表面并发送播放命令——解码、时序同步、音视频渲染、字幕、弹幕、音频输出均由 liberika 内部完成。原生视频层直接呈现；Flutter 纹理可交给宿主合成。

当前版本：**0.3.8**。从 [文档目录](docs/README.md) 选择接入、构建或内核设计说明。

**浏览器也能直接播放 MKV，并保留随片 ASS 字幕和字体附件。** [打开在线演示](https://liberika.aimes-soft.com/demo/)，
或 `npm install @aimessoft/liberika-web`。当前源码提供
[Web / WASM 后端](docs/web.zh.md)，已接入 Erika Web：浏览器 WebCodecs 解码音视频，
WASM 内的 libass 还原字幕字体、定位、描边与特效，Rust 弹幕与字幕按同一播放时钟
交给 WebGL2 合成。网站可以保留自己的界面，服务器提供原文件和 HTTP Range，
无需先在服务端拆出字幕或烧录进画面。

0.3.8 带来 Windows D3D11 截图、HTTP 来源的响应校验与停滞恢复、可取消的打开，
以及软件解码上传和字幕图集的性能改进。0.3.5–0.3.7 带来彩色 Emoji 弹幕、可选内嵌字体、
Bilibili mode 7 高级弹幕、Linux 线性 dma-buf 输出，以及 Linux x64 / ARM64 的
bundled、system 四种预编译包；详细变化与升级说明见 [更新日志](CHANGELOG.zh.md)。

## 能力

- 硬件解码与软件回退：VideoToolbox、D3D11VA、MediaCodec、AVCodec；Linux 提供 NVDEC / VA-API。
- Metal、Direct3D 11 与 wgpu 呈现，合成视频、字幕和弹幕；支持 HDR/EDR、Dolby Vision 映射，以及 ArtCNN、Anime4K、FSR1 和平台超分（可用性取决于设备与后端）。
- 本地文件与 HTTP(S) 播放，支持自定义请求头、后台预读和回退缓存。
- 音频主时钟同步、暂停、seek、倍速、多音轨、外挂字幕和多轨弹幕。
- Rust、C ABI、Flutter、SwiftPM 与 OpenHarmony ArkTS 接入。
- 浏览器 JavaScript / TypeScript 接入：MKV 解封装、ASS 字体附件、统一字幕与弹幕合成，字幕和弹幕可独立双倍采样。

## 选择接入方式

| 宿主 | 安装或入口 | 指南 |
|---|---|---|
| 浏览器 / JavaScript / TypeScript | `npm install @aimessoft/liberika-web`，或下载 `liberika-web-0.3.8.zip` | [Web 接入与部署](docs/web.zh.md) |
| Flutter | `flutter pub add erika_flutter`，当前 0.3.8 | [插件使用指南](packages/erika_flutter/README.zh.md) |
| Swift / Apple | Xcode 添加 `https://github.com/AimesSoft/liberika-swift`；SDK 最新公开版为 0.3.8（截至 2026-11-26） | [liberika-swift](https://github.com/AimesSoft/liberika-swift) |
| OpenHarmony / ArkTS | `ohpm install erika` | [ArkTS SDK](packages/erika_ohos/README.zh.md) |
| C / C++ | 下载匹配版本的 C ABI 归档与 `erika.h` | [原生接入](docs/integration.zh.md) |
| Rust | Git 依赖，固定 `tag = "v0.3.8"` | [构建与依赖](docs/building.zh.md) |
| Linux | x64 / ARM64，bundled / system 双模式，glibc 2.35 基线 | [Linux 接入](docs/linux.zh.md) |

截至 2026-11-26，OHPM 公开的最新版本为 0.3.8。原生二进制从
[GitHub Releases](https://github.com/AimesSoft/liberika/releases/tag/v0.3.8) 下载。

### 浏览器

```sh
npm install @aimessoft/liberika-web
```

```js
import { Player } from '@aimessoft/liberika-web';

const player = await Player.create({ canvas });
await player.open(fileOrURL);
await player.play();
```

先在[在线演示](https://liberika.aimes-soft.com/demo/)里试播，它直接从 npm CDN 加载同一个包。Vite 等打包器无需额外配置；
`@aimessoft/liberika-web-nofont` 不含约 6 MB 的回退字体，由宿主提供字体。
也可解压 Release 中的 `liberika-web-0.3.8.zip` 自行托管，或从源码构建：

```sh
# 准备 Emscripten、wasm-bindgen 等依赖后，一次构建全部 WASM 与运行时资源。
cargo run -p xtask -- web build
python3 scripts/serve_web.py
```

音视频编码由浏览器 WebCodecs 支持范围决定。
构建依赖、完整示例和 Erika 接入说明见 [浏览器指南](docs/web.zh.md)。

### Flutter

```dart
import 'package:erika_flutter/erika_flutter.dart';

final player = ErikaPlayer();
await player.open('/path/to/video.mp4');
await player.play();

// 放入 Widget 树；Apple 全播放器可选 ErikaWindowOverlayVideoView。
ErikaVideoView(player: player)
```

`ErikaTextureVideoView` 适合需要 Flutter 裁剪和颜色滤镜的视频；原生视频层适合
Apple EDR、Windows HDR10 等直接呈现。平台差异和生命周期在插件指南中说明。

### Rust

```toml
[dependencies]
erika = { git = "https://github.com/AimesSoft/liberika", tag = "v0.3.8" }
```

```rust
use erika::{MediaRequest, Player, PlayerConfig};

let player = Player::new(PlayerConfig::default());
player.open(MediaRequest::new("/path/to/video.mp4"))?;
player.play()?;
```

从源码接入前先准备对应平台的原生依赖。`Player` 用于控制和帧订阅；
`PresenterRuntime` 托管渲染与音频。C ABI 对应入口为 `ErikaHandle` 和
`ErikaPresenterHandle`，完整声明位于 [erika.h](crates/erika_capi/include/erika.h)。

Linux Rust 宿主还需启用 `wgpu`，并在消费者 workspace 根 manifest 中配置 HAL 补丁；
具体依赖、字体栈和构建要求见 [Linux 接入](docs/linux.zh.md)。

## 平台

| 平台 | 解码 | 渲染 | 音频 | 分发 |
|---|---|---|---|---|
| macOS 11+ | VideoToolbox / 软解 | Metal | CoreAudio | 预编译、Flutter、SwiftPM |
| iOS 13+ | VideoToolbox / 软解 | Metal | AudioQueue | XCFramework、Flutter、SwiftPM |
| tvOS 13+ | VideoToolbox / 软解 | Metal | AudioQueue | XCFramework、Flutter、SwiftPM |
| Windows 10+ | D3D11VA / 软解 | D3D11 | WASAPI | x64 / ARM64 预编译、Flutter |
| Android 8+ | MediaCodec / 软解 | wgpu Vulkan / GLES | AAudio | 四个 ABI 预编译、Flutter |
| OpenHarmony API 18+ | AVCodec / 软解 | wgpu | OHAudio | arm64 预编译、Flutter、OHPM |
| Linux（实验性） | NVDEC / VA-API / 软解 | wgpu X11 / Wayland | PulseAudio / pipewire-pulse | x64 / ARM64 × bundled / system 发布构建；Rust、C ABI、Flutter |
| 浏览器 | WebCodecs | WebGL2 / Canvas | Web Audio | npm、Release Web 归档或源码构建 |

各平台的视频表面和 HDR 路径在 [平台矩阵](docs/platform_matrix.zh.md) 中汇总。
浏览器后端当前输出 SDR，已在 Chrome 验证；需要 HTTPS 或 localhost，以及
WebCodecs 视频／音频解码、OffscreenCanvas、WebGL2 和 Web Audio。

## 开发

```sh
# Apple / Windows / Android / OpenHarmony：构建固定版本的原生依赖。
cargo run -p xtask -- deps build --all --profile lgpl
cargo build -p erika_capi
```

交叉编译需设置目标 triple；Linux 双模式入口为 `scripts/build_linux_release.sh`，
均以 Ubuntu 22.04 构建，system 可使用自行安装的兼容 FFmpeg 8 共享库。
构建参数在 [构建指南](docs/building.zh.md)，发布流程在 [发布指南](docs/releasing.zh.md)，
代码约定在 [贡献指南](CONTRIBUTING.zh.md)。

## 仓库结构

| 目录 | 内容 |
|---|---|
| `crates/erika` | 播放、解码、时钟、字幕、弹幕与渲染 |
| `crates/erika_capi` | C ABI 和公开头文件 |
| `crates/erika_ffmpeg_sys` | FFmpeg bindings |
| `packages/erika_flutter` | Flutter 插件 |
| `packages/erika_ohos` | OpenHarmony ArkTS SDK |
| `packages/erika_web` | 浏览器 Player、Worker、WebGL2 合成与 TypeScript 类型 |
| `examples` | 原生、Flutter、ArkTS 与浏览器播放器示例 |
| `xtask` | 原生依赖与完整 Web 包构建 |
| `docs` | 接入与设计文档；`investigations/` 保存历史调查和验证记录 |

版本变化见 [CHANGELOG](CHANGELOG.md)。

## 许可证

Rust workspace 使用 [MPL-2.0](LICENSE)。原生依赖的构建 profile 与许可说明见
[构建指南](docs/building.zh.md#许可证-profile) 和 [第三方声明](packaging/THIRD_PARTY_NOTICES.md)。
