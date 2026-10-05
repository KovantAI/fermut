#!/usr/bin/env bash
# Runs `fermut run`. Records the exit code instead of failing, so the report is
# always published; gate.sh decides the job's fate. A run that writes no report
# is a setup failure and fails here.
#
# env: FERMUT, PY, SOURCE, TESTS, OUT
#      DIFF_BASE   diff-scope to this base; empty runs the whole tree
#      SHARD       optional "i/n"; a shard never gates
#      FAIL_UNDER  optional threshold
#      EXTRA_ARGS  word-split extra flags
# out: rc
source "$(dirname "$0")/lib.sh"

args=(--coverage .coverage --no-verify-baseline --no-history
      --json "$OUT/report.json" --markdown "$OUT/report.md")
if [ -n "$SOURCE" ]; then args=("$SOURCE" "${args[@]}"); fi
if [ -n "$PY" ]; then args+=(--python "$PY"); fi
if [ -n "$TESTS" ]; then args+=(--tests "$TESTS"); fi
if [ -n "$DIFF_BASE" ]; then
  args+=(--diff-only "$DIFF_BASE")
else
  args+=(--no-diff-only)
fi
if [ -n "$SHARD" ]; then
  # A shard sees one slice of the universe; its score gates nothing.
  args+=(--shard "$SHARD" --no-fail)
elif [ -n "$FAIL_UNDER" ]; then
  args+=(--fail-under "$FAIL_UNDER")
fi

set +e
# shellcheck disable=SC2086 # EXTRA_ARGS is word-split on purpose.
"$FERMUT" run "${args[@]}" $EXTRA_ARGS
rc=$?
set -e

out rc "$rc"
if [ ! -s "$OUT/report.json" ]; then
  die "fermut exited $rc without writing a report — a setup failure, not a mutation result. See the log above."
fi
