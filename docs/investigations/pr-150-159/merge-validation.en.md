# 2026-10-13: main-branch integration and live validation

[中文](merge-validation.zh.md) · [日本語](merge-validation.ja.md)

Integration PR: [AimesSoft/liberika #164](https://github.com/AimesSoft/liberika/pull/164). Baseline: `14c1d5511a0a16c34cf2c5f5fe9132d1f17fce65`. Final production code: `211f347acf9d67fb23424d98de436d87931f9318`; subsequent commits only updated validation hosts and evidence. Actual Windows/Linux builds came from `051bfa4`; `211f347` only added an x86_64 OHOS-specific extension exclusion. The original workspace was preserved; all edits and builds were in the review worktree.

## Adopted scope and project goals

Retained the valid parts of #150, #151, #153, #154, #155, #156, #158 and #159, improving HTTP reads, media identification, tolerance of timestamp inputs, WASAPI output, D3D11 resource recreation, pause/seek intent, OHOS object lifetime and public subtitle parsing respectively. These serve embedded playback-core reliability. The original authors were credited in the commits adopting their code.

The #152 HDR filter was withdrawn: the generated real file did not expose the required abnormal mastering luminance through FFmpeg, so the affected path was not established. #157 was not adopted: neither the race premise for synchronous AudioQueueDispose nor completeness of the fix was proven. CoreAudio, Metal and other Apple runtime validation was left to the user; an invalid Linux timeout, invalid compensation on a FIFO, duplicate Dart lifecycle fixes and an arbitrary GIF frame limit were also excluded. See the [original review](review.zh.md) for each rationale. Integration retained main's existing prefix-skipping logic for nonzero-offset HTTP 200 whole-file responses.

## Live runs

The following hosts called production runtimes, device APIs and window systems. Unit-test counts are listed separately and cannot substitute for these runs.

| Environment | Execution and result | Evidence |
|---|---|---|
| Windows / D3D11VA | Real DLL, HWND, local BBB, HTTP 206, whole-file 200 ignoring Range; 64 KB cache forced nonzero-offset requests; pause, EOF seek, backward seek, resume, speed and invalid input; final 463 direct/zero-copy frames, 990 presentations, zero import/render errors | `merge-pinned-windows-live.log` |
| Windows / WASAPI | With Remote Audio available, production default-endpoint submission check passed; cover-art MP3 consumed 30720 frames (host retained 1.5x speed), zero audio errors; real WASAPI loopback then captured nonzero PCM | `merge-wasapi-endpoint.log`, `merge-windows-live.log`, `merge-wasapi-loopback.wav` |
| Linux / X11, Mesa, PulseAudio | Native demo produced 278 video / 144000 audio frames; independent C ABI host exercised display reconnect, seek, resize, resume and destruction; real BBB presented 287 frames; all errors zero | `merge-final-linux-desktop.log` |
| Linux / libass | Real external SRT briefly resumed after seek before pausing again; screenshot showed second subtitle “Second”; cover-art MP3 exposed only audio, consumed 48000 frames, drained queue, zero audio errors | `merge-final-linux-subtitle.log`, `merge-linux-subtitle-second.png` |
| OHOS / official API 18 emulator | Real HAP, XComponent, current Rust runtime and two complete production NAPI bridges completed all procedures below and exported strictly checked results | `merge-api18-ohos-live-summary.json`, two JSONLs, screenshot |
| OHOS / official API 24 emulator | Same final runtime and HAP completed the same procedure on a new official image; the pre-fix initialization crash log was also retained | `merge-api24-ohos-live-summary.json`, two JSONLs, screenshot, `merge-api24-cppcrash.log` |

Both OHOS bridges performed H.264+AAC playback, observed counters after confirming paused state, sought to EOF while paused, sought back to 3.5 seconds, resumed at 1.25x, detached/attached a real NativeWindow, played a 5-second cover-art MP3, made 64 real create/destroy cycles, called and destroyed old IDs, and played again on a new instance. The summarizer required every stage to complete, stable decode/audio consumption while paused, Paused after EOF seek, resumed decoding, no MP3 video frames and a drained audio queue, rejection of old IDs, continued video and audio from the new instance, and zero error counts. Each of the four bridge/image runs consumed 235190 MP3 frames. Raw counts are in the JSON summaries.

## Fixes and host corrections revealed by live runs

1. The API 18 emulator GLES adapter allowed only 7 inter-stage variables; the WebGL2 baseline requested 15, causing real nativeCreate to return 0. The GL path now requests the adapter's limit, and the current video shader creates and presents successfully.
2. The new API 24 image's Vulkan loader advertised cooperative matrix but SIGABRTed at property query. The optional extension's query and exposure are suppressed only for x86_64 OHOS; video rendering does not use it and ARM64 is unaffected.
3. Allocating 64 devices/renderers sometimes exceeded the old host's global playback deadline. The new instance's five-second observation window now starts after actual open completion.
4. Pause is asynchronous, and device consumption can continue between a pre-pause snapshot and command execution. The host now samples after confirming Paused, then observes one second and requires stable decoded and audio-read counts; pause validation was not removed. Runs collected early or stopped midway do not count as final passing evidence.

## Artifacts and recording

| Artifact | SHA256 |
|---|---|
| DLL used in final live Windows run | `1589cdf8f8e6a62afbff9886a8cf914d4d1df1e7afbdcec96cac7dc36736722f` |
| Shared library used in live Linux run | `90a54b9d7d2fae8d87b74ce85ca938c1770421c5674b760582669e6052c92766` |
| Rust runtime used on both OHOS images | `61048133599712676166b807456799ae172cb8077cb6df05f53c716f0743561b` |
| OHOS Flutter production bridge | `f1329a2f9405a8f894491a72c54aecb2f4f303e9ceced786124704dd1b090ecb` |
| OHOS ArkTS production bridge | `6a8737bc254916797683935d60e9099a5f617e9158e5d145f2b588192b40e7f2` |
| HAP used on both final images | `f70b1cdda011e6fde7ce95f394b4775d5ca2abf0dbfefed8e818af24ad0030c0` |
| WASAPI loopback WAV | `4166e50512f96f3c03d7b9273a55948258826e4d3f1833ce5b80617c280e036b` |

The loopback WAV is 48 kHz / stereo / 16-bit / 96000 frames. Rereading it yielded RMS `0.01962782635` and peak `0.09189123203`. The original float capture output had RMS `0.01963214017`, peak `0.09189660847`, 54176 audio frames consumed and zero errors; that successful text was overwritten by a later failed rerun, so the float values come from tool output at the time and the WAV is the retained original evidence. The recording host did not pump video ticks during blocking capture and produced a D3D decoder fallback warning; it only proves real audio output. Final D3D video validation used a separate continuously ticking host.

## Regression checks and failed attempts

Windows: 712 core + 4/6 integration + 42 C API passed, 1 ignored. Linux: 687 core + 4/6 integration + 41 C API + 26 xtask passed, 2 ignored. Flutter: 113 passed and analyze clean; 5 packaging checks passed (including execution of production C++ bridges); formatting and license checks passed. Final remote CI is authoritative in [PR #164's checks](https://github.com/AimesSoft/liberika/pull/164/checks).

The first default parallel Windows suite showed a P010 pixel comparison difference (60 vs 65); the corresponding main-branch unit test and this branch's full single-thread Vulkan suite passed. The initial failure log remains, and this was not presented as a fixed production defect. The HDR file probe failure log also remains. A later WASAPI rerun failed with `0x80070490` after the default endpoint disappeared; the final DLL video run explicitly used `--video-only`. The passing WASAPI production code was unchanged afterward, but the rerun without an endpoint was not counted as an audio pass.

## Coverage limits

- No live Apple evidence; successful platform compilation does not replace the user's Apple validation.
- Windows did not induce a real TDR or validate real failed-device reconnection; D3D11 does not implement the optional C API screenshot.
- Linux used Mesa llvmpipe / Pulse null sink, with no physical VAAPI/NVDEC or audible-speaker evidence.
- OHOS used official x86_64 emulators; the system decoder presented via CPU fallback. No real-device zero-copy evidence. The ArkTS HAP called Flutter's complete production NAPI bridge, not a full Flutter engine or NipaPlay.
- Pure paused subtitle-seek preview remains a separate unresolved observation; the successful screenshot included a brief resume and does not prove that issue fixed.
- #159 changed the public Rust text parsing API; normal external subtitles use FFmpeg, so live subtitle playback only establishes that path was not broken.

Complete raw evidence and artifacts are at local `C:\Users\rgliang\AppData\Local\Temp\erika-pr-review-20261005`. Repository [evidence](evidence/) retains verifiable logs, JSONL, screenshots and loopback recording; see [scripts](scripts/README.md) for reproduction.
