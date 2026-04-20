#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_DIR="${HOME}/.local/bin"
TARGET_BIN="${TARGET_DIR}/codex-pool"

cd "${SCRIPT_DIR}"
cargo build --release

mkdir -p "${TARGET_DIR}"
install -m 0755 "${SCRIPT_DIR}/target/release/codex-pool" "${TARGET_BIN}"

cat <<EOF
Installed codex-pool -> ${TARGET_BIN}

If "${TARGET_DIR}" is not in your PATH, add:
  export PATH="${TARGET_DIR}:\$PATH"
EOF
