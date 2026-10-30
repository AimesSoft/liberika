[中文](README.md) | English | [日本語](README.ja.md)

# liberika documentation

The current integration guides use **0.3.2** as their baseline. Start with the
installation and usage guide for your SDK, then use the references below when
building a rendering host or developing the kernel.

The browser / WASM backend is integrated into Erika Web and supports MKV ASS subtitles and attached fonts, libass, Rust danmaku, WebGL2 composition and WebCodecs decoding. See the [Web guide](web.md) to build or download the Release Web archive and deploy it; the package is not published to npm.

## Integration

| Topic | 中文 | English | 日本語 |
|---|---|---|---|
| Flutter SDK: installation, views and Dart API | [插件指南](../packages/erika_flutter/README.zh.md) | [Plugin guide](../packages/erika_flutter/README.md) | [プラグイン](../packages/erika_flutter/README.ja.md) |
| Swift SDK: SwiftPM and native views | [liberika-swift](https://github.com/AimesSoft/liberika-swift/blob/main/README.zh.md) | [liberika-swift](https://github.com/AimesSoft/liberika-swift) | [日本語](https://github.com/AimesSoft/liberika-swift/blob/main/README.ja.md) |
| Browser / WASM: MKV, ASS fonts, danmaku and deployment | [浏览器接入](web.zh.md) | [Browser integration](web.md) | [ブラウザー導入](web.ja.md) |
| OpenHarmony ArkTS SDK | [接入指南](../packages/erika_ohos/README.zh.md) | [Package guide](../packages/erika_ohos/README.md) | [組み込み](../packages/erika_ohos/README.ja.md) |
| Native hosts: C/C++ and others | [接入指南](integration.zh.md) | [Integration](integration.md) | [組み込み](integration.ja.md) |
| C ABI: signatures, types and ownership | [参考手册](capi_reference.zh.md) | [C ABI reference](capi_reference.md) | [C ABI](capi_reference.ja.md) |
| Native GPU textures: handles and synchronization | [纹理输出](external_textures.zh.md) | [Texture output](external_textures.md) | [テクスチャ出力](external_textures.ja.md) |
| Linux / WSLg (experimental) | [Linux 接入](linux.zh.md) | [Linux integration](linux.md) | [Linux 組み込み](linux.ja.md) |
| Flutter native surfaces and platform bridges | [嵌入设计](flutter_embedding.zh.md) | [Flutter embedding](flutter_embedding.md) | [Flutter 組み込み](flutter_embedding.ja.md) |

The platform and distribution matrix covers system versions, rendering surfaces
and prebuilt availability: [中文](platform_matrix.zh.md) · [English](platform_matrix.md) · [日本語](platform_matrix.ja.md).

## Kernel design

| Topic | 中文 | English | 日本語 |
|---|---|---|---|
| Architecture | [架构总览](architecture.zh.md) | [Architecture](architecture.md) | [アーキテクチャ](architecture.ja.md) |
| Danmaku | [弹幕架构](danmaku_architecture.md) | [Danmaku](danmaku_architecture.en.md) | [弾幕](danmaku_architecture.ja.md) |
| Dolby Vision / HDR color processing | [HDR 映射](dolby-vision.zh.md) | [HDR mapping](dolby-vision.md) | [HDR マッピング](dolby-vision.ja.md) |
| WSL D3D12 GPU frame copying (experimental) | [GPU 帧复制](wsl-gpu-copy.zh.md) | [GPU-copy bridge](wsl-gpu-copy.md) | [GPU コピー](wsl-gpu-copy.ja.md) |

## Development and releases

| Topic | 中文 | English | 日本語 |
|---|---|---|---|
| Dependencies and cross-compilation | [构建](building.zh.md) | [Building](building.md) | [ビルド](building.ja.md) |
| Release archives and ecosystem SDKs | [发布](releasing.zh.md) | [Releasing](releasing.md) | [リリース](releasing.ja.md) |
| Contribution conventions | [贡献](../CONTRIBUTING.zh.md) | [Contributing](../CONTRIBUTING.md) | [開発者ガイド](../CONTRIBUTING.ja.md) |
| Version history | [更新日志](../CHANGELOG.zh.md) | [Changelog](../CHANGELOG.md) | [更新履歴](../CHANGELOG.ja.md) |

## Investigation archive

[investigations/](investigations/README.en.md) contains troubleshooting, performance
experiments and environment-specific validation, with their dates and code
baselines. These records explain the evidence behind implementation decisions;
use the guides above for current installation, API and release procedures.

## Maintaining documentation

- The root README introduces the project, installation options and platforms; each SDK README maintains its public usage instructions.
- `docs/` covers integration, kernel design and development; experiments belong in `docs/investigations/`.
- Check API changes against `crates/erika_capi/include/erika.h` and SDK implementations, and update the corresponding guides in all three languages.
- Add new topics to this index; compatibility entries retaining old filenames should point only to current documents.
