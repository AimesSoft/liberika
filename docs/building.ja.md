# liberika のビルド

> 翻訳：[English](building.md) · [中文](building.zh.md)

ブラウザー向けには現在のソースで `cargo run -p xtask -- web build` を使います。
[Web ビルド・配信ガイド（英語）](web.md) を参照してください。Rust 弾幕エンジン、
libass、FFmpeg demuxer を含む JS/WASM パッケージを一括ビルドし、音声・映像は
WebCodecs でデコードします。以下はネイティブプラットフォームの依存とビルド手順です。

liberika は一連の**静的ビルドされたネイティブ依存**（FFmpeg、全ターゲット向け dav1d AV1
ソフトウェアフォールバック、オプションの libass 字幕スタック）をリンクする Rust workspace です。これらのネイティブライブラリは vendoring
されていません——`xtask` オーケストレータで一度ビルドすると `third_party/dist/` 配下に
配置され、Rust crate がそのステージングディレクトリをリンクします。

```
xtask deps build  ──▶  third_party/dist/<target>/<profile>/{ffmpeg,dav1d,zlib,libass,…}
                                        │
                          erika_ffmpeg_sys/build.rs（dist を自動発見、bindgen 実行）
                                        │
                                  cargo build -p erika
```

> 英語版：[building.md](building.md)。

## Linux のシステムライブラリビルド

Linux は FFmpeg 8、PulseAudio と liberika パッチ済み libass を使います。
以下の xtask リリースターゲットとは別経路です。[Linux ガイド](linux.md) から始めます。

## 前提

### Rust

- Rust **1.92+**（workspace edition 2024）。
- クロスターゲットでは対応する Rust std target を追加：
  `rustup target add aarch64-apple-ios` や
  `rustup target add x86_64-pc-windows-msvc`。
- tvOS source build は `rust-src` 付き nightly と Cargo `-Z build-std`
  （`panic_abort` std）を使い、release workflow と一致させます。

### ビルドツール —— macOS / Unix ホスト

`tar`、`make`、`clang`、`cmake`、`pkg-config`、`python3`（`venv` 付き）が `PATH` 上に
必要です。完全な字幕スタック（`--all`）には加えて `meson` と `ninja`（Intel ホストでは
FFmpeg の x86 アセンブリに `nasm`）が必要です。macOS では Xcode Command Line Tools と、
上記を Homebrew で導入します。

`erika_ffmpeg_sys` は **bindgen** を実行するため `libclang` が必要です。自動で見つから
ない場合は `LIBCLANG_PATH` を設定します。

### ビルドツール —— Windows（`x86_64-pc-windows-msvc`）

- **Visual Studio Build Tools**（MSVC）+ Windows SDK、および **CMake** コンポーネント。
- **POSIX シェル**（Git for Windows または MSYS2）——FFmpeg の `configure` に必要。
- **GNU make**（MSYS2 `make` または MinGW `mingw32-make`）。
- FFmpeg アセンブリに `nasm`。
- `--all` には **Python**（`venv` 付き）。`xtask` が `pkg-config` シムを自動で用意します。

MSVC 環境が有効なシェル（*"x64 Native Tools Command Prompt"* など）からコマンドを実行し、
`xtask` がツールチェーンを見つけられるようにします。

### ビルドツール —— Android

Android SDK + NDK r29、CMake、Ninja、GNU make、Python (`venv`)、Meson、
`pkg-config` が必要です。x86_64 では `nasm` も必要です。Windows ホストでは
Git Bash と Visual Studio Build Tools を使用します。NDK は `ANDROID_NDK_HOME`、
`ANDROID_NDK_ROOT`、SDK 環境変数、または Android Studio の既定 SDK から自動検出されます。
Android の最小 API は 26 です。

## `xtask` でネイティブ依存をビルド

`xtask` は workspace メンバーで、`cargo run -p xtask -- …` で呼びます。

