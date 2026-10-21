#!/usr/bin/env bash
set -euo pipefail
SP=/private/tmp/claude-501/-Users-drorasaf-code-fermut/3af52320-8c93-4f2a-a0e5-01c4c137bd95/scratchpad
VENV=$SP/hom-venv
FERMUT=/Users/drorasaf/code/fermut/target/debug/fermut
FIX=/Users/drorasaf/code/fermut/benchmarks/fixtures/fermut-pyjwt
MODULE=jwt/api_jws.py

cd "$FIX"
COV="$FIX/coverage.json"   # committed, has per-test contexts

echo "### Phase 0: FOM kill-sets for $MODULE"
time "$FERMUT" run "$MODULE" --tests tests \
  --python "$VENV/bin/python" --coverage "$COV" \
  --no-cache --no-history --no-smart-order --no-verify-baseline \
  --record-kill-sets "$SP/pyjwt-fom.jsonl" 2>&1 | tail -3

echo "### Phase 1: HOM experiment"
time "$FERMUT" hom "$MODULE" --tests tests \
  --python "$VENV/bin/python" --coverage "$COV" \
  --kill-sets "$SP/pyjwt-fom.jsonl" \
  --out "$SP/pyjwt-hom.jsonl" 2>&1 | tail -20
