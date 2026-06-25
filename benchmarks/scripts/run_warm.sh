#!/usr/bin/env bash
# Warm-run benchmark: tool + repo already installed, caches populated.
# Requires a prior cold run for the same (tool, repo); the harness will
# fall back to running cold first if no fixtures exist yet.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/_common.sh"

TOOL="${1:-all}"
REPO="${2:-all}"

"${RUN[@]}" run --scenario warm --tool "$TOOL" --repo "$REPO"
