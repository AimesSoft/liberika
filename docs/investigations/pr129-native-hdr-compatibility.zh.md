# PR #129 修复与验证记录

## 范围

- 原 PR：AimesSoft/liberika#129，head `2c5e470fd33b4a7297fa5b15bb22cb65ace1899e`。
- 对照 main：`7b0fa9179d0cd3b85e28974844f213d0e00aa058`。
- 本地修复分支：`codex/pr129-preserve-native-hdr`。
- 普通播放继续沿用 main 的原生 Windows HWND/swapchain 和 HDR10 协商路径；不以 Flutter texture 替换主路径。

## 修复结果

1. 恢复 Windows `ErikaVideoView` 到 `ErikaWindowOverlayVideoView`，透传 blendMode、opacity、debugLabel 和回调。默认 srcOver、auto/extendedLinear 不会隐式创建 texture。
2. `ErikaTextureVideoView` 才显式选择 Windows SDR BGRA8 texture。srcOver 支持 Flutter 透明度、裁剪和滤镜；overlay 转原生 Composition；其他 Windows 混合模式明确报错。删除了不能跨 TextureLayer 生效的 canvas saveLayer 包装。
3. 保留 PR 的双输入 Composition effect graph 和 FittedBox 变换矩形修复。
4. 新增纹理 API 改为可选加载，在解绑现有表面前检查能力。旧 DLL 缺少新符号不再阻止普通播放器创建。两份 C 头文件同步声明新 API。
5. GPU 输出发布为复制完成、之后不再写入的共享快照。插件仅持有 latest 和 raster callback 获取的 sampled 描述符；跳过的 resize 快照可回收。不能用 release_callback 或固定帧数推断 Flutter 已完成 GPU 采样，因此没有采用“超时强制删除快照”的建议。
6. 只有画面内容或尺寸变化才发布新快照并通知 Flutter。内容判定包括视频帧、seek generation、字幕/HUD、弹幕和超分模式；仅时钟推进不构成新画面。
7. 修复播放器销毁后纹理 owner 未清空，以及替换 overlay HWND 前旧对象引用未解绑的问题。
8. 撤回全局 D3D11VA 表面池扩容和无界退休帧队列。`ffmpeg.rs` 与上述 main 完全一致；额外 GPU handoff 等待只用于 Composition/texture，不加到普通不透明 HWND 播放。

显式 texture 为安全跨设备交接，每次画面变化会增加 GPU snapshot 分配、copy 和有界完成等待；没有 CPU 像素回读。这些开销不进入默认原生播放路径。

## 验证时发现的既有问题

- C API 的超分状态测试只对 Android 特判。Windows D3D11 在设备未绑定时本来就报告 Building，而测试期待 Inactive。在原始 main 上复现相同失败；只修改 Windows 测试预期，未修改超分行为。
- main 的异步弹幕 planner 在销毁时只发 shutdown 通知，没有等待线程结束。插件最后一个播放器释放后会卸载 DLL，尚未启动/退出的线程可能执行已卸载代码。异常地址解析到了 Rust 线程启动和分配函数；旧版 v0.1.7 DLL 压力测试也复现退出异常。修复只在 Windows 销毁时保存并 join 弹幕工作线程，先释放共享状态锁再等待，不改正常弹幕规划、播放、解码或 HDR 策略。其他平台保留原有退出策略。

## 已完成验证

- `flutter test --no-pub --reporter expanded`：104/104。
- `flutter analyze --no-pub`：无问题。
- `cargo test -p erika -p erika_capi --lib -- --test-threads=1`：核心 528/528、C API 41/41。联合运行启用了 C API 依赖的特性集合，因此数量高于单独的默认核心测试。
- `cargo build -p erika_capi`：成功生成 DLL；保留已有 unused/dead-code 和 LNK4098 警告。
- Windows 插件使用真实 Flutter SDK 完整编译、链接成功。
- 原生 CTest：3/3（GPU publication、旧 runtime API 兼容性、新 runtime texture 生命周期）。
- 新 runtime 实际插件连接与 DLL 卸载连续 50/50 次通过；不固定持有 DLL。
- GPU 测试包含 WARP 像素读回验证快照不可变、跳过 100 次 resize 发布的引用回收、10,000 次并发发布/读取，以及 idle/resize 内容失效。
- `git diff --check`、`cargo fmt --all -- --check`：通过。

