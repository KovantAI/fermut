"""Run a command and report the peak summed RSS of its whole process tree.

    python3 peakrss.py <out.json> -- <cmd...>

Samples `ps` every 0.5 s. RSS double-counts pages forked children share with
their template (copy-on-write), so the schemata number is an upper bound."""

import json
import subprocess
import sys
import time

out, cmd = sys.argv[1], sys.argv[sys.argv.index("--") + 1 :]
proc = subprocess.Popen(cmd)
peak = {"total_mb": 0.0, "procs": 0, "largest_mb": 0.0, "pythons": 0}
tmpl = {"templates_mb": 0, "templates": 0, "per_template_mb": 0}
start = time.time()
while proc.poll() is None:
    rows = subprocess.run(
        ["ps", "-A", "-o", "pid=,ppid=,rss=,args="],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.splitlines()
    kids, info, ppid_of = {}, {}, {}
    for row in rows:
        pid, ppid, rss, comm = row.split(None, 3)
        kids.setdefault(int(ppid), []).append(int(pid))
        ppid_of[int(pid)] = int(ppid)
        info[int(pid)] = (int(rss) / 1024, comm)
    tree, todo = [], [proc.pid]
    while todo:
        p = todo.pop()
        if p in info:
            tree.append(p)
        todo += kids.get(p, [])
    total = sum(info[p][0] for p in tree)
    # Schemata templates (not their forked children): the memory schemata adds.
    ts = [
        info[p][0]
        for p in tree
        if "_fermut_schema.py serve" in info[p][1]
        and "_fermut_schema.py serve" not in info.get(ppid_of.get(p), (0, ""))[1]
    ]
    if sum(ts) > tmpl["templates_mb"]:
        tmpl.update(
            templates_mb=round(sum(ts)),
            templates=len(ts),
            per_template_mb=round(max(ts)),
        )
    if total > peak["total_mb"]:
        peak.update(
            total_mb=round(total),
            procs=len(tree),
            largest_mb=round(max(info[p][0] for p in tree)),
            pythons=sum("python" in info[p][1] for p in tree),
            at_s=round(time.time() - start),
        )
    time.sleep(0.5)
peak.update(tmpl)
peak["wall_s"] = round(time.time() - start)
peak["rc"] = proc.returncode
with open(out, "w") as f:
    json.dump(peak, f)
print(json.dumps(peak))
