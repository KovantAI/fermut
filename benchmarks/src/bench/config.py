"""Config loaders for repos.toml and tools.toml."""
from __future__ import annotations

import tomllib
from dataclasses import dataclass, field
from pathlib import Path


CONFIG_DIR = Path(__file__).resolve().parents[2] / "configs"


@dataclass
class RepoCfg:
    name: str
    url: str
    ref: str
    src_path: str
    test_path: str
    install_cmd: str
    test_cmd: str
    loop_edit_file: str
    pytest_extra_args: list[str] = field(default_factory=list)
    # Glob patterns (relative to src_path) of files to skip during mutation.
    # FermutAdapter writes these into a worktree-local fermut.toml; mutmut
    # currently has no equivalent and mutates everything under src_path.
    fermut_exclude: list[str] = field(default_factory=list)
    # mutmut 3.x copies the project into `mutants/` and runs pytest there.
    # Default `also_copy` is src/tests/test*.py/setup.cfg/pyproject.toml —
    # repos with tests that read other paths (e.g. click's test_expand_args
    # reads docs/conf.py) need extras listed here.
    mutmut_also_copy: list[str] = field(default_factory=list)
    # Test node IDs to skip in mutmut's baseline + every mutant run. Used
    # for tests that depend on test-discovery order or global state that
    # mutmut 3.x's mutants/-cwd discovery rearranges.
    mutmut_deselect: list[str] = field(default_factory=list)
    # Whole test files to skip via pytest `--ignore=…`. Used when mutmut's
    # trampoline injection shifts source line numbers (any test that
    # asserts on warning/exception lineno breaks file-wide).
    mutmut_ignore: list[str] = field(default_factory=list)


@dataclass
class ToolCfg:
    name: str
    install_cmd: str
    version_cmd: str
    cache_dirs: list[str]
    score_regex: str
    mutants_regex: str | None = None
    # Optional Python version for the per-(tool, repo) venv. When unset, the
    # venv inherits the orchestrator's interpreter. Used to pin mutmut to
    # 3.11 — its Pony ORM dep crashes on 3.13.
    python: str | None = None


def load_repos() -> dict[str, RepoCfg]:
    raw = tomllib.loads((CONFIG_DIR / "repos.toml").read_text())
    return {name: RepoCfg(name=name, **body) for name, body in raw.items()}


def load_tools() -> dict[str, ToolCfg]:
    raw = tomllib.loads((CONFIG_DIR / "tools.toml").read_text())
    return {name: ToolCfg(name=name, **body) for name, body in raw.items()}
