# PR #129: fixes and validation

Translation of the [Chinese record](pr129-native-hdr-compatibility.zh.md).

## Scope

- Original PR: AimesSoft/liberika#129, head `2c5e470fd33b4a7297fa5b15bb22cb65ace1899e`.
- Comparison main: `7b0fa9179d0cd3b85e28974844f213d0e00aa058`.
- Local fix branch: `codex/pr129-preserve-native-hdr`.
- Normal playback continues using main's native Windows HWND/swapchain and HDR10 negotiation; Flutter texture does not replace the primary path.

## Fixes

1. Restore Windows `ErikaVideoView` to `ErikaWindowOverlayVideoView`, passing through blendMode, opacity, debugLabel and callbacks. Default srcOver and auto/extendedLinear do not implicitly create a texture.
2. Only `ErikaTextureVideoView` explicitly chooses a Windows SDR BGRA8 texture. srcOver supports Flutter opacity, clipping and filters; overlay switches to native Composition; other Windows blend modes explicitly error. Remove the canvas saveLayer wrapper that cannot work across a TextureLayer.
3. Retain the PR's two-input Composition effect graph and FittedBox transform-rectangle fix.
4. Load new texture APIs optionally and check capability before detaching the existing surface. An old DLL missing new symbols no longer prevents normal player creation. Both C header copies declare the new API.
5. Publish GPU output as shared snapshots that have completed copying and are never written again. The plugin retains only latest and the sampled descriptor obtained by the raster callback; skipped resize snapshots can be reclaimed. Neither release_callback nor a fixed frame count proves Flutter has finished GPU sampling, so forced snapshot deletion after a timeout was not adopted.
6. Publish a new snapshot and notify Flutter only when picture content or dimensions change. Content includes video frames, seek generation, subtitles/HUD, danmaku and upscaler mode; clock advancement alone is not a new picture.
7. Fix texture owner not being cleared after player destruction and old object references not being detached before replacing the overlay HWND.
8. Revert global D3D11VA surface-pool expansion and the unbounded retired-frame queue. `ffmpeg.rs` matches the above main exactly. Additional GPU handoff waits apply only to Composition/texture, not ordinary opaque HWND playback.

For safe cross-device handoff, explicit texture adds GPU snapshot allocation, copy and a bounded completion wait on each picture change; there is no CPU pixel readback. These costs are not on the default native playback path.

## Existing issues found during verification

- The C API upscaler status test special-cased only Android. Windows D3D11 already reported Building with no device attached, while the test expected Inactive. The same failure was reproduced on original main; only the Windows test expectation changed, not upscaler behavior.
- Main's asynchronous danmaku planner only sent a shutdown notification at destruction and did not wait for the thread. The plugin unloads the DLL when its last player is freed, so a thread not yet started or exited could execute unloaded code. Exception addresses resolved to Rust thread startup and allocation functions; a v0.1.7 DLL stress test also reproduced an exit exception. The fix stores and joins the danmaku worker only during Windows destruction, releasing the shared-state lock before waiting; normal planning, playback, decoding and HDR policy are unchanged. Other platforms retain the existing exit policy.

## Validation completed

- `flutter test --no-pub --reporter expanded`: 104/104.
- `flutter analyze --no-pub`: no issues.
- `cargo test -p erika -p erika_capi --lib -- --test-threads=1`: core 528/528, C API 41/41. The combined run enabled the C API dependency feature set, so the count exceeds a separate default core run.
- `cargo build -p erika_capi`: DLL built; existing unused/dead-code and LNK4098 warnings remained.
- Windows plugin fully compiled and linked with the real Flutter SDK.
- Native CTest: 3/3 (GPU publication, old runtime API compatibility, new runtime texture lifecycle).
- New runtime plugin connection and actual DLL unloading passed 50/50 consecutive runs without pinning the DLL.
- GPU tests include WARP pixel readback for snapshot immutability, reclamation of references from 100 skipped resize publications, 10,000 concurrent publications/reads and idle/resize content invalidation.
- `git diff --check` and `cargo fmt --all -- --check` passed.

The old-runtime compatibility test explicitly pins the DLL and checks loading, creation, capability checks and texture-owner cleanup only; it does not claim to fix the thread-exit defect inside a released binary. Source-runtime tests must actually unload the DLL to cover this exit-race fix.

Native tests can be configured independently:

```powershell
cmake -S packages/erika_flutter/windows/tests -B target/windows-plugin-tests -A x64 `
  -DFLUTTER_ENGINE_DIR="<Flutter SDK>/bin/cache/artifacts/engine/windows-x64" `
  -DERIKA_COMPAT_DLL="<released v0.1.7>/erika_capi.dll" `
  -DERIKA_SOURCE_DLL="<this checkout>/target/debug/erika_capi.dll"
cmake --build target/windows-plugin-tests --config Debug
ctest --test-dir target/windows-plugin-tests -C Debug --output-on-failure
```

## Release and acceptance limits

- The artifact manifest was still pinned to v0.1.7; that prebuilt library had neither the new texture API nor the thread-exit fix. New capabilities require a matching source runtime (`ERIKA_FORCE_SOURCE_BUILD=1` for a repository source build). A matching artifact and checksum must be built and updated before release; no nonexistent version was invented or substituted.
- Actual video playback on an HDR monitor, NipaPlay integration and real DWM Composition stress testing were not performed. Windowless GPU/API tests do not constitute acceptance of those scenarios.
- Suggested pre-merge checks: normal SDR/HDR10 playback with the default primary component and output status, pause/seek/subtitles/danmaku/upscaling, window resize and movement between displays; transparent scan-line overlay; explicit texture pause notifications, clipping/opacity, rapid resize and player switching/closure.
- Changes were kept in an independent worktree without modifying existing changes in the user's main workspace. Pushing to the original PR branch required an explicit user request; it was not merged to main.

## 2026-09-07: substantive fixes for Copilot feedback

- Address only confirmed overlay ownership errors and repeated CPU buffer copies. No unconfirmed `isTopmost=FALSE` fallback or diagnostic wording changes were added.
- HWND recreation restores only the original owner's binding, without relying on the last element of an unordered container. Explicit owner switches first detach the old producer; a late detach/hide from an old player must not hide the current picture or switch the shared HWND. With no owner or a detached owner, do not choose another player implicitly.
- Showing the same bound player again after hiding it does not recreate the swapchain merely because the owner marker was temporarily cleared. The default native HWND/HDR path, decode pool and output-format negotiation are unchanged.
- Texture-scene caches update for actual subtitle/HUD and danmaku content changes separately; a new video frame no longer causes unchanged CPU pixel buffers and instance lists to be deep-copied again. Exact content comparison remains; no hash that might miss updates or video-frame-number-only test was introduced. The danmaku atlas continues to use existing Arc sharing.
- New tests verify 120 consecutive video frames reuse the same cached allocation while subtitle pixels, alpha, danmaku positions and clearing still invalidate correctly. Windows native tests use a hidden HWND and real plugin state logic for owner retention, exclusive handoff, late detach, no owner and attach failure; only the surface attach/detach C entry points are replaced, and actual DWM image effects are not tested.
- Full regression in this round: Flutter 104/104, analyze clean; combined Rust core 529/529 and C API 41/41; plugin build and native CTest 3/3.
