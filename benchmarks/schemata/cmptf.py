"""Timeout budget (schemata default, --timeout-factor 3) vs flat --timeout under
schemata, at fermut's default --timeout: schemata-<repo>-rdN vs schflat-<repo>-rdN."""

import json
import re
import statistics
from pathlib import Path

D = Path(__file__).parent / "results/10repo"


def run(path):
    o = json.loads(path.read_text())
    st = {x["mutant"]["id"]: x["status"] for x in o["outcomes"]}
    log = re.sub(r"\x1b\[[0-9;]*m", "", path.with_suffix(".log").read_text())
    b = re.search(r"timeout_secs=(\d+)", log)
    return st, (int(b.group(1)) if b else None)


def wall(repo, mode, tag):
    for line in (D / "walls.txt").read_text().splitlines():
        if line.startswith(f"{repo} {mode} {tag} "):
            return int(line.split("wall=")[1].rstrip("s\n"))


print(
    f"{'repo':15}{'round':>6}{'flat':>7}{'budget':>8}{'timeouts flat/budget':>22}{'budget s':>10}"
    f"{'S→T':>5}{'T→S':>5}"
)
for repo in ["more-itertools", "pyjwt", "starlette"]:
    walls = {"schflat": [], "schemata": []}
    for n in (1, 2):
        tag = f"rd{n}"
        f, b = D / f"schflat-{repo}-{tag}.json", D / f"schemata-{repo}-{tag}.json"
        if not (f.exists() and b.exists()):
            continue
        (fs, _), (bs, secs) = run(f), run(b)
        s2t = [i for i in fs if fs[i] == "survived" and bs.get(i) == "timed_out"]
        t2s = [i for i in fs if fs[i] == "timed_out" and bs.get(i) == "survived"]
        fw, bw = wall(repo, "schflat", tag), wall(repo, "schemata", tag)
        walls["schflat"].append(fw)
        walls["schemata"].append(bw)
        ft = sum(s == "timed_out" for s in fs.values())
        bt = sum(s == "timed_out" for s in bs.values())
        print(
            f"{repo:15}{n:>6}{fw:>6}s{bw:>7}s{f'{ft} / {bt}':>22}{secs or '-':>10}{len(s2t):>5}{len(t2s):>5}"
        )
        for i in s2t[:3]:
            print("      survived→timed_out", i.split("/")[-1][:90])
    if walls["schflat"]:
        print(
            f"{'':15}{'median':>6}{statistics.median(walls['schflat']):>6.0f}s"
            f"{statistics.median(walls['schemata']):>7.0f}s"
        )
