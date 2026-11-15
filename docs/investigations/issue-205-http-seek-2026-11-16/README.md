# #205：真实 HTTP Range + 连续 seek 验证（2026-11-16）

本记录使用本机 TCP HTTP 服务、真实 `HttpRangeSource`、FFmpeg、demux worker 和
`Player` 播放线程，不替换 `MediaSource`。测试片为仓库合成 fixture 的循环版本，
不是用户的 Jellyfin 媒体或现场网络。

## 结果

| 代码 / 环境 | 在途读取取消 | 取消到服务端观察到断连 | seek 后推进 | 最后视频 PTS | 自然结束 | fatal 事件 |
|---|---:|---:|---|---|---|---:|
| 原始 main `f5deb2a3702c64bcce8be960430f9276fdc8460b` / macOS arm64 | 12 / 12 | 0.14–0.31 ms | 12 轮均恢复 | 未完成 | 否 | 1 |
| 本轮修复 / macOS arm64 | 12 / 12 | 0.08–0.22 ms | 每轮恢复 7 帧，位置超过 200 ms | 159.967 s | 是 | 0 |
| 本轮修复 / Windows x64 | 12 / 12 | 0.18–0.42 ms | 每轮恢复 6–7 帧，位置超过 200 ms | 159.967 s | 是 | 0 |

取消时间只统计 HTTP socket 断连，**不代表完整 seek 延迟**。测试使用软件视频解码，
消费真实解码帧和音频帧，不包含 GPU 呈现或音频设备输出的验收。

原始 main 与修复版使用相同测试和媒体。原始 main 在最后跳到 157 秒继续播放时失败：

```text
playback_fatal: demux worker error: source error: ffmpeg error in av_read_frame:
Input/output error (-5); custom AVIO read at offset 4248361 failed earlier:
source read cancelled
```

本次 A/B 复现的是取消被写成 EIO 后，残留 AVIO latch 在后续读取中升级为 fatal 的路径。
它没有复现报告中「中段 seek 后数毫秒立即 fatal」的全部线程交错。
修复版两平台最终均呈现最后 3 秒的 90 帧并收到 `PlayerEvent::Ended`。

## 测试流程

测试入口位于 [`media_lifecycle.rs`](../../../crates/erika/tests/media_lifecycle.rs)：
`http_seek_interruption_recovers_and_reaches_natural_eof`。
这是需显式运行的 ignored 集成测试；普通测试无需生成长片。

1. 把八秒原始合成 fixture 循环到 160 秒，避免 FFmpeg 探测与缓存吃下整个短片。
2. 本机 HTTP 服务提供 HEAD、206、Content-Range 和真实媒体字节。HTTP 前读和回退窗口各 64 KiB。
3. 正常开播并收到第一张解码帧。
4. 连续执行 12 轮：seek 到 80 秒，服务端暂不发送一条 206 响应体；等请求实际抵达并延迟
   10 ms 后，依次 seek 到 2 秒、0 秒、0 秒，包含一次重复滑块释放。服务端观察客户端关闭
   该连接，随后请求恢复正常供应字节。检查无 Error、当前 generation 恢复至少 3 帧且时钟推进。
5. 最后 seek 到 157 秒，继续消费音视频帧，要求自然 Ended、最后 PTS 至少 159 秒。

## 复现

在仓库根目录执行（生成步骤的 FFmpeg 版本为 7.1.1，仅复制已有音视频码流）：

```sh
ffmpeg -hide_banner -loglevel error -y -stream_loop 19 \
  -i crates/erika/testdata/playback/playback-fixture.mkv \
  -map 0 -c copy -t 160 /tmp/erika-http-seek-fixture.mkv
ERIKA_HTTP_SEEK_FIXTURE=/tmp/erika-http-seek-fixture.mkv \
  cargo test -p erika --test media_lifecycle \
  http_seek_interruption_recovers_and_reaches_natural_eof \
  -- --ignored --nocapture --test-threads=1
```

生成媒体 SHA-256：
`ee43c7fa732907dbca2af59b6e3e698a032a02b8132534558a005b6170312bce`。
Windows 使用同一媒体文件、MSVC 原生依赖，Cargo 命令另加 `--features wgpu`；
测试本身不会创建 GPU renderer。

A/B 基线是在独立目录解包上述 main 提交，仅复制集成测试文件，不应用 ffmpeg/playback 修复。

## 原始日志

- [原始 main / macOS：失败](main-macos.log)
- [修复版 / macOS：通过](fixed-macos.log)
- [修复版 / Windows：通过](fixed-windows.log)

Windows 日志由 PowerShell 收集，长行有显示折行；归档时仅统一换行并去掉行尾空白。主机上另运行了原有
`cargo test -p erika --test media_lifecycle -- --test-threads=1`，4 项通过、2 项手动实验忽略。
