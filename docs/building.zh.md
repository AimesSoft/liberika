# 构建 liberika

浏览器使用当前源码的 `cargo run -p xtask -- web build`，步骤见
[Web 构建与部署指南](web.zh.md)。该入口一次构建 Rust 弹幕、libass 和 FFmpeg
解封装器，产出完整 JS/WASM 包；音视频由 WebCodecs 解码。下文说明原生平台的
依赖与构建流程。

liberika 是一个 Rust workspace,它链接一组**静态构建的原生依赖**(FFmpeg、所有目标的
dav1d AV1 软解回退,以及可选的 libass 字幕栈)。这些原生库不随仓库 vendoring——你用 `xtask` 编排器构建一次,它会把
产物安置到 `third_party/dist/` 下,Rust crate 再链接那个目录。

```
xtask deps build  ──▶  third_party/dist/<target>/<profile>/{ffmpeg,dav1d,zlib,libass,…}
                                        │
                          erika_ffmpeg_sys/build.rs（自动发现 dist,运行 bindgen）
                                        │
                                  cargo build -p erika
```

> 英文版：[building.md](building.md)。

## Linux 系统库构建

Linux 使用系统 FFmpeg 8、PulseAudio 和 liberika 补丁 libass，入口不同于下文
xtask 发布目标。安装依赖和构建步骤在 [Linux 接入](linux.zh.md)。

## 前置依赖

### Rust

- Rust **1.92+**(workspace edition 2024)。
- 交叉目标需安装对应 Rust std target,如
  `rustup target add aarch64-apple-ios` 或
  `rustup target add x86_64-pc-windows-msvc`。
- tvOS 源码构建使用带 `rust-src` 的 nightly 与 Cargo `-Z build-std`
  （`panic_abort` std），与发布工作流一致。

### 构建工具 —— macOS / Unix 宿主

`tar`、`make`、`clang`、`cmake`、`pkg-config`、`python3`(带 `venv`)必须在 `PATH` 上。
基础集的 dav1d 需要 Meson 和 Ninja；缺失时 `xtask` 可通过 Python 虚拟环境安装。
x86_64 Apple 目标即使不使用 `--all`，也需 `nasm` 构建 dav1d 与 FFmpeg 汇编。
macOS 上安装 Xcode Command Line Tools，并按需通过 Homebrew 安装工具。

`erika_ffmpeg_sys` 运行 **bindgen**,需要 `libclang`。若未自动找到,设置 `LIBCLANG_PATH`。

### 构建工具 —— Windows(`x86_64-pc-windows-msvc`)

- **Visual Studio Build Tools**(MSVC)+ Windows SDK,以及 **CMake** 组件。
- 一个 **POSIX shell**(Git for Windows 或 MSYS2)——FFmpeg 的 `configure` 需要它。
- **GNU make**(MSYS2 `make` 或 MinGW `mingw32-make`)。
- x86_64 FFmpeg 与 dav1d 汇编需 `nasm`。
- 每次 Windows 依赖构建均需 **Python**（带 `venv`）；`xtask` 可自动提供 Meson/Ninja 和 `pkg-config` shim。

请在 MSVC 环境已激活的 shell 里运行命令(如 *"x64 Native Tools Command Prompt"*),
以便 `xtask` 定位工具链。

### 构建工具 —— Android

- Android SDK 与 NDK **r29**。`xtask` 会从 `ANDROID_NDK_HOME`、
  `ANDROID_NDK_ROOT`、`ANDROID_HOME`、`ANDROID_SDK_ROOT` 或 Android Studio
  默认 SDK 目录自动发现 NDK。
- CMake、Ninja、GNU make、Python(`venv`)、Meson、`pkg-config`;x86_64
  还需 `nasm`。Windows 宿主使用 Git Bash 执行 FFmpeg `configure`,并可直接使用
  NDK r29 自带的 `make.exe`;同时需要 Visual Studio Build Tools 提供宿主链接器。
