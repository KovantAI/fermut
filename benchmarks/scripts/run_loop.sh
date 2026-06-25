#!/usr/bin/env bash
# Loop benchmark: N small edits, re-run after each, average over iterations.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/_common.sh"

TOOL="${1:-all}"
REPO="${2:-all}"

"${RUN[@]}" run --scenario loop --tool "$TOOL" --repo "$REPO"
