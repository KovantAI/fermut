#!/usr/bin/env python3
"""Map the operator-catalogue gap between mutmut and fermut on the same repo.

For every mutmut mutation we classify its before->after token change into a
mutation *kind* (data-driven, not from either tool's docs), then measure how
well fermut covers that kind:

  - exact: fermut generated the identical mutation (same file + before/after
    source line).
  - on_line: fermut mutated the same source line at all (maybe a different op).
  - none: fermut produced nothing on that line.

Kinds with many mutmut mutations but near-zero fermut exact-coverage are the
operators fermut is effectively missing (or expresses too differently to
align). The reverse direction (fermut operators with no mutmut analog) is
tabulated from fermut's own operator names.

Usage: map_operators.py <fermut-report.json> <mutmut-extract.json> <out.md>
"""
from __future__ import annotations

import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from compare import load_fermut, norm, rel  # noqa: E402

COMPARISON = {"==", "!=", "<", ">", "<=", ">=", "<>"}
ARITH = {"+", "-", "*", "/", "//", "%", "**", "&", "|", "^", "<<", ">>", "~", "@"}
AUGMENTED = {f"{op}=" for op in ARITH}
BOOLOP = {"and", "or"}
NUM_RE = re.compile(r"^-?\d[\d_]*\.?[\d_]*(e-?\d+)?j?$", re.I)
STR_RE = re.compile(r'^[rbuf]*(["\']).*\1$', re.I | re.S)


def _is_bool_flip(bl: str, al: str) -> bool:
    """True if the line change is purely a True<->False flip (robust to the
    minimal-token trimmer mangling `True`/`False` into `Tru`/`Fals`)."""
    return bl.replace("True", "False") == al or bl.replace("False", "True") == al


def classify(before: str, after: str, before_line: str = "", after_line: str = "") -> str:
    b, a = before.strip(), after.strip()
    pair = {b, a}
    # mutmut's string operator wraps the literal in XX...XX (same convention as
    # fermut's string-sentinel) — detect before the generic string rules.
    if a == f"XX{b}XX" or b == f"XX{a}XX":
        return "string-sentinel (XX-wrap)"
    # mutmut also upper/lower-cases string literals — fermut has no analog.
    if b and a and b != a:
        if a == b.upper() and a != b.lower():
            return "string case-swap (UPPER)"
        if a == b.lower() and a != b.upper():
            return "string case-swap (lower)"
    if pair == {"True", "False"} or _is_bool_flip(before_line, after_line):
        return "bool-literal (True/False)"
    if b in BOOLOP and a in BOOLOP:
        return "bool-op (and/or)"
    if b in COMPARISON and a in COMPARISON:
        return "comparison-op"
    if b in ARITH and a in ARITH:
        return "arith/bit-op"
    if b in AUGMENTED or a in AUGMENTED:
        return "augmented-assign"
    if pair == {"in", "not in"} or pair == {"is", "is not"}:
        return "membership/identity"
    if a == "None" and b != "None":
        return "replace-with-None"
    if b == "None" and a != "None":
        return "None-replaced"
    if a == "" or b == "":
        return "deletion (arg/element)"
    if STR_RE.match(b):
        return "string-mutate"
    if NUM_RE.match(b) and (NUM_RE.match(a) or a in ("", "0", "1")):
        return "number-mutate"
    if {b, a} & {"break", "continue"}:
        return "break/continue"
    if b in ("<", ">", "<=", ">=") or a in ("<", ">", "<=", ">="):
        return "comparison-op"
    return f"other ({b!r}->{a!r})"


def load_mutmut_raw(path: Path) -> list[dict]:
    out = []
    for r in json.loads(path.read_text()):
        if r.get("parse_failed"):
            continue
        out.append({
            "file": rel(r["file"]),
            "before_line": norm(r["before_line"]),
            "after_line": norm(r["after_line"]),
            "before": r["before"],
            "after": r["after"],
            "status": r["status"],
        })
    return out


