set -euo pipefail
export XDG_RUNTIME_DIR=/tmp/erika-pr-desktop
export LIBGL_ALWAYS_SOFTWARE=1
pulseaudio --start --exit-idle-time=-1
trap 'pulseaudio --kill' EXIT
pactl load-module module-null-sink sink_name=erika_subtitle_review
python3 /review-logs/linux-live-subtitle.py "$@"
