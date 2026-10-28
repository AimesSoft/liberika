# ネイティブテクスチャ出力

> [English](external_textures.md) · [中文](external_textures.zh.md)

`PresenterRuntime::export_frame_texture(w, h)` は映像、字幕、有効な弾幕をネイティブ
GPU テクスチャへ直接描画します。ウィンドウ surface や Flutter registrar は不要で、
公開 API は wgpu の Rust 型を渡しません。ホストは異なる wgpu バージョンから
ネイティブリソースを import できます。

wgpu renderer の **Linux Vulkan OPAQUE_FD** と **Apple Metal** を実装しています。
その他の backend はエラーを返し、CPU へ暗黙にフォールバックしません。出力は
sRGB 値を格納した SDR RGBA8Unorm で、debug HUD は含みません。必要な transfer
変換はホストが行います。CPU ピクセルには `capture_frame_rgba_with_danmaku` を使います。

## Rust

`erika/wgpu` feature を有効にし、構築前に
`PresenterConfig.player.renderer = RendererBackendPreference::WgpuFallback`
を設定します。通常どおり open/play して、ホストの各フレームで呼び出します。

```rust,ignore
if let Some(frame) = presenter.export_frame_texture(960, 540)? {
    let info = *frame.info();
    // info.native_handle をホストのネイティブ renderer に import。
    // consumer GPU fence が完了し import した別名を破棄するまで frame を保持。
}
```

呼び出しは映像、字幕、音声を進めるため、追加の `render_tick` は不要です。
`None` は映像がまだない状態です。時刻、弾幕トラック、オフセット、設定は同じ
presenter に従います。

## C

`erika_presenter_create_for_texture_export(config)` で wgpu を選びます。既存の
`ErikaPresenterConfig` ABI は変わりません。フレームループで
`erika_presenter_export_frame_texture(p, w, h, &frame)` を呼びます。映像がなければ
`NoEvent`、`Ok` 以外では `frame` が null になります。
`erika_texture_frame_get_info(frame, &info)` は借用 handle と import 情報を返します。
consumer GPU の完了と import 別名の破棄後に `erika_texture_frame_release(frame)`
を呼びます。null は許可されます。

各出力は独立した不変テクスチャです。presenter の破棄、次の export、surface 変更や
seek の後も保持できます。GPU submission が必要とするフレームだけを保持してください。
producer GPU の完了を待って返し、timeout は五秒です。producer semaphore は不要で、
待機中の CPU ピクセル転送もありません。割り当てや両 renderer の scheduling が
影響するため、120 FPS の throughput は保証しません。

## ネイティブ import

| Kind | 契約 |
|---|---|
| `Metal` | `native_handle` は借用 `id<MTLTexture>`、`device_id` は MTLDevice registry ID。同じ GPU を使い、consumer コマンド完了まで frame を保持します。または MTLTexture を独立に retain してから frame を解放し、その参照を完了まで保持します。 |
| `VulkanOpaqueFd` | device/driver UUID を一致させ、借用 FD を複製します（0 も有効）。Vulkan は import 成功時に複製を消費します。指定の format/usage/tiling、flags 0、EXCLUSIVE、単一 mip/layer/sample の 2D image と dedicated allocation を使い、指定の size/type index で import します。サンプリング前に EXTERNAL から ownership を取得し、旧 layout は GENERAL とします。 |

producer は export 後の image を再利用・更新しません。consumer GPU の完了後、
view/image/memory を破棄してから frame を解放します。
[C importer](../examples/external_texture_host/vulkan_import.c) は image/view、ownership
barrier と cleanup を提供します。[使用説明](../examples/external_texture_host/README.md)
を読んで移植してください。

[独立ホスト例](../examples/external_texture_host/README.md) は liberika の wgpu 29
出力を wgpu 26 に import し、UI framework に依存しません。固定版ホスト HAL の
patch は Vulkan image の既存内容を保持します。ホスト adapter は ownership と
layout を維持する必要があり、異なる crate の texture 型のキャストや未初期化扱い
では内容を保持できません。Linux EGL/OpenGL、DMA-BUF、WebView import は未実装です。

## 検証

字幕・弾幕を含む全ピクセルを RGBA capture と比較し、presenter 破棄後も読みます。
Linux では独立 instance/device に FD を import し、ownership、ピクセル、FD 解放を
確認します。readback はテスト専用です。macOS では別 command queue で Metal texture
を読みます。[検証記録](investigations/issue-175/README.md) は Apple Metal hardware、WSL NVIDIA
hardware RGBA capture、Mesa llvmpipe Vulkan interop を区別しています。hardware
Vulkan、アプリ固有の UI 接続と再生性能はホスト実機での確認が必要です。
