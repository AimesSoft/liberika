#!/usr/bin/env bash
# Check an EXTRACTED release bundle on a clean runtime image, under Xvfb/Pulse.
set -euo pipefail
cd "$(dirname "$0")/.."
BUNDLE="$(cd "${1:?bundle directory}" && pwd)"
ARCH="${2:?x64 or arm64}"
MODE="${3:?bundled or system}"
export LD_LIBRARY_PATH="$BUNDLE/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
python3 packaging/verify_linux.py "$BUNDLE/lib/liberika_capi.so" \
  --archive "$BUNDLE/lib/liberika_capi.a" --header "$BUNDLE/include/erika.h" \
  --arch "$ARCH" --mode "$MODE"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cc -std=c11 -Wall -Wextra -Werror examples/linux_native_demo/capi_smoke.c \
  -I "$BUNDLE/include" -L "$BUNDLE/lib" -lerika_capi -lX11 -o "$work/smoke"
"$work/smoke" crates/erika/testdata/playback/playback-fixture.mkv
cc -std=c11 -Wall -Wextra -Werror examples/linux_native_demo/strict_frames.c \
  -I "$BUNDLE/include" -L "$BUNDLE/lib" -lerika_capi -o "$work/strict"
"$work/strict" gpu crates/erika/testdata/playback/playback-fixture.mkv
"$work/strict" zero crates/erika/testdata/playback/playback-fixture.mkv
