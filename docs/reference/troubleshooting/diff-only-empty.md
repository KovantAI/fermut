# `--diff-only` finds nothing

**Symptom.** `--diff-only main` shows zero mutants despite recent
edits.

**Cause.** Almost always `fetch-depth: 1` on `actions/checkout`.
The merge base with `main` isn't reachable, so the diff is empty.

## Fix

Set `fetch-depth: 0` in your checkout step:

```yaml
- uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10  # v6.0.3
  with:
    fetch-depth: 0
```
