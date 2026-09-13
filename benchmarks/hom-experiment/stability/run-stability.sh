#!/usr/bin/env bash
# SSHOM stability across commits (pyjwt jwt/api_jwk.py): discover the SSHOM set
# at each api_jwk-touching commit so we can measure Jaccard drift.
set -uo pipefail
export PYTHONDONTWRITEBYTECODE=1
SP=/private/tmp/claude-501/-Users-drorasaf-code-fermut/3af52320-8c93-4f2a-a0e5-01c4c137bd95/scratchpad
VENV=$SP/py313
FERMUT=/Users/drorasaf/code/fermut/target/debug/fermut
REPO=$SP/pyjwt-git
OUT=$SP/stability
MODULE=jwt/api_jwk.py
mkdir -p "$OUT"
: > "$OUT/status.log"

# chronological (oldest first)
SHAS="feffd51 c0eae05 53e9381 1451d70 8915570 30b7ca1"

cd "$REPO" || exit 1
for sha in $SHAS; do
  git checkout -q -f "$sha" 2>/dev/null || { echo "$sha CHECKOUT_FAIL" | tee -a "$OUT/status.log"; continue; }
  find . -name __pycache__ -type d -prune -exec rm -rf {} + 2>/dev/null
  rm -f .coverage coverage.json
  subj=$(git log -1 --format=%s "$sha")

  PYTHONPATH=. "$VENV/bin/python" -m pytest tests -q -p no:cacheprovider >/tmp/st-base.log 2>&1
  if [ $? -ne 0 ]; then echo "$sha BASELINE_FAIL | $subj" | tee -a "$OUT/status.log"; continue; fi

  PYTHONPATH=. "$VENV/bin/python" -m pytest tests --cov=jwt --cov-context=test \
    -q -p no:cacheprovider >/tmp/st-cov.log 2>&1
  "$VENV/bin/python" -m coverage json -o coverage.json --show-contexts >/dev/null 2>&1

  "$FERMUT" run "$MODULE" --tests tests --python "$VENV/bin/python" --coverage coverage.json \
    --no-cache --no-history --no-smart-order --no-verify-baseline \
    --record-kill-sets "$OUT/$sha-fom.jsonl" >/tmp/st-p0.log 2>&1
  "$FERMUT" hom "$MODULE" --tests tests --python "$VENV/bin/python" --coverage coverage.json \
    --kill-sets "$OUT/$sha-fom.jsonl" --out "$OUT/$sha-hom.jsonl" --max-pairs 100000 \
    >/tmp/st-p1.log 2>&1

  killed=$(grep -c '"status":"killed"' "$OUT/$sha-fom.jsonl" 2>/dev/null || echo 0)
  sshom=$(grep -c '"class":"sshom"' "$OUT/$sha-hom.jsonl" 2>/dev/null || echo 0)
  echo "$sha OK killed=$killed sshom=$sshom | $subj" | tee -a "$OUT/status.log"
done
echo "ALL DONE" | tee -a "$OUT/status.log"
