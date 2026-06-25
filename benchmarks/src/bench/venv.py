"""Per-(tool, repo) venv helpers.

Each (tool, repo) pair gets its own venv under fixtures/.venvs/<tool>-<repo>.
Keeps tool installs isolated (e.g. mutmut + cosmic-ray bring different click
versions) and lets cold-vs-warm be controlled purely via cache wipes, not by
re-installing the tool itself.
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

FIXTURES = Path(__file__).resolve().parents[2] / "fixtures"
VENVS = FIXTURES / ".venvs"


def venv_for(tool: str, repo: str) -> Path:
    return VENVS / f"{tool}-{repo}"


def ensure(tool: str, repo: str, *, fresh: bool = False, python: str | None = None) -> Path:
    """Return path to a usable venv. If `fresh`, recreate from scratch.

    When `python` is provided (e.g. "3.11"), uv creates the venv with that
    interpreter — fetching it if not already installed. Falls back to
    `python -m venv` using the orchestrator's interpreter when unset.
    """
    path = venv_for(tool, repo)
    if fresh and path.exists():
        import shutil
        shutil.rmtree(path)
    if not path.exists():
        VENVS.mkdir(parents=True, exist_ok=True)
        if python:
            subprocess.run(
                ["uv", "venv", "--python", python, "--seed", str(path)],
                check=True,
            )
        else:
            subprocess.run(
                [sys.executable, "-m", "venv", str(path)],
                check=True,
            )
        # Always upgrade pip — old pip can't resolve modern wheels.
        subprocess.run(
            [str(path / "bin" / "pip"), "install", "-U", "pip", "wheel"],
            check=True,
            capture_output=True,
        )
    return path