旧 runtime 的兼容性测试明确固定持有 DLL，仅验证加载、创建、能力检查和纹理 owner 清理；不声称修复已发布二进制内部的线程退出缺陷。源码 runtime 测试必须实际卸载 DLL，覆盖本次退出竞态修复。

原生测试可独立配置：

```powershell
cmake -S packages/erika_flutter/windows/tests -B target/windows-plugin-tests -A x64 `
  -DFLUTTER_ENGINE_DIR="<Flutter SDK>/bin/cache/artifacts/engine/windows-x64" `
  -DERIKA_COMPAT_DLL="<released v0.1.7>/erika_capi.dll" `
  -DERIKA_SOURCE_DLL="<this checkout>/target/debug/erika_capi.dll"
cmake --build target/windows-plugin-tests --config Debug
ctest --test-dir target/windows-plugin-tests -C Debug --output-on-failure
```

## 发布与验收边界

- 当前 artifact manifest 仍固定 v0.1.7，该预编译库没有新 texture API，也没有上述线程退出修复。使用新能力需匹配的源码 runtime（仓库源码构建可设置 `ERIKA_FORCE_SOURCE_BUILD=1`）；正式发布前需构建并更新匹配的 artifact 和校验值。没有虚构或修改成尚不存在的版本。
- 尚未执行 HDR 显示器上的实际视频播放、NipaPlay 集成和真实 DWM Composition 压力测试。无窗口 GPU/API 测试不等于这些场景已验收。
- 合并前建议验证：普通 SDR/HDR10 的默认主组件播放与输出状态、暂停/seek/字幕/弹幕/超分、窗口缩放和跨显示器移动；透明扫描线 overlay；显式 texture 的暂停通知、裁剪/透明度、快速缩放及播放器切换/关闭。
- 修改保存在独立 worktree，不改用户 main 工作区的已有修改；推送到原 PR 分支须经用户明确要求，不合并到 main。

## 2026-09-07：Copilot 意见的实质性修复

- 只处理已确认的 overlay 所有权错误及重复 CPU 缓冲复制。没有增加未经复现的 `isTopmost=FALSE` 回退，也没有调整诊断文案。
- HWND 重建只恢复原 owner 的绑定；不再依赖无序容器的最后一个元素。显式切换 owner 时先解绑旧 producer；旧播放器迟到的 detach/hide 不得隐藏当前画面或切换共享 HWND。无 owner 或 owner 已解绑时，不擅自选择其他播放器。
- 同一已绑定播放器隐藏后再次显示，不因 owner 标记暂时清空而重新创建 swapchain。默认原生 HWND/HDR 路径、解码池和输出格式协商不变。
- texture 场景缓存按字幕/HUD、弹幕各自的实际内容更新；新视频帧不再导致未变化的 CPU 像素缓冲和实例列表被重新深拷贝。保留精确内容比较，没有引入可能漏更新的哈希或仅视频帧号判定；弹幕 atlas 继续使用已有 Arc 共享。
- 新增测试验证连续 120 个视频帧沿用同一份缓存分配，同时字幕像素、alpha、弹幕位置及清除操作仍正确失效。Windows native 测试用隐藏 HWND 和真实插件状态逻辑覆盖 owner 保持、独占交接、迟到解绑、无 owner、绑定失败；仅 surface attach/detach 的 C 入口被替换，不测试真实 DWM 图像效果。
- 本轮完整回归：Flutter 104/104，analyze 无问题；联合 Rust 核心 529/529、C API 41/41；插件编译及 native CTest 3/3。
