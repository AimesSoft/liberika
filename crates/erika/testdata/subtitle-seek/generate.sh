#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
# Original synthetic material, under the repository's MPL-2.0 license.
# One video frame per second for 60 s; the SubRip files become ASS tracks.
video='color=c=0x304866:size=64x64:rate=1:duration=60'
common=(-c:v mpeg4 -q:v 5 -c:s ass -threads 1 -fflags +bitexact -flags:v +bitexact)
ffmpeg -hide_banner -loglevel error -nostdin \
  -f lavfi -i "$video" -i long.srt \
  -map 0:v -map 1:0 "${common[@]}" -y long.mkv
ffmpeg -hide_banner -loglevel error -nostdin \
  -f lavfi -i "$video" -i track-a.srt -i track-b.srt \
  -map 0:v -map 1:0 -map 2:0 "${common[@]}" -y two-tracks.mkv
shasum -a 256 long.mkv two-tracks.mkv long.srt track-a.srt track-b.srt > SHA256SUMS