- Meson 的宿主生成器需要宿主 C/C++ 编译器。若 `cc` / `c++` 不在 `PATH`,设置
  `CC_FOR_BUILD` / `CXX_FOR_BUILD`。
- 按需安装四个 Rust target:`aarch64-linux-android`、
  `armv7-linux-androideabi`、`x86_64-linux-android`、`i686-linux-android`。
- HarmonyOS:DevEco Studio 的 OpenHarmony Native SDK(由 `OHOS_NDK_HOME` 或
  `OHOS_SDK_NATIVE` 指向),以及 `aarch64-unknown-linux-ohos` Rust target。

Android 最低 API 为 **26**;只在需要更高版本时用 `ANDROID_API_LEVEL` 覆盖。

## 用 `xtask` 构建原生依赖

`xtask` 是一个 workspace 成员,用 `cargo run -p xtask -- …` 调用。

```sh
# 查看将要构建什么(无副作用)
cargo run -p xtask -- deps plan
cargo run -p xtask -- deps status

# 构建基础集(zlib + FFmpeg + dav1d,覆盖所有目标)—— LGPL profile
cargo run -p xtask -- deps build --profile lgpl

# 构建全部,含 libass 字幕栈
cargo run -p xtask -- deps build --all --profile lgpl
```

上例默认使用 `host`。Windows 上应指定 MSVC 目标：Cargo 构建脚本会自动查找目标专用目录，
而 `host` 将依赖放在 `dist/lgpl`。例如 x64：

```powershell
cargo run -p xtask -- deps build --all --profile lgpl --target x86_64-pc-windows-msvc
$env:ERIKA_NATIVE_TARGET = "x86_64-pc-windows-msvc"
cargo build -p erika_capi --target x86_64-pc-windows-msvc
```

Windows ARM64 使用 `aarch64-pc-windows-msvc`。默认 Cargo 构建和 workspace 测试启用
libass，因此需要 `--all`，仅构建基础集不够。

子命令:`plan`(打印计划)、`fetch`(只下载源)、`status`(已有/已构建)、`build`
(下载 + 编译)。

### 选项

| 标志 | 取值 | 默认 | 含义 |
|------|------|------|------|
| `--profile` | `lgpl`、`gpl-full` | `lgpl` | FFmpeg 许可证 profile(见下)。 |
| `--target` | 见目标表 | `host` | 交叉编译目标。 |
| `--all` | — | 关 | 同时构建 libass + FreeType + HarfBuzz + FriBidi(字幕渲染)。所有目标的基础集都是 zlib + FFmpeg + dav1d。 |
| `--force` | — | 关 | 即使已是最新标记也重建。 |
| `--jobs N` | 整数 | 自动 | 原生构建的并行度。 |

### 目标

| `--target` | Triple | 备注 |
|------------|--------|------|
| `host` | 当前机器 | 默认。 |
| `aarch64-apple-darwin` | Apple Silicon macOS | |
| `x86_64-apple-darwin` | Intel macOS | |
| `aarch64-apple-ios` | iOS 设备 | |
| `aarch64-apple-ios-sim` | iOS 模拟器(Apple Silicon) | |
| `x86_64-apple-ios` | iOS 模拟器(Intel) | |
| `aarch64-apple-tvos` | tvOS 设备 | 需要 nightly + `rust-src`。 |
| `aarch64-apple-tvos-sim` | tvOS 模拟器(Apple Silicon) | 需要 nightly + `rust-src`。 |
| `x86_64-apple-tvos` | tvOS 模拟器(Intel) | 需要 nightly + `rust-src`。 |
| `x86_64-pc-windows-msvc`(或 `windows-x64`) | Windows | FFmpeg 里把 VideoToolbox 换成 D3D11VA/DXVA2。 |
| `aarch64-pc-windows-msvc`(或 `windows-arm64`) | Windows ARM64 | 支持 ARM64 原生宿主和 x64 到 ARM64 交叉构建。 |
| `aarch64-linux-android`(或 `arm64-v8a`) | Android arm64 | 主流真机 ABI。 |
| `armv7-linux-androideabi`(或 `armeabi-v7a`) | Android ARMv7 | 32 位 ARM 兼容。 |
| `x86_64-linux-android`(或 `android-x64`) | Android x86_64 | 模拟器与 x86_64 设备。 |
| `i686-linux-android`(或 `x86`) | Android x86 | 32 位模拟器兼容;Android 共享库不允许对应的非 PIC 重定位,因此禁用 x86 汇编加速。 |
| `aarch64-unknown-linux-ohos`(或 `ohos-arm64`) | HarmonyOS arm64 | 使用 DevEco Studio OpenHarmony Native SDK。 |

