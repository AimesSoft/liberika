#!/usr/bin/env bash
# Cargo runner for native Linux CI: preserve failures and print native crash stacks.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
exec gdb --nx --batch --return-child-result \
  -iex 'set auto-load off' \
  -x "$SCRIPT_DIR/linux_test_runner.gdb" \
  --args "$@"
