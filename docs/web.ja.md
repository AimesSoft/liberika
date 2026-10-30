# ブラウザー再生：MKV、ASS フォントと弾幕

[中文](web.zh.md) | [English](web.md) | 日本語

liberika のブラウザービルドには、既存の Rust 弾幕エンジン、リポジトリでパッチを適用した libass と FreeType/HarfBuzz/FriBidi、コンテナーのデマルチプレックス用に絞った FFmpeg が含まれます。`Player` がパイプライン全体を管理し、アプリは Canvas とメディアソースを渡します。別の字幕・弾幕ライブラリは不要です。

このバックエンドは **Erika Web アプリ**の再生操作、トラック選択、チャプター、スクリーンショット、弾幕設定に統合されています。リリース 0.3.2 の `liberika-web-0.3.2.zip` は完全なブラウザーランタイムと `index.html` デモを含み、SHA-256 は `SHA256SUMS` に記録されています。アーカイブ全体を展開し、HTTPS または localhost で配信してください。ネイティブバンドルとは別で、npm では未公開です。ソースからのビルドも可能です。

## MKV 字幕と添付フォント

ブラウザーは元のコンテナーを読みます。WASM 内の FFmpeg がストリームと添付フォントを検出し、ASS/SSA ヘッダーと時刻付きパケットを libass に渡し、フォントデータを登録します。libass はスクリプトのフォント、位置、縁取り、効果を描画します。サーバーは元の MKV を HTTP Range で配信できます。サーバー側での字幕抽出、テキスト変換、焼き込みは不要です。ローカルファイルもメディアサーバーなしで同じパイプラインを使います。

字幕と弾幕は再生クロックと最終コンポジターを共有します。字幕は libass、弾幕の解析、シェーピング、配置、衝突回避はネイティブの Rust エンジンで処理します。両方が liberika 内にあり、ホストは一つの `Player` と Canvas を操作します。字幕・弾幕のスーパーサンプリングはそれぞれ既定で 2 倍解像度になり、表示サイズと動画解像度は変わりません。`player.configureSubtitles({ supersampling: false })` と `player.configureDanmaku({ supersampling: false })` で個別に無効化でき、フィールドを省略すると現在の設定を維持します。

| 段階 | ブラウザーでの実装 |
|---|---|
| 読み込みと分離 | Worker が File/Blob のチャンクまたは HTTP Range を読み、FFmpeg WASM がパケット、トラック、チャプター、添付ファイルを抽出 |
| 音声・映像デコード | 圧縮パケットをブラウザーの WebCodecs に渡す |
| 字幕描画 | パッチ済み libass + FreeType/HarfBuzz/FriBidi を WASM で実行し、MKV 添付フォントも使用 |
| 弾幕描画 | ネイティブ Rust DFM エンジンを WASM にコンパイル |
| 同期と表示 | Web Audio による音声出力とメディアクロック、WebGL2 による映像・字幕・弾幕の合成 |

コーデック構成はプロファイルと寸法を含め実行時に確認します。`open(source, { hardwareDecode: false })` は `prefer-software` を要求し、既定では `prefer-hardware`、続いて `no-preference` を試します。これはブラウザーへの希望であり、`info.hardwarePreference` は要求値を示し、実際に選択されたハードウェアは示しません。非対応コーデックでは明示的にエラーになります。WASM ビルドには音声・映像のソフトウェアデコーダーやサーバートランスコードへのフォールバックは含まれません。

## ビルド

必要なもの：Rust（workspace の rust-version を参照）、Node、Python 3、CMake、Meson、Ninja、make、pkg-config、Emscripten **4.0.23**、wasm-bindgen-cli **0.2.126**。

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked
# ローカルにインストール。シェルの起動ファイルは変更しません。
git clone https://github.com/emscripten-core/emsdk.git target/web-tools/emsdk
target/web-tools/emsdk/emsdk install 4.0.23
target/web-tools/emsdk/emsdk activate 4.0.23
cargo run -p xtask -- web build
python3 scripts/serve_web.py
```

既存の xtask 依存管理を用いて固定バージョンのネイティブソースを取得し、同じ libass パッチを適用してテキスト依存と FFmpeg、Rust WASM をビルドし、完全なパッケージを `target/web/` に出力します。有効化済み SDK は `EMSDK` または PATH 上の `emcc` で指定できます。`web build --skip-native` はビルド済み Web C ライブラリを再利用し、ブリッジ、Rust、ブラウザーパッケージを再ビルドします。ネイティブ依存の出力先は変わらず、Web ライブラリは `target/web-build` に置かれます。

`http://127.0.0.1:18765` を開いてください。サンプルはローカルファイル、外部字幕、Bilibili XML / Dandanplay JSON 弾幕を受け付け、`?src=URL` でリモートメディアを開きます。付属の localhost サーバーは Range に対応します。生成ディレクトリはデプロイ可能なパッケージであり npm 公開物ではありません。JS、WASM、Worker、ライセンス表記を一緒に保管してください。`build-manifest.json` に出力の SHA-256 ハッシュが記録されます。

