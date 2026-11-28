# Subtitle seek fixtures

Small synthetic Matroska files with embedded ASS subtitles, for the tests of
what a seek shows on screen. Made for the liberika repository from the
adjacent SubRip files and FFmpeg's color source; no third-party media. They
are distributed under the repository root's MPL-2.0 license.

- `long.mkv`: one video stream and one subtitle stream (track 1) with a cue
  from 0 to 50 s, so a seek to 40 s lands inside a cue that began long before
  the seek's keyframe, plus cues at 10 s and 30 s.
- `two-tracks.mkv`: one video stream and two subtitle streams (tracks 1 and
  2) whose cues cover 40-50 s and 56-59 s with different text.
- `switch-track.mkv`: one video stream and two subtitle streams; track 1 has
  a cue at 0-1 s, track 2 one cue from 35 to 45 s, which is on at a switch
  to track 2 at 40 s although it began before that.

`./generate.sh` rebuilds them and refreshes `SHA256SUMS`; CI checks the
committed files against it. The bytes depend on the FFmpeg version.
