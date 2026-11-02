# Flutter Embedding

[中文](flutter_embedding.zh.md) | [English](flutter_embedding.md) | [日本語](flutter_embedding.ja.md)

liberika は Flutter の動画レンダラーではありません。Flutter はあくまで任意の host UI です。再生コアが decode、timing、native rendering、字幕、弾幕、音声、HDR presentation を担当します。

## API Families

C ABI の entrypoint family は 2 つあります。

- `ErikaHandle`: control と event API。host が自前の presenter loop を持つ場合や、再生の probe / control だけを行いたい場合に使います。
- `ErikaPresenterHandle`: presenter-owned API。liberika が `Player + renderer + audio output` を所有し、host は native surface と display tick callback だけを提供する場合に使います。

どちらも `crates/erika_capi/include/erika.h` に定義されています。

## Apple Surface Strategies

Apple HDR path は Flutter Texture ではなく native Metal-backed surface を使います。Flutter plugin は macOS / iOS / tvOS に 2 つの native surface strategy を公開し、host が UI に合う composition model を選べるようにしています。

### ErikaVideoView (Platform View)

標準的な Flutter platform view です。macOS では `NSView`/`CAMetalLayer`、iOS / tvOS では `UIView`/`CAMetalLayer` で実装されます。plugin は `erika_flutter/video_view` として登録された native video view を作り、presenter に attach し、display link から描画を駆動します。

simple embedder や診断用途には便利です。macOS では AppKit/Flutter platform view composition の都合で black flicker などの compositor artifact が出ることがあるため、production path としては推奨されません。

### ErikaWindowOverlayVideoView (Window Overlay)

HDR/EDR の推奨 path は、Flutter の platform-view compositor の外側に置かれる window-hosted native overlay です。

1. Dart の `ErikaWindowOverlayVideoView` が widget tree 上の rectangle を確保します。
2. platform plugin が window-level native view を作り、`CAMetalLayer` を Flutter host view の sibling / underlay として配置します。
3. Flutter は該当 widget 領域を transparent に描画し、native video 用の hole を残します。
4. widget は位置を追跡し、surface generation number 付きで geometry update を送るため、dispose 済み widget からの古い hide call が新しく attach された surface に影響しません。
5. attach retry は exponential backoff で window readiness のタイミングを吸収します。

この overlay path は NipaPlay や他の full-player UI に推奨です。video presentation は liberika/Metal が持ち、Flutter は control / layout layer に専念できます。iOS / tvOS では window + sibling `UIView`/`CAMetalLayer`、macOS では host `NSWindow` + sibling `NSView`/`CAMetalLayer` を使います。

touch events は両方の native video strategy を通過するので、Flutter controls を video surface の上や周囲に置けます。

### ErikaTextureVideoView (Flutter Texture)

`ErikaTextureVideoView` は platform view ではなく Flutter の texture registrar 経由で描画します。macOS では plugin が IOSurface-backed の `CVPixelBuffer` pool を確保し、その Metal texture に直接描画（フレームごとの CPU readback なし）して Flutter へ frame を publish するため、`Opacity`、clipping、transform、color filter といった通常の Flutter effect が video に適用されます。OpenHarmony では登録済みの external texture を再利用します。Windows は SDR BGRA8 GPU texture 出力を提供します。それ以外の platform では `ErikaVideoView` に fallback します。

macOS で `blendMode` が `srcOver` 以外の場合、widget は native platform view に切り替え、Core Animation に実際の背景に対する blending を任せます（下記「透明 video と blend mode」を参照）。

## Android Surface Strategies

Android では 2 つの video widget が同じ native-view selector を使います。SDR は実体のある
`TextureView` を使い、検証済みです。wgpu は Vulkan を優先し、bounded GLES fallback も
備えます。
`ErikaOutputMode.extendedLinear` を要求すると、FP16 scRGB を Flutter の texture-layer
compositor に通さないよう、`PlatformViewLink` と Hybrid Composition で `SurfaceView` を
作ります。surface は `Choreographer` で駆動し、lifecycle、resize、audio focus、output
fallback は plugin が管理します。

FP16 extended-linear scRGB は `Rgba16Float` negotiation と
`ADATASPACE_SCRGB_LINEAR` verification まで実装済みですが、active path はまだ実機検証済み
とは claim しません。最終 acceptance には API 35 HDR device が必要です。HDR 非対応 display、
GLES、`TextureView`、FP16 不在、dataspace verification failure では SDR playback を継続し、
query 可能な fallback reason と明示的な log を提供します。

## HarmonyOS Surface Strategies

