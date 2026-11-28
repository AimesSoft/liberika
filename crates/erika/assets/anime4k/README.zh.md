# Anime4K 网络

[中文](README.zh.md) | [English](README.md) | [日本語](README.ja.md)

`.esr` 文件是 [bloc97/Anime4K](https://github.com/bloc97/Anime4K)（MIT，见 `LICENSE.Anime4K`）的 Anime4K v3.2 CNN 超分网络，由提交 `7684e95`（2024-04-25）的 mpv 着色器转换为 Erika 的网络格式（`src/renderer/sr/model.rs`）。

| 文件 | 着色器 | 参数量 | 每输入像素乘加数 |
|------|--------|-------:|----------------:|
| `anime4k_upscale_{s,m,l,vl,ul}.esr` | `Upscale/Anime4K_Upscale_CNN_x2_{S,M,L,VL,UL}.glsl` | 0.9K–16K | 900–16,140 |
| `anime4k_upscale_denoise_{s,m,l,vl,ul}.esr` | `Upscale+Denoise/Anime4K_Upscale_Denoise_CNN_x2_{S,M,L,VL,UL}.glsl` | 0.9K–16K | 900–16,140 |

Anime4K 挂在 mpv 的 `MAIN`（RGB）上，而 Erika 只放大亮度，所以导入时让网络看到 `R = G = B = Y`（先扩展到全范围）：第一层的 RGB 权重相加，常量 alpha 并入偏置，VL/UL 的 RGB 残差按 BT.709 系数合成亮度残差。只提取权重，渲染时不执行着色器中的任何代码。

从仓库的 `glsl/` 目录重新生成：

```sh
ERIKA_ANIME4K_GLSL_DIR=/path/to/Anime4K/glsl \
  cargo test -p erika --lib regenerate_anime4k_assets -- --ignored --nocapture
```
