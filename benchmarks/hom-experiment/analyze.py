#!/usr/bin/env python3
"""Phase-2 metrics for the HOM experiment: SSHOM density, decoupled rate, and a
greedy net-FOM-reduction estimate."""
import json, sys, collections

fom_path, hom_path = sys.argv[1], sys.argv[2]

foms = [json.loads(l) for l in open(fom_path) if l.strip()]
killed_foms = [f for f in foms if f["status"] == "killed" and f["kill_set"]]
n_fom_total = len(foms)
n_fom_killed = len(killed_foms)

homs = [json.loads(l) for l in open(hom_path) if l.strip()]
cls = collections.Counter(h["class"] for h in homs)
n_run = len(homs)
sshoms = [h for h in homs if h["class"] == "sshom"]

print("=== pyjwt api_jws.py — HOM (2nd order) ===")
print(f"FOMs: {n_fom_total} total, {n_fom_killed} killed")
print(f"candidate pairs run: {n_run}")
for k in ("sshom", "decoupled", "non-subsuming"):
    pct = 100 * cls[k] / n_run if n_run else 0
    print(f"  {k:14} {cls[k]:5}  ({pct:.1f}%)")

# Greedy set-cover: pick SSHOMs that cover the most not-yet-covered FOMs.
# Each chosen SSHOM replaces its 2 constituent FOMs with 1 mutant.
edges = [(h["f1_id"], h["f2_id"]) for h in sshoms]
covered = set()
chosen = 0
remaining = edges[:]
while True:
    best, best_gain = None, 0
    for e in remaining:
        gain = len({e[0], e[1]} - covered)
        if gain > best_gain:
            best, best_gain = e, gain
    if not best or best_gain == 0:
        break
    covered |= {best[0], best[1]}
    chosen += 1
    remaining.remove(best)

foms_in_sshom = len({x for e in edges for x in e})
# new mutant count over the killed set = uncovered FOMs + chosen HOMs
new_count = (n_fom_killed - len(covered)) + chosen
reduction = (n_fom_killed - new_count) / n_fom_killed * 100 if n_fom_killed else 0

print(f"\nFOMs in >=1 SSHOM: {foms_in_sshom} / {n_fom_killed}")
print(f"greedy collapse: {len(covered)} FOMs -> {chosen} SSHOMs")
print(f"net FOM reduction (killed set): {reduction:.1f}%  "
      f"({n_fom_killed} -> {new_count})")

# Cost: extra HOM runs vs the baseline FOM run.
cost_ratio = (n_fom_total + n_run) / n_fom_total if n_fom_total else 0
print(f"\ncost: {n_run} HOM runs on top of {n_fom_total} FOM runs "
      f"= {cost_ratio:.1f}x the FOM pass")
