#!/usr/bin/env bash
# Build liberika's patched libass against the Linux distribution's font libraries.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$(rustc -vV | sed -n 's/^host: //p')"
case "$TARGET" in
  *-unknown-linux-gnu) ;;
  *) echo "Run this script on a native GNU/Linux Rust toolchain." >&2; exit 1 ;;
esac
VERSION=0.17.5
SHA256=2dca25c0e0c837ddf00b52011b3f82cac1e4ddd3ad018227806b0c2288864acc
ARCHIVE="$ROOT/third_party/cache/libass-$VERSION.tar.xz"
SOURCE="$ROOT/third_party/src/linux-libass-$VERSION"
BUILD="$ROOT/third_party/build/$TARGET/system/libass"
PREFIX="$ROOT/third_party/dist/$TARGET/system/libass"
# Every patch of this version, in order, as `xtask` applies them elsewhere.
PATCHES=("$ROOT/third_party/patches/libass-$VERSION/"*.patch)

for tool in curl sha256sum tar patch meson ninja pkg-config; do
  command -v "$tool" >/dev/null || { echo "Required tool missing: $tool" >&2; exit 1; }
done
pkg-config --exists freetype2 harfbuzz fribidi fontconfig
mkdir -p "$(dirname "$ARCHIVE")" "$SOURCE" "$(dirname "$BUILD")"
if [[ ! -f "$ARCHIVE" ]]; then
  curl --fail --location --retry 3 \
    "https://github.com/libass/libass/releases/download/$VERSION/libass-$VERSION.tar.xz" \
    --output "$ARCHIVE.download"
  mv "$ARCHIVE.download" "$ARCHIVE"
fi
printf '%s  %s\n' "$SHA256" "$ARCHIVE" | sha256sum --check --status
PATCH_HASH="$(cat "${PATCHES[@]}" | sha256sum | cut -d ' ' -f 1)"
if [[ ! -f "$SOURCE/.erika-patch" ]] || [[ "$(cat "$SOURCE/.erika-patch")" != "$PATCH_HASH" ]]; then
  rm -rf "$SOURCE"
  mkdir -p "$SOURCE"
  tar -xJf "$ARCHIVE" -C "$SOURCE" --strip-components=1
  for patch in "${PATCHES[@]}"; do
    patch --directory="$SOURCE" --strip=1 --input="$patch"
  done
  printf '%s\n' "$PATCH_HASH" > "$SOURCE/.erika-patch"
fi
SETUP=()
if [[ -f "$BUILD/build.ninja" ]]; then SETUP+=(--reconfigure); fi
meson setup "${SETUP[@]}" "$BUILD" "$SOURCE" --prefix="$PREFIX" --libdir=lib \
  --default-library=static --buildtype=release -Db_staticpic=true \
  -Dtest=disabled -Dprofile=disabled -Dasm=disabled -Dlibunibreak=disabled \
  -Dfontconfig=enabled -Dcoretext=disabled -Ddirectwrite=disabled
meson compile -C "$BUILD" -j "${ERIKA_BUILD_JOBS:-4}"
meson install -C "$BUILD"
printf 'Patched libass ready: %s\n' "$PREFIX"
