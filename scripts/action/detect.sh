#!/usr/bin/env bash
# PR gate: resolves the diff base, makes it diffable, and short-circuits a PR
# that touched no Python. Without this a docs-only PR would report a vacuous
# 100%. Runs in the working directory, so the pathspec is scoped to it.
#
# env: WARM_ONLY       "true" skips detection; the run always proceeds
#      BASE_REF        explicit base, may be empty
#      PR_BASE         github.base_ref
#      DEFAULT_BRANCH  repository default branch
# out: go (true|false), skip (true|false), base
source "$(dirname "$0")/lib.sh"

if [ "$WARM_ONLY" = "true" ]; then
  out go true
  out skip false
  exit 0
fi

base="$BASE_REF"
if [ -z "$base" ]; then
  branch="${PR_BASE:-$DEFAULT_BRANCH}"
  [ -n "$branch" ] || die "Cannot infer the base branch on a ${GITHUB_EVENT_NAME} event. Set base-ref."
  base="origin/$branch"
fi

# A merge-base needs history; actions/checkout defaults to depth 1.
if [ "$(git rev-parse --is-shallow-repository)" = "true" ]; then
  echo "Shallow checkout — fetching full history (set fetch-depth: 0 on actions/checkout to skip this)."
  git fetch --no-tags --quiet --unshallow origin
fi
if ! git rev-parse --verify --quiet "$base^{commit}" >/dev/null && [[ "$base" == origin/* ]]; then
  git fetch --no-tags --quiet origin "+refs/heads/${base#origin/}:refs/remotes/$base" || true
fi
git rev-parse --verify --quiet "$base^{commit}" >/dev/null \
  || die "base-ref \"$base\" does not resolve to a commit, and fetching it from origin failed. Set base-ref, or check out with fetch-depth: 0."

changed=$(git diff --name-only "$base...HEAD" -- '*.py' | wc -l | tr -d ' ')
echo "Python files changed vs $base: $changed"
out base "$base"
if [ "$changed" = "0" ]; then
  out go false
  out skip true
  echo "No Python changes vs \`$base\` — mutation testing N/A." >> "$GITHUB_STEP_SUMMARY"
else
  out go true
  out skip false
fi
