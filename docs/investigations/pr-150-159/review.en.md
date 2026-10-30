# PR #150–#159: Purpose, trade-offs, and runtime verification

Investigation: 2026-10-05; HarmonyOS runtime verification added: 2026-10-08; latest main-branch integration: 2026-10-13.<br>
Repository: AimesSoft/liberika. Author: TsangAsuna.<br>
Evaluation baseline: `4390c78f4467e2047e19690700fbead6a18d8e31`.<br>
Playback-kernel review commit: `004b118`; subsequent branch commits provide emulator build support and runtime evidence.<br>
Branch: `codex/pr-150-159-reviewed`.

Erika is NipaPlay's embedded playback kernel, responsible for decoding, timing, rendering, subtitles, danmaku, and audio output. This review judges contributions by actual playback reliability, host lifecycle, platform consistency, and observability. There is no evidence that a PR's writing style identifies its generating model, nor does it establish the value of its changes.

**Conclusion: localized changes from eight PRs became merge candidates; #152 lacks a real affected input, and #157's problem premise remains unproven.** The current product value of #152 and #159 is lower; #159 does not change the FFmpeg path used for normal external subtitles. Suitable changes were organized into separate commits, with several gaps in the original implementations corrected. The integration PR is [#164](https://github.com/AimesSoft/liberika/pull/164). See the [2026-10-13 integration verification](merge-validation.zh.md) for the latest results. Commit IDs, counts, and environments below describe the initial review and October 8 verification; the latest integration verification governs the final main-branch outcome.

## Individual assessments

