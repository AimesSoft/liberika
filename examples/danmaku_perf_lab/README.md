# liberika Danmaku Perf Lab

Controlled danmaku performance harness for Erika. This is intentionally outside
NipaPlay so danmaku density, viewport size, media time, video decode, and trace
output can be varied without Flutter/UI noise.

## Synthetic danmaku load

```sh
cargo run -p danmaku_perf_lab -- \
  --frames 600 \
  --rate 300 \
  --duration 120 \
  --pattern dense \
  --size 1920x1080
```

## Video-driven media time

```sh
cargo run -p danmaku_perf_lab -- \
  --video /path/to/video.mp4 \
  --frames 600 \
  --rate 300 \
  --duration 120 \
  --pattern dense
```

Use `--software` to force software decode when isolating CPU decode load.
On Windows, `--d3d11va` can be passed explicitly when checking hardware decode
selection; the `video_frames` summary reports hardware frames for both
VideoToolbox and D3D11VA.

## Windows native presenter smoke

The lab's `--window` mode is macOS/Metal-only. Use the Windows native demo for
HWND/D3D11VA presenter verification and JSONL metrics:

```sh
cargo run -p windows_native_demo --target x86_64-pc-windows-msvc -- \
  "D:\path\to\video.mp4" \
  --platform-native \
  --synthetic-danmaku 120 \
  --smoke-seconds 10 \
  --metrics-log D:\erika_windows_smoke.jsonl
```

## Native Metal window stress

Use release builds for renderer profiling; debug Rust is far too slow for useful
GPU-path conclusions.

```sh
cargo run --release -p danmaku_perf_lab -- \
  --window \
  --fullscreen \
  --target-fps 165 \
  --video /path/to/video.mp4 \
  --pattern scroll \
  --rate 600 \
  --duration 120 \
  --font-size 16 \
  --display-area 1.0 \
  --scroll-duration 10 \
  --stacking \
  --window-size 1600x900 \
  --hide-panel \
  --surface-scale 1.0 \
  --metrics-log /tmp/erika_lab_stress.jsonl \
  --auto-exit 18
```

Use `--uncapped` instead of `--target-fps` for raw throughput tests. In uncapped
mode the lab disables CAMetalLayer display sync and pumps frames as fast as the
main run loop can present them; this is useful for renderer stress but noisier
than a fixed refresh-rate run.

This intentionally creates more visible danmaku than DFM collision avoidance would
normally allow, so it is a renderer stress case rather than a real-world density
profile. The JSONL log is the source of truth for performance; the window is only
for visual sanity checks.

## Neural upscaler comparison

`--upscaler off|artcnn-c4f16|artcnn-c4f16-ds|artcnn-c4f32` selects the Metal neural luma
doubler. Run the same clip with each mode and compare the JSONL fields
`upscaler_backend`, `upscaler_fallbacks`, `upscaler_encode_ms`,
`gpu_frame_ms`, `upscaled_frames`, and `fps`:

The DS variant uses the C4F16 topology and compute cost but is trained to
denoise and sharpen heavily compressed sources.

```sh
cargo run --release -p danmaku_perf_lab -- \
  --window --hide-panel --items 1 \
  --video /path/to/anime_720p.mp4 \
  --target-fps 60 \
  --upscaler artcnn-c4f16-ds \
  --metrics-log /tmp/erika_sr.jsonl \
  --auto-exit 25
```

The upscaler only engages when the drawable shows the video above its source
resolution, so the window (in physical pixels) must be larger than the source.
`gpu_frame_ms` samples completed command buffers; ticks that reuse the cached
upscale dominate those samples, so use
`cargo test --release -p erika --test artcnn_upscaler -- --ignored --nocapture bench`
for the isolated GPU cost of the network itself.

Tuning knobs for kernel experiments: `ERIKA_SR_BACKEND=scalar|matmul` forces a
kernel backend (default: matmul on Apple Silicon), `ERIKA_SR_BLOCK=WxH` sets
the scalar backend's per-thread output block, `ERIKA_SR_PXF=N` sets the matmul
backend's pixel fragments per simdgroup.

## Atlas prewarm comparison

```sh
cargo run -p danmaku_perf_lab -- \
  --frames 600 \
  --rate 300 \
  --duration 120 \
  --pattern dense \
  --prewarm-frames 720
```

## Reading the output

- `prepare_ms`: DFM+ prepare, measurement, filtering, track allocation, collision avoidance.
- `standalone_frame_layout_ms`: a direct frame query over the prepared layout.
- `render_plan_total_ms`: frame query plus glyph instance expansion and atlas snapshot access.
- `current_metal_draws`: estimated current Metal danmaku draw calls with per-glyph shadow/outline/fill.
- `batched_target_draws`: estimated draw calls after batching shadow/outline/fill passes.
- `atlas_changes`: number of atlas version changes during sampled frames.
- `draw_call_reduction_target`: current draw calls divided by expected batched draw calls.

Window JSONL fields worth watching:

- `fps`, `tick_ms`, `pump_ms`, `render_ms`: overall frame health.
- `video_pump_ms`, `danmaku_plan_ms`: presenter-side decode/import and render-plan cost.
- `danmaku_vertex_build_ms`, `danmaku_vertex_copy_ms`, `danmaku_encode_ms`: Metal danmaku pass CPU cost.
- `draw_items_per_new_pass`: glyph instances per newly rendered danmaku pass.
- `danmaku_vertex_bytes`: bytes written into the Metal instance buffer for the current frame.

This lab is the baseline for Metal/WGPU danmaku batching work.
