#!/usr/bin/env bash
# One-command installer for triagent: builds, installs the binary and skills.
set -euo pipefail
cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo (Rust) not found."
  echo "Install it with:"
  echo '  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
  echo '  source "$HOME/.cargo/env"'
  exit 1
fi

echo "==> building triagent (release)"
cargo build --release

BIN_DIR="${TRIAGENT_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$BIN_DIR"
install -m755 target/release/triagent "$BIN_DIR/triagent"
echo "==> binary: $BIN_DIR/triagent"

./scripts/install-skills.sh

echo
echo "Done. Next steps:"
echo "  1) log in:   triagent login claude   (or chatgpt / gemini)"
echo "  2) run:      triagent --provider claude"
echo
echo "If '$BIN_DIR' is not in your PATH, add this to ~/.bashrc:"
echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
