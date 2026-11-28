set -euo pipefail
cd /work
export XDG_RUNTIME_DIR=/tmp/erika-pr-desktop
pulseaudio --start --exit-idle-time=-1
trap 'pulseaudio --kill' EXIT
pactl load-module module-null-sink sink_name=erika_pr_review
/target/debug/deps/erika-97c3bc9988221846 pulse_audio_pause_resume_flush_and_reconfigure --ignored --nocapture --test-threads=1
/target/debug/linux_native_demo --x11 --smoke-seconds 5 --require-audio crates/erika/testdata/playback/playback-fixture.mkv
/target/debug/linux_capi_smoke crates/erika/testdata/playback/playback-fixture.mkv
/target/debug/linux_native_demo --x11 --smoke-seconds 5 /review-logs/bbb-10s.mp4