HarmonyOS では `ErikaVideoView` を使います。ArkTS plugin が Flutter external
texture を登録し、その texture の surface を `OHNativeWindow` として取得して
presenter に attach します。wgpu はその上で Vulkan 描画を行い、window system
integration には `VK_OHOS_surface` を使います。

video decode の既定は HarmonyOS AVCodec（H.264 / HEVC）です。AVCodec は Surface に
直接 decode し、その `OHNativeBuffer` を Vulkan external image として import して
Vulkan YCbCr sampler で解決するため、decode したフレームは CPU コピーなしで
compositor に届きます。字幕・danmaku・overlay は他 platform と同じ wgpu pass で
合成されます。

必要な Vulkan extension が無い端末は FFmpeg software decode と CPU upload に
fallback します。fallback は再生を失敗させず、`VideoDecoderChanged` event と
presenter diagnostics から報告されます。HarmonyOS path は実機で検証済みですが、
CI は OpenHarmony C ABI をビルドしますが、デバイス側の実行検証はありません。

## 透明 video と blend mode

liberika は Flutter の layout / composition を保ったまま alpha を持つ video asset を表示できます。透明は player ごとに要求します。

```dart
final player = ErikaPlayer(
  videoAlphaMode: ErikaVideoAlphaMode.packedAlphaRight,
);
```

`ErikaVideoAlphaMode.packedAlphaRight` は左右分割の encoded frame を前提とします。左半分が color、右半分が grayscale alpha mask です。GPU の presentation shader（Metal / wgpu / D3D11）が premultiplied alpha の frame を再構築し、video は encoded width の半分のサイズで表示されます。この mode を要求しない player は frame を完全な不透明として扱うため、既存 content には影響しません。

blending と opacity は video widget に指定します。

```dart
ErikaTextureVideoView(
  player: player,
  blendMode: BlendMode.overlay,
  opacity: 0.8,
)
```

対応状況は platform によって異なります。

| Platform と path | `blendMode` | `opacity` |
|------------------|-------------|-----------|
| macOS、`ErikaTextureVideoView`（Flutter texture） | `srcOver` のみ。それ以外は native layer に切替 | Flutter `Opacity` |
| macOS、native layer（`blendMode != srcOver`） | `overlay` 対応。他の値は無視 | native `CALayer` opacity |
| Windows window overlay（DirectComposition） | `srcOver` / `overlay`。他の値は plugin error | `IDCompositionEffectGroup` opacity |
| Android / iOS / OpenHarmony | creation parameter としては受け取るが未使用 | texture path では Flutter 側で適用 |

overlay blending は backdrop-aware です。macOS の native layer は window 内で video の背後にある content に対して合成し、Windows の blend effect は同じ HWND を sampling するため、背後の Flutter や game の画面は見えたまま維持されます。Windows の透明 overlay video は Flutter HWND に bind した SDR BGRA8 premultiplied composition swap chain を使い、HDR source は liberika 内部でその合成空間へ tone map されます。不透明な Windows overlay video は従来どおり専用 child HWND path を使います。decoder や output device が swap chain を再構築した場合は composition content が自動で再 bind されます。

## iOS Build Path

iOS plugin は CocoaPod script phase 経由で liberika C ABI static library を app にリンクします。既定では一致する prebuilt アーカイブをダウンロードし、`ERIKA_FORCE_SOURCE_BUILD=1`（`ERIKA_REPO_ROOT` と併用）で初めて対象 iOS architecture 向けに Rust の `erika_capi` crate をビルドします。

## tvOS Build Path

tvOS plugin は CocoaPod script phase 経由で liberika C ABI static library をリンクします。iOS と同様に既定では prebuilt アーカイブをダウンロードし、`ERIKA_FORCE_SOURCE_BUILD=1` のときのみソースビルドします。tvOS 13+、arm64 実機、arm64/x86_64 simulator に対応します。Rust nightly、prebuilt bundle、source build の詳細は [`packages/erika_flutter/README.ja.md`](../packages/erika_flutter/README.ja.md) を参照してください。

## macOS Build Path

macOS pod は package に固定された checksum 検証済み dylib を download して app に bundle します。repository checkout 内も同じ既定値です。ローカル変更の開発では `ERIKA_FORCE_SOURCE_BUILD=1` と、必要に応じて `ERIKA_REPO_ROOT` を設定します。architecture と library の指定は [Flutter README](../packages/erika_flutter/README.ja.md) に記載します。

## Minimal Presenter Flow