## 組み込み

生成したランタイムを相対パスを保ってサイトの `liberika/` ディレクトリにコピーします。以下はユーザーがローカル MKV を選択し再生するページです。埋め込み ASS と添付フォントは自動で読み込まれます。

```html
<input id="media" type="file">
<button id="play" disabled>再生</button>
<canvas id="video" style="width:100%;height:60vh"></canvas>
<pre id="status"></pre>
<script type="module">
import { Player } from './liberika/index.js';
const fileInput = document.querySelector('#media');
const playButton = document.querySelector('#play');
const status = document.querySelector('#status');
const showError = error => { status.textContent = error.message; };
const player = await Player.create({ canvas: document.querySelector('#video') });
player.addEventListener('error', event => showError(event.detail));
fileInput.onchange = async () => {
  const file = fileInput.files[0];
  if (!file) return;
  fileInput.disabled = true;
  playButton.disabled = true;
  try {
    const info = await player.open(file); // File / Blob、または CORS でアクセス可能な URL
    status.textContent = `${file.name}: ${info.duration.toFixed(1)} s`;
    playButton.disabled = false;
  } catch (error) {
    showError(error);
  } finally {
    fileInput.disabled = false;
  }
};
// ブラウザーの音声ポリシーに従い、ユーザージェスチャーから play() を呼び出す。
playButton.onclick = () => player.play().catch(showError);
// アプリのアンマウント処理で：await player.destroy();
</script>
```

ファイルピッカー、WASM、コマンドキューなどを通してプレーヤーの作成が遅延する場合は、ページ設定時に音声を準備し、そのセッションをプレーヤーに渡します。

```js
import { AudioSession, Player } from './liberika/index.js';
const audioSession = new AudioSession(); // ユーザーの最初のクリックより前
// その後、Canvas が用意できたら：
const player = await Player.create({ canvas, audioSession });
```

セッションは信頼された入力時に音声を同期的に再開し、準備中は無音を維持します。`play()` は再生を要求し、`play` イベントで音声クロックの開始を確認できます。ブラウザーのアクティベーション待ちでメディアコマンドキューをブロックしてはいけません。音声許可待ちでも seek、pause、close は完了します。`destroy()` はセッションも閉じ、入力リスナーを削除します。各プレーヤーが一つのセッションを所有します。

字幕、弾幕、操作には `open()` 後の同じインスタンスを使います。

```js
await player.addDanmaku(await danmakuFile.text()); // Bilibili XML / Dandanplay JSON
await player.addSubtitle(await subtitleFile.text()); // ASS / SSA / SRT / WebVTT
await player.configureDanmaku({ opacity: 0.8, fontSize: 30, delay: 0, supersampling: true });
await player.configureSubtitles({ scale: 1, supersampling: true });
```

| 操作 | API と動作 |
|---|---|
| 再生 | `play()`、`pause()`、`seek(seconds)`。シーク後も一時停止 / 再生状態を維持 |
| 音量・速度 | `setVolume(0…1)`、`setRate(0.25…4)` |
| トラック | `info.tracks`、`selectAudio(id)`、`selectSubtitle(id)`。`null` で音声 / 字幕を無効化 |
| チャプター | `info.chapters` に `title`、秒単位の `start` / `end`（`end` は `null` の場合あり）。`start` にシーク |
| 外部字幕 | `addSubtitle(text)` は有効な字幕スクリプトを置換。外部トラックの切り替えにはホストがスクリプトを保持 |
| 追加フォント | メディアを開いた後に `addFont(name, bytes)`。ASS の埋め込みフォントは自動読み込み |
| 弾幕ファイル | `addDanmaku(text)` はコメントを追加し、その呼び出しで追加した件数を返す |
| ローカルのリアルタイムコメント | `sendDanmaku(text)` は現在の再生位置に追加。ネットワークへの投稿はホストが担当 |
| 弾幕タイミング | `configureDanmaku({ delay: seconds })` は −60〜60 秒。正の値で遅延 |
| スクリーンショット | `capture(width?)` は映像、字幕、弾幕を含む PNG Blob を返す |
| 閉じる / 再利用 | `close()` は現在のメディアをクリアし、`open()` で別ソースに再利用 |
| 破棄 | `destroy()` は Worker、デコーダー、フレーム、GPU リソース、AudioContext を解放 |

