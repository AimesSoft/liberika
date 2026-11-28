# liberika Danmaku Perf Lab

[中文](README.zh.md) | [English](README.md) | [日本語](README.ja.md)

liberika 的受控弹幕性能测试工具。它刻意独立于 NipaPlay，这样弹幕密度、viewport 大小、media time、视频解码和 trace 输出都可以在没有 Flutter/UI 干扰的情况下自由变化。

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

使用 `--software` 可在隔离 CPU decode 负载时强制软件解码。

## Native Metal window stress

做 renderer profiling 时请使用 release build；debug Rust 对 GPU path 来说太慢，不适合做有意义的结论。

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

需要原始吞吐时，用 `--uncapped` 替代 `--target-fps`。在 uncapped 模式下，lab 会关闭 CAMetalLayer 的 display sync，并尽可能快地推进主 run loop；这适合 renderer stress，但比固定刷新率测试更嘈杂。

这个模式会刻意制造比 DFM collision avoidance 通常允许的更多可见弹幕，因此它是 renderer stress case，而不是现实密度画像。JSONL 日志才是性能真相；窗口只用于肉眼检查。

## Neural upscaler comparison

`--upscaler off|artcnn-c4f16|artcnn-c4f16-ds|artcnn-c4f32` 用来选择 Metal 神经亮度 doubler。用同一段视频分别跑不同模式，再比较 JSONL 字段 `upscaler_backend`、`upscaler_fallbacks`、`upscaler_encode_ms`、`gpu_frame_ms`、`upscaled_frames` 和 `fps`。DS 变体使用与 C4F16 相同的结构和计算成本，面向严重压制片源的降噪与锐化：

```sh
cargo run --release -p danmaku_perf_lab -- \
  --window --hide-panel --items 1 \
  --video /path/to/anime_720p.mp4 \
  --target-fps 60 \
  --upscaler artcnn-c4f16-ds \
  --metrics-log /tmp/erika_sr.jsonl \
  --auto-exit 25
```

只有当 drawable 显示的视频高于源分辨率时，upscaler 才会启动，所以窗口物理像素必须大于源视频。`gpu_frame_ms` 采样的是已完成 command buffer；那些复用缓存 upscaled 结果的 tick 会主导这些样本，因此如果要单独看网络本体的 GPU 成本，请用 `cargo test --release -p erika --test artcnn_upscaler -- --ignored --nocapture bench`。

实验 kernel 的调参开关：`ERIKA_SR_BACKEND=scalar|matmul` 强制 kernel backend（默认：Apple Silicon 上用 matmul），`ERIKA_SR_BLOCK=WxH` 设置 scalar backend 的每线程输出块，`ERIKA_SR_PXF=N` 设置 matmul backend 的每个 simdgroup 像素片段数。

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

- `prepare_ms`：DFM+ prepare、measurement、filtering、track allocation、collision avoidance。
- `standalone_frame_layout_ms`：prepared layout 的直接 frame query。
- `render_plan_total_ms`：frame query 加 glyph instance 扩展和 atlas snapshot access。
- `current_metal_draws`：按 glyph shadow/outline/fill 计算的当前 Metal danmaku draw call 估计。
- `batched_target_draws`：按 shadow/outline/fill 批处理后的 draw call 估计。
- `atlas_changes`：采样帧期间 atlas version 改变次数。
- `draw_call_reduction_target`：当前 draw call 除以预期批处理 draw call。

窗口 JSONL 字段里值得关注的有：

- `fps`、`tick_ms`、`pump_ms`、`render_ms`：整体 frame 健康度。
- `video_pump_ms`、`danmaku_plan_ms`：presenter 侧的 decode/import 与 render-plan 成本。
- `danmaku_vertex_build_ms`、`danmaku_vertex_copy_ms`、`danmaku_encode_ms`：Metal danmaku pass 的 CPU 成本。
- `draw_items_per_new_pass`：新渲染的 danmaku pass 中 glyph instance 数量。
- `danmaku_vertex_bytes`：当前 frame 写入 Metal instance buffer 的字节数。

这个 lab 是 Metal/WGPU 弹幕 batching 工作的基线。
