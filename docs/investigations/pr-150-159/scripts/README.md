# Reproducing the native checks

> [中文](README.zh.md) · [English](README.md) · [日本語](README.ja.md)

These are independent hosts of the actual binaries. They complement the
baseline/head regression logs. Actual OHOS emulator results are recorded in
`../ohos-runtime.zh.md`; Apple coverage remains separate.

## Windows

Build this branch's `erika_capi` using the native dependencies described in
`docs/building.zh.md`, MSVC, LLVM, and the Windows SDK. Install Python 3.
Place the resulting DLL at `scripts/target/debug/erika_capi.dll`, and download
the BBB sample described in `../review.zh.md` to `scripts/bbb-10s.mp4`.
Then, from PowerShell:

```powershell
python .\windows-live-capi.py C:\path\to\Erika
```

The host creates a Win32 HWND, attaches the production D3D11 renderer, plays
local media and HTTP 206/whole-file 200 responses with small cache windows,
and exercises pause, EOF seek, resume, rate, MP3 probe and malformed inputs.
The default run requires a real default WASAPI endpoint. Pass `--video-only`
to explicitly omit audio playback when no endpoint is available. D3D11 does
not implement the optional C API screenshot; that limitation is logged.

## Linux

The recorded environment was Ubuntu 26.04 in the existing
`erika-performance-ci:20260930` image, Rust 1.93.1, FFmpeg 8, patched libass,
Xvfb, PulseAudio, and Mesa llvmpipe. Build the branch with libass and wgpu:

```sh
export CARGO_TARGET_DIR=/target
cargo build --locked -p erika_capi -p linux_native_demo
cc examples/linux_native_demo/capi_smoke.c -I crates/erika_capi/include \
  -L /target/debug -Wl,-rpath,/target/debug -lerika_capi -lX11 \
  -o /target/debug/linux_capi_smoke
```

The scripts assume the checkout at `/work`, binaries at `/target/debug`, and
scripts/media/output at `/review-logs`. Use a non-root account, a writable
`/tmp/erika-pr-desktop` directory with mode 0700, and a null PulseAudio sink.

```sh
xvfb-run -a bash /review-logs/native-desktop.sh
xvfb-run -a bash /review-logs/linux-live-subtitle.sh whitespace-separators.srt
```

Create the subtitle sidecar as two SRT cues, `First` from 1–2 seconds and
`Second` from 3–4 seconds, with `space + tab + space` on their separator line.
The host captures a paused frame after a short resumed seek; inspect the
`whitespace-separators-second.png` output for the visible `Second` cue. The
pixel count also includes any video refresh caused by track switching and is
not a measure of the subtitle's area.

The Python host runs the 1-second attached-cover MP3 for 4 seconds and asserts
actual audio consumption. It consumed 48000 frames with an empty final queue
and zero audio errors. A shorter 1.5-second observation initially measured no
consumption and was insufficient to judge the output. The separate native demo
consumed audio on a 5-second MP3, but its generic smoke exit check requires video
and exits 1 for audio-only files. `linux-cover-audio.sh` preserves that result.

Generate its two samples using the repository's attached-cover MP3:

```sh
ffmpeg -y -stream_loop -1 -i /work/crates/erika/testdata/software/mp3-attached-pic.mp3 \
  -t 5 -map 0:a -map 0:v -c:a libmp3lame -c:v copy \
  -disposition:v attached_pic /review-logs/cover-5s.mp3
ffmpeg -y -i /review-logs/cover-5s.mp3 -map 0:a -c copy /review-logs/no-cover-5s.mp3
xvfb-run -a bash /review-logs/linux-cover-audio.sh
```

## OHOS SDK build

Official public archive:

<https://repo.huaweicloud.com/openharmony/os/5.1.0-Release/ohos-sdk-windows_linux-public.tar.gz>

SHA256: `8c1f97df34812e45fa4f0e3275987b261e80622abb7c51392bb063f853451344`.

Extract the Linux native/toolchains/previewer archives to
`/opt/erika-ohos-sdk-5.1.0/linux`, then:

```sh
bash /review-logs/ohos-native-build.sh
```

This builds the full modified Flutter and ArkTS bridges for ARM64 against
API 18, links the SDK libraries, and links the checksum-pinned v0.2.1 C API
release. It does not rebuild this branch's Rust runtime or run an emulator.

## OHOS emulator: actual production runtime

The SDK-only command above is an initial ARM64 link check. For actual playback,
use the official DevEco Studio emulator, a matching x86_64 image, and this
branch's Rust runtime and full production NAPI bridges:

```sh
bash /review-logs/ohos-x86_64-deps-build.sh
bash /review-logs/ohos-x86_64-runtime-build.sh
bash /review-logs/ohos-x86_64-bridges-build.sh
```

Copy these scripts and their CMake project directory from this folder to
`/review-logs` first. `ohos-live/setup.ps1` prepares the ArkTS HAP project and
stages the source and fixture files. Build with `devecocli build`, install
the unsigned HAP on the official emulator with `hdc install`, then start
`com.aimesoft.erika.review/EntryAbility` with `hdc shell aa start`.

The app runs two XComponents, each through a production bridge, and records
video/audio playback, pause, EOF/middle seek, rate changes, actual NativeWindow
detach/attach, MP3 output, 64 create/destroy cycles, and stale-ID rejection.
Export the two JSONL files from filesDir to `ohos-arkts-review.jsonl` and
`ohos-flutter-review.jsonl`, run `python summarize-ohos-live.py /path/to/output`,
and inspect an hdc screenshot.

Alternatively, run the collector to wait for both bridges and check the summary:

```powershell
./collect-ohos-live.ps1 -Hdc C:\path\to\hdc.exe -OutputDirectory C:\path\to\output -ApiVersion 18 -OperatingSystem OpenHarmony-5.1.0.105
```

It leaves the
emulator running on failure or timeout. The current host samples after pause
has settled and compares a second sample, then separately checks the paused
EOF seek. Older JSONL files without `pause-settled` do not satisfy this check.

For Windows audio, install `numpy` and `soundcard`, then run
`python wasapi-loopback.py C:\path\to\Erika` with the same DLL/media layout.
It records two seconds from the real default WASAPI output, requires nonzero
PCM and actual audio consumption, and saves `merge-wasapi-loopback.wav`.

See `../ohos-runtime.zh.md` and `../merge-validation.zh.md` for versions,
binaries, results, failed attempts and coverage limits. This is production
NAPI execution in an ArkTS app; it does not run a full Flutter engine or
establish physical-device zero-copy performance.
