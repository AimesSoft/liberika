#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: prepare_apple_prebuilt.sh <ios|tvos|macos> <sdk> <arch> <output>" >&2
  exit 2
fi

PLATFORM="$1"
SDK_NAME="$2"
ARCH="$3"
OUTPUT="$4"
SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
PACKAGE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
ARTIFACT_MANIFEST="$PACKAGE_ROOT/native_artifacts.properties"
if [ ! -f "$ARTIFACT_MANIFEST" ]; then
  echo "error: Erika native artifact manifest is missing: $ARTIFACT_MANIFEST" >&2
  exit 1
fi
# The manifest is maintained in this package and contains simple KEY=VALUE pairs.
# shellcheck disable=SC1090
. "$ARTIFACT_MANIFEST"

DEFAULT_TAG="v$ERIKA_NATIVE_VERSION"
case "$PLATFORM:$ARCH" in
  ios:*) ASSET="erika-capi-ios"; DEFAULT_SHA256="$ERIKA_IOS_SHA256" ;;
  tvos:*) ASSET="erika-capi-tvos"; DEFAULT_SHA256="$ERIKA_TVOS_SHA256" ;;
  macos:arm64) ASSET="erika-capi-macos-arm64"; DEFAULT_SHA256="$ERIKA_MACOS_ARM64_SHA256" ;;
  macos:x64) ASSET="erika-capi-macos-x64"; DEFAULT_SHA256="$ERIKA_MACOS_X64_SHA256" ;;
  macos:universal) ASSET="erika-capi-macos-universal"; DEFAULT_SHA256="$ERIKA_MACOS_UNIVERSAL_SHA256" ;;
  *)
    echo "error: unsupported Erika Apple prebuilt selection: $PLATFORM/$ARCH" >&2
    exit 1
    ;;
esac

PREBUILT_TAG="${ERIKA_PREBUILT_TAG:-$DEFAULT_TAG}"
if [ -n "${ERIKA_PREBUILT_SHA256:-}" ]; then
  PREBUILT_SHA256="$ERIKA_PREBUILT_SHA256"
elif [ "$PREBUILT_TAG" = "$DEFAULT_TAG" ]; then
  PREBUILT_SHA256="$DEFAULT_SHA256"
else
  echo "error: ERIKA_PREBUILT_SHA256 is required when ERIKA_PREBUILT_TAG overrides $DEFAULT_TAG" >&2
  exit 1
fi

CACHE_BASE="${ERIKA_PREBUILT_CACHE_DIR:-${TMPDIR:-/tmp}/erika-prebuilt-cache}"
CACHE_KEY="$(printf '%s' "$PREBUILT_TAG" | tr -c 'A-Za-z0-9._-' '_')"
WORK="$CACHE_BASE/$CACHE_KEY/$ASSET"
ZIP="$WORK/$ASSET.zip"
UNPACKED="$WORK/unpacked"
URL="https://github.com/AimesSoft/liberika/releases/download/$PREBUILT_TAG/$ASSET.zip"
mkdir -p "$WORK"

verify_sha256() {
  [ -f "$1" ] || return 1
  ACTUAL_SHA256="$(shasum -a 256 "$1" | awk '{print $1}')"
  [ "$ACTUAL_SHA256" = "$PREBUILT_SHA256" ]
}

if ! verify_sha256 "$ZIP"; then
  rm -f "$ZIP"
  PART="$ZIP.part.$$"
  rm -f "$PART"
  echo "Erika: downloading verified prebuilt $URL"
  if ! curl -fSL --retry 3 --connect-timeout 30 -o "$PART" "$URL"; then
    rm -f "$PART"
    echo "error: failed to download Erika prebuilt $PREBUILT_TAG" >&2
    exit 1
  fi
  if ! verify_sha256 "$PART"; then
    rm -f "$PART"
    echo "error: Erika prebuilt checksum mismatch for $ASSET.zip" >&2
    exit 1
  fi
  mv "$PART" "$ZIP"
fi

MARKER="$UNPACKED/.erika-sha256"
if [ ! -f "$MARKER" ] || [ "$(sed -n '1p' "$MARKER")" != "$PREBUILT_SHA256" ]; then
  TEMP_UNPACKED="$WORK/unpacked.$$"
  rm -rf "$TEMP_UNPACKED"
  mkdir -p "$TEMP_UNPACKED"
  if ! unzip -oq "$ZIP" -d "$TEMP_UNPACKED"; then
    rm -rf "$TEMP_UNPACKED"
    echo "error: failed to extract Erika prebuilt $PREBUILT_TAG" >&2
    exit 1
  fi
  printf '%s\n' "$PREBUILT_SHA256" > "$TEMP_UNPACKED/.erika-sha256"
  rm -rf "$UNPACKED"
  mv "$TEMP_UNPACKED" "$UNPACKED"
fi

case "$PLATFORM:$SDK_NAME" in
  macos:*)
    SOURCE="$(find "$UNPACKED" -type f -name 'liberika_capi.dylib' -print -quit)"
    ;;
  ios:iphonesimulator)
    XCF="$(find "$UNPACKED" -type d -name 'erika_capi.xcframework' -print -quit)"
    SLICE="$(find "$XCF" -maxdepth 1 -type d -name '*simulator*' -print -quit)"
    SOURCE="$(find "$SLICE" -maxdepth 1 -type f -name '*.a' -print -quit)"
    ;;
  ios:*)
    XCF="$(find "$UNPACKED" -type d -name 'erika_capi.xcframework' -print -quit)"
    SLICE="$(find "$XCF" -maxdepth 1 -type d -name 'ios-*' ! -name '*simulator*' -print -quit)"
    SOURCE="$(find "$SLICE" -maxdepth 1 -type f -name '*.a' -print -quit)"
    ;;
  tvos:appletvsimulator)
    XCF="$(find "$UNPACKED" -type d -name 'erika_capi.xcframework' -print -quit)"
    SLICE="$(find "$XCF" -maxdepth 1 -type d -name '*simulator*' -print -quit)"
    SOURCE="$(find "$SLICE" -maxdepth 1 -type f -name '*.a' -print -quit)"
    ;;
  tvos:*)
    XCF="$(find "$UNPACKED" -type d -name 'erika_capi.xcframework' -print -quit)"
    SLICE="$(find "$XCF" -maxdepth 1 -type d -name 'tvos-*' ! -name '*simulator*' -print -quit)"
    SOURCE="$(find "$SLICE" -maxdepth 1 -type f -name '*.a' -print -quit)"
    ;;
esac

if [ -z "${SOURCE:-}" ] || [ ! -f "$SOURCE" ]; then
  echo "error: Erika prebuilt $PREBUILT_TAG is missing the $PLATFORM/$SDK_NAME runtime" >&2
  exit 1
fi
mkdir -p "$(dirname "$OUTPUT")"
cp "$SOURCE" "$OUTPUT"
echo "Erika: using verified prebuilt $PREBUILT_TAG -> $OUTPUT"
