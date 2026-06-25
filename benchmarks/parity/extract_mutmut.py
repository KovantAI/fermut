#!/usr/bin/env python3
"""Extract per-mutant data from a completed mutmut 3.x run.

For each mutant: status (killed/survived/...), source file, original 1-based
line number, and the (before_token -> after_token) minimal change derived from
mutmut's unified diff. Output is a JSON list, cached so the slow `mutmut show`
fan-out runs once.

Usage:
    extract_mutmut.py <mutmut-worktree> <venv-bin> <out.json>
"""
from __future__ import annotations

import json
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

RESULT_RE = re.compile(r"^\s*(\S+__mutmut_\d+):\s*(\w+)\s*$")
HUNK_RE = re.compile(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@")


def list_results(worktree: Path, mutmut: str) -> dict[str, str]:
    """id -> status for ALL mutants, from the per-file `.meta` caches.

    `mutmut results` prints only survivors; the `exit_code_by_key` maps in
    `mutants/**/*.py.meta` carry every executed mutant. Exit codes seen on
    pyjwt: 0 = suite passed → SURVIVED, 1 = a test failed → KILLED, 33 =
    mutmut's `no_tests` bucket (no covering test). We drop `no_tests` — it's
    the analogue of fermut's coverage-filtered `skipped`, so excluding it keeps
    the comparison to mutants both tools actually executed.
    """
    code_to_status = {0: "survived", 1: "killed"}
    res: dict[str, str] = {}
    for meta in (worktree / "mutants").rglob("*.py.meta"):
        try:
            data = json.loads(meta.read_text())
        except (OSError, ValueError):
            continue
        for mid, code in data.get("exit_code_by_key", {}).items():
            status = code_to_status.get(code)
            if status is not None:  # skip no_tests (33) and any other code
                res[mid] = status
    return res


def minimal_token_change(before: str, after: str) -> tuple[str, str]:
    """Strip the common prefix/suffix; return the differing middle of each."""
    b, a = before, after
    # common prefix
    i = 0
    while i < len(b) and i < len(a) and b[i] == a[i]:
        i += 1
    # common suffix
    j = 0
    while (
        j < len(b) - i
        and j < len(a) - i
        and b[len(b) - 1 - j] == a[len(a) - 1 - j]
    ):
        j += 1
    return b[i:len(b) - j].strip(), a[i:len(a) - j].strip()


def parse_show(text: str) -> dict | None:
    """Parse `mutmut show <id>` unified diff into file/line/before/after.

    Handles the common single-line-change mutant. Multi-line hunks fall back
    to the first changed line, which is what mutmut mutations always touch.
    """
    file_path = None
    hunk_old = None
    consumed_old = 0  # lines of old-file consumed within the hunk so far
    before_line = after_line = None
    before_lineno = None
    for line in text.splitlines():
        if line.startswith("--- "):
            file_path = line[4:].strip()
            continue
        if line.startswith("+++ "):
            continue
        h = HUNK_RE.match(line)
        if h:
            hunk_old = int(h.group(1))
            consumed_old = 0
            continue
        if hunk_old is None:
            continue
        if line.startswith("-") and not line.startswith("---"):
            if before_line is None:
                before_line = line[1:]
                before_lineno = hunk_old + consumed_old
            consumed_old += 1
        elif line.startswith("+") and not line.startswith("+++"):
            if after_line is None:
                after_line = line[1:]
        else:  # context line
            consumed_old += 1
    if file_path is None or before_line is None or after_line is None:
        return None
    before_tok, after_tok = minimal_token_change(before_line, after_line)
    return {
        "file": file_path,
        "line": before_lineno,
        "before_line": before_line.strip(),
        "after_line": after_line.strip(),
        "before": before_tok,
        "after": after_tok,
    }


def show_one(args) -> dict | None:
    mid, status, worktree, mutmut = args
    text = subprocess.run(
        [mutmut, "show", mid], cwd=worktree, capture_output=True, text=True
    ).stdout
    parsed = parse_show(text)
    if parsed is None:
        return {"id": mid, "status": status, "parse_failed": True}
    parsed.update(id=mid, status=status)
    return parsed


def main() -> None:
    worktree = Path(sys.argv[1])
    venv_bin = Path(sys.argv[2])
    out = Path(sys.argv[3])
    mutmut = str(venv_bin / "mutmut")

    results = list_results(worktree, mutmut)
    print(f"mutmut: {len(results)} mutants, fanning out `mutmut show`...")
    tasks = [(mid, st, worktree, mutmut) for mid, st in results.items()]
    records = []
    with ThreadPoolExecutor(max_workers=16) as ex:
        for i, rec in enumerate(ex.map(show_one, tasks)):
            if rec:
                records.append(rec)
            if (i + 1) % 200 == 0:
                print(f"  {i + 1}/{len(tasks)}")
    failed = sum(1 for r in records if r.get("parse_failed"))
    out.write_text(json.dumps(records, indent=1))
    print(f"wrote {len(records)} records ({failed} parse-failed) -> {out}")


if __name__ == "__main__":
    main()
