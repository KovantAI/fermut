"""Run metadata: timestamps, tool versions, repo commits.

Called by main after each scenario finishes so every result JSON carries
enough provenance to be readable months later without diving into git.
"""
from __future__ import annotations

import datetime as _dt
import subprocess
from pathlib import Path

from .config import RepoCfg, ToolCfg
from .venv import venv_for


def utc_now() -> str:
    return _dt.datetime.now(_dt.timezone.utc).isoformat(timespec="seconds")


def tool_version(tool: ToolCfg, repo_name: str) -> str | None:
    """Run the tool's `version_cmd` inside its venv and return stdout.

    Returns None if the venv or binary isn't there yet (e.g. error before
    install completed).
    """
    venv = venv_for(tool.name, repo_name)
    if not venv.exists():
        return None
    bin_dir = venv / "bin"
    parts = tool.version_cmd.split()
    exe = bin_dir / parts[0]
    if not exe.exists():
        return None
    try:
        out = subprocess.run(
            [str(exe), *parts[1:]],
            capture_output=True, text=True, timeout=30,
        )
        return (out.stdout.strip() or out.stderr.strip()) or None
    except Exception:  # noqa: BLE001
        return None


def repo_commit(repo: RepoCfg, worktree: Path) -> dict[str, str | None]:
    """Read the worktree's pinned ref + actual HEAD sha.

    `ref` is whatever repos.toml asked for; `head` is what git checked out.
    They should agree, but recording both makes drift obvious.
    """
    out: dict[str, str | None] = {"ref": repo.ref, "head": None}
    if not worktree.exists():
        return out
    try:
        sha = subprocess.check_output(
            ["git", "rev-parse", "HEAD"],
            cwd=worktree, text=True, timeout=10,
        ).strip()
        out["head"] = sha
    except Exception:  # noqa: BLE001
        pass
    return out
