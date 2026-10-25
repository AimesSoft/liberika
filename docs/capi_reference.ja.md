# liberika C ABI リファレンス

本書は `erika_capi` が公開する安定 C ABI を説明します。宣言は
[`crates/erika_capi/include/erika.h`](../crates/erika_capi/include/erika.h) にあります。
この ABI は Rust 以外のすべてのホスト（C、C++、Swift、Dart FFI、Win32 …）の唯一の
組み込み面です。Rust 組み込み側は `erika` crate を直接使ってください。本層は FFI の
ためだけに存在します。

組み込みの流れ（surface attach、レンダーループ、解放）は
[integration.ja.md](integration.ja.md) を、高レベル設計は
[architecture.ja.md](architecture.ja.md) を参照してください。

> 英語版：[capi_reference.md](capi_reference.md)。

## 2 つの handle ファミリー

liberika は独立した 2 つのエントリーポイントを公開します。組み込みごとに一方を選びます。

| Handle | モデル | 描画担当 | 用途 |
|--------|--------|----------|------|
| `ErikaHandle` | **プル** | ホスト | レンダーループを自分で持ち、デコード済みフレームを pull / 独自コンポジタを駆動する。 |
| `ErikaPresenterHandle` | **プッシュ** | liberika | native surface を liberika に渡し、表示フレームごとに `render_tick` を 1 回呼ぶ。liberika が decode・timing・audio・overlay・presentation を所有する。 |

`ErikaPresenterHandle` が推奨パスで、Flutter plugin と native demo もこれを使います。
コンパイルされるのは **macOS / iOS / tvOS / Windows / Android / Linux / HarmonyOS**。surface attach は
platform ごとに異なり、Apple は Metal、Windows は HWND/D3D11、Android/HarmonyOS は
wgpu/Vulkan surface を使います。HarmonyOS Flutter bridge は ArkTS platform channel が
構造化値を serialize して渡すため、presenter の JSON helper を使います。未対応 target では
`erika_presenter_create` が export されても `NULL` を返す場合があるため、platform guard と
create 成功の両方を確認してください。

2 つのファミリーは状態を共有しません。1 プロセスで両方使えますが、1 つのメディア
セッションは正確に 1 つの handle に属します。

### Linux Flutter のフレーム読み戻し

```c
ErikaStatus erika_presenter_copy_flutter_frame_rgba(
    ErikaPresenterHandle *handle, uint8_t *out_rgba, uintptr_t capacity,
    uint32_t *out_width, uint32_t *out_height);
```

Linux は wgpu で X11/Wayland に対応します。`erika_presenter_render_tick` の後に
`erika_presenter_copy_flutter_frame_rgba` を呼ぶと、字幕・弾幕・HUD を含む最新の合成
フレームを取得して消費します。呼び出し側が幅 × 高さ × 4 バイト以上のメモリを確保し、
成功時に密に並んだ RGBA8 と物理サイズを受け取ります。free 関数は不要です。
未生成なら `ErikaStatus_NoEvent`、容量不足ならエラーを返します。容量不足でもフレーム
は消費されるため、次の tick を待って再試行します。同じ presenter の操作は直列化して
ください。この経路は CPU メモリをコピーし、ゼロコピーではありません。

## GIF エクスポート

```c
typedef enum ErikaGifExportQuality {
  ErikaGifExportQuality_Normal = 0,
  ErikaGifExportQuality_High = 1,
} ErikaGifExportQuality;

typedef struct ErikaGifExportOptions {
  const char *input_uri;
  const char *output_path;
  uint64_t start_millis;
  uint64_t end_millis;
  uint32_t frames_per_second;
  uint32_t output_width;
  uint32_t output_height;
  int32_t quality;
  int32_t loop_count;
  bool overwrite;
  const ErikaHttpHeader *headers;
  uintptr_t header_count;
  uint64_t http_read_ahead_bytes;
  uint64_t reserved[3];
} ErikaGifExportOptions;

typedef struct ErikaGifExportResult {
  uint32_t width;
  uint32_t height;
  uint64_t frame_count;
  uint64_t file_size;
} ErikaGifExportResult;
```

```c
ErikaStatus erika_export_gif( const ErikaGifExportOptions *options, ErikaGifExportResult *out_result);
```

handle を必要としない同期処理です。UI では worker から呼びます。時刻はミリ秒、サイズは出力ピクセルです。
品質 Normal は bilinear、High は Lanczos。loop_count は -1 がループなし、0 が無限です。

```c
ErikaStatus erika_export_frame(const ErikaFrameExportOptions *options, ErikaFrameExportResult *out_result);
void erika_frame_export_result_free(ErikaFrameExportResult *result);
```

`position_millis` の時刻に表示される 1 枚（その時刻以前の最後のフレーム）を、独立した demuxer と
マルチスレッドのソフトウェアデコーダで取り出し、RGBA（1 行 `width * 4` バイト、alpha 255）または
PNG（8 bit RGB）で返します。handle 不要の同期処理なので UI では worker から呼びます。
`max_width`/`max_height` は上限（0 は無制限）で、表示アスペクト比を保ち、動画より大きくはしません。
`data` は `erika_frame_export_result_free` まで liberika が所有します。HDR ソース（PQ、HLG、Dolby Vision）は
Erika のレンダラが SDR ディスプレイ向けに使う演算（ITU-R BT.2390 EETF、目標 203 nit、BT.2020 から BT.709 への
gamut mapping、gamma 2.2）で SDR にトーンマッピングし、`result.reserved_flags` に
`ERIKA_FRAME_EXPORT_TONE_MAPPED` を立てます。SDR ソースは通常の色変換のままです。

複数枚を取るときは `ErikaFrameExportSession` を一度だけ開き、1 枚ずつ要求します。demuxer とデコーダは開いたままです。

```c
ErikaStatus erika_frame_export_session_open(const ErikaFrameExportSessionOptions *options, ErikaFrameExportSession **out_session);
ErikaStatus erika_frame_export_session_frame(ErikaFrameExportSession *session, const ErikaFrameExportRequest *request, ErikaFrameExportResult *out_result);
void erika_frame_export_session_close(ErikaFrameExportSession *session);
```

前回のデコード位置と今回の要求の間にキーフレームがなければ続きをデコードし（demuxer の seek index で判定。index がなければ
2 秒以内、または見たキーフレーム間隔の最大値の半分以内）、あれば直前のキーフレームへ seek します。順序は任意ですが、
時刻の昇順が最も安価です。結果・サイズ・形式・トーンマッピングは `erika_export_frame` と同じです。失敗した要求の後も
セッションは使えます。同一セッションの呼び出しは直列化され、すべて同期です。`erika_frame_export_session_close` は
`NULL` を受け付け、実行中の呼び出しがあってはならず、閉じた後に呼んでもいけません。

## 規約

### ステータスコード

失敗しうる呼び出しはすべて `ErikaStatus` を返します。

| 値 | コード | 意味 |
|----|--------|------|
| `ErikaStatus_Ok` | 0 | 成功。スレッドローカルのエラーをクリア。 |
| `ErikaStatus_NullPointer` | 1 | 必須の handle / out ポインタが `NULL`（または surface ポインタが 0）。 |
| `ErikaStatus_InvalidUtf8` | 2 | `const char*` 引数が不正な UTF-8。 |
| `ErikaStatus_PlayerError` | 3 | エンジンが呼び出しを拒否。メッセージを読む（下記）。 |
| `ErikaStatus_Panic` | 4 | 境界で Rust panic を捕捉。panic 前に完了した分を除き呼び出しは無効。handle は疑わしい状態とみなす。 |
| `ErikaStatus_NoEvent` | 5 | `*_poll_event` のみ：キューが空（エラーではない）。 |

戻り値は必ず確認してください。非エラーは `Ok` と `NoEvent` だけです。

### panic 安全性

ABI は FFI 境界を unwind が越えることを決して許しません。各エントリーは本体を
`catch_unwind` で包み、panic は `ErikaStatus_Panic` になりメッセージが設定されます。
C++ の `noexcept`/SEH を気にせず境界を越えて呼べます。

