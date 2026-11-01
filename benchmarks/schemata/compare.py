"""Phase 2: verdict-parity diff between fermut reference runs and spike runs.

    python compare.py --ref ref-1.json [ref-2.json ...] --spike spike-1.json [...]

Detected = killed | timeout (fermut scores both as detected). A diff is
*stable* when every ref run agrees with each other, every spike run agrees with
each other, and the two sides disagree. Anything else is flakiness.
"""

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path


def load(path, ref):
    data = json.loads(Path(path).read_text())["outcomes"]
    if ref:
        return {
            o["mutant"]["id"]: o["status"]
            for o in data
            if o["status"] in ("killed", "survived", "timed_out", "error")
        }
    return {o["id"]: o["status"] + ("*" if "fallback" in o else "") for o in data}


def detected(s):
    return {"killed": "D", "timed_out": "D", "timeout": "D", "survived": "S"}.get(
        s.rstrip("*"), "E"
    )


ap = argparse.ArgumentParser()
ap.add_argument("--ref", nargs="+", required=True)
ap.add_argument("--spike", nargs="+", required=True)
a = ap.parse_args()
refs = [load(p, True) for p in a.ref]
spikes = [load(p, False) for p in a.spike]

ids = set(refs[0])
cells, stable, flaky_ref, flaky_spike, fallback = Counter(), defaultdict(list), 0, 0, 0
for i in ids:
    r = {detected(x[i]) for x in refs if i in x}
    s = {detected(x[i]) for x in spikes if i in x}
    if any(x.get(i, "").endswith("*") for x in spikes):
        fallback += 1
        continue
    if len(r) > 1:
        flaky_ref += 1
    if len(s) > 1:
        flaky_spike += 1
    if len(r) == 1 and len(s) == 1:
        (rv,), (sv,) = r, s
        cells[(rv, sv)] += 1
        if rv != sv:
            stable[f"ref {rv} / spike {sv}"].append(i)

print(
    f"mutants {len(ids)}  fallback {fallback}  flaky ref {flaky_ref}  flaky spike {flaky_spike}"
)
print("ref\\spike " + " ".join(f"{c:>6}" for c in "DSE"))
for rv in "DSE":
    print(f"{rv:9} " + " ".join(f"{cells[(rv, sv)]:6}" for sv in "DSE"))
for k, v in stable.items():
    print(f"\nSTABLE DIFF {k}: {len(v)}")
    for i in v[:15]:
        print("  ", i)
