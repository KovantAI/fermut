#!/usr/bin/env python3
"""Cross-repo fermut↔mutmut catalogue-gap report.

For each benchmark repo with an existing mutmut run, generate fermut's mutants
(with current operators), extract mutmut's, and measure the CATALOGUE gap at
GENERATION level: for every mutmut mutation, does fermut generate ANY mutation
on that source line (`covered`) or nothing at all (`true hole`)? Execution /
coverage filtering is deliberately ignored here — this answers "what mutation
kinds does fermut's catalogue miss", not "what ran".

Also reports detection-loss (executed both, mutmut killed / fermut survived).

Usage: parity_all.py [--reuse]   (--reuse skips regenerating existing JSON)
"""
from __future__ import annotations

import collections
import json
import os
import pathlib
import subprocess
import sys
import tomllib

HERE = pathlib.Path(__file__).parent
sys.path.insert(0, str(HERE))
from compare import load_fermut, norm, rel  # noqa: E402
from map_operators import classify  # noqa: E402

ROOT = pathlib.Path("/Users/drorasaf/code/fermut")
BENCH = ROOT / "benchmarks"
FIX = BENCH / "fixtures"
VENVS = FIX / ".venvs"
FERMUT = ROOT / "target" / "debug" / "fermut"
REUSE = "--reuse" in sys.argv
REPOS = ["pyjwt", "starlette", "markupsafe", "more-itertools", "click", "typer"]
CFG = tomllib.load(open(BENCH / "configs" / "repos.toml", "rb"))


def gen_fermut(repo: str) -> pathlib.Path | None:
    out = HERE / f"fermut-{repo}.json"
    if out.exists() and REUSE:
        return out
    c = CFG[repo]
    wt = FIX / f"fermut-{repo}"
    venv = VENVS / f"fermut-{repo}"
    cov = wt / "coverage.json"
    if not cov.exists():
        print(f"  [{repo}] no coverage.json — skip")
        return None
    excl = c.get("fermut_exclude")
    if excl:
        (wt / "fermut.toml").write_text(
            "exclude = [\n" + "".join(f'  "{p}",\n' for p in excl) + "]\n"
        )
    argv = [str(FERMUT), "run", c["src_path"], "--tests", c["test_path"],
            "--coverage", str(cov.resolve()), "--no-verify-baseline",
            "--no-history", "--no-cache", "--json", str(out), "-q"]
    for ex in c.get("pytest_extra_args", []):
        argv.append(f"--pytest-arg={ex}")
    env = {**os.environ, "PATH": f"{venv}/bin:{os.environ['PATH']}"}
    r = subprocess.run(argv, cwd=wt, env=env, capture_output=True, text=True)
    if not out.exists():
        print(f"  [{repo}] fermut failed: {r.stderr[-300:]}")
        return None
    return out


def gen_mutmut(repo: str) -> pathlib.Path | None:
    out = HERE / f"mutmut-{repo}.json"
    if out.exists() and REUSE:
        return out
    wt = FIX / f"mutmut-{repo}"
    venvbin = VENVS / f"mutmut-{repo}" / "bin"
    if not (wt / "mutants").exists():
        print(f"  [{repo}] no mutmut run — skip")
        return None
    subprocess.run([sys.executable, str(HERE / "extract_mutmut.py"),
                    str(wt), str(venvbin), str(out)],
                   capture_output=True, text=True)
    return out if out.exists() else None


def _mrel(p: str) -> str:
    """mutmut diff paths are already repo-root-relative — just normalize."""
    return p.replace("\\", "/").lstrip("./") if p.startswith("./") else p.replace("\\", "/")


def analyze(repo: str, fpath: pathlib.Path, mpath: pathlib.Path) -> dict:
    # Both sides keyed on REPO-ROOT-RELATIVE paths so any package layout aligns.
    froot = FIX / f"fermut-{repo}"
    # Generation-level fermut line set (include coverage-skipped mutants).
    gen = load_fermut(fpath, include_skipped=True, root=froot)
    gen_lines = {(r["file"], r["before_line"]) for r in gen}
    # Executed fermut for detection comparison.
    ex = load_fermut(fpath, include_skipped=False, root=froot)
    f_exact_exec = {(r["file"], r["before_line"], r["after_line"]): r for r in ex}

    mut = [r for r in json.loads(mpath.read_text()) if not r.get("parse_failed")]
    by_kind = collections.defaultdict(lambda: [0, 0])  # total, true_hole
    detection_loss = 0
    shared = 0
    for r in mut:
        k = classify(r["before"], r["after"], r["before_line"], r["after_line"])
        by_kind[k][0] += 1
        key_line = (_mrel(r["file"]), norm(r["before_line"]))
        if key_line not in gen_lines:
            by_kind[k][1] += 1
        # detection: exact match among executed mutants
        key_ex = (_mrel(r["file"]), norm(r["before_line"]), norm(r["after_line"]))
        if key_ex in f_exact_exec:
            shared += 1
            if r["status"] == "killed" and f_exact_exec[key_ex]["status"] == "survived":
                detection_loss += 1
    return {
        "fermut_gen": len(gen),
        "mutmut_total": len(mut),
        "by_kind": {k: v for k, v in by_kind.items()},
        "true_holes": sum(v[1] for v in by_kind.values()),
        "shared_exec": shared,
        "detection_loss": detection_loss,
    }


def main() -> None:
    results = {}
    for repo in REPOS:
        print(f"[{repo}] generating…")
        f = gen_fermut(repo)
        m = gen_mutmut(repo)
        if f and m:
            results[repo] = analyze(repo, f, m)

    # Cross-repo summary.
    print("\n## Per-repo summary (generation-level catalogue gap)\n")
    print(f"{'repo':<16}{'fermut_gen':>11}{'mutmut':>8}{'true_holes':>11}"
          f"{'hole%':>7}{'det_loss':>9}")
    agg_kind = collections.defaultdict(lambda: [0, 0])
    for repo, r in results.items():
        hp = 100 * r["true_holes"] // max(1, r["mutmut_total"])
        print(f"{repo:<16}{r['fermut_gen']:>11}{r['mutmut_total']:>8}"
              f"{r['true_holes']:>11}{hp:>6}%{r['detection_loss']:>9}")
        for k, (t, h) in r["by_kind"].items():
            agg_kind[k][0] += t
            agg_kind[k][1] += h

    print("\n## Aggregate true catalogue holes by kind (all repos)\n")
    print(f"{'total':>7}{'hole':>7}  kind")
    for k, (t, h) in sorted(agg_kind.items(), key=lambda x: -x[1][1]):
        if h:
            print(f"{t:>7}{h:>7}  {k}")
    out = HERE / "parity_all.json"
    out.write_text(json.dumps(results, indent=1))
    print(f"\n-> {out}")


if __name__ == "__main__":
    main()
