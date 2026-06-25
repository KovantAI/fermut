# Phantom survivors

**Symptom.** `fermut run` reports a survivor, but when you patch
the file by hand and re-run pytest, the test fails.

**Cause.** Usually one of:

- The cache is stale because two `mutant.id`s collided after a
  fermut upgrade. Rare — `fermut clean` fixes it.
- Hypothesis happened to generate a passing input on this run.
  `hypothesis_seed = 12345` (or any fixed value) fixes it.
- Coverage `coverage.json` is stale and missed a test that
  exercises the line. Re-generate.

## Fix

```sh
fermut clean                                              # evict cache
# pin Hypothesis seed in fermut.toml
pytest --cov=src --cov-context=test                       # regenerate coverage
coverage json -o coverage.json --show-contexts
fermut run src/ --tests tests/
```