def main() -> None:
    fermut = load_fermut(Path(sys.argv[1]))
    mutmut = load_mutmut_raw(Path(sys.argv[2]))
    out = Path(sys.argv[3])

    f_exact = {(r["file"], r["before_line"], r["after_line"]) for r in fermut}
    f_lines = {(r["file"], r["before_line"]) for r in fermut}

    by_kind = defaultdict(lambda: {"total": 0, "exact": 0, "on_line": 0,
                                   "none": 0, "examples": []})
    for r in mutmut:
        k = classify(r["before"], r["after"], r["before_line"], r["after_line"])
        e = by_kind[k]
        e["total"] += 1
        if (r["file"], r["before_line"], r["after_line"]) in f_exact:
            e["exact"] += 1
        elif (r["file"], r["before_line"]) in f_lines:
            e["on_line"] += 1
        else:
            e["none"] += 1
            if len(e["examples"]) < 3:
                e["examples"].append(f"{r['before']!r}->{r['after']!r} @ {r['file']}")

    L = ["# Operator-catalogue gap: mutmut → fermut (pyjwt)\n"]
    L.append("Each mutmut mutation classified by its before→after change, then "
             "checked against fermut: **exact** = fermut made the identical "
             "mutation; **on_line** = fermut mutated that line differently; "
             "**none** = fermut produced nothing there.\n")
    L.append("| mutmut kind | total | fermut exact | on-line | none | exact% |")
    L.append("|---|--:|--:|--:|--:|--:|")
    for k in sorted(by_kind, key=lambda x: -by_kind[x]["total"]):
        e = by_kind[k]
        pct = 100 * e["exact"] // max(1, e["total"])
        L.append(f"| {k} | {e['total']} | {e['exact']} | {e['on_line']} | "
                 f"{e['none']} | {pct}% |")
    L.append("")

    # Missing-operator candidates: kinds where fermut never produced an exact
    # match (and rarely even touched the line).
    L.append("## Operators fermut is missing or under-covers\n")
    missing = [(k, e) for k, e in by_kind.items()
               if e["exact"] == 0 and e["total"] >= 3]
    if not missing:
        L.append("None — every mutmut kind with >=3 instances has at least one "
                 "exact fermut equivalent.")
    for k, e in sorted(missing, key=lambda x: -x[1]["total"]):
        L.append(f"- **{k}** — {e['total']} mutmut mutations, 0 fermut exact "
                 f"({e['on_line']} same-line different-op, {e['none']} untouched). "
                 f"e.g. {'; '.join(e['examples'][:2])}")
    L.append("")

    # Synthesis: separate TRUE coverage holes (fermut mutates nothing on the
    # line) from redundantly-covered kinds (fermut hits the line via another op,
    # so detection is likely retained even without an exact-match operator).
    L.append("## Synthesis\n")
    total_none = sum(e["none"] for e in by_kind.values())
    L.append(f"- **True coverage holes** (mutmut mutates a line fermut leaves "
             f"untouched): {total_none} mutmut mutations across all kinds. "
             f"Still dominated by `replace-with-None`: fermut's None ops now cover "
             f"assignments, returns, and call arguments (`arg-to-none`), plus "
             f"`none-to-value` for the reverse, but NOT whole-call results, bare "
             f"attribute/name reads, or subscripts replaced with None.")
    L.append("- **Redundantly covered** (fermut mutates the same line with a "
             "*different* operator, so the line is still exercised): "
             f"{sum(e['on_line'] for e in by_kind.values())} mutations. The big "
             "one is `string case-swap` (UPPER/lower) — fermut has no case-swap, "
             "but its `string-to-empty` + `string-sentinel` hit the same string "
             "literals, so detection is largely retained.\n")
    L.append("**Operators fermut genuinely lacks (catalogue gaps):**")
    L.append("1. `expression → None` beyond call arguments — `arg-to-none` now "
             "covers call args, but mutmut also replaces whole-call results, "
             "attribute/name reads, and subscripts with None (the residual "
             "`replace-with-None` holes).")
    L.append("2. `string case-swap` (UPPER / lower) — no fermut analog "
             "(low marginal detection value; string-sentinel overlaps).\n")
    L.append("**Operators fermut has that mutmut lacks (fermut advantages):** "
             "`not-insertion` (150 — wraps booleans in `not(...)`), "
             "`string-to-empty`, `boundary-shift`, `unary-op-swap`, "
             "`slice-bound-drop`, `bytes-sentinel`.\n")

    # fermut operator inventory (what fermut brings).
    L.append("## fermut operator inventory (executed mutants)\n")
    fc = Counter(r["operator"] for r in fermut)
    L.append("| fermut operator | count |")
    L.append("|---|--:|")
    for op, n in fc.most_common():
        L.append(f"| {op} | {n} |")

    out.write_text("\n".join(L))
    print("\n".join(L))
    print(f"\n-> {out}")


if __name__ == "__main__":
    main()
