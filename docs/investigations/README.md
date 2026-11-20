中文 | [English](README.en.md) | [日本語](README.ja.md)

# 调查与验证记录

记录按各自的日期、提交和设备阅读。现行接入文档在 [文档目录](../README.md)。下表标明原记录的语言；日志、截图等原始证据保持原样。

| 记录 | 范围 |
|---|---|
| [#197 无 surface 字幕导出实测（中文）](issue-197-headless-subtitles-2026-11-16/README.md) | 2026-11-16，Apple Metal：1080p ASS → 640×360 原生纹理；原主干与修复版 A/B、逐帧像素与字幕层调用数 |
| [#205 HTTP 取消与连续 seek 实测（中文）](issue-205-http-seek-2026-11-16/README.md) | 2026-11-16，macOS / Windows：真实 HTTP Range、12 轮在途取消、旧主干失败对照与修复后自然结束 |
| [Issue #213：Rust 消费端与 Windows host 验证（English）](issue-213-rust-consumer-2026-11-16/README.md) | 2026-11-16；显式 HAL patch、Linux 编译链接与 Windows xtask/C API/原生检查 |
| [Vulkan 半精度空间末层优化](sr-gpu-head-2026-11-03/README.md) | 2026-11-03，RTX 5060 Ti；布局、半精度、误差验证与约 7–8% 整网收益 |
| [PR #150–#159 的目的、取舍与运行验证](pr-150-159/review.zh.md) · [English](pr-150-159/review.en.md) · [日本語](pr-150-159/review.ja.md) | 2026-10-05 调查，10-08 鸿蒙补充，10-13 主分支集成；HTTP、媒体识别、时间戳、音频、GPU 与生命周期 |
| [主分支集成与运行验证](pr-150-159/merge-validation.zh.md) · [English](pr-150-159/merge-validation.en.md) · [日本語](pr-150-159/merge-validation.ja.md) | 2026-10-13，PR #164 的集成与 Windows、Linux、OHOS 实测 |
| [鸿蒙模拟器运行记录](pr-150-159/ohos-runtime.zh.md) · [English](pr-150-159/ohos-runtime.en.md) · [日本語](pr-150-159/ohos-runtime.ja.md) | 2026-10-08，完整 Rust runtime、NAPI 桥接和模拟器 |
| [Apple 验证交接](pr-150-159/apple-handoff.zh.md) · [English](pr-150-159/apple-handoff.en.md) · [日本語](pr-150-159/apple-handoff.ja.md) | PR #154、#155、#157 的 Apple 验证范围与待办 |
| [原生检查复现步骤](pr-150-159/scripts/README.zh.md) · [English](pr-150-159/scripts/README.md) · [日本語](pr-150-159/scripts/README.ja.md) | Windows、Linux、OHOS 和 Apple 检查脚本与证据 |
| [Doctor-X 音频调查](doctorx-audio-2026-10-26.zh.md) · [English](doctorx-audio-2026-10-26.en.md) · [日本語](doctorx-audio-2026-10-26.ja.md) | 2026-10-26，macOS 偶发破音 |
| [v0.1.8 API 对齐](v0.1.8-api-alignment.md) · [English](v0.1.8-api-alignment.en.md) · [日本語](v0.1.8-api-alignment.ja.md) | 0.1.8 时的绑定、ABI 与 SDK 差异 |
| [Windows 原生 HDR 与纹理兼容](pr129-native-hdr-compatibility.zh.md) · [English](pr129-native-hdr-compatibility.en.md) · [日本語](pr129-native-hdr-compatibility.ja.md) | PR #129 的平台路径与实验 |
| [HTTP 资源生命周期](issue-125-verification.zh.md) · [English](issue-125-verification.en.md) · [日本語](issue-125-verification.ja.md) | 2026-09-28，Issue #125 的取消、重播与关闭 |
| [iOS 弹幕显示时序](ios-dfm-frame-pacing.zh.md) · [English](ios-dfm-frame-pacing.md) · [日本語](ios-dfm-frame-pacing.ja.md) | PR #146 的目标时间采样和 mailbox 设计 |
| [Linux 验证（中文）](linux-validation.zh.md) · [English](linux-validation.en.md) · [日本語](linux-validation.ja.md) | 2026-09-29，WSLg、GPU、Flutter 与 C ABI |