### エラーメッセージ（スレッドローカル）

`Ok`/`NoEvent` 以外の結果では、liberika は可読メッセージを**スレッドローカル**スロットに
保存します。取得：

```c
char *msg = erika_last_error_message();   // ヒープ確保、NULL の可能性あり
if (msg) { fprintf(stderr, "erika: %s\n", msg); erika_string_free(msg); }
```

スレッドローカルなので、失敗した呼び出しを行った**同じスレッド**で、かつそのスレッドの
次の呼び出しより前に読んでください（その後の `Ok` がクリアします）。
`erika_last_error_message` は所有権を持つコピーを返します——`erika_string_free` で解放。

### 文字列の所有権

liberika が返す `char*` はすべてヒープ確保で呼び出し側の所有です。

- 単独の文字列（例：`erika_last_error_message`）→ `erika_string_free` で解放。
- `ErikaTrackInfo` 内の文字列 → `erika_track_info_free(&track)` でレコード全体を解放
  （内部文字列をすべて解放）。
- `ErikaDanmakuTrackInfo` 内の文字列 → `erika_danmaku_track_info_free(&track)` で解放。

これらを libc の `free()` で解放しないでください。常に対応する liberika の解放関数を使い、
確保が同じ allocator 上で ABI を越えるようにします。

渡す `const char*` 引数は呼び出し中のみ借用され、liberika は必要分をコピーします。NUL
終端の UTF-8 でなければなりません。

### カウント配列イディオム

リスト getter（`erika_tracks`、`erika_presenter_tracks`、
`erika_presenter_danmaku_tracks`）は呼び出し側確保のバッファを使います。

```c
size_t total = 0;
erika_presenter_tracks(p, NULL, 0, &total);          // 1) 件数を問い合わせ
ErikaTrackInfo *buf = calloc(total, sizeof *buf);
erika_presenter_tracks(p, buf, total, &total);        // 2) 充填
for (size_t i = 0; i < total; i++) { /* buf[i] を使用 */ }
for (size_t i = 0; i < total; i++) erika_track_info_free(&buf[i]);
free(buf);
```

`out_len` は**常に**利用可能なレコードの総数に設定されます。書き込まれるのは最大
`capacity` 件。`capacity == 0`（`NULL` バッファ付き）はサイズ取得の正式な方法です。
実際に書き込まれたレコードのみ解放すべき文字列を保持します。

### Surface ジオメトリと scale

`attach_*` と `resize_surface` の `width`/`height` は**物理ピクセル**、`scale` は
backing/DPI 係数（Retina の `2.0`、Windows のモニタ倍率など）です。surface ポインタが
`0` だと `NullPointer` で拒否されます。

### スレッド

単一 handle は**内部同期されません**。同じ handle を複数スレッドから同時に呼ばないで
ください。自分で直列化する（または handle を 1 スレッドに閉じ込める）こと。presenter の
`render_tick` は表示タイマー / surface を持つスレッドから駆動します。異なる handle は
異なるスレッドで独立です。エラーメッセージはスレッドローカルである点に注意。

## `ErikaHandle` —— プルモデル

ホストが自分で描画を駆動し、状態/イベントを pull します。

### ライフサイクル

```c
ErikaHandle *erika_create(void);
void         erika_destroy(ErikaHandle *handle);
char        *erika_last_error_message(void);   // スレッドローカル、呼び出し側が解放
void         erika_string_free(char *value);
```

`erika_create` は失敗しません（有効な handle を返す）。`erika_destroy(NULL)` は no-op。
handle の破棄は再生を止め全リソースを解放します。

### 再生制御

```c
ErikaStatus erika_open(ErikaHandle *handle, const char *uri);   // ファイルパスまたは URL
ErikaStatus erika_open_with_headers(ErikaHandle *handle, const char *uri,
                                    const ErikaHttpHeader *headers, uintptr_t header_count);
ErikaStatus erika_open_with_options(ErikaHandle *handle, const char *uri,
                                    const ErikaOpenOptions *options);
ErikaStatus erika_play(ErikaHandle *handle);
ErikaStatus erika_pause(ErikaHandle *handle);
ErikaStatus erika_stop(ErikaHandle *handle);
ErikaStatus erika_close(ErikaHandle *handle);
ErikaStatus erika_seek(ErikaHandle *handle, uint64_t position_micros);
```

`uri` はローカルパスまたは HTTP(S) URL。`erika_open_with_headers` は HTTP(S) 再生用の
header を設定します。`headers` は呼び出し中だけ読み取られ、戻り値の後に解放できます。
`header_count` が 0 より大きい場合、`headers` は NULL にできません。header は HEAD、Range
GET、prefetch request に使用されます。
認証情報と Cookie は liberika の log に書き込まれません。`seek` はマイクロ秒。

`erika_open_with_options` は `erika_open_with_headers` の上位互換で、header 配列と
リクエスト単位のチューニングを `ErikaOpenOptions` 構造体にまとめて渡します。
`options` に NULL を指定するとすべて既定値になります。`http_read_ahead_bytes` は
このリクエストの HTTP(S) 先読みウィンドウ（バイト）を上書きします。`0` は
プロセス全体の環境変数 `ERIKA_HTTP_READAHEAD_BYTES` があればその値を使い、
なければ 2 MiB の既定値を使います。明示的な非ゼロ値は環境変数より優先されます。
`http_back_buffer_bytes` は HTTP(S) のリワインドバジェット（バイト）を上書きします。
これは再生位置より後に保持される既読データ量で、この範囲内の巻き戻しはネットワーク
要求なしに処理されます。`0` は 16 MiB の既定値。高ビットレートのメディアでは
ビットレートから値を決めるのが推奨です（約 15 秒 × ビットレート。71 Mbps での
-10 秒は約 89 MB に相当）。
`reserved` の非ゼロ値は拒否され、
将来のフィールド追加が古いホストの動作を黙って変えないようになっています。
これらのパラメータは HTTP(S) 再生にのみ影響し、ローカルファイルには無効です。

```c
typedef struct ErikaOpenOptions {
  const ErikaHttpHeader *headers;
  uintptr_t header_count;
  uint64_t http_read_ahead_bytes;   /* 0 = 環境変数、なければ 2 MiB */
  uint64_t http_back_buffer_bytes;  /* 0 = 16 MiB のリワインドバジェット */
  uint64_t reserved[2];             /* ゼロでなければなりません */
} ErikaOpenOptions;
```

`open` は同期的にストリームを調査して `Ready` に遷移し、`play` は非同期に
キューへ投入されます。ブロックする open はホスト UI スレッド以外で実行し、
同じ handle への呼び出しはすべて直列化してください。以後の再生状態は
`StateChanged`、`DurationChanged`、`Error` イベントで確認します。

### トラックと字幕

```c
ErikaStatus erika_add_external_subtitle(ErikaHandle *, const char *uri, int64_t *out_track_id);
ErikaStatus erika_remove_subtitle_track(ErikaHandle *, int64_t track_id);
ErikaStatus erika_select_audio_track(ErikaHandle *, int64_t track_id);
ErikaStatus erika_select_subtitle_track(ErikaHandle *, int64_t track_id);
ErikaStatus erika_track_selection(ErikaHandle *, ErikaTrackSelection *out_selection);
ErikaStatus erika_tracks(ErikaHandle *, ErikaTrackInfo *out_tracks, uintptr_t capacity, uintptr_t *out_len);
void        erika_track_info_free(ErikaTrackInfo *track);
```

`erika_tracks` はカウント配列イディオムに従います。`erika_track_selection` は現在選択中の
video/audio/subtitle トラック id（`-1` は無し）を報告します。字幕トラック id `-1` の選択で
字幕が無効になります。

### 状態とイベント

```c
ErikaStatus erika_state(ErikaHandle *, ErikaState *out_state);
ErikaStatus erika_poll_event(ErikaHandle *, ErikaEvent *out_event);
```

