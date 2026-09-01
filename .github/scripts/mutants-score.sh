#!/usr/bin/env bash
# Sum cargo-mutants outcomes across one or more `mutants.out` directories and
# emit the mutation score as KEY=VALUE lines, ready to append to $GITHUB_OUTPUT.
#
#   .github/scripts/mutants-score.sh mutants.out
#   .github/scripts/mutants-score.sh shards/mutants-out-*
#
# Emits: caught, missed, scored, score, floor.
#
# `scored` is caught + missed. Timeouts and unviable mutants are deliberately
# outside it: an unviable mutant did not compile and a timeout did not reach a
# verdict, so neither answers "is this line asserted on?". Excluding them is
# also the forgiving direction — they can never drag the score down.
#
# `score` is -1 when nothing reached a verdict. Callers must check `scored`
# before comparing against `floor`.
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

# Comments and blank lines are allowed in the floor file; the first remaining
# line is the number. A missing or unparseable file means "no floor" (0), which
# no score can fall below — the gate fails open, never closed.
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
