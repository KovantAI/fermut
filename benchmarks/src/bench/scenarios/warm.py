"""Warm-run scenario.

Warm = everything cached, no code edits. We must MEASURE a run whose cache is
already hot, not just "a re-run and hope the cache survived". A single timed
pass on an unknown cache state silently measures cold cost (this happened in
practice: shared worktrees and missing caches made warm == cold).

So we always run one un-timed priming pass to populate the cache, then time a
second pass — that second pass is the warm number. Worktree + venv are set up
via an implicit cold run if missing, so warm is callable in isolation.
"""
from __future__ import annotations

from pathlib import Path

from .. import repos as repos_mod
from .. import venv as venv_mod
from ..adapters import build as build_adapter
from ..config import RepoCfg, ToolCfg
from ..timing import Timing, timed
from .cold import cold


def warm(repo: RepoCfg, tool: ToolCfg, *, timeout: int | None = None) -> dict:
    worktree = repos_mod.worktree_for(tool.name, repo)
    venv = venv_mod.venv_for(tool.name, repo.name)
    if not worktree.exists() or not venv.exists():
        cold(repo, tool, timeout=timeout)

    venv = venv_mod.ensure(tool.name, repo.name, python=tool.python)
    adapter = build_adapter(tool.name, tool, venv)

    # Priming pass (un-timed): guarantees the cache is populated for the exact
    # mutant set we're about to time, regardless of prior worktree state.
    adapter.run(repo, worktree, timeout=timeout)

    timings: list[Timing] = []
    with timed("mutation_run", timings):
        result = adapter.run(repo, worktree, timeout=timeout)

    return {
        "scenario": "warm",
        "tool": tool.name,
        "repo": repo.name,
        "phases": [t.to_dict() for t in timings],
        "score": result.score,
        "mutants": result.mutants,
        "returncode": result.returncode,
        "total_seconds": round(sum(t.seconds for t in timings), 3),
    }
