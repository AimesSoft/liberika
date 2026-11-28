# Doctor-X S01E01 macOS 偶发破音调查

日期：2026-10-26。基线：`f83ccdb7bf9e759851d42e11af2e956dd5194c6b`，分支：`codex/doctorx-audio`。
本机 NipaPlay 为 1.11.10，其 Flutter 依赖为 `erika_flutter 0.3.0`。

用户在这台 Mac 播放 Doctor-X 第一季第一集时，46 分钟和 48 分钟附近出现明显破音，切换 MDK 后正常。
用户进一步确认：回退重播时，破音位置会变化，有时不出现。

已确认 CoreAudio 输出存在一个错误：只要回调有部分 underflow，就把整个输出块标为
`OUTPUT_IS_SILENCE`。有效 PCM 的前缀已经从队列取出、计入音频时钟，却可能被 CoreAudio
当作静音跳过。这个错误会扩大短暂缺音造成的声音断点。修复仅在完全没有读到 PCM 时设置
静音标记，并在后续有 PCM 时清除该标记。

目前尚未取得 NipaPlay 在用户听到破音那一刻的 underflow 记录，因此这个确定的代码缺陷
与本次症状之间仍缺少直接运行证据；缓冲耗尽的触发原因也仍待定位。

## 媒体与解码验证

样本是本机已挂载媒体目录中的
`Doctor.X.Surgeon.Michiko.Daimon.S01E01.2012.1080p.Netflix.WEB-DL.AVC.AAC.2.0-DBTV.mkv`。
时长 3508.822 秒，视频 H.264，音频 HE-AAC、48 kHz、双声道。系统默认输出是
48 kHz 的 MacBook Pro 内置扬声器。这集不会经过环绕声下混。

| 检查 | 结果 |
|---|---|
| 标准 FFmpeg 解码 40:00–50:00 | 无解码错误；峰值 0.936851（约 −0.567 dBFS）；无 NaN/Inf |
| 约 45:00–49:00 的 AAC 压缩包直接截取 | Matroska seek 含前滚，样本实际长 245.546 秒 |
| Erika `PlaybackSession` 导出该样本为 f32 PCM | 11,786,240 个双声道帧，23,572,480 个采样值 |
| 与标准 FFmpeg 的 f32 PCM 比较 | 长度相同，逐采样完全一致，最大误差和 RMS 误差均为 0 |
| Erika PCM 峰值与时序 | 峰值 0.644198120；超幅、非有限值和大于 2 ms 的 PTS 断点均为 0 |

两份 PCM 的 SHA-256 均为：

```text
8d1801105989a09d2ba905b0171da28b4ba80596032863fcfd22dd7f8a819e8e
```

这项对比覆盖了用户指出的片段，支持继续检查实时供给和输出链路。没有对 MDK 的解码输出做逐采样比较。

## 回归与实时检查

新增的 CoreAudio 回归测试向环形缓冲写入两帧 PCM，再请求四帧，验证有效前缀、零填充尾部、
静音标记和读帧统计；随后验证完全空缓冲，以及重新补入 PCM 后的音量渐变。

旧条件 `underflow_frames > 0` 下测试失败，提示有效前缀被错误标为静音。
改为 `frames == 0` 后，`cargo test -p erika audio -- --nocapture` 的 77 项测试通过。

另用独立诊断程序运行 Presenter、VideoToolbox、Metal 纹理和真实 CoreAudio 输出，音量设为 0，
以免打断用户正在观看的内容。修复前从 47:30 开始运行 75 秒，音频读帧持续推进，
underflow 和丢帧计数均为 0。这个测试没有复现用户的偶发故障。

完整负载检查另加入同名弹幕 XML、内嵌字幕和 1920×1080 Metal 纹理。
第一次从 45:45 seek 时触发 2 秒的 worker barrier 超时，播放未开始；该次不能作为音频验证。
重试从 45:55 开始运行 165 秒，覆盖至约 48:39，underflow 和丢帧计数仍均为 0。
该次是修复后的代码，验证了正常播放路径；由于未触发 underflow，不能用它证明故障已消失。
日志为本机调查产物的 `output-danmaku-retry.log`。

## 复测入口

修复文件为 `crates/erika/src/apple.rs`。本次没有替换已安装 NipaPlay 的运行库。
下一次应用内复测应同时记录音频 underflow、渲染 tick 耗时和缓冲量，确认破音是否与缓冲耗尽同时发生。
若仍有断流，继续区分渲染线程供给延迟和媒体读取延迟。

原始 PCM、媒体片段、临时探针源码、前后测试日志和实时输出日志保留在调查机器的
`out/doctorx-audio-2026-10-26/`，不作为仓库测试素材提交。仓库内的回归测试使用合成 PCM，
可以通过上述 Cargo 测试命令独立重现。
