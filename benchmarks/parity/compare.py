#!/usr/bin/env python3
"""Compare fermut vs mutmut detection on the same repo.

Answers the migration-risk question — "if I switch from mutmut to fermut, do I
lose detection?" — at two granularities:

  1. Exact-mutation overlap: mutations BOTH tools generate (same file, line,
     and before->after token change). On that shared set, do the kill/survive
     verdicts agree? Disagreements where mutmut killed but fermut survived are
     the real detection-loss candidates.

  2. Line-level detection: a coarser but catalogue-independent view. For each
     source line, did each tool detect it (kill >=1 mutant there)? Lines mutmut
     detects but fermut does not are the migration risk; the reverse is a
     fermut advantage.

Exact-mutation alignment is necessarily a SUBSET — the tools ship different
operator catalogues, so most mutations exist in only one tool. That's reported,
not hidden. There is no single "superset" number; this prints the buckets that
actually bear on the decision.

Usage: compare.py <fermut-report.json> <mutmut-extract.json> <out.md>
"""
from __future__ import annotations

import json
import os
import re
import sys
from collections import defaultdict
from pathlib import Path

WS = re.compile(r"\s+")


def norm(s: str) -> str:
    return WS.sub(" ", s).strip()


def rel(path: str) -> str:
    """Normalize a file path to start at the package dir (jwt/...)."""
    p = path.replace("\\", "/")
    i = p.find("/jwt/")
    if i != -1:
        return p[i + 1:]
    return p.lstrip("./")


_SRC_CACHE: dict[str, str] = {}


def _src(path: str) -> str:
    if path not in _SRC_CACHE:
        _SRC_CACHE[path] = Path(path).read_text()
    return _SRC_CACHE[path]


def _line_at(text: str, offset: int) -> tuple[int, int, str]:
    """Return (line_start_offset, line_end_offset, line_text) for byte offset."""
    start = text.rfind("\n", 0, offset) + 1
    end = text.find("\n", offset)
    if end == -1:
        end = len(text)
    return start, end, text[start:end]


def load_fermut(path: Path, include_skipped: bool = False, root: Path | None = None) -> list[dict]:
    """Reconstruct each mutant's full before/after SOURCE LINE from its byte
    range, so matching is line-number-independent (mutmut's reported line
    numbers are shifted by its trampoline injection; line *content* is not).

    `include_skipped=False` (default) keeps only executed (killed/survived)
    mutants — correct for the DETECTION comparison. Set True to include
    coverage/ty-filtered `skipped` mutants too — correct for the CATALOGUE
    (generation) comparison, i.e. "does fermut produce this mutation at all,
    regardless of whether the coverage filter ran it?"."""
    data = json.loads(path.read_text())
    out = []
    for o in data["outcomes"]:
        st = o["status"]
        if not include_skipped and st not in ("killed", "survived"):
            continue  # drop skipped/equivalent/errored/timeout
        m = o["mutant"]
        try:
            text = _src(m["file"])
            start, end = m["range"]
            ls, le, before_line = _line_at(text, start)
            # Splice the replacement into the line at the mutant's columns.
            after_line = text[ls:start] + m["replacement"] + text[end:le]
        except (OSError, KeyError, IndexError):
            continue
        # Repo-relative path when `root` given (works for any package layout);
        # else the legacy `/jwt/` heuristic.
        if root is not None:
            try:
                fkey = os.path.relpath(m["file"], root).replace("\\", "/")
            except ValueError:
                fkey = rel(m["file"])
        else:
            fkey = rel(m["file"])
        out.append({
            "file": fkey,
            "before_line": norm(before_line),
            "after_line": norm(after_line),
            "operator": m["operator"],
            "status": st,
        })
    return out


def load_mutmut(path: Path) -> list[dict]:
    data = json.loads(path.read_text())
    out = []
    for r in data:
        if r.get("parse_failed"):
            continue
        out.append({
            "file": rel(r["file"]),
            "before_line": norm(r["before_line"]),
            "after_line": norm(r["after_line"]),
            "status": r["status"],
        })
    return out


def mkey(r: dict) -> tuple:
    """Exact mutation identity, line-number-independent: file + the full
    original line + the full mutated line."""
    return (r["file"], r["before_line"], r["after_line"])


def lkey(r: dict) -> tuple:
    """Line identity by content (line numbers don't align across tools)."""
    return (r["file"], r["before_line"])


def line_detection(records: list[dict]) -> dict[tuple, bool]:
    """(file,line) -> True if the tool KILLED at least one mutant there."""
    killed = defaultdict(bool)
    for r in records:
        killed[lkey(r)] |= (r["status"] == "killed")
    return dict(killed)


