"""Phase 0: project schemata speedup from a real fermut run.

    python project.py <ref.json> <startup.json-line> [--collected]

Each executed func-body mutant saves (spawn_one_test - fork) ms of fixed cost;
everything else (fallback mutants, fermut's own pre/post phases) is unchanged.
"""

import json
import statistics
import subprocess
import sys
from pathlib import Path

ref = json.loads(Path(sys.argv[1]).read_text())
st = json.loads(sys.argv[2])
fork = st["fork_after_collect" if "--collected" in sys.argv else "fork_one_test"]
saved = st["spawn_one_test"] - fork

scope = subprocess.run(
    [sys.executable, Path(__file__).with_name("scope.py"), sys.argv[1], "--executed"],
    check=True,
    capture_output=True,
    text=True,
).stdout
shares = {
    line.split()[0]: float(line.split()[2].rstrip("%")) / 100
    for line in scope.splitlines()
    if "%" in line
}
func_share = shares.get("func", 0.0)
trap_share = sum(v for k, v in shares.items() if k.startswith("trap:"))

ran = [
    o
    for o in ref["outcomes"]
    if o["status"] not in ("skipped", "equivalent") and "duration_ms" in o
]
d = [o["duration_ms"] for o in ran]
t = [o.get("test_ms") or 0 for o in ran]
mutant_ms = sum(d)
ratio = fork / st["spawn_one_test"]


def speedup(share):
    """Subtract model: a fixed `saved` ms per mutant (exact when unloaded)."""
    return round(
        mutant_ms / sum(share * max(x - saved, fork) + (1 - share) * x for x in d), 2
    )


def speedup_ratio(share):
    """Ratio model: non-test time shrinks by fork/spawn (load inflates both alike)."""
    new = sum(share * (y + (x - y) * ratio) + (1 - share) * x for x, y in zip(d, t))
    return round(mutant_ms / new, 2)


print(
    json.dumps(
        {
            "ref": Path(sys.argv[1]).name,
            "executed": len(d),
            "p50_ms": statistics.median(d),
            "p90_ms": sorted(d)[int(0.9 * len(d))],
            "p50_test_ms": statistics.median(t),
            "func_share": round(func_share, 3),
            "saved_per_mutant_ms": round(saved, 1),
            "trap_share": round(trap_share, 3),
            "speedup_first_cut": speedup(func_share),
            "speedup_with_traps": speedup(func_share + trap_share),
            "ratio_first_cut": speedup_ratio(func_share),
            "ratio_with_traps": speedup_ratio(func_share + trap_share),
            "wall_s": ref["duration_ms"] / 1000,
        }
    )
)
