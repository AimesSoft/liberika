# External texture host examples

[Native output guide](../../docs/external_textures.md) ·
[中文](../../docs/external_textures.zh.md) · [日本語](../../docs/external_textures.ja.md)

These windowless consumers demonstrate the generic native texture contract.
They have no framework dependency. The producer renders video, subtitles and
enabled danmaku directly into the exported texture; native import does not
read pixels back or copy the output texture.

## Runnable cross-version wgpu host

The standalone Cargo workspace uses **wgpu 26.0.1** for the host and
**wgpu 29.0.3** for liberika. Linux requires Vulkan with
`VK_KHR_external_memory_fd`; macOS uses Metal. Both devices must select the
same GPU (and, for Vulkan, driver).

From the repository root, with liberika's normal native build dependencies
installed:

```sh
python3 examples/external_texture_host/prepare.py
CARGO_TARGET_DIR=examples/external_texture_host/target \
  cargo test --manifest-path examples/external_texture_host/Cargo.toml --lib
CARGO_TARGET_DIR=examples/external_texture_host/target \
  cargo run --manifest-path examples/external_texture_host/Cargo.toml -- \
  --require-hardware --frames 120 \
  --report examples/external_texture_host/artifacts/native.json \
  --png examples/external_texture_host/artifacts/native.png
```

Keep the host build directory separate from the main workspace build directory.
The two Cargo lockfiles can resolve different engine dependencies; sharing
`CARGO_TARGET_DIR` can overwrite the engine cdylib/rlib artifacts.

`prepare.py` verifies and unpacks the pinned official wgpu-hal 26.0.6 archive
into the repository's ignored `target/host-wgpu-hal-26`, then applies the
included patch. It does not modify the global Cargo registry. The patch adds
an opt-in initial usage for imported Vulkan images: the normal first
UNINITIALIZED/UNDEFINED transition would discard their existing contents.
This patch is specific to the example's host HAL version, and is not a new
liberika dependency or a requirement on host framework types. Adapters for
other wgpu versions must preserve the same native ownership/layout contract.

`src/import.rs` validates device identity and wraps a native texture in the
host's own wgpu version. Vulkan duplicates the borrowed FD, imports dedicated
memory and waits for its ownership acquire before returning. The allocation
retains the original frame until the host HAL retires it. Metal independently
retains the MTLTexture. The host draws the imported image with a fullscreen
`textureLoad` shader. Only validation reads pixels back.

The test compares all pixels at 320×180, 640×360 and 319×181, retains imported
frames across producer destruction, then draws them again. It covers an NV12
fixture, a bitmap subtitle and actual rasterized danmaku.

The CLI measures **renderer-only**, debug-build throughput using a static
**64×36 NV12 source scaled to 960×540** by default. It excludes decoding,
window presentation and final validation readback; it includes per-frame
allocation, GPU waits and resource retirement. These numbers do not reproduce
an application's playback FPS. `--rgba` explicitly measures capture/upload
instead of native import. `--capture-only` measures producer capture without
a consumer. For GLES use `--capture-only`: the two wgpu versions do not
coordinate EGL display teardown. GLES native export is unsupported.

See the dated [validation record](../../docs/investigations/issue-175/README.md)
for tested adapters and the distinction between hardware and software Vulkan.

## Native C Vulkan importer

`vulkan_import.c` supplies a sampled native image view, independent of wgpu
versions and Flutter.

1. Enable `VK_KHR_external_memory_fd` on the host device and use Vulkan 1.2+.
2. Export a frame with `erika_presenter_export_frame_texture`.
3. Call `erika_example_import_texture` on the host render thread. Success takes
   ownership of the frame; on failure, release the frame yourself.
4. Record `erika_example_acquire_texture` once before the first fragment-shader
   sampling submission. Sample `texture.view` as RGBA8Unorm; apply any transfer
   conversion needed by your compositor.
5. After all consuming GPU fences complete, call
   `erika_example_destroy_texture`. It destroys imported aliases and releases
   the original frame. Do not release the frame separately after successful import.

The importer matches device and driver UUIDs and uses a duplicate FD with a
dedicated allocation. The acquire barrier starts in GENERAL with external
queue ownership. The producer has completed its render/release submission
before returning the frame. Compile the adapter alongside the host, linking
Vulkan and the liberika C ABI:

```sh
cc -std=gnu11 -Wall -Wextra -Werror -fsyntax-only \
  -I crates/erika_capi/include examples/external_texture_host/vulkan_import.c
```
