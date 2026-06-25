# Coverage

How to enable coverage-based per-mutant test selection — the single
biggest wall-clock win on slow test suites.

`coverage` and `pytest-cov` are
[required prerequisites](../getting-started/installation.md#prerequisites);
`fermut doctor` reports either as `[fail]` when missing. The reason
is below.

## The easy path: `fermut coverage`

You don't have to remember the pytest-cov incantation. Run:

```sh
fermut coverage
```

It discovers your source and tests from `fermut.toml` / `pyproject.toml`
(same walk as `run`), runs the suite under coverage, and writes a
`.coverage` database fermut reads directly.

The first run measures the whole suite. **Every run after that only
re-measures the test files you changed** and appends them in — so
adding one test costs that file's runtime, not a full-suite sweep:

```sh
# edit tests/test_orders.py, add a case…
fermut coverage            # re-runs only test_orders.py, updates .coverage
fermut run --coverage .coverage
```

This solves the "coverage goes stale the moment I add a test" problem:
re-running the entire suite for one new test is exactly what `fermut
coverage` avoids. Modified tests are handled correctly too — their old
coverage is purged before re-measuring, so a test that now touches
fewer lines can't leave behind phantom coverage.

The rest of this page covers the manual pytest-cov recipe (for CI
pipelines that already generate coverage their own way) and how the
filter uses the result.

## Reading `.coverage` directly vs `coverage.json`

fermut accepts **either** coverage.py's native `.coverage` SQLite
database **or** a `coverage.json` export — it sniffs the file's format,
so `--coverage .coverage` and `--coverage coverage.json` both work.

Prefer `.coverage` (what `fermut coverage` writes) on large suites. The
JSON export repeats every pytest node id on every line it touched; a
test crossing 500 lines stores its id 500 times. A multi-GB
`coverage.json` is typically tens of MB as SQLite, because the database
stores each node id once and records lines as a compact bitmap. fermut
also skips the slow `coverage json` export step entirely when reading
the database.

## Why coverage matters

Without coverage, fermut runs the **entire** test suite against every
mutant. For a project with 500 mutants and a 30-second test suite,
that's 4 hours of CI per run.

With coverage, fermut runs only the tests whose execution **touched
the mutated line**. The same 500 mutants might cost 5 minutes
because most of them only need 2–3 tests.

The slow path isn't just slower — it's the difference between
"runnable in a PR loop" and "runnable nightly only." That's why
coverage is a hard requirement, not a tuning knob.

## Generate `coverage.json` with per-test contexts

The contexts are mandatory — they're how fermut knows *which* test
exercised the line, not just *that* some test did.

```sh
pytest --cov=src --cov-context=test
coverage json -o coverage.json --show-contexts
```

`--cov-context=test` (from the `pytest-cov` plugin) tags each covered
line with the pytest nodeID — `tests/foo_test.py::test_bar` — which is
exactly the selector fermut feeds back to pytest when narrowing the
run per mutant.

> **Watch out for `coverage run --context=test`.** Coverage.py's own
> `--context=LABEL` flag sets a *single static label* for the whole run
> — every line gets tagged `"test"`, with no per-test resolution. It
> looks like it works (`--show-contexts` produces a non-empty file)
> but fermut can't select tests from it. Use the `pytest-cov` recipe
> above.
>
> The other dynamic option, `dynamic_context = test_function` in
> `.coveragerc`, *does* produce per-test contexts — but as dotted
> Python module paths (`tests.foo_test.test_bar`), not pytest
> nodeIDs. fermut feeds these straight to pytest as test selectors,
> which then fails with "file or directory not found" because the
> dotted form isn't a valid pytest selector. Stick with pytest-cov.

## Wire it into `fermut run`

```sh
fermut run src/ --tests tests/ --coverage coverage.json
```

Or set it persistently in `fermut.toml`:

```toml
coverage = "coverage.json"
```

The `pr-gate` profile sets this by default.

## What the filter does

For each mutant:

1. Find the source line the mutant targets.
2. Look up the per-test contexts that hit that line.
3. If **no test** touched the line → the mutant is skipped (it would
   always survive — there's nothing to kill it).
4. Otherwise, narrow the pytest invocation to **only those tests**
   via `-k` or `--last-failed-no-failures none` plus an explicit
   nodeID list.

Mutants dropped at step 3 appear in the JSON report as outcome
`skipped` with `filter: "coverage"`, alongside skips from the ty /
ruff / diff / sample filters. Counted in the `skipped` summary; not
counted toward the mutation score.

## Refreshing the coverage file

Coverage becomes stale when you add a test or move code between files.

Locally, the fast refresh is `fermut coverage` — it re-measures only
the changed test files and updates `.coverage` in place:

```sh
fermut coverage && fermut run src/ --tests tests/
```

In CI, regenerate at the start of every run. The manual recipe (when
your pipeline produces coverage its own way):

```sh
pytest --cov=src --cov-context=test && \
  coverage json -o coverage.json --show-contexts && \
  fermut run src/ --tests tests/
```

A stale `coverage.json` (or `.coverage`) wrongly skips mutants on lines
your new tests exercise — always refresh before a mutation run that
follows a test change.

## CI: combining coverage + diff-only

The PR-gate sweet spot is **both** filters at once:

```sh
fermut run src/ --tests tests/ \
    --diff-only origin/main \
    --coverage coverage.json
```

- `--diff-only` narrows the *mutants* to changed lines.
- `--coverage` narrows the *tests* per remaining mutant.

This is what the `pr-gate` profile sets. See
**[Integrations → GitHub Actions](integrations.md#github-actions)**
for the full workflow.

## Pitfalls

- **Not enough contexts.** Some pytest plugins (e.g. `pytest-xdist`
  with `-n auto`) can break context attribution. Run coverage
  single-process at least once on each branch.
- **Coverage of integration tests.** If your tests hit `src/` only
  via a network call (FastAPI test client, etc.), the per-test
  context will see the right lines — but only if coverage is
  collected *in-process*. Subprocess tests need extra setup.
- **Stale coverage file.** A mutant on a line your fresh test
  exercises will be wrongly skipped if `coverage.json` predates
  the test. Re-generate.

## Where next

- **[Filters](filters.md)** — `--diff-only`, `--since`, `--ops`,
  `--sample`, inline ignore markers.
- **[CLI → coverage integration](../reference/cli/run.md#coverage-integration)**
  — exact flag behavior.
