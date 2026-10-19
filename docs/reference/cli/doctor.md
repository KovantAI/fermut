# `fermut doctor`

Diagnose environment + config.

```sh
fermut doctor [PATH] [--strict]
```

| Flag       | Effect                                                                                             |
|------------|----------------------------------------------------------------------------------------------------|
| `PATH`     | Where to start the project-root walk.                                                              |
| `--strict` | Treat warnings as failures.                                                                         |

Exit code is `1` when any check fails (or any check warns with
`--strict`), `0` otherwise. Wire `fermut doctor --strict` into CI as
a preflight.

## What `doctor` checks

| Check          | Status   | Notes                                                                                             |
|----------------|----------|---------------------------------------------------------------------------------------------------|
| `config`       | warn     | Missing `fermut.toml` / `[tool.fermut]` — `fermut init` generates one.                            |
| `venv`         | info     | The project venv found by the same walk `run` uses (`$VIRTUAL_ENV`, else a nearby `.venv`). Every tool below is probed in its `bin/` first, then on `PATH`. Skipped when none is found. |
| `python`       | required | `python3` (else `python`) ≥ 3.10, venv first, then `PATH`.                                        |
| `pytest` / `rstest` / `runner` | required | Configured runner — the check is named `pytest`, `rstest` (when `runner = "rstest"`), or `runner` (unittest). `unittest` is stdlib and always passes. |
| `coverage`     | required | `coverage` binary.                                                                                |
| `pytest-cov`   | required | `pytest_cov` importable from the resolved Python. Required to produce per-test contexts.          |
| `coverage-data`| warn/fail| The `coverage = "…"` file from config, or the `.coverage` DB `run` would auto-discover at the project root: present and valid for either format — a `.coverage` SQLite DB with a populated `context` table, or a JSON export with a `"contexts"` field. |
| `ty`           | optional | `ty` binary. `run` uses the embedded checker, so a missing binary is `warn`; it is `fail` only with `FERMUT_TY_EMBEDDED=0`. |
| `ruff`         | required | Only when `ruff_filter = true` in config.                                                         |
| `gh`           | optional | Only needed for `fermut pr-comment`.                                                              |
| `hypothesis`   | warn     | Project depends on Hypothesis but no `hypothesis_seed` pinned — survivors flicker between runs.    |
| `tests`        | required | When `tests = "…"` is wired: the path resolves to an existing directory.                          |
| `legacy-artifacts` | warn | Detects leftover dirs from prior mutation tools (`mutants/`, `.mutmut-cache`, `cosmic-ray-data/`, `.cache/mutpy/`). `mutants/tests/` breaks pytest collection. |

> **uv / Poetry / per-project venv?** No need to activate it. `doctor`
> resolves the project venv the same way `run` does (`$VIRTUAL_ENV`, else a
> `.venv` found walking up from the project) and probes tools inside it
> before `PATH`. The `venv` row shows which `bin/` was used. If it reads
> "none found", tools come from `PATH` only. Activate the venv, or create
> `.venv` next to `pyproject.toml`.

See **[Troubleshooting](../troubleshooting/index.md)** for the
symptoms `doctor` catches.
