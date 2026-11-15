# #197：无 surface 字幕导出的实际播放验证

2026-11-16，在 **Apple M5 Pro / macOS / wgpu-Metal** 上，将修复代码与原主干
`f5deb2a` 对照运行。使用 FFmpeg 生成的 8 秒黑色视频：1920×1080、30 fps、H.264，
内嵌一条持续显示的白色 ASS 字幕；解码明确选择软件 H.264。

测试调用真实 `PresenterRuntime::open`、`play` 和 `export_frame_texture(640, 360, 1.0)`，
持续约 4 秒，始终不 attach surface。独立 Metal 消费者从导出的原生纹理回读像素，
没有用第二次 presenter capture 代替验证。

| 观察值 | 原主干 `f5deb2a` | 修复版 |
|---|---:|---:|
| 墙钟时间 | 4.007 s | 4.043 s |
| 导出帧数 | 95 | 100 |
| `overlay_frames` | 95 | **0** |
| `update_overlay` 调用 | 95 | **0** |
| `clock_overlay` 调用 | 95 | **0** |
| 逐帧回读检查 | 83 帧 | 89 帧 |
| 每张检查图像中的白色字幕像素 | 698 | 698 |
| surface attached | false | false |

原主干日志中的 playback canvas 为 **3840×2160**（默认字幕分辨率策略），
而实际导出为 640×360。修复后，上述两条无消费者的 playback overlay 路径不再执行，
字幕仍出现在每张检查过的导出纹理上。两次运行的最终 PNG **字节完全相同**，共同样本见
[export.png](export.png)。

这是功能和调用次数回归验证，**不是帧率或吞吐性能基准**；95/100 的帧数差不能用来估算
性能收益。诊断数量代表 overlay 生成调用，并不意味着每次调用都让 libass 重画位图。
本次只实测了 Apple Metal，**没有复测 issue 原始的 Linux / AMD RADV 环境**。
Windows 当前不支持这个原生纹理导出 API，因此不能把 Windows 的其他测试算作该场景验收。

## 重跑

需要 macOS、可用 Metal GPU、`ffmpeg`（含 libx264）、`ffprobe`、Python 3，以及已经按
[构建说明](../../building.zh.md)准备好的 Erika 原生依赖。测试文件位于
[`crates/erika/tests/headless_subtitle_export.rs`](../../../crates/erika/tests/headless_subtitle_export.rs)，
它是需显式运行的 ignored 集成测试。

在修复版仓库根目录运行：

```sh
bash docs/investigations/issue-197-headless-subtitles-2026-11-16/run.sh
```

脚本从 [fixture.ass](fixture.ass) 生成媒体，执行真实导出测试，并将完整日志、媒体探测结果、
JSON 汇总和输出 PNG 写入 `target/issue-validation/headless-ass`。也可通过
`ERIKA_HEADLESS_ARTIFACTS` 指定输出目录。生成的 MKV 不需要进入 Git。

要复现原主干对照，在修复版根目录创建单独 worktree，仅复制测试 harness，生产代码保持原样：

```sh
ROOT="$PWD"
BASELINE="$(dirname "$ROOT")/liberika-197-baseline"
git worktree add --detach "$BASELINE" f5deb2a
cp crates/erika/tests/headless_subtitle_export.rs "$BASELINE/crates/erika/tests/"
ln -s "$ROOT/third_party/dist" "$BASELINE/third_party/dist"
ERIKA_EXPECT_REDUNDANT_OVERLAY=1 \
  bash "$ROOT/docs/investigations/issue-197-headless-subtitles-2026-11-16/run.sh" "$BASELINE"
```

上例复用已准备好的同平台原生依赖；若通过 `ERIKA_FFMPEG_DIR` 等环境变量提供依赖，保持相同
环境即可，不需要创建 `dist` 链接。对照模式断言旧版确实产生冗余 overlay；默认模式断言该计数为零。

[summary.json](summary.json) 保存原始数值、fixture 探测结果及源码/harness 哈希。
[baseline.log](baseline.log) 和 [patched.log](patched.log) 是精简执行日志；完整诊断的重复行只保留
样例与计数。目录中不包含源码副本、媒体文件或构建产物。
