#!/usr/bin/env bash
#
# Stage and zip a liberika C ABI prebuilt bundle.
#
# Usage: packaging/bundle.sh <bundle-name> <out-zip> <artifact> [<artifact> ...]
#   <bundle-name>  directory name inside the zip, e.g. erika-capi-macos-universal
#   <out-zip>      output .zip path (created; parent dirs made)
#   <artifact>     one or more lib files or directories (e.g. an .xcframework)
#
# Produces a zip containing:
#   <bundle-name>/lib/<artifacts>
#   <bundle-name>/include/erika.h
#   <bundle-name>/LICENSE                 (liberika, MPL-2.0)
#   <bundle-name>/THIRD_PARTY_NOTICES.md  (FFmpeg LGPL etc.)
#   <bundle-name>/licenses/               (dependency and asset licenses/notices)
#   <bundle-name>/MANIFEST.txt           (ref/commit/profile/date)
#
# Requires `zip` on PATH (install via choco on Windows runners).
set -euo pipefail

if [ "$#" -lt 3 ]; then
  echo "usage: bundle.sh <bundle-name> <out-zip> <artifact>..." >&2
  exit 2
fi

NAME="$1"
OUT="$2"
shift 2

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

mkdir -p "$(dirname "$OUT")"
OUT="$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")"

STAGE_ROOT="$(mktemp -d)"
trap 'rm -rf "$STAGE_ROOT"' EXIT
STAGE="$STAGE_ROOT/$NAME"
mkdir -p "$STAGE/lib" "$STAGE/include" "$STAGE/licenses"

for artifact in "$@"; do
  if [ ! -e "$artifact" ]; then
    echo "bundle.sh: artifact not found: $artifact" >&2
    exit 1
  fi
  cp -R "$artifact" "$STAGE/lib/"
done

cp "$ROOT/crates/erika_capi/include/erika.h" "$STAGE/include/erika.h"
cp "$ROOT/LICENSE" "$STAGE/LICENSE"
cp "$ROOT/packaging/THIRD_PARTY_NOTICES.md" "$STAGE/THIRD_PARTY_NOTICES.md"
cp "$ROOT/packaging/LICENSE.Apache-2.0" "$STAGE/licenses/LICENSE.Apache-2.0"
cp "$ROOT/packaging/LICENSE.LGPL-2.1" "$STAGE/licenses/LICENSE.LGPL-2.1"
cp "$ROOT/packaging/LICENSE.LGPL-3.0" "$STAGE/licenses/LICENSE.LGPL-3.0"
cp "$ROOT/packaging/LICENSE.GPL-3.0" "$STAGE/licenses/LICENSE.GPL-3.0"
cp "$ROOT/packaging/LICENSE.FFmpeg.md" "$STAGE/licenses/LICENSE.FFmpeg.md"
cp "$ROOT/packaging/LICENSE.dav1d" "$STAGE/licenses/LICENSE.dav1d"
cp "$ROOT/packaging/LICENSE.libass" "$STAGE/licenses/LICENSE.libass"
cp "$ROOT/packaging/LICENSE.FreeType" "$STAGE/licenses/LICENSE.FreeType"
cp "$ROOT/packaging/LICENSE.HarfBuzz" "$STAGE/licenses/LICENSE.HarfBuzz"
cp "$ROOT/packaging/LICENSE.zlib" "$STAGE/licenses/LICENSE.zlib"
cp "$ROOT/crates/erika/assets/NOTICE.md" "$STAGE/licenses/NOTICE.Droid-Sans-Fallback.md"
cp "$ROOT/crates/erika/assets/artcnn/LICENSE.ArtCNN" "$STAGE/licenses/LICENSE.ArtCNN"
cp "$ROOT/crates/erika/assets/anime4k/LICENSE.Anime4K" "$STAGE/licenses/LICENSE.Anime4K"
cp "$ROOT/crates/erika/assets/fsr1/LICENSE.FSR1" "$STAGE/licenses/LICENSE.FSR1"

profile="${ERIKA_NATIVE_PROFILE:-lgpl}"
if [[ -n "${ERIKA_LINUX_DEPENDENCIES:-}" ]]; then
  case "$ERIKA_LINUX_DEPENDENCIES" in
    bundled) ;;
    system) profile='system (external FFmpeg license/configuration varies)' ;;
    *) echo 'Invalid Linux dependency mode' >&2; exit 1 ;;
  esac
  case "${ERIKA_LINUX_ARCH:-}" in
    x64|arm64) ;;
    *) echo 'Missing/invalid Linux architecture' >&2; exit 1 ;;
  esac
  test "$NAME" = "erika-capi-linux-$ERIKA_LINUX_ARCH-$ERIKA_LINUX_DEPENDENCIES"
  test -f "${ERIKA_BUNDLE_METADATA_DIR:?Linux bundle metadata required}/DEPENDENCIES.txt"
  cp "$ERIKA_BUNDLE_METADATA_DIR/DEPENDENCIES.txt" "$STAGE/"
  cp "$ERIKA_BUNDLE_METADATA_DIR/README.md" "$STAGE/"
  cp -R "$ERIKA_BUNDLE_METADATA_DIR/licenses/." "$STAGE/licenses/"
fi

commit="${GITHUB_SHA:-$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)}"
cat > "$STAGE/MANIFEST.txt" <<EOF
Erika C ABI prebuilt bundle
bundle:  $NAME
ref:     ${GITHUB_REF_NAME:-local}
commit:  $commit
built:   $(date -u +%Y-%m-%dT%H:%M:%SZ)
profile: $profile

Erika is MPL-2.0. See THIRD_PARTY_NOTICES.md and licenses/ for dependency
licenses and the LGPL relink note. Linux system bundles use external FFmpeg
and font libraries; other native bundles statically embed those dependencies.
Header: include/erika.h. Reference: docs/capi_reference.md.
EOF
if [[ -n "${ERIKA_LINUX_DEPENDENCIES:-}" ]]; then
  cat >> "$STAGE/MANIFEST.txt" <<EOF

platform: linux
arch: $ERIKA_LINUX_ARCH
dependencies: $ERIKA_LINUX_DEPENDENCIES
baseline: glibc 2.35 / GLIBCXX_3.4.30 / CXXABI_1.3.13
release-build-image: ubuntu:22.04
runtime-details: README.md, DEPENDENCIES.txt
EOF
fi

( cd "$STAGE_ROOT" && zip -r "$STAGE_ROOT/bundle.zip" "$NAME" >/dev/null )
mv "$STAGE_ROOT/bundle.zip" "$OUT"
echo "bundle.sh: wrote $OUT"
ls -l "$OUT"
