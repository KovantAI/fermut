# `fermut coverage`

Generate or refresh the `.coverage` database that drives per-mutant
test selection — the one command to run after you touch your tests, so
you don't have to remember the pytest-cov incantation.

```sh
fermut coverage [PATH] [--source <dir>] [--tests <dir>] [--full] \
    [--output <path>] [--python <path>] [--pytest-arg <arg>]
```

With no `.coverage` yet, runs the full suite under coverage. With one
present, runs **only the test files changed since it was written** and
appends them in (purging their stale contexts first), so adding a test
costs that file's runtime, not a full sweep. fermut reads the
`.coverage` SQLite directly — there is no `coverage json` export step.

| Flag                 | Default                                  | Effect                                                                                          |
|----------------------|------------------------------------------|-------------------------------------------------------------------------------------------------|
| `PATH`               | `.`                                      | Where to start project-root discovery (same walk as `run`).                                     |
| `--source <dir>`     | configured `source_root`, else project root | Measured source root (`--cov=<this>`).                                                       |
| `--tests <dir>`      | configured `tests`, else `<source>/tests` | Tests location.                                                                                 |
| `--full`             | off                                      | Re-run the full suite even when an up-to-date `.coverage` exists.                               |
| `--output <path>`    | `<project>/.coverage`                    | Database output path.                                                                            |
| `--python <path>`    | auto-discover                            | Interpreter or virtualenv to run pytest with (`<python> -m pytest`) — coverage generation with no `pytest` on PATH. Same discovery as [`fermut run`](run.md#python-interpreter). |
| `--pytest-arg <arg>` | none                                     | Extra argument forwarded to pytest. Repeatable.                                                 |

## Incremental correctness

`pytest --cov-append` *unions* coverage data per context. For a
brand-new test that's exactly right. For a **modified** test that now
executes fewer lines, the old line bits would linger and falsely mark a
line as covered — so before appending, `fermut coverage` deletes the
changed test files' contexts from the database, forcing their coverage
to be rebuilt from scratch.

Change detection is by file modification time vs the database's. It
acts at test-**file** granularity: editing any test in a file
re-measures that whole file.

## Wiring it into `fermut run`

`fermut coverage` prints the exact next step. Either pass the path:

```sh
fermut run --coverage .coverage
```

or set it once in your config so it's automatic:

```toml
coverage = ".coverage"
```

## Requirements

`pytest` and `pytest-cov` must be installed (`fermut doctor` checks
both). The command runs pytest from the project root; your project's
own pytest configuration must make the source importable, exactly as a
normal `pytest` run would.

## See also

- **[Coverage guide](../../guides/coverage.md)** — why per-test
  selection matters and the manual pytest-cov recipe.
- **[`run`](run.md)** — the `--coverage` flag this feeds.
