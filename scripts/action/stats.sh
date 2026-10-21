#!/usr/bin/env bash
# Reads the headline numbers from the report's `summary` object and publishes
# the job summary. `scored` separates a real 100% from an empty denominator.
#
# env: OUT, HEADING (optional job-summary heading)
# out: score (empty when N/A), scored, killed, survived, errored
source "$(dirname "$0")/lib.sh"

s() { jq -r ".summary.$1 // 0" "$OUT/report.json"; }
scored=$(s scored); killed=$(s killed); survived=$(s survived); errored=$(s errored)
# Reports from fermut < 0.3 lack `scored`; derive it.
if [ "$scored" = 0 ]; then scored=$(( killed + survived + $(s timed_out) )); fi
score=""
if [ "$scored" -gt 0 ]; then score=$(s mutation_score); fi

out scored "$scored"
out killed "$killed"
out survived "$survived"
out errored "$errored"
out score "$score"
echo "score=${score:-N/A} scored=$scored killed=$killed survived=$survived errored=$errored"

{
  if [ -n "${HEADING:-}" ]; then echo "### $HEADING"; fi
  if [ "$scored" = 0 ]; then
    echo "No mutants were scored — the score is N/A, not 100%. Changed lines may be untested or not mutable."
    echo
  fi
  if [ -f "$OUT/report.md" ]; then cat "$OUT/report.md"; fi
} >> "$GITHUB_STEP_SUMMARY"
