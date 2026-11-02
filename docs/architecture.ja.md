# liberika Architecture

[中文](architecture.zh.md) | [English](architecture.md) | [日本語](architecture.ja.md)

liberika は埋め込み可能な Rust メディア再生ライブラリです。ホストアプリは Rust API、C ABI (`erika_capi`)、または Flutter バインディング (`erika_flutter`) から呼び出せます。動画フレーム、字幕、弾幕はすべてエンジン内部に留まり、レンダラー内で合成され、native surface は直接表示し、Flutter texture は host で合成します。

ブラウザーは JavaScript / TypeScript `Player` から組み込みます。
[Web バックエンド（英語）](web.md) は Rust 弾幕とパッチ済み libass を WASM で再利用し、
FFmpeg がコンテナーと MKV 添付フォントを読み取ります。WebCodecs、Web Audio、
WebGL2 がデコード、再生クロック、最終合成を担当し、Erika Web に組み込み済みです。
以下の概要はネイティブランタイムを説明します。Worker の構成とライフサイクルは Web ガイドを参照してください。

## システム概要

```text
Rust Player Core
  source abstraction ─── file + HTTP range
  FFmpeg wrappers ────── custom AVIO, probe, demux, decode, seek, audio resample
  playback engine ────── video/audio tick, clock, frame scheduler
  video decode ───────── VideoToolbox, D3D11VA, MediaCodec, AVCodec, NVDEC, VA-API, software fallback
  audio output ───────── CoreAudio, AudioQueue, WASAPI, AAudio, OHAudio, PulseAudio, ring buffer
  overlay timeline ───── subtitle + danmaku composition
  renderer core ──────── color state, render graph, tone map, scaler policy
  Metal renderer ─────── zero-copy NV12/P010, HDR/EDR, subtitle/danmaku pass
  D3D11 renderer ─────── zero-copy D3D11VA, HDR10, subtitle/danmaku pass (Windows)
  wgpu renderer ──────── cross-platform video, overlays, capture, Android scRGB, OHOS Vulkan
  presenter runtime ──── ties player + renderer + audio + overlays
  C ABI ──────────────── versioned public header、2 つの handle ファミリー
  Flutter plugin ─────── macOS + iOS + tvOS + Windows + Android + OpenHarmony + Linux embedding
```

## HTTP source と表示時刻

HTTP Range source は byte budget で先読みと巻き戻し cache を管理します。`http_read_ahead_bytes` の既定は 2 MiB（`ERIKA_HTTP_READAHEAD_BYTES` で指定可能）、`http_back_buffer_bytes` は 16 MiB です。C の `ErikaOpenOptions` と Dart `open()` で request ごとに指定できます。stop、再 open、close は旧 session の network task を cancel して resource を解放します。

iOS Flutter bridge は `CADisplayLink.targetTimestamp` と最新 tick だけを保持する mailbox を使い、render queue の蓄積を避けます。`render_tick_with_timing` は映像・字幕・弾幕を同じ表示目標で sample し、audio master clock の進行を維持します。[C ABI reference](capi_reference.ja.md)。

## ネイティブ依存関係

`xtask` は固定の upstream からネイティブ依存関係をダウンロード・ビルド・インストールし、`third_party/` に配置します。既定 profile は `lgpl` です。

| 依存関係 | バージョン | 目的 |
|----------|-----------|------|
| FFmpeg | 8.1.2 | Demux、decode、audio resample、プラットフォーム HW decode |
| dav1d | 1.5.1 | 全ターゲットの AV1 software decode（8-bit / high bit depth） |
| libass | 0.17.5 | ASS subtitle 描画 |
| FreeType | 2.14.3 | フォントラスタライズ（libass 依存） |
| HarfBuzz | 14.2.1 | テキストシェーピング（libass 依存） |
| FriBidi | 1.0.16 | 双方向テキスト処理（libass 依存） |

リリース対象は依存を static link します。Linux は system FFmpeg、PulseAudio と patch 済み libass を使います。libass とその依存関係は既定で有効です（`features = ["libass"]`）。

```sh
cargo run -p xtask -- deps build --all --profile lgpl
cargo run -p xtask -- deps status
```

## FFmpeg 統合

`erika_ffmpeg_sys` は build 時に bindgen で低レベルバインディングを生成します。`erika::ffmpeg` は安全な Rust ラッパーを提供します。

