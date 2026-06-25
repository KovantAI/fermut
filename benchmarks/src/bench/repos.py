"""Clone + install reference repos into isolated worktrees."""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

from .config import RepoCfg

FIXTURES = Path(__file__).resolve().parents[2] / "fixtures"


def worktree_for(tool: str, repo: RepoCfg) -> Path:
    """Per-(tool, repo) worktree path.

    Keyed by tool so two tools never share one tree: mutmut copies/mutates
    the source in place, which would change file hashes and invalidate
    fermut's file-hash-keyed cache (and vice versa). Isolated trees keep each
    tool's cache and cold/warm timing honest.
    """
    return FIXTURES / f"{tool}-{repo.name}"


def clean(tool: str, repo: RepoCfg) -> None:
    """Wipe the (tool, repo) worktree. Used before every cold-scenario clone."""
    target = worktree_for(tool, repo)
    if target.exists():
        shutil.rmtree(target)


def clone(tool: str, repo: RepoCfg) -> Path:
    """Clone repo@ref into fixtures/<tool>-<name>. Returns the worktree path.

    Uses `--depth 1` with `--branch` to keep cold-clone time honest — full
    history would skew metric 1 for big repos like httpx/flask.
    """
    target = worktree_for(tool, repo)
    FIXTURES.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "clone", "--depth", "1", "--branch", repo.ref, repo.url, str(target)],
        check=True,
    )
    return target


def install(repo: RepoCfg, worktree: Path, venv_python: str) -> None:
    """Run the repo's install_cmd inside the per-tool venv.

    After install, assert pytest is importable. A missing pytest makes every
    mutant error at spawn, which silently inflates the score to 100% — a
    fake "perfect" result. Fail loudly here instead.
    """
    # install_cmd is space-separated shell — run via /bin/sh -c so chained
    # commands like `pip install -e . && pip install pytest` work.
    env = os.environ.copy()
    env["PATH"] = f"{Path(venv_python).parent}:{env.get('PATH', '')}"
    subprocess.run(
        ["/bin/sh", "-c", repo.install_cmd],
        cwd=worktree,
        check=True,
        env=env,
    )
    check = subprocess.run(
        [venv_python, "-c", "import pytest"],
        cwd=worktree,
        env=env,
        capture_output=True,
        text=True,
    )
    if check.returncode != 0:
        raise RuntimeError(
            f"pytest not importable in venv after installing {repo.name!r} "
            f"(install_cmd={repo.install_cmd!r}). Every mutant would error at "
            f"spawn and the score would falsely read 100%. Add pytest to the "
            f"repo's install_cmd.\n{check.stderr}"
        )


def touch_loop_edit(worktree: Path, repo: RepoCfg) -> Path:
    """Append a comment to the loop_edit_file and return its path.

    Adapters that key cache on file hash (fermut, mutmut) will invalidate
    everything in that file but keep cached results elsewhere — the point of
    the loop scenario.
    """
    target = worktree / repo.loop_edit_file
    with target.open("a") as fh:
        fh.write("\n# benchmark loop touch\n")
    return target


def revert_loop_edit(worktree: Path, repo: RepoCfg) -> None:
    """Discard the loop edit via `git checkout --`."""
    subprocess.run(
        ["git", "checkout", "--", repo.loop_edit_file],
        cwd=worktree,
        check=True,
    )
