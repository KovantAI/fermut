#!/usr/bin/env bash
# Sum cargo-mutants outcomes across one or more `mutants.out` directories and
# emit caught/missed/scored/score/floor as KEY=VALUE, ready for $GITHUB_OUTPUT.
#
#   .github/scripts/mutants-score.sh mutants.out
#   .github/scripts/mutants-score.sh shards/mutants-out-*
#
# `scored` is caught + missed. Timeouts and unviable mutants are outside it —
# neither reached a verdict on "is this line asserted on?" — which is also the
# forgiving direction. `score` is -1 when nothing did; check `scored` first.
set -euo pipefail

[ "$#" -gt 0 ] || { echo "usage: $0 MUTANTS_OUT_DIR..." >&2; exit 2; }

floor_file="$(dirname "$0")/../mutants-floor.txt"

count_lines() { # count_lines DIR NAME -> line count, 0 if the file is absent
  if [ -f "$1/$2.txt" ]; then wc -l < "$1/$2.txt" | tr -d ' '; else echo 0; fi
}

caught=0
missed=0
for dir in "$@"; do
  [ -d "$dir" ] || { echo "no such directory: $dir" >&2; exit 1; }
  caught=$((caught + $(count_lines "$dir" caught)))
  missed=$((missed + $(count_lines "$dir" missed)))
done

scored=$((caught + missed))
if [ "$scored" -gt 0 ]; then
  score=$((caught * 100 / scored))
else
  score=-1
fi

# First non-comment, non-blank line is the number. Missing or unparseable
# means "no floor" (0), which no score falls below — fails open, not closed.
floor=0
if [ -f "$floor_file" ]; then
  parsed=$(sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$floor_file" | head -1 | tr -cd '0-9')
  [ -n "$parsed" ] && floor=$parsed
fi

echo "caught=$caught"
echo "missed=$missed"
echo "scored=$scored"
echo "score=$score"
echo "floor=$floor"