- **Demuxer**: `AVFormatContext` を保持し、`MediaSource` 由来の Rust-backed custom `AVIOContext` を使うこともできます。stream selection、reference-counted packets、timestamp-based seek をサポートします。
- **Decoder**: software に加えて VideoToolbox、D3D11VA、MediaCodec、OpenHarmony AVCodec の hardware backend を持ちます。hardware frames は BT.2020/PQ metadata を保持し、platform native handle（Apple では `CVPixelBufferRef`）を通じて renderer に zero-copy で渡せます。
- **AudioResampler**: `libswresample` を包み、interleaved f32 PCM（既定 48 kHz stereo）へ変換します。
- **SubtitleDecoder**: 埋め込みテキスト字幕と bitmap 字幕ストリームをデコードします。

## 再生エンジン

`PlaybackSession` は media を開き、track を選び、decode backend を設定し、video frame と PCM audio block を生成します。

Decoder availability は session invariant です。video track が選択されている場合、play / seek / video-frame pump の各入口には active video decoder が必要です。MediaCodec seek reopen や Surface→ByteBuffer/software fallback のような破壊的 transition では、先に decoder-unavailable reason を記録します。最終 software decoder の open まで失敗した場合、各入口はその明示的 error を返して media の reopen を要求し、audio-only の偽 `Playing` state には入りません。

`VideoPlaybackEngine` は clocked playback を追加します。

- Play / pause / stop / seek / playback rate control / EOF detection。
- `PlaybackClock`: audio-master clock discipline を持つ media-time anchor。
- `VideoFrameScheduler`: decoded video frame の present / wait / drop を決定します。
- `DisplaySyncState`: residual frame-duration error を持ち回る vsync quantizer です。

## 音声出力

時刻フィードバックには `Player::capture_audio_clock` と
`update_audio_clock_observation` を使います。観測値は player の識別子、再生
generation、command sequence、output epoch、単調な取得時刻を保持します。
worker は現在の再生意図と実行済みコマンドの両方を照合し、取得から 500 ms で
観測値を失効させます。キューと buffering の判断にも同じ検証を適用します。
presenter は出力リセット、再設定、デバイス状態の遷移、速度変更で epoch を
更新します。取得と出力変更は引き続き出力の所有者が直列化します。

engine は最初に基準値を記録し、消費した PCM frame 数と media time の両方が
進んだ場合だけ共有 `PlaybackClock` を補正します。無音の underflow callback や
停止した snapshot は時計を繰り返し巻き戻しません。有効な大きいずれは両方向に
再同期できます。配送遅延の補間は取得時のキュー内 PCM 時間を上限とし、確定済み
の速度を使います。pause や最初の frame 待ちで止めた時計は動かしません。
background 再生と foreground の video 復帰中も、動作中の audio master は有効です。

従来の Rust API `update_audio_clock(snapshot)` は現在の時間軸で即座に取得する
同期フィードバック向けに残ります。保存済み snapshot の取得時の識別情報は
復元できないため、遅延する呼び出しには観測 API を使います。C ABI とキューの
目標量は変更せず、250 ms の速度変更 bridge と遷移中の混在した速度の観測を
抑制する契約を維持します。音声供給はまだ presenter tick 内で行うため、描画が
止まると underflow は発生し得ます。時刻の復旧と音声供給の独立化は別の処理です。

- **macOS**: ring buffer と PTS-tracking clock snapshot を持つ CoreAudio 出力。presenter は snapshot を player worker に返し、audio-master clock discipline を維持します。
- **iOS**: 同じ ring buffer / clock snapshot model を持つ AudioQueue 出力。
- Ring buffer: interleaved f32、容量可変、overflow は oldest drop、volume control 対応。

## 字幕システム

- **Parsing**: SRT、WebVTT、ASS timeline parsing。embedded / external subtitle track を扱い、external track は runtime で追加・削除できます。
- **libass renderer**: static link で既定有効。ASS script を受け取り、`ass_render_frame` を呼び、alpha plane を liberika の overlay system に取り込みます。macOS では CoreText font provider、Windows では DirectWrite を使います。それ以外の target は system font provider 無しで動作するため（vendored libass は fontconfig を無効化）、全 platform で memory font として登録される内蔵の Droid Sans Fallback が libass の既定 family になります。
- **字幕スタイル**: custom な font family と font file、色、metrics（サイズ・縁取り・影・blur・字間・拡大率）、attribute、border style、alignment、margin は fallback として働き、script が指定していない部分と plain-text 字幕の styling を埋めます（metrics には字幕 scale がさらに掛かります）。`override_mask` は選んだ field を libass の selective style override に昇格させ、ASS dialogue が要求する styling も置き換えます。override 時の metrics は再正規化され、script の `PlayResY` に関係なく同じ pixel になります。
- **SubtitleRendererCore**: changed / unchanged frame を追跡し、不要な GPU upload を避ける renderer-facing boundary です。

