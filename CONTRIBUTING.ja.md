# liberika へのコントリビュート

> [English](CONTRIBUTING.md) | [中文](CONTRIBUTING.zh.md)

liberika は NipaPlay の自社開発再生コアです：demux/decode から GPU プレゼンテーション、
オーディオ、字幕、弾幕までを所有する Rust エンジンで、C ABI と Flutter プラグインを通じて
ホストに公開されます。本書は作業のためのオリエンテーション地図です。

関連：[architecture.ja.md](docs/architecture.ja.md)（エンジン設計）、
[building.ja.md](docs/building.ja.md)（ツールチェーン + ネイティブ依存）、
[capi_reference.ja.md](docs/capi_reference.ja.md) と
[integration.ja.md](docs/integration.ja.md)（組み込み面）。

## リポジトリ構成

```
crates/erika              コアエンジン（ライブラリ）
crates/erika_capi         C ABI エクスポート層  →  erika.h
crates/erika_ffmpeg_sys   FFmpeg bindgen バインディング
packages/erika_flutter    Flutter プラグイン（macOS + iOS + tvOS + Windows + Android + HarmonyOS）
examples/                 検証 / スモーク / demo バイナリ
xtask/                    ネイティブ依存ビルドのオーケストレーション
docs/                     アーキテクチャと組み込みドキュメント
third_party/              ビルドされたネイティブ依存（gitignore 出力）
```

### `crates/erika` の内部

| モジュール | 責務 |
|-----------|------|
| `core` | 公開設定 + `RendererBackend` trait、`PlatformSurface`、`RendererBackendPreference`。 |
| `playback` | 再生エンジン：映像/音声 tick、マスタークロック、フレームスケジューラ、`VideoDecodePreference`。 |
| `ffmpeg` | demux、decode、resample、seek；`DecoderBackend`（software / VideoToolbox / D3D11VA / MediaCodec）。 |
| `audio` | `AudioOutputBackend` trait、リングバッファ、オーディオクロック。 |
| `renderer` | `metal`（Apple）、`d3d11`（Windows）、`wgpu`（クロスプラットフォームと Android）、`pipeline`（backend 非依存の色/トーンマップ/scaler 判断）。 |
| `overlay` / `subtitle` / `text` | overlay タイムライン；SRT/WebVTT/ASS + libass；フォントプロバイダ。 |
| `danmaku` | Bilibili XML/JSON 解析、衝突回避レイアウト、グリフアトラス。 |
| `presenter` | `PresenterRuntime`——player + renderer + audio + overlay をつなぐ。`render_tick` が駆動する対象。 |
| `source` | `MediaSource` trait——file + HTTP range。 |
| `apple` / `windows` / `android` | プラットフォームグルー：CoreAudio/AudioQueue/VideoToolbox/Metal 相互運用；WASAPI；AAudio/MediaCodec/native-window 相互運用。 |

## ランタイムモデル

- **`PresenterRuntime` がスタックを所有。** ホストは表示フレームごとに
  `render_tick(time_seconds)` を 1 回呼びます。runtime はデコード済みフレームを pump し、
  overlay（字幕 + 弾幕）を更新し、現在の `RendererBackend` で描画・present します。弾幕の
  plan 生成は `generation + media_time` でゲートされ、映像と同期します。
- **オーディオがマスタークロック。** オーディオ出力は独自の backend スレッド（WASAPI
  render スレッド、CoreAudio コールバック）で動きます。再生スケジューラは映像の
  presentation をオーディオクロックに対して vsync 量子化します。
- **イベントは channel で流れる。** プレイヤー状態変化は crossbeam channel に publish され、
  `poll_event` 経由でホストに届きます。
- **backend はプラガブル**で、3 つの trait の背後にあります——`RendererBackend`、
  `AudioOutputBackend`、そして `DecoderBackend` の選択——presenter をプラットフォーム
  非依存に保ちます。`RendererBackendPreference`（`PlatformNative` vs `WgpuFallback`）と
  `VideoDecodePreference` が具体的な backend を選びます。

## プラットフォーム backend の追加

新しいプラットフォームは通常 3 つのピースを意味し、それぞれ `#[cfg(target_os = …)]` の
背後に置きます：

1. **Decode** —— `ffmpeg.rs`（`DecoderBackend`）にハードウェアデバイスを設定するバリアント
   /パスを追加し（D3D11VA と VideoToolbox のパスを参照）、レンダラがゼロコピーで import
   できる `PlayerVideoFrame` を生成します。相互運用が使えないときはソフトデコードに
   フォールバック。
