# 鸿蒙 GLES 初始化与桥接验证（2026-11-03）

基线为 main `77de7e4`，加上本 PR 的 GLES dispatch limit 修复。复用 Windows 工作站已有的 ErikaReview 模拟器：HarmonyOS 5.1.0 / API 18 / x86_64，Emulator 26.0.0.402，RTX 5060 Ti。

## 初始化失败与修复

原版本 nativeCreate 返回 0。Vulkan 不可用后，GLES request_device 报：

```
Limit 'max_compute_workgroups_per_dimension' value 65535 is better than allowed 33778
```

GLES 的请求值现在为 `min(基线限制, adapter 实际限制)`，本例为 33778；仍保留 compute 能力。其他后端的申请策略不变。增加针对该限制值的回归单元测试。修复后模拟器可以创建播放器并播放。

验证包括格式检查、提取生产 requested_device_limits 函数并使用 wgpu-types 29.0.3 执行两个限制测试，以及重新交叉编译生产 Rust runtime 和两套 C++ 桥接、打包 HAP、安装运行。独立函数测试不等同于完整 crate 测试。

## 桥接结果

宿主为 ArkTS XComponent，分别调用生产 `liberika_ohos.so` 和 `liberika_flutter.so`。播放使用 nativeStartFrames 的生产 owner 时钟和事件回调；JS 定时器只推进验证阶段。未启动完整 Flutter 引擎。原生构建使用 wgpu，未包含 libass。

两套桥接分别完成：

- 异步命令、ArtCNN C4F16 DN 模型加载与清除。
- 合成 640×360 / 30 fps / 10 秒 H.264 + AAC 素材的音视频播放。
- 暂停稳定后解码和音频消费停止、暂停状态跳到 EOF、恢复及 1.25 倍速。
- Surface 分离与重连、带封面的 MP3 音频播放。
- 64 次创建/销毁、旧 ID 拒绝及重建后播放。

| 统计阶段 | ArkTS 渲染帧数 | Flutter 桥接渲染帧数 |
| --- | ---: | ---: |
| 首次播放约 4 秒 | 233 | 241 |
| 重建后约 5 秒 | 298 | 296 |

计数为 presenter 的累计呈现次数，不能解释为不同解码帧或性能基准。所有采样阶段 renderFailures、importFailures、audioFailures 均为 0。模型加载后即清除，本次没有测超分推理画质或性能。

## 连续压力运行触发宿主驱动崩溃

先连续跑 ArkTS、Flutter：ArkTS 完整通过，Flutter 阶段模拟器退出。转储中的异常为 `0xc0000005`，读取地址 `0x8`，异常指令位于宿主 `nvoglv64.dll + 0xa69ab2`。退出前模拟器 qemu.log 出现 GL 错误 0x505（GL_OUT_OF_MEMORY）和 0x506（GL_INVALID_FRAMEBUFFER_OPERATION）。随后 emulator-crash-service 本身也报错；一次恢复启动被残留崩溃服务以 snapshot boot timeout 终止。结束残留服务后冷启动恢复。

单独冷启动后，Flutter 桥接同样完整通过 64 次创建/销毁及全部功能检查。汇总使用首次连续运行的 ArkTS 结果和独立运行的 Flutter 结果。连续运行的宿主驱动崩溃仍待定位，当前证据不能确定 GPU 资源累积发生在应用、wgpu、模拟器转译层还是驱动；本 PR 只修复已复现的初始化限制申请问题。

## 留存与复现

- `Index.ets`：连续运行宿主；`Index-flutter.ets`：独立 Flutter 桥接宿主，使用时替换测试工程的 Index.ets。
- `flutter.d.ts` / `ohos.d.ts`：宿主类型声明。
- `summary.json`：两次成功运行的统计与旧 ID 检查。
- 原始 JSONL 和宿主崩溃转储留存于本次任务临时结果目录，未将 Windows 转储提交仓库。
- 测试宿主入口中的异步启动失败可能以未捕获异常停止，必须检查本次运行的 COMPLETE 标记并清除旧日志，不能仅凭无 FAILED 判断成功。
- 构建过程复用 `../pr-150-159/scripts/ohos-x86_64-{deps,runtime,bridges}-build.sh`；素材中的 bbb-av-10s.mp4 本次实际是 ffmpeg testsrc2 + sine 合成文件。cover-5s.mp3 来自仓库 mp3-attached-pic.mp3。

## CI

main 的 Android run 41621110509 在 setup-android 下载 Google command-line tools 时发生 ECONNRESET，尚未进入源码构建。仅重跑失败 job 后全部成功。同期 OpenHarmony、OpenHarmony ArkTS SDK、Flutter package、Linux native 工作流通过。
