"""Verdict parity over repeated runs: ref-<repo>-r*.json vs schemata-<repo>-r*.json
in results/10repo. Detected = killed|timed_out. A diff is stable when every ref
run agrees, every schemata run agrees, and the two sides disagree; anything
else is flakiness on the side that varied. `python3 cmp3.py [N]` prints N
stable diff ids per repo."""

import json
import os
import re
import statistics
import sys
from collections import Counter
from pathlib import Path

D = Path(os.environ.get("RESULTS", Path(__file__).parent / "results/10repo"))
DET = {"killed": "D", "timed_out": "D", "survived": "S", "error": "E"}


def load(p):
    o = json.loads(p.read_text())
    return {x["mutant"]["id"]: x["status"] for x in o["outcomes"]}, o["summary"]


def wall(repo, mode):
    walls = [
        int(line.split("wall=")[1].rstrip("s\n"))
        for line in (D / "walls.txt").read_text().splitlines()
        if re.match(rf"{re.escape(repo)} {mode} r\d+ ", line)
    ]
    return f"{statistics.median(walls):.0f}s" if walls else "-"


show = int(sys.argv[1]) if len(sys.argv) > 1 else 0
print(
    f"{'repo':15}{'runs':>5}{'executed':>9}{'ref score':>16}{'sch score':>16}"
    f"{'stable':>7}{'flaky r/s':>10}{'ref wall':>9}{'sch wall':>9}"
)
repos = sorted({p.stem.split("-r")[0][4:] for p in D.glob("ref-*-r[0-9].json")})
for repo in repos:
    refs = [load(p) for p in sorted(D.glob(f"ref-{repo}-r[0-9].json"))]
    schs = [load(p) for p in sorted(D.glob(f"schemata-{repo}-r[0-9].json"))]
    if not refs or not schs:
        continue
    ids = [i for i, s in refs[0][0].items() if s in DET]
    stable, kinds, flaky_r, flaky_s = [], Counter(), 0, 0
    for i in ids:
        r = {DET.get(x[0].get(i), "?") for x in refs}
        s = {DET.get(x[0].get(i), "?") for x in schs}
        flaky_r += len(r) > 1
        flaky_s += len(s) > 1
        if len(r) == 1 and len(s) == 1 and r != s:
            stable.append(i)
            kinds[f"{r.pop()}->{s.pop()}"] += 1

    def score(runs):
        v = [x[1]["mutation_score"] for x in runs]
        return f"{min(v):.1f}-{max(v):.1f}" if min(v) != max(v) else f"{v[0]:.1f}"

    print(
        f"{repo:15}{f'{len(refs)}/{len(schs)}':>5}{len(ids):>9}{score(refs):>16}"
        f"{score(schs):>16}{len(stable):>7}{f'{flaky_r}/{flaky_s}':>10}"
        f"{wall(repo, 'ref'):>9}{wall(repo, 'schemata'):>9}"
        + (f"  {dict(kinds)}" if kinds else "")
    )
    for i in stable[:show]:
        print(
            "   ",
            i.split("/fixtures/")[-1],
            [x[0][i] for x in refs],
            [x[0][i] for x in schs],
        )
