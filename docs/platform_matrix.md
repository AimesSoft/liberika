# liberika platform capability matrix

> [中文](platform_matrix.zh.md) · [English](platform_matrix.md) · [日本語](platform_matrix.ja.md)

These platforms use 0.3.4 as their baseline. The browser backend is integrated into Erika Web and a separate Web archive is available in the Release. Linux remains experimental; the current release workflow adds x64 / ARM64 × bundled / system packages, all built on Ubuntu 22.04.

| Platform | Minimum | Decode / render | Audio | Distribution and builds |
|---|---|---|---|---|
| macOS | 11 | VideoToolbox / software + Metal | CoreAudio | arm64, x64, universal; Flutter / SwiftPM |
| iOS | 13 | VideoToolbox / software + Metal | AudioQueue | Device and simulator XCFramework; Flutter / SwiftPM |
| tvOS | 13 | VideoToolbox / software + Metal | AudioQueue | Device and simulator XCFramework; Flutter / SwiftPM |
| Windows | 10 | D3D11VA / software + D3D11 | WASAPI | x64 / ARM64; Flutter |
| Android | 8 / API 26 | MediaCodec / software + wgpu | AAudio | Four ABIs; Flutter |
| OpenHarmony | API 18 | AVCodec / software + wgpu | OHAudio | arm64; Flutter / ArkTS OHPM |
| Linux (experimental) | glibc 2.35; system mode also needs FFmpeg 8 shared libraries | NVDEC / VA-API / software + wgpu | PulseAudio / pipewire-pulse | x64 / ARM64 × bundled / system; Rust / C ABI / Flutter |
| Browser | HTTPS / localhost; WebCodecs video and audio, OffscreenCanvas, WebGL2, Web Audio | WebCodecs + WebGL2 / Canvas | Web Audio | Prebuilt Release archive or JS / WASM source build; not published on npm |

Native platforms provide a C ABI Presenter; browsers use the JavaScript / TypeScript `Player`. Hardware decode depends on the codec, device and driver; browser codec configuration support is checked at runtime.

## Browser playback

FFmpeg WASM demuxes the container, libass renders MKV-embedded ASS/SSA with attached fonts, and the Rust DFM engine renders danmaku. Subtitles and danmaku share the playback clock and WebGL2 compositor and can each use 2× supersampling. External ASS/SSA, SRT and WebVTT subtitles are supported. Servers provide the original file with HTTP Range.

Output is SDR and WebCodecs decodes audio and video. HDR/EDR, Dolby Vision, ArtCNN, bitmap subtitles and conversion of embedded non-ASS subtitles have not yet been ported. See the [browser guide](web.md) for requirements, limitations, build and deployment.

## Video surfaces and HDR

| Platform | Native presentation | Flutter texture and composition |
|---|---|---|
| Apple | CAMetalLayer with EDR | macOS IOSurface / Metal texture for Flutter clipping and filters |
| Windows | HWND / D3D11 with HDR10; DirectComposition for transparent video | Shared D3D11 SDR texture; overlay blend switches to the native layer |
| Android | SDR TextureView; scRGB uses SurfaceView, Vulkan and FP16 / data-space negotiation | TextureView compatibility path is SDR |
| OpenHarmony | OHNativeWindow, wgpu / AVCodec; OHNativeBuffer import | Flutter external texture |
| Linux | X11 / Wayland; native video layer on Wayland | RGBA SDR texture requires readback; native layer sits below transparent Flutter UI |

Android scRGB and Linux HDR require a surface that supports the respective encoding. Query `erika_presenter_get_output_status` for active output, headroom and fallback reason.

Linux VA-API → Vulkan direct import and the WSL GPU-copy path are experimental. Texture composition, hardware decode and zero-copy decoded frames are distinct paths; see the [Linux guide](linux.md) for selection.

## Fallback font

Subtitles and danmaku try the system's fonts for text the requested font lacks, and
use the embedded Droid Sans Fallback when those cannot be read.

| Platform | System fonts |
|---|---|
| macOS, Windows, Linux | Read directly |
| Android | Chosen from the language list in `fonts.xml` |
| OpenHarmony | Read directly |
| iOS, tvOS | Embedded font. From iOS 18 on, and on tvOS, the system's Chinese font `PingFangUI.ttc` keeps its outlines in the `hvgl` table, which FreeType cannot read |
| Browser | Out of reach: the font in the wasm, or one passed as `fallbackFont` |

Without `embedded-fallback-font` there is no embedded font: Chinese does not show on
iOS and tvOS, and a browser page must be given a font.

## Colour emoji

Danmaku draw emoji in the font's own colours. Subtitles draw them in one colour,
since libass only produces alpha bitmaps.

| Platform | Emoji font |
|---|---|
| macOS, iOS, tvOS | Apple Color Emoji |
| Windows | Segoe UI Emoji |
| Linux | A colour font on the system, such as Noto Color Emoji; monochrome without one |
| Android | Noto Color Emoji, with flags in `NotoColorEmojiFlags` |
| OpenHarmony | The system's colour font |
| Browser | The browser's own fonts |

## CI and validation

Native builds cover seven prebuilt platforms. The Linux workflow builds both modes on both native architectures, using Xvfb, Mesa and PulseAudio for source and extracted-release playback checks, plus clean Ubuntu 22.04 runtime/ABI validation. The Flutter consumer workflow still checks its original six platforms; Linux uses `ERIKA_LIBRARY_DIR` for manual integration. Device experiments for HDR output and platform lifecycle are in the [validation archive](investigations/README.md).

The Web workflow builds the complete WASM package and runs standalone player regressions. Erika Web was validated on macOS with Chrome 155.0.8059.85 using H.264/AAC/ASS and attached-font MKV, VP9/Opus WebM, 2,000 danmaku, live settings, chapters, screenshots and player lifecycle; Safari and Firefox remain unvalidated.
