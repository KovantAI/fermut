#!/usr/bin/env bash
# ref vs --schemata parity on app suites copied to APPS (agent-library, apb-mutation),
# each with its own venv at ~/code/<app>/.venv. Usage: RUN=n runapps.sh [app...]
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/results/10repo
FERMUT=${FERMUT:-$HERE/../../target/release/fermut}
APPS=${APPS:?set APPS to the directory holding the app copies}
UNIT="not integration and not acceptance and not manual and not redteam and not agentdojo and not bdd and not azure"
mkdir -p "$OUT"
for app in ${*:-apb-mutation agent-library}; do
  case $app in
    apb-mutation) sample=0.04; extra=() ;;
    agent-library) sample=0.012; extra=(
      --pytest-arg=--deselect=tests/component_registry_test.py::test_subagent_records_should_match_runtime_when_each_published_tool_is_requested
      --pytest-arg=--deselect=tests/config_code_references_test.py::test_a_fresh_runtime_process_should_start_with_no_unit_roots
      --pytest-arg=--deselect=tests/component_registry_test.py::test_committed_tree_is_not_stale
      # reads .github/scripts/, which fermut's mirror doesn't copy (hidden dir)
      --pytest-arg=--deselect=tests/red_team/collect_findings_test.py) ;;
  esac
  for mode in ${MODES:-ref schemata}; do
    flag=(--no-schemata); [ "$mode" = schemata ] && flag=(--schemata)  # on by default
    t0=$(date +%s)
    ( cd "$APPS/$app" && PATH="$HOME/code/$app/.venv/bin:$PATH" ${MEASURE:+python3 "$HERE/peakrss.py" "$OUT/$mode-$app${RUN:+-r$RUN}.rss.json" --} "$FERMUT" run src --tests tests \
        --coverage "$APPS/$app/.coverage" --no-cache --no-history --timeout 30 --baseline-timeout 1800 \
        --sample "$sample" --pytest-arg=-m "--pytest-arg=$UNIT" ${extra[@]+"${extra[@]}"} \
        --json "$OUT/$mode-$app${RUN:+-r$RUN}.json" ${flag[@]+"${flag[@]}"} ) > "$OUT/$mode-$app${RUN:+-r$RUN}.log" 2>&1
    echo "$app $mode${RUN:+ r$RUN} rc=$? wall=$(( $(date +%s) - t0 ))s" | tee -a "$OUT/walls.txt"
  done
done
