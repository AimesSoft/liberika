#!/usr/bin/env bash
# Requires an X11 display and a PulseAudio-compatible server.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
mkdir -p "$CARGO_TARGET_DIR"
CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"
export CARGO_TARGET_DIR
# Direct source checks keep using the user's installed FFmpeg 8 by default.
# CI/release callers prepare the isolated SDK through linux_build_env.sh first.
if [[ "${ERIKA_USE_SYSTEM_LIBS:-1}" != 0 ]]; then
  bash scripts/build_linux_libass.sh
fi
BUILD_DIR="$CARGO_TARGET_DIR${CARGO_BUILD_TARGET:+/$CARGO_BUILD_TARGET}/debug"
cargo fmt --all -- --check
python3 packaging/tests/test_api_alignment.py -v
cargo test --locked -j "${ERIKA_BUILD_JOBS:-4}" -p erika -p erika_capi --features wgpu --lib -- --test-threads=2
cargo test --locked -p erika --features wgpu --test wgpu_artcnn -- --test-threads=1
cargo test --locked -p erika --features wgpu --lib pulse_audio_pause_resume_flush_and_reconfigure -- --ignored
cargo build --locked -j "${ERIKA_BUILD_JOBS:-4}" -p linux_native_demo -p erika_capi
FIXTURE=crates/erika/testdata/playback/playback-fixture.mkv
"$BUILD_DIR/linux_native_demo" --x11 --smoke-seconds 5 --require-audio "$FIXTURE"
if [[ "${ERIKA_TEST_WAYLAND:-0}" == 1 ]]; then
  "$BUILD_DIR/linux_native_demo" --wayland --smoke-seconds 5 --require-audio "$FIXTURE"
fi
cc -std=c11 -Wall -Wextra -Werror examples/linux_native_demo/capi_smoke.c \
  -I crates/erika_capi/include -L "$BUILD_DIR" \
  -Wl,-rpath,"$BUILD_DIR" -lerika_capi -lX11 \
  -o "$BUILD_DIR/linux_capi_smoke"
"$BUILD_DIR/linux_capi_smoke" "$FIXTURE"
cc -std=c11 -Wall -Wextra -Werror examples/linux_native_demo/strict_frames.c \
  -I crates/erika_capi/include -L "$BUILD_DIR" \
  -Wl,-rpath,"$BUILD_DIR" -lerika_capi \
  -o "$BUILD_DIR/linux_strict_frames"
"$BUILD_DIR/linux_strict_frames" gpu "$FIXTURE"
"$BUILD_DIR/linux_strict_frames" zero "$FIXTURE"