部署最低版本默认 macOS `11.0` / iOS `13.0` / tvOS `13.0`，可用
`MACOSX_DEPLOYMENT_TARGET` / `IPHONEOS_DEPLOYMENT_TARGET` /
`TVOS_DEPLOYMENT_TARGET` 覆盖。

Android 使用 `android-26`、`c++_shared`、PIC 静态依赖和所选 ABI 对应的 NDK LLVM
工具链。

HarmonyOS 使用 `OHOS_NDK_HOME` 或 `OHOS_SDK_NATIVE` 指向 DevEco Studio 的
`openharmony/native` 目录，并通过其中的
`aarch64-unknown-linux-ohos-clang` 构建 FFmpeg、zlib 和 liberika。Flutter 插件的
Hvigor/CMake 构建会自动执行这条链路，并把 `liberika_capi.so` 与
`liberika_flutter.so` 打入 HAP。

### 选择源码构建架构

Flutter 依赖项目通过 `ERIKA_MACOS_ARCHS=arm64|x86_64|universal` 选择 macOS 架构，通过 `ERIKA_WINDOWS_ARCH=x64|arm64` 选择 Windows 架构，通过 `ERIKA_ANDROID_ABIS=arm64-v8a,armeabi-v7a,x86_64,x86` 选择 Android ABI。设置 `ERIKA_FORCE_SOURCE_BUILD=1` 可强制跳过预构建包。

Apple 插件默认使用带校验的预构建归档，唯一例外是 liberika checkout 内的 macOS：pod 脚本在包上层发现 `crates/erika_capi/Cargo.toml`（或 `ERIKA_REPO_ROOT` 指向 checkout）时，会改为从源码构建 `erika_capi`，使本地改动无需等待新的预构建发布即可生效。设置 `ERIKA_FORCE_PREBUILT=1` 可强制始终下载归档——仓库内没有 Rust 工具链的消费者请使用该开关；注意解析到 pub cache 的 git 依赖保留了整个仓库，因此同样会切换为源码构建。

直接构建原生库时，三个目标参数必须一致：

```sh
cargo run -p xtask -- deps build --all --profile lgpl --target aarch64-apple-darwin
ERIKA_NATIVE_PROFILE=lgpl ERIKA_NATIVE_TARGET=aarch64-apple-darwin \
  cargo build -p erika_capi --release --target aarch64-apple-darwin
```

tvOS 的 Rust 目标虽已是 tier 2，但 liberika 直接构建时仍用 nightly 并从 `rust-src`
构建标准库：

```sh
rustup toolchain install nightly --component rust-src
cargo run -p xtask -- deps build --all --profile lgpl \
  --target aarch64-apple-tvos-sim
ERIKA_NATIVE_PROFILE=lgpl ERIKA_NATIVE_TARGET=aarch64-apple-tvos-sim \
  cargo +nightly rustc -Z build-std=std,panic_abort -p erika_capi --release \
  --target aarch64-apple-tvos-sim --no-default-features --features libass \
  --lib --crate-type staticlib
```

Windows ARM64 使用对应的 PowerShell 命令：

```powershell
cargo run -p xtask -- deps build --all --profile lgpl --target aarch64-pc-windows-msvc
$env:ERIKA_NATIVE_PROFILE = "lgpl"
$env:ERIKA_NATIVE_TARGET = "aarch64-pc-windows-msvc"
cargo build -p erika_capi --release --target aarch64-pc-windows-msvc
```

