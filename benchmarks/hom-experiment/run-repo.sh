#!/usr/bin/env bash
# usage: hom_repo.sh <fixture-name> <module-rel-path> <pkg-import-name> <tag>
set -uo pipefail
SP=/private/tmp/claude-501/-Users-drorasaf-code-fermut/3af52320-8c93-4f2a-a0e5-01c4c137bd95/scratchpad
VENV=$SP/hom-venv
FERMUT=/Users/drorasaf/code/fermut/target/debug/fermut
FIX=/Users/drorasaf/code/fermut/benchmarks/fixtures/fermut-$1
MODULE=$2
TAG=$4
cd "$FIX" || exit 1
COV="$FIX/coverage.json"

echo "### [$TAG] baseline suite"
PYTHONPATH=".:src" "$VENV/bin/python" -m pytest tests -q -p no:cacheprovider >/tmp/hom-$TAG-base.log 2>&1
tail -1 /tmp/hom-$TAG-base.log

echo "### [$TAG] Phase 0"
"$FERMUT" run "$MODULE" --tests tests \
  --python "$VENV/bin/python" --coverage "$COV" \
  --no-cache --no-history --no-smart-order --no-verify-baseline \
  --record-kill-sets "$SP/$TAG-fom.jsonl" 2>&1 | tail -2

echo "### [$TAG] Phase 1 (same-function)"
"$FERMUT" hom "$MODULE" --tests tests \
  --python "$VENV/bin/python" --coverage "$COV" \
  --kill-sets "$SP/$TAG-fom.jsonl" \
  --out "$SP/$TAG-hom.jsonl" --max-pairs 100000 2>&1 | tail -13

echo "### [$TAG] analysis"
python3 "$SP/analyze.py" "$SP/$TAG-fom.jsonl" "$SP/$TAG-hom.jsonl"
