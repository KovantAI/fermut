# Phantom survivors

**Symptom.** `fermut run` reports a survivor, but when you patch
the file by hand and re-run pytest, the test fails.

**Cause.** Usually one of:

- The cache is stale because two `mutant.id`s collided after a
  fermut upgrade. Rare — `fermut clean` fixes it.
- Hypothesis happened to generate a passing input on this run.
  `hypothesis_seed = 12345` (or any fixed value) fixes it.
- The coverage database is stale and missed a test that exercises
  the line. A stale `.coverage` — including one fermut auto-discovered
  at the project root — silently narrows the test set (the same holds
  for a legacy `coverage.json` export). Re-generate.

## Fix

```sh
fermut clean                                              # evict cache
# pin Hypothesis seed in fermut.toml
pytest --cov=src --cov-context=test                       # rebuild .coverage
fermut run src/ --tests tests/                            # auto-discovers .coverage
```

`fermut run` auto-discovers the freshly written `.coverage` at the
project root; pass `--coverage .coverage` if it lives elsewhere.
[`fermut coverage`](../cli/coverage.md) rebuilds it for you with the
right flags. (Or regenerate the JSON export manually: `coverage json -o
coverage.json --show-contexts`, then `fermut run --coverage
coverage.json`.)
