# Building liberika

> Translations: [中文](building.zh.md) · [日本語](building.ja.md)

For browsers, use `cargo run -p xtask -- web build` from current source and
follow the [Web build and deployment guide](web.md). That entry point builds
the Rust danmaku engine, libass and FFmpeg demuxer into a complete JS/WASM
package; audio/video decoding uses WebCodecs. The steps below cover native
platform dependencies and builds.

liberika is a Rust workspace that links a set of **statically built native
dependencies** (FFmpeg, dav1d for AV1 software decode on every target, and optionally
the libass subtitle stack). Those native libraries are not vendored — you build them once with the `xtask` orchestrator,
which stages them under `third_party/dist/`, and the Rust crates link against
that staging directory.

```
xtask deps build  ──▶  third_party/dist/<target>/<profile>/{ffmpeg,dav1d,zlib,libass,…}
                                        │
                          erika_ffmpeg_sys/build.rs  (auto-discovers dist, runs bindgen)
                                        │
                                  cargo build -p erika
```

## Linux system-library builds

Linux uses system FFmpeg 8 and PulseAudio, with Erika-patched libass. It does
not use the xtask release-target path below. Start with [Linux integration](linux.md).

## Prerequisites

### Rust

- Rust **1.92+** (workspace edition 2024).
- For cross-targets, add the Rust std target, e.g.
  `rustup target add aarch64-apple-ios` or
  `rustup target add x86_64-pc-windows-msvc`.
- tvOS source builds use nightly with `rust-src` and Cargo's `-Z build-std`
  (`panic_abort` std), matching the release workflow.

### Build tools — macOS / Unix host

