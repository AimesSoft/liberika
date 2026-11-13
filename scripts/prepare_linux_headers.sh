#!/usr/bin/env bash
# Build-only headers for FFmpeg 8 on Ubuntu 22.04. Never replace system loaders.
# Run from a native Linux toolchain; installs only in third_party/dist.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$(rustc -vV | sed -n 's/^host: //p')"
case "$TARGET" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) ;;
  *) echo "A native x86_64 or aarch64 GNU/Linux toolchain is required." >&2; exit 1 ;;
esac
PREFIX="$ROOT/third_party/dist/$TARGET/linux-build-headers"
VULKAN_VERSION=1.4.341
VULKAN_SHA=8876aad926b2e72bdefddc34885472d5332e2d20aa3ed57313466f0bc982cf3f
NVCODEC_VERSION=12.1.14.0
NVCODEC_SHA=2fefaa227d2a3b4170797796425a59d1dd2ed5fd231db9b4244468ba327acd0b
DIRECTX_VERSION=1.614.1
DIRECTX_SHA=344eb454c979ea68d8255d82c818bf7daf01f5109d26ac104f9911d18fae3b21
mkdir -p "$ROOT/third_party/cache" "$PREFIX/include" "$PREFIX/lib/pkgconfig" "$PREFIX/licenses"

fetch_source() {
  local name="$1" url="$2" digest="$3" work
  # Each download/extraction has its own directory; never run tools from it.
  work="$(mktemp -d "$ROOT/third_party/cache/$name.XXXXXX")"
  curl --fail --location --retry 3 "$url" --output "$work/source.tar.gz"
  printf '%s  %s\n' "$digest" "$work/source.tar.gz" | sha256sum --check --status
  mkdir "$work/source"
  tar -xzf "$work/source.tar.gz" -C "$work/source" --strip-components=1
  printf '%s\n' "$work/source"
}

if [[ ! -f "$PREFIX/vulkan-$VULKAN_SHA" ]]; then
  source_dir="$(fetch_source vulkan-headers \
    "https://codeload.github.com/KhronosGroup/Vulkan-Headers/tar.gz/refs/tags/v$VULKAN_VERSION" "$VULKAN_SHA")"
  cp -R "$source_dir/include/vulkan" "$source_dir/include/vk_video" "$PREFIX/include/"
  mkdir -p "$PREFIX/licenses/Vulkan-Headers"
  cp "$source_dir/LICENSE.md" "$PREFIX/licenses/Vulkan-Headers/"
  cp -R "$source_dir/LICENSES" "$PREFIX/licenses/Vulkan-Headers/"
  touch "$PREFIX/vulkan-$VULKAN_SHA"
fi
if [[ ! -f "$PREFIX/nvcodec-$NVCODEC_SHA" ]]; then
  source_dir="$(fetch_source nv-codec-headers \
    "https://codeload.github.com/FFmpeg/nv-codec-headers/tar.gz/refs/tags/n$NVCODEC_VERSION" "$NVCODEC_SHA")"
  cp -R "$source_dir/include/ffnvcodec" "$PREFIX/include/"
  # Upstream carries the complete MIT/NVIDIA notices in the headers themselves.
  mkdir -p "$PREFIX/licenses/nv-codec-headers"
  cp "$source_dir/include/ffnvcodec/"*.h "$PREFIX/licenses/nv-codec-headers/"
  touch "$PREFIX/nvcodec-$NVCODEC_SHA"
fi
# A local pkg-config overlay pairs newer declarations with the distro loader.
cat > "$PREFIX/lib/pkgconfig/vulkan.pc" <<EOF
prefix=$PREFIX
includedir=\${prefix}/include
Name: Vulkan
Description: Build-only Vulkan headers with the system Vulkan loader
Version: $VULKAN_VERSION
Libs: -lvulkan
Cflags: -I\${includedir}
EOF
cat > "$PREFIX/lib/pkgconfig/ffnvcodec.pc" <<EOF
prefix=$PREFIX
includedir=\${prefix}/include
Name: ffnvcodec
Description: FFmpeg NVIDIA headers (no driver included)
Version: $NVCODEC_VERSION
Cflags: -I\${includedir}
EOF

# Optional WSL bridge coverage uses the same old libc baseline. These static
# GUID/header helpers do not ship a Windows driver or replace a system library.
if [[ "${ERIKA_LINUX_D3D12_INTEROP:-0}" == 1 && ! -f "$PREFIX/directx-$DIRECTX_SHA" ]]; then
  source_dir="$(fetch_source directx-headers \
    "https://codeload.github.com/microsoft/DirectX-Headers/tar.gz/refs/tags/v$DIRECTX_VERSION" "$DIRECTX_SHA")"
  mkdir -p "$ROOT/third_party/build/$TARGET"
  build_dir="$(mktemp -d "$ROOT/third_party/build/$TARGET/directx-headers.XXXXXX")"
  cmake -S "$source_dir" -B "$build_dir" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$PREFIX" \
    -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DBUILD_TESTING=OFF -DDXHEADERS_BUILD_TEST=OFF
  cmake --build "$build_dir" --parallel "${ERIKA_BUILD_JOBS:-4}"
  cmake --install "$build_dir"
  cp "$source_dir/LICENSE" "$PREFIX/licenses/LICENSE.DirectX-Headers"
  touch "$PREFIX/directx-$DIRECTX_SHA"
fi
# Upstream DirectX-Headers.pc omits the directx and wsl/stubs include paths
# that Debian's package adds; ensure pkg-config exposes both for Linux/WSL.
pc_file="$PREFIX/lib/pkgconfig/DirectX-Headers.pc"
if [[ "${ERIKA_LINUX_D3D12_INTEROP:-0}" == 1 && -f "$pc_file" ]] && ! grep -q '/wsl/stubs' "$pc_file"; then
  python3 -I - "$pc_file" <<'PY'
from pathlib import Path
import sys

pc = Path(sys.argv[1])
updated = []
for line in pc.read_text().splitlines():
    if line.startswith("Cflags:"):
        parts = line.split()
        inc = next((p[2:] for p in parts[1:] if p.startswith("-I")), None)
        if inc:
            for suffix in ("/directx", "/wsl/stubs"):
                flag = f"-I{inc}{suffix}"
                if flag not in parts:
                    parts.append(flag)
            line = " ".join(parts)
    updated.append(line)
pc.write_text("\n".join(updated) + "\n")
PY
fi
printf 'Linux build headers ready: %s\n' "$PREFIX"