```sh
# 何がビルドされるか確認（副作用なし）
cargo run -p xtask -- deps plan
cargo run -p xtask -- deps status

# 基本セット（全ターゲットで zlib + FFmpeg + dav1d）—— LGPL profile
cargo run -p xtask -- deps build --profile lgpl

# libass 字幕スタックを含めすべて
cargo run -p xtask -- deps build --all --profile lgpl
```

サブコマンド：`plan`（計画を表示）、`fetch`（ソースのみ取得）、`status`（存在/ビルド
状況）、`build`（取得 + コンパイル）。

### オプション

| フラグ | 値 | 既定 | 意味 |
|--------|----|------|------|
| `--profile` | `lgpl`、`gpl-full` | `lgpl` | FFmpeg ライセンス profile（下記）。 |
| `--target` | ターゲット表参照 | `host` | クロスコンパイル先。 |
| `--all` | — | off | libass + FreeType + HarfBuzz + FriBidi（字幕描画）も。全ターゲットの基本セットは zlib + FFmpeg + dav1d。 |
| `--force` | — | off | 最新マーカーがあっても再ビルド。 |
| `--jobs N` | 整数 | 自動 | ネイティブビルドの並列度。 |

### ターゲット

| `--target` | Triple | 備考 |
|------------|--------|------|
| `host` | 現在のマシン | 既定。 |
| `aarch64-apple-darwin` | Apple Silicon macOS | |
| `x86_64-apple-darwin` | Intel macOS | |
| `aarch64-apple-ios` | iOS 実機 | |
| `aarch64-apple-ios-sim` | iOS sim（Apple Silicon） | |
| `x86_64-apple-ios` | iOS sim（Intel） | |
| `aarch64-apple-tvos` | tvOS 実機 | nightly + `rust-src` が必要。 |
| `aarch64-apple-tvos-sim` | tvOS sim（Apple Silicon） | nightly + `rust-src` が必要。 |
| `x86_64-apple-tvos` | tvOS sim（Intel） | nightly + `rust-src` が必要。 |
| `x86_64-pc-windows-msvc`（または `windows-x64`） | Windows | FFmpeg で VideoToolbox を D3D11VA/DXVA2 に置換。 |
| `aarch64-pc-windows-msvc`（または `windows-arm64`） | Windows ARM64 | ARM64 native host と x64 から ARM64 への cross build をサポート。 |
| `aarch64-linux-android`（`arm64-v8a`） | Android arm64 | |
| `armv7-linux-androideabi`（`armeabi-v7a`） | Android ARMv7 | |
| `x86_64-linux-android`（`android-x64`） | Android x86_64 | |
| `i686-linux-android`（`x86`） | Android x86 | Android 共有ライブラリで非 PIC 再配置を避けるため、x86 アセンブリ高速化を無効化。 |
| `aarch64-unknown-linux-ohos`（`ohos-arm64`） | HarmonyOS arm64 | DevEco Studio の OpenHarmony Native SDK を使用。`OHOS_NDK_HOME` または `OHOS_SDK_NATIVE` と `aarch64-unknown-linux-ohos` Rust target が必要。 |

デプロイ最小バージョンは既定で macOS `11.0` / iOS `13.0` / tvOS `13.0`。
`MACOSX_DEPLOYMENT_TARGET` / `IPHONEOS_DEPLOYMENT_TARGET` /
`TVOS_DEPLOYMENT_TARGET` で上書き可能。

HarmonyOS build は `OHOS_NDK_HOME` または `OHOS_SDK_NATIVE` が指す DevEco Studio の
`openharmony/native` ディレクトリを使い、その中の `aarch64-unknown-linux-ohos-clang`
で FFmpeg、zlib、liberika を build します。Flutter plugin の Hvigor/CMake build が
この chain を自動実行し、`liberika_capi.so` と `liberika_flutter.so` を HAP に
package します。

### Source build architecture の選択

