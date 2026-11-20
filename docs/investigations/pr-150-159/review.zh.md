# PR #150–#159：目的、取舍与运行验证

调查日期：2026-10-05；鸿蒙运行验证补充：2026-10-08；最新主分支集成：2026-10-13。
仓库：AimesSoft/liberika。作者：TsangAsuna。
评估基线：`4390c78f4467e2047e19690700fbead6a18d8e31`。
播放内核审阅提交：`004b118`；模拟器构建支持与运行证据见分支后续提交。
分支：`codex/pr-150-159-reviewed`。

Erika 是 NipaPlay 的嵌入式播放内核，负责解码、时序、渲染、字幕、弹幕和音频输出。
这里以实际播放可靠性、宿主生命周期、平台一致性和可观测性判断贡献。
没有证据可以从 PR 文风识别其生成模型，也不能据此判断改动有没有价值。

**结论：8 个 PR 的局部改动进入合并候选；#152 缺真实受影响输入，#157 的问题前提尚未成立。**
#152 与 #159 的当前产品价值较低，后者不改变正常外挂字幕的 FFmpeg 路径。
已将适合保留的部分整理为独立提交，并修正数处原实现缺口。
集成 PR 为 [#164](https://github.com/AimesSoft/liberika/pull/164)。最新验证见 [2026-10-13 集成验证](merge-validation.zh.md)。
下文提交号、计数和环境是最初审阅及 10 月 8 日验证记录；最终主分支集成结果以上述最新验证为准。

## 逐项判断

| PR | 想解决什么 | 对项目的意义 | 处理结果 |
|---|---|---|---|
| [#150](https://github.com/AimesSoft/Erika/pull/150) | HTTP Range 返回超量、错误续传位置、未知长度重复探测 | 高：防止内存浪费、缓存错位、重复请求 | 保留，并修复恰好收到请求字节后仍等待服务器尾部的问题 |
| [#151](https://github.com/AimesSoft/Erika/pull/151) | 封面误选成视频、解码时间基契约、损坏位图字幕 | 高：正常媒体识别与损坏数据容错 | 保留三项；真实 MP3 的封面不再作为视频轨道 |
| [#152](https://github.com/AimesSoft/Erika/pull/152) | 不合理的 HDR mastering 峰值误导映射 | 低至中：合理的元数据兜底，但缺真实坏片源 | 暂缓；初步审阅分支的过滤已从本次集成撤回 |
| [#153](https://github.com/AimesSoft/Erika/pull/153) | 用户字幕／弹幕时间戳导致 panic | 高：嵌入式内核不应因输入数据崩溃 | 改为检查实际可表示范围；原作者仅检查有限值仍挡不住 `1e100` |
| [#154](https://github.com/AimesSoft/Erika/pull/154) | 音频线程异常退出、倍速桥接漏算设备队列、CoreAudio 部分样本误标静音 | 高，但需平台拆分 | 完成 WASAPI；修正队列统计与恢复预算；CoreAudio 交 Apple 验证 |
| [#155](https://github.com/AimesSoft/Erika/pull/155) | GPU 重建、导入诊断、HDR 截图、Linux 等待、OHOS 丢帧诊断 | Windows 部分高；其余不能打包背书 | 保留 D3D11 与 OHOS trace；Linux 等待没有真正限时；Metal 截图需另行修正与验证 |
| [#156](https://github.com/AimesSoft/Erika/pull/156) | 暂停 seek、未执行 pause 意图、恢复命令丢失 | 高：直接影响用户控制 | 保留三个可复现状态修复；FIFO 饱和下原“补偿”仍丢消息，未采用 |
| [#157](https://github.com/AimesSoft/Erika/pull/157) | iOS AudioQueue disposal 回调释放竞争 | 待证明：官方同步 disposal 契约与其前提冲突 | 不采用，交 Apple 用实际复现证明问题；计数器本身也未覆盖入口窗口 |
| [#158](https://github.com/AimesSoft/Erika/pull/158) | OHOS 旧 ID 指向新播放器、Flutter 晚到事件、GIF 数量上限 | OHOS ID 修复高；其他两项依据不足 | 保留双桥接单调 ID；现有 Dart 生命周期已安全，补回归；不引入任意 GIF 限制 |
| [#159](https://github.com/AimesSoft/Erika/pull/159) | 空格／Tab 分隔的 SRT/VTT 被自带解析器合并 | 低：公共 Rust 接口修复，正常播放另走 FFmpeg | 保留小修复；不宣称它修复了播放器里的外挂字幕 |

### #150：HTTP 读取契约

基线对请求 8 字节的过量响应缓存了 64 字节。原 PR 限制缓存大小有价值，但服务器
先发够请求字节、随后不结束响应时，它仍继续等尾部。审阅版本在满足范围后立即停止。
对非零续传偏移的 206 必须校验 Content-Range；未知长度探测结果也可以缓存，瞬时错误不缓存。
71 个 source 测试通过，包含实际 localhost HTTP 服务器及基线／原 PR 对照。
完整 Windows C ABI 播放另使用真实 H.264 文件和实际 Range 服务。

提交：`3123f52`。证据：`pr150-baseline.log`、`pr150-original.log`、`pr150-reviewed.log`。

### #151：媒体识别与 decoder 契约

附封面的 MP3 在基线暴露 mjpeg 视频轨道；审阅版本仅暴露真实音轨，时长仍为 1 秒。
显式设置 pkt_timebase 是契约修正。损坏 bitmap rect 按 rect 跳过，可保留同一包中的有效字幕。
45 个 FFmpeg 测试通过；Windows 实际 DLL 和 Linux 实际 shared library 都打开了该文件并检查轨道。
Linux 原生 demo 另对 5 秒附封面 MP3 消费了 144480 个音频帧；demo 的退出检查要求视频帧，
因此该音频专用输入退出码为 1，不能将整条 demo 命令写成通过。

独立 Python C ABI 宿主先以 1.5 秒观察窗口测得消费为 0，延长至 4 秒后消费了 48000 帧，
队列归零、音频错误为零，完成这段 1 秒 MP3 的实际输出。不能把短观察窗口当成确定的播放故障。

提交：`4d01f1b`。证据：`pr151-baseline-probe.log`、`pr151-reviewed-probe.log`、`final-linux-cover-audio.log`。

### #152：HDR sanity 策略

“最大 mastering 亮度低于 1 nit”不适合作为 HDR 峰值；最低亮度低于 1 nit 是正常数据，必须保留。
35 个 frame/HDR 测试通过，但均不能代替真实异常 mastering 文件和 HDR 显示器上的对比。
原 PR 没给出坏片源，0.0001 nit 是“已知 sentinel”的说法没有可核验来源。
10 月 13 日尝试生成真实异常 mastering 文件，但 FFmpeg 未暴露所需亮度元数据，未证明实际受影响路径。
因此本次集成通过 `0004667` 撤回此改动；历史测试日志保留，不作为合入依据。

提交：`bf23517`。证据：`pr152-reviewed.log`。

### #153：检查 Duration 可表示范围

有限不等于可表示。原 PR 仍会在有限 `1e100` 上触发 Duration 构造 panic。
改用 `Duration::try_from_secs_f64`，保留弹幕负数归零语义，拒绝 NaN／无限值／超范围数；
字幕小时、分钟、秒的累加使用 checked arithmetic。
93 个弹幕与 54 个字幕测试通过。实际 Windows C ABI 加载异常弹幕未崩溃；
异常外挂 SRT 返回错误，但该路径由 FFmpeg 拒绝，不能把它当成自带字幕解析器修复的运行证据。

提交：`b6c36d2`。证据：`pr153-original.log`、`pr153-reviewed-danmaku.log`、`pr153-reviewed-subtitle.log`。

### #154：WASAPI 队列和恢复

原 PR 在写入前记录 padding，实际队列 4800 帧时仍报告 1200 帧。
改为 ReleaseBuffer 成功后报告 padding + 本次写入量。恢复预算也不能在每次 reopen 成功后就重置：
若 reopen 成功但每次 render 仍失败，原方案会永远循环。现在成功 render 才标记恢复完成。
原生 Windows COM 接口 harness 调用生产 GetCurrentPadding／GetBuffer／ReleaseBuffer，4 项通过。
它有真实 ABI 交互，但仍是受控 endpoint，不能代替真实声卡故障。

初次审阅时本机默认音频 endpoint 查询返回 `0x80070490`。10 月 13 日 Remote Audio 可用期间，
已完成真实默认 endpoint 提交、附封面 MP3 消费及 WASAPI 回环录音；后续 endpoint 断开导致复验失败。
真实设备异常重连仍未验证。精确结果及录音哈希见最新集成验证。
CoreAudio 部分详见 [Apple 交接](apple-handoff.zh.md)。

提交：`84d62de`。证据：`pr154-original-com.log`、`pr154-original-native.log`、`pr154-reviewed.log`。

### #155：按平台拆开

D3D11 检测 device removed 后清理所有 device-bound 状态，并让既有路径建新设备；导入失败计数也有价值。
在真实 WARP 设备上执行失效、创建新设备、Flutter target 重建和像素读回：资源 identity 更换，白色像素一致。
20 个 D3D11 测试通过。真实 Windows D3D11VA 播放另累计 289 个 direct／zero-copy 帧，导入和渲染错误为零。
未诱发真实 TDR，因此“真实 GPU 重置恢复”仍未验证。

Linux 的 100 ms poll 没有限制底层 release 中的 barrier／无限 fence wait／DeviceWaitIdle，
不能实现其承诺。也不能据此直接断言 UAF：同队列 barrier 会在释放前等待。
Metal 的 BGRA8 capture target 与 EDR 模式的 RGBA16Float pipeline cache 可能不匹配，
需要专用 capture pipeline 和 alpha 验证。OHOS textureless diagnostic 是小型、可保留的可观测性改动。

提交：`4adc1b4`。证据：`pr155-reviewed-windows.log`、`final-windows-live-capi.log`。

### #156：真实状态修复与无效补偿

基线分别复现暂停 seek 越过尾帧改为 Ended、暂停意图被 worker 运行时钟覆盖。
审阅版本保持 Paused、放弃 decode-suspended 时不可能完成的预览，并让未执行的 pause 序列拥有冻结时钟。
Windows 实际 H.264 本地与 HTTP 播放均执行暂停、拖到 10 秒 EOF、拖回 3.5 秒、恢复和倍速；
EOF seek 后公开状态仍为 Paused，暂停期间 decodedVideoFrames 不增长。

原 PR 对 FIFO 发送超时的“补偿”使用相同满队列上的 try_send；两次仍会都丢弃。
确定性的队列饱和验证有 2 项失败，故未保留这部分，也不宣称 transition resume 饱和问题已解决。
96 个 playback／119 个 core 过滤测试通过；最终 Windows 和 Linux 完整相关套件也通过。
预览测试采用实际 elapsed deadline 与 yield，避免紧密轮询饿死异步 demux。

提交：`e7cf59e`、`004b118`。证据：`pr156-baseline-preview.log`、`pr156-baseline-clock.log`、`pr156-original-saturation.log`。

### #157：前提未证明，方案也未闭合

当前调用点全部使用 `AudioQueueDispose(queue, true)`。
[Apple 官方契约](https://developer.apple.com/documentation/audiotoolbox/audioqueuedispose(_:_:))
说明 true 为同步 disposal，返回后不再执行回调。作者未给出违反该契约的复现。
即便有此类实现缺陷，回调在递增计数之前已经解引用 user_data，入口窗口仍未受计数器保护。
有限等待并泄漏 state 的改动不能凭此合入。验证要求见 [Apple 交接](apple-handoff.zh.md)。

无代码提交。作者 CI 有 iOS／tvOS 编译，不能误写为没有平台检查；编译不证明该竞争。

### #158：OHOS ID 有实质价值，另外两项不宜顺带合入

两个 NAPI 桥接原来用 presenter 地址作 ID；分配器复用地址时旧宿主调用可击中新播放器。
改为进程内单调 ID，与其他宿主实现方向一致。
host harness 强制每次 presenter 返回同一地址 0x10，执行两个生产文件的 NativeCreate、FindPlayer、NativeDestroy；
基线都失败，修复后旧 lookup／destroy 不影响新对象。5 个 packaging 检查实际执行 C++，无跳过。

SDK 5.1.0.107 / API 18 已安装。本分支 Rust C API runtime 已成功编译为 ARM64 和 x86_64，
两个完整生产桥接均链接本分支运行库。早期链接 v0.2.1 release 的构建日志仅作为历史证据保留。
DevEco Studio 26.0.0.851 与官方 HarmonyOS 5.1.0.110 手机镜像已安装；
本地 x86_64／API 18 模拟器通过 hdc 连接，真实 HAP 已安装并运行。

两个生产桥接分别完成 H.264＋AAC 播放、暂停、暂停 seek 到 EOF、seek 回 3.5 秒、
1.25 倍速恢复、Surface detach／attach、附封面 MP3、销毁重建与旧 ID 调用。
各执行 64 次真实创建／销毁后，旧 ID 2 的 stop 返回 unknown player，旧 destroy 不影响新 ID 67；
新播放器均继续输出视频和音频。渲染／导入／音频错误均为零，截图已核实。
这是 ArkTS HAP 调用 Flutter 的完整生产 NAPI 桥接，不是完整 Flutter 引擎／NipaPlay 应用运行证明。

另将两个桥接换回基线、仍链接同一当前 Rust runtime，各尝试 128 次真实创建／销毁，
没有复用到指定旧地址。不能将这组结果写成真实基线故障复现；
地址复用缺陷的确定性证据仍来自上述强制复用生产 C++ harness。
详细计数与重现说明见 [鸿蒙运行记录](ohos-runtime.zh.md)。

Flutter 现有 Dart 代码已经正确处理 dispose 等待期间的事件、关闭流、100 个晚到事件及未知 ID；
113 项测试通过，analysis 无问题。无需笼统吞掉 StateError。
GIF 36000 帧硬上限属于任意产品限制，不能代替取消、进度或显式预算设计。

提交：`627b846`。证据：`pr158-baseline-ohos.log`、`pr158-reviewed-ohos.log`、
`ohos-arkts-review.jsonl`、`ohos-flutter-review.jsonl`、`ohos-live-summary.json`。

### #159：公共解析接口有改进，播放器收益有限

SRT/VTT 按 whitespace-only 行分块修复了公共 `parse_srt`／`parse_webvtt` 接口，
覆盖空格、Tab、LF／CRLF 和多行 cue。基线合并两条 cue，修复后 56 个字幕测试通过。
但 `ExternalSubtitleSession::open` 使用 Demuxer＋FFmpeg SubtitleDecoder，自带文本解析器未接入该路径。
实际 Linux C ABI＋wgpu＋libass 已从 whitespace-separated SRT 在 3.5 秒处显示“Second”，
保存并检查了截图；此运行路径不受 #159 的解析器改动影响。

仅暂停后 seek 的预览没有显示 cue；短暂恢复再暂停后显示。此现象需要独立调查，
不能把解析器测试或像素差异笼统当成暂停字幕预览通过。
截图对照还包含 track switch 引发的视频更新，因此 changed-pixel 数不是字幕面积。

提交：`28e949a`。证据：`pr159-baseline.log`、`pr159-reviewed.log`、`final-linux-subtitle-pixels.log`。

## 初次审阅及 10 月 8 日的真实运行与覆盖边界

| 环境 | 实际执行 | 结果与边界 |
|---|---|---|
| Windows 真实 D3D11VA | 隐藏 HWND＋实际 DLL，真实 BBB 本地与 localhost HTTP Range，暂停／EOF seek／恢复／倍速 | 289 direct／zero-copy、654 rendered，0 import／render errors；片段无音轨 |
| Windows WARP | device 状态失效、新设备、新 Flutter targets、像素读回 | 20 项通过；未诱发真实 TDR |
| Windows WASAPI | 实际 COM ABI 受控 endpoint | 4 项通过；本机无默认 endpoint，真实声卡未验证 |
| Linux X11＋Mesa＋PulseAudio | 独立原生窗口，5 秒音视频；独立 C 宿主暂停、显示重连、seek、resize、恢复、destroy | 262 video／144000 audio；C 宿主 328 video／103200 audio；错误 0／0 |
| Linux 真实 H.264 | 实际 BBB 文件经 FFmpeg 解码＋wgpu 呈现 | 295 rendered，0 errors；软件 llvmpipe，不是 VAAPI／NVDEC 硬件证据 |
| Linux 字幕 | 实际 shared library＋Xlib＋libass，外挂 SRT、seek、恢复后暂停、RGBA capture | 画面“Second”已肉眼核实；纯暂停预览未通过 |
| Linux 附封面 MP3 | 独立 Python C ABI 宿主，真实 MP3 解码、PulseAudio 输出 | 48000 audio frames、队列归零、0 audio errors；null sink 无听感验证 |
| Linux PulseAudio | 正式 native pause／resume／flush／reconfigure 测试＋null sink | 通过；null sink 不证明扬声器听感 |
| OHOS 官方手机模拟器 | 当前 Rust runtime＋两个生产桥接，真实 HAP／XComponent、H.264＋AAC、暂停／EOF seek／倍速／Surface 重连／封面 MP3／销毁重建 | ArkTS 518 rendered、Flutter 517 rendered；MP3 各消费 235190 音频帧；旧 ID 拒绝，错误均为零；系统 decoder 经 CPU 回退呈现，非真机零拷贝证据 |
| Apple | 本机未运行 | 由用户单独处理，不能标记完成 |

使用的 BBB 文件 969201 字节，SHA256：

`18b99ec25f32f6bd2223aa54e4b5632533328bf5cc81c283eba7604c42649f75`。

下载来源：

<https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/720/Big_Buck_Bunny_720_10s_1MB.mp4>

原视频来自 Blender Foundation / Big Buck Bunny，媒体元数据标注 CC BY 3.0。

## 初次审阅汇总回归

Windows：erika 单元 702 通过／1 ignored，integration 4＋6 通过，C API 42 通过。
Linux：erika 单元 677 通过／2 ignored，integration 4＋6 通过，C API 41 通过。
Flutter 113 通过，flutter analyze 无问题；packaging 5 通过；Rust fmt 通过。
鸿蒙模拟器依赖构建新增 x86_64 target 支持；xtask 26 项通过，ARM64／x86_64 实际构建均通过。
Windows clippy 完成，但有现存 warnings，未以 `-D warnings` 运行。

整个 `--workspace` 在 Apple-only demo 的 ObjC 构建脚本失败，后续验证限定核心与 C API。
Linux synthetic Vulkan direct-frame 检查因 llvmpipe 缺 synchronization2／external-semaphore 能力失败，
不是该路径验证通过。#155 的 Linux 代码没有被采用。

原作者 10 个 PR 的 fork CI 均成功，而且运行 SHA 与审阅的 head 精确匹配；
详情见 author-ci.json。CI 通过不能替代上面的功能与运行判断。

## 重现材料

[evidence](evidence/) 保存基线、原实现、审阅版本的针对性日志，及真实运行日志。
[scripts](scripts/) 保存独立宿主和 SDK 构建命令。它们依赖说明中的工具、媒体和路径，
不把 mock／编译检查当成平台运行测试。

本机完整原始证据、媒体、构建产物与 SDK 位于
`C:\\Users\\rgliang\\AppData\\Local\\Temp\\erika-pr-review-20261005`。
原工作区未修改；全部代码在隔离 worktree 内。
