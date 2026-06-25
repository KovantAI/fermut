#!/usr/bin/env bash
# Mutation-score-uplift benchmark: Claude writes tests in a loop until the
# score climbs >= 3 points over baseline. Requires `claude` CLI on PATH and
# valid Anthropic credentials.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/_common.sh"

if ! command -v claude >/dev/null 2>&1; then
    echo "warning: 'claude' CLI not on PATH — runs will be marked skipped_no_claude" >&2
fi

TOOL="${1:-all}"
REPO="${2:-all}"

"${RUN[@]}" run --scenario score_3pct --tool "$TOOL" --repo "$REPO"