`xtask --target`、`ERIKA_NATIVE_TARGET` 和 Cargo `--target` 必须相同，否则 Cargo 可能链接到其他架构的原生依赖。

Windows 原生库默认链接 C 运行库的 DLL（`/MD`），与 Rust 的默认相同。宿主用 `-C target-feature=+crt-static` 静态链接 C 运行库时，原生库也要静态链接，否则链接时缺 `__imp_strtol` 等符号：加 `--crt static`，产物放在单独的 `<profile>-crt-static` 目录，`erika_ffmpeg_sys` 与 `erika` 发现 Rust 静态链接 C 运行库时自动使用它。

```powershell
cargo run -p xtask -- deps build --all --profile lgpl --target x86_64-pc-windows-msvc --crt static
```

### Windows PowerShell 构建 Android x86_64

```powershell
$env:ANDROID_NDK_HOME = "$env:LOCALAPPDATA\Android\Sdk\ndk\29.0.14206865"
$env:ANDROID_NDK_ROOT = $env:ANDROID_NDK_HOME
$env:ANDROID_API_LEVEL = "26"

rustup target add x86_64-linux-android
cargo run -p xtask -- deps build --all --profile lgpl --target x86_64-linux-android

$bin = "$env:ANDROID_NDK_HOME\toolchains\llvm\prebuilt\windows-x86_64\bin"
$prebuilt = Split-Path $bin -Parent
$env:CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER = "$bin\x86_64-linux-android26-clang.cmd"
$env:CC_X86_64_LINUX_ANDROID = $env:CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER
$env:CXX_X86_64_LINUX_ANDROID = "$bin\x86_64-linux-android26-clang++.cmd"
$env:AR_X86_64_LINUX_ANDROID = "$bin\llvm-ar.exe"
$env:LIBCLANG_PATH = $bin
$env:Path = "$bin;$env:Path"
$env:BINDGEN_EXTRA_CLANG_ARGS_x86_64_linux_android = "--target=x86_64-linux-android26 --sysroot=`"$prebuilt\sysroot`""
$env:CFLAGS_x86_64_linux_android = "-fPIC"
$env:CXXFLAGS_x86_64_linux_android = "-fPIC"
$env:ERIKA_NATIVE_PROFILE = "lgpl"
$env:ERIKA_NATIVE_TARGET = "x86_64-linux-android"

cargo build -p erika_capi --release --target x86_64-linux-android `
  --no-default-features --features libass,wgpu
```

Cargo 会在 `target/x86_64-linux-android/release` 下同时生成
`liberika_capi.so` 和 `liberika_capi.a`。Flutter Android Gradle 任务直接打包
cdylib 与匹配 ABI 的 NDK `libc++_shared.so`。

## 许可证 profile

原生构建按 profile 分割,使许可证边界明确:

- **`lgpl`**(默认)—— FFmpeg 配置为 `--disable-gpl --enable-version3`,静态,无网络,
  仅 file 协议,一组精选的 demuxer/decoder/parser,启用 zlib,外加 VideoToolbox(Apple)、
  D3D11VA/DXVA2(Windows)或 JNI/MediaCodec(Android)。所有目标均包含源码构建的 dav1d AV1 软解器。
- **`gpl-full`** —— 同一集合加 `--enable-gpl`。仅当你接受产物的 GPL 条款时使用。

Rust workspace 本身是 MPL-2.0(见 [`LICENSE`](../LICENSE))。`xtask` 与你的
`cargo build` 之间保持 profile 一致。`cargo run -p xtask -- check license` 检查默认 feature、
FFmpeg 配置标志和 dav1d 许可声明，不检查已构建产物，也不检测 profile 混用。

## `dist` 布局

构建后,库会落到(按 target + profile):

```
third_party/
  cache/                       下载的归档
  src/                         解压后的源
  build/<target>/<profile>/    out-of-tree 构建树
  dist/<target>/<profile>/     crate 链接的安装前缀:
    ffmpeg/{include,lib}
    dav1d/   zlib/    libass/    freetype/    harfbuzz/    fribidi/
```

