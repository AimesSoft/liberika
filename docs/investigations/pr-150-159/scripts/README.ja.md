# ネイティブチェックの再現

[中文](README.zh.md) · [English](README.md)

これらは実際のバイナリを使う独立ホストで、ベースライン/head の回帰ログを補完します。OHOS エミュレーターの実行結果は [OHOS 実行記録](../ohos-runtime.ja.md) にあり、Apple の検証は別途行います。

## Windows

`docs/building.zh.md` のネイティブ依存関係に従い、MSVC、LLVM、Windows SDK を使ってこのブランチの `erika_capi` をビルドし、Python 3 をインストールします。生成した DLL を `scripts/target/debug/erika_capi.dll` に配置し、[元レビュー](../review.zh.md) に記載された BBB 素材を `scripts/bbb-10s.mp4` にダウンロードします。その後 PowerShell から実行します。

```powershell
python .\windows-live-capi.py C:\path\to\Erika
```

ホストは Win32 HWND を作成し、製品版 D3D11 レンダラーを接続して、ローカルメディアおよび小さなキャッシュウィンドウの HTTP 206/全ファイル 200 応答を再生します。一時停止、EOF シーク、再開、速度、MP3 プローブ、不正入力を検証します。既定の実行には実際の既定 WASAPI endpoint が必要です。endpoint がない場合は `--video-only` で音声再生を明示的に省略します。D3D11 は任意の C API スクリーンショットを実装せず、この制限はログに記録されます。

## Linux

記録された環境は既存の `erika-performance-ci:20260930` イメージ内の Ubuntu 26.04、Rust 1.93.1、FFmpeg 8、パッチ適用済み libass、Xvfb、PulseAudio、Mesa llvmpipe です。libass と wgpu を有効にしてブランチをビルドします。

```sh
export CARGO_TARGET_DIR=/target
cargo build --locked -p erika_capi -p linux_native_demo
cc examples/linux_native_demo/capi_smoke.c -I crates/erika_capi/include \
  -L /target/debug -Wl,-rpath,/target/debug -lerika_capi -lX11 \
  -o /target/debug/linux_capi_smoke
```

スクリプトは checkout を `/work`、バイナリを `/target/debug`、スクリプト/メディア/出力を `/review-logs` に想定します。root 以外のアカウント、モード 0700 の書き込み可能な `/tmp/erika-pr-desktop` ディレクトリ、PulseAudio null sink を使います。

```sh
xvfb-run -a bash /review-logs/native-desktop.sh
xvfb-run -a bash /review-logs/linux-live-subtitle.sh whitespace-separators.srt
```

2 つの SRT cue を持つ sidecar を作成します。`First` は 1–2 秒、`Second` は 3–4 秒で、区切り行に `スペース + tab + スペース` を入れます。ホストはシーク後に短く再開して一時停止したフレームをキャプチャします。`whitespace-separators-second.png` で `Second` が見えることを確認してください。ピクセル数はトラック切り替えによる動画更新も含み、字幕の面積を示すものではありません。

Python ホストはカバー付き 1 秒 MP3 を 4 秒再生し、実際の音声消費を検証します。48000 フレームを消費し、最終キューは空、音声エラーはゼロでした。初回の短い 1.5 秒観測では消費が測定されず、出力の判定には不十分でした。別のネイティブ demo は 5 秒 MP3 で音声を消費しましたが、汎用 smoke 終了チェックは動画を要求し、音声のみのファイルでは 1 を返します。`linux-cover-audio.sh` はその結果を保存します。

リポジトリのカバー付き MP3 から 2 つのサンプルを生成します。

```sh
ffmpeg -y -stream_loop -1 -i /work/crates/erika/testdata/software/mp3-attached-pic.mp3 \
  -t 5 -map 0:a -map 0:v -c:a libmp3lame -c:v copy \
  -disposition:v attached_pic /review-logs/cover-5s.mp3
ffmpeg -y -i /review-logs/cover-5s.mp3 -map 0:a -c copy /review-logs/no-cover-5s.mp3
xvfb-run -a bash /review-logs/linux-cover-audio.sh
```

## OHOS SDK ビルド

公式公開アーカイブ：

<https://repo.huaweicloud.com/openharmony/os/5.1.0-Release/ohos-sdk-windows_linux-public.tar.gz>

SHA256：`8c1f97df34812e45fa4f0e3275987b261e80622abb7c51392bb063f853451344`。

Linux native/toolchains/previewer アーカイブを `/opt/erika-ohos-sdk-5.1.0/linux` に展開し、実行します。

```sh
bash /review-logs/ohos-native-build.sh
```

このコマンドは API 18 向けの変更済み ARM64 Flutter と ArkTS の完全なブリッジをビルドし、SDK ライブラリとチェックサム固定の v0.2.1 C API Release にリンクします。このブランチの Rust runtime は再ビルドせず、エミュレーターも実行しません。

## OHOS エミュレーター：実際の製品版 runtime

上記の SDK のみのコマンドは最初の ARM64 リンクチェックです。実際の再生には公式 DevEco Studio エミュレーター、対応する x86_64 イメージ、このブランチの Rust runtime、完全な製品版 NAPI ブリッジを使用します。

```sh
bash /review-logs/ohos-x86_64-deps-build.sh
bash /review-logs/ohos-x86_64-runtime-build.sh
bash /review-logs/ohos-x86_64-bridges-build.sh
```

事前にこのフォルダからスクリプトと CMake プロジェクトディレクトリを `/review-logs` にコピーします。`ohos-live/setup.ps1` は ArkTS HAP プロジェクトを準備し、ソースと fixture を配置します。`devecocli build` でビルドし、`hdc install` で未署名 HAP を公式エミュレーターにインストールし、`hdc shell aa start` で `com.aimesoft.erika.review/EntryAbility` を起動します。

アプリは 2 つの XComponent をそれぞれ製品版ブリッジで実行し、音動画再生、一時停止、EOF/途中へのシーク、速度変更、実 NativeWindow の detach/attach、MP3 出力、64 回の作成/破棄、古い ID の拒否を記録します。filesDir の 2 つの JSONL を `ohos-arkts-review.jsonl`、`ohos-flutter-review.jsonl` として取り出し、`python summarize-ohos-live.py /path/to/output` を実行して hdc スクリーンショットを確認します。

または収集器を実行し、両ブリッジを待って集計を検査します。

```powershell
./collect-ohos-live.ps1 -Hdc C:\path\to\hdc.exe -OutputDirectory C:\path\to\output -ApiVersion 18 -OperatingSystem OpenHarmony-5.1.0.105
```

失敗またはタイムアウト時、収集器はエミュレーターを動作中のままにします。現在のホストは一時停止が安定してからサンプルを取得し、2 回目のサンプルと比較した後、一時停止状態の EOF シークを別途検査します。`pause-settled` のない古い JSONL はこの検査を満たしません。

Windows 音声では `numpy` と `soundcard` をインストールし、同じ DLL/メディア配置で `python wasapi-loopback.py C:\path\to\Erika` を実行します。実際の既定 WASAPI 出力から 2 秒録音し、非ゼロ PCM と実音声消費を要求して `merge-wasapi-loopback.wav` を保存します。

バージョン、バイナリ、結果、失敗した試行、検証範囲の限界は [OHOS 実行記録](../ohos-runtime.ja.md) と [統合検証](../merge-validation.ja.md) を参照してください。これは ArkTS アプリ内での製品版 NAPI 実行であり、完全な Flutter エンジンは動かさず、実機のゼロコピー性能も実証しません。
