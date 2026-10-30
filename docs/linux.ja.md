# Linux ネイティブ組み込み（実験的）

> [中文](linux.zh.md) · [English](linux.md) · [日本語](linux.ja.md)

Linux では、システムの FFmpeg 8.x、liberika のパッチ適用済み libass、wgpu、PulseAudio を使用して Rust プレーヤー、C ABI、Flutter をサポートします。PipeWire デスクトップには `pipewire-pulse` が必要です。WSL2 では WSLg の表示・音声サービスを使用します。

動画は X11 / Wayland のウィンドウに描画できます。Flutter では RGBA SDR テクスチャか Wayland ネイティブ動画レイヤーを使用できます。NVIDIA は NVDEC、Intel/AMD は VA-API を使用し、FFmpeg のソフトウェアデコードも利用できます。Linux は現在ソースからビルドします。

HDR サーフェスのネゴシエーションと VA-API の直接インポートは実験的です。[WSL D3D12 GPU コピー](wsl-gpu-copy.md) もデコードフレームの転送方法の一つです。デバイス、性能、検証結果は [Linux 検証記録](investigations/linux-validation.en.md) にあります。

## Ubuntu 26.04 でのビルド

Rust 1.92 以降と FFmpeg **8.x** の開発ライブラリが必要です。Ubuntu 22.04 / 24.04 の標準 FFmpeg パッケージは、このビルドには古すぎます。

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config clang libclang-dev rustc cargo rustfmt \
  curl ca-certificates meson ninja-build patch xz-utils python3 \
  libavdevice-dev libavfilter-dev libavformat-dev libavcodec-dev \
  libswresample-dev libswscale-dev libavutil-dev \
  libfreetype-dev libharfbuzz-dev libfribidi-dev libfontconfig-dev \
  libpulse-dev libx11-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-0 \
  libxcursor1 libxi6 libxrandr2 \
  libvulkan-dev mesa-vulkan-drivers libegl1 libgl1-mesa-dri \
  fonts-dejavu-core pulseaudio-utils

