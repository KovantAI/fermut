#!/usr/bin/env bash
# Appends the merged run's history entry to the persisted history, then
# publishes the trend. Refuses to record a point that would corrupt the trend:
# an empty denominator (a vacuous 100%), too many errored mutants, or a history
# file fermut cannot fully read (appending to it would hide the damage).
#
# env: FERMUT, ENTRY (entry written by merge.sh), HISTORY (absolute path)
#      SCORED, ERRORED, MAX_ERRORED, LIMIT, BLOCKING
# out: recorded (true|false), delta (points vs the previous comparable entry;
#      empty when there is none)
source "$(dirname "$0")/lib.sh"

out recorded false
out delta ""

if [ "$SCORED" = 0 ]; then
  gate_fail "No mutants were scored — refusing to record a vacuous 100% in the trend."
fi
# MAX_ERRORED is a count, or a percentage of tested mutants (scored + errored).
if [[ "$MAX_ERRORED" == *% ]]; then
  if awk -v e="$ERRORED" -v t="$((SCORED + ERRORED))" -v p="${MAX_ERRORED%\%}" \
       'BEGIN { exit !(e * 100 > p * t) }'; then
    gate_fail "$ERRORED of $((SCORED + ERRORED)) tested mutant(s) errored (max-errored $MAX_ERRORED) — refusing to record an untrustworthy score in the trend."
  fi
elif [ -n "$MAX_ERRORED" ] && [ "$ERRORED" -gt "$MAX_ERRORED" ]; then
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
# Drop survivor_ids (~80 B per survivor, ~1MB for a large suite) before the
# entry is appended or pushed: the history branch keeps every entry forever and
# git stores each one as a new blob. Costs `fermut trend --diff` its survivor
# diff on CI history; the merged report artifact still lists every survivor.
# Rewriting ENTRY in place is what keeps them off the branch too.
jq -c 'del(.survivor_ids)' "$ENTRY" > "$ENTRY.tmp" && mv "$ENTRY.tmp" "$ENTRY"
cat "$ENTRY" >> "$HISTORY"
out recorded true

# Score delta vs the latest earlier comparable entry: scored, same branch,
# same config_hash and fermut_version. The pair the regression gate compares
# (`regression_against`); keep the two in step.
delta=$("$FERMUT" trend --history-path "$HISTORY" --all --format json | jq -r '
  def scored: (.killed + .survived + .timed_out) > 0 and (.partial | not);
  (.[-1]) as $cur
  | [ .[:-1][] | select(scored)
      | select(.config_hash == $cur.config_hash and .fermut_version == $cur.fermut_version)
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
