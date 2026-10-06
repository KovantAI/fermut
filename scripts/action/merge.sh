#!/usr/bin/env bash
# Combines downloaded shard reports. `fermut merge` errors on a mutant only a
# skip placeholder covers — a missing shard — so a crashed shard cannot
# inflate the combined score.
#
# env: FERMUT, OUT, ARTIFACT
#      ENTRY        optional path; also writes a history entry for the merged
#                   report there (trend/ only)
#      PROJECT      project root for the entry's git sha/branch
#      CONFIG_HASH  optional config_hash to stamp into the entry
source "$(dirname "$0")/lib.sh"

shopt -s nullglob
reports=("$OUT"/shards/*/report.json)
if [ "${#reports[@]}" = 0 ]; then
  die "No shard reports found in artifacts named $ARTIFACT-shard-*. Did the shard jobs use KovantAI/fermut/sweep with a shard input and the same artifact-name?"
fi
echo "Merging ${#reports[@]} shard report(s)"
args=(--json "$OUT/report.json" --markdown "$OUT/report.md")
if [ -n "${ENTRY:-}" ]; then
  args+=(--history "$ENTRY" --project "$PROJECT")
  if [ -n "${CONFIG_HASH:-}" ]; then args+=(--config-hash "$CONFIG_HASH"); fi
fi
"$FERMUT" merge "${reports[@]}" "${args[@]}"
