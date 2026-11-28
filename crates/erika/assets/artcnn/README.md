# ArtCNN weights

`artcnn_c4f16.bin`, `artcnn_c4f16_ds.bin`, and `artcnn_c4f32.bin` are converted from the upstream ONNX
releases of [ArtCNN](https://github.com/Artoriuz/ArtCNN) (MIT, see
`LICENSE.ArtCNN`). The neutral models were fetched from `main` on 2026-06-11;
the DS model was fetched on 2026-08-11.

The C-series models are luma doublers (1 channel in, 2x resolution out)
trained on Manga109 for anime/line-art content:

| Blob | Architecture | Parameters |
|------|--------------|------------|
| `artcnn_c4f16.bin` | 7 convs, 16 features, residual, DepthToSpace 2x | ~12K |
| `artcnn_c4f16_ds.bin` | C4F16 denoising/sharpening variant for degraded sources | ~12K |
| `artcnn_c4f32.bin` | 7 convs, 32 features, residual, DepthToSpace 2x | ~48K |

Regenerate with `export_artcnn.py` (needs `onnx`, `onnxruntime`, `numpy`):

```sh
python3 export_artcnn.py ArtCNN_C4F16.onnx artcnn_c4f16.bin \
    --test-vector ../../tests/data/artcnn/c4f16
```

The blob layout is documented in the script header and consumed by
`src/renderer/metal/upscaler.rs`.

Newer models are stored in Erika's generic network format (`.esr`,
`src/renderer/sr/model.rs`), exported with `export_artcnn_esr.py` from the
ONNX files fetched from `main` on 2026-11-02:

| File | Model | Parameters |
|------|-------|-----------|
| `artcnn_c4f16_dn.esr` | C4F16 denoising variant | ~12K |
| `artcnn_c4f32_ds.esr`, `artcnn_c4f32_dn.esr` | C4F32 denoise/sharpen and denoise variants | ~48K |
| `artcnn_r8f64.esr` | 8 residual blocks, 64 features, SiLU (desktop builds only) | ~926K |

```sh
python3 export_artcnn_esr.py ArtCNN_R8F64.onnx artcnn_r8f64.esr \
    --test-vector ../../tests/data/artcnn/r8f64 --test-size 32x24
```

R16F96 (~4.2M parameters, 8 MB) is not bundled; export it the same way and
load it with `erika_presenter_load_upscaler_model`.
