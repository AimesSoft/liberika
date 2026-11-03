# ArtCNN 权重

[中文](README.zh.md) | [English](README.md) | [日本語](README.ja.md)

`artcnn_c4f16.bin`、`artcnn_c4f16_ds.bin` 和 `artcnn_c4f32.bin` 是从 [ArtCNN](https://github.com/Artoriuz/ArtCNN) 的上游 ONNX release 转换而来（MIT，见 `LICENSE.ArtCNN`）。neutral 模型来自 2026-06-11 抓取的 `main` 分支，DS 模型抓取于 2026-08-11。

C 系列模型是亮度 doubler（1 通道输入，2x 分辨率输出），针对动漫/线稿内容在 Manga109 上训练：

| Blob | Architecture | Parameters |
|------|--------------|------------|
| `artcnn_c4f16.bin` | 7 convs, 16 features, residual, DepthToSpace 2x | ~12K |
| `artcnn_c4f16_ds.bin` | 面向劣化片源的 C4F16 降噪/锐化变体 | ~12K |
| `artcnn_c4f32.bin` | 7 convs, 32 features, residual, DepthToSpace 2x | ~48K |

使用 `export_artcnn.py` 可重新生成（需要 `onnx`、`onnxruntime`、`numpy`）：

```sh
python3 export_artcnn.py ArtCNN_C4F16.onnx artcnn_c4f16.bin \
    --test-vector ../../tests/data/artcnn/c4f16
```

blob 布局记录在脚本头部，并由 `src/renderer/metal/upscaler.rs` 消费。

较新的模型以 Erika 的通用网络格式（`.esr`，见 `src/renderer/sr/model.rs`）保存，由 `export_artcnn_esr.py` 从 2026-11-02 抓取的 `main` 分支 ONNX 文件导出：

| 文件 | 模型 | 参数量 |
|------|------|--------|
| `artcnn_c4f16_dn.esr` | C4F16 降噪变体 | ~12K |
| `artcnn_c4f32_ds.esr`、`artcnn_c4f32_dn.esr` | C4F32 降噪锐化变体、降噪变体 | ~48K |
| `artcnn_r8f64.esr` | 8 个残差块，64 特征，SiLU（仅桌面平台内置） | ~926K |

```sh
python3 export_artcnn_esr.py ArtCNN_R8F64.onnx artcnn_r8f64.esr \
    --test-vector ../../tests/data/artcnn/r8f64 --test-size 32x24
```

R16F96（约 420 万参数，8 MB）不内置；用同样方法导出后通过 `erika_presenter_load_upscaler_model` 加载。
