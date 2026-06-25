"""poodle adapter.

poodle takes its config from poodle.toml (or pyproject.toml). We write a
minimal poodle.toml into the worktree so each repo benchmarks consistently
regardless of upstream defaults.
"""
from __future__ import annotations

from pathlib import Path
from textwrap import dedent

from ..config import RepoCfg
from .base import Adapter, RunResult


CONFIG_TEMPLATE = dedent("""\
    [poodle]
    source_folders = ["{src_path}"]
    runner_cmd = "python -m pytest -x -q {test_path}"
    reporters = ["summary"]
""")


class PoodleAdapter(Adapter):
    def _write_config(self, worktree: Path, repo: RepoCfg) -> None:
        (worktree / "poodle.toml").write_text(CONFIG_TEMPLATE.format(
            src_path=repo.src_path,
            test_path=repo.test_path,
        ))

    def run(self, repo: RepoCfg, worktree: Path, *, timeout: int | None = None) -> RunResult:
        self._write_config(worktree, repo)
        return self._exec(["poodle"], cwd=worktree, timeout=timeout)
