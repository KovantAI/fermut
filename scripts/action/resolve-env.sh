#!/usr/bin/env bash
# Finds the test interpreter and the fermut binary. Mirrors fermut's own
# interpreter lookup — $VIRTUAL_ENV, then a .venv walking up to the repo root,
# then python on PATH — so pytest-cov lands where fermut will run the tests.
# The path is absolute: fermut runs tests from a mirror directory, where a
# relative --python would not resolve.
#
# env: SPEC      install-fermut.sh's spec output
#      NEED_COV  "true" installs pytest-cov when missing
# out: python, bin
source "$(dirname "$0")/lib.sh"

py=""
if [ -n "${VIRTUAL_ENV:-}" ] && [ -x "$VIRTUAL_ENV/bin/python" ]; then
  py="$VIRTUAL_ENV/bin/python"
else
  dir="$PWD"
  while :; do
    if [ -x "$dir/.venv/bin/python" ]; then py="$dir/.venv/bin/python"; break; fi
    if [ -e "$dir/.git" ] || [ "$dir" = "/" ]; then break; fi
    dir="$(dirname "$dir")"
  done
fi
[ -n "$py" ] || py="$(command -v python3 || command -v python || true)"

if [ "$SPEC" = "project" ]; then
  bin=""
  [ -n "$py" ] && [ -x "$(dirname "$py")/fermut" ] && bin="$(dirname "$py")/fermut"
  [ -n "$bin" ] || bin="$(command -v fermut || true)"
  [ -n "$bin" ] || die "fermut-version is \"project\" but fermut is not installed in the project environment (${py:-no interpreter found})."
else
  bin="$(uv tool dir --bin)/fermut"
fi

echo "fermut: $bin ($("$bin" --version))"
echo "python: ${py:-<none>}"
out python "$py"
out bin "$bin"

if [ "$NEED_COV" = "true" ] && [ -n "$py" ] && ! "$py" -c "import pytest_cov" 2>/dev/null; then
  echo "pytest-cov missing from $py — installing it (fermut coverage needs it)."
  uv pip install --quiet --python "$py" pytest-cov || "$py" -m pip install --quiet pytest-cov
fi