对 `host` 目标,`<target>` 路径段省略(`third_party/dist/<profile>/…`)。

## crate 如何找到 `dist`

`erika_ffmpeg_sys/build.rs` 自动发现 FFmpeg 前缀:

1. `ERIKA_FFMPEG_DIR`(若设置,显式覆盖)。
2. 否则 `third_party/dist/$ERIKA_NATIVE_TARGET/$ERIKA_NATIVE_PROFILE/ffmpeg`
   (若 `ERIKA_NATIVE_TARGET` 已设)。
3. 否则为 Windows、Android、iOS（`ios`）与 tvOS 推断目标目录；其他目标若存在匹配的
   Cargo 目标目录则使用它，否则使用 `third_party/dist/<profile>/ffmpeg`。设置
   `ERIKA_NATIVE_TARGET` 可明确选择所需目录。

相关环境变量:`ERIKA_NATIVE_PROFILE`、`ERIKA_NATIVE_TARGET`、`ERIKA_FFMPEG_DIR`、
`ERIKA_DAV1D_DIR`、`ERIKA_ZLIB_DIR`、`LIBCLANG_PATH`,以及 `ERIKA_ALLOW_LEGACY_FFMPEG`(应急开关)。liberika
需要 FFmpeg **8.x**（`libavutil >= 60`）。`ERIKA_ALLOW_LEGACY_FFMPEG=1` 仅供本地
兼容性实验，且只跳过 bundled 依赖的版本检查；Linux 系统库路径仍通过 pkg-config 要求 FFmpeg 8。

## 编译与测试

```sh
cargo build -p erika                 # 核心库
cargo build -p erika_capi            # C ABI(产出 dylib/staticlib/dll)
cargo test --workspace               # 单元 + 集成测试
```

`erika_capi` 产出原生宿主链接的工件:

- macOS:`liberika_capi.dylib`(macOS Flutter 插件用 `dlopen` 加载;用 `ERIKA_CAPI_DYLIB`
  覆盖)。
- iOS:`liberika_capi.a`(静态)。
- Windows:`erika_capi.dll`(Flutter Windows 插件通过 `build_erika_runtime.cmake` 构建)。
- Android:每 ABI 一个 `liberika_capi.so` 与匹配的 NDK
  `libc++_shared.so`;同时保留 `liberika_capi.a` 供原生嵌入方使用。

Android FFmpeg 明确保留 H.264、HEVC、MPEG-2、MPEG-4、VP8、VP9、AV1 的
MediaCodec decoder。主路径让 MediaCodec 输出软件可读 YUV,继续复用共享 wgpu 上传、
字幕、弹幕和截图合成。这是“硬解 + CPU upload”,不是 Surface 零拷贝,统计与日志必须
如实区分。AV1 MediaCodec 无法打开或解码失败时,软件路径会显式选择 FFmpeg 的
`libdav1d` decoder。`xtask` 为所有目标(包括 Windows 与 OpenHarmony)从源码构建 dav1d 1.5.1,同时支持
8-bit 与高位深;32 位 x86 为保证 PIC 安全会禁用汇编。

### 验证 Android 输出协商

Android 可选的高 headroom 模式是 `ExtendedLinear`，实现为 FP16 extended-linear
scRGB；它不是 HDR10/PQ，也不会输出 HDR10 metadata。SDR 使用普通 `TextureView`；请求
extended-linear 时使用由 Flutter Hybrid Composition 承载的 `SurfaceView`。真正激活还要求
HDR 显示器、Vulkan、wgpu surface capabilities 包含 `Rgba16Float`，并在 configure 后成功
回读 `ADATASPACE_SCRGB_LINEAR`（`406913024`、`0x18410000`）。最低版本仍是 API 26，
但 API 26/27 没有 native-window dataspace API，因此会以原因 `5` 明确回退 SDR。