Flutter の依存 project は、macOS では `ERIKA_MACOS_ARCHS=arm64|x86_64|universal`、Windows では `ERIKA_WINDOWS_ARCH=x64|arm64`、Android では `ERIKA_ANDROID_ABIS=arm64-v8a,armeabi-v7a,x86_64,x86` で architecture を選択します。`ERIKA_FORCE_SOURCE_BUILD=1` を設定すると prebuilt download を無効化できます。

Apple plugin の既定は checksum 検証済み prebuilt アーカイブですが、liberika checkout 内の macOS だけは例外です。pod script が package の上位に `crates/erika_capi/Cargo.toml` を見つけた場合（または `ERIKA_REPO_ROOT` が指す場合）、新しい prebuilt release を待たずにローカルの変更を取り込めるよう、`erika_capi` をソースからビルドします。`ERIKA_FORCE_PREBUILT=1` を設定すると常にアーカイブのダウンロードになります——Rust toolchain の無いリポジトリ内 consumer はこちらを使ってください。pub cache に解決された git dependency はリポジトリ全体を保持するため、こちらもソースビルドに切り替わる点に注意してください。

native library を直接 build する場合、3 つの target 指定を一致させます：

```sh
cargo run -p xtask -- deps build --all --profile lgpl --target aarch64-apple-darwin
ERIKA_NATIVE_PROFILE=lgpl ERIKA_NATIVE_TARGET=aarch64-apple-darwin \
  cargo build -p erika_capi --release --target aarch64-apple-darwin
```

Rust の tvOS target は tier 3 のため、直接 build する場合は nightly と
`rust-src` から standard library を build します：

```sh
rustup toolchain install nightly --component rust-src
cargo run -p xtask -- deps build --all --profile lgpl \
  --target aarch64-apple-tvos-sim
ERIKA_NATIVE_PROFILE=lgpl ERIKA_NATIVE_TARGET=aarch64-apple-tvos-sim \
  cargo +nightly rustc -Z build-std=std,panic_abort -p erika_capi --release \
  --target aarch64-apple-tvos-sim --no-default-features --features libass \
  --lib --crate-type staticlib
```

Windows ARM64 では対応する PowerShell command を使います：

```powershell
cargo run -p xtask -- deps build --all --profile lgpl --target aarch64-pc-windows-msvc
$env:ERIKA_NATIVE_PROFILE = "lgpl"
$env:ERIKA_NATIVE_TARGET = "aarch64-pc-windows-msvc"
cargo build -p erika_capi --release --target aarch64-pc-windows-msvc
```

`xtask --target`、`ERIKA_NATIVE_TARGET`、Cargo `--target` は同じ値にしてください。一致しない場合、Cargo が別 architecture の native dependency を link する可能性があります。

Windows の native library は既定で C runtime の DLL（`/MD`）を link します（Rust の既定と同じ）。host が `-C target-feature=+crt-static` で C runtime を静的に link する場合は native library も静的にする必要があり、そうしないと link 時に `__imp_strtol` などが見つかりません：`--crt static` を付けてください。成果物は別の `<profile>-crt-static` directory に置かれ、Rust が C runtime を静的に link するとき `erika_ffmpeg_sys` と `erika` が自動的に使います。

```powershell
cargo run -p xtask -- deps build --all --profile lgpl --target x86_64-pc-windows-msvc --crt static
```

## ライセンス profile

ネイティブビルドはライセンス境界を明示するため profile で分かれています：

- **`lgpl`**（既定）—— FFmpeg を `--disable-gpl --enable-version3`、静的、ネットワーク
  なし、file プロトコルのみ、厳選した demuxer/decoder/parser セット、zlib 有効、加えて
  VideoToolbox（Apple）、D3D11VA/DXVA2（Windows）、または JNI/MediaCodec（Android）で構成。全ターゲットにソースビルドの dav1d AV1 ソフトウェアデコーダーを含む。
- **`gpl-full`** —— 同じセットに `--enable-gpl`。成果物の GPL 条項を受け入れる場合のみ。

