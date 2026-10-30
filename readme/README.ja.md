[中文](../README.md) | [English](README.en.md) | [日本語](README.ja.md)

# liberika

> 「GOOD！私はliberika、NipaPlayにおいてmdk、video player、libmpv、media kitに次ぐ5番目のプレイヤーカーネルです。」
> 「あなたを数えても、プレイヤーカーネルは4つだけ！」

**NipaPlay の自社開発再生コア。** Rust 実装、組み込み可能、デコードからレンダリングまで一手に引き受けます。

> 名前の由来は『うみねこのなく頃に』の探偵 **古戸ヱリカ**。
> そして [NipaPlay](https://github.com/AimesSoft/NipaPlay-Reload) は『ひぐらしのなく頃に』の古手梨花の口癖「にぱー☆」から——コミュニティではみんな「梨花」と呼んでいます。
> 一方は表舞台のプレイヤー、もう一方は舞台裏のエンジン。同じ世界から生まれた、表裏一体の存在です。

ホストアプリケーションはレンダリングサーフェスの提供と再生コマンドの送信のみを行い、デコード、タイミング同期、映像レンダリング、字幕、弾幕、音声出力はすべて liberika 内部で完結します。

現在のリリースは **0.3.1** です。[ドキュメント一覧](../docs/README.ja.md) からガイドを選べます。

**MKV 内の ASS 字幕と添付フォントを使って、ブラウザーで直接再生できます。**
現在のソースには [Web / WASM バックエンド](../docs/web.md) が含まれ、Erika Web に
組み込み済みです。WebCodecs が映像・音声をデコードし、WASM の libass が字幕の
フォント・配置・縁取り・エフェクトを描画します。Rust 弾幕と字幕は同じ再生クロックに
従い、WebGL2 で合成されます。サイト独自の UI を維持したまま、サーバーは元ファイルを
HTTP Range で配信でき、字幕の事前抽出や映像への焼き付けは不要です。

## 機能

- VideoToolbox、D3D11VA、MediaCodec、AVCodec によるハードウェアデコードとソフトウェアフォールバック。Linux は実験的に NVDEC / VA-API に対応。
- Metal、D3D11、wgpu による映像・字幕・弾幕の合成、HDR/EDR、Dolby Vision マッピング、ArtCNN 輝度アップスケーリング。
- ローカルファイルと HTTP(S)、カスタムヘッダー、先読みとリワインドキャッシュ。
- 音声マスタークロック、再生・一時停止・シーク・倍速、トラック切替と外部字幕。
- Rust、C ABI、Flutter、SwiftPM、OpenHarmony ArkTS から組み込み可能。
- ブラウザーの JavaScript / TypeScript 組み込み：MKV の分離、ASS 添付フォント、字幕・弾幕の統合合成と独立したスーパーサンプリング。

## 組み込み

| ホスト | インストール / 入口 | ガイド |
|---|---|---|
| ブラウザー / JavaScript / TypeScript | `liberika-web-0.3.1.zip` またはソースビルド | [Web 組み込み・配信（英語）](../docs/web.md) |
| Flutter | `flutter pub add erika_flutter`（0.3.1） | [プラグイン](../packages/erika_flutter/README.ja.md) |
| Swift / Apple | Xcode に `https://github.com/AimesSoft/liberika-swift` を追加。公開済み SDK 最新版は 2026-10-31 時点で 0.3.0 | [liberika-swift](https://github.com/AimesSoft/liberika-swift) |
| OpenHarmony / ArkTS | `ohpm install erika` | [ArkTS SDK](../packages/erika_ohos/README.ja.md) |
| C / C++ | 同じバージョンの C ABI バンドルと `erika.h` | [ネイティブ組み込み](../docs/integration.ja.md) |
| Rust | Git 依存を `v0.3.1` に固定 | [ビルド](../docs/building.ja.md) |
| Linux | システム FFmpeg 8 とパッチ済み libass、ソースビルド | [Linux](../docs/linux.md) |

2026-10-31 時点の OHPM 公開最新バージョンは 0.3.0 です。0.3.1 はアップロード後の審査待ちです。
ネイティブバンドルは [GitHub Releases](https://github.com/AimesSoft/liberika/releases/tag/v0.3.1) にあります。

### ブラウザー

```sh
# 先に Emscripten、wasm-bindgen などのビルド依存を準備します。
cargo run -p xtask -- web build
python3 scripts/serve_web.py
```

`http://127.0.0.1:18765` でサンプルを開けます。`target/web/` のランタイムを配信し、
`Player.create({ canvas })` と `player.open(fileOrURL)` で独自の UI に組み込みます。
Release の `liberika-web-0.3.1.zip` は `index.html` デモを含みます。
全体を展開し HTTPS または localhost で配信できます。ソースビルドも可能で、npm には未公開です。音声・映像コーデックは
ブラウザーの WebCodecs 対応範囲に依存します。依存、サンプル、Erika への組み込みは
[ブラウザーガイド（英語）](../docs/web.md) を参照してください。

### Flutter

```dart
import 'package:erika_flutter/erika_flutter.dart';

final player = ErikaPlayer();
await player.open('/path/to/video.mp4');
await player.play();

// Widget ツリーに追加。Apple の全画面プレイヤーにはネイティブ overlay も選べます。
ErikaVideoView(player: player)
```

Flutter のクリップやカラーフィルターには `ErikaTextureVideoView`、Apple EDR や
Windows HDR10 にはネイティブ映像サーフェスを使います。選択とライフサイクルは
プラグインガイドにまとめています。

### Rust

```toml
[dependencies]
erika = { git = "https://github.com/AimesSoft/liberika", tag = "v0.3.1" }
```

```rust
use erika::{MediaRequest, Player, PlayerConfig};

let player = Player::new(PlayerConfig::default());
player.open(MediaRequest::new("/path/to/video.mp4"))?;
player.play()?;
```

先にネイティブ依存を準備します。`Player` は制御とフレーム購読、`PresenterRuntime`
はレンダリングと音声を担当します。C の入口は `ErikaHandle` と `ErikaPresenterHandle`、
宣言は [erika.h](../crates/erika_capi/include/erika.h) にあります。

## プラットフォーム

| プラットフォーム | デコード | レンダリング | 音声 | 配布 |
|---|---|---|---|---|
| macOS 11+ | VideoToolbox / software | Metal | CoreAudio | バンドル、Flutter、SwiftPM |
| iOS 13+ | VideoToolbox / software | Metal | AudioQueue | XCFramework、Flutter、SwiftPM |
| tvOS 13+ | VideoToolbox / software | Metal | AudioQueue | XCFramework、Flutter、SwiftPM |
| Windows 10+ | D3D11VA / software | D3D11 | WASAPI | x64 / ARM64 バンドル、Flutter |
| Android 8+ | MediaCodec / software | wgpu Vulkan / GLES | AAudio | 4 ABI バンドル、Flutter |
| OpenHarmony API 18+ | AVCodec / software | wgpu | OHAudio | arm64 バンドル、Flutter、OHPM |
| Linux（実験的） | NVDEC / VA-API / software | wgpu X11 / Wayland | PulseAudio / pipewire-pulse | Rust、C ABI、Flutter ソースビルド |
| ブラウザー | WebCodecs | WebGL2 / Canvas | Web Audio | Release の Web archive または JS / WASM ソースビルド |

サーフェスと HDR の対応は [プラットフォーム表](../docs/platform_matrix.zh.md) にまとめています。
ブラウザー版は現在 SDR 出力で、Chrome で検証済みです。HTTPS または localhost、
WebCodecs の映像・音声デコード、OffscreenCanvas、WebGL2、Web Audio が必要です。

## 開発

```sh
# Apple / Windows / Android / OpenHarmony のネイティブ依存
cargo run -p xtask -- deps build --all --profile lgpl
cargo build -p erika_capi
```

クロスビルドでは target triple を指定します。Linux はシステムライブラリを使います。
[ビルド](../docs/building.ja.md)、[リリース](../docs/releasing.ja.md)、
[開発者ガイド](../CONTRIBUTING.ja.md) に手順をまとめています。

## リポジトリ

| ディレクトリ | 内容 |
|---|---|
| `crates/erika` | 再生、デコード、クロック、字幕、弾幕、レンダリング |
| `crates/erika_capi` | C ABI と公開ヘッダー |
| `crates/erika_ffmpeg_sys` | FFmpeg bindings |
| `packages/erika_flutter` | Flutter プラグイン |
| `packages/erika_ohos` | OpenHarmony ArkTS SDK |
| `packages/erika_web` | ブラウザー Player、Worker、WebGL2 合成と TypeScript 型 |
| `examples` | ネイティブ、Flutter、ArkTS、ブラウザープレイヤーのサンプル |
| `xtask` | ネイティブ依存と Web パッケージのビルド |
| `docs` | ガイドと設計。過去の調査・検証記録は `investigations/` |

変更履歴は [CHANGELOG](../CHANGELOG.md) にあります。

## ライセンス

Rust workspace は [MPL-2.0](../LICENSE) です。ネイティブ依存のプロファイルとライセンスは
[ビルドガイド](../docs/building.ja.md#ライセンス-profile) と
[第三者ライセンス](../packaging/THIRD_PARTY_NOTICES.md) に記載しています。