全オプションは `packages/erika_web/index.d.ts` を参照してください。`loaded`、`stats`、`timeupdate`、`error`、`log` は `detail` にデータを持つ CustomEvent、`play`、`pause`、`ended` は通常の Event です。各プレーヤーは専用 Worker、WASM インスタンス、映像フレーム、GPU リソース、AudioContext を所有します。別ファイルには `open()` を再利用し、プレーヤー削除時は `destroy()` を呼びます。Canvas は一度だけ転送され、新しいプレーヤーインスタンスには再利用できません。

メニューを開くときやレイアウトを切り替えるときは、同じインスタンスと Canvas をマウントしたままにします。ホスト側で未処理のスライダー更新をまとめてください。不透明度の変更はグリフアトラスを再利用し、フォントサイズの変更は後述のプレビュー / 確定経路を使います。

## デプロイ

2026-10-22 のリリースビルドでは再生ランタイムがファイル合計 **10.93 MiB**、各ファイルを gzip レベル 9 で圧縮した合計が **4.56 MiB** でした。両 WASM モジュール、JavaScript ランタイム、完全な組み込みフォールバックフォントを含み、ホスト UI、デモ、ドキュメント、メディアは含みません。重複したフォントコピーを削除する前はそれぞれ 17.00 MiB、7.30 MiB でした。サーバーが gzip を有効にすると転送量は圧縮サイズに近づきます。これらはパッケージサイズであり、実行時メモリ使用量ではありません。

- HTTPS または localhost で配信し、JavaScript モジュールの MIME type と `.wasm` 用の `application/wasm` を設定します。JS、両 WASM モジュール、Worker、`licenses/` を一緒に配置します。生成された `index.html` は任意のデモです。
- Canvas にゼロでない CSS 幅と高さを指定します。`Player` はサイズを監視し描画面を自動更新します。
- リモートメディアはブラウザーからのリクエストを許可する必要があります。8 MiB を超えるファイルには、総サイズが既知の有効な HTTP `206` バイト範囲応答が必要です。クロスオリジンサーバーは `Access-Control-Expose-Headers` で `Content-Range` を公開します。`scripts/serve_web.py` はローカルテスト用に Range をサポートします。
- ランタイムをバージョン付きディレクトリにデプロイするか、エントリーポイントとリソースをまとめて再検証し、キャッシュされた JS が別ビルドの WASM を読み込まないようにします。
- SharedArrayBuffer とクロスオリジン分離は不要です。音声はユーザージェスチャーから開始します。ローカルメディアはユーザーが選択した File/Blob を使います。

## Erika Web への統合

