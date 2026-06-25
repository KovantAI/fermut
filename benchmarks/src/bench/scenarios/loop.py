"""Incremental-loop scenario.

Simulates the inner dev loop: small edit → re-run mutation testing → repeat.
Each iteration touches the configured loop_edit_file (append + revert), so
adapters that cache by file-AST hash (fermut, mutmut) invalidate only that
file and reuse cached results for the rest of the repo.

We require a prior cold run so caches exist; otherwise the first iteration
becomes a disguised cold run and skews the average.
"""
from __future__ import annotations

from .. import repos as repos_mod
from .. import venv as venv_mod
from ..adapters import build as build_adapter
from ..config import RepoCfg, ToolCfg
from ..timing import Timing, timed
from .cold import cold


def loop(
    repo: RepoCfg,
    tool: ToolCfg,
    *,
    iterations: int = 5,
    timeout: int | None = None,
) -> dict:
    worktree = repos_mod.worktree_for(tool.name, repo)
    venv = venv_mod.venv_for(tool.name, repo.name)
    if not worktree.exists() or not venv.exists():
        cold(repo, tool, timeout=timeout)

    venv = venv_mod.ensure(tool.name, repo.name, python=tool.python)
    adapter = build_adapter(tool.name, tool, venv)

    iteration_results: list[dict] = []
    for i in range(iterations):
        print(f"  -- loop iteration {i + 1}/{iterations}", flush=True)
        repos_mod.touch_loop_edit(worktree, repo)
        timings: list[Timing] = []
        with timed(f"iter_{i}", timings):
            result = adapter.run(repo, worktree, timeout=timeout)
        repos_mod.revert_loop_edit(worktree, repo)
        print(
            f"    -> score={result.score} rc={result.returncode} "
            f"({timings[0].seconds:.1f}s)",
            flush=True,
        )
        iteration_results.append({
            "iteration": i,
            "seconds": timings[0].seconds,
            "score": result.score,
            "mutants": result.mutants,
            "returncode": result.returncode,
        })

    seconds = [r["seconds"] for r in iteration_results]
    avg = sum(seconds) / len(seconds) if seconds else 0.0
    # All iterations mutate the same repo, so the count should be stable; we
    # surface the last iteration's value (the only one not racing with edits).
    last_mutants = iteration_results[-1]["mutants"] if iteration_results else None
    return {
        "scenario": "loop",
        "tool": tool.name,
        "repo": repo.name,
        "iterations": iteration_results,
        "avg_seconds": round(avg, 3),
        "min_seconds": round(min(seconds), 3) if seconds else 0.0,
        "max_seconds": round(max(seconds), 3) if seconds else 0.0,
        "mutants": last_mutants,
    }
