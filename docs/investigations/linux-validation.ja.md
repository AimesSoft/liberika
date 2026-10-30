# Linux 検証記録

[English](linux-validation.en.md) の翻訳です。[中文](linux-validation.zh.md) も参照できます。

2026-09-29、Ubuntu 26.04 / WSLg、x86_64。元のバージョン、テスト数、デバイスの詳細を以下に記します。

## 検証

```bash
# X11 と PulseAudio 互換サーバーが必要です。
bash scripts/check_linux.sh
# コンポジターが利用できる場合は Wayland も検証します。
ERIKA_TEST_WAYLAND=1 bash scripts/check_linux.sh
```

このスクリプトはユニットテスト、実音声の pause / resume / reconfigure テスト、ヘッダーの整合性、再生スモークテスト、さらに pause、seek、resize、破棄を検証する別リンクの C 実行ファイルを実行します。`Linux native` ワークフローは Ubuntu 26.04 コンテナーで Xvfb、Mesa ソフトウェアレンダリング、PulseAudio null sink を使用します。リモート CI の結果は提出後に確認が必要です。ローカル検証環境は x86_64 Ubuntu 26.04 on WSLg、Rust 1.93.1、FFmpeg 8.0.1、llvmpipe、Mesa D3D12 経由の RTX 5070 です。X11、Wayland、C ABI の pause / display 再生成 / seek / resize 経路、CPU フォールバックの拒否はローカルで通過しました。ARM64 と独立したデスクトップのテストは未実施です。詳細は[中国語ガイド](../linux.zh.md)を参照してください。

2026-09-29 のローカル受け入れ記録（上流基準 `97bbb84`）：

| チェック | 結果 |
|---|---|
| `erika` コアユニットテスト | 657 件通過、音声統合テスト 1 件は既定で無視 |
| `erika_capi` ユニットテスト | 41 件通過 |
| 実 PulseAudio 統合テストを個別に有効化 | 1 件通過 |
| C API / package ヘッダーの整合性 | 5 件通過、3 つのヘッダーの SHA256 は一致 |
| X11 / Wayland の映像・音声スモーク | 両方通過、描画・音声エラーはいずれも 0 |
| `.so` にリンクした独立 C プログラムで pause / seek / resize / 破棄 | 通過、描画・音声エラーはいずれも 0 |
| フォーマット、シェル構文、diff の空白、動的依存の解決 | 通過、動的ライブラリの欠落なし |
| RTX 5070 の X11 / Wayland 映像・音声スモーク | 両方通過、実アダプターは D3D12 RTX 5070、エラーはすべて 0 |
| RTX 5070 の C ABI pause、display を閉じた後の再 attach、seek、resize | 通過、一時停止画像を保持し、音声も正常に再開 |
| GPU strict mode で llvmpipe を強制 | 期待どおり起動を拒否 |
| RTX 5070 の GPU 画像読み戻し / wgpu テスト | 27 件通過、ピクセル値の比較とスレッド終了を含む |
| RTX 5070 の NVDEC H.264 / HEVC Main10 / AV1 | strict ハードウェアデコードで全件通過、エラーはすべて 0 |
| 無効な VA-API デバイス + strict ハードウェアデコード | 明示的に失敗、ソフトウェアへのフォールバックなし |
| ネイティブ Flutter texture C API の長時間 pause（12.5 秒） | ハードウェア 200 フレーム、ソフトウェア 0 フレーム、描画 / 音声エラー 0、再開後も音声クロックが進行 |
| 最終の presenter 集中ユニットテスト | 24 件通過 |
| NipaPlay 1.10.12 Linux release の実 UI | 再生、数分 pause 後の再開、seek、全画面の開始 / 終了、中国語字幕、進行弾幕が通過 |
| NipaPlay の生 RGBA スクリーンショットとサムネイル保存 | 427×240、409920 バイト、保存成功 |
| インストールパッケージの動的リンクとバージョン | インストール先から liberika を解決、SHA256 は release ビルドと一致 |

インストール済み NipaPlay 1.10.12 でも、中国語 SRT 表示、進行弾幕、RGBA サムネイルキャプチャ、seek、全画面切り替え、数分間 pause 後の再開が通過しました。ネイティブテクスチャー C API テストでは 12.5 秒の pause 後、ハードウェアフレーム 200、ソフトウェアフレーム 0、音声・描画エラー 0 でした。最終の presenter 集中テストは 24 件通過。NVDEC の H.264、HEVC Main10、AV1 フィクスチャーも通過し、無効な VA-API デバイスは strict mode で失敗します。これらは物理 Intel / AMD またはネイティブデスクトップの検証を意味しません。

ネイティブ Wayland 動画レイヤーの更新後、NipaPlay 1.11.9 / Flutter 3.47.0-0.3.pre で Rust と Flutter の release ビルドも成功しました。Weston のスクリーンショットで、動画上の透明な Flutter コントロール、中国語 SRT、進行弾幕を確認しました。NipaPlay で pause、全画面の開始・終了、サムネイルキャプチャが動作しました。別のネイティブビュープローブでは OpenGL と実験的な Dozen Vulkan の両方で pause、resize、detach / reattach、seek を完了し、一時停止中の画像を維持しました。更新後のチェックは wgpu テスト 29 件（HDR ネゴシエーション 2 件を含む）、API / ヘッダーテスト 5 件、C/C++ の警告をエラー扱いにしたビルド、strict CPU フレーム拒否 2 ケース、C ABI ライフサイクル再生（映像 316 フレーム、描画 / 音声エラー 0）を通過しました。NipaPlay 全体のセッションではリモート音声 underflow カウンターが非ゼロでした。これらのチェックはネイティブデスクトップの滑らかな音声、物理 HDR 出力、デコーダーのゼロコピーを証明しません。

## 元の導入ガイドの環境メモ

Linux は Rust エンジンと `liberika_capi.so` のソースビルドに対応し、NVDEC / VA-API ハードウェアデコード、FFmpeg ソフトウェアデコード、X11 / Wayland の wgpu SDR 表示、Linux Flutter テクスチャープラグイン、字幕 / 弾幕、PulseAudio 出力（PipeWire-Pulse と WSLg を含む）を提供します。Wayland では透明な Flutter UI の下にネイティブ動画 subsurface もあります。HDR surface negotiation は実装済みですが、物理 HDR 出力と直接ゼロコピーはハードウェア検証に通過していません。Linux のビルド済みリリースはまだありません。

X11 は現在 screen 0 を使用します。ネイティブ ARM64 のビルド経路は準備済みですが、ローカルの受け入れ検証は x86_64 のみです。RTX 5070 上の WSLg ハードウェア描画は Mesa D3D12 / OpenGL で検証し、NVDEC の H.264、HEVC 10-bit、AV1 デコードも実機再生を通過しました。Intel / AMD VA-API は実装済みですが、これらの物理カードでは未検証です。オプトインの [WSL D3D12 GPU コピー経路](../wsl-gpu-copy.ja.md)は、この RTX 5070 でデコード済みフレームの CPU 転送を排除し、4K60 Main10 再生を計測しました。GPU 内にはコピーが残り、エンドツーエンドのゼロコピーではありません。Dozen の Linux 表示では引き続きソフトウェア WSI を使用します。リンク先にビルド手順と受け入れ範囲の詳細があります。
