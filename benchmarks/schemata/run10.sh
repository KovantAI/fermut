#!/usr/bin/env bash
# ref vs --schemata parity on the 10 fermut-* fixtures. Usage: run10.sh [repo...]
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
FX=${FX:-$HERE/../fixtures}
OUT=${OUT:-$HERE/results/10repo}
FERMUT=${FERMUT:-$HERE/../../target/release/fermut}
mkdir -p "$OUT"
# repo  src  tests  sample  extra-args
SPECS='
pyjwt jwt tests 1
markupsafe src/markupsafe tests 1
more-itertools more_itertools tests 1
typer typer tests 1 --pytest-arg=--deselect=tests/test_tutorial/test_parameter_types/test_path/test_tutorial002.py::test_not_exists --pytest-arg=--deselect=tests/test_type_conversion.py::test_path_convert_failures
starlette starlette tests 1
click src/click tests 0.3
flask src/flask tests 0.3 --pytest-arg=--deselect=tests/test_reqctx.py::test_bad_environ_raises_bad_request
jinja2 src/jinja2 tests 0.3
werkzeug src/werkzeug tests 0.3 --pytest-arg=--deselect=tests/test_serving.py
trio src/trio src/trio/_tests 0.3 --pytest-arg=-p --pytest-arg=trio._tests.pytest_plugin --pytest-arg=--import-mode=importlib --pytest-arg=--deselect=src/trio/_tests/test_exports.py::test_static_tool_sees_all_symbols --pytest-arg=--ignore=src/trio/_tests/tools/test_gen_exports.py
'
want=" $* "
echo "$SPECS" | while read -r repo src tests sample extra; do
  [ -z "$repo" ] && continue
  [ $# -gt 0 ] && [[ "$want" != *" $repo "* ]] && continue
  wt=$FX/fermut-$repo
  # Root ignores chmod (the Linux container): starlette's permission test gets 200, not 401.
  [ "$repo" = starlette ] && [ "$(id -u)" = 0 ] && \
    extra="--pytest-arg=--deselect=tests/test_staticfiles.py::test_staticfiles_with_invalid_dir_permissions_returns_401"
  cov=$wt/.coverage; [ -f "$cov" ] || cov=$wt/coverage.json
  for mode in ${MODES:-ref schemata}; do
    case $mode in
      schemata) flag=--schemata ;;
      schflat) flag="--schemata --no-timeout-factor" ;;  # schemata, flat --timeout
      cap) flag="" ;;               # defaults: schemata + auto memory cap
      nocap) flag=--no-max-memory ;;  # defaults minus the memory cap
      *) flag=--no-schemata ;;  # the normal run (schemata is on by default)
    esac
    T=${TIMEOUT-10}  # TIMEOUT= (empty): fermut's own default
    t0=$(date +%s)
    ( cd "$wt" && PATH="$FX/.venvs/fermut-$repo/bin:$PATH" ${MEASURE:+python3 "$HERE/peakrss.py" "$OUT/$mode-$repo${RUN:+-r$RUN}.rss.json" --} "$FERMUT" run "$src" --tests "$tests" \
        --coverage "$cov" --no-cache --no-history ${T:+--timeout $T} ${JOBS:+--jobs $JOBS} --baseline-timeout 1200 \
        ${RUNNER:+--runner $RUNNER} --sample "$sample" --json "$OUT/$mode-$repo${RUN:+-r$RUN}.json" $flag $extra ) \
      > "$OUT/$mode-$repo${RUN:+-r$RUN}.log" 2>&1
    echo "$repo $mode${RUN:+ r$RUN} rc=$? wall=$(( $(date +%s) - t0 ))s" | tee -a "$OUT/walls.txt"
  done
done