`tar`, `make`, `clang`, `cmake`, `pkg-config`, and `python3` (with `venv`) must
be on `PATH`. Building the full subtitle stack (`--all`) additionally needs
`meson` and `ninja` (and `nasm` for FFmpeg's x86 assembly on Intel hosts). On
macOS, install the Xcode Command Line Tools plus the above via Homebrew.

`erika_ffmpeg_sys` runs **bindgen**, which needs `libclang`. If it is not found
automatically, set `LIBCLANG_PATH`.

### Build tools — Windows (`x86_64-pc-windows-msvc`)

- **Visual Studio Build Tools** (MSVC) + Windows SDK, and the **CMake**
  component.
- A **POSIX shell** (Git for Windows or MSYS2) — FFmpeg's `configure` needs it.
- **GNU make** (MSYS2 `make` or MinGW `mingw32-make`).
- `nasm` for FFmpeg assembly.
- For `--all`: **Python** (with `venv`); `xtask` provisions a `pkg-config` shim
  automatically.

Run the commands from a shell where the MSVC environment is active (e.g. a
*"x64 Native Tools Command Prompt"*), so `xtask` can locate the toolchain.

### Build tools — Android

- Android SDK plus NDK **r29** (the build also accepts another modern
  side-by-side NDK). `xtask` discovers it from `ANDROID_NDK_HOME`,
  `ANDROID_NDK_ROOT`, `ANDROID_HOME`, `ANDROID_SDK_ROOT`, or the standard
  Android Studio SDK directory.
- CMake, Ninja, GNU make, `nasm` (for x86_64), Python with `venv`, Meson,
  and `pkg-config`. On Windows, Git Bash supplies the POSIX shell required by
  FFmpeg, Visual Studio Build Tools supplies the host linker, and NDK r29's
  bundled `make.exe` is discovered automatically.
- A host C/C++ compiler for Meson-generated build tools. Set `CC_FOR_BUILD` and
  `CXX_FOR_BUILD` when `cc` / `c++` are not on `PATH`.
- Rust standard-library targets for the ABIs you build:
  `aarch64-linux-android`, `armv7-linux-androideabi`,
  `x86_64-linux-android`, and `i686-linux-android`.
- For HarmonyOS: the DevEco Studio OpenHarmony Native SDK, pointed at by
  `OHOS_NDK_HOME` or `OHOS_SDK_NATIVE`, plus the
  `aarch64-unknown-linux-ohos` Rust standard-library target.

liberika's Android minimum is API **26**. Override it with
`ANDROID_API_LEVEL` only when targeting a newer API.

## Native dependencies via `xtask`

`xtask` is a workspace member; invoke it with `cargo run -p xtask -- …`.

```sh
# Inspect what would be built (no side effects)
cargo run -p xtask -- deps plan
cargo run -p xtask -- deps status

# Build the baseline set (zlib + FFmpeg + dav1d on every target) — LGPL profile
cargo run -p xtask -- deps build --profile lgpl

# Build everything, including the libass subtitle stack
cargo run -p xtask -- deps build --all --profile lgpl
```

Subcommands: `plan` (print the plan), `fetch` (download sources only),
`status` (what is present/built), `build` (fetch + compile).

### Options

| Flag | Values | Default | Meaning |
|------|--------|---------|---------|
| `--profile` | `lgpl`, `gpl-full` | `lgpl` | FFmpeg license profile (see below). |
| `--target` | see targets table | `host` | Cross-compile target. |
| `--all` | — | off | Also build libass + FreeType + HarfBuzz + FriBidi (subtitle rendering). The baseline is zlib + FFmpeg + dav1d on every target. |
| `--force` | — | off | Rebuild even if up-to-date markers exist. |
| `--jobs N` | integer | auto | Parallelism for the native builds. |

### Targets

| `--target` | Triple | Notes |
|------------|--------|-------|
| `host` | current machine | Default. |
| `aarch64-apple-darwin` | Apple Silicon macOS | |
| `x86_64-apple-darwin` | Intel macOS | |
| `aarch64-apple-ios` | iOS device | |
| `aarch64-apple-ios-sim` | iOS sim (Apple Silicon) | |
| `x86_64-apple-ios` | iOS sim (Intel) | |
| `aarch64-apple-tvos` | tvOS device | Requires nightly + `rust-src`. |
| `aarch64-apple-tvos-sim` | tvOS sim (Apple Silicon) | Requires nightly + `rust-src`. |
| `x86_64-apple-tvos` | tvOS sim (Intel) | Requires nightly + `rust-src`. |
| `x86_64-pc-windows-msvc` (or `windows-x64`) | Windows | Swaps VideoToolbox for D3D11VA/DXVA2 in FFmpeg. |
| `aarch64-pc-windows-msvc` (or `windows-arm64`) | Windows ARM64 | Supports native ARM64 hosts and x64-to-ARM64 cross builds. |
| `aarch64-linux-android` (or `arm64-v8a`) | Android arm64 | Flutter/Play primary device ABI. |
| `armv7-linux-androideabi` (or `armeabi-v7a`) | Android ARMv7 | 32-bit ARM compatibility ABI. |
| `x86_64-linux-android` (or `android-x64`) | Android x86_64 | Emulator and x86_64 device ABI. |
| `i686-linux-android` (or `x86`) | Android x86 | 32-bit emulator compatibility ABI; x86 assembly acceleration is disabled because Android shared libraries cannot contain its non-PIC relocations. |
| `aarch64-unknown-linux-ohos` (or `ohos-arm64`) | HarmonyOS arm64 | Uses the DevEco Studio OpenHarmony Native SDK. |

Deployment minimums default to macOS `11.0` / iOS `13.0` / tvOS `13.0` and can
be overridden with `MACOSX_DEPLOYMENT_TARGET` /
`IPHONEOS_DEPLOYMENT_TARGET` / `TVOS_DEPLOYMENT_TARGET`.

Android builds use `android-26`, `c++_shared`, PIC static dependencies, and the
NDK LLVM toolchain selected for the requested ABI.

HarmonyOS builds take the DevEco Studio `openharmony/native` directory from
`OHOS_NDK_HOME` or `OHOS_SDK_NATIVE` and build FFmpeg, zlib, and liberika with the
`aarch64-unknown-linux-ohos-clang` toolchain inside it. The Flutter plugin's
Hvigor/CMake build runs this chain automatically and packages `liberika_capi.so`
and `liberika_flutter.so` into the HAP.

### Selecting source-build architectures

Flutter consumers select macOS with `ERIKA_MACOS_ARCHS=arm64|x86_64|universal`,
Windows with `ERIKA_WINDOWS_ARCH=x64|arm64`, and Android with
`ERIKA_ANDROID_ABIS=arm64-v8a,armeabi-v7a,x86_64,x86`. Set
`ERIKA_FORCE_SOURCE_BUILD=1` to bypass prebuilt downloads.

The Apple plugins default to the checksummed prebuilt archive, except on macOS
inside a liberika checkout: when the pod script finds
`crates/erika_capi/Cargo.toml` above the package (or `ERIKA_REPO_ROOT` points
at one), it builds `erika_capi` from source so local changes are picked up
without a new prebuilt release. `ERIKA_FORCE_PREBUILT=1` always downloads the
archive instead — use this for in-repo consumers without a Rust toolchain;
note that git dependencies resolved into the pub cache keep the whole
repository and therefore also switch to source builds.

For direct native builds, keep all three target selectors identical:

```sh
cargo run -p xtask -- deps build --all --profile lgpl --target aarch64-apple-darwin
ERIKA_NATIVE_PROFILE=lgpl ERIKA_NATIVE_TARGET=aarch64-apple-darwin \
  cargo build -p erika_capi --release --target aarch64-apple-darwin
```

Direct tvOS builds use nightly and build the standard library from `rust-src`
(the CI pipeline runs `-Z build-std=std,panic_abort`):

```sh
rustup toolchain install nightly --component rust-src
cargo run -p xtask -- deps build --all --profile lgpl \
  --target aarch64-apple-tvos-sim
ERIKA_NATIVE_PROFILE=lgpl ERIKA_NATIVE_TARGET=aarch64-apple-tvos-sim \
  cargo +nightly rustc -Z build-std=std,panic_abort -p erika_capi --release \
  --target aarch64-apple-tvos-sim --no-default-features --features libass \
  --lib --crate-type staticlib
```

On Windows ARM64, use the equivalent PowerShell commands:

```powershell
cargo run -p xtask -- deps build --all --profile lgpl --target aarch64-pc-windows-msvc
$env:ERIKA_NATIVE_PROFILE = "lgpl"
$env:ERIKA_NATIVE_TARGET = "aarch64-pc-windows-msvc"
cargo build -p erika_capi --release --target aarch64-pc-windows-msvc
```

The `xtask --target`, `ERIKA_NATIVE_TARGET`, and Cargo `--target` values must
match, otherwise Cargo can attempt to link native dependencies for a different
architecture.

Windows native libraries link the DLL C runtime (`/MD`) by default, as Rust
does. A host that links the C runtime statically
(`-C target-feature=+crt-static`) needs native libraries that do too, or the
link misses symbols such as `__imp_strtol`: add `--crt static`. The build goes
to its own `<profile>-crt-static` directory, which `erika_ffmpeg_sys` and
`erika` use when Rust links the C runtime statically.

```powershell
cargo run -p xtask -- deps build --all --profile lgpl --target x86_64-pc-windows-msvc --crt static
```

### Android x86_64 build (Windows PowerShell)

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

Cargo produces both `liberika_capi.so` and `liberika_capi.a` under
`target/x86_64-linux-android/release`. The Flutter Android Gradle task packages
the cdylib directly together with the matching NDK `libc++_shared.so`.

## License profiles

The native build is split into profiles so the license boundary is explicit:

- **`lgpl`** (default) — FFmpeg configured `--disable-gpl --enable-version3`,
  static, no network, file protocol only, a curated demuxer/decoder/parser set,
  zlib enabled, plus VideoToolbox (Apple), D3D11VA/DXVA2 (Windows), or
  JNI/MediaCodec (Android). Every target also includes source-built dav1d
  for AV1 software decode.
- **`gpl-full`** — the same set with `--enable-gpl`. Use only if you accept GPL
  terms for the resulting binary.

The Rust workspace itself is MPL-2.0 (see [`LICENSE`](../LICENSE)). Keep the
profile consistent across `xtask` and your `cargo build`. `cargo run -p xtask --
check license` validates the policy.

## The `dist` layout

After a build, libraries land under (per target + profile):

```
third_party/
  cache/                       downloaded archives
  src/                         extracted sources
  build/<target>/<profile>/    out-of-tree build trees
  dist/<target>/<profile>/     install prefixes the crates link:
    ffmpeg/{include,lib}
    dav1d/   zlib/    libass/    freetype/    harfbuzz/    fribidi/
```

For the `host` target the `<target>` path segment is omitted
(`third_party/dist/<profile>/…`).

## How the crates find `dist`

`erika_ffmpeg_sys/build.rs` discovers the FFmpeg prefix automatically:

1. `ERIKA_FFMPEG_DIR`, if set (explicit override).
2. else `third_party/dist/$ERIKA_NATIVE_TARGET/$ERIKA_NATIVE_PROFILE/ffmpeg`,
   if `ERIKA_NATIVE_TARGET` is set.
3. else `third_party/dist/<profile>/ffmpeg` under the workspace root (with an
   `ios/` segment when building for iOS).

Relevant environment variables: `ERIKA_NATIVE_PROFILE`, `ERIKA_NATIVE_TARGET`,
`ERIKA_FFMPEG_DIR`, `ERIKA_DAV1D_DIR`, `ERIKA_ZLIB_DIR`, `LIBCLANG_PATH`, and
`ERIKA_ALLOW_LEGACY_FFMPEG` (escape hatch). liberika requires FFmpeg **8.x**
(`libavutil >= 60`); the Windows and Android native core enforces this. Set
`ERIKA_ALLOW_LEGACY_FFMPEG=1` only for local compatibility experiments.

## Compile and test

```sh
cargo build -p erika                 # core library
cargo build -p erika_capi            # C ABI (produces the dylib/staticlib/dll)
cargo test --workspace               # unit + integration tests
```

`erika_capi` produces the artifact native hosts link:

- macOS: `liberika_capi.dylib` (loaded via `dlopen` by the macOS Flutter plugin;
  override with `ERIKA_CAPI_DYLIB`).
- iOS: `liberika_capi.a` (static).
- Windows: `erika_capi.dll` (the Flutter Windows plugin builds it through
  `build_erika_runtime.cmake`).
- Android: per-ABI `liberika_capi.so` plus the matching NDK
  `libc++_shared.so`; `liberika_capi.a` is also available for native embedders.

Android FFmpeg keeps the H.264, HEVC, MPEG-2, MPEG-4, VP8, VP9, and AV1
MediaCodec decoders enabled. The intended hardware path asks MediaCodec for
software-readable YUV frames and reuses the shared wgpu upload/composition
pipeline, preserving subtitles, danmaku, and screenshots. This is hardware
decode with a CPU upload, not a zero-copy Surface path; metrics must report it
accordingly. If AV1 MediaCodec cannot open or fails while decoding, the software
path explicitly selects FFmpeg's `libdav1d` decoder. `xtask` builds dav1d 1.5.1
from source for every target, including Windows and OpenHarmony, with both 8-bit and
high-bit-depth support; the 32-bit Android x86 slice disables assembly to
preserve PIC safety.

### Verify Android output negotiation

Android's optional high-headroom mode is `ExtendedLinear`, implemented as FP16
extended-linear scRGB. It is not HDR10/PQ and does not emit HDR10 metadata. SDR
uses the normal `TextureView`; an extended-linear request uses a `SurfaceView`
hosted with Flutter Hybrid Composition. Activation additionally requires an
HDR-capable display, Vulkan, `Rgba16Float` in the wgpu surface capabilities,
and successful post-configure readback of `ADATASPACE_SCRGB_LINEAR`
(`406913024`, `0x18410000`). API 26 remains supported, but API 26/27 cannot use
the native-window dataspace API and therefore fall back to SDR with reason `5`.

Use `getOutputStatus()` in Flutter or
`erika_presenter_get_output_status()` in a native harness; never infer active
output from the requested mode alone. On a non-HDR emulator/device, an
extended-linear request should remain playable and report SDR, a nonzero
`fallbackCount`, and a stable `fallbackReason` in `0..8` (normally `1`,
`display_hdr_unsupported`). On the final API 35 HDR-device acceptance run,
require all of the following:

- `activeEncoding == AndroidExtendedLinearScRgb`,
  `surfaceFormat == SixteenBitFloat`, `nativeDataSpace == 406913024`,
  `extendedLinearActive == true`, `fallbackReason == None`, and increasing
  `extendedLinearFrames`;
- the same state survives resize/rotation and background/foreground recovery;
- independent surfaces work with multiple players; and
- screenshots remain SDR RGBA8, tone-mapped from the current composited frame.

Current emulator/non-HDR coverage validates the fallback branch. Do not mark
the active Android extended-linear path as device-validated until the API 35
HDR-device run above passes.

## Verify the playback path

```sh
# macOS
export SAMPLE="/path/to/video.mp4"
cargo run -p macos_native_demo -- "$SAMPLE"
cargo run -p macos_native_demo -- --smoke-seconds 3 "$SAMPLE"

# Windows
cargo run -p windows_native_demo -- "%SAMPLE%"
cargo run -p windows_native_demo -- --smoke-seconds 3 --metrics-log out.jsonl "%SAMPLE%"
```

The demos print per-frame pipeline stats (decoded/rendered frames, zero-copy vs
CPU fallback, HDR10 activity, audio underflow) — a quick way to confirm hardware
decode and zero-copy interop are engaged.

## Troubleshooting

- **"FFmpeg headers were not found …"** — you haven't run `xtask deps build` for
  this target/profile, or `ERIKA_NATIVE_TARGET`/`ERIKA_NATIVE_PROFILE` don't
  match what you built. Run `deps status` to see what's present.
- **bindgen / libclang errors** — set `LIBCLANG_PATH` to your LLVM `lib`
  directory.
- **Windows: configure fails** — ensure a POSIX shell (Git Bash/MSYS2) and GNU
  make are on `PATH`, and that you launched from an MSVC environment.
- **Android NDK not found** — set `ANDROID_NDK_HOME` explicitly and verify
  `build/cmake/android.toolchain.cmake` exists beneath it.
- **Android Meson native-tool failure** — install a host compiler or set
  `CC_FOR_BUILD` / `CXX_FOR_BUILD`; these compile generators that run on the
  build machine, not Android code.
- **Android extended-linear request remains SDR** — read `fallbackReason` from
  `getOutputStatus()` and keep the numeric code in logs. Common causes are a
  non-HDR display (`1`), non-Hybrid composition (`2`), GLES (`3`), missing
  `Rgba16Float` (`4`), unavailable dataspace API (`5`), or failed
  `SCRGB_LINEAR` verification (`6`). Codes `7` and `8` identify surface
  configuration failure and an Apple-EDR request on an unsupported backend.
- **Legacy FFmpeg rejected** — the native core requires the 8.x bundle
  (`libavutil >= 60`) and fails at build time otherwise; the check is skipped
  only when `ERIKA_ALLOW_LEGACY_FFMPEG=1` is set for local experiments.
- **License check fails** — your profile mixes GPL and LGPL artifacts; rebuild
  deps with a single `--profile`.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the development workflow and
[architecture.md](architecture.md) for how the pieces fit together.