Flutter harness 应调用 `getOutputStatus()`，原生 harness 应调用
`erika_presenter_get_output_status()`；不能只根据请求模式判断实际输出。在非 HDR
模拟器/设备上，请求 extended-linear 后应继续正常播放，并报告 SDR、非零
`fallbackCount` 和稳定的 `0..8` `fallbackReason`（通常为 `1`，
`display_hdr_unsupported`）。最终 API 35 HDR 真机验收必须同时满足：

- `activeEncoding == AndroidExtendedLinearScRgb`、
  `surfaceFormat == SixteenBitFloat`、`nativeDataSpace == 406913024`、
  `extendedLinearActive == true`、`fallbackReason == None`，且
  `extendedLinearFrames` 持续增长；
- resize/旋转和前后台恢复后仍保持上述状态；
- 多 player 使用相互独立的 surface；
- 截图始终是从当前合成帧 tone-map 得到的 SDR RGBA8。

当前模拟器/非 HDR 覆盖验证的是回退分支。在上述 API 35 HDR 真机测试通过前，不应把
Android active extended-linear 路径标为“已通过真机验证”。

## 验证播放路径

macOS（shell）：

```sh
SAMPLE="/path/to/video.mp4"
cargo run -p macos_native_demo -- "$SAMPLE"
cargo run -p macos_native_demo -- --smoke-seconds 3 "$SAMPLE"
```

Windows（PowerShell）：

```powershell
$sample = "C:\path\to\video.mp4"
cargo run -p windows_native_demo -- $sample
cargo run -p windows_native_demo -- --smoke-seconds 3 --metrics-log out.jsonl $sample
```

Windows 冒烟测试结束时打印解码、零拷贝、HDR10 和音频等汇总指标；`--metrics-log`
约每 250 ms 写入一次指标。macOS demo 默认报告播放和错误信息；使用 `--seek-sequence`
时还会输出解码、渲染与失败计数。

## 排错

- **"FFmpeg headers were not found …"** —— 你没为该 target/profile 跑 `xtask deps build`,
  或 `ERIKA_NATIVE_TARGET`/`ERIKA_NATIVE_PROFILE` 与你构建的不匹配。跑 `deps status` 看
  现有什么。
- **bindgen / libclang 报错** —— 把 `LIBCLANG_PATH` 设到你的 LLVM `lib` 目录。
- **Windows:configure 失败** —— 确保 POSIX shell(Git Bash/MSYS2)和 GNU make 在 `PATH`
  上,且你是从 MSVC 环境启动的。
- **找不到 Android NDK** —— 显式设置 `ANDROID_NDK_HOME`,并确认其下存在
  `build/cmake/android.toolchain.cmake`。
- **Android Meson 宿主工具失败** —— 安装宿主编译器或设置 `CC_FOR_BUILD` /
  `CXX_FOR_BUILD`;这些生成器运行在构建机上,不是 Android 上。
- **Android 请求 extended-linear 后仍为 SDR** —— 从 `getOutputStatus()` 读取
  `fallbackReason`，并在日志中保留数值码。常见原因是显示器不支持 HDR（`1`）、没有
  Hybrid Composition（`2`）、使用 GLES（`3`）、没有 `Rgba16Float`（`4`）、dataspace
  API 不可用（`5`）或 `SCRGB_LINEAR` 验证失败（`6`）。`7` 与 `8` 分别表示 surface
  configure 失败和在不支持的 backend 上请求 Apple EDR。
- **旧版 FFmpeg 被拒** —— 安装或构建 FFmpeg 8.x。`ERIKA_ALLOW_LEGACY_FFMPEG=1` 只在本地兼容性实验中跳过 bundled 依赖检查，不会跳过 Linux 系统库的 pkg-config 版本要求。
- **license 校验失败** —— 根据报错检查默认 feature、FFmpeg 配置标志或 dav1d 许可声明。
  此检查不扫描构建产物；需另行确认依赖 profile 与 Cargo 构建一致。

开发工作流见 [CONTRIBUTING.zh.md](../CONTRIBUTING.zh.md),各部分如何拼接见
[architecture.zh.md](architecture.zh.md)。
