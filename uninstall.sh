#!/usr/bin/env bash
set -euo pipefail

TARGET_BIN="${HOME}/.local/bin/codex-pool"

if [[ -f "${TARGET_BIN}" ]]; then
  rm -f "${TARGET_BIN}"
  echo "Removed ${TARGET_BIN}"
else
  echo "No installed binary found at ${TARGET_BIN}"
fi

echo "Account data in ~/.codex/account-pool was left untouched."
