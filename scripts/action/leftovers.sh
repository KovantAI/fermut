#!/usr/bin/env bash
# Leftover wave (sweep/ with `leftovers: true`): merges the downloaded shard
# reports and counts the mutants a shard never started before --max-time
# (`skipped/time-budget`). run.sh then tests only those, via --mutants-from.
#
# env: FERMUT, OUT, ARTIFACT
# out: count   unresolved mutants; "0" skips the rest of the job
source "$(dirname "$0")/lib.sh"

shopt -s nullglob
reports=("$OUT"/shards/*/report.json)
[ "${#reports[@]}" != 0 ] \
  || die "No shard reports found in artifacts named $ARTIFACT-shard-*. Run the leftovers job after the shard matrix, with the same artifact-name."

# Without --require-complete merge only warns about budget skips.
"$FERMUT" merge "${reports[@]}" --json "$OUT/merged.json"
count="$(jq '[.outcomes[] | select(.status == "skipped" and .filter == "time-budget")] | length' "$OUT/merged.json")"
out count "$count"
echo "$count mutant(s) left untested by the shards"
