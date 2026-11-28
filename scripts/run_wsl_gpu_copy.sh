#!/usr/bin/env bash
# Opt-in experimental WSL decode/presentation without CPU pixel transfers.
set -euo pipefail
PREFIX=${ERIKA_WSL_MESA_PREFIX:-$HOME/.local/opt/erika-wsl-mesa}
[[ -e /dev/dxg ]] || { echo 'WSL GPU interface /dev/dxg is required' >&2; exit 1; }
[[ $# -gt 0 ]] || { echo 'usage: run_wsl_gpu_copy.sh COMMAND [ARG...]' >&2; exit 2; }
[[ -f "$PREFIX/lib/dri/d3d12_drv_video.so" ]] || { echo "Missing isolated VA driver in $PREFIX" >&2; exit 1; }
shopt -s nullglob
ICDS=("$PREFIX"/share/vulkan/icd.d/dzn_icd.*.json)
[[ ${#ICDS[@]} -eq 1 ]] || { echo "Expected one Dozen ICD in $PREFIX" >&2; exit 1; }
export VK_DRIVER_FILES=${ICDS[0]}
export LIBVA_DRIVERS_PATH="$PREFIX/lib/dri"
export LIBVA_DRIVER_NAME=d3d12
export GALLIUM_DRIVER=d3d12 WGPU_BACKEND=vulkan
export WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1
export ERIKA_WSL_D3D12=1 ERIKA_HWDEC=vaapi
export ERIKA_VAAPI_DEVICE=${ERIKA_VAAPI_DEVICE:-${DISPLAY:-:0}}
export ERIKA_REQUIRE_HARDWARE_GPU=1 ERIKA_REQUIRE_HARDWARE_DECODE=1
export ERIKA_REQUIRE_GPU_FRAMES=1
# Leave ERIKA_REQUIRE_ZERO_COPY untouched: this path must reject strict direct
# zero-copy requests because it performs one GPU copy of the decoded planes.
exec "$@"
