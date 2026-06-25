#!/usr/bin/env bash
# Shared bootstrap for all run_* scripts.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_ROOT="$(cd "$HERE/.." && pwd)"
cd "$BENCH_ROOT"

# Use uv if available, fall back to plain venv + pip.
if command -v uv >/dev/null 2>&1; then
    uv sync --quiet
    RUN=("uv" "run" "fermut-bench")
else
    if [ ! -d .venv ]; then
        python3 -m venv .venv
        .venv/bin/pip install -U pip wheel
        .venv/bin/pip install -e .
    fi
    RUN=(".venv/bin/fermut-bench")
fi

export RUN
