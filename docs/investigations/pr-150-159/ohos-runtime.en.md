# HarmonyOS emulator runtime record

[中文](ohos-runtime.zh.md) · [日本語](ohos-runtime.ja.md)

Date: 2026-10-08. Code: reviewed result of `codex/pr-150-159-reviewed`. The full production Rust runtime and NAPI bridges were used; the player, decoder, audio output and Surface implementation were not replaced.

## Environment and artifacts

- DevEco Studio 26.0.0.851; Huawei Authenticode signature on the installer was valid.
- Official HarmonyOS 5.1.0.110(SP2DEVC00E110R4P11) image, API 18, x86_64.
- Instance ErikaReview, hdc `127.0.0.1:5555`, Linux 5.10.210 x86_64.
- Rust 1.93.1, OpenHarmony SDK 5.1.0.107; `--no-default-features --features wgpu`.
- The HAP was compiled with Studio's bundled SDK 26, compatibleSdkVersion 18, and installed successfully on the API 18 emulator.
- The current runtime's ARM64 build and both bridge links also passed; ARM64 was not run.

x86_64 runtime SHA256: `7325a0e0915d828cee42c3dc64e6aa5e6870d2a15415619d9662c3dbafac8ad3`.

ArkTS bridge: `6a8737bc254916797683935d60e9099a5f617e9158e5d145f2b588192b40e7f2`. Flutter bridge: `f1329a2f9405a8f894491a72c54aecb2f4f303e9ceced786124704dd1b090ecb`. HAP packaging strips binaries; these hashes are for the original linked artifacts before packaging and cannot be compared directly with stripped files in the HAP.

The video was the original report's 10-second BBB H.264 file with a 48 kHz / 440 Hz AAC track added; the video stream was copied. Audio/video fixture SHA256: `980be2f90110acf9c407581986453e483f5d18947569fe329c3eea64c2714bff`. The cover-art MP3 was `cover-5s.mp3` already used in the same investigation.

## Live procedure and results

Two XComponents in the HAP were passed to the production ArkTS and Flutter bridges respectively and run in sequence. Each played for 4 seconds, paused for 2 seconds, sought to the 10-second EOF and back to 3.5 seconds, then resumed at 1.25x. It detached and reattached the actual NativeWindow, played the cover-art MP3, performed 64 real create/destroy cycles and calls using an old ID, and finally played with a recreated player.

| Observation | ArkTS | Flutter bridge |
|---|---:|---:|
| Initial rendered video / consumed audio frames | 111 / 187236 | 116 / 191694 |
| Decoded after pause and EOF seek | 114 → 114 | 121 → 121 |
| Public state after EOF seek | 4 (Paused) | 4 (Paused) |
| Rendered / consumed audio frames after surface reconnection | 427 / 248688 | 431 / 248688 |
| Cover MP3 consumed / queued frames | 235190 / 0 | 235190 / 0 |
| Rendered / consumed audio frames after recreation | 91 / 151572 | 86 / 142656 |
| Total rendered video (both instances) | 518 | 517 |
| Render / import / audio failures across all snapshots | 0 / 0 / 0 | 0 / 0 / 0 |

The MP3 exposed only one audio track; decoded/rendered video were both 0 and the cover was not selected as video. `stop` on old ID 2 returned `{ok:false, status:1, error:"unknown Erika player"}`; after the old destroy, new ID 67 remained alive and actually played. This checks monotonically increasing IDs in both bridges and real host calls. One frame was already being decoded after pause (ArkTS 113 → 114); no further frames were decoded between stable pause and the EOF seek.

The emulator's system decoder was counted as `hardwareVideoFrames`, but all output used CPU fallback and `zeroCopyVideoFrames` was 0. This does not establish hardware-decode performance on real devices, NativeBuffer zero-copy, or audible physical-speaker output. libass was not built; subtitles, HDR and real device loss were outside this OHOS validation. The Flutter part called the production NAPI bridge without starting a full Flutter engine; Dart-side evidence remains the tests and analysis in the original report.

![Live video through both bridges](evidence/ohos-live-finished.png)

The screenshot was obtained and verified via hdc. The repository copy is reduced to 660 pixels wide; the original 1320×2232 image is in the local evidence directory.

## Limits of the baseline comparison

Both bridge C++ files were replaced with baseline `4390c78` while linking the same current runtime. Each attempted 128 real create/destroy cycles, but did not reuse the specified old object address. This is recorded as not reproduced, not a passed failure/fix comparison. The production C++ forced-address-reuse harness reproduced the old lookup/destroy mistakenly affecting a new object in the original investigation; that deterministic regression and this live run complement each other.

The first validation program called `open()` again after `close()` and received `player is closed`. `close()` is terminal; the procedure above completed after changing the host to actually destroy and recreate the player. Production code was not changed for this harness error.

## Reproduction materials

`evidence/ohos-*-review.jsonl` contains raw records written to the app's filesDir and exported via hdc. `ohos-live-summary.json` contains full counts; `scripts/summarize-ohos-live.py` validates success markers, playback consumption, paused state, rejection of old IDs and error counts.

`scripts/ohos-live/` contains the ArkTS host, baseline comparison host, native type declarations and setup script. `scripts/ohos-*-build.sh` and the corresponding CMake directories retain the actual runtime/bridge build commands. Scripts use Docker mounts `/work` (isolated worktree), `/review-logs` (local evidence directory), and `/target` (persistent artifacts). Install nasm and use the OHOS std from the official Rust toolchain before building.

On Windows, the official devecocli created and started the instance and built the HAP; SDK hdc install and aa start were then used. The complete temporary project with fixture and `.so` files is at `C:\Users\rgliang\AppData\Local\Temp\erika-pr-review-20261005\ohos-live-app`. SDKs, system images, runtime binaries and account material are not committed to the repository.
