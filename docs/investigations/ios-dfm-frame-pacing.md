# iOS DFM+ presentation timing

> [中文](ios-dfm-frame-pacing.zh.md) · [English](ios-dfm-frame-pacing.md) · [日本語](ios-dfm-frame-pacing.ja.md)

liberika already rendered on a serial worker driven by CADisplayLink. Its DFM+
positions nevertheless sampled the shared playback clock after audio/video
pumping, using the worker's current time. Variable queue/pump latency therefore
changed scroll displacement between display frames. While a render was queued,
all later callbacks were discarded, including the newest display target.

The iOS driver now passes CADisplayLink.targetTimestamp through a bounded,
latest-request mailbox. At most one worker and one pending request exist; a
busy worker consumes the newest target instead of replaying old frames. The
worker holds the player for only one call at a time so sustained load cannot
prevent disposal. Driver changes cancel pending requests, preserving the
existing background audio-only path.

The new optional C entry point `erika_presenter_render_tick_with_timing` accepts
a nullable signed delay from the call's monotonic time to the display target.
Rust converts it to an Instant before taking the presenter lock or pumping
media. Danmaku, subtitles and render context then sample one PlaybackSnapshot
at that instant. The playback clock itself, audio-master discipline, seek
generations, rate, offsets and layout/collision policy are unchanged.

The legacy entry point passes a null target and keeps its original sampling
behavior. The Swift plugin looks up the new entry point optionally, so older
prebuilt libraries still work with legacy timing. Using this optimization fully
requires a source-built library or a future prebuilt containing the symbol.
No new required linker symbol or change to public struct sizes is introduced.

## Validation, 2026-09-29

Only the iPhone 17 Pro / iOS 26.5 simulator was used for native execution.
Rust was built with the release profile; the Flutter smoke shell was debug.
No Android emulator was used.

- iOS app compiled and ran with the locally built static library. `nm` confirmed
  the timing symbol survived static linking; the actual driver logged
  `mainThread=false displayTarget=true`.
- Eleven Rust tests executed on iOS: the new presenter target-time test,
  eight playback-clock tests, and the existing layout-generation and stale
  plan invalidation tests. The new test covers 60/120 Hz target sampling,
  repeated targets, pause, long/short seeks and 0.5/1/2x rates.
- The Swift mailbox executable passed on iOS: coalescing, callbacks while busy,
  stale/duplicate/nonfinite rejection and driver changes.
- Three iOS Dart scheduler/interruption contracts passed; smoke harness static
  analysis and header/API alignment checks passed (one optional C++ check was
  skipped because CXX was not configured).
- Twelve real playback cases passed through the Flutter plugin and native
  presenter trace. Paused seeks to 20, 8, 8.1 and 8 seconds updated both media
  time and generation. Pause stayed fixed; offsets, hidden seeks, empty scenes
  and consecutive seeks remained functional. Over about 1.51 seconds, media
  time advanced 0.759 / 1.492 / 3.005 seconds at 0.5 / 1 / 2x.
- Captured native output at 8 -> 8.1 seconds moved approximately 38 physical
  pixels to the left. Returning to 8 seconds produced a pixel-identical image.
  This verifies the short seek reached the rendered output, beyond trace data.
- Render, import and audio failure counters were zero in the regression run.

Machine-readable functional evidence is in `ios-dfm-functional.json`.

This fixes an identified timing source; it does not establish smoothness parity
with Titan or a measured percentage CPU/GPU reduction. The trace run is not a
performance benchmark. A subsequent attempt at background/overhead measurement
was interrupted by host locking and is excluded. True device refresh cadence,
battery use and background audio recovery remain unmeasured here.

At the same refresh rate this adds constant-size timestamp/mailbox work and no
extra rendering pass. Rendering an available latest frame instead of dropping
it can increase utilization under saturation. Pause still follows the existing
display-refresh behavior; this patch does not optimize idle/pause power.

## Reproduce

Build the iOS simulator native dependencies and release static library using the
repository's usual source-build instructions. Create a temporary Flutter iOS
app, depend on `packages/erika_flutter` by path, and copy
`packages/erika_flutter/tool/ios_danmaku_smoke.dart` to its `lib/main.dart`.
Declare `assets/test.mp4` in the temporary app's pubspec. A local test fixture:

```sh
ffmpeg -f lavfi -i color=black:s=1280x720:r=24 \
  -f lavfi -i anullsrc=r=48000:cl=stereo -t 90 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac assets/test.mp4
```

On Apple Silicon, exclude `i386 x86_64` for the temporary app's simulator build
and Pod targets when supplying an arm64-only static library. Set
`ERIKA_IOS_CAPI_STATICLIB` to the newly built `liberika_capi.a` when building the
app; otherwise the plugin may select an older released prebuilt.

For functional inspection, launch with `ERIKA_DANMAKU_TRACE=1` and
`ERIKA_DANMAKU_TRACE_FILE` set to a writable path, using `SIMCTL_CHILD_` prefixes
with `simctl launch`. The harness starts paused at 8 seconds. After connecting
the Flutter debug VM:

```sh
python3 packages/erika_flutter/tool/verify_ios_danmaku.py <VM-Service-URL> \
  --trace <native-trace-file> --output /tmp/erika-functional.json
```

The trace records presenter time and glyph count, not physical display latency.
Inspect screenshots separately. Disable trace logging, use a fresh process and
keep the host unlocked for any future CPU/GPU measurement. Do not compare this
release native renderer plus video/audio workload directly with NipaPlay's
standalone debug overlay or with unmatched Titan density.

To run the mailbox test directly on a booted iOS simulator:

```sh
xcrun --sdk iphonesimulator swiftc -target arm64-apple-ios13.0-simulator \
  packages/erika_flutter/ios/Classes/ErikaTickMailbox.swift \
  packages/erika_flutter/test/ios_tick_mailbox/main.swift \
  -o /tmp/erika-ios-tick-mailbox-test
xcrun simctl spawn <UDID> /tmp/erika-ios-tick-mailbox-test
```
