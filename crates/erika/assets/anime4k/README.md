# Anime4K networks

[中文](README.zh.md) | [English](README.md) | [日本語](README.ja.md)

The `.esr` files are the Anime4K v3.2 CNN upscalers from
[bloc97/Anime4K](https://github.com/bloc97/Anime4K) (MIT, see
`LICENSE.Anime4K`), converted from the mpv shaders of commit `7684e95`
(2024-04-25) into Erika's network format (`src/renderer/sr/model.rs`).

| File | Shader | Parameters | MAC / input pixel |
|------|--------|-----------:|------------------:|
| `anime4k_upscale_{s,m,l,vl,ul}.esr` | `Upscale/Anime4K_Upscale_CNN_x2_{S,M,L,VL,UL}.glsl` | 0.9K–16K | 900–16,140 |
| `anime4k_upscale_denoise_{s,m,l,vl,ul}.esr` | `Upscale+Denoise/Anime4K_Upscale_Denoise_CNN_x2_{S,M,L,VL,UL}.glsl` | 0.9K–16K | 900–16,140 |

Anime4K hooks mpv's `MAIN` (RGB). Erika upscales luma, so the importer feeds
the network `R = G = B = Y` (expanded to full range), sums the first layer's
RGB weights, folds the constant alpha into its bias, and combines the
RGB residuals of VL/UL with BT.709 coefficients. Only weights are extracted;
nothing from the shaders runs at render time.

Regenerate from a checkout of the repository's `glsl/` directory:

```sh
ERIKA_ANIME4K_GLSL_DIR=/path/to/Anime4K/glsl \
  cargo test -p erika --lib regenerate_anime4k_assets -- --ignored --nocapture
```