## 弾幕システム

弾幕サブシステムは NipaPlay DFM+ の layout algorithm を Rust で native 実装しています。完全な設計は `docs/danmaku_architecture.md` を参照してください。

- **入力**: Bilibili XML、JSON、JSON-lines parsing。
- **DanmakuSession**: multi-track 管理、track ごとの enable/disable、track offset、global offset。
- **DFM+ layout core**: prepare / frame-query 分離。prepare は measurement、filtering、duplicate merge、collision avoidance、lane allocation を一括で処理します。frame query は指定 media time の positioned items を返します。
- **Text rasterizer**: fill / outline alpha mask を持つ glyph atlas と、GPU texture reuse 用の version tracking。
- **Render plan**: `DanmakuRenderPlan` は screen rect、atlas tex rect、色、outline、shadow を持つ glyph instances を運びます。Metal と wgpu は atlas から instanced quad を描画します。

## レンダラー

### Metal Renderer（macOS/iOS/tvOS）

Apple platform の主 renderer です。

- `CVMetalTextureCache` 経由で CVPixelBuffer → MTLTexture を zero-copy import。
- YCbCr sampling、transfer decode、gamut mapping（BT.2020→BT.709、Display P3→BT.709）。
- Tone mapping: Mobius、Reinhard、clip with absolute nits。
- SDR output（`BGRA8Unorm`）と Apple EDR output（`RGBA16Float` + EDR headroom）。
- 輝度アップスケーラ（`LumaUpscalerMode`）: ネットワーク系モード（ArtCNN、Anime4K、`Custom`）を Metal compute として decoded Y plane に適用し、render pass と同じ command buffer に encode します。クロマは source resolution のままです。ネットワーク出力は decoded frame ごとにキャッシュされ、同じフレームの vsync tick では compute を省略します。kernel は background thread で compile されるため、準備ができるまで再生は未アップスケールのまま続きます。各エンジンファミリー（汎用ネットワークエンジン、FSR1、MetalFX spatial、VideoToolbox）は [スーパー解像度](#スーパー解像度) を参照してください。
- Subtitle overlay: RGBA plane upload と alpha blending。
- Danmaku: atlas からの instanced glyph quad drawing（shadow → outline → fill）。
- Presentation layout は source aspect ratio を保ちます。

### Direct3D 11 Renderer（Windows）

Windows のネイティブ renderer（`renderer/d3d11.rs`）：

- ゼロコピー D3D11VA デコードテクスチャ相互運用：デコードされた `ID3D11Texture2D` surface を render device に共有し、CPU を経由しません。
- YCbCr サンプリングと色空間変換（HLSL shader）、Metal と同じ pipeline model。
- ゼロコピー D3D11VA luma plane 上で、生成した compute kernel によるニューラル / プラットフォームのスーパー解像度を実行します。計測によるバックエンド選択、RTX/Intel VSR、opt-in の Vulkan / D3D12 アクセラレータ sidecar を含みます（[スーパー解像度](#スーパー解像度) 参照）。アップスケール後の輝度は通常の 2x `R16_FLOAT` テクスチャで、decoded frame ごとにキャッシュされます。動かせないモードは理由付きで明示的に `Inactive` fallback を報告します。
- HDR10 出力：`R10G10B10A2_UNORM` swapchain + `DXGI_HDR_METADATA_HDR10`、SDR（`BGRA8`）fallback あり。
- 字幕 overlay の alpha-atlas upload と blending、atlas からの instanced danmaku glyph quad 描画。
- `render_tick` で駆動される window-hosted swapchain。

### wgpu Renderer（cross-platform）

移植性向けの第二 backend です。

- `wgpu` dependency と device / surface / pipeline creation。
- NV12/P010 video frame upload と WGSL YCbCr conversion shader。
- Android MediaCodec Surface を AHardwareBuffer/Vulkan で import。native interop が使えない場合は ByteBuffer/CPU upload に明示的に fallback します。
- 共通の `renderer/frame.rs` boundary は geometry/color metadata と、FFmpeg decoded frame または独立した prepared AHardwareBuffer を保持します。MediaCodec Surface の AVFrame は decoder callback context が有効な playback worker 上で解放され、presenter と GPU recovery は decoder-owned AVFrame を保持しません。
- 色空間変換と tone mapping（Metal と同じ pipeline model）。
- 超解像（`renderer/wgpu_sr.rs` と `renderer/sr/wgsl.rs`）: D3D11、Metal と同じ `SrModel` IR から、すべてのネットワークモード（ArtCNN 1-7、Anime4K 16-25、`Custom`）と `Fsr1` を実行します。ネットワークの出力は packed 2x2 depth-to-space quad を持つ source-sized の `Rgba16Float` texture で、FSR1 は表示サイズの luma texture を書き出します。native luma と Android の変換済み nonlinear RGB texture の両方を受け付け、chroma は `rgb + (Y_sr - Y)` で保持します。`PlatformVideo` と `MetalFxSpatial` は `Inactive` です。GLES 3.0 は compute を試さず、構造化された `native_luma_sampling` fallback とともに `Inactive` を報告します。
- subtitle/danmaku composite、frame capture、headless testing 用 offscreen target。display surface が extended-linear の場合も screenshot は常に SDR RGBA8 target へ offscreen render し、未マップの scRGB 値を SDR pixel として返しません。
- surface handle model は macOS NSView、iOS UIView、Windows HWND、X11/Wayland、Android native window、OpenHarmony `OHNativeWindow` をカバーします。
- OpenHarmony では AVCodec の Surface 出力を `OHNativeBuffer` 由来の Vulkan external image として import し、Vulkan YCbCr sampler で YUV 変換を GPU 上で行うため、decode したフレームは CPU コピーなしで wgpu compositor に到達します。必要な Vulkan extension がない端末では software decode と CPU upload に fallback し、その経緯は diagnostics event から確認できます。
- Android は bounded Vulkan/GLES backend recovery と import/capability/quality/device-failure diagnostics を備えます。high-headroom output は FP16 **extended-linear scRGB** であり HDR10/PQ ではありません。renderer は `Rgba16Float`、Vulkan extended-sRGB-linear color space を使い、configure/reconfigure ごとに `ANativeWindow` の `ADATASPACE_SCRGB_LINEAR`（`0x18410000`）を検証します。Android scRGB は BT.709 primaries、`1.0 = 80 nit` で、PQ や HDR10 static metadata は出力しません。
- Extended-linear が active になる条件は、`ExtendedLinear` の明示要求、HDR 対応 display/surface、Flutter Hybrid Composition の `SurfaceView`、Vulkan wgpu backend、surface の `Rgba16Float` 対応、`SCRGB_LINEAR` dataspace readback 成功です。どれかが欠けると通常の SDR surface を直ちに選び、安定 ABI の fallback reason `0..8` を記録します。そのため GLES と `TextureView` は SDR path です。
- API 34+ では Android host が `Display.registerHdrSdrRatioChangedListener` で display を監視し、実際の変化を `erika_presenter_set_output_headroom` で liberika に publish します。wgpu は surface を reattach せず、後続 frame の effective content headroom と queryable output status を更新します。ratio が available なら `activeHeadroomKnown` は true で、known flag または ratio が実際に変わった場合だけ `headroomUpdates` が増えます。
- Flutter extended-linear player で `edrHeadroom` を明示しない場合、content ceiling は default 4x、`SurfaceView` の desired headroom は system auto を意味する `0` です。明示値は content ceiling となり、API 35 では per-`SurfaceView` desired headroom にもなります。global Window は変更しません。
- emulator / non-HDR device では明示的な SDR fallback branch を検証済みです。API 35 HDR 実機で `Rgba16Float + SCRGB_LINEAR` の acceptance が完了するまで、active extended-linear path を実機検証済みとは表現しません。

### スーパー解像度

ネットワーク系モードは `crates/erika/src/renderer/sr/` にあるバックエンド非依存のネットワーク IR を共有します。

- `model.rs`: ネットワークグラフ（1 チャンネル輝度入力に対する畳み込み、入力ごとの kernel、CReLU 変換、残差、ReLU/SiLU/PReLU、2x depth-to-space ヘッド）と `.esr` シリアライズ。
- `catalog.rs` はモードを同梱モデルに対応付け、`mpv.rs` は Anime4K / FSRCNNX の mpv hook shader を取り込み（重みのみで shader コードは実行しません）、`artcnn.rs` は従来の ArtCNN blob を変換し、`fsr1.rs` は FSR 1.0 です。
- `fold.rs` は厳密な最適化を行います。兄弟 pass の統合と、線形レイヤーの隣接レイヤーへの代数的な畳み込み（ArtCNN の conv0 を conv1 へ、conv5 を conv6 へ）で、9 種類の境界クラス別重みにより画像の端も厳密に保ちます。ArtCNN の MAC は 26-33% 減ります。
- `reference.rs` は CPU oracle で、すべての GPU backend がこれを基準にテストされます。
- `hlsl.rs` は D3D11 の kernel generator、`wgsl.rs` は wgpu 用です。

Direct3D 11（`renderer/d3d11_sr.rs` と関連ファイル）:

- `d3d11_sr.rs` は executor です。レイヤーごとの kernel、リテラルインデックスで constant buffer に置いた重み、groupshared による入力のステージング、epilogue/head の融合、境界用の ring dispatch 1 回、192 MB 予算内での tiling、background FXC compile とディスクキャッシュ、timestamp query による GPU 時間計測を行います。
- `d3d11_sr_select.rs` はネットワークごとにスカラーと各アクセラレータを計測して最速のものを選び、計測した候補を列挙する `luma_upscaler_backend_selection` 診断イベントを出します。
- `d3d11_vsr.rs` は video processor の vendor extension で NVIDIA RTX Video / Intel VSR を駆動します。
- Sidecar: `d3d11_vk_coopmat.rs` と `vk_coopmat/`（Vulkan cooperative matrix）、`d3d11_d3d12_linalg.rs` と `d3d12_linalg/`（opt-in の D3D12 SM 6.10 preview）。

Metal（`renderer/metal/`）: `upscaler_network.rs` が汎用ネットワークエンジン（M5/A19 は Metal 4 tensor ops、それ以外は `simdgroup_matrix` またはスカラー）で、`upscaler_fsr.rs`、`upscaler_metalfx.rs`、`upscaler_vt.rs` がそれぞれ FSR1、MetalFX spatial、VideoToolbox を担当し、`upscaler.rs` が選択と status を扱います。手書きの ArtCNN kernel は `ERIKA_SR_BACKEND=legacy` で引き続き選択できます。

wgpu（`renderer/wgpu_sr.rs`、`renderer/sr/wgsl.rs`）: `wgsl.rs` が `SrModel` IR から WGSL を生成し（`reference.rs` で検証）、`wgpu_sr.rs` が executor として background compile（`Building` status）、scratch 予算内での tiling、モデルキャッシュを担当します。生成される kernel は、workgroup memory による入力のステージング、bias/residual/activation の epilogue と depth-to-space head の融合、畳み込み済みレイヤーの境界クラス用 perimeter dispatch 1 回、adapter が `SHADER_F16` を持つ場合の fp16、対応を公表する Vulkan adapter（NVIDIA）での 16x16 cooperative matrix を使います。adapter ごとの既定値は最初の数フレームで kernel variant を少数計測して調整されます（プロセス内キャッシュ。`ERIKA_WGPU_SR_AUTOTUNE=0` で無効化）。FSR1 は EASU+RCAS を融合した kernel です。`wgpu_artcnn.rs`（従来の ArtCNN executor）は回帰比較のためだけに残っています。


### Render Pipeline

`renderer::pipeline` は backend が消費する前に、Rust 側で描画判断を記述します。

- `SourceColorState` / `TargetColorState`: primaries、transfer、range。
- `VideoRenderPipeline`: gamut matrix、tone map operator、transfer functions。
- `renderer::output`: native renderer と wgpu が共有する requested mode、active encoding、surface format、dataspace/headroom state、stable fallback diagnostics。
- HDR metadata: mastering display、content light level、nominal peak nits。

## Presenter Runtime

`PresenterRuntime` は Player、MetalRenderer、OverlayTimeline、DanmakuEngine、audio output をつなぎます。host は native surface を提供し、display timer から `render_tick` を呼びます。

- video frame を pump し、overlay（subtitle + danmaku）を更新し、render して present します。
- danmaku plan generation は generation + media_time gate で video frame と同期します。
- seek、track selection、fallback、open/close など decoder transition は quiesce/ACK barrier を使い、frame output 停止、renderer cache/receiver 解放、transition 完了後の resume の順で処理します。playback generation は reopen をまたいで単調増加し、古い generation / MediaCodec route の import feedback は破棄されます。
- playback rate、volume、track selection、subtitle/danmaku configuration を runtime で変更できます。

## C ABI

正本のヘッダーは `crates/erika_capi/include/erika.h` で、各 package のコピーは一致させます。2 つの `erika_presenter_windows_*_iunknown` getter は Windows のみで export されますが、共通ヘッダーには全 platform で宣言されます。

`erika_capi` は 2 つの handle family を提供します。

- **`ErikaHandle`**: player control と event polling。rendering は host 管理です。
- **`ErikaPresenterHandle`**: liberika が full stack を所有します。host は surface を渡して `render_tick` を呼びます。

create/destroy、open/play/pause/stop/seek、track selection、subtitle track add/remove、danmaku track management（add/remove/enable/offset/config）、surface attach/detach/resize、event polling、volume、playback rate、neural luma upscaler switching、upscaler diagnostics、`erika_presenter_get_output_status` が返す 13-field output status snapshot を含みます。

Header: `crates/erika_capi/include/erika.h`

## Flutter Plugin

`packages/erika_flutter` は macOS / iOS / tvOS / Windows / Android / HarmonyOS / Linux の Flutter embedding を提供します。

- **Dart**: `ErikaPlayer`（commands + events）、`ErikaWindowOverlayVideoView`（推奨の window-hosted native surface——Apple では Metal、Windows では D3D11 swapchain）、`ErikaVideoView`（compatibility platform view）。
- **macOS Swift plugin**: `liberika_capi.dylib` を読み込み、`NSWindow` overlay または `NSView`/`CAMetalLayer` platform view surface を作成し、display link から `render_tick` を駆動します。
- **iOS Swift plugin**: `liberika_capi.a` を static link し、`UIWindow` overlay または `UIView`/`CAMetalLayer` platform view surface を作成し、同じ presenter model を使います。
- **tvOS Swift plugin**: `liberika_capi.a` を static link し、Apple TV の `UIView`/`CAMetalLayer` platform view で表示し、同じ presenter model を使います。tvOS 実機と arm64/x86_64 simulator に対応します。
- **Windows C++ plugin**（`ErikaFlutterPluginCApi`）: CMake（`build_erika_runtime.cmake`、cargo target `x86_64-pc-windows-msvc`）で `erika_capi.dll` をビルド・リンクし、window-level D3D11 swapchain を host し、frame scheduler から `render_tick` を駆動します。
- **Android Kotlin/JNI plugin**: Android ABI 向け Rust runtime をビルドし、player ごとに独立した native surface を持ちます。SDR は `TextureView`、extended-linear の要求時は Flutter Hybrid Composition の `SurfaceView` を使います。Activity surface lifecycle、audio focus、noisy-route policy、HDR eligibility/headroom を調整し、active player がある間だけ shared frame scheduler を駆動します。
- **HarmonyOS ArkTS/N-API plugin**: Hvigor/CMake で `aarch64-unknown-linux-ohos` 向けに `liberika_capi.so` をビルドし、Flutter external texture を登録して、その texture の `OHNativeWindow` を presenter に attach します。音声は OHAudio を通じて interleaved f32 PCM で出力します。

embedding model と HDR strategy は `docs/flutter_embedding.md` を参照してください。

## Platform Support

| Platform | Decode | Render | Audio | Status |
|----------|--------|--------|-------|--------|
| macOS 11+ | VideoToolbox | Metal | CoreAudio | Available |
| iOS 13+ | VideoToolbox | Metal | AudioQueue | Available |
| tvOS 13+ (Apple TV) | VideoToolbox | Metal | AudioQueue | Available |
| Windows 10+ | D3D11VA | Direct3D 11 | WASAPI | Available |
| Linux | NVDEC / VA-API / FFmpeg | wgpu X11/Wayland | PulseAudio / PipeWire-Pulse | Experimental |
| Android 8+ | MediaCodec / software | wgpu Vulkan + GLES fallback | AAudio | Available。SDR は検証済み、extended-linear scRGB は API 35 HDR 実機 acceptance 待ち |
| HarmonyOS API 18+ | AVCodec（H.264/HEVC）/ software | wgpu Vulkan、`OHNativeBuffer` zero-copy import | OHAudio | Available。実機で検証済み、CI は OpenHarmony C ABI をビルドするがデバイス側の実行検証はなし |