Rust workspace 自体は MPL-2.0（[`LICENSE`](../LICENSE)）。`xtask` と `cargo build` で
profile を一致させます。`cargo run -p xtask -- check license` がポリシーを検証します。

## `dist` レイアウト

ビルド後、ライブラリは（target + profile ごとに）次に配置されます：

```
third_party/
  cache/                       ダウンロードしたアーカイブ
  src/                         展開したソース
  build/<target>/<profile>/    out-of-tree ビルドツリー
  dist/<target>/<profile>/     crate がリンクする install prefix:
    ffmpeg/{include,lib}
    dav1d/   zlib/    libass/    freetype/    harfbuzz/    fribidi/
```

`host` ターゲットでは `<target>` のパスセグメントは省略されます
（`third_party/dist/<profile>/…`）。

## crate が `dist` を見つける仕組み

`erika_ffmpeg_sys/build.rs` が FFmpeg prefix を自動発見します：

1. `ERIKA_FFMPEG_DIR`（設定されていれば明示上書き）。
2. なければ `third_party/dist/$ERIKA_NATIVE_TARGET/$ERIKA_NATIVE_PROFILE/ffmpeg`
   （`ERIKA_NATIVE_TARGET` が設定されている場合）。
3. なければ workspace ルート配下の `third_party/dist/<profile>/ffmpeg`（iOS 向けビルド
   では `ios/` セグメント付き）。

関連する環境変数：`ERIKA_NATIVE_PROFILE`、`ERIKA_NATIVE_TARGET`、`ERIKA_FFMPEG_DIR`、
`ERIKA_DAV1D_DIR`、`ERIKA_ZLIB_DIR`、`LIBCLANG_PATH`、`ERIKA_ALLOW_LEGACY_FFMPEG`（脱出ハッチ）。liberika は
FFmpeg **8.x**（`libavutil >= 60`）を要求し、Windows と Android のネイティブコアはこれを強制します。
`ERIKA_ALLOW_LEGACY_FFMPEG=1` はローカルの互換性実験のときだけ設定してください。

## コンパイルとテスト

```sh
cargo build -p erika                 # コアライブラリ
cargo build -p erika_capi            # C ABI（dylib/staticlib/dll を生成）
cargo test --workspace               # ユニット + 統合テスト
```

`erika_capi` はネイティブホストがリンクする成果物を生成します：

- macOS：`liberika_capi.dylib`（macOS Flutter プラグインが `dlopen` で読み込む。
  `ERIKA_CAPI_DYLIB` で上書き）。
- iOS：`liberika_capi.a`（静的）。
- Windows：`erika_capi.dll`（Flutter Windows プラグインが `build_erika_runtime.cmake`
  でビルド）。
- Android：ABI ごとの `liberika_capi.so` と対応する NDK
  `libc++_shared.so`。ネイティブ埋め込み向けの `liberika_capi.a` も生成されます。

Android の MediaCodec パスは H.264、HEVC、MPEG-2、MPEG-4、VP8、VP9、AV1 を有効にし、
読み取り可能な YUV を共有 wgpu 合成パイプラインへ渡します。ハードウェアデコードですが
CPU upload を伴い、Surface ゼロコピーではありません。AV1 MediaCodec が開けない、または
デコードに失敗した場合、ソフトウェアパスは FFmpeg の `libdav1d` decoder を明示的に選択します。
`xtask` はWindows と OpenHarmony を含む全ターゲット向けに dav1d 1.5.1 をソースからビルドし、8-bit と高ビット深度を
有効にします。32-bit x86 では PIC 安全性のためアセンブリを無効にします。

### Android output negotiation の検証

