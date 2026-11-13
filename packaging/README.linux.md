# Linux C ABI bundle

Choose the archive matching your CPU (`x64` or `arm64`) and dependency mode.
Both release variants are built on Ubuntu 22.04, with a glibc 2.35 baseline
and libstdc++ symbol requirements no newer than GLIBCXX_3.4.30. They are not
musl/Alpine binaries. See `MANIFEST.txt` for the mode and `DEPENDENCIES.txt`
for the actual direct shared-library and symbol-version requirements.

## bundled

FFmpeg 8.1.2, dav1d, zlib, patched libass, FreeType, HarfBuzz and FriBidi
are statically embedded. You do not need FFmpeg shared libraries. This is
**not a fully static Linux binary**: system libc/libstdc++, PulseAudio,
Fontconfig, VA-API/DRM and Vulkan loader/driver interfaces remain external.
Fontconfig can itself load the distribution's FreeType; that does not replace
the embedded subtitle rasterizer. Drivers and system libraries are not shipped.

Typical Ubuntu 22.04 runtime packages:

```sh
sudo apt-get install libstdc++6 libpulse0 libfontconfig1 libva2 libva-drm2 libdrm2 libvulkan1
```

Install the appropriate Mesa or vendor graphics drivers, fonts and your
host application's X11/Wayland dependencies as well. PipeWire installations
can supply PulseAudio compatibility. Hardware acceleration still depends on
the GPU/driver; compiling a backend does not certify a real device.

## system

FFmpeg and the font stack are shared system dependencies; **patched libass
and SoundTouch remain embedded**. Supply FFmpeg 8 shared libraries, not just
an `ffmpeg` command or a static-only build. Required ABI majors:
`libavdevice.so.62`, `libavfilter.so.11`, `libavformat.so.62`,
`libavcodec.so.62`, `libswresample.so.6`, `libswscale.so.9`, `libavutil.so.60`.
Unused components may be removed from direct dependencies by the linker.
FreeType, HarfBuzz, FriBidi and the desktop runtime libraries above are also
required. The FFmpeg build/test SDK is **not included** in this archive.

Ubuntu 22.04/24.04's default FFmpeg packages are too old. You can install or
build an ABI-compatible FFmpeg 8 yourself, using `--enable-shared`, then
configure the loader with `ldconfig` or `LD_LIBRARY_PATH`. Build that FFmpeg
for your own system's libc baseline. FFmpeg license and codec/hardware support
depend on its build configuration; this package does not designate an
externally supplied FFmpeg as LGPL. Never rename an older SONAME to masquerade
as FFmpeg 8.

## Integration

Use `include/erika.h` together with this exact `lib/liberika_capi.so`.
Set your application's loader path (or install the library), for example:

```sh
export LD_LIBRARY_PATH="/absolute/path/to/bundle/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
```

The Flutter Linux bridge accepts `ERIKA_LIBRARY_DIR=/absolute/path/to/bundle/lib`;
it does not automatically download this archive. For static linking,
`liberika_capi.a` still needs the system/driver libraries (and FFmpeg/font shared
libraries in system mode). Use the source build's linker diagnostics for your
host; the `.a` file is not a promise of a fully static executable.

See `THIRD_PARTY_NOTICES.md` and `licenses/` for applicable licenses and LGPL
relink instructions. Linux playback/HDR/zero-copy maturity and real-device
acceptance remain as documented in `docs/linux.md` at the manifest commit.
