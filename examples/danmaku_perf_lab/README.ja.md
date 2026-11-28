# liberika Danmaku Perf Lab

[中文](README.zh.md) | [English](README.md) | [日本語](README.ja.md)

liberika の制御された弾幕パフォーマンス計測ハーネスです。NipaPlay から意図的に独立させてあり、弾幕密度、viewport size、media time、video decode、trace output を Flutter/UI のノイズなしで変えられます。

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

CPU decode 負荷だけを切り出したい場合は `--software` を使って software decode を強制します。

## Native Metal window stress

renderer profiling では release build を使ってください。debug Rust は GPU path の結論を出すには遅すぎます。

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

生の throughput を見たい場合は `--target-fps` の代わりに `--uncapped` を使います。uncapped mode では CAMetalLayer の display sync を無効化し、main run loop が出せる限り速く frame を回します。renderer stress には便利ですが、固定 refresh-rate の run よりノイズが多くなります。

このモードは DFM collision avoidance が通常許すよりも多くの弾幕をあえて見えるようにするため、実運用の密度ではなく renderer stress case です。性能の真実は JSONL ログにあり、window は visual sanity check 用です。

## Neural upscaler comparison

`--upscaler off|artcnn-c4f16|artcnn-c4f16-ds|artcnn-c4f32` で Metal の neural luma doubler を選びます。同じ clip を各 mode で走らせ、JSONL の `upscaler_backend`、`upscaler_fallbacks`、`upscaler_encode_ms`、`gpu_frame_ms`、`upscaled_frames`、`fps` を比較してください。DS variant は C4F16 と同じ構造と計算コストで、強く圧縮されたソース向けに denoise/sharpen されています。

```sh
cargo run --release -p danmaku_perf_lab -- \
  --window --hide-panel --items 1 \
  --video /path/to/anime_720p.mp4 \
  --target-fps 60 \
  --upscaler artcnn-c4f16-ds \
  --metrics-log /tmp/erika_sr.jsonl \
  --auto-exit 25
```

upscaler は drawable が source resolution より大きく video を表示したときだけ動作します。つまり、window の物理ピクセルが source より大きい必要があります。`gpu_frame_ms` は完了済み command buffer をサンプルします。同じ frame の cached upscale を再利用する tick がサンプルを支配するので、ネットワーク自体の isolated GPU cost を知りたい場合は `cargo test --release -p erika --test artcnn_upscaler -- --ignored --nocapture bench` を使ってください。

実験用 kernel の調整項目: `ERIKA_SR_BACKEND=scalar|matmul` で backend を固定（既定は Apple Silicon で matmul）、`ERIKA_SR_BLOCK=WxH` で scalar backend の per-thread output block、`ERIKA_SR_PXF=N` で matmul backend の pixels per simdgroup を設定します。

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

- `prepare_ms`: DFM+ prepare、measurement、filtering、track allocation、collision avoidance。
- `standalone_frame_layout_ms`: prepared layout への直接 frame query。
- `render_plan_total_ms`: frame query と glyph instance 展開、atlas snapshot access の合計。
- `current_metal_draws`: glyph の shadow / outline / fill を考慮した現在の Metal 弾幕 draw call 推定値。
- `batched_target_draws`: shadow / outline / fill を batch した場合の draw call 推定値。
- `atlas_changes`: サンプル frame 中の atlas version change 数。
- `draw_call_reduction_target`: 現在の draw call ÷ 期待 batch draw call。

window JSONL で見るべき field:

- `fps`、`tick_ms`、`pump_ms`、`render_ms`: 全体の frame health。
- `video_pump_ms`、`danmaku_plan_ms`: presenter 側の decode/import と render-plan cost。
- `danmaku_vertex_build_ms`、`danmaku_vertex_copy_ms`、`danmaku_encode_ms`: Metal 弾幕 pass の CPU cost。
- `draw_items_per_new_pass`: 新しく描画した danmaku pass あたりの glyph instance 数。
- `danmaku_vertex_bytes`: current frame で Metal instance buffer に書き込まれた byte 数。

この lab は Metal/WGPU 弾幕 batching 作業の baseline です。
