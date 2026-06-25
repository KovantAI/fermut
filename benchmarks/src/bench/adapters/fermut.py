"""fermut adapter."""
from __future__ import annotations

import subprocess
from pathlib import Path

from ..config import RepoCfg
from ..wheel import ensure_fermut_wheel
from .base import Adapter, RunResult


class FermutAdapter(Adapter):
    def env(self) -> dict[str, str]:
        env = super().env()
        repo_root = Path(env["FERMUT_REPO"])
        wheel_root = repo_root / "benchmarks" / "fixtures" / ".wheels"
        env["FERMUT_WHEEL"] = str(ensure_fermut_wheel(repo_root, wheel_root))
        return env

    def _write_fermut_toml(self, repo: RepoCfg, worktree: Path) -> None:
        """Emit a worktree-local fermut.toml carrying repo.fermut_exclude.

        Paths are matched relative to src_path by the loader, so we just
        forward the patterns. Overwrites any prior file from earlier runs.
        """
        if not repo.fermut_exclude:
            return
        lines = ["exclude = ["]
        for pat in repo.fermut_exclude:
            escaped = pat.replace('"', '\\"')
            lines.append(f'  "{escaped}",')
        lines.append("]\n")
        (worktree / "fermut.toml").write_text("\n".join(lines))

    def _coverage_is_stale(self, cov: Path, worktree: Path, repo: RepoCfg) -> bool:
        """True if coverage.json is missing or older than any test file.

        The score_3pct scenario adds NEW tests under tests/_bench_added/; if
        we kept a stale coverage.json those tests would never be selected for
        any mutant (the coverage filter only runs tests it has contexts for),
        so they could never kill a survivor and the score would look frozen.
        Regenerate whenever the test tree is newer than the coverage file.
        """
        if not cov.exists():
            return True
        cov_mtime = cov.stat().st_mtime
        test_root = worktree / repo.test_path
        if not test_root.exists():
            return False
        for p in test_root.rglob("*.py"):
            if p.stat().st_mtime > cov_mtime:
                return True
        return False

    def _ensure_coverage(self, repo: RepoCfg, worktree: Path) -> Path | None:
        """Generate a per-test-context coverage.json in the worktree.

        fermut's coverage filter narrows the test set per mutant — without it
        every mutant runs the *whole* suite, which is the slow path. This is
        a core part of running fermut as designed, so the benchmark enables
        it. The cost (one un-mutated suite run + JSON export) is incurred on
        the first run and reused thereafter (warm runs skip it) — unless the
        test tree changed, in which case we regenerate so new tests are seen.

        Returns the coverage.json path, or None if generation produced no
        usable contexts (caller then runs without the coverage filter).
        """
        cov = worktree / "coverage.json"
        if not self._coverage_is_stale(cov, worktree, repo):
            return cov

        env = self.env()
        # pytest-cov pulls coverage>=7; install into the fermut venv — but
        # only if absent. This is tooling setup, not mutation work: the cold
        # scenario runs `prepare()` (which calls this) in its own
        # `coverage_prep` phase, so a stray pip install here must not creep
        # into the timed `mutation_run`. Skip the network round-trip when it's
        # already importable (warm/loop/score_3pct re-entry).
        have_cov = subprocess.run(
            [self.python, "-c", "import pytest_cov"],
            env=env, capture_output=True,
        )
        if have_cov.returncode != 0:
            subprocess.run(
                [self.python, "-m", "pip", "install", "-q", "pytest-cov"],
                env=env,
                check=True,
            )
        # Baseline (un-mutated) run with per-test contexts. `--cov-context=test`
        # emits pytest nodeid contexts (tests/x_test.py::test_y) — the form
        # fermut can pass back to pytest. Tests should pass on the pristine
        # tree; tolerate a nonzero rc (xfail/flaky) since we only need the data
        # file to exist.
        cov_cmd = (
            f"{repo.test_cmd} --cov={repo.src_path} "
            f"--cov-context=test --cov-report="
        )
        baseline = subprocess.run(
            ["/bin/sh", "-c", cov_cmd], cwd=worktree, env=env, check=False,
            capture_output=True, text=True,
        )
        cj = subprocess.run(
            [self.python, "-m", "coverage", "json", "--show-contexts", "-o", "coverage.json"],
            cwd=worktree, env=env, check=False, capture_output=True, text=True,
        )
        if cj.returncode != 0 or not cov.exists():
            # Almost always a test-collection failure (missing test dep) →
            # coverage wrote no data file. Surface the baseline tail so the
            # missing import is obvious, instead of a bare `coverage json`
            # nonzero exit.
            tail = "\n".join((baseline.stdout + baseline.stderr).splitlines()[-15:])
            raise RuntimeError(
                f"coverage generation for {repo.name!r} produced no data — the "
                f"baseline test run likely failed to collect (missing test "
                f"dependency?). Fix the repo's install_cmd.\n--- baseline tail ---\n{tail}"
            )
        return cov if cov.exists() else None

    def prepare(self, repo: RepoCfg, worktree: Path) -> None:
        """Generate coverage.json ahead of the timed run (cold scenario).

        Hoists the pytest-cov install + baseline coverage pass out of
        `mutation_run` and into the cold `coverage_prep` phase. `run()` still
        calls `_ensure_coverage`, but after this it finds a fresh, non-stale
        coverage.json and returns immediately — zero coverage cost inside the
        timed mutation phase.
        """
        (worktree / ".fermut").mkdir(exist_ok=True)
        self._ensure_coverage(repo, worktree)

    def run(self, repo: RepoCfg, worktree: Path, *, timeout: int | None = None) -> RunResult:
        self._write_fermut_toml(repo, worktree)
        # fermut's --json writer does not create parent dirs; the cache lives
        # under <src>/.fermut, so the worktree-root .fermut may not exist yet.
        (worktree / ".fermut").mkdir(exist_ok=True)
        coverage = self._ensure_coverage(repo, worktree)
        argv = [
            "fermut", "run", repo.src_path,
            "--tests", repo.test_path,
            "--format", "human",
            # Survivor report consumed by the score_3pct scenario's
            # _survivor_dump (reads .fermut/report.json).
            "--json", ".fermut/report.json",
        ]
        if coverage is not None:
            # Absolute path: fermut resolves coverage keys against the detected
            # coverage cwd, but the flag value itself must point at the file
            # regardless of any internal cwd juggling.
            argv += ["--coverage", str(coverage.resolve())]
        # `--pytest-arg=<value>` (single token) instead of two tokens — clap
        # rejects bare `-p` after `--pytest-arg` because it parses the next
        # hyphen-prefixed token as a separate flag.
        for extra in repo.pytest_extra_args:
            argv.append(f"--pytest-arg={extra}")
        return self._exec(argv, cwd=worktree, timeout=timeout)
