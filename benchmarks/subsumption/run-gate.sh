#!/bin/bash
# Run the `minimal`-profile gate (see gate.py) on the parity corpus.
# Runs only site-tagged operators under the hidden `ror-all` profile, then gates.
# Fixtures + venvs are the parity harness's (benchmarks/fixtures, gitignored).
B=${FERMUT:-$(cd "$(dirname "$0")/../.." && pwd)/target/release/fermut}
F=${FIXTURES:-$(cd "$(dirname "$0")/.." && pwd)/fixtures}
S=${OUT:-$(cd "$(dirname "$0")" && pwd)/results}; mkdir -p "$S"
# Same names as the JSON report's `operator` field.
OPS=compare-op-swap,boundary-shift,ror-equality,ror-const,region-rest,bool-op-swap,bool-operand-drop,bool-const

run() {
  local r=$1 src=$2; shift 2
  cd "$F/fermut-$r" || return
  local s; s=$(date +%s)
  PATH="$F/.venvs/fermut-$r/bin:$PATH" "$B" run "$src" --tests tests \
    --operators ror-all --ops $OPS --no-history --no-cache --json "$S/gate-$r.json" "$@" \
    > "$S/log-$r.txt" 2>&1
  echo "$r $(( $(date +%s) - s ))s :: $(grep -E 'mutants —' "$S/log-$r.txt" | tail -1)"
}

run pyjwt jwt --coverage "$F/fermut-pyjwt/coverage.json"
run markupsafe src/markupsafe --coverage "$F/fermut-markupsafe/coverage.json"
run typer typer --coverage "$F/fermut-typer/coverage.json"
run click src/click --coverage "$F/fermut-click/coverage.json"
# Fails on its baseline under Python 3.13 / pytest 7.4 (expects a DeprecationWarning).
run starlette starlette --coverage "$F/fermut-starlette/coverage.json" \
  "--pytest-arg=--deselect=tests/test_routing.py::test_lifespan_with_on_events"

python3 "$(dirname "$0")/gate.py" "$S"/gate-*.json
