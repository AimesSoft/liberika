#!/usr/bin/env bash
# Real hardware integration check. Intel/AMD hosts can set ERIKA_HWDEC=vaapi.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$HOME/.cache/erika-target}
export ERIKA_REQUIRE_HARDWARE_DECODE=1 ERIKA_REQUIRE_HARDWARE_GPU=1
if [[ -e /dev/dxg ]]; then
  export GALLIUM_DRIVER=${GALLIUM_DRIVER:-d3d12} WGPU_BACKEND=${WGPU_BACKEND:-gl}
fi
mkdir -p "$CARGO_TARGET_DIR"
if [[ $# == 0 ]]; then
  ffmpeg -y -v error -f lavfi -i testsrc2=s=1280x720:r=30 \
    -f lavfi -i sine=frequency=440:sample_rate=48000 -t 8 -c:v libx264 \
    -preset fast -pix_fmt yuv420p -c:a aac "$CARGO_TARGET_DIR/hardware-test.mp4"
  set -- "$CARGO_TARGET_DIR/hardware-test.mp4"
fi
cargo build --locked -p erika_capi -p linux_native_demo
cc -D_DEFAULT_SOURCE -std=c11 -Wall -Wextra -Werror \
  examples/linux_native_demo/capi_texture_smoke.c -I crates/erika_capi/include \
  -L "$CARGO_TARGET_DIR/debug" -Wl,-rpath,"$CARGO_TARGET_DIR/debug" \
  -lerika_capi -o "$CARGO_TARGET_DIR/capi_texture_smoke"
for video in "$@"; do
  "$CARGO_TARGET_DIR/debug/linux_native_demo" --smoke-seconds 10 \
    --require-audio --min-media-seconds 7.8 "$video"
  ERIKA_TEST_LONG_PAUSE=1 "$CARGO_TARGET_DIR/capi_texture_smoke" "$video"
done
