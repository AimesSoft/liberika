#!/usr/bin/env bash
# Cargo runner for native Linux CI: preserve failures and print native crash stacks.
set -euo pipefail
exec gdb --nx --batch --return-child-result \
  -iex 'set auto-load off' \
  -ex 'set disable-randomization off' \
  -ex 'set print thread-events off' \
  -ex run \
  -ex 'thread apply all bt 32' \
  -ex 'info sharedlibrary' \
  --args "$@"
