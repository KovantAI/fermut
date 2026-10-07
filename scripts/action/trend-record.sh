#!/usr/bin/env bash
# Appends the merged run's history entry to the persisted history, then
# publishes the trend. Refuses to record a point that would corrupt the trend:
# an empty denominator (a vacuous 100%), too many errored mutants, or a history
# file fermut cannot fully read (appending to it would hide the damage).
#
# env: FERMUT, ENTRY (entry written by merge.sh), HISTORY (absolute path)
#      SCORED, ERRORED, MAX_ERRORED, LIMIT, BLOCKING
# out: recorded (true|false), delta (points vs the previous entry on the same
#      branch; empty when there is none)
source "$(dirname "$0")/lib.sh"

out recorded false
out delta ""

if [ "$SCORED" = 0 ]; then
  gate_fail "No mutants were scored — refusing to record a vacuous 100% in the trend."
fi
if [ -n "$MAX_ERRORED" ] && [ "$ERRORED" -gt "$MAX_ERRORED" ]; then
  gate_fail "$ERRORED mutant(s) errored (max-errored $MAX_ERRORED) — refusing to record an untrustworthy score in the trend."
fi
[ -s "$ENTRY" ] || die "fermut merge wrote no history entry at $ENTRY."

if [ -s "$HISTORY" ]; then
  echo "Restored history: $(grep -c . "$HISTORY") entry line(s)"
  "$FERMUT" trend --history-path "$HISTORY" --all --strict --format json >/dev/null \
    || die "$HISTORY has malformed lines (see above). Fix or remove them; appending would hide the damage."
else
  echo "No history restored — starting a new trend."
fi
mkdir -p "$(dirname "$HISTORY")"
# The entry is one JSON line; normalise its trailing newline.
printf '%s\n' "$(cat "$ENTRY")" >> "$HISTORY"
out recorded true

# Score delta vs the latest earlier scored entry on the same branch, the pair
# the regression gate compares.
delta=$("$FERMUT" trend --history-path "$HISTORY" --all --format json | jq -r '
  def scored: (.killed + .survived + .timed_out) > 0;
  (.[-1]) as $cur
  | [ .[:-1][] | select(scored)
      | select(.git_branch == null or $cur.git_branch == null or .git_branch == $cur.git_branch) ]
  | if length == 0 then "" else ($cur.mutation_score - .[-1].mutation_score) * 10 | round / 10 | tostring end')
out delta "$delta"
echo "delta=${delta:-<none>}"

{
  echo "### fermut trend"
  echo
  echo '```text'
  "$FERMUT" trend --history-path "$HISTORY" --limit "$LIMIT"
  echo '```'
} >> "$GITHUB_STEP_SUMMARY"
