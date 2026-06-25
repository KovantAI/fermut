"""cosmic-ray adapter.

cosmic-ray needs an upfront TOML config and a sqlite session file. We
generate both into the worktree on first run, then `init` + `exec` + `dump`.
The dump output is parsed for the surviving-mutant percentage.
"""
from __future__ import annotations

from pathlib import Path
from textwrap import dedent

from ..config import RepoCfg
from .base import Adapter, RunResult


CONFIG_TEMPLATE = dedent("""\
    [cosmic-ray]
    module-path = "{src_path}"
    timeout = 30.0
    excluded-modules = []
    test-command = "python -m pytest -x -q {test_path}"

    [cosmic-ray.distributor]
    name = "local"
""")


class CosmicRayAdapter(Adapter):
    # Session DB caches verdicts keyed on source; new tests need a fresh
    # session to be exercised on a re-score.
    rescore_needs_cache_clear = True

    def _write_config(self, worktree: Path, repo: RepoCfg) -> Path:
        cfg = worktree / "cosmic-ray.toml"
        cfg.write_text(CONFIG_TEMPLATE.format(
            src_path=repo.src_path,
            test_path=repo.test_path,
        ))
        return cfg

    def run(self, repo: RepoCfg, worktree: Path, *, timeout: int | None = None) -> RunResult:
        cfg = self._write_config(worktree, repo)
        session = worktree / "cosmic-ray.session.sqlite"
        # `init` seeds the session with all pending work items. Re-`init`
        # wipes prior verdicts, so skip it when a session already exists —
        # that's what lets the warm scenario reuse a populated session
        # (`exec` on a complete session is a near-instant no-op). The cold
        # scenario clears the session first, so it always inits fresh.
        init_secs = 0.0
        if not session.exists():
            init_result = self._exec(
                ["cosmic-ray", "init", str(cfg), str(session)],
                cwd=worktree,
                timeout=300,
            )
            if init_result.returncode != 0:
                return init_result
            init_secs = init_result.seconds
        exec_result = self._exec(
            ["cosmic-ray", "exec", str(cfg), str(session)],
            cwd=worktree,
            timeout=timeout,
        )
        # Score comes from `cr-report` (NOT `cosmic-ray dump`, which prints raw
        # work-items with no aggregate). cr-report prints `total jobs: N` and
        # `surviving mutants: N (X%)` — see tools.toml regexes.
        report = self._exec(
            ["cr-report", str(session)],
            cwd=worktree,
            timeout=120,
        )
        # Time accounting: exec is the dominant cost; init+report small but real.
        exec_result.seconds += init_secs + report.seconds
        # cr-report's regex captures the *surviving* percentage; every other
        # tool reports a kill score, so convert for comparability.
        if report.score is not None:
            exec_result.score = round(100.0 - report.score, 2)
        if report.mutants is not None:
            exec_result.mutants = report.mutants
        return exec_result
