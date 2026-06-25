"""Benchmark orchestrator CLI.

Usage:
  fermut-bench run --scenario cold --tool fermut --repo more-itertools
  fermut-bench run --scenario warm --tool all --repo all
  fermut-bench report results/runs/

`--tool all` / `--repo all` expand against configs/{tools,repos}.toml.
Each (scenario, tool, repo) run writes a JSON file under
results/runs/<timestamp>-<scenario>-<tool>-<repo>.json. `report` aggregates
those JSON files into a single markdown table.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
import traceback
from pathlib import Path

from . import meta as meta_mod
from . import repos as repos_mod
from .config import load_repos, load_tools
from .scenarios import SCENARIOS

RESULTS_DIR = Path(__file__).resolve().parents[2] / "results" / "runs"


def _expand(value: str, choices: list[str]) -> list[str]:
    if value == "all":
        return choices
    return [v.strip() for v in value.split(",") if v.strip()]


def cmd_run(args: argparse.Namespace) -> int:
    repos = load_repos()
    tools = load_tools()
    scenarios = _expand(args.scenario, list(SCENARIOS))
    tool_names = _expand(args.tool, list(tools))
    repo_names = _expand(args.repo, list(repos))

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")

    total_cells = len(scenarios) * len(tool_names) * len(repo_names)
    print(
        f"plan: {len(scenarios)} scenarios x {len(tool_names)} tools x "
        f"{len(repo_names)} repos = {total_cells} runs",
        flush=True,
    )
    rc = 0
    idx = 0
    for scenario in scenarios:
        fn = SCENARIOS[scenario]
        for tool in tool_names:
            for repo in repo_names:
                idx += 1
                key = f"{stamp}-{scenario}-{tool}-{repo}"
                print(f"\n==> [{idx}/{total_cells}] {scenario} | {tool} | {repo}", flush=True)
                started_at = meta_mod.utc_now()
                cell_start = time.perf_counter()
                try:
                    out = fn(repos[repo], tools[tool], timeout=args.timeout)
                except Exception as exc:  # noqa: BLE001 — record + continue
                    out = {
                        "scenario": scenario,
                        "tool": tool,
                        "repo": repo,
                        "status": "error",
                        "error": str(exc),
                        "trace": traceback.format_exc(),
                    }
                    rc = 1
                    print(f"    !! ERROR: {exc}", flush=True)
                cell_dur = time.perf_counter() - cell_start
                # Provenance block — populated post-run so it reflects what
                # actually got built / cloned, not what was requested.
                worktree = repos_mod.worktree_for(tool, repos[repo])
                out["meta"] = {
                    "started_at": started_at,
                    "finished_at": meta_mod.utc_now(),
                    "duration_seconds": round(cell_dur, 3),
                    "tool_version": meta_mod.tool_version(tools[tool], repo),
                    "repo": meta_mod.repo_commit(repos[repo], worktree),
                }
                summary = (
                    out.get("status", "ok"),
                    out.get("total_seconds") or out.get("avg_seconds") or "-",
                    out.get("score") or out.get("final_score") or "-",
                    out.get("mutants") if out.get("mutants") is not None else "-",
                )
                print(
                    f"    summary: status={summary[0]} seconds={summary[1]} "
                    f"score={summary[2]} mutants={summary[3]} "
                    f"(cell took {cell_dur:.1f}s)",
                    flush=True,
                )
                (RESULTS_DIR / f"{key}.json").write_text(json.dumps(out, indent=2))
    print(f"\nall done. results in {RESULTS_DIR}", flush=True)
    return rc


def cmd_clear(args: argparse.Namespace) -> int:
    import shutil
    root = Path(__file__).resolve().parents[2]
    targets = [root / "results" / "runs"]
    if args.fixtures or args.all:
        targets += [p for p in (root / "fixtures").iterdir() if p.is_dir()] \
            if (root / "fixtures").exists() else []
    if args.all:
        targets.append(root / "fixtures" / ".wheels")
    for t in targets:
        if t.exists():
            shutil.rmtree(t)
            print(f"removed: {t.relative_to(root)}")
    (root / "results" / "runs").mkdir(parents=True, exist_ok=True)
    return 0


def cmd_report(args: argparse.Namespace) -> int:
    from tabulate import tabulate

    root = Path(args.dir) if args.dir else RESULTS_DIR

    def fmt_num(v, spec):
        return format(v, spec) if isinstance(v, (int, float)) else (v or "")

    rows = []
    for path in sorted(root.glob("*.json")):
        data = json.loads(path.read_text())
        meta = data.get("meta") or {}
        repo_meta = meta.get("repo") or {}
        head = (repo_meta.get("head") or "")[:7]
        ref = repo_meta.get("ref") or ""
        version = (meta.get("tool_version") or "").splitlines()[0]
        seconds = data.get("total_seconds") or data.get("avg_seconds")
        score = data.get("score") or data.get("final_score")
        rows.append([
            (meta.get("started_at") or "")[:16].replace("T", " "),
            data.get("scenario"),
            version,
            data.get("repo"),
            f"{ref}@{head}" if head else ref,
            data.get("status", "ok"),
            fmt_num(seconds, ".1f"),
            fmt_num(score, ".1f"),
            data.get("mutants") if data.get("mutants") is not None else "",
            fmt_num(data.get("delta"), "+.1f"),
        ])
    print(tabulate(
        rows,
        headers=[
            "started", "scenario", "version",
            "repo", "ref", "status", "secs", "score%", "mutants", "Δ",
        ],
        tablefmt="simple",
        colalign=("left", "left", "left", "left", "left", "left",
                  "right", "right", "right", "right"),
        disable_numparse=True,
    ))
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="fermut-bench")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_run = sub.add_parser("run", help="Run benchmark scenario(s)")
    p_run.add_argument("--scenario", default="cold",
                       help=f"comma-list or 'all' ({','.join(SCENARIOS)})")
    p_run.add_argument("--tool", default="fermut",
                       help="comma-list or 'all' (fermut,mutmut,cosmic-ray,poodle)")
    p_run.add_argument("--repo", default="more-itertools",
                       help="comma-list or 'all'")
    p_run.add_argument("--timeout", type=int, default=3600,
                       help="per-run hard timeout in seconds")
    p_run.set_defaults(fn=cmd_run)

    p_report = sub.add_parser("report", help="Aggregate result JSONs into a table")
    p_report.add_argument("--dir", help="results dir (default: results/runs)")
    p_report.set_defaults(fn=cmd_report)

    p_clear = sub.add_parser("clear", help="Wipe benchmark artifacts (results, fixtures, wheels)")
    p_clear.add_argument("--fixtures", action="store_true",
                         help="also wipe cloned repos + tool venvs")
    p_clear.add_argument("--all", action="store_true",
                         help="results + fixtures + built wheels")
    p_clear.set_defaults(fn=cmd_clear)

    args = parser.parse_args(argv)
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