| PR | Problem | Project significance | Disposition |
|---|---|---|---|
| [#150](https://github.com/AimesSoft/Erika/pull/150) | Excess HTTP Range responses, incorrect resume positions, repeated unknown-length probes | High: avoids wasted memory, misplaced cache data, and repeated requests | Retained; also fixed waiting for the server's response tail after exactly the requested bytes arrive. |
| [#151](https://github.com/AimesSoft/Erika/pull/151) | Cover art mistaken for video, decoder time-base contract, corrupt bitmap subtitles | High: correct media identification and resilience to corrupt data | All three retained; cover art in a real MP3 is no longer exposed as a video track. |
| [#152](https://github.com/AimesSoft/Erika/pull/152) | Implausible HDR mastering peak misleads mapping | Low to medium: reasonable metadata fallback, but no real faulty source | Deferred; the preliminary review branch's filter was withdrawn from this integration. |
| [#153](https://github.com/AimesSoft/Erika/pull/153) | User subtitle/danmaku timestamps cause panic | High: input data should not crash an embedded kernel | Check the representable range; the author's finite-only check still permits `1e100`. |
| [#154](https://github.com/AimesSoft/Erika/pull/154) | Audio thread failure, playback-rate bridge omits device queue, CoreAudio marks partial samples silent | High, but requires platform-specific treatment | WASAPI completed; queue accounting and recovery budget corrected; CoreAudio handed off for Apple validation. |
| [#155](https://github.com/AimesSoft/Erika/pull/155) | GPU recreation, import diagnostics, HDR capture, Linux waiting, OHOS dropped-frame diagnostics | Windows part high; remaining changes cannot be endorsed as a bundle | Retained D3D11 and OHOS trace; Linux wait is not truly bounded; Metal capture needs separate repair and verification. |
| [#156](https://github.com/AimesSoft/Erika/pull/156) | Paused seek, unexecuted pause intent, lost resume command | High: directly affects user control | Three reproducible state fixes retained; the original FIFO-saturation compensation still loses messages and was not adopted. |
| [#157](https://github.com/AimesSoft/Erika/pull/157) | iOS AudioQueue disposal/callback deallocation race | Unproven: the premise conflicts with Apple's synchronous-disposal contract | Not adopted; Apple needs an actual reproduction, and the counter itself misses the entry window. |
| [#158](https://github.com/AimesSoft/Erika/pull/158) | Old OHOS ID points at a new player, late Flutter events, GIF frame limit | OHOS ID fix high; insufficient basis for the other two | Retained monotonic IDs in both bridges; existing Dart lifecycle is safe and regression coverage added; no arbitrary GIF limit. |
| [#159](https://github.com/AimesSoft/Erika/pull/159) | Built-in parser merges SRT/VTT cues separated by spaces or tabs | Low: fixes public Rust APIs; normal playback uses FFmpeg | Small fix retained; it is not claimed to fix external subtitles in the player. |

### #150: HTTP read contract

At baseline an 8-byte request cached a 64-byte excess response. The original PR's cache limit is valuable, but if the server sends the requested bytes and does not end the response, it still waits for the tail. The reviewed version stops immediately when the range is satisfied. A 206 response for a nonzero resume offset must validate Content-Range; unknown-length probe results may be cached, but transient errors must not be cached. All 71 source tests passed, including a real localhost HTTP server and baseline/original-PR comparisons. Full Windows C ABI playback separately used a real H.264 file and Range server.

Commit: `3123f52`. Evidence: `pr150-baseline.log`, `pr150-original.log`, `pr150-reviewed.log`.

### #151: Media identification and decoder contract

At baseline an MP3 with cover art exposed an mjpeg video track; the reviewed version exposes only the real audio track and preserves its 1-second duration. Explicit pkt_timebase is a contract fix. Corrupt bitmap rectangles are skipped individually, retaining valid subtitles in the same packet. All 45 FFmpeg tests passed; the actual Windows DLL and Linux shared library both opened the file and checked tracks. The Linux native demo separately consumed 144480 audio frames from a 5-second MP3 with cover art. Its exit check requires a video frame, so the audio-only input exits with code 1; the entire demo command must not be reported as passing.

An independent Python C ABI host initially measured zero consumption in a 1.5-second observation window. Extending it to 4 seconds produced 48000 consumed frames, an empty queue, and zero audio errors, completing actual output of the 1-second MP3. A short observation window is not proof of a playback failure.

Commit: `4d01f1b`. Evidence: `pr151-baseline-probe.log`, `pr151-reviewed-probe.log`, `final-linux-cover-audio.log`.

### #152: HDR sanity policy

Maximum mastering luminance below 1 nit is unsuitable as an HDR peak; minimum luminance below 1 nit is normal and must be retained. All 35 frame/HDR tests passed, but none substitutes for comparing a genuinely abnormal mastering file on an HDR display. The original PR supplied no bad source, and its claim that 0.0001 nit is a known sentinel has no verifiable source. On October 13 an attempt to generate a genuinely abnormal mastering file failed because FFmpeg did not expose the required luminance metadata, leaving the affected path unproven. This integration therefore withdrew the change with `0004667`; historical test logs remain but are not evidence for merging.

Commit: `bf23517`. Evidence: `pr152-reviewed.log`.

### #153: Check the representable Duration range

Finite does not imply representable: the original PR still panics constructing a Duration from finite `1e100`. The review uses `Duration::try_from_secs_f64`, preserves clamping negative danmaku times to zero, and rejects NaN, infinity, and out-of-range numbers; subtitle hour/minute/second accumulation uses checked arithmetic. All 93 danmaku and 54 subtitle tests passed. An actual Windows C ABI load of malformed danmaku did not crash; malformed external SRT returned an error, but FFmpeg rejected that path, so it is not runtime evidence for the built-in parser fix.

Commit: `b6c36d2`. Evidence: `pr153-original.log`, `pr153-reviewed-danmaku.log`, `pr153-reviewed-subtitle.log`.

### #154: WASAPI queue and recovery

The original PR recorded padding before writing and still reported 1200 frames when the actual queue held 4800. The review reports padding plus the current write after ReleaseBuffer succeeds. The recovery budget must not reset after each successful reopen: if reopen succeeds but every render fails, the original scheme loops forever. Recovery is now marked complete only after successful render. A native Windows COM harness called production GetCurrentPadding/GetBuffer/ReleaseBuffer; all 4 checks passed. It exercises the real ABI but uses a controlled endpoint, not a real sound-card failure.

At initial review, the local default audio endpoint query returned `0x80070490`. During Remote Audio availability on October 13, real default-endpoint submission, cover-art MP3 consumption, and WASAPI loopback recording were completed; a subsequent endpoint disconnection caused revalidation to fail. Real-device failure and reconnection remain unverified. Exact results and recording hashes are in the latest integration verification. See the [Apple handoff](apple-handoff.zh.md) for CoreAudio.

Commit: `84d62de`. Evidence: `pr154-original-com.log`, `pr154-original-native.log`, `pr154-reviewed.log`.

### #155: Separate the platforms

After D3D11 detects device removal, it clears all device-bound state and lets the existing path create a new device; import-failure counts are also useful. On an actual WARP device, invalidation, new-device creation, Flutter target recreation, and pixel readback changed resource identity while preserving the white pixel. All 20 D3D11 tests passed. Real Windows D3D11VA playback separately accumulated 289 direct/zero-copy frames with zero import and render errors. A real TDR was not induced, so recovery from a real GPU reset remains unverified.

The Linux 100 ms poll does not bound barriers, infinite fence waits, or DeviceWaitIdle inside the underlying release, and cannot meet its promise. Nor does that alone prove UAF: a barrier on the same queue waits before release. Metal's BGRA8 capture target may not match the RGBA16Float pipeline cache in EDR mode; it needs a dedicated capture pipeline and alpha verification. The OHOS textureless diagnostic is a small observability change worth retaining.

Commit: `4adc1b4`. Evidence: `pr155-reviewed-windows.log`, `final-windows-live-capi.log`.

### #156: Real state fixes and ineffective compensation

At baseline, paused seek past the final frame changed state to Ended, and a worker's running clock overwrote pause intent. The review preserves Paused, abandons a preview that cannot finish while decode-suspended, and gives an unexecuted pause sequence a frozen clock. Actual Windows local and HTTP H.264 playback exercised pause, seek to 10-second EOF, seek back to 3.5 seconds, resume, and playback-rate changes; public state remained Paused after EOF seek and decodedVideoFrames did not increase while paused.

The original PR's FIFO send-timeout compensation uses try_send on the same full queue; both attempts can still be dropped. Two deterministic queue-saturation checks failed, so that part was not retained and transition-resume saturation is not claimed fixed. All 96 playback and 119 core filtered tests passed, as did the final full relevant Windows and Linux suites. Preview tests use an actual elapsed deadline and yield to avoid starving asynchronous demux by tight polling.

Commits: `e7cf59e`, `004b118`. Evidence: `pr156-baseline-preview.log`, `pr156-baseline-clock.log`, `pr156-original-saturation.log`.

### #157: Unproven premise and incomplete solution

All current call sites use `AudioQueueDispose(queue, true)`. The [Apple contract](https://developer.apple.com/documentation/audiotoolbox/audioqueuedispose(_:_:)) says true performs synchronous disposal and no callbacks execute after return. The author provided no reproduction violating that contract. Even if such an implementation defect exists, the callback dereferences user_data before incrementing the counter, leaving the entry window unprotected. Bounded waiting and leaking state cannot be merged on that basis. See the [Apple handoff](apple-handoff.zh.md) for validation requirements.

No code commit. The author's CI did compile iOS/tvOS; it would be wrong to say there was no platform check, but compilation does not prove the race.

### #158: Substantive OHOS ID fix, insufficient basis for the other two

Both NAPI bridges used presenter addresses as IDs; allocator address reuse could cause an old host call to hit a new player. The fix uses process-local monotonic IDs, consistent with other hosts. A host harness forced the same presenter address 0x10 on every creation and ran NativeCreate, FindPlayer, and NativeDestroy from both production files. Both baselines failed; after the fix, old lookup/destroy does not affect the new object. All 5 packaging checks actually executed C++, with no skips.

SDK 5.1.0.107 / API 18 was installed. The branch's Rust C API runtime built for ARM64 and x86_64, and both complete production bridges linked to that runtime. Earlier logs linking the v0.2.1 release are retained only as historical evidence. DevEco Studio 26.0.0.851 and the official HarmonyOS 5.1.0.110 phone image were installed; the local x86_64/API 18 emulator connected via hdc, and a real HAP was installed and run.

Both production bridges exercised H.264＋AAC playback, pause, paused seek to EOF, seek back to 3.5 seconds, resume at 1.25 speed, Surface detach/attach, cover-art MP3, destroy/recreate, and old-ID calls. After 64 real create/destroy cycles each, stop on old ID 2 returned unknown player, old destroy did not affect new ID 67, and both new players continued video and audio output. Render/import/audio errors were zero and screenshots were checked. This is an ArkTS HAP calling the full production Flutter NAPI bridge, not proof of a complete Flutter engine/NipaPlay application run.

Both bridges were also reverted to baseline while linked to the same current Rust runtime, with 128 real create/destroy attempts each; the specified old address was not reused. These results are not a real baseline-failure reproduction. Deterministic evidence for address reuse remains the production C++ harness with forced reuse. See the [HarmonyOS runtime record](ohos-runtime.zh.md) for detailed counts and reproduction.

Existing Flutter Dart code already correctly handles events while dispose waits, stream closure, 100 late events, and unknown IDs; all 113 tests passed and analysis found no issues. There is no need to swallow StateError indiscriminately. A hard cap of 36000 GIF frames is an arbitrary product limit, not a substitute for cancellation, progress, or an explicit budget.

Commit: `627b846`. Evidence: `pr158-baseline-ohos.log`, `pr158-reviewed-ohos.log`, `ohos-arkts-review.jsonl`, `ohos-flutter-review.jsonl`, `ohos-live-summary.json`.

### #159: Public parser improvement, limited player benefit

Splitting SRT/VTT on whitespace-only lines fixes the public `parse_srt`/`parse_webvtt` APIs, covering spaces, Tab, LF/CRLF, and multiline cues. Baseline merged two cues; all 56 subtitle tests passed after the fix. However, `ExternalSubtitleSession::open` uses Demuxer＋FFmpeg SubtitleDecoder; the built-in text parser is not on that path. Actual Linux C ABI＋wgpu＋libass displayed “Second” at 3.5 seconds from whitespace-separated SRT, and the screenshot was saved and checked; that runtime path is unaffected by #159's parser change.

A seek preview while remaining paused did not display the cue; it appeared after briefly resuming and pausing again. This requires separate investigation; parser tests or pixel differences must not be treated as a general pass for paused subtitle preview. The screenshot comparison also includes video updates caused by track switching, so the changed-pixel count is not the subtitle area.

Commit: `28e949a`. Evidence: `pr159-baseline.log`, `pr159-reviewed.log`, `final-linux-subtitle-pixels.log`.

## Actual runs and coverage boundaries at initial review and October 8

| Environment | Actual execution | Result and limitation |
|---|---|---|
| Windows real D3D11VA | Hidden HWND＋actual DLL, real BBB local and localhost HTTP Range, pause/EOF seek/resume/playback-rate change | 289 direct/zero-copy, 654 rendered, 0 import/render errors; clip has no audio track. |
| Windows WARP | Device invalidation, new device and Flutter targets, pixel readback | 20 passed; no real TDR induced. |
| Windows WASAPI | Actual COM ABI, controlled endpoint | 4 passed; no local default endpoint at initial review, real sound card unverified. |
| Linux X11＋Mesa＋PulseAudio | Separate native window, 5 seconds of audio/video; separate C host pause, display reconnect, seek, resize, resume, destroy | 262 video/144000 audio; C host 328 video/103200 audio; errors 0/0. |
| Linux real H.264 | Actual BBB file, FFmpeg decode＋wgpu presentation | 295 rendered, 0 errors; software llvmpipe, not VAAPI/NVDEC hardware evidence. |
| Linux subtitles | Actual shared library＋Xlib＋libass, external SRT, seek, pause after resume, RGBA capture | “Second” visually confirmed; paused-only preview did not pass. |
| Linux MP3 with cover art | Independent Python C ABI host, real MP3 decoding and PulseAudio output | 48000 audio frames, empty queue, 0 audio errors; null sink does not verify audible output. |
| Linux PulseAudio | Production native pause/resume/flush/reconfigure tests＋null sink | Passed; null sink does not prove speaker output. |
| OHOS official phone emulator | Current Rust runtime＋two production bridges, real HAP/XComponent, H.264＋AAC, pause/EOF seek/playback rate/Surface reconnect/cover MP3/destroy-recreate | ArkTS 518 rendered, Flutter 517 rendered; each consumed 235190 MP3 audio frames; old IDs rejected, zero errors. System decoder used CPU fallback for presentation; this is not evidence of zero-copy on a real device. |
| Apple | Not run on this machine | To be handled separately by the user; cannot be marked complete. |

The BBB file was 969201 bytes, SHA256:

`18b99ec25f32f6bd2223aa54e4b5632533328bf5cc81c283eba7604c42649f75`.

Download: <https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/720/Big_Buck_Bunny_720_10s_1MB.mp4>. The original video is Blender Foundation / Big Buck Bunny; media metadata states CC BY 3.0.

## Initial review regression summary

Windows: erika unit tests 702 passed/1 ignored, integration 4＋6 passed, C API 42 passed. Linux: erika unit tests 677 passed/2 ignored, integration 4＋6 passed, C API 41 passed. Flutter 113 passed, flutter analyze clean; packaging 5 passed; Rust fmt passed. The HarmonyOS emulator dependency build added x86_64 target support; xtask 26 passed and actual ARM64/x86_64 builds passed. Windows clippy completed with existing warnings and was not run with `-D warnings`.

The entire `--workspace` failed in an Apple-only demo's ObjC build script; subsequent verification was limited to core and C API. Linux synthetic Vulkan direct-frame checks failed because llvmpipe lacks synchronization2/external-semaphore capability; this is not a pass for that path. The #155 Linux code was not adopted.

All 10 original author PR fork CI runs passed and their run SHAs exactly matched the reviewed heads; see [author-ci.json](author-ci.json). CI success does not replace the functional and runtime judgments above.

## Reproduction materials

[evidence](evidence/) contains targeted baseline, original, reviewed, and actual-run logs. [scripts](scripts/) contains standalone host and SDK build commands. They depend on the tools, media, and paths described; mock/compile checks are not platform runtime tests.

The complete original local evidence, media, build products, and SDK were at `C:\\Users\\rgliang\\AppData\\Local\\Temp\\erika-pr-review-20261005`. The original workspace was not modified; all code was in an isolated worktree.
