# Native texture output

> [中文](external_textures.zh.md) · [日本語](external_textures.ja.md)

`PresenterRuntime::export_frame_texture(w, h, scale)` renders video, subtitles and
enabled danmaku directly into a native GPU texture. It needs no window surface
or Flutter registrar and exports no wgpu Rust types. The host can use a
different wgpu version and import the underlying native resource.

Currently supported: **Linux Vulkan OPAQUE_FD**, **Linux Vulkan DMA_BUF** (linear,
through `export_frame_dmabuf`) and **Apple Metal**, through the wgpu renderer. Other backends return an error; there is no implicit CPU
fallback. Output is SDR sRGB-encoded RGBA8Unorm, without the debug HUD. The host
compositor performs any needed transfer conversion. For CPU pixels use
`capture_frame_rgba_with_danmaku` instead.

## Rust

Enable the `erika/wgpu` feature. Configure `PresenterConfig.player.renderer =
RendererBackendPreference::WgpuFallback` before constructing the presenter.
Open/play normally, then call once per host frame:

```rust,ignore
if let Some(frame) = presenter.export_frame_texture(1920, 1080, 2.0)? {
    let info = *frame.info();
    // Import info.native_handle into the host's native renderer.
    // Transfer frame to the host submission owner; drop it only after the
    // consumer GPU fence signals and all imported aliases are destroyed.
}
```

Each call pumps video, subtitles and audio. No additional `render_tick` is
required. `None` means no video is available yet. Playback time, danmaku
tracks, offsets and settings follow the same presenter as RGBA capture.
`scale` is the host's physical pixels per logical point, as for
`resize_surface`, so danmaku keeps its on-screen size and speed on HiDPI
outputs. Danmaku is laid out by the same background planner as on-screen
playback (at the export size when no surface is attached); after a seek,
track change or resize, comments can appear a few frames later. Unsupported
backends and sizes fail before any playback work.

### dma-buf (Linux)

A host that imports a dma-buf itself — GTK's `GdkDmabufTexture`, EGL, a Wayland
compositor, another process — needs no Vulkan device of its own:

```rust,ignore
if let Some(frame) = presenter.export_frame_dmabuf(1920, 1080, 2.0)? {
    // One plane, fourcc ABGR8888, modifier 0; `frame.stride()` bytes between
    // rows and `frame.offset()` bytes to the first pixel.
    // Import frame.raw_fd(), or take frame.into_fd() and close it when done.
}
```

It pumps and renders exactly as `export_frame_texture`: same call site, same
once-per-host-frame rule, same `None` until a video frame exists. The composite
still renders into an `OPTIMAL` target, and one same-device copy moves it into
a linear `DMA_BUF`-backed image, so the two exports differ only in the handle
they hand over — an OPAQUE_FD import needs a Vulkan device and the exact image
parameters, a dma-buf import needs a row pitch.

Exports take a slot from a small ring, so a descriptor must be closed before
the same slot is reused (three exports later). A compositor that paints each
frame does so; a host that queues very many frames should close descriptors
promptly. The buffer stays alive while any handle is open, including after the
producing image is destroyed.

## C

Use `erika_presenter_create_for_texture_export(config)` to select wgpu without
changing the existing `ErikaPresenterConfig` ABI. Then call
`erika_presenter_export_frame_texture(p, w, h, scale, &frame)` on the host frame loop.
`NoEvent` means no video yet; every non-`Ok` result clears `frame` to null.
`erika_texture_frame_get_info(frame, &info)` returns borrowed handles and
metadata. Finally call `erika_texture_frame_release(frame)` after consumer
completion and alias destruction. Null release is allowed.

Frames are immutable and can outlive their presenter, later exports, surface
changes and seeks. Each export allocates a separate texture. Retain only the
frames still needed by host GPU submissions. Producer completion is waited on
before returning (five-second timeout), so no producer semaphore is required.
This wait transfers no pixels to the CPU. There is no per-frame zero-copy FPS
guarantee; allocation and both renderers' scheduling affect throughput.

## Native import

| Kind | Import contract |
|---|---|
| `Metal` | `native_handle` is a borrowed `id<MTLTexture>`; `device_id` is its MTLDevice registry ID. Use the same GPU. Keep the frame alive until consumer commands complete, or independently retain the MTLTexture before releasing the frame and hold that native reference through completion. |
| `DmabufFrame` (from `export_frame_dmabuf`) | Single-plane linear RGBA8: fourcc `ABGR8888` (`0x3432_4241`), modifier 0, `stride` bytes between rows, `offset` bytes to the first pixel. Any dma-buf importer takes it; no Vulkan needed, no device UUID to match. Close the descriptor when done — the buffer outlives the producer. |
| `VulkanOpaqueFd` | Match both device and driver UUIDs. Duplicate the borrowed FD (FD 0 is valid); Vulkan consumes the duplicate on successful import. Create a matching 2D, exclusive image with flags 0 and one mip/layer/sample, using the supplied format/usage/tiling. Import the supplied allocation size and memory type index with `VkMemoryDedicatedAllocateInfo`. Acquire ownership from `VK_QUEUE_FAMILY_EXTERNAL`, old layout `GENERAL`, before sampling. |

The Vulkan image is never reused or written after export. The host must finish
GPU access and destroy its view/image/memory before releasing the frame.
[A native C importer](../examples/external_texture_host/vulkan_import.c) supplies
the image, sampled view, ownership barrier and cleanup. Read its
[usage notes](../examples/external_texture_host/README.md) when adapting it.

The [standalone host example](../examples/external_texture_host/README.md)
imports liberika's wgpu 29 output into wgpu 26, with no framework dependency.
Its pinned host HAL patch preserves initialized Vulkan image contents. A host
adapter must preserve native ownership/layout state; casting texture types
between crate versions or wrapping an imported image as uninitialized is
insufficient. Linux EGL/OpenGL and WebView imports are not implemented by this
OPAQUE_FD path; for a dma-buf importer use `export_frame_dmabuf` instead.

## Validation

Tests compare complete exported frames against RGBA capture, including subtitle
and danmaku pixels, and read retained frames after presenter destruction. Linux
tests create a separate Vulkan instance/device, import duplicated FDs, acquire
ownership, verify pixels and check FD closure. The consumer readback exists
only in tests. macOS tests read the exported Metal texture through a separate
command queue. The [dated validation record](investigations/issue-175/README.md) distinguishes
Apple Metal hardware, WSL NVIDIA hardware RGBA capture and Mesa llvmpipe
Vulkan interop. Hardware Vulkan and application-specific UI/performance remain
host/device acceptance checks. `export_frame_dmabuf` has no automated test yet;
it is exercised by a GTK host on RADV (see the PR that added it).
