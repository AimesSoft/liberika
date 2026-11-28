#!/usr/bin/env bash
set -euo pipefail

REPORT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKOUT="$(cd "${1:-$REPORT_DIR/../../..}" && pwd)"
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'This scenario validates native Metal texture export and requires macOS.' >&2
  exit 1
fi
for tool in ffmpeg ffprobe python3 cargo; do command -v "$tool" >/dev/null; done
test -f "$CHECKOUT/crates/erika/tests/headless_subtitle_export.rs"
ARTIFACTS="${ERIKA_HEADLESS_ARTIFACTS:-$CHECKOUT/target/issue-validation/headless-ass}"
mkdir -p "$ARTIFACTS"
ARTIFACTS="$(cd "$ARTIFACTS" && pwd)"

ffmpeg -hide_banner -y -f lavfi -i color=c=black:s=1920x1080:r=30:d=8 \
  -i "$REPORT_DIR/fixture.ass" -map 0:v -map 1:s -c:v libx264 -preset ultrafast \
  -pix_fmt yuv420p -c:s ass -t 8 "$ARTIFACTS/fixture.mkv" \
  > "$ARTIFACTS/fixture-generation.log" 2>&1
ffprobe -v error -show_entries stream=index,codec_name,codec_type,width,height,duration \
  -of json "$ARTIFACTS/fixture.mkv" > "$ARTIFACTS/fixture-probe.json"

cd "$CHECKOUT"
ERIKA_SUBTITLE_DIAG=1 \
ERIKA_HEADLESS_SUBTITLE_FIXTURE="$ARTIFACTS/fixture.mkv" \
ERIKA_HEADLESS_SUBTITLE_PNG="$ARTIFACTS/output.png" \
  cargo test --locked -p erika --features wgpu --test headless_subtitle_export \
  -- --ignored --nocapture 2>&1 | tee "$ARTIFACTS/run.log"

python3 - "$ARTIFACTS" <<'PY'
import json
import pathlib
import re
import sys

out = pathlib.Path(sys.argv[1])
log = (out / "run.log").read_text()
line = next(line for line in log.splitlines() if line.startswith("HEADLESS_ASS_RESULT "))
result = json.loads(line.split(" ", 1)[1])
result["updateOverlayCalls"] = len(re.findall("stage=update_overlay ", log))
result["clockOverlayCalls"] = len(re.findall("stage=clock_overlay ", log))
result["clockOverlayViewports"] = sorted(set(re.findall(r"overlay_viewport=(\d+x\d+)", log)))
result["fixtureProbe"] = json.loads((out / "fixture-probe.json").read_text())
(out / "run-summary.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps(result, indent=2))
PY
