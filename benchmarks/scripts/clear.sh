#!/usr/bin/env bash
# Wipe benchmark artifacts. Default: results only.
#
# Usage:
#   ./scripts/clear.sh                # results/runs/*.json
#   ./scripts/clear.sh --fixtures     # also wipe cloned repos + tool venvs
#   ./scripts/clear.sh --all          # results + fixtures + built wheels
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_ROOT="$(cd "$HERE/.." && pwd)"

clear_results() {
    rm -rf "$BENCH_ROOT/results/runs"
    mkdir -p "$BENCH_ROOT/results/runs"
    echo "cleared: results/runs/"
}

clear_fixtures() {
    rm -rf "$BENCH_ROOT/fixtures"/*/
    rm -rf "$BENCH_ROOT/fixtures/.venvs"
    echo "cleared: fixtures/ (cloned repos + venvs)"
}

clear_wheels() {
    rm -rf "$BENCH_ROOT/fixtures/.wheels"
    echo "cleared: fixtures/.wheels/"
}

case "${1:-results}" in
    results) clear_results ;;
    --fixtures) clear_results; clear_fixtures ;;
    --all) clear_results; clear_fixtures; clear_wheels ;;
    -h|--help) sed -n '1,/^set/p' "$0" | sed 's/^# \?//'; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
esac
