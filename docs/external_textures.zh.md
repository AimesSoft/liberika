# 原生纹理输出

> [English](external_textures.md) · [日本語](external_textures.ja.md)

`PresenterRuntime::export_frame_texture(w, h, scale)` 将视频、字幕和已启用的弹幕直接
绘制到原生 GPU 纹理，无需窗口 surface 或 Flutter registrar。公开接口不传递
wgpu Rust 类型，宿主可以使用不同版本的 wgpu，通过原生资源导入进行合成。

目前通过 wgpu renderer 支持 **Linux Vulkan OPAQUE_FD**、**Linux Vulkan DMA_BUF**（线性，走
`export_frame_dmabuf`）和 **Apple Metal**。
其他后端返回错误，不会隐式回退到 CPU。输出为存放 sRGB 编码值的 SDR RGBA8Unorm，
不含调试 HUD；宿主合成器按自身需要转换 transfer。需要 CPU 像素时使用
`capture_frame_rgba_with_danmaku`。

## Rust

启用 `erika/wgpu` feature，创建 presenter 前设置
`PresenterConfig.player.renderer = RendererBackendPreference::WgpuFallback`。
正常 open/play 后，每个宿主帧调用一次：

```rust,ignore
if let Some(frame) = presenter.export_frame_texture(1920, 1080, 2.0)? {
    let info = *frame.info();
    // 将 info.native_handle 导入宿主的原生渲染器。
    // 把 frame 交给宿主 submission 持有；消费端 GPU fence 完成且所有导入
    // 别名销毁后，才能 drop frame。
}
```

该调用同时驱动视频、字幕和音频，无需额外 `render_tick`。`None` 表示尚无视频帧。
播放时间、弹幕轨道、偏移与配置沿用同一个 presenter。
`scale` 是宿主每个逻辑点对应的物理像素数，与 `resize_surface` 相同，使 HiDPI 输出
上的弹幕保持与屏幕一致的大小和速度。弹幕与屏幕播放共用后台排版（未绑定 surface
时按导出尺寸排版），seek、切换轨道或改变尺寸后，弹幕可能晚几帧出现。
不支持的后端或尺寸会在任何播放工作之前返回错误。

### dma-buf（Linux）

宿主自己就能导入 dma-buf 时（GTK 的 `GdkDmabufTexture`、EGL、Wayland 合成器、另一个进程），
不需要自己的 Vulkan 设备：

```rust,ignore
if let Some(frame) = presenter.export_frame_dmabuf(1920, 1080, 2.0)? {
    // 单平面，fourcc ABGR8888，modifier 0；行间 `frame.stride()` 字节，
    // 首像素偏移 `frame.offset()` 字节。
    // 导入 frame.raw_fd()，或者 frame.into_fd() 拿走、用完关闭。
}
```

它的泵送与渲染和 `export_frame_texture` 完全一致：同一个调用点、同样"每宿主帧一次"、
视频帧就绪前同样返回 `None`。合成仍然渲染进 `OPTIMAL` 目标，由一次同设备拷贝搬进
线性的 `DMA_BUF` 图像 —— 两种导出的区别只在交出去的句柄：OPAQUE_FD 导入需要 Vulkan
设备和完全一致的图像参数，dma-buf 导入只需要行跨度。

导出从一个小环形缓冲取槽，所以描述符必须在同一个槽被复用（三次导出后）之前关闭。
每帧绘制的合成器天然满足；一次排很多帧的宿主应当及时关闭描述符。缓冲区在任何句柄
仍打开期间都活着，即使生产端图像已销毁。

## C

使用 `erika_presenter_create_for_texture_export(config)` 选择 wgpu，不改变已有
`ErikaPresenterConfig` ABI。宿主帧循环调用
`erika_presenter_export_frame_texture(p, w, h, scale, &frame)`；`NoEvent` 表示暂无视频，
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
| `DmabufFrame`（来自 `export_frame_dmabuf`） | 单平面线性 RGBA8：fourcc `ABGR8888`（`0x3432_4241`）、modifier 0、行间 `stride` 字节、首像素偏移 `offset` 字节。任何 dma-buf 导入方都能吃；不需要 Vulkan，也不用匹配 device UUID。用完关闭描述符 —— 缓冲区比生产端活得久。 |
| `VulkanOpaqueFd` | 匹配 device/driver UUID；先复制借用 FD（0 也是合法 FD），Vulkan 仅在导入成功时消费副本。按返回的 format/usage/tiling 创建 flags 为 0、独占共享模式、单 mip/layer/sample 的二维图像。按返回的 allocation size、memory type index，配合 `VkMemoryDedicatedAllocateInfo` 导入。采样前从 `VK_QUEUE_FAMILY_EXTERNAL` 获取 ownership，旧 layout 为 `GENERAL`。 |

导出后生产端不再复用或写入图像。宿主 GPU 完成访问后，先销毁自己的 view/image/memory，
再释放 frame。[C 导入示例](../examples/external_texture_host/vulkan_import.c) 提供图像、
采样 view、ownership barrier 和清理代码；移植前阅读其
[使用说明](../examples/external_texture_host/README.md)。

[独立宿主示例](../examples/external_texture_host/README.md) 将 liberika 的 wgpu 29
输出导入 wgpu 26，不依赖 UI 框架。示例对固定版本宿主 HAL 的补丁用于保留 Vulkan
图像已有内容。宿主适配必须保留 ownership 和 layout，不能直接转换不同 crate 版本
的纹理类型，也不能将已有内容当作未初始化纹理处理。此 OPAQUE_FD 路径不实现
Linux EGL/OpenGL 或 WebView 导入；dma-buf 导入方请改用 `export_frame_dmabuf`。

## 验证

测试将完整输出与 RGBA 捕获逐像素比较，包含字幕和弹幕，并在 presenter 销毁后
读取保留的纹理。Linux 测试创建独立 Vulkan instance/device，复制并导入 FD，
获取 ownership、核验像素和 FD 释放；像素读回只存在于测试端。macOS 测试用独立
命令队列读取 Metal 纹理。[验证记录](investigations/issue-175/README.md) 分别记载 Apple Metal 硬件、WSL NVIDIA
硬件 RGBA 捕获与 Mesa llvmpipe Vulkan 互操作结果。硬件 Vulkan、具体应用的 UI
接入与播放性能仍需在宿主设备上验收。
