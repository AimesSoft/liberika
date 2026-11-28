# WSL D3D12 デコードフレームのインポート（実験的）

> [中文](wsl-gpu-copy.zh.md) · [English](wsl-gpu-copy.md) · [日本語](wsl-gpu-copy.ja.md)

この任意パスは WSL で**デコード済み NV12/P010 フレーム**が CPU メモリを往復する処理を取り除きます。Mesa D3D12 VA-API がデコードし、D3D12 が 2 つのプレーンを再利用可能な GPU テクスチャにコピーし、Dozen がそのテクスチャを wgpu 用に Vulkan へインポートします。P010 は 10-bit のままです。`av_hwframe_transfer_data` と CPU プレーンのアップロードは使いません。

これは **GPU コピー**であり、直接ゼロコピーではありません。Dozen 26.0.8 は非準拠で、Linux では software WSI を使用します（`dzn_wsi.c` の `sw_device = true`）。そのためウィンドウ表示時に描画結果を読み戻す場合があります。WSLg/RDP やコンポジタのコピーは liberika のデコードフレームカウンタの対象外です。このパスは厳密なエンドツーエンドのゼロコピーや実際の HDR 出力を立証するものではありません。

## Ubuntu 26.04 / WSLg x86_64 での再現

まず通常の [Linux ビルド依存関係](linux.ja.md) をインストールし、Mesa / ブリッジ用の依存関係を追加します。

```bash
sudo apt install directx-headers-dev libva-dev libdrm-dev \
  libxcb-dri3-dev libxcb-present-dev libxcb-xfixes0-dev libxcb-sync-dev \
  libxshmfence-dev libxext-dev libxfixes-dev libxxf86vm-dev \
  libwayland-dev wayland-protocols python3-mako python3-yaml \
  bison flex zlib1g-dev libzstd-dev

# Linux ファイルシステム上に展開した上流 Mesa 26.0.8 ソースを使用。
bash scripts/build_wsl_interop_mesa.sh /path/to/mesa-26.0.8
export CARGO_TARGET_DIR="$HOME/.cache/erika-target"
ERIKA_LINUX_D3D12_INTEROP=1 cargo build --locked --release -p erika_capi

# ラッパーは任意のコマンドを受け付ける。Flutter の追加 RGBA 読み戻しを避けるにはネイティブ表示が必要。
ERIKA_LINUX_D3D12_INTEROP=1 bash scripts/run_wsl_gpu_copy.sh \
  cargo run --locked --release -p linux_native_demo -- --wayland /path/to/video.mp4
```

スクリプトは `~/.local/opt/erika-wsl-mesa` にインストールし、システム Mesa を変更しません。任意の第 2 ビルド引数で prefix を変更でき、実行時は `ERIKA_WSL_MESA_PREFIX` に同じ値を設定します。ラッパーは起動するアプリにのみドライバ変数を設定し、ハードウェア描画、ハードウェアデコード、GPU デコードフレームのインポート成功を要求します。デコードフレームのダウンロードへ無言でフォールバックしません。

`ERIKA_REQUIRE_ZERO_COPY=1` はこのコピーパスを意図的に拒否します。別のネイティブ Linux VA-API/DRM 直接インポート実装は変更されません。Intel/AMD 実機での受け入れと、ネイティブ Linux NVIDIA の直接 NVDEC インポートは未完了です。対応コーデックは実際のハードウェアと VA ドライバに依存します。

## 同期と寿命

- Vulkan デバイスの LUID を DXCore アダプタと照合します。NV12/P010 と単一の opaque resource FD を持つ Mesa D3D12 VA サーフェスのみ受け入れます。これらの FD は **DMA-BUF ではありません**。
- VA デコード完了を待ち、GPU 上でプレーンをコピーし、D3D12 fence を待ち、シェーダーによるサンプリング前に Vulkan の所有権を取得します。
- wgpu の完了までインポートテクスチャを保持し、外部所有権を解放して小さなプールに戻します。サイズやフォーマットが変わった場合はプールを交換します。
- 完了状態が不明な送信失敗ではブリッジを再利用不可とし、使用中のメモリを再利用しないよう、参照されたリソースをプロセス終了まで保持します。

