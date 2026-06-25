"""mutmut 3.x adapter.

mutmut 3 is a rewrite: no Pony ORM, no `--paths-to-mutate`/`--runner` CLI
flags. Config moved to `[mutmut]` in setup.cfg / pyproject.toml. Cache is
plain JSON under `mutants/`. Score comes from
`mutmut export-cicd-stats` → `mutants/mutmut-cicd-stats.json`.

The adapter writes a per-worktree `setup.cfg` `[mutmut]` block before
running so each repo gets the right `source_paths` + pytest test selection.
"""
from __future__ import annotations

import json
import shlex
from configparser import ConfigParser
from pathlib import Path

from ..config import RepoCfg
from .base import Adapter, RunResult


class MutmutAdapter(Adapter):
    # mutmut caches per-mutant verdicts keyed on source; adding tests doesn't
    # invalidate them, so a re-score would stay frozen unless we wipe the
    # `mutants/` cache first and let mutmut re-run against the new suite.
    rescore_needs_cache_clear = True

    def install(self) -> None:
        super().install()

    def _write_config(self, repo: RepoCfg, worktree: Path) -> None:
        """Merge `[mutmut]` keys into the worktree's setup.cfg.

        Preserves any existing setup.cfg content — only the `[mutmut]`
        section is owned by the benchmark. Keys: `source_paths` (what to
        mutate), `pytest_add_cli_args_test_selection` (where tests live),
        `pytest_add_cli_args` (forwarded from repo.pytest_extra_args).
        """
        cfg_path = worktree / "setup.cfg"
        cp = ConfigParser()
        if cfg_path.exists():
            cp.read(cfg_path)
        if not cp.has_section("mutmut"):
            cp.add_section("mutmut")
        cp.set("mutmut", "source_paths", repo.src_path)
        cp.set("mutmut", "pytest_add_cli_args_test_selection", repo.test_path)
        extras = list(repo.pytest_extra_args)
        for node in repo.mutmut_deselect:
            extras += ["--deselect", node]
        for path in repo.mutmut_ignore:
            extras.append(f"--ignore={path}")
        if extras:
            cp.set("mutmut", "pytest_add_cli_args", "\n".join(extras))
        if repo.mutmut_also_copy:
            cp.set("mutmut", "also_copy", "\n".join(repo.mutmut_also_copy))
        # Mirror fermut_exclude → mutmut `do_not_mutate` so scoping stays
        # consistent across tools. fermut_exclude patterns are relative
        # to src_path; mutmut's do_not_mutate matches against the path
        # under source_paths, so we prefix accordingly.
        if repo.fermut_exclude:
            prefix = repo.src_path.rstrip("/") + "/"
            do_not = [prefix + pat for pat in repo.fermut_exclude]
            cp.set("mutmut", "do_not_mutate", "\n".join(do_not))
        with cfg_path.open("w") as fh:
            cp.write(fh)

    def run(self, repo: RepoCfg, worktree: Path, *, timeout: int | None = None) -> RunResult:
        self._write_config(repo, worktree)
        result = self._exec(["mutmut", "run"], cwd=worktree, timeout=timeout)
        # Materialize stats. Failure here doesn't sink the run — the
        # caller still gets timing + raw stdout.
        self._exec(["mutmut", "export-cicd-stats"], cwd=worktree, timeout=120)
        score, mutants = self._score_from_stats(worktree)
        if score is not None:
            result.score = score
        if mutants is not None:
            result.mutants = mutants
        return result

    @staticmethod
    def _score_from_stats(worktree: Path) -> tuple[float | None, int | None]:
        """Read mutants/mutmut-cicd-stats.json and compute killed-pct.

        Schema (mutmut 3.x): killed / survived / total / no_tests /
        skipped / suspicious / timeout / segfault. Score numerator =
        killed + suspicious + timeout (anything not survived/skipped/
        no_tests counts as a verdict); denominator = numerator + survived.
        Mirrors mutmut's own banner math.
        """
        path = worktree / "mutants" / "mutmut-cicd-stats.json"
        if not path.exists():
            return None, None
        try:
            data = json.loads(path.read_text())
        except (OSError, ValueError):
            return None, None
        killed = int(data.get("killed", 0)) + int(data.get("suspicious", 0)) + int(data.get("timeout", 0))
        survived = int(data.get("survived", 0))
        scored = killed + survived
        if scored == 0:
            return None, None
        return 100.0 * killed / scored, scored
