#!/usr/bin/env bash
# Isolated experimental drivers. Never installs into /usr or replaces system Mesa.
set -euo pipefail
SOURCE=$(realpath "${1:?usage: build_wsl_interop_mesa.sh MESA_26_0_8_SOURCE [PREFIX]}")
PREFIX=$(realpath -m "${2:-$HOME/.local/opt/erika-wsl-mesa}")
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
[[ $(cat "$SOURCE/VERSION") == 26.0.8 ]] || { echo "Mesa 26.0.8 source required" >&2; exit 1; }
PATCH="$SCRIPT_DIR/patches/mesa-26.0.8-wsl-interop.patch"
if patch -d "$SOURCE" -p1 --reverse --dry-run < "$PATCH" >/dev/null 2>&1; then
  echo "WSL interop patch already present"
else
  patch -d "$SOURCE" -p1 --forward --dry-run < "$PATCH"
  patch -d "$SOURCE" -p1 --forward < "$PATCH"
fi
BUILD="$SOURCE/build-erika-wsl"
SETUP=()
[[ ! -f "$BUILD/va/meson-private/coredata.dat" ]] || SETUP=(--reconfigure)
meson setup "${SETUP[@]}" "$BUILD/va" "$SOURCE" --prefix="$PREFIX" --libdir=lib \
  -Dbuildtype=release -Dgallium-drivers=d3d12,softpipe -Dvulkan-drivers= \
  -Dglx=xlib -Degl=disabled -Dgles1=disabled -Dgles2=disabled -Dllvm=disabled \
  -Dgallium-va=enabled -Dvideo-codecs=h264dec,h265dec,av1dec \
  -Dplatforms=x11 -Dlibunwind=disabled -Dcpp_link_args=-lxcb-xfixes
ninja -C "$BUILD/va" -j "${ERIKA_BUILD_JOBS:-4}" src/gallium/targets/va/libgallium_drv_video.so
mkdir -p "$PREFIX/lib/dri"
install -m755 "$BUILD/va/src/gallium/targets/va/libgallium_drv_video.so" "$PREFIX/lib/dri/d3d12_drv_video.so"
SETUP=()
[[ ! -f "$BUILD/dzn/meson-private/coredata.dat" ]] || SETUP=(--reconfigure)
meson setup "${SETUP[@]}" "$BUILD/dzn" "$SOURCE" --prefix="$PREFIX" --libdir=lib \
  -Dbuildtype=release -Dgallium-drivers= -Dvulkan-drivers=microsoft-experimental \
  -Dglx=disabled -Degl=disabled -Dgles1=disabled -Dgles2=disabled -Dllvm=disabled \
  -Dplatforms=x11,wayland -Dlibunwind=disabled
ninja -C "$BUILD/dzn" -j "${ERIKA_BUILD_JOBS:-4}"
meson install -C "$BUILD/dzn"
echo "Isolated drivers installed in $PREFIX"
