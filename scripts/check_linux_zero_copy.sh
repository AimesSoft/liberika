#!/usr/bin/env bash
# A real Vulkan image/synchronization test. This does not replace VA-API
# decoder testing on Intel/AMD hardware or physical HDR display validation.
set -euo pipefail
cd "$(dirname "$0")/.."
export WGPU_BACKEND=vulkan ERIKA_TEST_LINUX_VULKAN=1
cc -std=c11 -Wall -Wextra -Werror -DERIKA_TEST_LINUX_VULKAN \
  -fsyntax-only crates/erika/src/renderer/linux_vulkan.c \
  $(pkg-config --cflags libavutil vulkan)
cargo test --locked -p erika --features wgpu --lib \
  linux_direct_vulkan_sampling_preserves_pixels_and_releases_frames -- --nocapture --test-threads=1
