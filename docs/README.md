中文 | [English](README.en.md) | [日本語](README.ja.md)

# liberika 文档 / Documentation

当前接入说明以 **0.4.0** 为基线。选择 SDK 后，先读其安装和使用指南；需要自建
渲染宿主或开发内核时，再进入下列参考文档。

当前源码另提供 **浏览器 / WASM 播放后端**，已接入 Erika Web。支持在浏览器读取
MKV 内嵌 ASS 和字体附件，由 libass 绘制字幕、Rust 引擎绘制弹幕，统一交给 WebGL2
合成，音视频使用 WebCodecs 解码。从 [Web 接入指南](web.zh.md) 开始，可一次构建
完整运行时并接入自有网页界面；也可下载 Release 提供的 Web 归档，尚未发布到 npm。

## 接入 / Integration

| 主题 / Topic | 中文 | English | 日本語 |
|---|---|---|---|
| Flutter SDK：安装、视图与 Dart API | [插件指南](../packages/erika_flutter/README.zh.md) | [Plugin guide](../packages/erika_flutter/README.md) | [プラグイン](../packages/erika_flutter/README.ja.md) |
| Swift SDK：SwiftPM 与原生视图 | [liberika-swift](https://github.com/AimesSoft/liberika-swift/blob/main/README.zh.md) | [liberika-swift](https://github.com/AimesSoft/liberika-swift) | [日本語](https://github.com/AimesSoft/liberika-swift/blob/main/README.ja.md) |
| Browser / WASM：MKV、ASS 字体、弹幕与部署 | [浏览器接入](web.zh.md) | [Browser integration](web.md) | [ブラウザー導入](web.ja.md) |
| OpenHarmony ArkTS SDK | [接入指南](../packages/erika_ohos/README.zh.md) | [Package guide](../packages/erika_ohos/README.md) | [組み込み](../packages/erika_ohos/README.ja.md) |
| C/C++ 等原生宿主 / Native hosts | [接入指南](integration.zh.md) | [Integration](integration.md) | [組み込み](integration.ja.md) |
| C ABI：签名、类型、所有权 / Reference | [参考手册](capi_reference.zh.md) | [C ABI reference](capi_reference.md) | [C ABI](capi_reference.ja.md) |
| 外部 GPU 纹理：原生句柄与同步 / Native textures | [纹理输出](external_textures.zh.md) | [Texture output](external_textures.md) | [テクスチャ出力](external_textures.ja.md) |
| Linux / WSLg（实验性） | [Linux 接入](linux.zh.md) | [Linux integration](linux.md) | [Linux 組み込み](linux.ja.md) |
| Flutter 原生表面与平台桥接 / Native surfaces | [嵌入设计](flutter_embedding.zh.md) | [Flutter embedding](flutter_embedding.md) | [Flutter 組み込み](flutter_embedding.ja.md) |

平台能力与分发矩阵汇总系统版本、渲染表面与预编译范围：[中文](platform_matrix.zh.md) · [English](platform_matrix.md) · [日本語](platform_matrix.ja.md)。

## 内核设计 / Kernel design

| 主题 / Topic | 中文 | English | 日本語 |
|---|---|---|---|
| 架构 / Architecture | [架构总览](architecture.zh.md) | [Architecture](architecture.md) | [アーキテクチャ](architecture.ja.md) |
| 弹幕 / Danmaku | [弹幕架构](danmaku_architecture.md) | [Danmaku](danmaku_architecture.en.md) | [弾幕](danmaku_architecture.ja.md) |
| Dolby Vision / HDR 色彩处理 | [HDR 映射](dolby-vision.zh.md) | [HDR mapping](dolby-vision.md) | [HDR マッピング](dolby-vision.ja.md) |
| WSL D3D12 GPU 帧复制（实验性） | [GPU 帧复制](wsl-gpu-copy.zh.md) | [GPU-copy bridge](wsl-gpu-copy.md) | [GPU コピー](wsl-gpu-copy.ja.md) |

## 开发与发布 / Development and releases

| 主题 / Topic | 中文 | English | 日本語 |
|---|---|---|---|
| 构建依赖与交叉编译 / Building | [构建](building.zh.md) | [Building](building.md) | [ビルド](building.ja.md) |
| 发布归档与生态 SDK / Releasing | [发布](releasing.zh.md) | [Releasing](releasing.md) | [リリース](releasing.ja.md) |
| 贡献约定 / Contributing | [贡献](../CONTRIBUTING.zh.md) | [Contributing](../CONTRIBUTING.md) | [開発者ガイド](../CONTRIBUTING.ja.md) |
| 版本变化 / Changelog | [更新日志](../CHANGELOG.zh.md) | [Changelog](../CHANGELOG.md) | [更新履歴](../CHANGELOG.ja.md) |

## 调查与验证记录 / Investigation archive

[investigations/](investigations/README.md) 保存问题排查、性能实验与特定环境的验证记录，
并标明其时间和代码基线。它们用于追踪实现依据；当前安装、接口和发布步骤由上面的指南维护。

## 文档维护

- 根 README 介绍项目、安装入口和平台；SDK README 维护该 SDK 的公开用法。
- `docs/` 维护接入、内核设计与开发流程；实验结果放在 `docs/investigations/`。
- API 变更按 `crates/erika_capi/include/erika.h` 和 SDK 实现核对，并同步对应三语指南。
- 新主题加入本目录；保留旧文件名的兼容入口时，入口只指向现行文档。
