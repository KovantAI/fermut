# Tightening the inner loop

Once you've done a baseline run via
**[First steps](../getting-started/first-steps.md)**, the next thing
to optimize is the dev-loop cycle time: how fast you can write a
test, see whether it kills the mutant you intended, and iterate.
Two flags do most of the work — `--diff-only` and `--watch` —
plus `fermut list` as the cheap dry-run companion.

## The dev-loop one-liner

```sh
fermut run src/ --tests tests/ --coverage .coverage --diff-only main --watch
```

What each flag does:

- **`--diff-only main`** — restricts mutation to lines that differ
  from `main`, so fermut only touches code you've changed on this
  branch. Massive scope reduction; most branches mutate a handful
  of lines instead of thousands.
- **`--watch`** — re-runs on every `.py` file change until Ctrl+C.
  Pair with your editor's save-on-blur for true continuous feedback.
- **`--coverage .coverage`** — keeps per-mutant test selection
  on. Same database [First steps step 2](../getting-started/first-steps.md#2-get-an-honest-baseline)
  generates via `fermut coverage`. Per-mutant cost stays low.

Together: sub-second feedback on whether the test you just wrote
actually kills the mutant you intended.

> **Needs git history.** `--diff-only` computes a merge base via
> `git diff main...HEAD`. On a shallow clone (`git clone --depth 1`
> or CI checkout with `fetch-depth: 1`) the merge base isn't
> reachable and zero mutants run. Re-clone with full history. See
> [diff-only empty](../reference/troubleshooting/diff-only-empty.md).

## Regenerating coverage

Coverage data is a snapshot of which tests touched which lines at
the moment you ran it. When you add a new test, that test isn't in
the snapshot yet — fermut won't know to run it for any mutant.
Regenerate after each new test or test-file change.

The inner-loop way is `fermut coverage`: it re-measures **only the
test files you changed** and appends them in, so adding one test
costs that file's runtime, not a full-suite sweep.

```sh
fermut coverage && fermut run src/ --tests tests/
```

That single pair is the test→coverage→mutate cycle. The full manual
pytest-cov recipe, the `.coverage`-vs-`coverage.json` tradeoff, and
the context-flag gotchas live in **[Coverage](coverage.md)**.

If coverage tooling is broken in your environment, drop the
`--coverage` flag entirely — fermut runs every test against every
mutant, slower but correct.

## `fermut list` — the dry-run companion

```sh
fermut list src/
```

Enumerates every mutant the operator catalogue would emit (after
ty / diff / coverage / op-allowlist filters), without invoking
pytest. Useful for:

- Tuning `--ops` / `--skip-ops` without paying for test runs.
- Sanity-checking `--diff-only` actually scoped to what you
  expected.
- Estimating how long a full run will take before kicking it off.

Add `--experimental` to see what the experimental op set would add.

## Profile pairing

The **`local`** profile is purpose-built for this loop — 25% mutant
sample (seed-pinned, reproducible), all stable ops, `--timeout 15`.
Switch with:

```sh
fermut init --profile local --force
```

The sample knocks total cost down by 4× at the cost of survivor
recall. Acceptable for inner loop; not for the PR gate.

## When the loop is too slow even with `--diff-only`

If `--diff-only --watch` still feels slow, three levers in order
of impact:

1. **Check `ty` is on `$PATH`.** Without it the filter chain skips
   ~30% fewer mutants. `fermut doctor` reports.
2. **Tighten `--ops`.** If you only care about boundary regressions
   today, `--ops boundary-shift,compare-op-swap` cuts mutant count
   dramatically.
3. **Drop `--sample` to 10%** for stress-iteration sessions:
   `--sample 0.1`. Less recall, faster cycle.

For the team-scale equivalent (PR gate, nightly sweep), see
**[Working on projects](projects.md)** and
**[CI quickstart](ci-quickstart.md)**.

## Where next

- **[Working on projects](projects.md)** — when you're ready to
  share the score with the team.
- **[Survivor triage](survivor-triage.md)** — what to do with each
  surviving mutant the inner loop surfaces.
- **[Coding agents](coding-agents.md)** — same flags, agent-driven
  cadence and reward shaping.
- **[`fermut run` reference](../reference/cli/run.md)** — every flag.
