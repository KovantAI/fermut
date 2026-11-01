#!/usr/bin/env bash
# Linux schemata parity, from macOS via Docker. Results land in
# benchmarks/schemata/results/linux on the host.
#   linux/run.sh setup        build fermut + fixtures (idempotent)
#   linux/run.sh parity [n]   n rounds (default 3) of run10.sh ref vs schemata
#   linux/run.sh compare      cmp3.py over the Linux results
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
IMAGE=fermut-schemata-linux
docker build -q -t $IMAGE "$HERE" >/dev/null
run() {
  docker run --rm -v "$ROOT":/fermut -v fermut-linux-work:/work \
    -e JOBS="${JOBS:-8}" -e FORCE="${FORCE:-}" $IMAGE bash -c "$1"
}
case ${1:-} in
  setup)
    run "cargo build --release --locked -q && python3 benchmarks/schemata/linux/setup.py"
    ;;
  parity)
    run "cd benchmarks/schemata && for r in \$(seq 1 ${2:-3}); do \
         RUN=\$r OUT=/fermut/benchmarks/schemata/results/linux FERMUT=/work/target/release/fermut ./run10.sh; done"
    ;;
  compare)
    run "cd benchmarks/schemata && RESULTS=results/linux python3 cmp3.py 5"
    ;;
  *) echo "usage: $0 setup | parity [rounds] | compare" >&2; exit 2 ;;
esac
