#!/usr/bin/env bash
# Run after the Linux build dependencies and patched libass are installed.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ -e /dev/dxg ]] || { echo "WSL GPU interface missing" >&2; exit 1; }
export GALLIUM_DRIVER=d3d12 WGPU_BACKEND=gl ERIKA_REQUIRE_HARDWARE_GPU=1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/erika-target}"
mkdir -p "$CARGO_TARGET_DIR"
CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"
export CARGO_TARGET_DIR
cargo build --locked -j "${ERIKA_BUILD_JOBS:-4}" -p linux_native_demo -p erika_capi
cargo test --locked -p erika --features wgpu --lib renderer::wgpu::tests:: -- --test-threads=1
cargo test --locked -p erika --features wgpu --lib pulse_audio_pause_resume_flush_and_reconfigure -- --ignored
FIXTURE=crates/erika/testdata/playback/playback-fixture.mkv
for protocol in x11 wayland; do
  "$CARGO_TARGET_DIR/debug/linux_native_demo" "--$protocol" --smoke-seconds 10 \
    --require-audio --min-media-seconds 7.9 "$FIXTURE"
done
cc -std=c11 -Wall -Wextra -Werror examples/linux_native_demo/capi_smoke.c \
  -I crates/erika_capi/include -L "$CARGO_TARGET_DIR/debug" \
  -Wl,-rpath,"$CARGO_TARGET_DIR/debug" -lerika_capi -lX11 \
  -o "$CARGO_TARGET_DIR/debug/linux_capi_smoke"
"$CARGO_TARGET_DIR/debug/linux_capi_smoke" "$FIXTURE"
