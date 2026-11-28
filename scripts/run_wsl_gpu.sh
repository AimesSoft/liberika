#!/usr/bin/env bash
# Use WSLg's Mesa D3D12 driver for hardware presentation, scoped to this app.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ! -e /dev/dxg ]]; then
  echo "WSL GPU interface /dev/dxg is missing. Use this launcher inside WSL2 with WSLg." >&2
  exit 1
fi
export GALLIUM_DRIVER=d3d12
export WGPU_BACKEND=gl
export ERIKA_REQUIRE_HARDWARE_GPU=1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/erika-target}"
# Optional: MESA_D3D12_DEFAULT_ADAPTER_NAME=NVIDIA (or AMD/Intel).
exec cargo run --locked -p linux_native_demo -- "$@"