bash scripts/build_linux_libass.sh
cargo build --locked -p linux_native_demo -p erika_capi
cargo run --locked -p linux_native_demo -- --wayland /path/to/video.mkv
```

X11 は `--x11` で選択し、どちらも指定しなければ自動選択します。メディアを指定しない場合はテストパターンを表示します。スペースで一時停止・再開、矢印キーで 5 秒シーク、Esc で終了します。ビルド成果物は `target/debug` にあり、`--release` を付けると `target/release` に出力されます。WSL では `CARGO_TARGET_DIR="$HOME/.cache/erika-target"` を設定すると Linux ファイルシステム上に成果物を置けます。`pactl info` でサーバーを確認してください。WSLg には既にサーバーがあります。Vulkan/EGL ドライバが必要です。`/dev/dxg` の存在だけではハードウェアアクセラレーションを確認できません（`vulkaninfo --summary` を確認）。

### WSLg の GPU 描画

```bash
bash scripts/run_wsl_gpu.sh --wayland /path/to/video.mkv
# X11 も利用可能:
bash scripts/run_wsl_gpu.sh --x11 /path/to/video.mkv
```

ランチャーは `GALLIUM_DRIVER=d3d12`、`WGPU_BACKEND=gl`、`ERIKA_REQUIRE_HARDWARE_GPU=1` を Erika にのみ設定します。必要に応じて `MESA_D3D12_DEFAULT_ADAPTER_NAME=NVIDIA`（または AMD / Intel）で GPU を選択します。WSL は `/dev/dxg` 経由で Windows GPU ドライバを使うため、Linux の NVIDIA カーネルドライバは不要です。経路は wgpu → EGL/OpenGL → Mesa D3D12 → Windows GPU ドライバです。実際のウィンドウデバイスは `displayBound: true` の `adapter_selected` ログで確認します。検証機では `D3D12 (NVIDIA GeForce RTX 5070)`、`backend: Gl` と報告されました。厳格モードは CPU / ソフトウェアアダプタを拒否します。ネイティブ Linux の Vulkan（`WGPU_BACKEND=vulkan`）も利用可能です。

EGL は実際のホストディスプレイで初期化され、アダプタ選択時にサーフェスとの互換性を確認します。detach 成功時にはそのディスプレイの EGL オブジェクトを解放します。一時停止画像を detach / 再 attach 後も維持するため、CPU フレームを 1 枚保持します。WSL の OpenGL パスでは NVDEC / VA-API フレームを FFmpeg でダウンロードし、wgpu にアップロードします。ハードウェアデコードと CPU 転送の組み合わせで、統計では別々に報告されます。

EGL の終了後もスレッドローカルのクリーンアップコールバックを有効に保つため、liberika はプロセス終了まで WSL の D3D ランタイムを保持します。音声は他のリングバッファ方式と同様、コールバックの消費クロックを使用します。WSLg/RDP のデバイス遅延は排出・速度変更用に別途報告されます。この遅延をクロックから差し引くと、制限された音声プリフェッチウィンドウが枯渇する可能性があります。最終デバイスの遅延は WSLg/RDP シンクに依存します。

WSLg で一時停止すると、メディアリングを消費せず無音でリモート音声転送を維持します。再開時は RDP シンクの uncork を待たずにメディア消費を再開します。通常の Linux PulseAudio / PipeWire セッションは一時停止時に cork します。PulseAudio の cork / flush 操作は非同期で送信されます。

`bash scripts/check_wsl_gpu.sh` で GPU 読み戻し・ピクセルテスト、実音声、X11 / Wayland 再生、C ABI ライフサイクルを検証します。単に描画を発行するだけでなく、8 秒の素材の少なくとも 7.9 秒まで 10 秒以内に到達する必要があります。RTX 5070 では読み戻しとスレッド終了を含む wgpu テスト 27 件すべてに合格しました。

## 組み込み

Linux は既定で CUDA/NVDEC、次に VA-API を試します。`ERIKA_HWDEC=cuda`、`vaapi`、`software` でバックエンドを選択できます。`ERIKA_CUDA_DEVICE=0` で CUDA ordinal、`ERIKA_VAAPI_DEVICE=/dev/dri/renderD128` で render node を指定します。NVIDIA にはユーザー空間の CUDA/NVDEC ライブラリ、Intel には media-driver/iHD（古いカードは i965）、AMD には Mesa radeonsi VA-API が必要です。対応コーデックはカードとドライバに依存します。WSL の Intel/AMD は `/dev/dri` ノードの有無ではなく Mesa D3D12 VA ドライバにも依存します。この NVIDIA WSL 検証機には `/dev/dri` がありません。

`ERIKA_REQUIRE_HARDWARE_DECODE=1` はソフトウェアへのフォールバックを拒否します。これは描画アダプタを確認する `ERIKA_REQUIRE_HARDWARE_GPU=1` とは別です。厳格モードでは未対応動画は明示的に失敗し、それ以外では切り替えがログに記録されます。

Flutter では `erika_capi --release` をビルドし、`liberika_capi.so` があるフォルダを `ERIKA_LIBRARY_DIR` に指定し、Linux Flutter SDK と `packages/erika_flutter` への path dependency を使用します。`ErikaVideoView` と `ErikaTextureVideoView` は GTK の SDR ピクセルバッファテクスチャを使います。Wayland では透明な Flutter 動画領域と `ErikaWindowOverlayVideoView` を使い、Flutter の直下に動画、字幕、弾幕、HUD を直接表示できます。ネイティブパスはフレームごとの RGBA 読み戻しを避けますが、不透明な `srcOver` のみをサポートし、Flutter のクリッピング・カラーフィルタは使えません。X11 はテクスチャパスを使用します。MPRIS は未実装です。同じビルドのヘッダーとライブラリを同梱してください。明示的なスクリーンショットは引き続き隙間のない RGBA バイト列を返します。プラグインは `EGL_BAD_ACCESS` を避けるため、ネイティブ呼び出しの前後で GTK の EGL/GLX コンテキストを保存・解除・復元します。`ERIKA_DEBUG_HUD=1` で診断 HUD を表示できます。インストール済みプラグインは `$ORIGIN` により同階層の共有ライブラリを解決します。

### GPU フレームインポートと HDR

実験的な Vulkan ブリッジは FFmpeg の `AV_HWFRAME_MAP_DIRECT` で VA-API フレームをマップし、インポートした NV12/P010 VkImage を直接サンプリングします。デコーダーとシェーダーの間で転送先プレーンの確保や CPU / GPU ピクセルコピーを行いません。対応するデバイス・フォーマット、外部メモリ / セマフォ拡張、タイムラインセマフォ、synchronization2 が必要です。Intel/AMD 実機でのインポートは未検証です。FFmpeg 8 の CUDA/Vulkan 転送は、インポート失敗時のクリーンアップでクラッシュしうるため拒否されます（[上流の修正](https://github.com/FFmpeg/FFmpeg/commit/c29d710cd5d0f80bbb56f7ec1f35d5fb7ed44d05)）。FFmpeg 9 には修正が含まれますが、この組み込みを使った再ビルドは未検証です。通常の NVDEC デコードは CPU プレーン転送で引き続き利用できます。

`ERIKA_REQUIRE_GPU_FRAMES=1` はデコードプレーンの CPU アップロードを拒否します。`ERIKA_REQUIRE_ZERO_COPY=1` は直接 VA-API パスのみを許可し、ソフトウェアフレーム、インポート失敗、WSL D3D12 コピー、CUDA GPU コピーを拒否します。ゼロコピーのカウンタは直接インポート成功後にのみ増加します。Flutter の RGBA 読み戻しも避けるには Wayland ネイティブ表示を使用します。明示的なスクリーンショットは引き続き読み戻します。

各インポート画像は wgpu が両方の画像エイリアスの使用を完了するまでデコーダー参照を保持します。サンプリング前にタイムライン待機と所有権 / レイアウト遷移を行い、解放時は再利用前に画像を外部所有者へ返します。同梱 Vulkan HAL は初回使用時に外部画像の初期化済みレイアウトを保持します。現在のフレーム置換は未完了の GPU 処理を待機します。これは正確性優先の実装であり、スループットや遅延の受け入れ結果ではありません。

`bash scripts/check_linux_zero_copy.sh` はテスト専用の FFmpeg Vulkan フィクスチャを有効にします。画像ハンドルの完全一致、NV12/P010 ピクセルの CPU アップロード参照画像との比較、繰り返し再描画 / スクリーンショット、セマフォの進行、解放後の生成元ピクセル不変、GPU 処理送信後の終了を検証します。独立した Mesa Dozen を通じた RTX 5070 で合格しました。このフィクスチャは VA-API デコーダーではなく、Intel/AMD 実機でのデコードから表示までのゼロコピーは未検証です。NVIDIA NVDEC の直接ゼロコピーは未実装で、CUDA 転送は別のコピーパスです。

`ERIKA_LINUX_HDR=auto` で HDR / SDR ソースの変化に追従します（`on` は拡張リニア出力を要求、`off` は SDR を選択）。HDR には `R16G16B16A16_SFLOAT + EXTENDED_SRGB_LINEAR_EXT` を通知する Vulkan WSI のみを受け入れます。10-bit SDR フォーマットだけでは不十分です。コンポジタが HDR 動画サブサーフェスと SDR Flutter UI を合成し、最終的なディスプレイ変換を制御します。`ERIKA_REQUIRE_HDR=1` は SDR 専用サーフェスまたは RGBA8 Flutter テクスチャパスで HDR ソースを拒否します。既定の scRGB ヘッドルーム 12.5 はコンテンツ目標（1000/80 nits）であり、ディスプレイ能力の測定値ではありません。実測がない場合 `activeHeadroomKnown` は false のままです。この実装では HDR10+ と Dolby Vision のパススルーは未対応です。

検証した WSL 環境では、独立した Mesa Dozen 26.0.8 が RTX 5070 を Vulkan デバイスとして認識しました。ローカルのフォーマット特性実験は CUDA 外部メモリインポートまで到達しましたが、`CUDA_ERROR_NOT_SUPPORTED` が返されました。WSLg は HDR の色管理 / WSI 機能も公開しませんでした。これは特定の相互運用 / 表示の制限であり、OpenGL と Vulkan のハードウェア描画はどちらも動作します。実験的 Mesa ビルドは必須依存関係ではなく、liberika のランチャーもインストールしません。任意の WSL GPU コピーブリッジは CUDA 外部メモリインポートの代わりに Mesa D3D12 VA-API を使います。[ビルド手順、性能結果、表示上の制限](wsl-gpu-copy.md) を参照してください。

Rust 利用者は `wgpu` feature を有効にしてください。[ネイティブデモ](../examples/linux_native_demo/src/main.rs) を参照。C 利用者は [`erika.h`](../crates/erika_capi/include/erika.h) と共有ライブラリを使用します。[C smoke example](../examples/linux_native_demo/capi_smoke.c) を参照してください。

`erika_presenter_attach_wgpu_surface` では、`XlibWindow` に X11 の `Window` ID と `Display*`、`WaylandSurface` に `wl_surface*` と `wl_display*` を渡します。ポインタは `uintptr_t` を経由して `uint64_t` にキャストします。detach または presenter の破棄が完了するまで、両方のホスト所有ハンドルを有効に保ってください。ウィンドウのイベントループスレッドで attach / render / resize / detach を行い、presenter ごとの呼び出しを直列化し、表示フレームごとに `render_tick` を実行します。幅と高さは物理ピクセル、scale は独立した DPI 係数です。`.so` 依存関係は rpath または `LD_LIBRARY_PATH` で解決し、`ldd` で確認します。

Linux はシステムの FFmpeg 共有ライブラリと、静的 PIC の**パッチ適用済み libass 0.17.5**、システムのフォントライブラリを使用します。スクリプトは上流アーカイブの SHA256 を検証し、liberika のメモリフォントの順序付きフォールバックパッチを適用します。標準 libass はこの動作を保持しません。`ERIKA_LIBASS_DIR` に同等のパッチ適用済み prefix を指定できます。`ERIKA_FFMPEG_DIR` は既存の静的 FFmpeg バンドルを選択し、`ERIKA_USE_SYSTEM_LIBS=0` は従来のバンドル検索パスを維持しますが、対応する依存関係をすべて別途準備する必要があります。このガイドはシステムライブラリを使うネイティブ GNU/Linux ビルドを検証したもので、クロスコンパイルや musl は対象外です。ディストリビューションの FFmpeg コーデックとライセンスが適用され、`ERIKA_NATIVE_PROFILE=lgpl` はその構成を変更しません。PulseAudio 切断時はエラーを報告し、自動再接続は未実装です。

## 検証

```bash
bash scripts/check_linux.sh
# ディスプレイセッションが Wayland を提供する場合:
ERIKA_TEST_WAYLAND=1 bash scripts/check_linux.sh
```

ユニットテスト、実音声の一時停止 / 再開、ヘッダーの整合性、再生、および別途リンクした C ABI ライフサイクルテストを実施します。ディスプレイと PulseAudio サービスが必要です。デバイスのバージョンと測定結果は [Linux 検証記録](investigations/linux-validation.en.md) にあります。
