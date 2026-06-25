"""Cold-run scenario.

Cold = nothing cached. We wipe the worktree, re-clone, install repo deps,
install the tool (in a fresh venv), then run one full mutation pass. Each
sub-phase is timed individually so the breakdown is visible.
"""
from __future__ import annotations

from pathlib import Path

from .. import repos as repos_mod
from .. import venv as venv_mod
from ..adapters import build as build_adapter
from ..config import RepoCfg, ToolCfg


def cold(repo: RepoCfg, tool: ToolCfg, *, timeout: int | None = None) -> dict:
    """Run the cold scenario. Returns a JSON-serializable dict."""
    phases: list[dict] = []

    # Phase 1: clone (fresh).
    from ..timing import Timing, timed
    timings: list[Timing] = []
    repos_mod.clean(tool.name, repo)
    with timed("clone", timings):
        worktree = repos_mod.clone(tool.name, repo)

    # Phase 2: tool venv + install (always fresh — that's what makes it cold).
    venv = venv_mod.ensure(tool.name, repo.name, fresh=True, python=tool.python)
    adapter = build_adapter(tool.name, tool, venv)
    with timed("tool_install", timings):
        adapter.install()

    # Phase 3: repo install (inside same venv).
    with timed("repo_install", timings):
        repos_mod.install(repo, worktree, adapter.python)

    # Phase 4: one-time setup that is NOT the mutation phase (fermut's
    # baseline coverage gen + pytest-cov install). Default adapters no-op.
    # Keeping this out of `mutation_run` makes that column the honest
    # mutation-phase-only time the report claims.
    with timed("coverage_prep", timings):
        adapter.prepare(repo, worktree)

    # Phase 5: cold mutation run. Cache wipe is a no-op on a fresh clone but
    # kept for symmetry with warm/loop scenarios.
    adapter.clear_cache(worktree)
    with timed("mutation_run", timings):
        result = adapter.run(repo, worktree, timeout=timeout)

    return {
        "scenario": "cold",
        "tool": tool.name,
        "repo": repo.name,
        "phases": [t.to_dict() for t in timings],
        "score": result.score,
        "mutants": result.mutants,
        "returncode": result.returncode,
        "total_seconds": round(sum(t.seconds for t in timings), 3),
    }
