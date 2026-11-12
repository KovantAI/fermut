#!/bin/sh
# Full corpus, quiet machine. Logs one summary line per cell.
#
# Cold runs alternate the tools ABBA (mutmut, fermut, fermut, mutmut) so
# neither one always gets the cooler machine; warm and sharded follow, as
# they reuse what the cold runs left behind.
cd "$(dirname "$0")"
uptime
run() { echo "== $1 $2 $3"; uv run fermut-bench run --scenario "$1" --tool "$2" --repo "$3" 2>&1 | grep -E "summary|Error|error|Traceback"; }
for r in healthchecks flaskbb werkzeug; do
  run cold mutmut $r
  run cold fermut $r
  run cold fermut $r
  run cold mutmut $r
  run warm mutmut $r
  run warm fermut $r
  run sharded fermut $r
  uptime
done