`erika_poll_event` は非ブロッキングで、キューが空なら `NoEvent` を返します。ループ内で
ドレインしてください。[イベント](#イベント) 参照。

### Surface attach（ホスト管理）

```c
ErikaStatus erika_attach_metal_layer(ErikaHandle *, uint64_t raw_layer, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_attach_wgpu_surface(ErikaHandle *, ErikaWgpuSurfaceKind kind,
                                      uint64_t raw_window, uint64_t raw_display,
                                      uint32_t w, uint32_t h, double scale);
ErikaStatus erika_attach_wgpu_surface_with_output_capabilities(
                                      ErikaHandle *, ErikaWgpuSurfaceKind kind,
                                      uint64_t raw_window, uint64_t raw_display,
                                      uint32_t w, uint32_t h, double scale,
                                      ErikaSurfaceOutputCapabilities capabilities);
ErikaStatus erika_attach_flutter_texture(ErikaHandle *, ErikaFlutterTextureKind kind,
                                         int64_t texture_id, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_detach_surface(ErikaHandle *);
```

`raw_layer` は `CAMetalLayer*` を `uint64_t` にキャストしたもの。
`erika_attach_wgpu_surface` では `raw_window`/`raw_display` がその `kind` のプラット
フォームの window/display ハンドル（例：`WindowsHwnd` なら `HWND` + `HINSTANCE`、
`XlibWindow` なら xcb/Xlib window + display）。`erika_attach_flutter_texture` は外部
テクスチャ id をプラットフォーム texture registrar に登録します。Android native host
が direct-composited かつ HDR-eligible な `SurfaceView` を宣言する場合は
`_with_output_capabilities` variant が必要です。短い関数は all-false/default
capabilities を渡すため Android extended-linear output は active になりません。

## `ErikaPresenterHandle` —— プッシュモデル

liberika がフルスタックを所有し、ホストは surface を提供して `render_tick` を呼びます。
**macOS / iOS / tvOS / Windows / Android / HarmonyOS / Linux。**

### ライフサイクルと設定

```c
ErikaPresenterHandle *erika_presenter_create(void);
ErikaPresenterHandle *erika_presenter_create_with_config(ErikaPresenterConfig config);
ErikaPresenterHandle *erika_presenter_create_with_output_mode(int32_t output_mode, float edr_headroom);
ErikaPresenterHandle *erika_presenter_create_with_output_mode_and_alpha(int32_t output_mode, float edr_headroom,
                                                                        int32_t video_alpha_mode);
void                  erika_presenter_destroy(ErikaPresenterHandle *handle);
```

v0.1.8 の `ErikaPresenterConfig` は 4 フィールド、16 bytes です。v0.1.7 は
3 フィールド、12 bytes でした。値渡しする構造体のため、この間にバイナリ互換性は
ありません。C/C++ の呼び出し側と手書き Swift/FFI mirror は対応するヘッダーと
native library で再ビルドし、不透明 video では `video_alpha_mode` を `0` にします。

`ErikaPresenterConfig` は出力モード（`Sdr`、Apple `AppleEdr`、Android
`ExtendedLinear`）、requested EDR/scRGB content-headroom ceiling、初期輝度アップスケーラ、そして
`video_alpha_mode`（`ErikaVideoAlphaMode`）を選びます。
Android `ExtendedLinear` は FP16 extended-linear scRGB で HDR10/PQ ではありません。
`create_with_output_mode` と `create_with_output_mode_and_alpha` は短縮形、`create` は既定値
（SDR、不透明 video、アップスケーラ無し）。
`NULL` が返れば作成失敗——`erika_last_error_message` を確認。

### 再生とランタイムパラメータ

```c
ErikaStatus erika_presenter_open(ErikaPresenterHandle *, const char *uri);
ErikaStatus erika_presenter_open_with_headers(ErikaPresenterHandle *, const char *uri,
                                              const ErikaHttpHeader *headers,
                                              uintptr_t header_count);
ErikaStatus erika_presenter_open_with_options(ErikaPresenterHandle *, const char *uri,
                                              const ErikaOpenOptions *options);
ErikaStatus erika_presenter_play(ErikaPresenterHandle *);
ErikaStatus erika_presenter_pause(ErikaPresenterHandle *);
ErikaStatus erika_presenter_stop(ErikaPresenterHandle *);
ErikaStatus erika_presenter_close(ErikaPresenterHandle *);
ErikaStatus erika_presenter_seek(ErikaPresenterHandle *, uint64_t position_micros);
ErikaStatus erika_presenter_set_playback_rate(ErikaPresenterHandle *, double rate);
ErikaStatus erika_presenter_set_volume(ErikaPresenterHandle *, double volume);   // 0.0–1.0
ErikaStatus erika_presenter_set_upscaler(ErikaPresenterHandle *, int32_t mode);  // ErikaLumaUpscalerMode
ErikaStatus erika_presenter_set_subtitle_scale(ErikaPresenterHandle *, double scale);
ErikaStatus erika_presenter_set_subtitle_font(ErikaPresenterHandle *, const char *family, const char *file_path);
ErikaStatus erika_presenter_set_subtitle_style(ErikaPresenterHandle *, ErikaSubtitleStyle style);
ErikaStatus erika_presenter_set_output_headroom(ErikaPresenterHandle *, float headroom, bool known);
```

`set_playback_rate(1.0)` が通常速度。`erika_presenter_open_with_options` は
`erika_open_with_options` の push モデル版で、同じ `ErikaOpenOptions`
（header と `http_read_ahead_bytes` / `http_back_buffer_bytes`）を受け付けます
（[`erika_open_with_options`](#erikahandle--プルモデル) 参照）。`set_upscaler` はランタイムで神経輝度アップ
スケーラを切り替えます（[`erika_presenter_get_upscaler_status`](#診断とスクリーンショット)
参照）。Metal、feature level 11.0+ の D3D11、compute-capable な wgpu renderer は ArtCNN を実行し、
それ以外の backend は native luma sampling を維持して `Inactive` fallback を明示します。

`set_subtitle_font` と `set_subtitle_style` は字幕の **fallback** の見た目を設定します。
family や path が空ならその指定を解除します。色は `0xRRGGBBAA` で、既定は不透明な白の
文字（`0xFFFFFFFF`）と半透明な黒の縁取り（`0x0000007F`）です。`font_size`（既定 `48`、
`8..400` にクランプ）と `outline_width`（既定 `2`、`0..32` にクランプ）は ASS script 単位で、
`set_subtitle_scale` がさらに掛かります。container の ASS script は自身の styling を保ち、
これらは script が指定していない部分、system が解決できない font、そして plain-text
（SRT/WebVTT）字幕の見た目を埋めるだけです。

`ErikaSubtitleStyle` が見た目の全体を運びます。範囲外の値は拒否せずクランプされます：

```c
typedef struct ErikaSubtitleStyle {
  const char *font_family;      /* NULL または空で platform 既定に戻す */
  const char *font_file_path;
  uint32_t primary_color_rgba;  /* 0xRRGGBBAA */
  uint32_t outline_color_rgba;
  double font_size;             /* 8..400   */
  double outline_width;         /* 0..32    */
  bool bold, italic, underline, strike_out;
  double spacing;               /* -100..100 */
  double scale_x_percent;       /* 1..1000、100 が等倍 */
  double scale_y_percent;
  int32_t border_style;         /* 1 = 縁取り+影、3 = 不透明ボックス */
  double shadow_depth;          /* 0..32 */
  double blur;                  /* 0..100 */
  int32_t alignment;            /* 1..9、テンキー配置、2 = 下中央 */
  int32_t margin_left, margin_right, margin_vertical;
  uint32_t override_mask;
} ErikaSubtitleStyle;
```

`override_mask` は、どの field を fallback から libass の selective style override
に昇格させ、ASS dialogue が要求する styling を置き換えるかを決めます。`erika.h` の
`ERIKA_SUBTITLE_OVERRIDE_*` マクロの bitmask で、libass の `ASS_OVERRIDE_BIT_*` に
対応します：

| マクロ | 置き換える field |
| --- | --- |
| `ERIKA_SUBTITLE_OVERRIDE_FONT_SIZE_FIELDS` | `font_size`、`spacing`、`scale_x_percent`、`scale_y_percent` |
| `ERIKA_SUBTITLE_OVERRIDE_FONT_NAME` | `font_family` |
| `ERIKA_SUBTITLE_OVERRIDE_COLORS` | `primary_color_rgba`、`outline_color_rgba` |
| `ERIKA_SUBTITLE_OVERRIDE_ATTRIBUTES` | `bold`、`italic`、`underline`、`strike_out` |
| `ERIKA_SUBTITLE_OVERRIDE_BORDER` | `border_style`、`outline_width`、`shadow_depth` |
| `ERIKA_SUBTITLE_OVERRIDE_ALIGNMENT` | `alignment` |
| `ERIKA_SUBTITLE_OVERRIDE_MARGINS` | `margin_left`、`margin_right`、`margin_vertical` |
| `ERIKA_SUBTITLE_OVERRIDE_BLUR` | `blur` |
| `ERIKA_SUBTITLE_OVERRIDE_ALL` | 上記すべて |

`0` ならすべて fallback のままで、未知の bit は捨てられます。override 時もサイズは
解像度に依存せず、`font_size` が `48` なら script の `PlayResY` に関係なく同じ
pixel になります。ただし margin は違います：libass は override の margin を script
自身の単位のまま扱うため、container の ASS track ではその script の `PlayResY` に
比例し、plain-text 字幕（liberika が frame サイズで script を生成する）では pixel に
なります。

`ErikaHttpHeader` は次のように定義されます：

```c
typedef struct ErikaHttpHeader {
  const char *name;
  const char *value;
} ErikaHttpHeader;
```

例：

```c
ErikaHttpHeader headers[] = {
    {"Authorization", "Bearer token"},
    {"Referer", "https://example.com/"},
};
erika_presenter_open_with_headers(presenter, "https://example.com/video.mp4",
                                  headers, 2);
```

header は HTTP(S) source にだけ適用され、local file と Android の `content://` source では
無視されます。呼び出し側は URI、header 名、値を呼び出し中有効なまま保持してください。
この API はこれらの文字列の所有権を取得しません。

player 自身が生成する header は merge されず reject されます。下層の HTTP client は重複を
置き換えず追加するためです：`Range`、`Host`、`Content-Length`、`Transfer-Encoding`、
`Connection`（大文字小文字を区別せず一致）はいずれも呼び出しを
`ERIKA_STATUS_PLAYER_ERROR` で失敗させます。HTTP token として不正な header 名や、field
value に使えない文字を含む値も同様です。検証は最初の range request ではなく `open` の
時点で行われます。

header が適用されるのは media source だけです。外部 subtitle track と danmaku sidecar は
まだ header なしで取得されるため、video を認証する token はこれらの URL にはまだ適用
されません。

`set_output_headroom` は display の current HDR/SDR ratio を publish します。Android API 34+
host は `Display.registerHdrSdrRatioChangedListener` から呼び、valid ratio は `known = true`、
measurement unavailable は `(1.0f, false)` を渡します。値は `1.0..10000.0` に sanitize
されます。wgpu は duplicate state を無視し、surface reattach 無しで後続 frame target を
更新し、known state または ratio が実際に変わった場合だけ `headroom_updates` を増やします。
effective content target は configured content ceiling、正の surface `desired_headroom`、known
display ratio で制限されます。他 renderer はこの advisory update を無視できます。

### トラックと字幕

```c
ErikaStatus erika_presenter_add_external_subtitle(ErikaPresenterHandle *, const char *uri, int64_t *out_track_id);
ErikaStatus erika_presenter_remove_subtitle_track(ErikaPresenterHandle *, int64_t track_id);
ErikaStatus erika_presenter_select_audio_track(ErikaPresenterHandle *, int64_t track_id);
ErikaStatus erika_presenter_select_subtitle_track(ErikaPresenterHandle *, int64_t track_id);
ErikaStatus erika_presenter_track_selection(ErikaPresenterHandle *, ErikaTrackSelection *out_selection);
ErikaStatus erika_presenter_tracks(ErikaPresenterHandle *, ErikaTrackInfo *out_tracks, uintptr_t capacity, uintptr_t *out_len);
```

`ErikaHandle` のトラック関数と同じ意味論です。

`ErikaTrackInfo` の `codec`、`width`、`height`、`pixel_format`、`profile`、`level` は
track metadata です。`bit_rate` の単位は bit/s、`frame_rate_numerator` /
`frame_rate_denominator` は video track の有理 frame rate です。`0` は unknown を表します。
frame rate は average frame rate、`r_frame_rate`、FFmpeg guessed frame rate の順に probe されます。
bitrate は track 自身の parameter を優先し、単一 video track の bitrate がなく、container total と
他の全 audio track bitrate が既知の場合のみ、container bitrate から audio bitrate を引いて推定します。
どちらも瞬間 bitrate や real-time render FPS ではなく、推定 bitrate には container overhead や
non-audio stream が含まれる場合があります。

### 音声・リソース・メモリフォントの補足

```c
ErikaStatus erika_presenter_audio_only_tick( ErikaPresenterHandle *handle, ErikaPresenterStats *out_stats);
ErikaStatus erika_presenter_clear_subtitle_memory_fonts(ErikaPresenterHandle *handle);
ErikaStatus erika_presenter_get_resource_status( ErikaPresenterHandle *handle, ErikaPresenterResourceStatus *out_status);
ErikaStatus erika_presenter_get_subtitle_memory_font_info( ErikaPresenterHandle *handle, uint64_t font_id, ErikaSubtitleMemoryFontInfo *out_info);
ErikaStatus erika_presenter_get_subtitle_memory_font_status( ErikaPresenterHandle *handle, ErikaSubtitleMemoryFontStatus *out_status);
ErikaStatus erika_presenter_register_subtitle_memory_font( ErikaPresenterHandle *handle, const uint8_t *data, uintptr_t data_len, uint64_t *out_font_id);
ErikaStatus erika_presenter_select_subtitle_memory_fonts( ErikaPresenterHandle *handle, const uint64_t *font_ids, uintptr_t font_count);
void erika_subtitle_memory_font_info_free(ErikaSubtitleMemoryFontInfo *info);
void erika_subtitle_memory_font_status_free( ErikaSubtitleMemoryFontStatus *status);
```

`audio_only_tick` は映像なしの音声処理、`get_resource_status` はリソース情報を返します。
メモリフォントは登録した ID を順序付きで選択します。状態・情報の割り当ては対応する `_free` で解放します。

`ErikaSubtitleMemoryFontFace` は face index、family JSON、PostScript name、weight、italic、monospaced を持ちます。`ErikaSubtitleMemoryFontInfo.faces` と内部の string は `erika_subtitle_memory_font_info_free` でまとめて解放します。

### 弾幕（ダンマク）

```c
ErikaStatus erika_presenter_load_danmaku_file(ErikaPresenterHandle *, const char *uri);
ErikaStatus erika_presenter_load_danmaku_json(ErikaPresenterHandle *, const char *json);
ErikaStatus erika_presenter_add_danmaku_track_file(ErikaPresenterHandle *, const char *uri, const char *name, int64_t offset_micros, uint64_t *out_track_id);
ErikaStatus erika_presenter_add_danmaku_track_json(ErikaPresenterHandle *, const char *json, const char *name, int64_t offset_micros, uint64_t *out_track_id);
ErikaStatus erika_presenter_remove_danmaku_track(ErikaPresenterHandle *, uint64_t track_id);
ErikaStatus erika_presenter_set_danmaku_track_enabled(ErikaPresenterHandle *, uint64_t track_id, bool enabled);
ErikaStatus erika_presenter_set_danmaku_track_offset(ErikaPresenterHandle *, uint64_t track_id, int64_t offset_micros);
ErikaStatus erika_presenter_set_danmaku_global_offset(ErikaPresenterHandle *, int64_t offset_micros);
ErikaStatus erika_presenter_danmaku_tracks(ErikaPresenterHandle *, ErikaDanmakuTrackInfo *out_tracks, uintptr_t capacity, uintptr_t *out_len);
ErikaStatus erika_presenter_clear_danmaku(ErikaPresenterHandle *);
ErikaStatus erika_presenter_set_danmaku_enabled(ErikaPresenterHandle *, bool enabled);
ErikaStatus erika_presenter_set_debug_hud_enabled(ErikaPresenterHandle *, bool enabled);
ErikaStatus erika_presenter_set_danmaku_config(ErikaPresenterHandle *, ErikaDanmakuConfig config);
ErikaStatus erika_presenter_set_danmaku_config_ptr(ErikaPresenterHandle *, const ErikaDanmakuConfig *config);
ErikaStatus erika_presenter_get_danmaku_config(ErikaPresenterHandle *, ErikaDanmakuConfig *out_config);
ErikaStatus erika_presenter_set_danmaku_font(ErikaPresenterHandle *, const char *family, const char *file_path);
ErikaStatus erika_presenter_set_danmaku_block_words_json(ErikaPresenterHandle *, const char *json);
```

`load_danmaku_*` は弾幕を 1 つの匿名トラックで置き換え、`add_danmaku_track_*` は
マルチトラックリスト（各トラックに名前と時間オフセット）を構築します。入力は Bilibili
XML（`*_file`、パス/URL）または JSON（`*_json`、インライン）。`offset_micros` は 1 つの
トラックのタイムラインをずらし、global offset は全トラックをずらします。
`set_danmaku_config` / `_ptr` は完全な `ErikaDanmakuConfig` を適用（`_ptr` 版は構造体の
値渡しを避ける）、`get_danmaku_config` で読み戻します。レイアウトエンジンは
[danmaku_architecture.md](danmaku_architecture.md) を参照。
`set_danmaku_block_words_json` はフィルタ用の文字列 JSON 配列を取ります。

inline JSON の root は item array、または `comments`、`danmaku`、`items` の
いずれかの array を持つ object です。各 item は次の field を受け取ります
（括弧内は alias）。

- `content`（`text`、`c`）：本文。欠落または空白だけの item は skip されます。
- `time`（`t`）：表示時刻（秒）。既定値は `0` です。
- `type`（`mode`、`y`）：`scroll`/`1`、`bottom`/`4`、`top`/`5`、
  `reverse`/`6`、`special`/`7`。数値の `type_code` / `mode_code` も使えます。
- `color`（`r`）：decimal RGB、`#RRGGBB`、または `rgb(r,g,b)`。
- `font_size`（`fontSize`、`size`、`s`）、`opacity`（`alpha`、`a`）、
  `is_me`（`isMe`、`self`、`mine`）：任意の style / self-danmaku metadata。
- `id`：任意の unsigned 64-bit integer または decimal string。省略時は入力全体での
  item position が割り当てられます。session は別の内部 layout identity を生成するため、
  planner window 間の track 安定性のために host が business ID を合成する必要はありません。

未知の field は無視されるため、`cid` や `danmakuId` などの source field を含む
standard map もそのまま渡せます。

`set_debug_hud_enabled` は default で off です。on にすると Presenter は native video
composition に diagnostic HUD を描画します。HUD は track technical metadata、
playback state、decoded/rendered FPS、decode/zero-copy counters、render/audio state、HDR
output、danmaku item count を表示します。video frame がある時だけ表示され、host による
stats polling は不要で、`ErikaPresenterStats` にも書き込みません。off-screen
`capture_frame_rgba` は HUD を含みません。

### Surface とプレゼンテーション

```c
ErikaStatus erika_presenter_attach_metal_layer(ErikaPresenterHandle *, uint64_t raw_layer, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_attach_wgpu_surface(ErikaPresenterHandle *, ErikaWgpuSurfaceKind kind, uint64_t raw_window, uint64_t raw_display, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_attach_wgpu_surface_with_output_capabilities(ErikaPresenterHandle *, ErikaWgpuSurfaceKind kind, uint64_t raw_window, uint64_t raw_display, uint32_t w, uint32_t h, double scale, ErikaSurfaceOutputCapabilities capabilities);
ErikaStatus erika_presenter_attach_windows_hwnd(ErikaPresenterHandle *, uint64_t hwnd, uint64_t hinstance, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_resize_surface(ErikaPresenterHandle *, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_detach_surface(ErikaPresenterHandle *);
```

macOS/iOS/tvOS は `attach_metal_layer`（`CAMetalLayer*`）、Windows は `attach_windows_hwnd`
（`attach_wgpu_surface` + kind `WindowsHwnd` の便利ラッパで、`HWND` + `HINSTANCE` を渡す）。
surface に紐づくレンダラ backend（native Metal、native Direct3D 11、wgpu）は attach 呼び
出しではなく presenter 設定で決まります。drawable サイズや scale が変わったら
`resize_surface` を呼びます。

Android extended-linear では Flutter Hybrid Composition `SurfaceView` の
`AndroidNativeWindow` を `_with_output_capabilities` に渡します。display/surface HDR probe
成功時だけ `extended_linear` を、direct `SurfaceView` の場合だけ
`direct_composition = true` を設定し、host probe failure は `fallback_reason` に保持します。
`desired_headroom = 0` は system auto、正の値は surface ceiling で、API 35 per-`SurfaceView`
`setDesiredHdrHeadroom` に使用できます。
liberika は Vulkan、`Rgba16Float`、`ADATASPACE_SCRGB_LINEAR` も検証します。どれかが失敗
すると SDR に fallback し、その理由を取得できます。

### Flutter texture surface

```c
ErikaStatus erika_presenter_attach_flutter_texture(ErikaPresenterHandle *, ErikaFlutterTextureKind kind,
                                                   int64_t texture_id, uint32_t w, uint32_t h, double scale);
ErikaStatus erika_presenter_set_flutter_texture_buffer(ErikaPresenterHandle *, uint64_t raw_texture,
                                                       uint32_t w, uint32_t h);
```

`attach_flutter_texture` は `texture_id` で識別される texture-registrar surface に
presenter を紐づけます（現在は Apple の
`MacOsTextureRegistrar` / `IosTextureRegistrar`）。pixel buffer は host が所有します。**毎**
`render_tick` の前に `set_flutter_texture_buffer` で次フレームの GPU target を選択し、
`uint64_t` にキャストした `id<MTLTexture>` ポインタを渡します。`BGRA8Unorm` で宣言された
`w`×`h` と一致している必要があります。texture はそのフレームの間だけ借用され、所有権は
host 側に残るため、`render_tick` が返れば再利用も解放も可能です。Flutter plugin が macOS で
使う `ErikaTextureVideoView` はこの surface です。

### Windows DirectComposition swap chain

```c
ErikaStatus erika_presenter_windows_composition_swapchain_iunknown(ErikaPresenterHandle *, void **out_swapchain);
```

Windows のみ。presenter が `attach_wgpu_surface_with_output_capabilities`
（`direct_composition = true`）で attach され、透明 video alpha mode または overlay
blend で再生する場合、liberika は対象 HWND 向けに premultiplied alpha の composition
swap chain を作成します。この getter は **AddRef 済みの `IUnknown*`** として返します。
返された COM reference の所有者は呼び出し側で、`Release` が必須です。decoder や device
喪失の後は再取得してください——liberika は swap chain を再構築し、新しいオブジェクトを
公開します。ポインタが同じなら再構築はありません。

### Windows Flutter texture

```c
ErikaStatus erika_presenter_windows_flutter_texture_iunknown(ErikaPresenterHandle *, void **out_texture);
```

Windows 専用で、他の platform ではこの symbol は export されません。最新の完了済みで不変な SDR Flutter GPU frame を **AddRef 済みの `IUnknown*`** として返します。呼び出し側が参照を所有し、`Release` が必要です。frame の使用中は参照を保持してください。texture の内容はその寿命中に変更されません。出力ポインターが null なら `NullPointer`、有効な presenter に完了済み Flutter texture がなければ `PlayerError` を返して出力を null にします。frame を要求する前に描画を駆動してください。

### レンダーループとイベント

```c
ErikaStatus erika_presenter_render_tick(ErikaPresenterHandle *, double time_seconds, ErikaPresenterStats *out_stats);
ErikaStatus erika_presenter_get_stats(ErikaPresenterHandle *, ErikaPresenterStats *out_stats);
ErikaStatus erika_presenter_poll_event(ErikaPresenterHandle *, ErikaEvent *out_event);
```

表示フレームごとに `render_tick` を 1 回呼びます（`CADisplayLink`、`CVDisplayLink`、
Windows のフレームスケジューラなど）。`time_seconds` はそのフレームのホスト表示クロック
（秒）——liberika は vsync 量子化スケジューリングに使うので、wall-clock 差分ではなく
**プレゼンテーションタイムスタンプ**を渡します。`out_stats` が非 `NULL` なら、パイプ
ラインカウンタのスナップショットが書き込まれます。`poll_event` は非ブロッキングで、
アイドル時は `NoEvent` を返します。

`get_stats` は同じ `ErikaPresenterStats` スナップショットを、フレームを描画せずに
書き込みます。ホストが表示ループとは別の周期でカウンタをサンプリングする場合に
使ってください。プレゼンテーションは進みません。

### 表示目標時刻のレンダリング

```c
ErikaStatus erika_presenter_render_tick_with_timing( ErikaPresenterHandle *handle, double time_seconds, const double *presentation_delay_seconds, ErikaPresenterStats *out_stats);
```

delay は呼び出し入口から表示目標までの秒数（±0.25 秒）。NULL は従来のサンプリングです。映像・字幕・弾幕は同じ目標を使い、再生クロックは変更しません。

### JSON ブリッジ

プラットフォームチャネルが既に構造化引数をシリアライズしている埋め込み側
（例えば HarmonyOS ArkTS プラグイン）向けに、同じ presenter 面が JSON でも
利用できます。

```c
char *erika_presenter_invoke_json(ErikaPresenterHandle *, const char *method,
                                  const char *arguments_json);
char *erika_presenter_render_tick_json(ErikaPresenterHandle *, double time_seconds);
char *erika_presenter_poll_event_json(ErikaPresenterHandle *);
```

返される文字列は liberika が所有するため、`erika_string_free` で解放してください。
`poll_event_json` はイベントが無いとき envelope ではなく `NULL` を返すので、
`NULL` はエラーではなくアイドルを意味します。

3 つとも結果を同じ envelope に包みます。

```json
{ "ok": true,  "status": 0, "value": <result> }
{ "ok": false, "status": 3, "error": "<message>" }
```

`arguments_json` は JSON オブジェクトである必要があります。`method` は
`crates/erika_capi/src/presenter_json.rs` の dispatch table（`JSON_METHODS`）の
1 項目を選びます。table は C エントリポイントに対応します: 再生（`open`、`play`、
`pause`、`stop`、`close`、`seek`、`setPlaybackRate`、`setVolume`）、トラックと字幕
（`tracks`、`trackSelection`、`selectAudioTrack`、`selectSubtitleTrack`、
`addExternalSubtitle`、`removeSubtitleTrack`、`setSubtitleScale`、`setSubtitleFont`、
`setSubtitleStyle`、字幕メモリフォント系）、danmaku 系（`getDanmakuConfig` を含む）、
出力とアップスケーラ（`setUpscaler`、`getUpscalerStatus`、`setOutputHeadroom`、
`getOutputStatus`、`setDebugHudEnabled`）、カウンタ（`getPresenterStats`、
`getResourceStatus`）、および後述の診断（`getDiagnosticsSnapshot`、`getLogs`）。

`erika_presenter_invoke_json` はエンジンへの汎用パススルーも兼ねます。

- `listMethods` は table 自体（`{schema: 1, fields, methods: [{name, scope, summary,
  arguments: [{name, type, required}], returns, blocking, writesFiles}]}`）を返すので、ホスト（とその先の
  エージェント）はコード変更なしに機能を発見・転送できます。機能の登録は `invoke` の
  `match` 分岐と `JSON_METHODS` の項目を追加するだけで、両者が食い違うと単体テストが
  失敗します。
- 各項目は `blocking`（呼び出しスレッド上で処理が終わってから返り、時間が入力（メディア長、ファイルサイズ）に応じて増える）と
  `writesFiles`（ディスク上にファイルを作り、結果にパスを列挙する）も持ち、最上位の `fields` が両方を説明します。
  `exportFrame`、`exportFrames` は上のフレーム書き出しでメディアから画像を取ります。`outputPath` / `outputDir` を
  指定すると PNG または RGBA ファイルを書いてパスを返し、`outputPath` なしの `exportFrame` は base64 のバイト列を返します。
- `scope` が `global` のメソッド（`listMethods`、`getLogs`、`exportFrame`、`exportFrames`）は `NULL` ハンドルを
  受け付けます。
- `invokeJson`（`{method, arguments}`）は table の別メソッドを呼び、その envelope 全体を
  `value` として返します。envelope をネイティブ側で外すプラグイン（Linux、HarmonyOS、
  Android）は Dart の `ErikaPlayer.invokeJson` をこれでそのまま転送します。
- 未知の method は `{"ok": false, "status": 3, "code": "unknown_method", "method":
  "<名前>", "error": "unsupported Erika presenter method: <名前>"}` を返し、
  `arguments_json` が不正なら `"code": "invalid_arguments"` が付きます。どちらも abort
  しません。

このブリッジは上の関数をそのまま呼ぶだけで、追加の機能はありません。構造体を渡せる
ホストは型付きエントリポイントを使っても構いません。

### 診断とスクリーンショット

```c
ErikaStatus erika_presenter_get_upscaler_status(ErikaPresenterHandle *, ErikaUpscalerStatus *out_status);
ErikaStatus erika_presenter_get_output_status(ErikaPresenterHandle *, ErikaOutputStatus *out_status);
ErikaStatus erika_presenter_capture_frame_rgba(ErikaPresenterHandle *, uint32_t width, uint32_t height,
                                               uint8_t *out_rgba, uintptr_t out_capacity);
```

`get_upscaler_status` は要求アップスケーラモード、現在の backend（off / inactive /
building / scalar / simdgroup-matrix）、フォールバック回数、アップスケール済みフレーム数、
直近の encode/GPU 時間（マイクロ秒）を報告します。

`get_output_status` は request だけでなく実際に negotiated された output を返します。
13 field は次のとおりです。

| Field | 意味 |
|-------|------|
| `requested_mode` | 作成時に要求した `ErikaPresenterOutputMode`。 |
| `active_encoding` | 実際の `SdrSrgb`、`AppleEdr`、`AndroidExtendedLinearScRgb`、`Hdr10Pq` encoding。 |
| `surface_format` | 実際の 8-bit UNORM、10-bit UNORM、16-bit float surface class。 |
| `native_data_space` | Android `ANativeWindow` dataspace。`406913024` は `SCRGB_LINEAR`、`-1` は unavailable/not applicable。 |
| `requested_headroom` | sanitize 済み requested content-headroom ceiling。最小 `1.0`。 |
| `active_headroom` | known なら current display HDR/SDR ratio、unknown なら effective-content fallback value。 |
| `active_headroom_known` | `active_headroom` が authoritative platform ratio 由来か。API 34+ ratio available なら Android は true。 |
| `extended_linear_active` | FP extended-linear presentation path が active か。Apple EDR と Android scRGB は `active_encoding` で区別。 |
| `fallback_reason` | requested mode が active でない理由を示す stable `ErikaOutputFallbackReason`。 |
| `fallback_count` | 記録された output fallback transition/failure の回数。 |
| `data_space_failures` | dataspace/output color-space validation failure 回数。 |
| `headroom_updates` | runtime headroom state の実変化回数。duplicate ratio/known publish では増えない。 |
| `extended_linear_frames` | active extended-linear path で present した frame 数。 |

Fallback value は ABI-stable です。新しい reason は末尾へ追加し、`0..8` を renumber しません。

| Code | Enum | Stable label | 意味 |
|------|------|--------------|------|
| 0 | `None` | `none` | fallback なし。 |
| 1 | `DisplayHdrUnsupported` | `display_hdr_unsupported` | display/surface HDR capability probe failure。 |
| 2 | `HybridCompositionRequired` | `hybrid_composition_required` | Android surface が direct `SurfaceView` composition ではない。 |
| 3 | `WgpuBackendNotVulkan` | `wgpu_backend_not_vulkan` | active wgpu backend が Vulkan ではない（例：GLES）。 |
| 4 | `Rgba16FloatSurfaceFormatUnavailable` | `rgba16float_surface_format_unavailable` | surface capabilities に `Rgba16Float` がない。 |
| 5 | `NativeWindowDataSpaceApiUnavailable` | `native_window_dataspace_api_unavailable` | `ANativeWindow_*DataSpace` API がない（API 26/27 を含む）。 |
| 6 | `ScrgbDataSpaceVerificationFailed` | `scrgb_dataspace_verification_failed` | `SCRGB_LINEAR` set/readback verification failure。 |
| 7 | `SurfaceConfigureFailed` | `surface_configure_failed` | requested output surface configure failure。 |
| 8 | `LegacyAppleEdrUnsupported` | `legacy_apple_edr_unsupported` | Apple EDR 未実装 backend でこの mode を要求。 |

`capture_frame_rgba` は**スクリーンショット**です。現在の合成フレーム（映像 + 字幕、
弾幕は含みません）を、要求した `width`×`height`（表示 surface サイズとは独立）で呼び出し側確保の
RGBA8 バッファにオフスクリーン描画します。`out_capacity` は少なくとも `width*height*4`。
フレームがまだ無いときは `PlayerError` を返します。Metal と wgpu（Android を含む）は
capture を実装済みで、現在の D3D11 backend は未実装です。capture は常に SDR RGBA8
offscreen target を使い、HDR/extended-linear content を tone-map します。したがって display
output が Apple EDR、HDR10、Android extended-linear scRGB の場合も返す byte は SDR です。

```c
uint32_t w = 1920, h = 1080;
uint8_t *rgba = malloc((size_t)w * h * 4);
if (erika_presenter_capture_frame_rgba(p, w, h, rgba, (uintptr_t)w * h * 4) == ErikaStatus_Ok) {
    /* rgba は w*h の密に詰まった RGBA8 ピクセル——PNG へエンコードなど */
}
free(rgba);
```

### エージェント向け診断（JSON）

```c
ErikaStatus erika_presenter_get_diagnostics_json(ErikaPresenterHandle *, char **out_json);
ErikaStatus erika_log_read_json(uint32_t limit, int32_t min_level, uint64_t since_seq,
                                char **out_json);
```

新しい診断データは JSON ドキュメントで返し、上の構造体にフィールドは追加しません
（Apple プラグインは構造体を手書きでミラーしているため、追加するとライブラリと
プラグインの同時更新が必要になります）。両シンボルはオプションとして解決し、欠けて
いれば「非対応」として扱ってください。`Ok` のとき `*out_json` は呼び出し側所有の UTF-8
ドキュメントで、`erika_string_free` で解放します。それ以外の status では `NULL` です。
各ドキュメントは `schema`（現在 `1`）を持ち、同じ schema 内ではフィールドの追加のみ
行います。不明な値は `null` で、0 を捏造しません。時間はミリ秒（マイクロ秒精度）です。

#### 診断スナップショット

`erika_presenter_get_diagnostics_json` は 1 回の呼び出しで全体を返します。presenter の
フィールド、レンダラ/オーディオのカウンタ、ロックフリーの再生カウンタを読むだけで、
デコードもレンダリングもせず、再生スレッドを待ちません（macOS の release ビルドで再生中、
JSON エンコード込みで中央値 約 0.3 ms）。スレッド規則は他のハンドル呼び出しと同じです。

セクション: `playback`（状態、位置、長さ、倍速、`audioOnly`、`buffering`）、`media`
（選択中の映像/音声/字幕トラック）、`decoder`（直近のデコーダ遷移: 要求/実際/直前の
バックエンド、`hardware`、`fellBack`、`fallbackCount`、`stage`、`reason` など。初回遷移前は
`null`）、`video`（ハードウェア/ソフトウェア/ゼロコピー各フレーム数、CPU フォールバック、
失敗、バックプレッシャー破棄）、`presenter`（surface、各段の直近所要時間）、`output`、
`hdr`、`resources`、`upscaler`、`audio`（キュー、アンダーフロー、リングの破棄、デバイス
復旧）、`buffers`（再生スレッドが公開するキュー深さ `{queued, capacity}`、presenter の
フレームチャネル）、`timing`:

- `timing.scheduler`: 設定（`leadTimeMs`、`dropToleranceMs`、`maxConsecutiveDrops`）、
  `presentedFrames`、`lateDroppedFrames`（ドロップ許容を超えて破棄。従来は未計数）、
  `forcedLatePresents`（許容超過だが連続ドロップ枠を使い切ったため表示）、
  `maxDropBurst`、`maxDroppedLateMs`、`lastDropAgoMs`、`seekPrerollDroppedFrames`、直近
  128 表示フレームの `lateByMs` 窓（`samples`、`meanMs`、`p50Ms`、`p95Ms`、`maxMs`）、
  `lateHistogram`。
- `timing.presentation`: liberika がレンダリングした時点で計測（実際の vsync ではない）。
  `tickIntervalMs`、`newFramesPresented`、`supersededBeforePresent`、`frameIntervalMs`、
  `jitterMs`（ソースフレーム間隔÷倍速との絶対差）、`longFrames`（1.5 間隔超）。
- `timing.clock`: オーディオマスターの補正（従来は trace のみ）。`observations`、
  `corrections`、`snaps`、前進/後退回数、`lastDriftMs`（正: オーディオが再生クロックより
  先行）、`maxAbsDriftMs`、`appliedTotalMs`、最終補正/スナップからの経過、`absDriftMs` 窓。

`timing` と `buffers.playback` は新しいメディアを開くとリセットされます。各フィールドの
完全な一覧は英語版 `capi_reference.md` を参照してください。

#### ログリング

`erika_log_read_json` はプロセス全体のメモリ内リング（最大 2000 件 / 512 KiB、1 件の本文は
4 KiB まで）を出力します。ハンドル不要で、どのスレッドからも呼べます。内容は、従来
stderr / logcat / hilog に出ていた運用診断（出力自体は不変）、以前は素の `eprintln!` だった
運用上の失敗、警告以上の FFmpeg `av_log` です。FFmpeg コールバックはプロセス全体で共有され、
スタックバッファで整形し、ブロックせず（競合時は `droppedContended`）、数字/アドレスだけが
異なる連続メッセージは `count` 付きの 1 件にまとめ、新規 FFmpeg 件数は毎秒 50 件まで
（超過分は `rateLimited`）。FFmpeg 自身の stderr 出力は従来どおりです。
`ERIKA_FFMPEG_LOG_CAPTURE=0` で無効化、Linux のシステム共有 FFmpeg では
`ERIKA_FFMPEG_LOG_CAPTURE=1` のときのみ有効です。

引数: `limit` はフィルタ後の最新件数（`0` で全件）、`min_level` は `ErikaLogLevel`
（`Trace` 0 … `Error` 4）または全レベルの `-1`、`since_seq` は `seq >= since_seq` の
件のみ（前回の `nextSeq` を渡すと差分読み出し）。件は古い順で、構造化診断は `data`、
それ以外は `message` として返ります。

レベル: 呼び出し側が分かる場合は明示。FFmpeg は PANIC/FATAL/ERROR → `error`、
WARNING → `warn`、libass は 0–1 → `error`、2 → `warn`。その他の構造化診断は
`event`/`stage` 名から既定値を決めます（fatal/panic は `error`、失敗・タイムアウト・
利用不可/喪失/拒否/欠落・ストール・フォールバックは `warn`、それ以外は `info`）。

マスキングは出力時に行います: URL のユーザー情報と `?`/`#` 以降、認証情報らしき
`key=value`/`key: value` と JSON キー、`Bearer`/`Basic` 認証、現在のユーザーのホーム
ディレクトリ（`~`）、その他の `/Users/<user>`、`/home/<user>`、`C:\Users\<user>`。
リングはプロセスメモリ内にのみあり、要求時に読み出されます。

## 列挙

| 列挙 | 値 |
|------|----|
| `ErikaState` | `Idle` `Opening` `Ready` `Playing` `Paused` `Stopped` `Closed` `Error` |
| `ErikaEventKind` | `None` `StateChanged` `DurationChanged` `PositionChanged` `TracksChanged` `BufferingChanged` `VideoParamsChanged` `VideoDecoderChanged` `AudioOutputChanged` `SurfaceAttached` `SurfaceDetached` `Error` `TrackSelectionChanged` |
| `ErikaTrackKind` | `Video` `Audio` `Subtitle` |
| `ErikaTrackSource` | `Embedded` `External` |
| `ErikaWgpuSurfaceKind` | `Unknown` `MacOsNsView` `MacOsCaMetalLayer` `IosUiView` `WindowsHwnd` `XlibWindow` `WaylandSurface` `AndroidNativeWindow` `OhosNativeWindow` |
| `ErikaFlutterTextureKind` | `Unknown` `MacOsTextureRegistrar` `IosTextureRegistrar` `AndroidSurfaceTexture` `WindowsTextureRegistrar` `LinuxTextureRegistrar` |
| `ErikaVideoAlphaMode` | `Opaque` `PackedAlphaRight` |
| `ErikaPresenterOutputMode` | `Auto` `Sdr` `AppleEdr` `ExtendedLinear` |
| `ErikaActiveOutputEncoding` | `SdrSrgb` `AppleEdr` `AndroidExtendedLinearScRgb` `Hdr10Pq` `LinuxExtendedLinearScRgb` |
| `ErikaOutputSurfaceFormat` | `EightBitUnorm` `TenBitUnorm` `SixteenBitFloat` |
| `ErikaOutputFallbackReason` | `None` `DisplayHdrUnsupported` `HybridCompositionRequired` `WgpuBackendNotVulkan` `Rgba16FloatSurfaceFormatUnavailable` `NativeWindowDataSpaceApiUnavailable` `ScrgbDataSpaceVerificationFailed` `SurfaceConfigureFailed` `LegacyAppleEdrUnsupported` |
| `ErikaLumaUpscalerMode` | `Off` `ArtCnnC4F16` `ArtCnnC4F32` `ArtCnnC4F16Ds` |
| `ErikaUpscalerBackendStatus` | `Off` `Inactive` `Building` `Scalar` `SimdgroupMatrix` |

## 構造体

- **`ErikaPresenterConfig`** `{ int32 output_mode; float edr_headroom; int32 luma_upscaler; int32 video_alpha_mode; }` —
  `create_with_config` に値で渡す。`video_alpha_mode` は `ErikaVideoAlphaMode`
  （既定 `Opaque`、左右分割の color/alpha asset には `PackedAlphaRight`）。
- **`ErikaSurfaceOutputCapabilities`** `{ bool extended_linear; bool direct_composition; float desired_headroom; int32 fallback_reason; }` —— attach 時に渡す Android host の display/surface probe result。`desired_headroom == 0` は system auto。
- **`ErikaUpscalerStatus`** —— 要求モード、現在の backend、フォールバック回数、
  アップスケール済みフレーム数、直近の encode/GPU マイクロ秒。
- **`ErikaOutputStatus`** —— 上記「診断とスクリーンショット」の 13-field negotiated
  output snapshot。
- **`ErikaDanmakuConfig`** —— 弾幕レイアウト/外観の全設定（フォントサイズ、不透明度、
  表示領域、スクロールタイミング、衝突/スタックフラグ、ブロックモード、影スタイル）。
  `font_size` は NipaPlay/Flutter の*論理*サイズで、liberika が surface scale を掛けて
  グリフピクセルにします。
- **`ErikaDanmakuTrackInfo`** `{ id, enabled, offset_micros, item_count, char *name, char *source }` —
  `erika_danmaku_track_info_free` で解放。
- **`ErikaVideoParams`** `{ width, height, primaries, transfer }` —— `VideoParamsChanged`
  で通知される色メタデータ。
- **`ErikaTrackCounts`** / **`ErikaTrackSelection`** —— 種別ごとの件数 / 選択 id
  （`-1` = 無し）。
- **`ErikaTrackInfo`** —— トラックごとの全メタデータ。6 つの `char*` フィールドは
  caller が所有（`erika_track_info_free` で解放）。video track はさらに `bit_rate`
  （bit/s）と `frame_rate_numerator` / `frame_rate_denominator`（`0` = unknown）を持ちます。
- **`ErikaEvent`** —— 構造体による tagged union：`kind` がどのフィールドが有効かを選ぶ
  （`state`、`duration_micros`、`position_micros`、`buffering`、`video`、`tracks`）。
  `Error` イベントは `status` がコードを運ぶ。
- **`ErikaPresenterStats`** —— パイプラインカウンタ：デコード/描画フレーム数、push した
  オーディオフレーム、overlay/弾幕フレーム、ハード vs ソフト vs ゼロコピーフレーム数、
  HDR ソース/HDR10 出力/SDR トーンマップ数、オーディオクロックの read/queued/underflow
  フレーム数、直近の描画時間。

## イベント

ループの各反復でポーリングし `kind` で分岐します。

```c
ErikaEvent ev;
while (erika_presenter_poll_event(p, &ev) == ErikaStatus_Ok) {
    switch (ev.kind) {
        case ErikaEventKind_StateChanged:    /* ev.state */                 break;
        case ErikaEventKind_DurationChanged: /* ev.duration_micros */        break;
        case ErikaEventKind_PositionChanged: /* ev.position_micros */        break;
        case ErikaEventKind_TracksChanged:   /* erika_*_tracks を再取得 */    break;
        case ErikaEventKind_BufferingChanged:/* ev.buffering */              break;
        case ErikaEventKind_VideoParamsChanged: /* ev.video */               break;
        case ErikaEventKind_Error:           /* ev.status + last_error */    break;
        default: break;
    }
}
```

イベントキューは有界で、ポーリングでドレインします。ポーリングをやめたホストは状態の
観測をやめるだけです。`position_micros` は再生中に周期的に発行されます。

## 最小 presenter 組み込み（C）

```c
#include "erika.h"

ErikaPresenterHandle *p = erika_presenter_create();
erika_presenter_attach_metal_layer(p, (uint64_t)layer, w, h, scale);  // または attach_windows_hwnd
if (erika_presenter_open(p, "/path/to/video.mkv") != ErikaStatus_Ok) {
    char *m = erika_last_error_message(); /* ログ */ erika_string_free(m);
}
erika_presenter_play(p);

// 表示フレームごと:
ErikaPresenterStats stats;
erika_presenter_render_tick(p, host_time_seconds, &stats);
ErikaEvent ev;
while (erika_presenter_poll_event(p, &ev) == ErikaStatus_Ok) { /* 分岐 */ }

// 解放:
erika_presenter_detach_surface(p);
erika_presenter_destroy(p);
```

プラットフォームごとの surface と表示タイマーの詳細は
[integration.ja.md](integration.ja.md)、実行可能な
[`macos_native_demo`](../examples/macos_native_demo) /
[`windows_native_demo`](../examples/windows_native_demo) の例も参照してください。
