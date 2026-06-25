#!/usr/bin/env bash
# Aggregate all JSON results into a Markdown table.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/_common.sh"
"${RUN[@]}" report
