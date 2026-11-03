# Anime4K networks

[中文](README.zh.md) | [English](README.md) | [日本語](README.ja.md)

`.esr` ファイルは [bloc97/Anime4K](https://github.com/bloc97/Anime4K)（MIT、`LICENSE.Anime4K` を参照）の Anime4K v3.2 CNN upscaler で、commit `7684e95`（2024-04-25）の mpv shader から Erika の network format（`src/renderer/sr/model.rs`）へ変換したものです。

| File | Shader | Parameters | MAC / input pixel |
|------|--------|-----------:|------------------:|
| `anime4k_upscale_{s,m,l,vl,ul}.esr` | `Upscale/Anime4K_Upscale_CNN_x2_{S,M,L,VL,UL}.glsl` | 0.9K–16K | 900–16,140 |
| `anime4k_upscale_denoise_{s,m,l,vl,ul}.esr` | `Upscale+Denoise/Anime4K_Upscale_Denoise_CNN_x2_{S,M,L,VL,UL}.glsl` | 0.9K–16K | 900–16,140 |

Anime4K は mpv の `MAIN`（RGB）に hook しますが、Erika は luma だけを拡大するため、importer は network に `R = G = B = Y`（full range に展開）を与えます。最初の layer の RGB weight を合算し、定数 alpha を bias に畳み込み、VL/UL の RGB residual は BT.709 係数で luma residual にまとめます。抽出するのは weight だけで、描画時に shader のコードは実行しません。

リポジトリの `glsl/` directory から再生成するには:

```sh
ERIKA_ANIME4K_GLSL_DIR=/path/to/Anime4K/glsl \
  cargo test -p erika --lib regenerate_anime4k_assets -- --ignored --nocapture
```
