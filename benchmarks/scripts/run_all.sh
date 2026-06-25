#!/usr/bin/env bash
# Run every scenario across every tool x every repo. Long: hours.
# Cold runs first so warm/loop/score scenarios have prepared fixtures.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/_common.sh"

"${RUN[@]}" run --scenario cold       --tool all --repo all
"${RUN[@]}" run --scenario warm       --tool all --repo all
"${RUN[@]}" run --scenario loop       --tool all --repo all
"${RUN[@]}" run --scenario score_3pct --tool all --repo all
"${RUN[@]}" report
