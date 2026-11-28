#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
# Original synthetic material, under the repository's MPL-2.0 license.
ffmpeg -hide_banner -loglevel error -nostdin \
  -f lavfi -i 'color=c=0x304866:size=320x180:rate=10:duration=60' \
  -f ffmetadata -i chapters.ffmetadata -map 0:v -map_metadata 1 -map_chapters 1 \
  -c:v mpeg4 -q:v 5 -g 10 -bf 0 -threads 1 -fflags +bitexact -flags:v +bitexact \
  -y chapters.mkv
