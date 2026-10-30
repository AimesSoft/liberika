# Doctor-X S01E01: intermittent macOS audio distortion

Translation of the [Chinese investigation](doctorx-audio-2026-10-26.zh.md).

Date: 2026-10-26. Baseline: `f83ccdb7bf9e759851d42e11af2e956dd5194c6b`, branch `codex/doctorx-audio`. The local NipaPlay was 1.11.10 with Flutter dependency `erika_flutter 0.3.0`.

On this Mac, the user heard pronounced distortion around 46 and 48 minutes while playing Doctor-X season 1 episode 1; it played normally after switching to MDK. The user also confirmed that when rewinding and replaying, the location changed and sometimes the distortion did not occur.

A CoreAudio output defect was confirmed: any partial callback underflow marked the whole output block `OUTPUT_IS_SILENCE`. The valid PCM prefix had already been removed from the queue and counted in the audio clock, but CoreAudio could skip it as silence. This defect can widen the audible interruption caused by a brief underflow. The fix sets the silence flag only when no PCM was read and clears it when PCM is available again.

No NipaPlay underflow record was captured at the moment the user heard the distortion, so direct runtime evidence linking this confirmed defect to the symptom is still missing. The trigger for buffer exhaustion also remains to be identified.

## Media and decode checks

The sample was `Doctor.X.Surgeon.Michiko.Daimon.S01E01.2012.1080p.Netflix.WEB-DL.AVC.AAC.2.0-DBTV.mkv` in a locally mounted media directory. Duration: 3508.822 seconds; video: H.264; audio: HE-AAC, 48 kHz, stereo. The default output was the MacBook Pro's built-in speakers at 48 kHz. This episode does not undergo surround downmixing.

| Check | Result |
|---|---|
| Standard FFmpeg decode 40:00–50:00 | No decode errors; peak 0.936851 (about −0.567 dBFS); no NaN/Inf |
| Direct extraction of AAC compressed packets around 45:00–49:00 | Matroska seek includes preroll; actual sample duration 245.546 seconds |
| Erika `PlaybackSession` export of the sample as f32 PCM | 11,786,240 stereo frames, 23,572,480 samples |
| Comparison with standard FFmpeg f32 PCM | Same length and exactly equal sample by sample; maximum and RMS errors both 0 |
| Erika PCM peak and timing | Peak 0.644198120; clipping, non-finite values and PTS discontinuities over 2 ms all 0 |

Both PCM files had SHA-256:

```text
8d1801105989a09d2ba905b0171da28b4ba80596032863fcfd22dd7f8a819e8e
```

The comparison covers the section the user identified and supports further investigation of real-time supply and output. MDK's decoded output was not compared sample by sample.

## Regression and live checks

The added CoreAudio regression writes two PCM frames to the ring and requests four, checking the valid prefix, zero-filled tail, silence flag and frames-read statistics. It then checks a completely empty buffer and the volume ramp after PCM is replenished.

The test failed under the old condition `underflow_frames > 0`, indicating that the valid prefix was incorrectly marked silent. After changing it to `frames == 0`, all 77 tests in `cargo test -p erika audio -- --nocapture` passed.

A separate diagnostic program exercised Presenter, VideoToolbox, Metal textures and actual CoreAudio output at volume 0 to avoid interrupting the user's viewing. Before the fix, it ran for 75 seconds starting at 47:30; audio frames read continued to advance, and underflow and dropped-frame counts were both 0. This test did not reproduce the intermittent failure.

A full-load check also added the same-named danmaku XML, embedded subtitles and a 1920×1080 Metal texture. The first seek from 45:45 hit a 2-second worker barrier timeout and playback did not start; it cannot count as audio validation. A retry from 45:55 ran for 165 seconds through approximately 48:39, again with 0 underflows and dropped frames. That run used the fixed code and validated the normal playback path; because no underflow occurred, it cannot prove the fault has disappeared. The log was the local investigation artifact `output-danmaku-retry.log`.

## Retest

The fix is in `crates/erika/src/apple.rs`. This investigation did not replace the runtime in the installed NipaPlay. The next in-app retest should record audio underflows, render-tick duration and buffer levels together to determine whether distortion coincides with buffer exhaustion. If interruptions persist, distinguish render-thread supply delay from media-read delay.

The original PCM, media segment, temporary probe source, before/after test logs and live-output logs remain on the investigation machine under `out/doctorx-audio-2026-10-26/`; they are not committed as repository test fixtures. The repository regression uses synthetic PCM and can be reproduced independently with the Cargo command above.
