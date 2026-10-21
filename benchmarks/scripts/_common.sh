#!/usr/bin/env bash
# Shared bootstrap for all run_* scripts.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_ROOT="$(cd "$HERE/.." && pwd)"
cd "$BENCH_ROOT"

# Use uv if available, fall back to plain venv + pip. Both install exactly what
# uv.lock pins; --locked fails rather than silently re-resolving a stale lock.
# requirements.txt is the hash-locked export of uv.lock for the pip path:
#   uv export --locked --format requirements-txt --no-emit-project --no-dev -o requirements.txt
if command -v uv >/dev/null 2>&1; then
    uv sync --locked --quiet
    RUN=("uv" "run" "--locked" "fermut-bench")
else
    if [ ! -d .venv ]; then
        python3 -m venv .venv
        .venv/bin/pip install --require-hashes -r requirements.txt
    fi
    # Run from source instead of `pip install -e .`, which would pull an
    # unpinned hatchling into the build-isolation env.
    RUN=("env" "PYTHONPATH=$BENCH_ROOT/src" ".venv/bin/python" "-m" "bench.main")
fi

export RUN
