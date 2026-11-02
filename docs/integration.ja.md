# ネイティブホストへの liberika 組み込み

本ガイドは liberika を非 Flutter ホスト——C/C++/Swift アプリ、Win32 ウィンドウ、C FFI を
持つ任意のランタイム——に組み込む手順です。**presenter（プッシュ）モデル**を使います。
liberika が decode・timing・audio・overlay・presentation を所有し、ホストは surface を提供して
フレームごとに `render_tick` を 1 回呼びます。

前提：C ABI（[capi_reference.ja.md](capi_reference.ja.md)）とビルド済みの liberika ライブラリ
（[building.ja.md](building.ja.md)）。Flutter では代わりに
[`erika_flutter`](../packages/erika_flutter) プラグインを使い、
[flutter_embedding.ja.md](flutter_embedding.ja.md) を参照してください。

> 英語版：[integration.md](integration.md)。

実行可能な参考が 2 つ付属します：
[`examples/macos_native_demo`](../examples/macos_native_demo)（AppKit + `CAMetalLayer`）と
[`examples/windows_native_demo`](../examples/windows_native_demo)（Win32 + `HWND`）。これらは
Rust の `PresenterRuntime` を直接駆動します。以下の C ABI 呼び出しは 1:1 の等価物です。

## 1. handle ファミリーを選ぶ

自分で描画する理由がなければ `ErikaPresenterHandle` を使います。プルモデルの
`ErikaHandle` は、独自コンポジタを持ち liberika の decode/timing/state だけが欲しいホスト
向けです。本ガイドの残りは presenter ベースです。

presenter ファミリーは **macOS / iOS / tvOS / Windows / Android / OpenHarmony / Linux（実験的）** でコンパイルされます。
native host API は platform ごとに異なります。HarmonyOS Flutter host は desktop sample を
そのまま再現するのではなく、通常 plugin の ArkTS JSON bridge を使います。

## 2. ライフサイクル

```
create ──▶ attach surface ──▶ open ──▶ play ──▶ (render_tick + poll_event ループ)
                                                       │
                               pause / seek / set_* ◀──┤
                                                       ▼
                         detach surface ──▶ destroy
```

`open` は同期的にメディアを調査し、成功時に `Ready` で戻ります。`play` は再生処理を
キューに送り、結果は `StateChanged` と `Error` で確認します。HTTP の調査は待ち時間を
伴うため、UI を止めない場合は同じ handle の呼び出しをすべて直列 worker に置きます。
再生前に surface を attach してください。`close` 後は新しい handle を作成します。
先頭からの再生には `stop` を使えます。

## 3. presenter を作る

```c
ErikaPresenterConfig cfg = {
    .output_mode  = ErikaPresenterOutputMode_Sdr,   // AppleEdr / Android ExtendedLinear
    .edr_headroom = 1.0f,                            // requested content-headroom ceiling
    .video_alpha_mode = ErikaVideoAlphaMode_Opaque,
    .luma_upscaler = ErikaLumaUpscalerMode_Off,      // または任意の ErikaLumaUpscalerMode（例: ArtCnnC4F16）
};
ErikaPresenterHandle *p = erika_presenter_create_with_config(cfg);
if (!p) { /* erika_last_error_message() を読む */ }
```

