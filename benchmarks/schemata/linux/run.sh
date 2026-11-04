#!/usr/bin/env bash
# Linux schemata parity, from macOS via Docker. Results land in
# benchmarks/schemata/results/linux on the host.
#   linux/run.sh setup        build fermut + fixtures (idempotent)
#   linux/run.sh parity [n]   n rounds (default 3) of run10.sh ref vs schemata
#   linux/run.sh apps [n]     n rounds of runapps.sh (apps prepared in /work/apps)
#   linux/run.sh compare      cmp3.py over the Linux results
set -eu
# A sleeping Mac pauses the Docker VM mid-run: inflated walls and false timeouts.
if [ -z "${CAFFEINATED:-}" ] && command -v caffeinate >/dev/null; then
  CAFFEINATED=1 exec caffeinate -ims "$0" "$@"
fi
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
  apps)
    # Apps copied to /work/apps with Linux venvs (see README, "Linux parity").
    # 4 workers: the app suites at the default 14 OOM the 8 GB VM.
    run "cd benchmarks/schemata && for r in \$(seq 1 ${2:-3}); do \
         RUN=\$r JOBS=${APP_JOBS:-4} APPS=/work/apps VENVS=/work/apps OUT=/fermut/benchmarks/schemata/results/linux \
         FERMUT=/work/target/release/fermut ./runapps.sh; done"
    ;;
  compare)
    run "cd benchmarks/schemata && RESULTS=results/linux python3 cmp3.py 5"
    ;;
  *) echo "usage: $0 setup | parity [rounds] | apps [rounds] | compare" >&2; exit 2 ;;
esac
