# Coverage in CI

Every mutation run needs a fresh `.coverage`, and on a slow suite building it
is the long pole of a PR job. These recipes cut that cost; they work with the
hand-written workflows in [CI recipes](integrations.md). The
[packaged action](github-action.md) already caches coverage, and its
`warm-only` job is Recipe C.

The [PR gate](integrations.md#github-actions) runs `fermut coverage` fresh on every run, which
rebuilds the whole `.coverage` database from a full-suite sweep. On a
slow suite that sweep, not the mutation run, becomes the long pole. Two
recipes cut it — they compose, and both are safe alongside `--diff-only`.

Know what each one buys before reaching for it:

| Recipe | Saves | Does **not** save |
|--------|-------|-------------------|
| A. Cache `.coverage` across runs | suite runtime — refresh re-measures only changed test files, not the whole suite | the first (cold-cache) run, which still sweeps fully |
| B. Scope `--cov` to changed files | index size, fermut's parse time, cache footprint | suite runtime — every test still runs under coverage |

## Recipe A: cache `.coverage` so the refresh is incremental { #cache-coverage }

`fermut coverage` is already incremental: given an existing database it
re-runs only the test files whose content changed (and the tests whose
contexts cover changed *source* lines), then appends them in. In CI that
machinery is wasted unless the database survives between runs — a fresh
checkout starts with no `.coverage`, so the "incremental" refresh falls
straight back to a full sweep.

Cache the database and its fingerprint sidecar across PR runs:

```yaml
      - name: restore coverage db
        uses: actions/cache/restore@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
        with:
          path: |
            .coverage
            .coverage.fermut-fingerprints.json
          key: fermut-coverage-${{ runner.os }}-${{ github.sha }}
          restore-keys: |
            fermut-coverage-${{ runner.os }}-

      - run: uv tool run fermut coverage --source src --tests tests/

      - name: save coverage db
        if: always()
        uses: actions/cache/save@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
        with:
          path: |
            .coverage
            .coverage.fermut-fingerprints.json
          key: fermut-coverage-${{ runner.os }}-${{ github.sha }}
```

Both paths are mandatory. The sidecar
(`.coverage.fermut-fingerprints.json`) holds the per-file content hashes
`fermut coverage` diffs against; restore the `.coverage` DB without it
and fermut can't tell what changed, so it rebuilds fully — exactly the
sweep the cache was meant to skip.

Notes:

- **Content-hash, not mtime.** `fermut coverage` fingerprints test files
  by content, precisely so a fresh checkout (which rewrites every mtime)
  doesn't force a full re-measure. Caching is what makes that pay off.
- **Correctness is preserved, not traded away.** A changed source line
  re-runs the tests that cover it, so the incremental DB stays diff-safe
  for `--diff-only` / `--since`; a genuinely new code path under an
  unchanged test is conservatively coverage-skipped until the next
  refresh, never falsely killed. See
  [`fermut coverage` incremental correctness](../reference/cli/coverage.md#incremental-correctness).
- **Restore-keys prefix** lets a PR seed from the most recent cache even
  when no exact-SHA hit exists, so the first run on a branch is still
  incremental against `main`'s database.

## Recipe B: scope the index to the changed subtree { #scope-cov }

Under `--diff-only`, mutants land only on changed lines, so the coverage
filter only ever looks up tests for lines in changed files. Measuring the
whole source tree still produces a correct index — just a larger one than
the gate needs.

For a **subtree**, point `fermut coverage` at it — `--source` becomes the
`--cov=` target:

```sh
fermut coverage --source src/pkg/payments --tests tests/
```

For **specific scattered files**, drop to the manual pytest-cov path with
one `--cov=` per changed file (fermut reads the resulting `.coverage`
just the same):

```sh
CHANGED=$(git diff --name-only origin/main -- '*.py' | grep '^src/')
COVERAGE_CORE=ctrace pytest $(printf ' --cov=%s' $CHANGED) --cov-context=test
fermut run src/ --tests tests/ --diff-only origin/main --coverage .coverage
```

This is safe because every changed line lives in a measured file, so its
per-test contexts are always present; the continuation-line fallback
stays within the same file too. What you save is index size and fermut's
parse time — the suite still runs in full, since coverage instruments the
whole run regardless of `--cov` scope. On suites where the multi-GB
`coverage.json` or a slow parse was the pain, this is the lever; on
suites where the *sweep* is the pain, reach for Recipe A instead (or both).

> **Don't combine Recipe B with Recipe A's cache blindly.** A cached
> `.coverage` scoped to one PR's changed files is wrong for the next PR's
> different files. If you scope `--cov`, either skip the cache or key it
> so a different changed-file set misses. For most repos, cache the
> full-tree database (Recipe A) and leave `--cov` at the source root —
> the incremental refresh already keeps it cheap.

## Recipe C: build the baseline on `main`, reuse it in PRs { #baseline-on-main }

Recipe A's `restore-keys` prefix seeds a PR from *whichever* cache is most
recent — possibly another feature branch's database. Recipe C makes the
seed deterministic: a push-to-`main` job builds one authoritative
`.coverage`, and every PR restores **that** key read-only, then
`fermut coverage` patches only its own diff on top. It's the same
architecture as [history persistence in CI](integrations.md#pr-gate-nightly) — the
main-branch job owns the artifact, PRs consume it — applied to the
coverage database instead of `history.jsonl`.

Why it's correct: a PR is `main` plus a diff, and given main's database
`fermut coverage` re-runs the changed test files *and* every test whose
contexts cover a changed source line, so the changed lines that
`--diff-only` mutates always get fresh contexts. A baseline that has
drifted behind `main` only means more files to re-measure, never a wrong
selection. And it's visible at all because **GitHub Actions lets a PR
read caches created on its base branch** — a `.coverage` built on `main`
is restorable from a PR targeting `main`.

Baseline builder — a job on push to `main` that saves the database under a
stable, run-scoped key:

```yaml
# .github/workflows/fermut-baseline.yml
name: Coverage baseline

on:
  push:
    branches: [main]

concurrency:
  group: fermut-coverage-baseline
  cancel-in-progress: true

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10  # v6.0.3
        with:
          fetch-depth: 0
      - uses: astral-sh/setup-uv@fac544c07dec837d0ccb6301d7b5580bf5edae39  # v8.2.0
        with:
          python-version: "3.12"
      - run: |
          uv sync
          uv tool install fermut
      - run: uv tool run fermut coverage --source src --tests tests/
      - uses: actions/cache/save@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
        with:
          path: |
            .coverage
            .coverage.fermut-fingerprints.json
          key: fermut-coverage-main-${{ github.sha }}
```

PR gate — restore the baseline **read-only** (no save step), then let
`fermut coverage` patch the diff:

```yaml
      - name: restore main coverage baseline
        uses: actions/cache/restore@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
        with:
          path: |
            .coverage
            .coverage.fermut-fingerprints.json
          key: fermut-coverage-main-${{ github.event.pull_request.base.sha }}
          restore-keys: |
            fermut-coverage-main-

      - run: uv tool run fermut coverage --source src --tests tests/
      # …fermut run --diff-only --coverage .coverage as usual
```

- **Read-only in PRs is the whole point.** No save step on the PR job, so
  a feature branch's coverage can never overwrite the baseline. Same rule
  the [nightly history job](integrations.md#pr-gate-nightly) follows.
- **Exact key then prefix.** The `key` targets the base commit's baseline
  directly; `restore-keys: fermut-coverage-main-` falls back to the most
  recent main baseline when that exact SHA wasn't built (e.g. a fast merge
  before the builder finished).
- **Full-tree baseline, unscoped `--cov`.** Recipe C shares one database
  across every PR, so it must measure the whole source root — don't layer
  Recipe B's `--cov` scoping onto the baseline (that warning above still
  holds).
- **A vs C.** Use A for a single active branch or a solo repo — simpler,
  one workflow. Use C when many PRs are open at once and you want each to
  start from the same clean `main` seed instead of racing over a shared
  key.
