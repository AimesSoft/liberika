#!/usr/bin/env python3
"""Build the complete liberika browser package from the pinned native sources.

Requires emsdk 4.0.23 (or EMCC on PATH), Rust wasm32-unknown-unknown,
wasm-bindgen-cli 0.2.130, CMake, Meson, Ninja, make and pkg-config.
No globally installed SDK or native artifacts are modified.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
BUILD = ROOT / 'target/web-build'
PREFIX = BUILD / 'prefix'
OUT = ROOT / 'target/web'
SRC = ROOT / 'third_party/src'
ENV = os.environ.copy()
JOBS = str(min(os.cpu_count() or 4, 12))


def run(args, cwd=ROOT):
    print('+', ' '.join(map(str, args)), flush=True)
    subprocess.run(list(map(str, args)), cwd=cwd, env=ENV, check=True)


def meson(name, source, options):
    directory = BUILD / name
    if not (directory / 'build.ninja').exists():
        run(['meson', 'setup', directory, SRC / source, '--cross-file', BUILD / 'emscripten.ini', '--prefix', PREFIX,
             '--libdir=lib', '--default-library=static', '--buildtype=release', *options])
    run(['meson', 'compile', '-C', directory, '-j', JOBS])
    run(['meson', 'install', '-C', directory])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--skip-native', action='store_true', help='reuse previously built Web C libraries')
    args = parser.parse_args()
    sdk = Path(ENV.get('EMSDK', ROOT / 'target/web-tools/emsdk'))
    emdir = sdk / 'upstream/emscripten'
    if emdir.exists():
        ENV['PATH'] = str(emdir) + os.pathsep + ENV['PATH']
        ENV['EMSDK'] = str(sdk)
    for tool in ['emcc', 'em++', 'emcmake', 'emar', 'emranlib', 'meson', 'ninja', 'cmake', 'wasm-bindgen']:
        if not shutil.which(tool, path=ENV['PATH']):
            raise SystemExit(f'Missing {tool}. See docs/web.md for the pinned toolchain setup.')
    BUILD.mkdir(parents=True, exist_ok=True)
    OUT.mkdir(parents=True, exist_ok=True)
    PREFIX.mkdir(parents=True, exist_ok=True)
    ENV['PKG_CONFIG_LIBDIR'] = str(PREFIX / 'lib/pkgconfig')
    ENV['PKG_CONFIG_PATH'] = str(PREFIX / 'lib/pkgconfig')
    cross = "[binaries]\n" + '\n'.join(f"{key} = '{shutil.which(value, path=ENV['PATH'])}'" for key, value in
        [('c','emcc'),('cpp','em++'),('ar','emar'),('strip','emstrip'),('pkg-config','pkg-config')])
    cross += "\n[host_machine]\nsystem = 'emscripten'\ncpu_family = 'wasm32'\ncpu = 'wasm32'\nendian = 'little'\n[properties]\nneeds_exe_wrapper = true\n[built-in options]\nc_args = ['-O3']\ncpp_args = ['-O3']\n"
    (BUILD / 'emscripten.ini').write_text(cross)
    if not args.skip_native:
        run(['cargo', 'run', '-p', 'xtask', '--', 'deps', 'fetch', '--all'])
        run(['emcmake', 'cmake', '-S', SRC / 'freetype-VER-2-14-3', '-B', BUILD / 'freetype', '-G', 'Ninja',
             f'-DCMAKE_INSTALL_PREFIX={PREFIX}', '-DCMAKE_INSTALL_LIBDIR=lib', '-DCMAKE_BUILD_TYPE=Release', '-DBUILD_SHARED_LIBS=OFF',
             '-DFT_DISABLE_ZLIB=ON', '-DFT_DISABLE_BZIP2=ON', '-DFT_DISABLE_PNG=ON', '-DFT_DISABLE_HARFBUZZ=ON', '-DFT_DISABLE_BROTLI=ON'])
        run(['cmake', '--build', BUILD / 'freetype', '-j', JOBS])
        run(['cmake', '--install', BUILD / 'freetype'])
        meson('fribidi', 'fribidi-1.0.16', ['-Ddocs=false', '-Dbin=false', '-Dtests=false'])
        meson('harfbuzz', 'harfbuzz-14.2.1', [f'-D{k}=disabled' for k in ['glib','gobject','cairo','chafa','png','zlib','icu','freetype','tests','introspection','docs','utilities','subset','raster','vector','gpu','gpu_demo']])
        meson('libass', 'libass-0.17.5', [f'-D{k}=disabled' for k in ['test','compare','profile','fuzz','checkasm','fontconfig','directwrite','coretext','asm','libunibreak']] + ['-Drequire-system-font-provider=false'])
        ffbuild = BUILD / 'ffmpeg'
        ffbuild.mkdir(exist_ok=True)
        if not (ffbuild / 'config.h').exists():
            run([SRC / 'FFmpeg-n8.1.2/configure', f'--prefix={PREFIX}', '--target-os=none', '--arch=wasm32', '--enable-cross-compile',
                 '--cc=emcc', '--cxx=em++', '--ar=emar', '--ranlib=emranlib', '--nm=emnm', '--disable-everything', '--disable-autodetect',
                 '--disable-asm', '--disable-inline-asm', '--disable-x86asm', '--disable-pthreads', '--disable-network', '--disable-programs', '--disable-doc',
                 '--disable-avdevice', '--disable-avfilter', '--disable-swscale', '--disable-swresample', '--enable-small',
                 '--enable-demuxer=mov,matroska,mpegts,avi,flv,ogg,mp3,aac,wav,flac,ass,srt,webvtt',
                 '--enable-parser=h264,hevc,av1,vp9,aac,mpegaudio,opus,vorbis', '--enable-decoder=ass,ssa,subrip,webvtt', '--extra-cflags=-O3'], ffbuild)
        run(['make', '-j', JOBS], ffbuild)
        run(['make', 'install'], ffbuild)
    native = ROOT / 'crates/erika/src/web/media.c'
    exports = ['malloc','free','erika_open','erika_close','erika_info','erika_read','erika_packet_data','erika_packet_size','erika_packet_stream','erika_packet_time','erika_packet_duration','erika_packet_key','erika_seek','erika_error','erika_subtitle_select','erika_subtitle_load','erika_subtitle_render','erika_subtitle_changed','erika_image_next','erika_image_data','erika_image_width','erika_image_height','erika_image_stride','erika_image_x','erika_image_y','erika_image_color','erika_add_font']
    run(['emcc', native, '-O3', '-I'+str(PREFIX/'include'), '-L'+str(PREFIX/'lib'), '-lavformat', '-lavcodec', '-lavutil', '-lass', '-lharfbuzz', '-lfribidi', '-lfreetype', '-lc++',
         '-sMODULARIZE=1', '-sEXPORT_ES6=1', '-sENVIRONMENT=worker', '-sALLOW_MEMORY_GROWTH=1', '-sMAXIMUM_MEMORY=536870912', '-sSTACK_SIZE=1048576', '-sASYNCIFY=1',
         '-sFILESYSTEM=0', '-sEXPORTED_FUNCTIONS='+json.dumps(['_'+s for s in exports]), '-sEXPORTED_RUNTIME_METHODS=["ccall","UTF8ToString","HEAPU8"]', '-o', OUT/'media.js'])
    run(['cargo', 'build', '-p', 'erika', '--lib', '--release', '--target', 'wasm32-unknown-unknown'])
    run(['wasm-bindgen', ROOT/'target/wasm32-unknown-unknown/release/erika.wasm', '--target', 'web', '--out-dir', OUT, '--out-name', 'erika_core'])
    for file in (ROOT/'packages/erika_web').glob('*'):
        if file.is_file() and file.suffix in ['.js','.json','.ts','.md']: shutil.copy2(file, OUT/file.name)
    shutil.copy2(ROOT/'examples/web_player/index.html', OUT/'index.html')
    # Rust embeds the fallback font and supplies it to libass at runtime.
    # Retire the extra file too when rebuilding an existing output directory.
    (OUT/'subfont.ttf').unlink(missing_ok=True)
    shutil.copy2(ROOT/'LICENSE', OUT/'LICENSE')
    shutil.copy2(ROOT/'docs/web.md', OUT/'WEB-GUIDE.md')
    shutil.copy2(ROOT/'docs/web.zh.md', OUT/'WEB-GUIDE.zh.md')
    readme = (OUT/'README.md').read_text().replace('../../docs/web.md', 'WEB-GUIDE.md').replace('../../docs/web.zh.md', 'WEB-GUIDE.zh.md')
    (OUT/'README.md').write_text(readme)
    licenses = OUT/'licenses'; licenses.mkdir(exist_ok=True)
    shutil.copy2(ROOT/'crates/erika/assets/NOTICE.md', licenses/'FONT-NOTICE.md')
    shutil.copy2(ROOT/'packaging/LICENSE.Apache-2.0', licenses/'LICENSE.Apache-2.0')
    for source, names in [('FFmpeg-n8.1.2',['COPYING.LGPLv2.1']),('libass-0.17.5',['COPYING']),('harfbuzz-14.2.1',['COPYING']),('fribidi-1.0.16',['COPYING']),('freetype-VER-2-14-3',['docs/FTL.TXT','docs/GPLv2.TXT'])]:
        for name in names: shutil.copy2(SRC/source/name, licenses/(source+'-'+Path(name).name))
    manifest = {p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in OUT.glob('*') if p.is_file() and p.name != 'build-manifest.json'}
    (OUT/'build-manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    print(f'Browser package: {OUT}', flush=True)

if __name__ == '__main__': main()
