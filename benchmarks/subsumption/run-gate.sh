#!/bin/bash
# Run the `minimal`-profile gate (see gate.py) on the parity corpus.
# Runs only site-tagged operators under the hidden `ror-all` profile, then gates.
# Fixtures + venvs are the parity harness's (benchmarks/fixtures, gitignored).
# Each repo's `.coverage` is rebuilt first (--full) (NO_COVERAGE_REFRESH=1 skips it).
# Usage: run-gate.sh [repo...] (default: all; the gate reads every gate-*.json in OUT).
B=${FERMUT:-$(cd "$(dirname "$0")/../.." && pwd)/target/release/fermut}
F=${FIXTURES:-$(cd "$(dirname "$0")/.." && pwd)/fixtures}
S=${OUT:-$(cd "$(dirname "$0")" && pwd)/results}; mkdir -p "$S"
# Same names as the JSON report's `operator` field.
OPS=compare-op-swap,boundary-shift,ror-equality,ror-const,region-rest,bool-op-swap,bool-operand-drop,bool-const

want=" $* "
run() {
  local r=$1 src=$2; shift 2
  [ "$want" != "  " ] && [[ "$want" != *" $r "* ]] && return
  cd "$F/fermut-$r" || return
  local s; s=$(date +%s)
  export PATH="$F/.venvs/fermut-$r/bin:$PATH"
  if [ -z "${NO_COVERAGE_REFRESH:-}" ]; then
    "$B" coverage --full . "$@" > "$S/coverage-$r.txt" 2>&1 || { echo "$r: fermut coverage failed (see $S/coverage-$r.txt)"; return; }
  fi
  "$B" run "$src" --tests tests --coverage "$F/fermut-$r/.coverage" \
    --operators ror-all --ops $OPS --no-history --no-cache --json "$S/gate-$r.json" "$@" \
    > "$S/log-$r.txt" 2>&1
  echo "$r $(( $(date +%s) - s ))s :: $(grep -E 'mutants —' "$S/log-$r.txt" | tail -1)"
}

# Subshells: each repo's PATH stays its own.
(run pyjwt jwt)
(run markupsafe src/markupsafe)
# Fail their own baseline on this env (rich-formatted click errors); as run10.sh.
(run typer typer \
  --pytest-arg=--deselect=tests/test_tutorial/test_parameter_types/test_path/test_tutorial002.py::test_not_exists \
  --pytest-arg=--deselect=tests/test_type_conversion.py::test_path_convert_failures)
(run click src/click)
(run starlette starlette)

python3 "$(dirname "$0")/gate.py" "$S"/gate-*.json
