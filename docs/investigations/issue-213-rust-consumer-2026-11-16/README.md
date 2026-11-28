# Issue #213: external Rust consumer and Windows host validation

Validation date: 2026-11-16. Source base: `f5deb2a3702c64bcce8be960430f9276fdc8460b`, plus this task's uncommitted fixes copied to isolated workstation directories. These results describe the tested snapshots, not the unchanged v0.3.7 release tag. Windows source-file hashes are recorded in its manifest.

## Resolution

[Issue #213](https://github.com/AimesSoft/liberika/issues/213) exposed Cargo's workspace-local patch behavior: the repository's `[patch.crates-io]` does not propagate to downstream consumers. Linux `wgpu` consumers must explicitly patch `wgpu-hal` in their own workspace root to the matching liberika version and supply the Linux PIC flags for `soundtouch-ffi`. The [Linux guide](../../linux.md) contains the v0.3.7 Git dependency example, lockfile recovery command, and PIC configuration. This retains the patched zero-copy import path; it does **not** establish support for consumers without the patch.

The Windows follow-up exposed a separate default-target issue. `xtask deps` now resolves `host` to the current x64/ARM64 MSVC target before selecting dependency paths and build settings. Released v0.3.7 still needs the explicit Windows `--target` shown in the [build guide](../../building.md).

## Linux result

The [independent consumer script](../../../scripts/check_linux_rust_consumer.sh) creates its own temporary workspace outside the repository with a local HAL patch and its own PIC settings. It verifies the resolved HAL manifest, then runs `cargo build --locked`, retaining a reference to `WgpuRenderer::new` so native linking is exercised. The script is included in [Linux checks](../../../scripts/check_linux.sh).

The real x86_64 Linux run passed in **52.79 seconds**. It resolved the patched `wgpu-hal 29.0.3`, compiled erika and the external consumer, and linked successfully; seven erika compiler warnings remained. The run reused an existing Docker image and native libraries, rebuilding only the repository-patched libass as static PIC. Rust was 1.93.1; FFmpeg's libavcodec was 62.11.100, libass 0.17.5, PulseAudio 17.0, and Vulkan 1.4.341. Exact image identity, environment, commands and hashes are in the manifest; selected original output is in the log excerpt.

Coverage is dependency resolution, compilation and linking on x86_64 Linux. This run did not test GPU rendering, playback, ARM64, or the Ubuntu 22.04/glibc 2.35 release ABI baseline. The Docker run was launched through WSL Ubuntu 24.04; that host distribution is not evidence for the container's release baseline.

## Windows result

On x86_64 Windows MSVC with Rust 1.98.0, the same task's isolated source snapshot passed:

| Check | Result | Evidence |
|---|---|---|
| xtask unit tests, including default/explicit host equivalence and Windows dependency paths | 35 passed | xtask.log |
| `cargo run --locked -p xtask -- deps plan --all --profile lgpl`, without `--target` | Resolved `x86_64-pc-windows-msvc`, exit 0 | deps-plan.log |
| C API library tests with `wgpu` | 57 passed | capi.log |
| C API DLL build | Passed, with linker warning | capi-dll-build.log |
| Flutter Windows plugin compile check with `/W4 /WX` | Passed | windows-native-build.log |
| Native presenter worker and texture publication CTest | 2 passed | windows-native-ctest.log |
| Source/texture lifecycle CTest against the built C API DLL | 1 passed | windows-lifecycle-ctest.log |

The Windows manifest preserves the tested file hashes and also records the task's #197/#205 regression counts; their detailed records are separate. This is test/build coverage, not a full Flutter application playback or Windows ARM64 acceptance run. Retained logs normalize line endings and trailing whitespace; full originals remain under `target/issue-validation`. PowerShell represents some native stderr output as `NativeCommandError` in these original logs; the final test/build results and explicit exit markers determine success.

The full local logs remain in `target/issue-validation/linux/` and `target/issue-validation/windows/`. This directory keeps only the Linux excerpt and small relevant Windows logs; evidence-sha256.json identifies the retained files. No additional build was run while preparing this record.
