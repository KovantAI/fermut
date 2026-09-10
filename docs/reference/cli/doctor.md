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
| `python`       | required | `python3` ≥ 3.10 on `PATH`.                                                                       |
| `pytest`       | required | Pytest binary on `PATH` (when configured runner is pytest).                                       |
| `coverage`     | required | `coverage` binary on `PATH`.                                                                      |
| `pytest-cov`   | required | `pytest_cov` importable from the `python3` on `PATH`. Required to produce per-test contexts.      |
| `coverage-data`| info     | When `coverage = "…"` is wired in config: the file is present and valid for either format — a `.coverage` SQLite DB with a populated `context` table, or a JSON export with a `"contexts"` field.                |
| `ty`           | required | `ty` binary on `PATH`. Hard requirement unless `ty_filter = false` in `fermut.toml`.              |
| `ruff`         | required | Only when `ruff_filter = true` in config.                                                         |
| `gh`           | optional | Only needed for `fermut pr-comment`.                                                              |
| `hypothesis`   | warn     | Project depends on Hypothesis but no `hypothesis_seed` pinned — survivors flicker between runs.    |
| `tests`        | required | When `tests = "…"` is wired: the path resolves to an existing directory.                          |
| `legacy-artifacts` | warn | Detects leftover dirs from prior mutation tools (`mutants/`, `.mutmut-cache`, `cosmic-ray-data/`, `.cache/mutpy/`). `mutants/tests/` breaks pytest collection. |

> **uv / Poetry / per-project venv?** `doctor` resolves every tool
> from the current shell's `PATH`. It does not auto-discover a
> `.venv` next to `pyproject.toml`. Activate the project venv
> (`source .venv/bin/activate`, `poetry shell`) or invoke as `uv run
> fermut doctor` before relying on the result — a system `python3`
> on macOS will otherwise fail the ≥ 3.10 check even when the
> project's actual interpreter is 3.12, and `pytest-cov` won't be
> importable.

See **[Troubleshooting](../troubleshooting/index.md)** for the
symptoms `doctor` catches.
