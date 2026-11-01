"""Verdict parity: ref-<repo>.json vs schemata-<repo>.json in results/10repo.
Detected = killed|timed_out. Prints one row per repo + diff ids."""

import json
import sys
from pathlib import Path

D = Path(__file__).parent / "results/10repo"
DET = {"killed": "D", "timed_out": "D", "survived": "S"}


def load(p):
    o = json.loads(p.read_text())
    return {x["mutant"]["id"]: x["status"] for x in o["outcomes"]}, o.get("summary", {})


print(
    f"{'repo':16}{'executed':>9}{'ref score':>10}{'sch score':>10}{'diffs':>7}{'err r/s':>9}"
)
for ref in sorted(D.glob("ref-*.json")):
    repo = ref.stem[4:]
    sch = D / f"schemata-{repo}.json"
    if not sch.exists():
        continue
    (r, rs), (s, ss) = load(ref), load(sch)
    ex = [i for i in r if r[i] in DET or r[i] == "error"]
    diffs = [
        (i, r[i], s.get(i)) for i in ex if DET.get(r[i], "E") != DET.get(s.get(i), "E")
    ]
    err = sum(r[i] == "error" for i in ex), sum(s.get(i) == "error" for i in ex)
    print(
        f"{repo:16}{len(ex):>9}{rs.get('mutation_score', 0):>10.1f}{ss.get('mutation_score', 0):>10.1f}{len(diffs):>7}{f'{err[0]}/{err[1]}':>9}"
    )
    for d in diffs[: int(sys.argv[1]) if len(sys.argv) > 1 else 0]:
        print("   ", *d)