```c
ErikaPresenterHandle *presenter = erika_presenter_create();
erika_presenter_attach_metal_layer(
    presenter,
    (uint64_t)cametal_layer,
    width,
    height,
    backing_scale);
erika_presenter_open(presenter, "/path/to/media.mp4");
erika_presenter_play(presenter);

// On every display tick:
ErikaPresenterStats stats;
erika_presenter_render_tick(presenter, host_time_seconds, &stats);

// On resize:
erika_presenter_resize_surface(presenter, width, height, backing_scale);

// On dispose:
erika_presenter_detach_surface(presenter);
erika_presenter_destroy(presenter);
```

## Flutter Texture Path

Flutter Texture は機能が限定された compatibility path です。

用途:
- SDR fallback。
- native view composition がまだ使えない platform。
- Flutter compositor に入るべき透明 video（macOS / Windows / OpenHarmony の `ErikaTextureVideoView`）。
- test surface や制約の強い embedding 環境。

HDR/EDR の推奨 path ではありません。video が Flutter compositor に入ってしまうためです。Apple では surface は IOSurface-backed の `CVPixelBuffer` です。host が Flutter texture を登録し、毎 render tick の前に `erika_presenter_attach_flutter_texture` と `erika_presenter_set_flutter_texture_buffer` でその frame の Metal texture を選択すると、Flutter が同じ buffer を CPU readback なしで合成します。pixel buffer の所有権は host 側にあります。

## wgpu と Android

Apple HDR path は native Metal、Windows は native Direct3D 11 renderer（D3D11VA
zero-copy decode、HDR10 output）を使います。Android では wgpu が実 renderer です。Vulkan は
MediaCodec Surface frame を AHardwareBuffer 経由で import し、software frame には明示的な
CPU upload fallback があります。video、subtitle、danmaku、capture、ArtCNN compute はこの
path を共有します。Vulkan は FP16 extended-linear scRGB を negotiate でき、GLES または
capability negotiation failure は明示的に SDR へ fallback します。Android SDR は検証済み、
API 35 HDR device の active-path acceptance は未完了です。Linux は実験的な X11/Wayland と Flutter 接続を提供します。

Windows では各 presenter が専用のネイティブ worker thread で動きます。create、コマンド、surface の attach/resize/detach、render tick、capture、destroy はすべてそこで実行され、フレーム要求は集約されます（presenter ごとに queue 済みまたは実行中の tick は最大 1 つ）。メソッドの結果、`EventSink` event、SMTC 更新、texture 登録、HWND/DirectComposition の操作は platform thread に残るため、遅い render tick がウィンドウのメッセージループを塞ぐことはなくなりました。`ERIKA_FLUTTER_FRAME_TRACE=1` を設定すると、presenter ごとに 1 回 `presenter_worker ... platform_thread=... render_thread=... off_platform=true` を、worker 上で計測した遅い tick を `frame_tick ... work_ms=` として log に出します。

他のプラットフォームも同じ owner モデルです。Linux は正規の `windows/erika_presenter_worker.h` を共有し、独自の 16 ms 単調クロックで駆動します。GTK、view、Flutter texture の登録は platform thread に残り、結果と event は idle source 経由で渡されます。OpenHarmony（Flutter プラグインと `erika_ohos` パッケージ）は `presenter_bridge.h` の背後に presenter ごと 1 つの owner thread を持ち、すべての N-API 関数に ArkTS thread で promise を resolve する `...Async` 版もあります。従来の C ヘルパー `ErikaOhosPlayerFromId` は、別スレッドが所有する presenter にネイティブコードが触れてはならないため、null を返すようになりました。Android は引き続き presenter ごとに専用の `HandlerThread` を使います。macOS、iOS、tvOS では各 presenter が専用スレッド（`ErikaPresenterOwner.swift`）を持ち、create、すべてのネイティブコマンド、render、event の polling、capture、destroy を FIFO 順で実行し、フレーム tick は集約されます。`CVDisplayLink` と `CADisplayLink` はそのスレッドを起こすだけで、メインスレッドは引数のデコード、結果と event の順序どおりの配送、AppKit/UIKit の処理（now-playing、リモートコマンド、view）を行います。このため遅い `open`、`seek`、`dispose` が UI スレッドを塞いだり描画を遅らせたりすることはなく、`ERIKA_FLUTTER_FRAME_TRACE=1` は各 tick とコマンドのスレッドを log に出します。

## Dart API

インストール、再生制御、HTTP キャッシュ、イベント、ビューの例は
[Flutter SDK ガイド](../packages/erika_flutter/README.ja.md) にまとめています。

## メディアトラック情報

