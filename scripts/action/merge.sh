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
#      PROJECTS     optional newline-separated repo-relative project dirs:
#                   pool `<ARTIFACT>-proj-<slug>[-shard-*]` reports per project
#                   as `DIR=REPORT` inputs (a monorepo aggregate)
source "$(dirname "$0")/lib.sh"

shopt -s nullglob
reports=("$OUT"/shards/*/report.json)
if [ "${#reports[@]}" = 0 ]; then
  if [ -n "${PROJECTS:-}" ]; then
    die "No project reports found in artifacts named $ARTIFACT-proj-*. Did the sweep jobs set project and the same artifact-name?"
  fi
  die "No shard reports found in artifacts named $ARTIFACT-shard-*. Did the shard jobs use KovantAI/fermut/sweep with a shard input and the same artifact-name?"
fi
if [ -n "${PROJECTS:-}" ]; then
  inputs=()
  while IFS= read -r dir; do
    dir="${dir#"${dir%%[![:space:]]*}"}"
    dir="${dir%"${dir##*[![:space:]]}"}"
    dir="${dir%/}"
    [ -n "$dir" ] || continue
    base="$OUT/shards/$ARTIFACT-proj-$(project_slug "$dir")"
    # `[n]` makes the unsharded path a glob too, so nullglob drops it if absent.
    found=("$base"/report.jso[n] "$base"-shard-*/report.json)
    [ "${#found[@]}" != 0 ] \
      || die "No reports for project $dir (artifact ${base##*/}[-shard-*]). Did its sweep job run with project: $dir?"
    for r in "${found[@]}"; do inputs+=("$dir=$r"); done
  done <<< "$PROJECTS"
  # A report no listed project claims would silently drop out of the pool.
  [ "${#inputs[@]}" = "${#reports[@]}" ] \
    || die "${#reports[@]} report(s) downloaded but the projects list claims ${#inputs[@]}. List every swept project in projects."
  reports=("${inputs[@]}")
fi
echo "Merging ${#reports[@]} report(s)"
args=(--json "$OUT/report.json" --markdown "$OUT/report.md")
if [ -n "${ENTRY:-}" ]; then
  args+=(--history "$ENTRY" --project "$PROJECT")
  if [ -n "${CONFIG_HASH:-}" ]; then args+=(--config-hash "$CONFIG_HASH"); fi
fi
"$FERMUT" merge "${reports[@]}" "${args[@]}"
