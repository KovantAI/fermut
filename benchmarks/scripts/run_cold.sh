#!/usr/bin/env bash
# Cold-run benchmark: fresh clone, fresh install, first mutation pass.
#
# Usage:
#   ./scripts/run_cold.sh                       # all tools x all repos
#   ./scripts/run_cold.sh fermut more-itertools # one tool, one repo
#   ./scripts/run_cold.sh fermut,mutmut httpx,rich
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/_common.sh"

TOOL="${1:-all}"
REPO="${2:-all}"

"${RUN[@]}" run --scenario cold --tool "$TOOL" --repo "$REPO"