`tracks()` は embedded/external の各トラックについて `ErikaTrackInfo` を返します。video
track には `codec`、`width`、`height`、`pixelFormat`、`profile`、`level`、`bitRate`、
`frameRateNumerator`、`frameRateDenominator` が含まれ、audio track にはさらに
`sampleRate`、`channels`、`sampleFormat` が含まれます。

- `bitRate` の単位は bit/s です。video track 自身の codec parameter を優先し、単一の
  video track に bitrate がなく、container total と他の全 audio track bitrate が既知の場合のみ、
  container bitrate から audio bitrate を引いて推定します。取得できない場合は `null` です。
  瞬間 bitrate ではなく、推定値には container overhead や非 audio stream が含まれる場合があります。
- `frameRateNumerator` / `frameRateDenominator` は `30000/1001` などの有理数を保持します。
  probe 順序は average frame rate、`r_frame_rate`、FFmpeg guessed frame rate です。
  `framesPerSecond` は Dart の convenience getter であり、VFR media では平均、宣言、または
  推定値です。
- `TracksChanged` と `TrackSelectionChanged` event には完全な `trackList` が含まれます。
  event 後に `tracks()` を再度呼んで current snapshot を取得することもできます。

## ネイティブ Debug HUD

`setDebugHudEnabled(true)` は native video composition に診断 HUD を描画します。Dart
を通じて render せず、Flutter widget tree も変更しません。既定では off で、development、
performance analysis、device 上の diagnosis 用です。

HUD は codec/resolution/bitrate/frame rate、playback position/rate、decoded/rendered FPS、
decode route、zero-copy/fallback counters、CPU/GPU render time、audio queue/underflow、HDR
output negotiation、danmaku item count を表示します。FPS は隣接 sample window の差分であり、
その他の frame/failure counter は presenter lifetime 中の累積値です。HUD は
`screenshot()` の off-screen capture には含まれません。

## ホストとエージェント向けの診断

3 つのメソッドが自動トラブルシューティング（カクつき、映像破損、デコード失敗、音ズレ）の
ためにエンジン自身の視点を提供します。

- `getDiagnosticsSnapshot()` は 1 つの JSON map（schema 1）を返します: デコーダと
  ハードウェア経路・フォールバック、presenter カウンタ、出力/HDR、リソース、
  アップスケーラ、オーディオ、バッファ深さ、タイミング（スケジューラの遅延ドロップ、
  表示間隔とジッタ、クロックのドリフトと補正）。不明な値は `null` です。
- `getLogs({limit, minLevel, sinceSeq})` はプロセス全体のマスク済みログリング
  （liberika の運用診断と FFmpeg の警告・エラー）を読みます。
- `invokeJson(method, [arguments])` はネイティブ JSON メソッド table の任意の項目を呼び、
  envelope（`{ok, status, value | error, code?}`）を返します。
  `invokeJson('listMethods')` で table を列挙できるので、新機能に Dart の変更は不要です。

フィールドは [capi_reference.ja.md](capi_reference.ja.md) の「エージェント向け診断
（JSON）」と「JSON ブリッジ」を参照してください。プラットフォームプラグインまたは同梱の
ネイティブライブラリにメソッドが無い場合は `UnsupportedError` を投げます。本リリースの
状況: macOS・iOS・tvOS は新シンボルをオプションで解決、Linux・HarmonyOS・Android は共有
JSON table を経由、Windows は `UnsupportedError`。

macOS と iOS では、プラグインが `invokeJson` と `getDiagnosticsSnapshot` を presenter の owner thread で、
`getLogs` をユーティリティキューで実行し、メインスレッドで応答するため、時間のかかる呼び出し（大きな字幕
ファイルの `addExternalSubtitle`、`exportFrame`）がメインスレッドを占有しません。時間のかかる presenter
メソッドはその presenter の次のフレームを遅らせます（フレームはコマンドの合間に実行されます）が、UI は
止めません。`scope` が `global` のメソッド（`exportFrame`、`exportFrames`、`getLogs`、`listMethods`）は
presenter ハンドルなしで実行されます。`listMethods` は各メソッドに `blocking`（処理が終わってから返り、時間が入力に応じて増える）と
`writesFiles`（ファイルを作り、結果にパスを列挙する）を付けます。

## プレーヤーなしで画像を取る

