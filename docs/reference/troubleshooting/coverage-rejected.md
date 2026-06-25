# Coverage rejected: no per-test contexts

**Symptom.** Two error wordings, same root cause:

- From `fermut run --coverage coverage.json` (runtime path):
  `coverage.json at <path> has no per-test contexts. Regenerate with
  `pytest --cov=src --cov-context=test` …`.
- From `fermut doctor` (pre-flight check on the configured
  `coverage` file): `<path> has no per-test contexts` — surfaced as
  a `[fail]` line alongside the other tooling checks.

**Cause.** Coverage was generated without per-test contexts, so the
file only knows which lines ran — not which tests ran them. Two
common ways to hit this:

- You ran `coverage run` with no context flag at all.
- You ran `coverage run --context=test -m pytest`. Despite the name,
  coverage.py's `--context=LABEL` sets a *single static label* for
  the whole run — every line gets tagged `"test"`, so `--show-contexts`
  emits a file that looks populated but carries zero per-test
  resolution. fermut can't select tests from it.

## Fix

Fastest: let fermut generate it correctly.

```sh
fermut coverage
fermut run --coverage .coverage
```

[`fermut coverage`](../cli/coverage.md) always passes
`--cov-context=test`, so the resulting `.coverage` carries the per-test
contexts this error is about — and refreshes incrementally when tests
change.

Or generate a JSON export by hand:

```sh
pytest --cov=src --cov-context=test
coverage json -o coverage.json --show-contexts
```

Either way requires `pytest-cov`: `uv add --dev pytest-cov` (or
`pipx`/`pip` equivalent).

See the **[Coverage guide](../../guides/coverage.md)** for the full
setup.
