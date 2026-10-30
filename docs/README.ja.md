[中文](README.md) | [English](README.en.md) | 日本語

# liberika ドキュメント

現在の導入ガイドは **0.3.0** を基準としています。まず使用する SDK のインストールと
利用ガイドを読み、描画ホストの作成やカーネルの開発には以下の資料を参照してください。

ブラウザー / WASM バックエンドは Erika Web に統合され、MKV の ASS 字幕と添付フォント、libass、Rust 弾幕、WebGL2 合成、WebCodecs デコードに対応します。完全なランタイムはソースからビルドするか Release の Web アーカイブをダウンロードできます。構築と配置は [Web ガイド](web.ja.md) を参照してください。npm には未公開です。

## 導入

| トピック | 中文 | English | 日本語 |
|---|---|---|---|
| Flutter SDK：インストール、ビュー、Dart API | [插件指南](../packages/erika_flutter/README.zh.md) | [Plugin guide](../packages/erika_flutter/README.md) | [プラグイン](../packages/erika_flutter/README.ja.md) |
| Swift SDK：SwiftPM とネイティブビュー | [liberika-swift](https://github.com/AimesSoft/liberika-swift/blob/main/README.zh.md) | [liberika-swift](https://github.com/AimesSoft/liberika-swift) | [日本語](https://github.com/AimesSoft/liberika-swift/blob/main/README.ja.md) |
| ブラウザー / WASM：MKV、ASS フォント、弾幕、配置 | [浏览器接入](web.zh.md) | [Browser integration](web.md) | [ブラウザー導入](web.ja.md) |
| OpenHarmony ArkTS SDK | [接入指南](../packages/erika_ohos/README.zh.md) | [Package guide](../packages/erika_ohos/README.md) | [組み込み](../packages/erika_ohos/README.ja.md) |
| C/C++ などのネイティブホスト | [接入指南](integration.zh.md) | [Integration](integration.md) | [組み込み](integration.ja.md) |
| C ABI：シグネチャ、型、所有権 | [参考手册](capi_reference.zh.md) | [C ABI reference](capi_reference.md) | [C ABI](capi_reference.ja.md) |
| Linux / WSLg（実験的） | [Linux 接入](linux.zh.md) | [Linux integration](linux.md) | [Linux 組み込み](linux.ja.md) |
| Flutter ネイティブサーフェスとプラットフォームブリッジ | [嵌入设计](flutter_embedding.zh.md) | [Flutter embedding](flutter_embedding.md) | [Flutter 組み込み](flutter_embedding.ja.md) |

システムバージョン、描画サーフェス、ビルド済み成果物の範囲はプラットフォーム・配布
マトリクスを参照してください：[中文](platform_matrix.zh.md) · [English](platform_matrix.md) · [日本語](platform_matrix.ja.md)。

## カーネル設計

| トピック | 中文 | English | 日本語 |
|---|---|---|---|
| アーキテクチャ | [架构总览](architecture.zh.md) | [Architecture](architecture.md) | [アーキテクチャ](architecture.ja.md) |
| 弾幕 | [弹幕架构](danmaku_architecture.md) | [Danmaku](danmaku_architecture.en.md) | [弾幕](danmaku_architecture.ja.md) |
| Dolby Vision / HDR 色処理 | [HDR 映射](dolby-vision.zh.md) | [HDR mapping](dolby-vision.md) | [HDR マッピング](dolby-vision.ja.md) |
| WSL D3D12 GPU フレームコピー（実験的） | [GPU 帧复制](wsl-gpu-copy.zh.md) | [GPU-copy bridge](wsl-gpu-copy.md) | [GPU コピー](wsl-gpu-copy.ja.md) |

## 開発とリリース

| トピック | 中文 | English | 日本語 |
|---|---|---|---|
| 依存関係とクロスコンパイル | [构建](building.zh.md) | [Building](building.md) | [ビルド](building.ja.md) |
| リリースアーカイブと各 SDK | [发布](releasing.zh.md) | [Releasing](releasing.md) | [リリース](releasing.ja.md) |
| 貢献の規約 | [贡献](../CONTRIBUTING.zh.md) | [Contributing](../CONTRIBUTING.md) | [開発者ガイド](../CONTRIBUTING.ja.md) |
| バージョン履歴 | [更新日志](../CHANGELOG.zh.md) | [Changelog](../CHANGELOG.md) | [更新履歴](../CHANGELOG.ja.md) |

## 調査・検証記録

[investigations/](investigations/README.ja.md) には、トラブル調査、性能実験、特定環境での
検証を、日付とコードの基準を明記して保存しています。実装の根拠を追跡するための記録であり、
現在のインストール、API、リリース手順は上記ガイドを参照してください。

## ドキュメントの保守

- ルート README はプロジェクト、導入方法、プラットフォームを紹介し、各 SDK README はその公開利用方法を管理します。
- `docs/` は導入、カーネル設計、開発手順を扱い、実験結果は `docs/investigations/` に保存します。
- API の変更は `crates/erika_capi/include/erika.h` と SDK 実装に照らして確認し、対応する三言語のガイドを更新します。
- 新しいトピックはこの索引に追加し、旧ファイル名を残す互換入口からは現行文書のみを参照します。