`ErikaPlayer.exportFrame(uri, position: ...)` は、独自の demuxer とマルチスレッドのソフトウェアデコーダで、
あるメディア時刻に表示される画像を RGBA または PNG で返します。プレーヤーは関与しません。HDR ソース
（PQ、HLG、Dolby Vision）は、プレーヤーが SDR ディスプレイ向けに使う演算で SDR にトーンマッピングされ、
`ErikaExportedFrame.toneMapped` で分かります。同じメディアから複数枚取るには `ErikaFrameExportSession`
（または `ErikaPlayer.exportFrames`）を使います: メディアは一度だけ開かれ、時刻の昇順の要求は seek し直さず
続きをデコードします。処理は UI スレッドの外で行われ、macOS はネイティブのワーカーキュー、FFI
プラットフォーム（iOS、tvOS、Android、Windows、Linux、HarmonyOS）はセッションごとのバックグラウンド
isolate です。セッションは `close()` で閉じます。`ERIKA_FLUTTER_LIBRARY` にライブラリのパスを設定すると、
FFI の書き出しを検証中のビルドに向けられます。

## Neural Upscaler Status

`setUpscaler(ErikaUpscalerMode)` でモード（ArtCNN、Anime4K、`fsr1`、`platformVideo`、`metalFxSpatial`、`custom`。
モードと各 platform での実行内容は [統合ガイド](integration.ja.md#アップスケーラ) を参照）を要求します。kernel は
background thread で compile されるため、host は `getUpscalerStatus` を poll して UI を更新する必要があります。

| `activeBackend` | 意味 |
|-----------------|------|
| `off` | どの mode も要求されていない。 |
| `inactive` | mode は要求されたがここでは動かせない（または backend が fallback/failure を記録した）。native sampling を使い、理由は診断の `upscaler` オブジェクトに出る。 |
| `building` | kernel を background で compile 中。準備完了まで native sampling で描画される。 |
| `scalar` | 汎用 compute shader。 |
| `simdgroupMatrix` | Apple `simdgroup_matrix`（M1-M4/A14+）。 |
| `cooperativeMatrix` | D3D11 と併用する Vulkan `VK_KHR_cooperative_matrix`（Tensor Core / WMMA / XMX）。 |
| `linalgMatrix` | D3D12 SM 6.10 `linalg::Matrix`。opt-in の preview。 |
| `videoProcessor` | D3D11 video processor の vendor extension: NVIDIA RTX Video / Intel VSR。 |
| `videoToolbox` | VideoToolbox のスーパー解像度。 |
| `metalFx` | MetalFX spatial scaler。 |
| `tensorOps` | Metal 4 tensor ops（Apple10 GPU: M5/A19）。 |

`getUpscalerStatus().lastGpuDuration` は Windows でも埋まるようになりました（D3D11 timestamp query）。

UI で伝えておきたい制限: RTX Video Super Resolution（Windows の `platformVideo`）は RTX GPU、NVIDIA App /
コントロールパネルでの設定有効化、SDR 映像が必要です。VideoToolbox のスケーラは 2x、8 ビット 4:2:0 で 96x96 から
960x960 のソースのみ、macOS/iOS/tvOS 26+ が必要です。D3D12 linalg は opt-in の preview です。wgpu renderer
（Linux、Android、OpenHarmony、Windows の wgpu fallback）はすべてのネットワークモードと `fsr1` を実行し、`platformVideo` と
`metalFxSpatial` は `inactive` を報告します。R8F64 は行列ユニットと SD/720p 素材でのみ実用的です。

`loadUpscalerModel(Uint8List data, {String name = 'custom'})` はカスタムネットワーク（Erika `.esr` ファイル、または
FSRCNNX / Anime4K CNN の mpv hook shader。ライセンス上 FSRCNNX は同梱せず、host がファイルを用意）を読み込み、
続けて `setUpscaler(ErikaUpscalerMode.custom)` を呼びます。`clearUpscalerModel()` で外します。Linux plugin は
JSON のファイルパス方式（`loadUpscalerModelFile`）のみです。

renderer 側の設計は `docs/architecture.md` を参照してください。

## Ownership Rule

Flutter は layout と controls を担当し、liberika は video plane、subtitle plane、danmaku plane、audio、timing を担当します。plugin は `MethodChannel` を通じて command と event を橋渡しし、rendering は Dart を経由しません。

## Linux の映像サーフェス

`ErikaVideoView` と `ErikaTextureVideoView` は GPU の結果を SDR RGBA として読み戻します。
Wayland の `ErikaWindowOverlayVideoView` は透明 Flutter UI の下にネイティブ映像を置き、
フレームごとの読み戻しを減らします。映像への Flutter クリップやフィルターは使えません。
ローカルのランタイムをビルドし `ERIKA_LIBRARY_DIR` を設定します。[Linux ガイド](linux.md)。
