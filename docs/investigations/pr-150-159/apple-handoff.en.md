# Apple handoff: #154, #155, #157

Translation of the [Chinese record](apple-handoff.zh.md). Baseline `4390c78f4467e2047e19690700fbead6a18d8e31`; PR head SHAs are in `author-ci.json`. The investigation machine had no Apple runtime environment; Windows/Linux results cannot substitute for these three checks.

## #154: CoreAudio silence flag on partial reads

The goal is sound: when the ring returns some real samples and zero-fills the remainder, the whole buffer must not be marked `OUTPUT_IS_SILENCE`. Set the flag only when `read_frames` is 0. This part was not included in the review branch.

Verification:

1. Fill the ring with fewer nonzero PCM frames than one callback requests and trigger a real CoreAudio callback.
2. Check that the real prefix remains, the tail is zero and `OUTPUT_IS_SILENCE` is not set.
3. With an empty ring, check all-zero output and the silence flag.
4. Play audio/video, covering pause/resume, playback-rate changes and underrun; check the sound and audio clock.

## #155: Metal SDR capture and packed alpha

The goal is sound, but the existing patch cannot be adopted directly. A BGRA8Unorm capture target needs a matching pipeline. In EDR mode the live drawable may be RGBA16Float, so the cached live pipeline cannot be used directly with a different target format. Use a separate capture pipeline/cache and explicitly select SDR mapping and alpha uniforms.

Verification:

1. Play HDR video on a real Metal layer with EDR enabled and capture an SDR PNG; verify Metal validation reports no format mismatch.
2. Compare midtones and highlights against an explicit SDR tone-mapped reference; merely producing a PNG or checking byte count is insufficient.
3. Use side-by-side/top-bottom packed-alpha video and inspect screenshot colors, transparent areas and premultiplied alpha.
4. On one player alternate EDR playback, SDR capture, resize and another capture; verify capture has not contaminated the live output mode or pipelines.

## #157: Reproduce first, then design the release protocol

Production call sites all use `AudioQueueDispose(queue, true)`. Apple documentation specifies synchronous disposal for true, with no callbacks after it returns:

<https://developer.apple.com/documentation/audiotoolbox/audioqueuedispose(_:_:)>.

The author's premise that an existing callback still accesses state after dispose returns requires runtime evidence. The proposed counter also cannot protect the window after a callback reads `user_data` and before it increments the counter; waiting 100 ms and potentially leaking is not a complete fix. This PR was not adopted.

Verification:

1. On iOS/tvOS, log thread and monotonic time at actual AudioQueue callback entry, after all state access, on dispose entry/return and at Box drop.
2. Repeatedly stop/configure/drop at high frequency with synchronous disposal and ASan or equivalent memory diagnostics; retain the shortest timeline when the issue occurs.
3. Distinguish a genuine post-dispose callback from the host freeing `user_data` prematurely on another path.
4. If the documented contract does not cover actual system behavior, design a stable owner/registry covering callback entry or an explicit cancellation protocol.

The author's fork CI passed for the exact heads, including iOS/tvOS builds. This demonstrates compilation coverage, not that the race was reproduced or fixed.
