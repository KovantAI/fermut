#!/usr/bin/env python3
"""SSHOM stability across commits: Jaccard of the SSHOM set between successive
commits, under two identities:
  - loose  : content only (operator + orig->repl of each constituent), position-
             insensitive — survives reformatting / code-above shifts.
  - strict : content + line — position-sensitive (a lower bound on stability).
Also reports killed-FOM-set Jaccard (loose) as the ceiling: SSHOMs can't persist
more than their constituent mutants do."""
import json, sys, os

OUT = sys.argv[1]
SHAS = sys.argv[2:]  # chronological


def content(mid):
    # id = "path@offset:op:orig->repl" -> "op:orig->repl"
    try:
        return mid.split("@", 1)[1].split(":", 1)[1]
    except IndexError:
        return mid


def load(sha):
    hom = os.path.join(OUT, f"{sha}-hom.jsonl")
    fom = os.path.join(OUT, f"{sha}-fom.jsonl")
    sshom_loose, sshom_strict, foms = set(), set(), set()
    if os.path.exists(hom):
        for l in open(hom):
            r = json.loads(l)
            if r.get("class") != "sshom":
                continue
            c1, c2 = content(r["f1_id"]), content(r["f2_id"])
            sshom_loose.add(frozenset((c1, c2)))
            sshom_strict.add(frozenset(((c1, r["line1"]), (c2, r["line2"]))))
    if os.path.exists(fom):
        for l in open(fom):
            r = json.loads(l)
            if r.get("status") == "killed":
                foms.add(content(r["mutant_id"]))
    return sshom_loose, sshom_strict, foms


def jac(a, b):
    if not a and not b:
        return 1.0
    return len(a & b) / len(a | b)


print(f"{'transition':<20} {'fom_J':>6} {'sshom_loose_J':>13} {'sshom_strict_J':>14}  n_sshom")
prev = None
for sha in SHAS:
    cur = load(sha)
    n = len(cur[0])
    if prev is not None:
        pl, ps, pf = prev
        cl, cs, cf = cur
        print(f"{psha}->{sha:<12} {jac(pf,cf):>6.2f} {jac(pl,cl):>13.2f} {jac(ps,cs):>14.2f}  {n}")
    prev = cur
    psha = sha
