"""Clone each benchmark repo at its pinned ref into $FX/fermut-<repo>, give it a
seeded venv at $FX/.venvs/fermut-<repo>, install it as configs/repos.toml says
plus pins/<repo>.txt (a `pip freeze` of the macOS fixture venv, so both
platforms test the same versions), and record per-test coverage (.coverage)
the way run10.sh expects.

    python3 setup.py [repo ...]     (inside the linux container; idempotent;
                                     FORCE=1 re-records coverage)
"""

import os
import subprocess
import sys
from pathlib import Path

import tomllib

HERE = Path(__file__).resolve().parent
FX = Path(os.environ["FX"])
REPOS = tomllib.loads((HERE / "../../configs/repos.toml").read_text())


def sh(cmd, cwd=None, env=None):
    print(f"$ {cmd}", flush=True)
    subprocess.run(
        cmd, shell=True, check=True, cwd=cwd, env=env, executable="/bin/bash"
    )


for name in sys.argv[1:] or REPOS:
    repo = REPOS[name]
    wt, venv = FX / f"fermut-{name}", FX / ".venvs" / f"fermut-{name}"
    if not wt.exists():
        sh(f"git clone -q --depth 1 --branch {repo['ref']} {repo['url']} {wt}")
    if repo.get("fermut_exclude"):
        globs = "".join(f'  "{g}",\n' for g in repo["fermut_exclude"])
        (wt / "fermut.toml").write_text(f"exclude = [\n{globs}]\n")
    if not venv.exists():
        sh(f"uv venv -q --seed -p 3.13 {venv}")
    env = {
        **os.environ,
        "VIRTUAL_ENV": str(venv),
        "PATH": f"{venv}/bin:{os.environ['PATH']}",
    }
    pins = HERE / "pins" / f"{name}.txt"
    sh(f"{repo['install_cmd']} && pip install -q pytest-cov -r {pins}", cwd=wt, env=env)
    if os.environ.get("FORCE"):
        (wt / ".coverage").unlink(missing_ok=True)
    if not (wt / ".coverage").exists():
        extra = " ".join(repo.get("pytest_extra_args", []))
        empty_rc = FX / "empty.coveragerc"
        empty_rc.write_text("")
        # The repo's own coverage config can redirect or split the data file
        # (typer: parallel, coverage/.coverage); an empty one writes .coverage
        # here. SIGINT after 30 min ends a hung test (werkzeug's live reloader)
        # cleanly, so the data still gets saved. Failing tests still record
        # contexts; a red suite shows in run10's baseline.
        sh(
            f"timeout -s INT 1800 python -m pytest -q -p no:cacheprovider {extra} "
            f"--cov={repo['src_path']} --cov-config={empty_rc} --cov-context=test "
            f"--cov-report= {repo['test_path']} > /dev/null 2>&1 || true",
            cwd=wt,
            env=env,
        )
    print(
        f"== {name}: coverage {'ok' if (wt / '.coverage').exists() else 'MISSING'}",
        flush=True,
    )
