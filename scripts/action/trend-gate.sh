#!/usr/bin/env bash
# Gates a recorded trend point: an optional absolute floor, then the
# regression gate (`fermut trend --fail-on-regression`, which compares the
# newest entry with the previous scored entry on the same branch).
#
# env: FERMUT, HISTORY, BLOCKING, FAIL_UNDER, SCORE, REGRESSION, DELTA
source "$(dirname "$0")/lib.sh"

if [ -n "$FAIL_UNDER" ]; then
  awk -v s="$SCORE" -v t="$FAIL_UNDER" 'BEGIN { exit !(s + 0 >= t + 0) }' \
    || gate_fail "Mutation score $SCORE% is below fail-under $FAIL_UNDER%."
fi

if [ -n "$REGRESSION" ]; then
  set +e
  msg=$("$FERMUT" trend --history-path "$HISTORY" --all --format json \
          --fail-on-regression "$REGRESSION" 2>&1 >/dev/null)
  rc=$?
  set -e
  if [ "$rc" != 0 ]; then
    gate_fail "${msg:-fermut trend exited $rc} — score $SCORE% (${DELTA} pts vs the previous run)."
  fi
fi
echo "Trend gate passed: $SCORE%${DELTA:+ (${DELTA} pts vs the previous run)}."