[Erika](https://github.com/MCDFsteve/Erika) は `web_app` ターゲットでこのバックエンドを使います。Rust アプリと Bern UI がブラウザーで動作し、Bern Web 拡張がメディアコマンドを liberika の `Player` に適合させます。ホストはメニューとミニプレーヤーで同じ Canvas と Worker を維持し、ローカル File を直接 Worker に渡し、非同期の字幕・弾幕操作を await します。

拡張のビルドは `cargo run -p xtask -- web build` を実行し、完全なランタイムをアプリに同梱します。アダプターとリリース手順は Erika の `web/player.js`、`web/build.py`、`docs/web-playback.md` を参照してください。Web サイトは上記 JavaScript API を直接利用でき、Bern は Erika ホストとの統合時だけ必要です。

## データフローと性能

- ローカル Blob は Worker 内で小さなチャンクとして読みます。リモートファイルは上限 2 MiB の Range キャッシュを使用。8 MiB 超では HTTP Range が必要で、クロスオリジンでは `Content-Range` を公開します。FFmpeg/libass WASM モジュールのメモリ上限は 512 MiB。動画全体を仮想ファイルシステムにコピーする必要はありません。
- FFmpeg WASM から WebCodecs に渡すのは圧縮映像パケットのみです。ブラウザーの VideoFrame は Rust/WASM への CPU ピクセル読み戻しなしに WebGL テクスチャーへアップロードされます。基盤となる GPU コピー経路はブラウザーが制御します。
- デコードキューには上限があり、seek / open / dispose 時に古いフレームを閉じます。seek は generation により非同期出力を無効化します。ASS パケットと添付フォントは同じデマルチプレクサーで抽出します。
- ネイティブ Rust DFM エンジンが解析、配置、衝突回避、シェーピング、グリフのラスタライズを行います。GPU は一つのコメントのマスクを統合し、不透明度を一度適用します。暗い無彩色には白い縁、彩度の高い色には黒い縁を付けます。
- 不透明度更新はアトラスを再利用します。フォントサイズ変更は既存グリフでプレビューし、120 ms 後、ドラッグ中は最大 500 ms 以内に確定します。可能なら dirty rectangle でアトラスを更新。表示 tick に応答を返すことで、遅い Worker に古いアニメーション要求が蓄積しないようにします。
- 字幕と弾幕は映像とは独立してスーパーサンプリング可能です。変更のない ASS ビットマップは GPU テクスチャーを維持します。完全なフォールバックフォントは `erika_core_bg.wasm` に一度だけ埋め込まれ、Worker はメディアを開くたびにそのバイト列を libass に登録します。別の `subfont.ttf` やフォント要求は不要で、インクリメンタルビルドは旧単独アセットを削除します。オフラインでも利用でき、アプリ独自のフォントデータも指定できます。

## 現在の対応範囲

対象はセキュアコンテキスト（HTTPS または localhost）で WebCodecs VideoDecoder **と** AudioDecoder、OffscreenCanvas、WebGL2、Web Audio を提供するブラウザーです。プレーヤーごとに一つの Worker を使い、このビルドでは SharedArrayBuffer とクロスオリジン分離は不要です。

コンテナーは MP4/MOV、Matroska/WebM、MPEG-TS、AVI、FLV、Ogg を読み取ります。ブラウザーのコーデックアダプターは H.264/HEVC 構成レコード、VP8/VP9/AV1、AAC/Opus/Vorbis/MP3/FLAC/ALAC を対象とし、実際の対応は実行時の問い合わせに通る必要があります。一部のコンテナーとコーデックの組み合わせには追加のビットストリーム変換が必要で、必要な構成がなければ拒否されます。

埋め込み ASS/SSA と外部 ASS/SSA、SRT、WebVTT に対応します。プレーンな SRT/VTT は ASS テキストに変換され、リッチ VTT の位置指定や HTML マークアップは保持されません。埋め込み ASS の seek は 5 秒のプリロールを使用し、観測済みイベントを保持します。初回 seek で、プリロールより前に始まった長いイベントの途中に移動した場合、そのイベントが表示されないことがあります。外部の完全な ASS スクリプトにはこの制限はありません。

ビットマップ字幕、埋め込みの非 ASS 字幕の変換、DRM、長さ不明のライブソース、デスクトップ HDR/EDR/Dolby Vision 処理、ArtCNN は対象外です。出力は SDR。現在、再生速度の変更は Web Audio により音程も変化します。

## 検証

2026-10-22、統合された Erika Web アプリは macOS の Chrome **155.0.8059.85** で合成メディアを用いたブラウザー回帰に合格しました。対象は以下の通りです。

- ASS と添付フォントを含む H.264/AAC MKV、VP9/Opus WebM。
- 弾幕 2,000 件、不透明度・サイズのリアルタイム設定、字幕切り替え、外部 SRT、字幕スーパーサンプリング、音声トラック選択。
- pause / seek / rate / volume、HTTP Range、コンテナーチャプターとチャプターへの seek。
- Canvas / Worker を保持したメニュー・ミニプレーヤー切り替え、実ピクセルを確認した PNG キャプチャ、close / reopen、resize、不正ファイルからの復旧。

独立した回帰テストでは、フォント添付のない WebM で中国語・日本語の字幕と弾幕を描画し、フォールバックフォントへの別リクエストがないこと、および共有フォントバイト列が完全なソースアセットと一致することも確認します。

Erika の回帰テストは `web/browser.test.mjs` にあり、レポートとスクリーンショットは同リポジトリの `target/web-review/` に出力されます。同日、Safari 27.0.1 でも ASS トラック 2 本と添付フォント 5 個を含む 1080p HEVC Main 10 / Opus MKV について、オープン、字幕、pause、チャプター seek、繰り返しスキップを手動検証しました。この過程で、音声アクティベーションがコマンドキューを塞ぐ問題と、HEVC open-GOP の RASL ピクチャーがランダムアクセス後にデコーダーエラーを起こす問題を修正しました。Firefox は未検証です。liberika 単独の回帰テストは次の通りです。

```sh
node --test packages/erika_web/tests/*.test.mjs
python3 scripts/web_test_fixtures.py
python3 scripts/serve_web.py
# 別のターミナルで、使い捨てディレクトリに Playwright をインストール：
npm install --prefix target/web-tests playwright@1.58.2
PLAYWRIGHT_MODULE="$PWD/target/web-tests/node_modules/playwright/index.mjs" \
  node packages/erika_web/tests/browser.mjs
```

ブラウザー回帰は既定でインストール済み Chrome を使います。別の Playwright チャンネルには `ERIKA_BROWSER_CHANNEL`、別のローカルテスト URL には `ERIKA_WEB_URL` を設定してください。実際のデコード済みフレームと PCM、埋め込み / 外部字幕、高密度弾幕、不透明度・サイズのリアルタイム変更、pause / seek / rate、HTTP Range 再生、再生終了、再オープン、クリーンアップを確認します。レポートとスクリーンショットは `target/web-tests/` に保存されます。テスト済み構成以外のブラウザー / コーデック対応は引き続き実行時の確認が必要です。
