# 原生纹理输出

> [English](external_textures.md) · [日本語](external_textures.ja.md)

`PresenterRuntime::export_frame_texture(w, h)` 将视频、字幕和已启用的弹幕直接
绘制到原生 GPU 纹理，无需窗口 surface 或 Flutter registrar。公开接口不传递
wgpu Rust 类型，宿主可以使用不同版本的 wgpu，通过原生资源导入进行合成。

目前通过 wgpu renderer 支持 **Linux Vulkan OPAQUE_FD** 和 **Apple Metal**。
其他后端返回错误，不会隐式回退到 CPU。输出为存放 sRGB 编码值的 SDR RGBA8Unorm，
不含调试 HUD；宿主合成器按自身需要转换 transfer。需要 CPU 像素时使用
`capture_frame_rgba_with_danmaku`。

## Rust

启用 `erika/wgpu` feature，创建 presenter 前设置
`PresenterConfig.player.renderer = RendererBackendPreference::WgpuFallback`。
正常 open/play 后，每个宿主帧调用一次：

```rust,ignore
if let Some(frame) = presenter.export_frame_texture(960, 540)? {
    let info = *frame.info();
    // 将 info.native_handle 导入宿主的原生渲染器。
    // 把 frame 交给宿主 submission 持有；消费端 GPU fence 完成且所有导入
    // 别名销毁后，才能 drop frame。
}
```

该调用同时驱动视频、字幕和音频，无需额外 `render_tick`。`None` 表示尚无视频帧。
播放时间、弹幕轨道、偏移与配置沿用同一个 presenter。

## C

使用 `erika_presenter_create_for_texture_export(config)` 选择 wgpu，不改变已有
`ErikaPresenterConfig` ABI。宿主帧循环调用
`erika_presenter_export_frame_texture(p, w, h, &frame)`；`NoEvent` 表示暂无视频，
所有非 `Ok` 返回都会将 `frame` 清为 null。
`erika_texture_frame_get_info(frame, &info)` 返回借用的原生句柄与导入参数。
消费端 GPU 完成且导入别名销毁后，调用 `erika_texture_frame_release(frame)`；允许传 null。

每张输出纹理独立且不可变，可跨越 presenter 销毁、后续导出、surface 变化和 seek
继续持有。只保留宿主 GPU submission 仍在使用的帧，避免堆积显存。导出在生产端
GPU 完成后返回，等待超时为五秒，无需另外等待生产端 semaphore；等待不传输 CPU
像素。不保证每帧的零拷贝吞吐达到 120 FPS，分配与两端调度仍影响性能。

## 原生导入

| 类型 | 约定 |
|---|---|
| `Metal` | `native_handle` 是借用的 `id<MTLTexture>`；`device_id` 是所属 MTLDevice 的 registry ID。使用同一 GPU，消费命令完成前保留 frame；也可先独立 retain MTLTexture，再释放 frame，并持有原生引用至消费完成。 |
| `VulkanOpaqueFd` | 匹配 device/driver UUID；先复制借用 FD（0 也是合法 FD），Vulkan 仅在导入成功时消费副本。按返回的 format/usage/tiling 创建 flags 为 0、独占共享模式、单 mip/layer/sample 的二维图像。按返回的 allocation size、memory type index，配合 `VkMemoryDedicatedAllocateInfo` 导入。采样前从 `VK_QUEUE_FAMILY_EXTERNAL` 获取 ownership，旧 layout 为 `GENERAL`。 |

导出后生产端不再复用或写入图像。宿主 GPU 完成访问后，先销毁自己的 view/image/memory，
再释放 frame。[C 导入示例](../examples/external_texture_host/vulkan_import.c) 提供图像、
采样 view、ownership barrier 和清理代码；移植前阅读其
[使用说明](../examples/external_texture_host/README.md)。

[独立宿主示例](../examples/external_texture_host/README.md) 将 liberika 的 wgpu 29
输出导入 wgpu 26，不依赖 UI 框架。示例对固定版本宿主 HAL 的补丁用于保留 Vulkan
图像已有内容。宿主适配必须保留 ownership 和 layout，不能直接转换不同 crate 版本
的纹理类型，也不能将已有内容当作未初始化纹理处理。此 OPAQUE_FD 路径不实现
Linux EGL/OpenGL、DMA-BUF 或 WebView 导入。

## 验证

测试将完整输出与 RGBA 捕获逐像素比较，包含字幕和弹幕，并在 presenter 销毁后
读取保留的纹理。Linux 测试创建独立 Vulkan instance/device，复制并导入 FD，
获取 ownership、核验像素和 FD 释放；像素读回只存在于测试端。macOS 测试用独立
命令队列读取 Metal 纹理。[验证记录](investigations/issue-175/README.md) 分别记载 Apple Metal 硬件、WSL NVIDIA
硬件 RGBA 捕获与 Mesa llvmpipe Vulkan 互操作结果。硬件 Vulkan、具体应用的 UI
接入与播放性能仍需在宿主设备上验收。
