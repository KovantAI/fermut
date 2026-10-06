#!/usr/bin/env bash
# Validates inputs and prepares the report directory.
#
# env: BOOL_<name>  every boolean input, checked to be exactly true/false
#      FAIL_UNDER   optional score threshold
#      REGRESSION   optional regression threshold in points (trend/ only)
#      MAX_ERRORED  optional errored-mutant ceiling (trend/ only)
#      WD           working directory (optional)
#      SHARD        optional "i/n"
#      ARTIFACT     report artifact name
#      BRANCH       default branch, when the action needs one (trend/ only)
# out: out, out_abs, artifact
source "$(dirname "$0")/lib.sh"

for var in $(compgen -v BOOL_ || true); do
  name="${var#BOOL_}"
  check_bool "${name//_/-}" "${!var}"
done
check_fail_under "${FAIL_UNDER:-}"
if [ -n "${REGRESSION:-}" ] && ! [[ "$REGRESSION" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
  die "fail-on-regression must be a non-negative number of score points, got \"$REGRESSION\"."
fi
if [ -n "${MAX_ERRORED:-}" ] && ! [[ "$MAX_ERRORED" =~ ^[0-9]+$ ]]; then
  die "max-errored must be a non-negative integer, got \"$MAX_ERRORED\"."
fi

if [ -n "${BRANCH+x}" ] && [ -z "$BRANCH" ]; then
  die "Cannot infer the default branch on a ${GITHUB_EVENT_NAME:-unknown} event. Set default-branch."
fi

if [ -n "${WD:-}" ] && [ ! -d "$WD" ]; then
  die "working-directory \"$WD\" does not exist. Did you forget actions/checkout?"
fi

artifact="$ARTIFACT"
if [ -n "${SHARD:-}" ]; then
  [[ "$SHARD" =~ ^[1-9][0-9]*/[1-9][0-9]*$ ]] || die "shard must look like \"i/n\", got \"$SHARD\"."
  [ "${SHARD%/*}" -le "${SHARD#*/}" ] || die "shard index ${SHARD%/*} exceeds total ${SHARD#*/}."
  artifact="$ARTIFACT-shard-${SHARD%/*}-of-${SHARD#*/}"
fi

# Reports live outside the project so `fermut run` never mutates or scans
# them, and under the workspace so upload-artifact can reach them.
dir=".fermut-action"
mkdir -p "$GITHUB_WORKSPACE/$dir"
out out "$dir"
out out_abs "$GITHUB_WORKSPACE/$dir"
out artifact "$artifact"
