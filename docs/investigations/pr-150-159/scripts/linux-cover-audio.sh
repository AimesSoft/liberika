set -euo pipefail
export XDG_RUNTIME_DIR=/tmp/erika-pr-desktop
export LIBGL_ALWAYS_SOFTWARE=1
pulseaudio --start --exit-idle-time=-1
trap 'pulseaudio --kill' EXIT
pactl load-module module-null-sink sink_name=erika_cover_review
set +e
/target/debug/linux_native_demo --x11 --smoke-seconds 5 --require-audio /review-logs/cover-5s.mp3
with_cover=$?
/target/debug/linux_native_demo --x11 --smoke-seconds 5 --require-audio /review-logs/no-cover-5s.mp3
without_cover=$?
printf 'Audio-only exit codes: cover=%s no-cover=%s\n' "$with_cover" "$without_cover"
exit 0
