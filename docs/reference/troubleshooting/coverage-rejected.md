# Coverage rejected: no per-test contexts

**Symptom.** Two error wordings, same root cause. The check is
format-agnostic — it rejects either a `.coverage` SQLite DB or a
`coverage.json` export that carries no per-test contexts:

- From `fermut run` (runtime path, whether the coverage file was
  auto-discovered, set via `coverage = "…"`, or passed with
  `--coverage`): `<path> has no per-test contexts. Regenerate with
  `pytest --cov=src --cov-context=test` …`.
- From `fermut doctor` (the `coverage-data` pre-flight check on the
  configured coverage file): `<path> has no per-test
  contexts` — surfaced as a `[fail]` line alongside the other tooling
  checks.

**Cause.** Coverage was generated without per-test contexts, so the
database only knows which lines ran — not which tests ran them. This
applies equally to a `.coverage` SQLite DB (empty/undifferentiated
`context` table) and a JSON export (no per-test `"contexts"`). Two
common ways to hit this:

- You ran `coverage run` with no context flag at all.
- You ran `coverage run --context=test -m pytest`. Despite the name,
  coverage.py's `--context=LABEL` sets a *single static label* for
  the whole run — every line gets tagged `"test"`, so `--show-contexts`
  emits a file that looks populated but carries zero per-test
  resolution. fermut can't select tests from it.

## Fix

Regenerate coverage with per-test contexts. The `--cov-context=test`
flag is the part that matters — it writes coverage.py's native
`.coverage` SQLite DB with the per-test resolution fermut needs:

```sh
pytest --cov=src --cov-context=test
fermut run          # auto-discovers .coverage at the project root
```

fermut picks up `.coverage` from the project root automatically; pass
`--coverage .coverage` if it lives elsewhere.

Even simpler, [`fermut coverage`](../cli/coverage.md) always passes
`--cov-context=test` for you (and refreshes incrementally when tests
change):

```sh
fermut coverage
fermut run --coverage .coverage
```

Or, manually, produce a legacy JSON export instead:

```sh
pytest --cov=src --cov-context=test
coverage json -o coverage.json --show-contexts
fermut run --coverage coverage.json
```

Every path requires `pytest-cov`: `uv add --dev pytest-cov` (or
`pipx`/`pip` equivalent).

See the **[Coverage guide](../../guides/coverage.md)** for the full
setup.