`erika_presenter_create()` は既定値（SDR、アップスケーラ無し）。使えるモードとプラットフォームごとの
実行内容は [アップスケーラ](#アップスケーラ) を参照してください。

### アップスケーラ

`set_upscaler(mode)` でモード（`ErikaLumaUpscalerMode`、値はファミリーごとにまとまっています）を選びます。

| 値 | ファミリー | 対象 |
|---|---|---|
| 1-7 | ArtCNN: C4F16、C4F32、C4F16 DS、C4F16 DN、C4F32 DS、C4F32 DN、R8F64 | アニメ。DS = ノイズ除去 + シャープ化（劣化した素材向け）、DN = ノイズ除去。R8F64（約 926K パラメータ）は行列ユニットと SD/720p 素材でのみ実用的で、デスクトップビルドにのみ同梱 |
| 16-25 | Anime4K v3.2 Upscale CNN x2（S、M、L、VL、UL）と Upscale+Denoise（21-25） | アニメ |
| 32 | `Fsr1`: 輝度に対する AMD FSR 1.0 EASU + RCAS（RCAS シャープネス 0.2 stops） | 任意 |
| 48 | `PlatformVideo`: プラットフォーム標準の映像スーパー解像度 | 実写 / 任意 |
| 49 | `MetalFxSpatial`（Apple のみ） | 任意 |
| 64 | `Custom`: 実行時に読み込むネットワーク | - |

ネットワーク系モードはデコード済み輝度プレーンを 2 倍にします（クロマはソース解像度のままで、通常のスケーラが
再構成します）。`Fsr1` と `MetalFxSpatial` は輝度を表示サイズまで直接スケールし、`PlatformVideo` は
フレーム全体をスケールします。

| プラットフォーム | 実行内容 |
|---|---|
| Windows（D3D11） | すべてのネットワークモード（`Custom` を含む。生成した compute kernel で、スカラー、Vulkan cooperative matrix、D3D12 linalg のうち最速のものをネットワークとフレームサイズごとに計測して選択）、`Fsr1`、`PlatformVideo`: NVIDIA RTX Video Super Resolution（RTX GPU が必要で、NVIDIA App / コントロールパネルで有効化）または Intel VSR（VSR 対応ドライバの Arc/Xe）。SDR 映像のみ、D3D11VA でデコードした NV12/P010 フレーム。`MetalFxSpatial` は `Inactive`。D3D12 linalg は opt-in の preview。 |
| Apple（Metal: macOS、iOS、tvOS） | すべてのネットワークモード（`Custom` を含む。M5/A19 は Metal 4 tensor ops、M1-M4/A14+ は `simdgroup_matrix`、それ以外はスカラー）、`Fsr1`、`MetalFxSpatial`、VideoToolbox `VTLowLatencySuperResolutionScaler` 経由の `PlatformVideo`: 2x のみ、8 ビット 4:2:0 で 96x96 から 960x960 のソース、macOS/iOS/tvOS 26+。それ以外は理由付きで `Inactive`。 |
| wgpu renderer（Linux、Android、OpenHarmony、Windows の wgpu fallback） | `Custom` を含むすべてのネットワークモードと `Fsr1`（生成した WGSL kernel。adapter が `SHADER_F16` を持てば fp16、16x16 cooperative matrix を公開する Vulkan adapter（NVIDIA など）では行列ユニットを使用。adapter ごとの既定値を、最初の数フレームで少数の kernel 候補を計測して最速のものに置き換えます）。`PlatformVideo` と `MetalFxSpatial` は `Inactive`。Android/OpenHarmony の実機ではまだ計測していません。 |
| Web | アップスケーラ無し。 |

動かせないモードは `Inactive` の backend status を報告して native luma sampling を維持し、理由は診断の
`upscaler` オブジェクト（`reason`、`engine`、`lastGpuMs`）に出ます。すべての backend で kernel を background
thread で compile し、準備ができるまでの status は `Building` で native sampling を使うため、host は
`get_upscaler_status` を poll して UI を更新してください。

実測 GPU 時間（指定ソースの 2 倍拡大。中央値、GPU timestamp）:

| モデル（1080p ソース） | Windows、RTX 5060 Ti（自動選択した backend） | Apple M5 Pro（Metal） | RTX 5060 Ti の wgpu/Vulkan（Linux の経路） | Intel UHD 770（D3D11） |
|---|---|---|---|---|
| ArtCNN C4F16 | 2.44 ms スカラー（従来 12.38 ms） | 2.65 ms tensor ops | 2.77 ms | 68 ms |
| ArtCNN C4F32 | 7.29 ms cooperative matrix（スカラー 9.5 ms、従来 39.4 ms） | 6.35 ms | 11.4 ms | 267 ms |
| Anime4K S / M / L / VL / UL | 0.35 / 0.78 / 0.83 / 2.07 / 4.11 ms | 1.0 / 2.8 / 2.0 / 4.96 / 7.3 ms | 0.45 / 1.04 / 1.03 / 2.85 / 5.80 ms | 8.9 / 18.7 / 22.4 / 54.5 / 173 ms |
| ArtCNN R8F64（720p ソース） | 71 ms cooperative matrix（D3D12 linalg で 58 ms） | 64.6 ms | 183 ms | 再生には不向き |
| FSR1（1080p から 4K） | 0.31 ms | 0.77 ms | 0.55 ms | 10 ms |
| MetalFX（1080p から 4K） | なし | 0.67 ms | なし | なし |

デノイズ/シャープ版（ArtCNN DS/DN、Anime4K Denoise）の時間は元のモデルと同じです。R8F64 はどの GPU でも
リアルタイムではありません。

Windows アクセラレータ、RTX 5060 Ti（特記なしは 1080p ソース。selector は実フレームで候補を計測して最速を
残すため、これらが選ばれるのは幅の広いモデルだけです）:

| モデル | D3D11 スカラー | Vulkan cooperative matrix | D3D12 linalg（opt-in） |
|---|---|---|---|
| ArtCNN C4F16 | 2.44 ms | 3.59 ms | 3.14 ms |
| ArtCNN C4F32 | 9.5 ms | 7.29 ms | 6.53 ms |
| ArtCNN C4F32、4K ソース | 39.5 ms | 27.7 ms | - |
| Anime4K UL | 4.11 ms | 7.39 ms | 20.2 ms |
| ArtCNN R8F64、720p ソース | 243 ms | 71.4 ms | 58.1 ms |

cooperative matrix の時間は既定の fp32 累積です。`ERIKA_VK_FP16_ACCUMULATION=1` では C4F32 が 6.24 ms、
R8F64 が 58.7 ms になり、精度はわずかに下がります。

**カスタムモデル。** `erika_presenter_load_upscaler_model(handle, name, data, len)` は Erika `.esr` ネットワーク
（`crates/erika/assets/artcnn/export_artcnn_esr.py` で ArtCNN ONNX をエクスポート。たとえば同梱されない R16F96）
または FSRCNNX / Anime4K CNN 方言の mpv hook shader を読み込みます。その後
`set_upscaler(handle, ErikaLumaUpscalerMode_Custom)` で選択し、`erika_presenter_clear_upscaler_model` で外します。
重みだけを解析し、shader コードは実行しません。FSRCNNX（GPL-3.0/LGPL-3.0）と RAVU/NNEDI3（LGPL-3.0）は
liberika が MPL-2.0 のため同梱せず、host がファイルを用意します。presenter JSON bridge では
`loadUpscalerModelFile {path, name?}` と `clearUpscalerModel` を使います。

**Windows のチューニング（診断用）。**

| 変数 | 効果 |
|---|---|
| `ERIKA_SR_BACKEND` | `scalar`、`coopmat`、`linalg` で D3D11 側の executor を固定（既定: 計測による自動選択）。Apple: `scalar`、`matmul`、`tensor`、`legacy`、`legacy-scalar`。 |
| `ERIKA_SR_NO_SHADER_CACHE=1` | D3D11 kernel のディスクキャッシュを無効化（`%LOCALAPPDATA%\liberika\sr-shader-cache`）。 |
| `ERIKA_SR_TILE_BUDGET_MB` | D3D11 の中間テンソル予算（既定はネットワークごとに 24-192 から選択）。 |
| `ERIKA_VK_FP16_ACCUMULATION=1` | Vulkan cooperative matrix backend で fp16 累積を使います。 |
| `ERIKA_WGPU_SR_AUTOTUNE=0` | wgpu（全プラットフォーム）: 最初の数フレームで候補を計測せず、adapter ごとの kernel 既定値を使います。その他の `ERIKA_WGPU_SR_*` 変数で個々の kernel 選択を固定できます。`crates/erika/src/renderer/wgpu_sr.rs` を参照。 |
| `ERIKA_SR_D3D12_LINALG=1` と `ERIKA_D3D12_AGILITY_SDK_PATH`、`ERIKA_D3D12_DXCOMPILER_PATH`、`ERIKA_D3D12_DXC_INCLUDE_PATH` | D3D12 SM 6.10 preview backend を opt-in（Agility SDK 1.722-preview と SM 6.10 preview の DXC。実験的 shader model には Windows の開発者モードが必要。正式版は Agility SDK 1.621 で 2026 年 12 月の見込み）。`crates/erika/src/renderer/d3d12_linalg/README.md` を参照。 |

### 字幕・弾幕の高画質設定の既定値

ネイティブ presenter では、Web backend と同様に字幕と弾幕のスーパーサンプリングが
既定で有効です。テキスト字幕と弾幕の字形を 2 倍の解像度で描画し、表示サイズ、
レイアウト、映像の解像度は維持します。Rust ホストでは
`PresenterRuntime::set_subtitle_supersampling(false)` と
`DanmakuLayoutConfig { supersampling: false, ..Default::default() }` で個別に無効化できます。
後者は `PresenterRuntime::set_danmaku_config` で適用します。

ネイティブ字幕の既存の性能制限は維持されます。元のキャンバスの幅または高さが
2048 ピクセルを超える場合、または面積が 2048 × 1080 ピクセルを超える場合は、
元の解像度を使います。60 回の字幕描画の平均処理時間が 4 ms を超えると、
現在のメディアでは元の解像度に戻ります。128 px より大きい弾幕の字形も元の解像度を使います。

## 4. surface を attach する

liberika は所有する surface に直接描画します。幅・高さは**物理ピクセル**、`scale` は
DPI/backing 係数です。

### macOS / iOS / tvOS —— `CAMetalLayer`

`CAMetalLayer` を作りサイズを設定し、そのポインタを liberika に渡します：

```c
// `layer` は CAMetalLayer*（例：NSView/UIView のホスト layer から）
erika_presenter_attach_metal_layer(p, (uint64_t)(uintptr_t)layer,
                                   pixel_w, pixel_h, backing_scale);
```

macOS で推奨の構成は、コンテンツビューの sibling / underlay となる window-hosted layer
で、映像を AppKit のビューコンポジタの外に保ちます（Flutter プラグインと同じモデル。
[flutter_embedding.ja.md](flutter_embedding.ja.md) 参照）。

### Windows —— `HWND`

```c
HWND hwnd = /* あなたのウィンドウ */;
HINSTANCE hinst = GetModuleHandleW(NULL);
UINT dpi = GetDpiForWindow(hwnd);
double scale = dpi ? (double)dpi / 96.0 : 1.0;
RECT rc; GetClientRect(hwnd, &rc);
uint32_t w = max(1, rc.right - rc.left), h = max(1, rc.bottom - rc.top);

erika_presenter_attach_windows_hwnd(p, (uint64_t)(uintptr_t)hwnd,
                                    (uint64_t)(uintptr_t)hinst, w, h, scale);
```

`attach_windows_hwnd` は `attach_wgpu_surface` + kind `WindowsHwnd` の便利ラッパです。
既定の presenter 設定では、この surface は**ネイティブ Direct3D 11** レンダラ（D3D11VA
ゼロコピー、HDR10）を駆動します。C ABI の `ErikaPresenterConfig` ではレンダラを選択
できません。Rust ホストは `PlayerConfig.renderer` で `WgpuFallback` を選択できます。

### 汎用 —— `attach_wgpu_surface`

X11/Wayland/Android、または surface 種別を明示したい場合は、対応する
`ErikaWgpuSurfaceKind` とプラットフォームハンドルを付けて
`erika_presenter_attach_wgpu_surface(p, kind, raw_window, raw_display, w, h, scale)`
を使います。

### Linux X11 / Wayland

`XlibWindow` は X11 Window ID と Display ポインター、`WaylandSurface` は
`wl_surface*` と `wl_display*` を受け取ります。detach 完了までホストが保持します。
ビルドとサンプルは [Linux ガイド](linux.md) にあります。

### Android extended-linear scRGB

Android `ExtendedLinear` は FP16 extended-linear scRGB で、HDR10/PQ ではありません。
native host は direct-composited `SurfaceView` から `ANativeWindow` を取得し（Flutter
plugin は Hybrid Composition を使用）、probe した output capabilities を attach に渡します。

```c
ErikaSurfaceOutputCapabilities caps = {
    .extended_linear = display_and_surface_are_hdr_capable,
    .direct_composition = true,       // SurfaceView。TextureView ではない
    .desired_headroom = requested_headroom, // 0 = system auto
    .fallback_reason = host_probe_reason, // eligible なら 0
};
erika_presenter_attach_wgpu_surface_with_output_capabilities(
    p, ErikaWgpuSurfaceKind_AndroidNativeWindow,
    (uint64_t)(uintptr_t)native_window, 0, w, h, scale, caps);
```

requested mode、display/surface eligibility、direct composition、Vulkan backend、
`Rgba16Float` support、configure 後の `ADATASPACE_SCRGB_LINEAR` readback がすべて
成功した場合だけ extended-linear が active になります。どれかが失敗すると SDR playback
を維持し、安定した `fallback_reason` code `0..8` を記録します。GLES と `TextureView` は
常に SDR path です。

`ErikaPresenterConfig.edr_headroom` は content ceiling です。正の `desired_headroom` は
optional surface ceiling、`0` は system auto を表します。wgpu の effective target はこれらの
ceiling と、known な場合の current display HDR/SDR ratio に従います。API 34+ の native host
は `Display.registerHdrSdrRatioChangedListener` を監視し、実際の state change ごとに
`erika_presenter_set_output_headroom(p, ratio, true)` を呼びます。ratio unavailable / view
detach では `(1.0f, false)` を publish します。Flutter plugin はこの wiring を自動で行います。
API 35 では `SurfaceView.setDesiredHdrHeadroom` も view ごとに呼び、global Window は変更しません。

attach 後と resize/recovery ごとに `erika_presenter_get_output_status` を取得します。
Android scRGB の active state は `AndroidExtendedLinearScRgb`、`SixteenBitFloat`、native
dataspace `406913024`、`extended_linear_active = true` です。Android が display ratio を
提供した場合は `active_headroom` がその ratio、`active_headroom_known = true` です。
提供されない場合は fallback value で `active_headroom_known = false` です。known state または
ratio が実際に変わった場合だけ `headroom_updates` が増えます。requested mode だけでは active
state の証明になりません。現在の non-HDR/emulator test は SDR fallback を対象とし、active
path は API 35 HDR 実機 acceptance が必要です。

## 5. open して play

```c
if (erika_presenter_open(p, "/path/to/video.mkv") != ErikaStatus_Ok) { /* ログ */ }
erika_presenter_play(p);
```

`uri` はローカルパスまたは HTTP(S) URL です。キャッシュ容量は open options で設定します。

```c
ErikaOpenOptions options = {0};
options.http_read_ahead_bytes = 8 * 1024 * 1024;
options.http_back_buffer_bytes = 128 * 1024 * 1024;
erika_presenter_open_with_options(p, "https://example.com/video.mp4", &options);
```

先読みは既定 2 MiB、リワインドは 16 MiB。0 は既定値を選びます。
`ERIKA_HTTP_READAHEAD_BYTES` は既定の先読み容量を指定します。容量はバイト単位です。

HTTP(S) サーバーが Range を無視し、ファイル全体を `200` で返す場合や、
Content-Length がない場合にも対応します。要求オフセットより前のバイトを破棄し、
必要なデータだけをバッファーに保持します。完全な応答の EOF から全長を取得できます。
このようなサーバーではシークや再接続のたびに先頭部分を再取得するため、大きなファイルは
`206` 対応のサーバーで高速に読み込めます。再試行中にリソースの検証子が変わった場合は、
異なるバージョンのバイトが混在しないよう応答を拒否し、同じソースでの同期取得への
フォールバックと以降の読み取りも停止します。弱い ETag は初回の読み取りには使えますが、
中断した応答の再開を許可する根拠にはなりません。

## 6. レンダーループ

surface の表示タイマー——`CADisplayLink`（iOS/tvOS）/ `CVDisplayLink` または `CADisplayLink`
（macOS）/ Windows のフレームスケジューラ / Android `Choreographer`——から `render_tick` を駆動します。そのフレームの
**プレゼンテーション時刻（秒）**を単調なホストクロックから渡します。liberika は vsync 量子化
スケジューリングに使うので、差分ではなく絶対タイムスタンプを渡します。

```c
// 表示フレームごと:
ErikaPresenterStats stats;
erika_presenter_render_tick(p, host_time_seconds, &stats);   // out_stats は NULL 可

// 同じ反復でイベントをドレイン:
ErikaEvent ev;
while (erika_presenter_poll_event(p, &ev) == ErikaStatus_Ok) {
    handle_event(&ev);
}
```

drawable サイズや scale が変わったら（ウィンドウリサイズ、モニタ DPI 変更、デバイス回転）、
次の tick の**前に** `erika_presenter_resize_surface(p, w, h, scale)` を呼びます。Windows
demo は毎フレーム `GetClientRect` + `GetDpiForWindow` をポーリングし、変化時にリサイズします。

`render_tick` はすぐ返ります。それ自体は vsync でブロックしません——リズムは表示タイマーが
与えます。表示コールバック上にいない場合（スモークテスト等）、反復ごとに `~16 ms` の
sleep で 60 Hz を近似できます。

### 表示目標時刻

0.2.1 の `erika_presenter_render_tick_with_timing` は表示目標時刻の映像・字幕・弾幕を
同じスナップショットでサンプルします。delay は呼び出し入口から目標までの秒数で、
範囲は ±0.25 秒。NULL は従来の動作です。

```c
double delay = display_target_seconds - monotonic_now_seconds;
erika_presenter_render_tick_with_timing(p, display_target_seconds, &delay, &stats);
```

iOS Flutter プラグインは `CADisplayLink.targetTimestamp` を tick mailbox で渡します。

## 7. イベント処理

`poll_event` は非ブロッキングで、キューが空なら `NoEvent` を返します。`ev.kind` で分岐：

| Kind | 意味 | 読む |
|------|------|------|
| `StateChanged` | 再生状態が遷移 | `ev.state` |
| `DurationChanged` | 尺が判明/更新 | `ev.duration_micros` |
| `PositionChanged` | 周期的な位置 tick | `ev.position_micros` |
| `TracksChanged` | トラックリスト変化 | `erika_presenter_tracks` を再取得 |
| `TrackSelectionChanged` | 選択変化 | `erika_presenter_track_selection` |
| `BufferingChanged` | バッファリング切替 | `ev.buffering` |
| `VideoParamsChanged` | 解像度 / 色メタデータ | `ev.video` |
| `Error` | 失敗発生 | `ev.status` + `erika_last_error_message` |

## 8. ランタイム制御

以下はすべて tick の合間にライブで安全に呼べます：

- **トランスポート:** `play` / `pause` / `stop` / `seek(position_micros)` /
  `set_playback_rate(rate)`。
- **オーディオ:** `set_volume(0.0–1.0)`。
- **トラック:** `erika_presenter_tracks`（カウント配列イディオム）、`select_audio_track` /
  `select_subtitle_track`（id `-1` で字幕無効）、`add_external_subtitle`、
  `remove_subtitle_track`、`set_subtitle_scale`。
- **字幕スタイル:** `set_subtitle_font(family, file_path)` と
  `set_subtitle_style(ErikaSubtitleStyle)`（font・色・metrics・attribute・縁取り・
  alignment・margin）。どちらも fallback で、対応する `override_mask` の bit を
  立てない限り ASS script は自身の styling を保ちます。サイズと縁取りには
  `set_subtitle_scale` がさらに掛かります。
  [capi_reference.ja.md](capi_reference.ja.md#再生とランタイムパラメータ) 参照。
- **弾幕:** トラックの読み込み（`load_danmaku_file` / `_json` またはマルチトラック
  `add_danmaku_track_*`）、トグル（`set_danmaku_enabled`）、`set_danmaku_config` で調整、
  トラックのオフセット、フォント設定、ブロックワード設定。
  [danmaku_architecture.md](danmaku_architecture.md) 参照。
- **アップスケーラ:** `set_upscaler(mode)`、`Custom` は `load_upscaler_model` / `clear_upscaler_model`。`get_upscaler_status` で確認。[アップスケーラ](#アップスケーラ) 参照。
- **出力:** `erika_presenter_get_output_status` で active mode と fallback counter を確認。
  Android native host は `erika_presenter_set_output_headroom` で dynamic display ratio を
  publish します。`capture_frame_rgba` は display output が extended-linear の場合も、映像 +
  字幕（弾幕は含みません）を SDR RGBA8 screenshot として返します。
- **ホストのオフスクリーン合成:** Rust の
  `PresenterRuntime::capture_frame_rgba_with_danmaku(w, h)` または C の
  `erika_presenter_capture_frame_rgba_with_danmaku` は surface を接続せずに
  有効な弾幕も含む RGBA8 を返し、ホストの UI テクスチャにアップロードできます。
  [capture の契約と使用例](capi_reference.ja.md)を参照。
  CPU readback のない GPU handle には `export_frame_texture` を使い、音声もその
  ループで進めます。[ネイティブテクスチャ出力](external_textures.ja.md)を参照。

## 9. 解放

```c
erika_presenter_detach_surface(p);   // 先に surface への描画を止める
erika_presenter_destroy(p);          // 再生を止め、すべて解放
```

ウィンドウ/layer を破棄する前に detach し、liberika が surface に触れるのを止めます。
`destroy` は `NULL` handle に対して安全です。

## 10. スレッドモデル

handle は**内部同期されません**。最もシンプルで正しい設計：presenter を 1 つのスレッド
——表示タイマーを動かすスレッド——で所有し、すべての呼び出し（`render_tick`、トランス
ポート、トラック変更）をそこから行います。別スレッドから呼ぶ必要がある場合（UI スレッドが
`seek` を発行する等）、同じ handle で 2 つの呼び出しが重ならないよう自分のロックで直列化
します。エラーメッセージはスレッドローカルなので、失敗した呼び出しを行ったスレッドで
`erika_last_error_message` を読みます。

## 言語別メモ

### C / C++

`erika.h` を include し、ライブラリをリンク（[building.ja.md](building.ja.md) 参照）すれば
完了——ABI は素の C です。C++ ではデストラクタで `erika_presenter_destroy` を呼ぶ RAII 型に
handle を包み、返された文字列 / `ErikaTrackInfo` レコードは対応する liberika の解放関数で解放
し、`delete` は使いません。

### Swift

bridging header か `erika.h` への module map で C ABI を取り込みます。`CAMetalLayer` は
`unsafeBitCast(layer, to: UInt64.self)` または `UInt64(UInt(bitPattern: ...))` でキャスト。
`CADisplayLink`/`CVDisplayLink` のコールバックから `erika_presenter_render_tick` を駆動します。
macOS/iOS/tvOS の Flutter Swift プラグインも同じ C ABI 上でこれを行っています。

### Dart FFI

`dart:ffi` でシンボルをバインド（dylib/dll は `DynamicLibrary.open`、静的リンクはプロセス
シンボル）。すべての FFI 呼び出しを 1 つの isolate に置き、文字列は `toNativeUtf8`/`free` で
マーシャリングします。高レベルの `erika_flutter` パッケージが既にこれを行っているので、
カスタム組み込みでない限りそちらを優先してください。

## チェックリスト

- [ ] presenter を作成（ディスプレイに合った出力モードで）。
- [ ] **物理ピクセル**サイズと正しい scale で surface を attach。
- [ ] Android extended-linear は Hybrid Composition の `SurfaceView` を使い、probe した
  output capabilities を渡し、`erika_presenter_get_output_status` で
  `Rgba16Float + SCRGB_LINEAR` を検証。失敗時は SDR reason を記録。
- [ ] API 34+ は `erika_presenter_set_output_headroom` で display HDR/SDR ratio change を
  publish。API 35 desired headroom は個別 `SurfaceView` に限定。
- [ ] open の成功後に play。HTTP probe は直列 worker で実行。
- [ ] 表示フレームごとに `render_tick(absolute_time_seconds)`。イベントをドレイン。
- [ ] サイズ/scale 変化のたびに `resize_surface`。
- [ ] handle ごとに 1 スレッド、または呼び出しを直列化。
- [ ] 返された文字列 / `ErikaTrackInfo` をすべて解放。`detach` してから `destroy`。
