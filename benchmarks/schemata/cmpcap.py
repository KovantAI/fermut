"""Memory-cap A/B: cap-<repo>.json (defaults) vs nocap-<repo>.json
(--no-max-memory) in results/memcap. A transition other than
timed_out/error/survived → killed on the cap side, or any detected → survived,
is a suspect false kill (or flake). `python3 cmpcap.py` prints the table and
every changed mutant."""

import json
import os
import re
from collections import Counter
from pathlib import Path

D = Path(os.environ.get("RESULTS", Path(__file__).parent / "results/memcap"))


def load(p):
    o = json.loads(p.read_text())
    return {x["mutant"]["id"]: x["status"] for x in o["outcomes"]}, o["summary"]


def walls():
    w = {}
    for line in (D / "walls.txt").read_text().splitlines():
        m = re.match(r"(\S+) (\S+) rc=\d+ wall=(\d+)s", line)
        if m:
            w[(m[1], m[2])] = int(m[3])
    return w


def rss(repo, mode):
    p = D / f"{mode}-{repo}.rss.json"
    if not p.exists():
        return "-"
    r = json.loads(p.read_text())
    return f"{r['total_mb'] / 1024:.1f}G/{r['largest_mb'] / 1024:.1f}G"


def cap_mib(repo):
    p = D / f"cap-{repo}.log"
    log = p.exists() and re.sub(r"\x1b\[[0-9;]*m", "", p.read_text())
    m = log and re.search(r"cap_mib=(\d+)", log)
    return m[1] if m else "-"


W = walls()
print(
    f"{'repo':15}{'cap MiB':>8}{'score cap':>10}{'nocap':>8}{'changed':>8}"
    f"{'wall cap':>9}{'nocap':>7}{'rss cap tot/max':>17}{'nocap':>11}"
)
changes = []
for p in sorted(D.glob("cap-*.json")):
    if p.name.endswith(".rss.json"):
        continue
    repo = p.stem[4:]
    q = D / f"nocap-{repo}.json"
    if not q.exists():
        continue
    (a, sa), (b, sb) = load(p), load(q)
    diff = [(i, b[i], a[i]) for i in a.keys() & b.keys() if a[i] != b[i]]
    changes += [(repo, *d) for d in diff]
    print(
        f"{repo:15}{cap_mib(repo):>8}{sa.get('mutation_score') or 0:>10.1f}{sb.get('mutation_score') or 0:>8.1f}"
        f"{len(diff):>8}{W.get((repo, 'cap'), 0):>8}s{W.get((repo, 'nocap'), 0):>6}s"
        f"{rss(repo, 'cap'):>17}{rss(repo, 'nocap'):>11}"
    )
print()
print("transitions nocap → cap:", dict(Counter((x, y) for _, _, x, y in changes)))
for repo, i, x, y in sorted(changes):
    print(f"  {repo}: {x} → {y}  {i}")
