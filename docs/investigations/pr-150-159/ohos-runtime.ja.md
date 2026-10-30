# HarmonyOS エミュレーター実行記録

[中文](ohos-runtime.zh.md) · [English](ohos-runtime.en.md)

日付：2026-10-08。コード：`codex/pr-150-159-reviewed` のレビュー結果。完全な製品版 Rust runtime と NAPI ブリッジを使用し、プレーヤー、デコーダー、音声出力、Surface 実装は置き換えていません。

## 環境と成果物

- DevEco Studio 26.0.0.851。インストーラーの Huawei Authenticode 署名は有効。
- 公式 HarmonyOS 5.1.0.110(SP2DEVC00E110R4P11) イメージ、API 18、x86_64。
- インスタンス ErikaReview、hdc `127.0.0.1:5555`、Linux 5.10.210 x86_64。
- Rust 1.93.1、OpenHarmony SDK 5.1.0.107、`--no-default-features --features wgpu`。
- HAP は Studio 同梱 SDK 26、compatibleSdkVersion 18 でビルドし、API 18 エミュレーターへのインストールに成功。
- 現在の runtime の ARM64 ビルドと両ブリッジのリンクも成功。ARM64 の実行はなし。

x86_64 runtime SHA256：`7325a0e0915d828cee42c3dc64e6aa5e6870d2a15415619d9662c3dbafac8ad3`。

ArkTS ブリッジ：`6a8737bc254916797683935d60e9099a5f617e9158e5d145f2b588192b40e7f2`。Flutter ブリッジ：`f1329a2f9405a8f894491a72c54aecb2f4f303e9ceced786124704dd1b090ecb`。HAP パッケージング時に strip されるため、これらはパッケージ前の元のリンク成果物の値であり、HAP 内の stripped ファイルとは直接比較できません。

動画は元レポートの 10 秒 BBB H.264 ファイルに 48 kHz / 440 Hz の AAC 音声を追加し、動画ストリームはコピーしました。音動画フィクスチャ SHA256：`980be2f90110acf9c407581986453e483f5d18947569fe329c3eea64c2714bff`。カバー画像付き MP3 は同じ調査で使用済みの `cover-5s.mp3` です。

## 実行手順と結果

HAP の 2 つの XComponent をそれぞれ ArkTS と Flutter の製品版ブリッジに渡し、順に実行しました。各フローは 4 秒再生、2 秒一時停止、10 秒の EOF へシーク、3.5 秒へ戻して 1.25 倍速で再開します。次に実際の NativeWindow を detach/attach し、カバー画像付き MP3 を再生、64 回の作成/破棄と旧 ID 呼び出しを行い、再作成したプレーヤーで再生しました。

| 観測点 | ArkTS | Flutter ブリッジ |
|---|---:|---:|
| 初回再生の rendered / 音声消費フレーム | 111 / 187236 | 116 / 191694 |
| 一時停止と EOF シーク後の decoded | 114 → 114 | 121 → 121 |
| EOF シーク後の公開 state | 4（Paused） | 4（Paused） |
| Surface 再接続後の rendered / 音声消費フレーム | 427 / 248688 | 431 / 248688 |
| カバー付き MP3 の消費 / 残りキュー | 235190 / 0 | 235190 / 0 |
| 再作成後の rendered / 音声消費フレーム | 91 / 151572 | 86 / 142656 |
| 動画 rendered 合計（2 インスタンス） | 518 | 517 |
| 全スナップショットの render / import / audio failures | 0 / 0 / 0 | 0 / 0 / 0 |

MP3 は音声トラック 1 本のみを公開し、decoded/rendered video はともに 0、カバーは動画として選択されませんでした。旧 ID 2 の stop は `{ok:false, status:1, error:"unknown Erika player"}` を返し、旧 destroy 後も新 ID 67 が生存して実際に再生しました。両ブリッジの単調増加 ID と実際のホスト呼び出しを検証しています。一時停止後に処理中のデコードフレームが 1 枚ありましたが（ArkTS 113 → 114）、安定した一時停止から EOF シークの区間に新規デコードはありませんでした。

動画はエミュレーターのシステム decoder を使用し `hardwareVideoFrames` としてカウントされましたが、出力はすべて CPU fallback を経由し、`zeroCopyVideoFrames` は 0 です。実機のハードウェアデコード効率、NativeBuffer ゼロコピー、物理スピーカーの聴感は検証できていません。libass はビルドしておらず、字幕、HDR、実際のデバイス喪失もこの OHOS 検証の対象外です。Flutter 部分は製品版 NAPI ブリッジを呼び出しましたが、完全な Flutter エンジンは起動していません。Dart 側の証拠は元レポートのテストと分析です。

![両ブリッジの実動画画面](evidence/ohos-live-finished.png)

スクリーンショットは hdc で取得・確認しました。リポジトリ内は幅 660 ピクセルに縮小し、元の 1320×2232 画像はローカル証拠ディレクトリにあります。

## ベースライン比較の限界

両ブリッジの C++ ファイルをベースライン `4390c78` に戻し、同じ現在の runtime にリンクしました。それぞれ 128 回の実作成/破棄を試みましたが、指定された旧オブジェクトアドレスは再利用されませんでした。未再現として記録し、障害の再現や修正の比較成功とは扱いません。製品版 C++ のアドレス強制再利用 harness は元の調査で旧 lookup/destroy が新オブジェクトを誤って操作する問題を再現しており、この決定的回帰と実行結果は相互に補完します。

初回の検証プログラムは `close()` 後に再度 `open()` を呼び `player is closed` を受け取りました。`close()` は終端操作です。実際に破棄・再作成するようホストを変更した後、上記フローが完了しました。この検証プログラムの誤りのために製品コードは変更していません。

## 再現資料

`evidence/ohos-*-review.jsonl` はアプリが filesDir に書き hdc で取り出した生記録です。`ohos-live-summary.json` は全カウントを保存し、`scripts/summarize-ohos-live.py` は成功マーカー、再生消費、一時停止状態、旧 ID の拒否、エラーカウントを検証します。

`scripts/ohos-live/` は ArkTS ホスト、ベースライン比較ホスト、ネイティブ型宣言、setup スクリプトを保存します。`scripts/ohos-*-build.sh` と対応する CMake ディレクトリに runtime / ブリッジの実際のビルドコマンドがあります。スクリプトは Docker の `/work`（分離 worktree）、`/review-logs`（ローカル証拠ディレクトリ）、`/target`（永続成果物）をマウントします。実ビルド前に nasm をインストールし、公式 Rust toolchain の OHOS std を使用してください。

Windows では公式 devecocli でインスタンスを作成・起動し HAP をビルドした後、SDK の hdc install と aa start を使用しました。フィクスチャと `.so` を含む完全な一時プロジェクトは `C:\Users\rgliang\AppData\Local\Temp\erika-pr-review-20261005\ohos-live-app` にあります。SDK、システムイメージ、ランタイムバイナリ、アカウント資料はリポジトリにコミットしません。
