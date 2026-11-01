"""Phase 3: speed + parity table across repos.

    python summarize.py <batch.log> [repo ...]

batch.log is the output of the run batch (lines `== spike|ref <repo> <n>` and
`== list <repo>`, each followed by a `real <s>` line). Timing should come from
ref/spike runs interleaved in that log: wall time drifts ~50% over a long
session (thermal), so separately-run batches aren't comparable.

Projected end-to-end for a real schemata runner = fermut fixed cost (generation
+ filters, from `fermut list`) + baseline suite run (results/baseline.json) +
spike wall + cold cost of its fallback mutants (ref wall per executed mutant x
fallback count, i.e. fermut's own parallel throughput — pessimistic, it
includes fermut's fixed cost again).
"""

import json
import re
import statistics
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).parent
RES = HERE / "results"

walls = defaultdict(list)
key = None
for line in Path(sys.argv[1]).read_text().splitlines():
    if m := re.match(r"== (spike|ref|list) (\S+)", line):
        key = (m[1], m[2])
    elif (m := re.match(r"real (\S+)", line)) and key:
        walls[key].append(float(m[1]))
        key = None

baseline = json.loads(Path(RES / "baseline.json").read_text())
repos = sys.argv[2:] or sorted({r for _, r in walls})
print(
    "| repo | executed | fermut wall | spike wall | fallback | projected | speedup | parity (ref×spike runs) |"
)
print("|---|---|---|---|---|---|---|---|")
for r in repos:
    refs = sorted(RES.glob(f"ref-{r}-[0-9].json"))
    spikes = sorted(RES.glob(f"spike-co-{r}-[0-9].json"))
    # Interleaved ref/spike pairs in the log are the fair timing (the machine
    # drifts ~50% over a long session); report durations are the fallback.
    ref_wall = statistics.mean(
        walls[("ref", r)]
        or [json.loads(Path(p).read_text())["duration_ms"] / 1000 for p in refs]
    )
    # template + mutant phase; the spike's Python prelude (parsing a coverage
    # JSON of up to 4 GB for test selection) is fermut's job in Rust and is
    # already inside fermut's wall.
    spike_wall = statistics.mean(
        d["template_s"] + d["phase_s"]
        for d in (json.loads(Path(p).read_text()) for p in spikes)
    )
    out = json.loads(Path(spikes[0]).read_text())["outcomes"]
    executed = len(out)
    fallback = sum("fallback" in o for o in out)
    fixed = statistics.mean(walls[("list", r)]) if walls[("list", r)] else 0.0
    projected = fixed + baseline[r] + spike_wall + ref_wall * fallback / executed
    cmp = subprocess.run(
        [
            sys.executable,
            HERE / "compare.py",
            "--ref",
            *map(str, refs),
            "--spike",
            *map(str, spikes),
        ],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    stable = sum(int(n) for n in re.findall(r"STABLE DIFF .*: (\d+)", cmp))
    flaky = re.search(r"flaky ref (\d+)\s+flaky spike (\d+)", cmp)
    print(
        f"| {r} | {executed} | {ref_wall:.1f}s | {spike_wall:.1f}s | {fallback} ({100 * fallback / executed:.0f}%) "
        f"| {projected:.1f}s | {ref_wall / projected:.1f}× | {len(refs)}×{len(spikes)}: {stable} stable diffs, "
        f"flaky ref {flaky[1]} / spike {flaky[2]} |"
    )
