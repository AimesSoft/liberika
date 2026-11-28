# ArtCNN weights

[中文](README.zh.md) | [English](README.md) | [日本語](README.ja.md)

`artcnn_c4f16.bin`、`artcnn_c4f16_ds.bin`、`artcnn_c4f32.bin` は [ArtCNN](https://github.com/Artoriuz/ArtCNN) の upstream ONNX release から変換したものです（MIT、`LICENSE.ArtCNN` を参照）。neutral モデルは 2026-06-11、DS モデルは 2026-08-11 の `main` branch から取得しました。

C-series モデルは luma doubler です（1 channel input、2x resolution output）。anime / line-art 向けに Manga109 で学習されています。

| Blob | Architecture | Parameters |
|------|--------------|------------|
| `artcnn_c4f16.bin` | 7 convs, 16 features, residual, DepthToSpace 2x | ~12K |
| `artcnn_c4f16_ds.bin` | 劣化ソース向け C4F16 denoise/sharpen variant | ~12K |
| `artcnn_c4f32.bin` | 7 convs, 32 features, residual, DepthToSpace 2x | ~48K |

`export_artcnn.py` で再生成できます（`onnx`、`onnxruntime`、`numpy` が必要です）。

```sh
python3 export_artcnn.py ArtCNN_C4F16.onnx artcnn_c4f16.bin \
    --test-vector ../../tests/data/artcnn/c4f16
```

blob layout はスクリプトの header に記載されており、`src/renderer/metal/upscaler.rs` から利用されています。

新しいモデルは Erika の汎用 network format（`.esr`、`src/renderer/sr/model.rs`）で保存しています。2026-11-02 に `main` から取得した ONNX を `export_artcnn_esr.py` で変換しました。

| File | Model | Parameters |
|------|-------|-----------|
| `artcnn_c4f16_dn.esr` | C4F16 denoise variant | ~12K |
| `artcnn_c4f32_ds.esr`、`artcnn_c4f32_dn.esr` | C4F32 denoise/sharpen、denoise variant | ~48K |
| `artcnn_r8f64.esr` | 8 residual blocks、64 features、SiLU（desktop build のみ同梱） | ~926K |

```sh
python3 export_artcnn_esr.py ArtCNN_R8F64.onnx artcnn_r8f64.esr \
    --test-vector ../../tests/data/artcnn/r8f64 --test-size 32x24
```

R16F96（約 420 万 parameters、8 MB）は同梱しません。同じ方法で変換し、`erika_presenter_load_upscaler_model` で読み込んでください。
