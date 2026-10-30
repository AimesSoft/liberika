# liberika プラットフォーム機能一覧

> [中文](platform_matrix.zh.md) · [English](platform_matrix.md) · [日本語](platform_matrix.ja.md)

以下は 0.4.0 を基準としたプラットフォームです。ブラウザバックエンドは Erika Web に組み込まれ、Release には独立した Web アーカイブがあります。Linux は現在、実験的なソースビルド対象です。

| プラットフォーム | 最低バージョン | デコード / 描画 | 音声 | 配布とビルド |
|---|---|---|---|---|
| macOS | 11 | VideoToolbox / ソフトウェア + Metal | CoreAudio | arm64、x64、universal；Flutter / SwiftPM |
| iOS | 13 | VideoToolbox / ソフトウェア + Metal | AudioQueue | 実機とシミュレータの XCFramework；Flutter / SwiftPM |
| tvOS | 13 | VideoToolbox / ソフトウェア + Metal | AudioQueue | 実機とシミュレータの XCFramework；Flutter / SwiftPM |
| Windows | 10 | D3D11VA / ソフトウェア + D3D11 | WASAPI | x64 / ARM64；Flutter |
| Android | 8 / API 26 | MediaCodec / ソフトウェア + wgpu | AAudio | 4 ABI；Flutter |
| OpenHarmony | API 18 | AVCodec / ソフトウェア + wgpu | OHAudio | arm64；Flutter / ArkTS OHPM |
| Linux（実験的） | FFmpeg 8.x | NVDEC / VA-API / ソフトウェア + wgpu | PulseAudio / pipewire-pulse | Rust / C ABI / Flutter をソースからビルド。ビルド済みパッケージなし |
| ブラウザ | HTTPS / localhost、WebCodecs 動画・音声、OffscreenCanvas、WebGL2、Web Audio | WebCodecs + WebGL2 / Canvas | Web Audio | Release アーカイブまたは JS / WASM ソースビルド。npm 未公開 |

ネイティブプラットフォームは C ABI Presenter を提供し、ブラウザは JavaScript / TypeScript の `Player` を使用します。ハードウェアデコードの可否はコーデック、デバイス、ドライバに依存し、ブラウザのコーデック設定は実行時に対応を確認します。

## ブラウザ再生

FFmpeg WASM がコンテナを分離し、libass が MKV 内の ASS/SSA と添付フォントを使って字幕を描画し、Rust DFM エンジンが弾幕を描画します。字幕と弾幕は再生クロックと WebGL2 コンポジタを共有し、それぞれ 2 倍のスーパーサンプリングを使用できます。外部 ASS/SSA、SRT、WebVTT 字幕に対応します。サーバーは元ファイルを HTTP Range で提供します。

出力は SDR で、音声と動画は WebCodecs がデコードします。HDR/EDR、Dolby Vision、ArtCNN、ビットマップ字幕、内蔵非 ASS 字幕の変換は未移植です。要件、制限、ビルドとデプロイの詳細は [ブラウザガイド](web.md) を参照してください。

## 動画サーフェスと HDR

| プラットフォーム | ネイティブ表示 | Flutter テクスチャと合成 |
|---|---|---|
| Apple | EDR 対応 CAMetalLayer | macOS の IOSurface / Metal テクスチャで Flutter のクリッピングとフィルタを利用可能 |
| Windows | HDR10 対応 HWND / D3D11。透過動画には DirectComposition を利用可能 | 共有 D3D11 SDR テクスチャ。overlay ブレンドはネイティブレイヤーに切り替わる |
| Android | SDR は TextureView。scRGB は SurfaceView、Vulkan、FP16 / data space のネゴシエーションを使用 | TextureView 互換パスは SDR |
| OpenHarmony | OHNativeWindow、wgpu / AVCodec、OHNativeBuffer インポート | Flutter 外部テクスチャ |
| Linux | X11 / Wayland。Wayland ではネイティブ動画レイヤーを利用可能 | RGBA SDR テクスチャは読み戻しが必要。ネイティブレイヤーは透明な Flutter UI の下に配置 |

Android scRGB と Linux HDR には、それぞれのエンコーディングをサポートするサーフェスが必要です。`erika_presenter_get_output_status` で現在の出力、ヘッドルーム、フォールバック理由を確認できます。

Linux VA-API → Vulkan の直接インポートと WSL GPU コピーパスは実験的です。テクスチャ合成、ハードウェアデコード、デコードフレームのゼロコピーは別のパスです。選択方法は [Linux ガイド](linux.ja.md) を参照してください。

## CI と検証

ネイティブビルドはビルド済み配布のある 6 プラットフォームを対象とします。Linux native workflow は Xvfb、Mesa、PulseAudio を使ってソースから再生を検証します。Flutter consumer workflow はビルド済み配布のある 6 プラットフォームを確認します。HDR 出力とプラットフォームのライフサイクルに関する実機検証は [検証記録](investigations/README.md) にあります。

Web workflow は完全な WASM パッケージをビルドし、独立プレーヤーの回帰テストを実行します。Erika Web は macOS の Chrome 155.0.8059.85 で H.264/AAC/ASS・添付フォント付き MKV、VP9/Opus WebM、2,000 件の弾幕、リアルタイム設定、チャプター、スクリーンショット、プレーヤーのライフサイクルを検証しました。Safari / Firefox は未検証です。
