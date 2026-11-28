#!/usr/bin/env bash
# Run a command with an owned PulseAudio child; reap it before deleting its files.
set -euo pipefail
if (( $# == 0 )); then
  printf 'Usage: %s command [args...]\n' "$0" >&2
  exit 2
fi
export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR="$(mktemp -d)"
export PULSE_RUNTIME_PATH="$XDG_RUNTIME_DIR/pulse"
# Never let a client autospawn a second server while the owned one starts.
export PULSE_SERVER="unix:$PULSE_RUNTIME_PATH/native"
pulse_pid=""
cleanup() {
  local status=$? pulse_status=0
  trap - EXIT
  if [[ -n "$pulse_pid" ]]; then
    if kill -0 "$pulse_pid" 2>/dev/null; then
      kill "$pulse_pid" || { if (( status == 0 )); then status=1; fi; }
    fi
    wait "$pulse_pid" || pulse_status=$?
    if (( status == 0 && pulse_status != 0 && pulse_status != 143 )); then
      status=$pulse_status
    fi
  fi
  rm -rf -- "$XDG_RUNTIME_DIR" || { if (( status == 0 )); then status=1; fi; }
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
pulseaudio --daemonize=no --exit-idle-time=-1 --log-target=stderr &
pulse_pid=$!
# A foreground server has no daemon-start handshake; wait for its socket.
deadline=$((SECONDS + 10))
until pactl info >/dev/null 2>&1; do
  if ! kill -0 "$pulse_pid" 2>/dev/null || (( SECONDS >= deadline )); then
    printf 'PulseAudio did not become ready.\n' >&2
    exit 1
  fi
  sleep 0.05
done
pactl load-module module-null-sink sink_name=erika_ci
"$@"
