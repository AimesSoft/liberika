# 2026-10-13：主分支集成与真实运行验证

> [中文](merge-validation.zh.md) · [English](merge-validation.en.md) · [日本語](merge-validation.ja.md)

集成 PR：[AimesSoft/liberika #164](https://github.com/AimesSoft/liberika/pull/164)。
基线：`14c1d5511a0a16c34cf2c5f5fe9132d1f17fce65`。
最终生产代码：`211f347acf9d67fb23424d98de436d87931f9318`；后续提交仅更新验证宿主与证据。
Windows/Linux 的实际构建来自 `051bfa4`；`211f347` 仅增加 x86_64 OHOS 专用扩展屏蔽。
原工作区保留，所有修改和构建均在审阅 worktree 内。

## 采用范围与项目目标

保留 #150、#151、#153、#154、#155、#156、#158、#159 的有效部分，分别改善 HTTP 读取、媒体识别、
时间戳输入容错、WASAPI 输出、D3D11 资源重建、暂停/seek 意图、OHOS 对象生命周期和公共字幕解析。
这些属于嵌入式播放内核的可靠性目标。原作者已在采用代码的提交中署名。

#152 的 HDR 过滤已撤回：生成的实际文件没有经 FFmpeg 暴露所需异常 mastering 亮度，未证明受影响路径。
#157 未采用：同步 AudioQueueDispose 的竞争前提及修复完整性均未证明。
CoreAudio、Metal 和其他 Apple 运行验证交由用户处理；Linux 的无效超时、FIFO 上的无效补偿、
重复 Dart 生命周期修复和任意 GIF 帧数上限也没有合入。逐项依据见 [原审阅报告](review.zh.md)。
集成保留当前 main 已有的整文件 HTTP 200 非零偏移前缀跳过逻辑。

## 实际运行

以下宿主调用生产运行库、设备 API 和窗口系统。单元测试计数另列，不能替代这些运行。

| 环境 | 实际执行及结果 | 证据 |
|---|---|---|
| Windows / D3D11VA | 实际 DLL、HWND、本地 BBB、HTTP 206、忽略 Range 返回整文件 200；64 KB 缓存强制非零偏移请求；暂停、EOF seek、回拖、恢复、倍速、异常输入；最终 463 个 direct/zero-copy 帧、990 次呈现，导入/渲染错误均为 0 | `merge-pinned-windows-live.log` |
| Windows / WASAPI | Remote Audio 可用时，生产默认 endpoint 提交检查通过；附封面 MP3 实际消费 30720 帧（宿主沿用 1.5 倍速），音频错误 0；随后真实 WASAPI 回环捕获到非零 PCM | `merge-wasapi-endpoint.log`、`merge-windows-live.log`、`merge-wasapi-loopback.wav` |
| Linux / X11、Mesa、PulseAudio | 原生 demo 实际输出 278 video / 144000 audio frames；独立 C ABI 宿主执行显示重连、seek、resize、恢复、销毁；真实 BBB 呈现 287 帧；错误均为 0 | `merge-final-linux-desktop.log` |
| Linux / libass | 实际外挂 SRT 在 seek 后短暂恢复再暂停，截图显示第二条字幕 “Second”；附封面 MP3 只暴露音轨，实际消费 48000 帧、队列清空、音频错误 0 | `merge-final-linux-subtitle.log`、`merge-linux-subtitle-second.png` |
| OHOS / 官方 API 18 模拟器 | 实际 HAP、XComponent、当前 Rust runtime、两套完整生产 NAPI 桥接；完成下述全部流程并导出严格检查结果 | `merge-api18-ohos-live-summary.json`、两份 JSONL、截图 |
| OHOS / 官方 API 24 模拟器 | 同一最终运行库和 HAP，在新的官方镜像上完成相同流程；初始化崩溃的修补前日志也保留 | `merge-api24-ohos-live-summary.json`、两份 JSONL、截图、`merge-api24-cppcrash.log` |

OHOS 两套桥接分别执行 H.264+AAC 播放、确认暂停状态后的连续计数观察、暂停 seek 到 EOF、
seek 回 3.5 秒、1.25 倍速恢复、真实 NativeWindow detach/attach、5 秒附封面 MP3、64 次真实创建/销毁、
旧 ID 调用与销毁、新实例重新播放。汇总器要求所有阶段完成，暂停期间解码/音频消费稳定，EOF seek 仍为 Paused，
恢复后继续解码，MP3 无视频帧且音频队列清空，旧 ID 被拒绝且新实例继续输出视频和音频，所有错误计数为零。
MP3 在四组桥接/镜像运行中各消费 235190 帧。原始计数见 JSON 汇总。

## 真实运行揭示的修补与宿主修正

1. API 18 模拟器的 GLES adapter 只允许 7 个 inter-stage variables，WebGL2 基线请求 15 导致真实 nativeCreate 返回 0。
   GL 路径现在按 adapter 上限申请，当前视频 shader 可正常创建和呈现。
2. 新 API 24 镜像的 Vulkan loader 声明 cooperative matrix 扩展，却在属性查询入口 SIGABRT。
   仅对 x86_64 OHOS 屏蔽此可选扩展的查询和暴露；视频渲染不使用它，ARM64 路径不受影响。
3. 64 次设备/renderer 分配有时超过旧宿主的全局播放截止时间。新实例的观察窗口改为从实际 open 完成后计算 5 秒。
4. 暂停命令是异步控制，暂停前的快照与命令执行间可能仍有一批设备消费。
   宿主现在确认 Paused 后采样，再观察一秒，要求解码和音频读计数均稳定；没有删除暂停验证。
   提前收集或中途停止的运行不计入最终通过证据。

## 产物与录音

| 产物 | SHA256 |
|---|---|
| Windows 最后实际运行的 DLL | `1589cdf8f8e6a62afbff9886a8cf914d4d1df1e7afbdcec96cac7dc36736722f` |
| Linux 实际运行的 shared library | `90a54b9d7d2fae8d87b74ce85ca938c1770421c5674b760582669e6052c92766` |
| OHOS 两镜像实际运行的 Rust runtime | `61048133599712676166b807456799ae172cb8077cb6df05f53c716f0743561b` |
| OHOS Flutter 生产桥接 | `f1329a2f9405a8f894491a72c54aecb2f4f303e9ceced786124704dd1b090ecb` |
| OHOS ArkTS 生产桥接 | `6a8737bc254916797683935d60e9099a5f617e9158e5d145f2b588192b40e7f2` |
| 最终两镜像使用的 HAP | `f70b1cdda011e6fde7ce95f394b4775d5ca2abf0dbfefed8e818af24ad0030c0` |
| WASAPI 回环 WAV | `4166e50512f96f3c03d7b9273a55948258826e4d3f1833ce5b80617c280e036b` |

回环 WAV 为 48 kHz / 双声道 / 16-bit / 96000 帧。重新读取录音得到 RMS `0.01962782635`、峰值 `0.09189123203`。
最初浮点捕获输出为 RMS `0.01963214017`、峰值 `0.09189660847`，音频消费 54176 帧、错误 0；
该成功文本在后来失败的重跑中被覆盖，浮点值来自当时工具输出，WAV 是保存下来的原始证据。
该录音宿主阻塞捕获期间没有泵视频 tick，出现 D3D 解码回退告警，只用于证明真实音频输出。
最终 D3D 视频验证使用独立的持续 tick 宿主。

## 回归检查及失败尝试

Windows：712 core + 4/6 integration + 42 C API 通过，1 ignored。
Linux：687 core + 4/6 integration + 41 C API + 26 xtask 通过，2 ignored。
Flutter 113 通过，analyze 无问题；packaging 5 项（包含执行生产 C++ 桥接）通过；格式和 license 检查通过。
最终远端 CI 以 [PR #164 的提交检查](https://github.com/AimesSoft/liberika/pull/164/checks) 为准。

Windows 初次默认并行套件出现 P010 像素比较差异（60 vs 65），主分支对应单测及本分支 Vulkan 单线程完整套件通过；
保留初次失败日志，未将它伪装为已修复的生产缺陷。HDR 文件探测失败日志也保留。
WASAPI 后续重跑因默认 endpoint 消失报 `0x80070490`；最后的 DLL 视频运行显式使用 `--video-only`。
已通过的 WASAPI 生产代码在此后没有修改，但没有把这次缺 endpoint 的重跑算作音频通过。

## 覆盖边界

- 没有实际 Apple 运行证据；平台编译通过不能代替用户的 Apple 验证。
- Windows 未诱发真实 TDR、未验证真实故障设备重连；D3D11 未实现可选 C API screenshot。
- Linux 为 Mesa llvmpipe / Pulse null sink，无物理 VAAPI/NVDEC、扬声器听感证据。
- OHOS 为官方 x86_64 模拟器，系统 decoder 经 CPU 回退呈现；没有真机零拷贝证据。
  ArkTS HAP 调用的是 Flutter 的完整生产 NAPI 桥接，没有运行完整 Flutter 引擎或 NipaPlay。
- 纯暂停字幕 seek 预览仍是独立未解决观察；成功画面包含短暂恢复，不能宣称该问题已修复。
- #159 改的是公共 Rust 文本解析 API；正常外挂字幕走 FFmpeg，实际字幕播放仅证明该路径没有被破坏。

完整原始证据和产物位于本机 `C:\Users\rgliang\AppData\Local\Temp\erika-pr-review-20261005`。
仓库 [evidence](evidence/) 保存可核验日志、JSONL、截图和回环录音；重现说明见 [scripts](scripts/README.md)。
