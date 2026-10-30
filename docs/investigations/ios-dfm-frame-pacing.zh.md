# iOS DFM+ 呈现时序

[English](ios-dfm-frame-pacing.md) · [日本語](ios-dfm-frame-pacing.ja.md)

liberika 已经在 CADisplayLink 驱动的串行工作线程上渲染，但其 DFM+ 位置在音视频 pumping 之后按工作线程当前时间采样共享播放时钟。因此可变的队列/pump 延迟会改变显示帧之间的滚动位移。当一次渲染排队时，之后所有回调均被丢弃，包括最新的显示目标。

iOS 驱动现通过有界、仅保留最新请求的 mailbox 传递 CADisplayLink.targetTimestamp。最多存在一个工作线程任务和一个待处理请求；工作线程繁忙时消费最新目标，不重播旧帧。工作线程每次只在一次调用期间持有 player，防止持续负载阻碍释放。驱动切换会取消待处理请求，保留现有后台仅音频路径。

新增可选 C 入口 `erika_presenter_render_tick_with_timing` 接受可为空的有符号延迟，表示调用单调时刻到显示目标的时间。Rust 在获取 presenter 锁或 pumping 媒体前将其转换为 Instant。弹幕、字幕和渲染上下文随后在该时刻采样同一个 PlaybackSnapshot。播放时钟本身、音频主时钟控制、seek generation、速率、偏移和布局/碰撞策略不变。

旧入口传入空目标并保持原采样行为。Swift 插件可选查找新入口，因此旧预编译库仍使用旧时序运行。完整使用此优化需要源码构建的库或未来包含该符号的预编译库。没有引入新的必需链接符号，也没有改变公开结构体大小。

## 验证，2026-09-29

原生运行仅使用 iPhone 17 Pro / iOS 26.5 模拟器。Rust 使用 release profile 构建，Flutter smoke shell 为 debug。未使用 Android 模拟器。

- iOS 应用使用本地构建的静态库编译并运行。`nm` 确认时序符号在静态链接后保留；实际驱动记录 `mainThread=false displayTarget=true`。
- iOS 上执行 11 项 Rust 测试：新增 presenter 目标时间测试、8 项播放时钟测试，以及现有布局 generation 和过期计划失效测试。新测试覆盖 60/120 Hz 目标采样、重复目标、暂停、长/短 seek 和 0.5/1/2x 速率。
- Swift mailbox 可执行程序在 iOS 通过：合并、繁忙时回调、拒绝过期/重复/非有限值和驱动切换。
- 3 项 iOS Dart 调度/中断契约通过；smoke harness 静态分析与头文件/API 对齐检查通过（由于未配置 CXX，跳过一项可选 C++ 检查）。
- 经 Flutter 插件和原生 presenter trace 的 12 个真实播放用例通过。暂停 seek 到 20、8、8.1 和 8 秒均更新媒体时间和 generation。暂停保持不动；偏移、隐藏 seek、空场景和连续 seek 均正常。在约 1.51 秒内，媒体时间在 0.5/1/2x 下分别前进 0.759/1.492/3.005 秒。
- 8 → 8.1 秒的原生输出截图向左移动约 38 个物理像素。返回 8 秒产生像素完全相同的图片。这验证短 seek 到达渲染输出，而非仅有 trace 数据。
- 回归运行的 render、import 和 audio failure 计数均为零。

机器可读功能证据位于 `ios-dfm-functional.json`。

此修复解决了一个已识别的时序来源；它不证明与 Titan 平滑度相当，也未测得 CPU/GPU 降幅百分比。trace 运行不是性能基准。后续后台/开销测量尝试因宿主锁定中断，未计入结果。真实设备刷新节奏、电池消耗及后台音频恢复在此仍未测量。

在相同刷新率下，此改动增加固定大小的时间戳/mailbox 工作，不增加渲染 pass。负载饱和时，渲染可用的最新帧而非丢弃它可能增加利用率。暂停仍遵循现有显示刷新行为；此补丁不优化空闲/暂停功耗。

## 复现

按仓库通常的源码构建说明构建 iOS 模拟器原生依赖及 release 静态库。创建临时 Flutter iOS 应用，以 path 依赖 `packages/erika_flutter`，并将 `packages/erika_flutter/tool/ios_danmaku_smoke.dart` 复制到其 `lib/main.dart`。在临时应用的 pubspec 声明 `assets/test.mp4`。本地测试素材：

```sh
ffmpeg -f lavfi -i color=black:s=1280x720:r=24 \
  -f lavfi -i anullsrc=r=48000:cl=stereo -t 90 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac assets/test.mp4
```

Apple Silicon 上提供仅 arm64 静态库时，临时应用的模拟器构建及 Pod 目标需排除 `i386 x86_64`。构建应用时将 `ERIKA_IOS_CAPI_STATICLIB` 指向新构建的 `liberika_capi.a`，否则插件可能选择旧版发布的预编译库。

功能检查时，以 `ERIKA_DANMAKU_TRACE=1` 和可写路径的 `ERIKA_DANMAKU_TRACE_FILE` 启动；使用 `simctl launch` 时加 `SIMCTL_CHILD_` 前缀。harness 从 8 秒处暂停开始。连接 Flutter debug VM 后：

```sh
python3 packages/erika_flutter/tool/verify_ios_danmaku.py <VM-Service-URL> \
  --trace <native-trace-file> --output /tmp/erika-functional.json
```

trace 记录 presenter 时间及字形数量，不记录物理显示延迟；截图需单独检查。未来测量 CPU/GPU 时应关闭 trace 日志、使用新进程并保持宿主未锁定。不要将此 release 原生渲染器及音视频负载直接与 NipaPlay 独立 debug overlay 或密度不匹配的 Titan 比较。

在已启动的 iOS 模拟器上直接运行 mailbox 测试：

```sh
xcrun --sdk iphonesimulator swiftc -target arm64-apple-ios13.0-simulator \
  packages/erika_flutter/ios/Classes/ErikaTickMailbox.swift \
  packages/erika_flutter/test/ios_tick_mailbox/main.swift \
  -o /tmp/erika-ios-tick-mailbox-test
xcrun simctl spawn <UDID> /tmp/erika-ios-tick-mailbox-test
```
