#!/usr/bin/env bash
# Native GNU/Linux: build, verify and package one dependency mode.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/linux_build_env.sh
source scripts/linux_build_env.sh
# The prepared SDK is shared by debug checks and release, with separate Cargo outputs.
erika_prepare_linux_deps
cargo build --locked -j "${ERIKA_BUILD_JOBS:-4}" -p erika_capi --release
R="$CARGO_TARGET_DIR/$ERIKA_NATIVE_TARGET/release"
NAME="erika-capi-linux-$ERIKA_LINUX_ARCH-$ERIKA_LINUX_DEPENDENCIES"
META="$CARGO_TARGET_DIR/bundle-metadata"
mkdir -p "$META/licenses/linux-build-headers"
cp packaging/README.linux.md "$META/README.md"
cp -R "$ERIKA_LINUX_HEADERS/licenses/." "$META/licenses/linux-build-headers/"
python3 packaging/verify_linux.py "$R/liberika_capi.so" \
  --archive "$R/liberika_capi.a" --header crates/erika_capi/include/erika.h \
  --arch "$ERIKA_LINUX_ARCH" --mode "$ERIKA_LINUX_DEPENDENCIES" \
  --report "$META/DEPENDENCIES.txt"
ERIKA_BUNDLE_METADATA_DIR="$META" bash packaging/bundle.sh "$NAME" "dist/$NAME.zip" \
  "$R/liberika_capi.so" "$R/liberika_capi.a"
