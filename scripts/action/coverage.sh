#!/usr/bin/env bash
# Builds the per-test coverage DB that lets fermut run only the tests covering
# each mutant. Incremental: a restored DB re-measures only invalidated tests.
#
# env: FERMUT, PY, SOURCE, TESTS
source "$(dirname "$0")/lib.sh"

args=()
if [ -n "$PY" ]; then args+=(--python "$PY"); fi
if [ -n "$SOURCE" ]; then args+=(--source "$SOURCE"); fi
if [ -n "$TESTS" ]; then args+=(--tests "$TESTS"); fi
"$FERMUT" coverage "${args[@]}"
