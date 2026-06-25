"""Adapter base class.

Each mutation tool has different invocation, output, and cache shape.
Adapters normalize them behind a small interface:

  - install():        one-time install into a tool-scoped venv
  - clear_cache():    wipe tool caches inside the worktree (cold setup)
  - run():            mutation testing run, returns (seconds, score, raw)
  - tool_dir():       location to scrub when going cold

Adapters live alongside this file; ADAPTERS in __init__.py is the registry.
"""
from __future__ import annotations

import os
import re
import shutil
import subprocess
import time
from abc import ABC, abstractmethod
from dataclasses import dataclass
from pathlib import Path

from ..config import RepoCfg, ToolCfg


@dataclass
class RunResult:
    seconds: float
    score: float | None
    returncode: int
    stdout: str
    stderr: str
    mutants: int | None = None


class Adapter(ABC):
    # Whether a re-score after the test suite changes (score_3pct: Claude
    # adds tests) requires wiping the tool cache first. fermut keys its cache
    # on per-mutant test selection (coverage scope), so newly-added tests
    # invalidate the affected entries automatically — no clear needed. mutmut
    # / cosmic-ray key on source only and would return stale "survived"
    # verdicts for mutants the new tests now kill, so they must be cleared.
    rescore_needs_cache_clear: bool = False

    def __init__(self, tool: ToolCfg, venv_dir: Path) -> None:
        self.tool = tool
        self.venv_dir = venv_dir
        self.python = str(venv_dir / "bin" / "python")
        self.bin = str(venv_dir / "bin")

    # ---- shared helpers --------------------------------------------------

    def env(self) -> dict[str, str]:
        env = os.environ.copy()
        env["PATH"] = f"{self.bin}:{env.get('PATH', '')}"
        env["VIRTUAL_ENV"] = str(self.venv_dir)
        repo_root = Path(__file__).resolve().parents[4]
        env["FERMUT_REPO"] = str(repo_root)
        return env

    def parse_score(self, stdout: str) -> float | None:
        m = re.search(self.tool.score_regex, stdout)
        if not m:
            return None
        groups = m.groups()
        # mutmut regex captures (killed, total); convert to %.
        if len(groups) == 2 and groups[0].isdigit() and groups[1].isdigit():
            killed, total = int(groups[0]), int(groups[1])
            return 100.0 * killed / total if total else 0.0
        try:
            return float(groups[-1])
        except (TypeError, ValueError):
            return None

    def parse_mutants(self, stdout: str) -> int | None:
        pattern = self.tool.mutants_regex
        if not pattern:
            return None
        m = re.search(pattern, stdout)
        if not m:
            return None
        try:
            return int(m.group(1))
        except (TypeError, ValueError, IndexError):
            return None

    def clear_cache(self, worktree: Path) -> None:
        for entry in self.tool.cache_dirs:
            path = worktree / entry
            if path.is_dir():
                shutil.rmtree(path)
            elif path.exists():
                path.unlink()

    def _exec(self, argv: list[str], cwd: Path, timeout: int | None = None) -> RunResult:
        start = time.perf_counter()
        # stdin=DEVNULL: prevents OpenSSL / getpass-style prompts (e.g. httpx
        # encrypted-PEM tests under mutation) from hanging on the TTY.
        proc = subprocess.run(
            argv,
            cwd=cwd,
            env=self.env(),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        dur = time.perf_counter() - start
        combined = proc.stdout + "\n" + proc.stderr
        return RunResult(
            seconds=dur,
            score=self.parse_score(combined),
            returncode=proc.returncode,
            stdout=proc.stdout,
            stderr=proc.stderr,
            mutants=self.parse_mutants(combined),
        )

    # ---- subclass hooks --------------------------------------------------

    def prepare(self, repo: RepoCfg, worktree: Path) -> None:
        """One-time, un-mutated setup whose cost is NOT the mutation phase.

        Default no-op. fermut overrides this to generate the per-test
        coverage.json (and install pytest-cov) so that work lands in the
        cold scenario's `coverage_prep` phase instead of inflating the
        timed `mutation_run`. Idempotent — re-runs are cheap no-ops.
        """

    def install(self) -> None:
        subprocess.run(
            ["/bin/sh", "-c", self.tool.install_cmd],
            env=self.env(),
            check=True,
        )

    @abstractmethod
    def run(self, repo: RepoCfg, worktree: Path, *, timeout: int | None = None) -> RunResult:
        """Run one mutation pass and return timing + score."""