2. **Render** —— 新しい `renderer/<backend>.rs` に `RendererBackend`（`core.rs`）を実装：
   `attach_surface` / `detach_surface` / `resize_surface`、`upload_player_frame`（import した
   GPU 表現を所有）、`render_current_frame`（overlay を合成）、任意で
   `capture_current_frame`、`runtime_stats`、`set_luma_upscaler`。色/トーンマップの判断は
   `renderer::pipeline` を使い、他の backend と挙動を揃えます。D3D11 backend
   （`renderer/d3d11.rs`）が最新の実例です。
3. **Audio** —— `AudioOutputBackend`（`audio.rs`）を実装：`configure(PcmFormat)` / `start` /
   `pause` / `stop` / `push(PcmAudioFrame)` / `set_volume` / `state` / `stats`、そして A/V 同期の
   `clock_snapshot`。リングバッファ配管は `BufferedAudioOutput` / `AudioRingBuffer` を再利用
   （`windows.rs` の WASAPI が参考）。

その後 backend を presenter の選択に接続し、エンドツーエンド検証用に
`<platform>_native_demo` の例を追加します。

## C ABI は契約

`crates/erika_capi` はすべての非 Rust ホストの安定面です。変更時：

- **panic を制御する。** 復帰をサポートする箇所では `catch_unwind` を使い、捕捉した panic を戻り値の型に合うエラーとメッセージに変換します。現在、一部のエクスポートは捕捉せず、abort ビルドでは復帰できません。[panic 安全性](docs/capi_reference.ja.md#panic-安全性)を参照してください。
- **所有権を守る。** 渡す文字列は呼び出し側の所有で対応する `*_free` で解放。新しいものは
  [capi_reference.md](docs/capi_reference.md) に記載。
- **`erika.h` を合わせて再生成 / 手編集**し、新関数に注釈を付ける。
- **⚠️ Swift のミラー構造体を同期。** macOS/iOS/tvOS プラグインは C 構造体（例：
  `ErikaPresenterStats`）を Swift 側で手でミラーしています。`erika.h` の構造体を変更したら、
  `packages/erika_flutter` 内のmacOS、iOS、tvOS の 3 つの Swift ミラーファイルを更新してください。不一致は
  スタックを破壊し、明白なレイアウトエラーではなく誤解を招く autorelease-pool クラッシュ
  として現れることがあります。

## テスト

```sh
cargo test --workspace          # ユニット + 統合テスト
cargo clippy --workspace
cargo fmt --all
```

- プラットフォーム固有コードは `#[cfg]` でゲートされます。ある `cfg` ブランチを触ったら、
  `macos` / `ios` / `windows` / `android` / フォールバックのすべてがコンパイルできる状態を保ちます。
  ローカルでテストできないターゲットは CI に任せます。
- 神経アップスケーラの重みは `tests/artcnn_upscaler.rs`（Metal）と
  `tests/wgpu_artcnn.rs`（tiled wgpu）で onnxruntime リファレンスと照合されます。
  対応する path を再確認せずに kernel を変更しないこと。
- Android SDR playback と明示的な output fallback は検証済みです。API 35 HDR device の
  active path が `Rgba16Float + SCRGB_LINEAR` acceptance と recovery check を通るまで、
  extended-linear scRGB を実機検証済みと表記しないこと。
- native demo はスモークテストも兼ねます：
  `cargo run -p macos_native_demo -- --smoke-seconds 3 "$SAMPLE"`（および Windows 版）が
  パイプラインカウンタを出力します——変更後もハードデコードとゼロコピー相互運用が効くか
  確認するのに使います。

## 規約

- **Edition 2024**、Rust 1.92+。`cargo fmt` を実行し `clippy` をクリーンに保つ。
- **ホットパスや FFI 越しで `unwrap`/`panic` しない。** `Result` を返し、境界で
  `ErikaStatus` にマップする。
- **周囲のコードに合わせる**——命名、コメント密度、既存の `cfg` 構造。プラットフォーム
  グルーは `apple.rs` / `windows.rs` / `android.rs` に置き、エンジン全体に散らさない。
- **ドキュメントを同期。** アーキテクチャ/組み込みドキュメントと三言語 README
  （`README.md` + `readme/*.md`、`docs/*.{md,zh.md,ja.md}`）はユーザに見える変更を反映する
  こと。基準ドキュメントは英語で、翻訳は後続。

## Pull Request

変更は焦点を絞る。ビルド/テストしたプラットフォームと、CI に残すプラットフォームを明記。
関連ドキュメントは同じ PR で更新。大きな機能（新 backend、ABI 変更）は PR 説明に短い設計
ノートを添えると、レビュアーがスレッドと所有権の含意を追いやすくなります。

## ドキュメントの管理場所

ドキュメントの入口は [docs/README.md](docs/README.md) です。 SDK の使用法は package README、接続・設計は `docs/`、日付付きの調査結果は `docs/investigations/` に保存します。新しい topic を目次に追加し、API 変更と翻訳を同時に更新します。