Android の optional high-headroom mode は `ExtendedLinear` で、FP16
extended-linear scRGB として実装されています。HDR10/PQ ではなく、HDR10 metadata も
出力しません。SDR は通常の `TextureView`、extended-linear の要求時は Flutter Hybrid
Composition の `SurfaceView` を使います。active になるには HDR 対応 display、Vulkan、
wgpu surface capabilities の `Rgba16Float`、configure 後の
`ADATASPACE_SCRGB_LINEAR`（`406913024`、`0x18410000`）readback 成功も必要です。
minimum API は 26 のままですが、API 26/27 は native-window dataspace API が無いため
reason `5` で SDR に明示 fallback します。

Flutter harness は `getOutputStatus()`、native harness は
`erika_presenter_get_output_status()` を使い、requested mode だけで active output を
推測しないでください。non-HDR emulator/device では extended-linear を要求しても再生を
継続し、SDR、非ゼロの `fallbackCount`、安定した `fallbackReason` `0..8`（通常は `1`、
`display_hdr_unsupported`）を報告する必要があります。最終の API 35 HDR 実機 acceptance
では以下をすべて要求します。

- `activeEncoding == AndroidExtendedLinearScRgb`、
  `surfaceFormat == SixteenBitFloat`、`nativeDataSpace == 406913024`、
  `extendedLinearActive == true`、`fallbackReason == None`、かつ
  `extendedLinearFrames` が増加すること。
- resize/rotation と background/foreground recovery 後も同じ状態を維持すること。
- multiple player が独立した surface で動作すること。
- screenshot が常に current composited frame を tone-map した SDR RGBA8 であること。

現在の emulator/non-HDR coverage が検証するのは fallback branch です。上記 API 35 HDR
実機テストが通るまで Android active extended-linear path を実機検証済みとしません。

## 再生パスの検証

```sh
# macOS
export SAMPLE="/path/to/video.mp4"
cargo run -p macos_native_demo -- "$SAMPLE"
cargo run -p macos_native_demo -- --smoke-seconds 3 "$SAMPLE"

# Windows
cargo run -p windows_native_demo -- "%SAMPLE%"
cargo run -p windows_native_demo -- --smoke-seconds 3 --metrics-log out.jsonl "%SAMPLE%"
```

demo はフレームごとのパイプライン統計（デコード/描画フレーム、ゼロコピー vs CPU
フォールバック、HDR10 のアクティブ状況、オーディオ underflow）を出力します——ハード
デコードとゼロコピー相互運用が効いているか手早く確認できます。

## トラブルシューティング

- **「FFmpeg headers were not found …」** —— その target/profile で `xtask deps build` を
  実行していないか、`ERIKA_NATIVE_TARGET`/`ERIKA_NATIVE_PROFILE` がビルドしたものと不一致。
  `deps status` で存在を確認。
- **bindgen / libclang エラー** —— `LIBCLANG_PATH` を LLVM の `lib` ディレクトリに設定。
- **Windows：configure 失敗** —— POSIX シェル（Git Bash/MSYS2）と GNU make が `PATH` 上に
  あり、MSVC 環境から起動していることを確認。
- **Android NDK が見つからない** —— `ANDROID_NDK_HOME` を設定し、
  `build/cmake/android.toolchain.cmake` の存在を確認。
- **Android で extended-linear を要求しても SDR のまま** —— `getOutputStatus()` の
  `fallbackReason` を読み、numeric code をログに残します。主な原因は HDR 非対応 display
  （`1`）、Hybrid Composition ではない（`2`）、GLES（`3`）、`Rgba16Float` 不在（`4`）、
  dataspace API 不在（`5`）、`SCRGB_LINEAR` verification failure（`6`）です。`7` と `8` は
  surface configure failure と unsupported backend での Apple EDR request を表します。
- **旧 FFmpeg が拒否される** —— 7.x バンドルを導入/ビルド。システム FFmpeg に依存しない。
- **license チェック失敗** —— profile に GPL と LGPL の成果物が混在。単一 `--profile` で
  deps を再ビルド。

開発ワークフローは [CONTRIBUTING.ja.md](../CONTRIBUTING.ja.md)、各部分の組み合わせ方は
[architecture.ja.md](architecture.ja.md) を参照してください。