Mesa パッチは共有可能な動画リソースと P010 リソースのエクスポートを有効化し、FFmpeg に必要な Dozen format-properties2 を提供し、デコーダーが別の画像を記録中に `vaSyncSurface` がアロケータをリセットすることを防ぎます。これはローカルの実験パッチであり、上流の Mesa リリースではありません。

R16/RG16 UNORM のないアダプタでも、通常の CPU フォールバックは 10-bit 精度を維持します。元のバイト対を RG8/RGBA8 でアップロードし、シェーダーで再構成します（ArtCNN を含む）。このフォールバックは引き続き CPU メモリを介してデコードフレームを転送するため、性能面では GPU インポートの代替ではありません。

## 測定と受け入れ、2026-09-29

RTX 5070、ドライバ 595.79、WSLg、FFmpeg 8.0.1、Mesa 26.0.8。素材は 25 秒、3840×2160 HEVC Main10、60 fps、約 35 Mbps、AAC 48 kHz。C ABI ベンチマークは 1920×1080 で表示し、最大 120 Hz でポーリングします。繰り返しの描画を新規フレームとして数えず、異なるソースフレームのインポートを測定します。

| 16 秒のサンプル | OpenGL/NVDEC CPU 転送の基準 | WSL GPU プレーンコピー |
|---|---:|---:|
| ハードウェアソースフレーム | 960 | 960 |
| ソースフレーム/秒 | 60.011 | 60.027 |
| デコードフレーム CPU 転送 | 960 | 0 |
| Tick P99 | 17.954 ms | 4.149 ms |
| 最大 tick | 25.649 ms | 6.498 ms |
| 16.67 ms を超えた tick | 21 | 0 |
| 測定区間の音声アンダーフローフレーム | 0 | 0 |

旧基準は P010 を 8-bit に変換していましたが、GPU コピーは 10-bit 入力を保持します。これらはローカル測定であり、他のハードウェア、出力解像度、負荷での結果を保証しません。Tick 時間はインポートと描画送信を含み、Windows の表示遅延や物理スキャンアウトは測定しません。

共有ライブラリをビルドした後の再現手順：

```bash
cc -std=c11 -O2 -Wall -Wextra -Werror \
  -Icrates/erika_capi/include examples/linux_native_demo/capi_performance.c \
  -L"$CARGO_TARGET_DIR/release" -lerika_capi -lX11 -o /tmp/erika-performance
LD_LIBRARY_PATH="$CARGO_TARGET_DIR/release" bash scripts/run_wsl_gpu_copy.sh \
  /tmp/erika-performance /path/to/4k60-p010.mp4 5 16

ERIKA_LINUX_D3D12_INTEROP=1 ERIKA_WSL_TEST_VIDEO=/path/to/video-stream-0.mp4 \
  bash scripts/run_wsl_gpu_copy.sh cargo test --locked -p erika --features wgpu \
  --lib linux_wsl_gpu_copy_matches_decoded_pixels -- --ignored --nocapture
```

実際の VA デコードのピクセル比較は、P010 と NV12 それぞれ連続 48 フレームで合格しました。プール再利用、繰り返しの読み戻し、最終フレームの終了処理も含みます。明示的な CPU 参照はテスト内に限定されます。C ABI ライフサイクルテストは一時停止、ディスプレイの detach/reattach、シーク、リサイズ、終了処理に合格しました。シェーダーの変更により現在の SPIR-V 検証で報告された不正な Function ストレージレイアウト修飾も除去され、その後の実再生で Vulkan 検証エラーは報告されませんでした。

NipaPlay 1.11.9 / Flutter 3.47.0-0.3.pre は配置されたブリッジを読み込み、VA-API、GPU 共有フレームのインポート成功、デコードフレームの CPU フォールバックゼロを報告しました。同じ RTX 上のネストした Weston GL セッションでは、スクリーンショットで 4K Main10 動画、進行弾幕、一時停止中のシーク、約 59.9 fps での再生再開、全画面の開始・終了、画像の保持を確認しました。サムネイル取得も成功しました。アプリ全体では音声アンダーフローが発生しており、C ベンチマークの無欠落区間は NipaPlay の音声が常に途切れないことを示すものではありません。