def main() -> None:
    fermut = load_fermut(Path(sys.argv[1]))
    mutmut = load_mutmut(Path(sys.argv[2]))
    out = Path(sys.argv[3])

    def counts(recs):
        k = sum(1 for r in recs if r["status"] == "killed")
        return len(recs), k, len(recs) - k

    f_tot, f_k, f_s = counts(fermut)
    m_tot, m_k, m_s = counts(mutmut)

    # --- 1. Exact-mutation overlap ---
    f_by = {mkey(r): r for r in fermut}
    m_by = {mkey(r): r for r in mutmut}
    shared = set(f_by) & set(m_by)
    both_killed = both_survived = 0
    mutmut_killed_fermut_survived = []   # detection-loss candidates
    fermut_killed_mutmut_survived = []   # fermut advantage
    for key in shared:
        fs, ms = f_by[key]["status"], m_by[key]["status"]
        if fs == ms == "killed":
            both_killed += 1
        elif fs == ms == "survived":
            both_survived += 1
        elif ms == "killed" and fs == "survived":
            mutmut_killed_fermut_survived.append(key)
        else:
            fermut_killed_mutmut_survived.append(key)

    # --- 2. Line-level detection ---
    f_line = line_detection(fermut)
    m_line = line_detection(mutmut)
    f_lines = set(f_line)
    m_lines = set(m_line)
    common_lines = f_lines & m_lines
    # Of lines mutmut DETECTS (killed >=1), how many does fermut also detect?
    m_detected = {ln for ln in m_lines if m_line[ln]}
    f_detected = {ln for ln in f_lines if f_line[ln]}
    m_detected_common = {ln for ln in m_detected if ln in f_lines}
    retained = {ln for ln in m_detected_common if f_line[ln]}
    gap_lines = sorted(m_detected_common - retained)  # mutmut detects, fermut misses
    # reverse: fermut detects, mutmut misses (on common lines)
    f_detected_common = {ln for ln in f_detected if ln in m_lines}
    adv_lines = sorted({ln for ln in f_detected_common if not m_line[ln]})

    L = []
    L.append("# fermut vs mutmut — detection parity on pyjwt\n")
    L.append(f"- Repo: **pyjwt 2.13.0**, package `jwt/`, same test suite.")
    L.append(f"- fermut (executed, coverage-filtered): **{f_tot}** mutants — "
             f"{f_k} killed / {f_s} survived.")
    L.append(f"- mutmut (executed, no_tests excluded): **{m_tot}** mutants — "
             f"{m_k} killed / {m_s} survived.\n")
    L.append("> The two mutant totals differ because the operator catalogues "
             "differ — neither set is a subset of the other. A single "
             "\"superset\" number isn't well-defined; the buckets below are.\n")

    L.append("## 1. Exact-mutation overlap (same file+line+token change)\n")
    L.append(f"- Mutations generated by **both** tools: **{len(shared)}** "
             f"(of {f_tot} fermut / {m_tot} mutmut executed).")
    L.append(f"  - both killed: {both_killed}")
    L.append(f"  - both survived: {both_survived}")
    L.append(f"  - **mutmut killed, fermut SURVIVED (detection-loss candidates): "
             f"{len(mutmut_killed_fermut_survived)}**")
    L.append(f"  - fermut killed, mutmut survived (fermut advantage): "
             f"{len(fermut_killed_mutmut_survived)}\n")
    if mutmut_killed_fermut_survived:
        L.append("### Detection-loss candidates (inspect these)\n")
        for k in sorted(mutmut_killed_fermut_survived)[:50]:
            f, before, after = k
            L.append(f"- `{f}` op `{f_by[k]['operator']}`: "
                     f"`{before}` → `{after}`")
        L.append("")

    L.append("## 2. Line-level detection (catalogue-independent)\n")
    L.append(f"- Source lines mutmut detects (kills >=1 mutant): {len(m_detected)}")
    L.append(f"- Source lines fermut detects: {len(f_detected)}")
    L.append(f"- Of mutmut-detected lines that fermut **also mutates** "
             f"({len(m_detected_common)}), fermut also detects "
             f"**{len(retained)}** "
             f"({100*len(retained)//max(1,len(m_detected_common))}%).")
    L.append(f"- **Lines mutmut detects but fermut misses (has only survivors): "
             f"{len(gap_lines)}**")
    L.append(f"- Lines fermut detects but mutmut misses (fermut advantage): "
             f"{len(adv_lines)}\n")
    if gap_lines:
        L.append("### Lines mutmut detects, fermut does not (inspect)\n")
        for f, before in gap_lines[:50]:
            ops = sorted({r["operator"] for r in fermut
                          if r["file"] == f and r["before_line"] == before})
            L.append(f"- `{f}` — `{before}` — fermut survivors only; "
                     f"ops: {', '.join(ops)}")
        L.append("")

    L.append("## Verdict\n")
    if not mutmut_killed_fermut_survived and not gap_lines:
        L.append("On the mutations and lines the two tools share, fermut detects "
                 "everything mutmut detects — no detection loss found on pyjwt.")
    else:
        L.append(f"**Not a clean superset.** On the {len(shared)} mutations both "
                 f"tools generate, fermut matches mutmut's verdict on "
                 f"{both_killed + both_survived} and diverges on "
                 f"{len(mutmut_killed_fermut_survived)} where mutmut kills but "
                 f"fermut survives. At line granularity fermut retains "
                 f"{len(retained)}/{len(m_detected_common)} "
                 f"({100*len(retained)//max(1,len(m_detected_common))}%) of the "
                 f"lines mutmut detects; {len(gap_lines)} gaps remain.\n")
        L.append("Most line-level gaps are `keyword-arg-drop` / warning-`stacklevel` "
                 "noise (equivalent or near-equivalent). But the "
                 "`\"verify_exp\"/\"verify_nbf\"/\"verify_signature\": True→False` "
                 "default flips are security-relevant and **reproducible**:\n")
        L.append("- The full pyjwt suite KILLS `verify_exp: True→False` "
                 "(`test_decode_with_expiration` + 2 others fail).")
        L.append("- fermut's full run reports it SURVIVED — while killing the "
                 "adjacent `verify_iat` flip, so mirror/import resolution is fine.")
        L.append("- Re-running fermut's OWN coverage-selected test set for that "
                 "line under the same mutation also KILLS it.")
        L.append("\nSo fermut's live run produced a false survivor its own "
                 "selection should have caught — a candidate test-selection bug, "
                 "not an equivalence artifact. Worth filing + a deeper dive.")
    out.write_text("\n".join(L))
    print("\n".join(L[:40]))
    print(f"\n... full report -> {out}")


if __name__ == "__main__":
    main()
