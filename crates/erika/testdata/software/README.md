# Software playback fixture

`wmv2-wmav2.asf` is two seconds of original synthetic video and audio,
distributed under the repository's MPL-2.0 license. It exercises issue #120:
WMV2 has no supported native hardware decoder route, so every hardware
preference must fall back to software. The regression checks video plane
conversion, WMA audio, seeking, and stop/replay without invoking FFmpeg at
test runtime.

Generated with FFmpeg 7.1.1:

```sh
ffmpeg -hide_banner -loglevel error -nostdin \
  -f lavfi -i testsrc2=size=160x90:rate=10:duration=2 \
  -f lavfi -i sine=frequency=440:sample_rate=44100:duration=2 \
  -map_metadata -1 -fflags +bitexact -flags:v +bitexact -flags:a +bitexact \
  -threads:v 1 -threads:a 1 -c:v wmv2 -b:v 80k -g 10 \
  -c:a wmav2 -b:a 32k -y wmv2-wmav2.asf
```

Native renderer smoke checks:

```sh
cargo run -p macos_native_demo -- --smoke-seconds 3 crates/erika/testdata/software/wmv2-wmav2.asf
cargo run -p wgpu_decode_png -- crates/erika/testdata/software/wmv2-wmav2.asf /tmp/wmv.png
```

Windows CI exercises D3D11 software uploads on a WARP device, so that this
path is tested independently of hardware video decoder availability.

`av1-8bit.ivf` and `av1-10bit.ivf` each contain two synthetic 64×48 AV1
frames. The software AV1 regression decodes both files through the bundled
`libdav1d` on every platform and checks NV12/P010 conversion and EOF. This
detects builds that register FFmpeg's hardware-only `av1` decoder but omit
the actual CPU decoder.

Generate with FFmpeg 7.1.1 and its libaom encoder, using `yuv420p` for the
8-bit fixture and `yuv420p10le` for the 10-bit fixture:

```sh
ffmpeg -hide_banner -loglevel error -nostdin \
  -f lavfi -i testsrc2=size=64x48:rate=2:duration=1 \
  -pix_fmt yuv420p10le -c:v libaom-av1 -cpu-used 8 \
  -threads 1 -row-mt 0 -crf 40 -b:v 0 \
  -fflags +bitexact -flags:v +bitexact -y av1-10bit.ivf
```

`av1-subnit.ivf` contains one synthetic 10-bit BT.2020/PQ frame with a
0.5 nit mastering peak. SVT-AV1 encodes the minimum luminance as 2/16384
nit. The software decoder exposes both values to the HDR pipeline.

```sh
ffmpeg -f lavfi -i 'color=c=gray:s=64x64:r=1,format=yuv420p10le,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc' \
  -frames:v 1 -c:v libsvtav1 -preset 12 \
  -svtav1-params 'lp=1:color-primaries=9:transfer-characteristics=16:matrix-coefficients=9:mastering-display=G(0.170,0.797)B(0.131,0.046)R(0.708,0.292)WP(0.3127,0.3290)L(0.50,0.0001)' \
  -f ivf av1-subnit.ivf
```
