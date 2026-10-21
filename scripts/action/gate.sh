#!/usr/bin/env bash
# Decides the job's fate from a finished run or merge.
#
# env: RC          `fermut run` exit code; empty after a merge, which does not
#                  gate, so the run's rule is re-applied to the merged summary
#      BLOCKING, FAIL_UNDER, SCORE, SCORED, SURVIVED, ERRORED
source "$(dirname "$0")/lib.sh"

if [ "$ERRORED" -gt 0 ] && [ "$SCORED" = 0 ]; then
  gate_fail "All $ERRORED mutant(s) errored — the suite likely fails under mutation for reasons unrelated to the change. The score is not trustworthy."
fi

if [ -n "$RC" ]; then
  if [ "$RC" != 0 ]; then
    gate_fail "fermut exited $RC — score ${SCORE:-N/A}%${FAIL_UNDER:+ (threshold $FAIL_UNDER%)}, $SURVIVED survivor(s). See the job summary."
  fi
  echo "Gate passed: ${SCORE:-N/A}% over $SCORED scored mutant(s)."
  exit 0
fi

if [ "$SCORED" = 0 ]; then
  echo "No mutants scored — N/A."
  exit 0
fi
if [ -n "$FAIL_UNDER" ]; then
  awk -v s="$SCORE" -v t="$FAIL_UNDER" 'BEGIN { exit !(s + 0 >= t + 0) }' \
    || gate_fail "Mutation score $SCORE% is below fail-under $FAIL_UNDER%."
elif [ "$SURVIVED" -gt 0 ]; then
  gate_fail "$SURVIVED mutant(s) survived. Set fail-under to gate on a score instead."
fi
echo "Gate passed: $SCORE% over $SCORED scored mutant(s)."
