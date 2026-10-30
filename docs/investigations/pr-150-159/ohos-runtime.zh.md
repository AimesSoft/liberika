# 鸿蒙模拟器真实运行记录

> [中文](ohos-runtime.zh.md) · [English](ohos-runtime.en.md) · [日本語](ohos-runtime.ja.md)

日期：2026-10-08。代码：`codex/pr-150-159-reviewed` 的审阅结果。
使用完整生产 Rust runtime 和 NAPI 桥接；没有替换播放器、解码器、音频输出或 Surface 实现。

## 环境和产物

- DevEco Studio 26.0.0.851，安装包华为 Authenticode 签名有效。
- 官方镜像 HarmonyOS 5.1.0.110(SP2DEVC00E110R4P11)，API 18，x86_64。
- 实例 ErikaReview，hdc `127.0.0.1:5555`，Linux 5.10.210 x86_64。
- Rust 1.93.1，OpenHarmony SDK 5.1.0.107；`--no-default-features --features wgpu`。
- HAP 使用 Studio 自带 SDK 26 编译，compatibleSdkVersion 18，在 API 18 模拟器实际安装成功。
- 当前 runtime 的 ARM64 构建、双桥接链接也通过；ARM64 未运行。

x86_64 runtime SHA256：
`7325a0e0915d828cee42c3dc64e6aa5e6870d2a15415619d9662c3dbafac8ad3`。

ArkTS 桥接：`6a8737bc254916797683935d60e9099a5f617e9158e5d145f2b588192b40e7f2`。
Flutter 桥接：`f1329a2f9405a8f894491a72c54aecb2f4f303e9ceced786124704dd1b090ecb`。
HAP 打包时会 strip；上述值对应打包前原始链接产物，不能与 HAP 内 stripped 文件直接比较。

视频为原报告中的 BBB 10 秒 H.264 文件，加 48 kHz／440 Hz 的 AAC 音轨，视频 stream copy。
音视频 fixture SHA256：
`980be2f90110acf9c407581986453e483f5d18947569fe329c3eea64c2714bff`。
附封面 MP3 为同次调查已有的 `cover-5s.mp3`。

## 真实流程和结果

HAP 的两个 XComponent 分别交给 ArkTS 和 Flutter 的生产桥接，依次运行。
每个流程先播放 4 秒，暂停 2 秒，seek 到 10 秒 EOF，seek 回 3.5 秒，
以 1.25 倍速恢复；随后 detach／attach 实际 NativeWindow，再播放附封面 MP3，
执行 64 次创建／销毁和旧 ID 调用，最后播放重建后的播放器。

| 观察点 | ArkTS | Flutter 桥接 |
|---|---:|---:|
| 初次播放 rendered / 音频消费帧 | 111 / 187236 | 116 / 191694 |
| 暂停与 EOF seek 后 decoded | 114 → 114 | 121 → 121 |
| EOF seek 后公开 state | 4（Paused） | 4（Paused） |
| Surface 重连后 rendered / 音频消费帧 | 427 / 248688 | 431 / 248688 |
| 封面 MP3 消费 / 剩余队列 | 235190 / 0 | 235190 / 0 |
| 重建后 rendered / 音频消费帧 | 91 / 151572 | 86 / 142656 |
| 视频 rendered 总计（两个实例相加） | 518 | 517 |
| 所有快照的 render / import / audio failures | 0 / 0 / 0 | 0 / 0 / 0 |

MP3 仅暴露一条音轨，decoded/rendered video 均为 0，封面未被选作视频。
旧 ID 2 的 stop 返回 `{ok:false, status:1, error:"unknown Erika player"}`；
旧 destroy 后新 ID 67 仍存活并实际播放。这同时验证双桥接单调 ID 及真实宿主调用。
暂停后存在一个已在处理的解码帧（ArkTS 113 → 114），稳定暂停到 EOF seek 的区间没有新增解码帧。

视频走模拟器的系统 decoder，计数记录为 hardwareVideoFrames；输出均经 CPU fallback，
zeroCopyVideoFrames 为 0。不能据此宣称真机硬解效率、NativeBuffer 零拷贝或物理扬声器听感已验证。
未构建 libass，字幕/HDR/真实设备丢失也不在这组鸿蒙验证内。
Flutter 部分调用的是生产 NAPI 桥接，没有启动完整 Flutter 引擎；Dart 侧证据仍是原报告的测试和分析。

![双桥接真实视频画面](evidence/ohos-live-finished.png)

截图由 hdc 取得并核实；仓库内缩小到 660 像素宽，原始 1320×2232 图保存在本机证据目录。

## 基线对照的边界

将两个桥接的 C++ 文件换回基线 `4390c78`，仍链接相同当前 runtime。
各尝试 128 次真实创建／销毁，未复用指定旧对象地址；记录为未复现，不能写成故障通过／修复对照通过。
生产 C++ 的强制地址复用 harness 已在原调查中复现旧 lookup／destroy 误伤新对象，
该确定性回归与这里的真实运行互相补充。

第一轮运行验证程序曾在 `close()` 后再次 `open()`，收到 player is closed。
`close()` 是终止性操作；改为真实销毁重建后，上表流程完成。未为这个验证程序错误修改生产代码。

## 重现材料

`evidence/ohos-*-review.jsonl` 是应用写入 filesDir 后通过 hdc 导出的原始记录。
`ohos-live-summary.json` 保存完整计数；`scripts/summarize-ohos-live.py` 验证成功标记、
播放消费、暂停状态、旧 ID 拒绝及错误计数。

`scripts/ohos-live/` 保存 ArkTS 宿主、基线对照宿主、native 类型声明和 setup 脚本。
`scripts/ohos-*-build.sh` 与对应 CMake 目录保存当前运行库／桥接的实际构建命令。
脚本沿用 Docker `/work`（隔离 worktree）、`/review-logs`（本机证据目录）、`/target`（持久产物）挂载。
实际构建前需安装 nasm，使用官方 Rust toolchain 的 OHOS std。

Windows 用官方 devecocli 创建实例、启动、构建 HAP；再用 SDK 的 hdc install 与 aa start。
完整临时项目含 fixture 和 `.so` 位于：
`C:\Users\rgliang\AppData\Local\Temp\erika-pr-review-20261005\ohos-live-app`。
不把 SDK、系统镜像、运行库二进制或账号材料提交到仓库。
