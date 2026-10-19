#!/usr/bin/env bash
# Install the bundled skills into triagent's config directory.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${TRIAGENT_SKILLS_DIR:-$HOME/.config/triagent/skills}"
mkdir -p "$DEST"
cp -a "$HERE/skills/." "$DEST/"
echo "installed $(ls "$DEST" | wc -l) skills into $DEST"
