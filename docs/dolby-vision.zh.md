# Dolby Vision HDR 映射

[English](dolby-vision.md) · [日本語](dolby-vision.ja.md) · [文档目录](README.md)

本文介绍 liberika 的 Dolby Vision RPU（参考处理单元）元数据处理和色彩映射。实现参考 [libplacebo](https://github.com/haasn/libplacebo)，即 mpv Dolby Vision 支持所使用的渲染器。

## 支持的 Profile 与解码

| Profile | 格式 | 解码策略 | RPU |
|---|---|---|---|
| 5 | 单层，不向后兼容（IPTPQc2） | VideoToolbox 和 D3D11VA 硬解；移动后端回退到软解 | 支持 |
| 8 | 单层，基础层向后兼容，如 8.1 HDR10、8.4 HLG | 可保留硬解 | 支持 |

Profile 8 的基础层带有标准 HDR10 PQ 或 HLG 信号，RPU 元数据作为 NAL 单元交织在流中。Profile 7 则是 UHD 蓝光常见的双层格式（基础层与 FEL/MEL 增强层）。

macOS VideoToolbox 和 Windows D3D11VA 保留硬件纹理旁的 `AV_FRAME_DATA_DOVI_METADATA` 帧侧数据，GPU shader 可据此逐帧重塑和映射颜色。MediaCodec 和通用 AvCodec 等移动或嵌入式硬解后端可能不提供 RPU 侧数据。Profile 5 不兼容标准 YCbCr，因此这些后端通过 FFmpeg `avcodec` 软解以获取元数据；桌面 VideoToolbox 和 D3D11VA 仍保留硬解。Profile 8 即使没有 RPU，向后兼容的基础层仍可按 HDR10/HLG 显示。参见 `crates/erika/src/playback.rs` 的 `dolby_vision_decode_fallback()`。

## 处理流程

1. MP4/MKV 容器的 `dvcC`/`dvvC` 标识 Profile 5 或 8。
2. FFmpeg 解码 HEVC、解析 NAL 中的 RPU 并输出 `AV_FRAME_DATA_DOVI_METADATA`。
3. `ffmpeg.rs` 的 `frame_dovi_metadata_result` 读取 `AVDOVIMetadata`，按基础层位深归一化 pivot、按 `2^(-coef_log2_denom)` 缩放系数，生成 `DoviSourceMetadata`。
4. `pipeline.rs` 将数据打包到 vec4 对齐的 `DoviUniforms`（约 3 KB），包括多项式与 MMR 系数，并按上传位深修正信号偏移。
5. WGSL、Metal 或 HLSL shader 依次执行各分量重塑、RPU `ycc_to_rgb` 非线性矩阵、PQ EOTF 线性化、HPE 逆矩阵与 `rgb_to_lms` 组合的 LMS→RGB 变换，以及面向显示器的色调映射。

## 重塑与颜色变换

各 Y、Cb、Cr 分量按 pivot 和系数进行分段重塑。若输入 `s` 位于 `p[i]` 与 `p[i+1]` 之间，多项式段为：

```text
output = (c2 · s + c1) · s + c0
```

MMR（多变量多项式回归）混合三个输入分量：一阶包括 `R`、`G`、`B` 和 `R·G`、`R·B`、`G·B`、`R·G·B`，二阶和三阶分别加入这些项的平方和立方。实现见 shader 中的 `dovi_reshaped_signal()`。

重塑后依次减去 `nonlinear_offset`，应用 RPU 的 `ycc_to_rgb`（此时仍为 PQ 编码），将 PQ 转为线性光，以 `(HPE⁻¹ × rgb_to_lms) × linearized` 转为 RGB，然后进入 HDR 色域和色调映射。

## 色调映射

色调映射参照 libplacebo 的 `pl_shader_color_map`：源原色 RGB（绝对 nits）经 HPE-LMS、PQ 编码转换到 IPT，在 IPT 强度轴上应用曲线；libplacebo 的 `hull` 三次色度规则保护饱和度，解码回目标原色时完成原色转换。

| 代码 | `ToneMapOperator` | `ToneMapConfig::curve_param` |
|---|---|---|
| 0 | Clip | — |
| 1 | Reinhard | 对比度，默认 0.5 |
| 2 | Mobius | 线性拐点，默认 0.3 |
| 3 | BT.2390（默认） | 拐点偏移，默认 1.0 |
| 4 | Spline | 斜率对比度，默认 0.30 |
| 5 | BT.2446 method A | — |
| 6 | SMPTE ST 2094-10 | 拐点适应度，默认 0.70；CPU 逐帧求解系数 |

黑点补偿使用 `target black = target peak / contrast`（`contrast_ratio`；SDR 自动使用 1000:1，HDR/EDR 目标为 0），SDR→SDR 不受影响。编码将 `[black, peak]` 映射到 `[0, 1]`，使补偿后的下限回到码值 0。

色调映射、黑点补偿与感知色域 LUT 是否启用由**静态**母版显示峰值 L0 决定，避免暗场景逐帧关闭处理。动态 L1 峰值仍设置曲线的源峰值（`tone_map_extra`/`nits.x`）。ST 2094-10 在 PQ 域选择拐点，在绝对 nits 中求解曲线，与 libplacebo 的 `pl_hdr_rescale` 往返一致。

## 场景自适应 HDR10

HDR10 通常只有静态 ST.2086 元数据。对软件解码的 PQ 帧，`crates/erika/src/luma_stats.rs` 稀疏采样 NV12/P010 亮度平面，将样本线性化、重新编码为 PQ 后在感知域求平均，再用 libplacebo IIR 滤波（`coeff = 1 - exp(-1/20)`）平滑。结果以 nits 存入 `PlayerVideoFrame::scene_avg_nits` 和 `SourceColorState.measured_scene_avg_nits`；`tone_map_extra.y` 优先使用它，使 spline 拐点跟随场景。

采样按帧的色彩范围归一化：有限范围使用合法区间 16..235 / 64..940 扩展，与 shader 的 `expand_ycbcr_range` 一致。VideoToolbox、D3D11VA 和 MediaCodec 硬解帧没有 CPU 亮度平面，仍使用静态元数据路径；generation 变化时估计器重置，避免 seek 沿用上一场景亮度。

## 感知色域映射

HDR 色调映射到较小色域（BT.2020→BT.709）时，渲染器使用 CPU 生成的 48 × 32 × 256 IPT LUT（`renderer::gamut`）进行感知色域映射。纹素保存 `(I, P + 0.5, T + 0.5)`；I 轴覆盖目标 `[black, peak]` 的 PQ 码值。布局与索引参照 libplacebo，但色度滚降目前采用简化的死区混合及 Möbius 软裁剪，而非完整逐色相边界搜索；高饱和 BT.2020 颜色可能与 mpv 略有差异。

shader 在源原色中重建 RGB，执行 LMS-PQ-IPT 往返，在 ICh 空间采样 LUT，再解码到目标原色。纹理按源、目标、目标黑点和峰值缓存，绑定 WGSL slot 4、Metal texture 2 或 D3D11 t2；快速路径仍绑定 1×1×1 占位纹理以保持布局有效。

## 逐帧 L1 亮度与 Profile 7

RPU 动态 DM 扩展块的 L1 以 12 位 PQ 码保存 `min_pq`、`max_pq` 和 `avg_pq`。FFmpeg 将其放入 `AVDOVIMetadata` 的 `ext_block_offset` 区域，`frame_dovi_level1` 验证并提取。存在有效 L1 时，`max_pq` 设置色调映射源峰值，`avg_pq` 驱动场景拐点；静态 L0 仍决定输出模式和映射/LUT 是否启用。L1 缺失、全零或上下限倒置时，回退到静态 `source_max_pq`，没有平均值时拐点固定为 40%；不会因此拒绝整个 RPU。

Profile 7 的增强层是独立 HEVC 层，当前渲染器不解码，因此不能合成 `disable_residual_flag == 0` 要求的残差。基础层重塑曲线、颜色矩阵和 L1 调整仍被应用。`Frame::dovi_el_status()` 返回 `DoviElStatus`（`residual_requested`、`nlq_nontrivial`），播放过程对每条流限频报告 `dovi_el_not_composed`。NLQ 仅解析用于报告，不送入 shader；结果类似 mpv/libplacebo，但没有增强层带来的额外细节。

## 强制 BT.2020/PQ

Profile 5/8 的 VUI 标签可能不可靠，`pipeline.rs` 的 `SourceColorState::dovi()` 强制使用 BT.2020 原色和 PQ 传递函数，避免未指定传递函数时把 PQ 样本按 sRGB gamma 解码。

## 验证与限制

单元测试覆盖侧数据解析、uniform 打包、缺失标签时强制 PQ、L1 解析与回退、动态峰值和静态输出决策、FEL 基础层映射，以及 Profile 5/8 的硬解和软解策略。可设置 `ERIKA_DV_SAMPLE`（Profile 5）和 `ERIKA_DV_PROFILE_8_SAMPLE`（Profile 8）运行需要样本的集成测试。

当前限制：Profile 7 FEL/MEL 增强层残差不合成；移动硬解无法提供 RPU 时 Profile 5 使用软解；约 3 KB 的 uniform buffer 可能超出很老移动 GPU 的限制；软解平面需要从 CPU 上传到 GPU。

## 实现与参考

- `crates/erika/src/ffmpeg.rs`：容器与帧元数据；`playback.rs`：Profile 解码回退。
- `renderer/pipeline.rs`：数据结构与 uniform 打包；`renderer/gamut.rs`：感知色域 LUT；`luma_stats.rs`：HDR10 场景亮度测量。
- `renderer/wgpu_video.wgsl`、`renderer/metal/apple.rs`、`renderer/d3d11.rs`：各 shader 实现。
- [Dolby Vision](https://professional.dolby.com/dolby-vision/) · [libplacebo dovi_reshape](https://github.com/haasn/libplacebo/blob/master/src/shaders/dovi.c) · [FFmpeg AVDOVIMetadata](https://ffmpeg.org/doxygen/trunk/structAVDOVIMetadata.html) · [BT.2100 PQ EOTF](https://www.itu.int/rec/R-REC-BT.2100)
