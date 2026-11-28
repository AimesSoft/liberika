#!/usr/bin/env bash
# Source this file, then call erika_prepare_linux_deps. Used by CI and packaging.
set -euo pipefail
ERIKA_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ERIKA_LINUX_DEPENDENCIES="${ERIKA_LINUX_DEPENDENCIES:-bundled}"
case "$ERIKA_LINUX_DEPENDENCIES" in
  bundled) export ERIKA_USE_SYSTEM_LIBS=0 ;;
  system) export ERIKA_USE_SYSTEM_LIBS=1 ;;
  *) echo "ERIKA_LINUX_DEPENDENCIES must be bundled or system." >&2; exit 1 ;;
esac
host="$(rustc -vV | sed -n 's/^host: //p')"
case "$host" in
  x86_64-unknown-linux-gnu) ERIKA_LINUX_ARCH=x64 ;;
  aarch64-unknown-linux-gnu) ERIKA_LINUX_ARCH=arm64 ;;
  *) echo "A native x86_64 or aarch64 GNU/Linux toolchain is required." >&2; exit 1 ;;
esac
if [[ "${ERIKA_NATIVE_TARGET:-$host}" != "$host" || "${CARGO_BUILD_TARGET:-$host}" != "$host" ]]; then
  echo "Linux release target must match the native Rust host ($host)." >&2
  exit 1
fi
# Dependency overrides would make the archive's mode/version claims untrue.
for variable in ERIKA_FFMPEG_DIR ERIKA_FFMPEG_SET ERIKA_LIBASS_DIR ERIKA_DAV1D_DIR \
  ERIKA_ZLIB_DIR ERIKA_FREETYPE_DIR ERIKA_HARFBUZZ_DIR ERIKA_FRIBIDI_DIR; do
  if [[ -n "${!variable:-}" ]]; then
    echo "Unset $variable before using the reproducible Linux build entry point." >&2
    exit 1
  fi
done
export ERIKA_NATIVE_TARGET="$host" CARGO_BUILD_TARGET="$host" ERIKA_NATIVE_PROFILE=lgpl
export ERIKA_LINUX_DEPENDENCIES ERIKA_LINUX_ARCH
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ERIKA_REPO_ROOT/target/linux-$ERIKA_LINUX_DEPENDENCIES}"
mkdir -p "$CARGO_TARGET_DIR"
CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"
export ERIKA_LINUX_HEADERS="$ERIKA_REPO_ROOT/third_party/dist/$host/linux-build-headers"
export ERIKA_LINUX_FFMPEG="$ERIKA_REPO_ROOT/third_party/dist/$host/lgpl/ffmpeg"
export PKG_CONFIG_PATH="$ERIKA_LINUX_HEADERS/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
if [[ "$ERIKA_LINUX_DEPENDENCIES" == system ]]; then
  # FFmpeg 8 is a build/test SDK, not part of the system-mode release archive.
  # No rpath to this checkout is embedded in the resulting C API library.
  export PKG_CONFIG_PATH="$ERIKA_LINUX_FFMPEG/lib/pkgconfig:$PKG_CONFIG_PATH"
  export LD_LIBRARY_PATH="$ERIKA_LINUX_FFMPEG/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi

erika_prepare_linux_deps() {
  bash "$ERIKA_REPO_ROOT/scripts/prepare_linux_headers.sh"
  local args=()
  if [[ "$ERIKA_LINUX_DEPENDENCIES" == bundled ]]; then args+=(--all); fi
  cargo run --locked --manifest-path "$ERIKA_REPO_ROOT/Cargo.toml" -p xtask -- \
    deps build --profile lgpl --target "$ERIKA_NATIVE_TARGET" \
    --jobs "${ERIKA_BUILD_JOBS:-4}" "${args[@]}"
  if [[ "$ERIKA_LINUX_DEPENDENCIES" == system ]]; then
    bash "$ERIKA_REPO_ROOT/scripts/build_linux_libass.sh"
  fi
  # Disabling autodetection must never silently remove a Linux hardware path.
  local config="$ERIKA_REPO_ROOT/third_party/build/$ERIKA_NATIVE_TARGET/lgpl/ffmpeg/config.h"
  for feature in VAAPI LIBDRM VULKAN FFNVCODEC CUDA NVDEC CUVID LIBDAV1D; do
    grep -q "^#define CONFIG_${feature} 1$" "$config"
  done
}
