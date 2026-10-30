# Issue #180: OpenHarmony text subtitles become rectangles

Investigation date: 2026-10-30. Base: `cc297a7`.
[Issue](https://github.com/AimesSoft/liberika/issues/180).

## Cause and released artifact

The OpenHarmony CI, release job and Flutter source-build command used
`--no-default-features --features wgpu`. This explicitly disables
`erika_capi/libass`, which is otherwise a default feature. Their native
`xtask deps build` commands also omitted `--all`, excluding the text dependency
stack. This affects the runtime shared by Flutter and ArkTS.

Without libass, `SubtitleFrameState::append_text_subtitles` calls
`append_text_subtitles_debug`. That uses `SubtitleTimeline::render_debug_frame`,
whose planes contain `[255, 255, 255, 220]` for every pixel. It creates one solid
rectangle per cue instead of rasterizing text. Danmaku uses another renderer
and is unaffected. This explains the solid block in the issue screenshot;
changing the subtitle font does not enable the missing renderer.

The latest published native release checked was **v0.3.0**. Its
`liberika_capi.so` contains both regular and dynamic ELF symbol tables and the
presenter entry points, but **no definitions** of `ass_library_init`,
`ass_process_chunk` or `ass_render_frame`. The [artifact record](release-0.3.0.json)
contains the download URL, sizes and SHA256 hashes. The library is not merely
stripped of its local symbol table.

## Fix

- Build the OpenHarmony text dependency stack with `xtask deps build --all`.
- Enable `libass,wgpu` in native CI, release builds and Flutter source builds.
- Check the linked libass implementation in the actual ARM64 runtime before
  accepting or packaging it; checking only C exports cannot detect this bug.
- Use a new complete-dependency cache key, with the old partial cache as a
  fallback so existing FFmpeg builds can be reused and text dependencies added.
- Add a Presenter regression using Matroska-style CodecPrivate plus an ASS
  event chunk with Chinese/Japanese text and an unavailable requested font.
  It checks for visible, nonuniform glyph alpha masks and no debug RGBA planes.

The prebuilt v0.3.0 release is unchanged. A corrected native release and rebuilt
NipaPlay app are needed to deliver the fix to existing users. A source consumer
can instead rebuild this checkout with `ERIKA_FORCE_SOURCE_BUILD=1`.

## Validation

macOS host: core **812 passed, 1 ignored**; C API **54 passed**. The new CJK
streaming ASS regression passed and selected the bundled Droid Sans Fallback
font despite the unavailable script font. All five API/header alignment tests,
including the optional C++ bridge test, passed. Formatting and diff checks
passed.

OpenHarmony ARM64 compilation and linked-renderer checks are part of the
updated PR workflow. Device playback of the reporter's MKV is not claimed:
the original media and exact NipaPlay build were not supplied. The local host
regression validates subtitle generation rather than an OpenHarmony display
or a complete NipaPlay application.

To reproduce the new text regression:

```sh
cargo test -p erika --lib \
  embedded_cjk_ass_rasterizes_glyphs_instead_of_debug_rectangles -- --nocapture
```

To check the released runtime using the OHOS SDK's `llvm-nm` (or an ELF-capable
GNU nm), inspect all defined symbols, not only dynamic C exports:

```sh
llvm-nm --defined-only liberika_capi.so | \
  awk '{print $NF}' | grep -E '^ass_(library_init|process_chunk|render_frame)$'
```

This finds no symbols in the v0.3.0 OpenHarmony runtime. The fixed CI/release
jobs require all three symbols in the newly built library.
