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
    .luma_upscaler = ErikaLumaUpscalerMode_Off,      // または ArtCnnC4F16 / C4F16Ds / C4F32
};
ErikaPresenterHandle *p = erika_presenter_create_with_config(cfg);
if (!p) { /* erika_last_error_message() を読む */ }
```

`erika_presenter_create()` は既定値（SDR、アップスケーラ無し）。神経輝度アップスケーラは
Apple platform では Metal、Android を含む compute-capable Vulkan/wgpu adapter では
wgpu、Windows では D3D11 compute が実行します。compute 非対応 backend（主に GLES 3.0）は
native luma sampling を維持し、`Inactive` status と fallback reason を明示します。

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
- **アップスケーラ:** `set_upscaler(mode)`。`get_upscaler_status` で確認。
- **出力:** `erika_presenter_get_output_status` で active mode と fallback counter を確認。
  Android native host は `erika_presenter_set_output_headroom` で dynamic display ratio を
  publish します。`capture_frame_rgba` は display output が extended-linear の場合も、映像 +
  字幕（弾幕は含みません）を SDR RGBA8 screenshot として返します。

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
