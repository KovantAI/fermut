"""Sharded scenario (fermut only).

Splits one cold mutation pass into N `--shard i/N` runs, then `fermut merge`s
their reports. In CI the shards run on separate hosts at once, so the wall
time a user waits on is the slowest shard plus the merge; here they run back
to back on one machine, each with every core, and each starts from an empty
cache as a fresh CI host would. The coverage DB is built once beforehand
(CI builds it in one job and hands it to the shards).
"""

from __future__ import annotations

import json
import shutil
import subprocess

from .. import repos as repos_mod
from .. import venv as venv_mod
from ..adapters import build as build_adapter
from ..config import RepoCfg, ToolCfg
from ..timing import Timing, timed
from .cold import cold

SHARDS = 4


def sharded(repo: RepoCfg, tool: ToolCfg, *, timeout: int | None = None) -> dict:
    if tool.name != "fermut":
        raise ValueError("the sharded scenario is fermut-only")
    worktree = repos_mod.worktree_for(tool.name, repo)
    venv = venv_mod.venv_for(tool.name, repo.name)
    if not worktree.exists() or not venv.exists():
        cold(repo, tool, timeout=timeout)
    venv = venv_mod.ensure(tool.name, repo.name, python=tool.python)
    adapter = build_adapter(tool.name, tool, venv)

    timings: list[Timing] = []
    with timed("coverage_prep", timings):
        adapter.prepare(repo, worktree)
    # Outside `.fermut/`, which each shard's cache wipe removes.
    out = worktree / ".bench-shards"
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir()
    reports, rcs = [], []
    for i in range(1, SHARDS + 1):
        adapter.clear_cache(worktree)  # a fresh CI host per shard
        name = f"shard-{i}.json"
        with timed(f"shard_{i}", timings):
            r = adapter.run(
                repo,
                worktree,
                timeout=timeout,
                extra_args=("--shard", f"{i}/{SHARDS}"),
                report_name=name,
            )
        rcs.append(r.returncode)
        dest = out / name
        shutil.move(worktree / ".fermut" / name, dest)
        reports.append(str(dest))
    merged = out / "merged.json"
    with timed("merge", timings):
        m = subprocess.run(
            [
                "fermut",
                "merge",
                "--expect-shards",
                str(SHARDS),
                "--json",
                str(merged),
                *reports,
            ],
            cwd=worktree,
            env=adapter.env(),
            capture_output=True,
            text=True,
            check=False,
        )
    summary = json.loads(merged.read_text())["summary"] if m.returncode == 0 else {}
    by = {t.label: t.seconds for t in timings}
    shard_secs = [by[f"shard_{i}"] for i in range(1, SHARDS + 1)]
    return {
        "scenario": "sharded",
        "tool": tool.name,
        "repo": repo.name,
        "phases": [t.to_dict() for t in timings],
        "score": summary.get("mutation_score"),
        "mutants": summary.get("total"),
        "returncode": m.returncode or max(rcs),
        "merge_stderr": m.stderr[-2000:] if m.returncode else None,
        "shards": SHARDS,
        # Shards in parallel on N hosts: the slowest one plus the merge.
        "wall_seconds": round(max(shard_secs) + by["merge"], 3),
        # Machine time across all hosts.
        "total_seconds": round(sum(shard_secs) + by["merge"], 3),
    }
