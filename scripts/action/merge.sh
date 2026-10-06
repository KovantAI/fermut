#!/usr/bin/env bash
# Combines downloaded shard reports. `fermut merge` errors on a mutant only a
# skip placeholder covers — a missing shard — so a crashed shard cannot
# inflate the combined score.
#
# env: FERMUT, OUT, ARTIFACT
source "$(dirname "$0")/lib.sh"

shopt -s nullglob
reports=("$OUT"/shards/*/report.json)
if [ "${#reports[@]}" = 0 ]; then
  die "No shard reports found in artifacts named $ARTIFACT-shard-*. Did the shard jobs use KovantAI/fermut/sweep with a shard input and the same artifact-name?"
fi
echo "Merging ${#reports[@]} shard report(s)"
"$FERMUT" merge "${reports[@]}" --json "$OUT/report.json" --markdown "$OUT/report.md"
