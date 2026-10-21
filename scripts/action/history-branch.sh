#!/usr/bin/env bash
# Persists the trend history on an orphan branch: one file per run under
# entries/, named `<timestamp>-<sha>.json`, so concurrent runs never conflict
# on content and the files sort into chronological order. Same layout as
# 4thbrain/actions' fermut-record-history.yml.
#
# Works in an isolated repo under RUNNER_TEMP; the workspace checkout is never
# touched.
#
# usage: history-branch.sh read|write
# env: BRANCH   the history branch
#      TOKEN    token for the push (write) and for private repos (read).
#               Empty: anonymous, e.g. a public repo or a file:// test remote.
#      HISTORY  absolute path of the history file. read overwrites it.
#      ENTRY    the entry to append (write only)
source "$(dirname "$0")/lib.sh"

mode="$1"
store="$RUNNER_TEMP/fermut-history-$mode"
rm -rf "$store"
git init -q "$store"
url="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY.git"
if [ -n "${TOKEN:-}" ]; then url="${url/:\/\//://x-access-token:${TOKEN}@}"; fi
git -C "$store" remote add origin "$url"

# Sets `state`: 0 = branch exists, 2 = absent (a first run). Anything else is
# an unreachable store, never mistaken for an empty one: that would silently
# re-baseline the trend. A function, not $(...), so die exits the script.
remote_state() {
  set +e
  git -C "$store" ls-remote --exit-code --heads origin "$BRANCH" >/dev/null
  state=$?
  set -e
  case "$state" in
    0|2) ;;
    *) die "Could not reach history branch $BRANCH (git ls-remote exit $state). Refusing to continue with an empty trend." ;;
  esac
}

if [ "$mode" = read ]; then
  mkdir -p "$(dirname "$HISTORY")"
  : > "$HISTORY"
  remote_state
  if [ "$state" = 2 ]; then
    echo "No $BRANCH branch yet — starting a new trend."
    exit 0
  fi
  git -C "$store" fetch -q --depth 1 origin "$BRANCH"
  git -C "$store" checkout -q FETCH_HEAD
  if [ -d "$store/entries" ]; then
    find "$store/entries" -maxdepth 1 -type f -name '*.json' -print0 \
      | sort -z | xargs -0 -r awk 1 > "$HISTORY"  # awk 1: one line per file, newline-terminated
  fi
  echo "Restored $(grep -c . "$HISTORY" || true) entry line(s) from $BRANCH."
  exit 0
fi

[ "$mode" = write ] || die "history-branch.sh: unknown mode \"$mode\"."
name="$(jq -r '"\(.timestamp)-\(.git_sha // "nosha")"' "$ENTRY" | tr ':' '-').json"
blob=$(printf '%s\n' "$(cat "$ENTRY")" | git -C "$store" hash-object -w --stdin)

# An unforced push is a compare-and-swap: a rejection means another run moved
# the branch, so rebuild on the new tip and retry.
for attempt in 1 2 3 4 5; do
  parent=""
  remote_state
  if [ "$state" = 0 ]; then
    # Full fetch: commit-tree cannot parent onto a shallow graft.
    git -C "$store" fetch -q origin "$BRANCH"
    parent=$(git -C "$store" rev-parse FETCH_HEAD)
  fi
  rm -f "$store/.git/index"
  if [ -n "$parent" ]; then git -C "$store" read-tree "$parent"; else git -C "$store" read-tree --empty; fi
  git -C "$store" update-index --add --cacheinfo "100644,$blob,entries/$name"
  tree=$(git -C "$store" write-tree)
  if [ -n "$parent" ] && [ "$tree" = "$(git -C "$store" rev-parse "$parent^{tree}")" ]; then
    echo "$name is already on $BRANCH."
    exit 0
  fi
  msg="chore(fermut): record trend entry $name"
  run="Run: $GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID"
  commit=$(GIT_AUTHOR_NAME=github-actions GIT_AUTHOR_EMAIL=actions@users.noreply.github.com \
           GIT_COMMITTER_NAME=github-actions GIT_COMMITTER_EMAIL=actions@users.noreply.github.com \
           git -C "$store" commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg" -m "$run")
  if git -C "$store" push -q origin "$commit:refs/heads/$BRANCH"; then
    echo "Recorded $name on $BRANCH."
    exit 0
  fi
  echo "::notice::Push to $BRANCH rejected (attempt $attempt); retrying on the new tip."
  sleep $((attempt * 3 + RANDOM % 5))
done
die "Could not push to $BRANCH after 5 attempts. Does the job have \`permissions: contents: write\`, and does no ruleset protect $BRANCH? The history is still in the <artifact-name>-trend artifact."
