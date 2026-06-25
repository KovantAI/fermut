# First steps

Five minutes from zero to a mutation report. Assumes fermut is installed
— see **[Installation](installation.md)** if not.

If you've never used mutation testing before, the short pitch: line and
branch coverage tells you which lines tests *execute*; mutation testing
tells you which lines tests *check*. fermut mutates the source, re-runs
your tests, and reports which mutations survived — those are the gaps
your tests don't actually catch.

This page walks the **solo loop**: one developer, one machine, no CI.
That's the fastest path to seeing whether fermut earns its keep on your
codebase. Once you're convinced, **[Working on projects](../guides/projects.md)**
covers the team rollout (baseline → measure-only CI → gate → tighten).

## 1. Initialize the project

`fermut init` walks up from the current directory to find the project root
(any ancestor containing `pyproject.toml` or `setup.cfg`), detects the
source/tests layout, checks which test runner is on `PATH`, and writes a
config tuned to the repo.

```sh
cd path/to/your/project
fermut init
```

Output:

```
fermut init — detected:
  project root : /work/project
  source root  : src
  tests        : tests
  runner       : pytest
  ty filter    : on
  coverage     : wired (coverage detected in project)
  pkg manager  : Uv
  python files : 142 (Medium)
  profile      : auto (size-based: Medium)

wrote /work/project/fermut.toml
```

`coverage: wired` means `init` wrote `coverage = "coverage.json"` into the
config (it detected `pytest-cov` / a coverage setup) — the JSON itself is
produced by the next step. If no coverage dependency were found you'd see
`coverage: off (no coverage dependency found)` here and `init` would skip
the key (and `fermut doctor` would surface a `[fail]` for the missing
`pytest-cov`).

`init` is non-interactive and idempotent. Re-run with `--force` to
overwrite, `--dry-run` to preview without touching disk.

For the solo loop, the auto-detected defaults are fine. Profile choice
(`pr-gate`, `nightly`, `local`, `library`) matters once you're wiring CI
— covered in **[Working on projects](../guides/projects.md#step-2-pick-a-profile)**.

## 2. Get an honest baseline

Generate per-test coverage first. fermut uses it to run only the tests
whose execution touched the mutated line — biggest wall-clock win on any
non-trivial suite.

You'll need `coverage` and `pytest-cov` installed (see
**[Installation prerequisites](installation.md#prerequisites)** if you
skipped it). Quick sanity check: `coverage --version`.

Generate the per-test coverage JSON:

```sh
pytest --cov=src --cov-context=test
coverage json -o coverage.json --show-contexts
```

`--cov-context=test` is what tags each covered line with the pytest
nodeID of the test that touched it. Without it, fermut knows the line
is covered but not by *which* test, so it falls back to running every
test against every mutant. See
**[Coverage](../guides/coverage.md)** for the full setup (and the
one common footgun if your `coverage.json` ends up with a single
static label instead of per-test contexts).

Then baseline. No `--diff-only`, no CI — just an absolute number to
anchor everything else.

```sh
fermut run src/ --tests tests/ --coverage coverage.json --json baseline.json
```

`fermut run` streams progress logs (the `INFO fermut::…` lines) to
**stderr**; the report goes to **stdout**. That's why piping to a file or
adding `-q` (quiet) gives just the clean report — the sample outputs shown
below are the `-q` form. The `INFO` lines aren't errors.

**How long should this take?** Rough wall-clock anchors for a fresh
run (no cache, default profile, `ty` filter on):

| Project size                     | Expected wall-clock      |
|----------------------------------|--------------------------|
| ~10 source files, ~50 tests      | 10–30 seconds            |
| ~50 source files, ~500 tests     | 1–5 minutes              |
| ~500 source files, ~5000 tests   | 15–45 minutes            |

If your run is dramatically slower than this on first pass, the
usual suspect is the coverage filter not being applied (no per-test
contexts — see [coverage-rejected](../reference/troubleshooting/coverage-rejected.md)).
A missing `ty` won't just slow the run — it aborts before any
mutant is tested; see
[ty-not-found](../reference/troubleshooting/ty-not-found.md).
Re-runs are much faster thanks to the AST-hash cache — typically
80–95% cache hit on a no-source-change re-run.

> **Coverage generation failed?** Fix it before continuing — fermut
> treats `coverage` + `pytest-cov` as required and `fermut doctor`
> reports them under `[fail]`, not `[warn]`. The most common causes
> are a forgotten venv activation (so `pytest-cov` isn't importable
> from the `python3` on `PATH`), or stale legacy mutation artifacts
> like `mutants/` breaking pytest collection (pass `--ignore=mutants`).
> See **[Coverage](../guides/coverage.md)** for runner-specific setup.

Output — a line per survivor/timeout/error, then a one-line summary:

```
SURVIVED  src/auth.py:42 [boundary-shift] `age >= 18` → `age > 18`
TIMEOUT   src/slow.py:88 [arith-op-swap] `n * 2` → `n / 2`

152 mutants — killed: 140, survived: 8, timeout: 1, skipped: 3 (coverage 3), equivalent: 0, errored: 0  | score: 94.6%
```

The `skipped` count breaks down by the filter that dropped each mutant
(`operator`, `parity`, `experimental`, `coverage`, `ty`, …) so a large
total reads as deliberate filtering, not mutants silently going untested.

`detected = killed + timeout`; skipped and equivalent mutants are excluded
from both sides of the ratio (here `141 / 149 = 94.6%`). See
**[mutation score](../concepts/mutation-testing.md#the-mutation-score)**
for the formula.

Don't panic if the first score is below 50%. On a suite that has
never been mutation-tested, **first runs typically land between
40% and 70%** — that's the size of the gap between "tests exist"
and "tests would catch a regression," not a tooling failure. The
number to watch is the trend, not the absolute floor. Save
`baseline.json` — it's the "before" snapshot.

Exit code is non-zero when any mutant survives, which is how `fermut run`
slots into CI as a gate later. For now, ignore it.

### What can go wrong on the first run

The most common surprises, in roughly the order people hit them:

- **Lots of `errored` outcomes.** Usually means your tests have
  side effects on the source tree — they write files inside `src/`,
  patch in-process state that doesn't survive `fork`, or rely on
  cwd. Switch to `--isolation copy` (slower, safer). See
  [errored outcomes](../reference/troubleshooting/errored-outcomes.md).
- **Score wobbles between identical runs.** Hypothesis seed isn't
  pinned, or you have order-dependent tests. Pin
  `hypothesis_seed` in `fermut.toml`; investigate flaky tests.
  See [score swings](../reference/troubleshooting/score-swings.md).
- **`--diff-only` produces zero mutants.** Shallow clone (`git
  clone --depth 1`, or CI checkout with `fetch-depth: 1`) means
  the merge base isn't reachable. Re-clone with full history. See
  [diff-only empty](../reference/troubleshooting/diff-only-empty.md).
- **Lots of timeouts.** Some tests don't terminate cleanly under
  arithmetic mutants (infinite loops on boundary-shifted
  conditions). Raise `--timeout`; tune later.
- **Score lower than expected.** Not a bug. See the "40–70% on a
  suite that has never been mutation-tested" note above the
  "What can go wrong" header.
- **Run is much slower than the wall-clock table.** Check that
  `ty` is installed and on `$PATH`, and that `--coverage` is being
  honored (the JSON has per-test contexts).
- **`pytest` fails to collect before fermut even starts.** A previous
  mutation tool may have left a top-level `mutants/` (mutmut) or
  similar directory containing duplicate test-module basenames. The
  duplicates trigger collection errors like *"No module named
  'foo.bar_test'"* across dozens of files. Add the legacy directory
  to `--ignore=` (e.g. `pytest --ignore=mutants`) or delete it before
  generating coverage. `fermut doctor` flags this under
  `legacy-artifacts` when it detects the dir.
- **Score is 100% with thousands of `skipped` and no tests actually
  running.** `coverage.json` was produced with `coverage run
  --context=test` (a static label, not per-test contexts) or with
  `dynamic_context = test_function` (dotted Python names, not pytest
  nodeIDs). Regenerate with `pytest --cov=src --cov-context=test`.
  See [Coverage](../guides/coverage.md#generate-coveragejson-with-per-test-contexts).

For everything else, see the
[troubleshooting reference](../reference/troubleshooting/index.md).

## 3. Inspect survivors

```sh
fermut show baseline.json            # list survivors (default)
fermut show baseline.json --all      # list every outcome
fermut show baseline.json 42         # detail view for outcome #42
fermut show baseline.json my-mutant  # match by substring of mutant id
```

Each mutant has a stable identifier called its
[`mutant.id`](../concepts/glossary.md#mutant-id) — it follows the grammar
`<file>@<byte-offset>:<operator>:<original>-><replacement>`, e.g.
`src/calculator.py@216:boundary-shift:<=-><`. It's the key the cache and the
sharded-run dedup use, and it's what `fermut show` matches when
you pass a substring as the selector.

The detail view prints file:line, operator, status, the mutation range,
and a unified diff against the working copy. Open the top survivor,
write a test that fails on that diff, re-run. That's the inner loop.

For the full decision tree on each survivor — kill it, mark it as
equivalent, or delete the dead code — see
**[Survivor triage](../guides/survivor-triage.md)**.

For richer reports later:

```sh
fermut run src/ --tests tests/ \
    --json report.json \
    --html report.html \
    --markdown report.md
```

The Markdown file is what `fermut pr-comment` posts to a PR. The HTML
file is self-contained — open locally or upload as a CI artifact.

## 4. Tighten the loop

Once you can read survivors, the next thing to optimize is cycle
time. The dev-loop one-liner:

```sh
fermut run src/ --tests tests/ --coverage coverage.json --diff-only main --watch
```

`--diff-only main` scopes mutation to your branch's changes;
`--watch` re-runs on every save. Full walkthrough including
coverage regeneration, `fermut list` dry-runs, and "when the loop
is still too slow" levers lives in
**[Tightening the inner loop](../guides/inner-loop.md)**.

## 5. Watch the trend

`fermut trend` reads `.fermut/history.jsonl` (one line per run, appended
automatically) and prints score trajectory:

```sh
fermut trend
```

```
history: /work/project/.fermut/history.jsonl (5 entries)

score: ▅▆▇▇▇   62.5% → 92.0%  (+29.5 pts)

  timestamp              score  killed  survived  timeout    delta git
  2026-05-01T10:00:00    62.5%      10        6        0         — main@aaa1111
  2026-05-03T10:00:00    71.4%      15        6        0      +8.9 main@bbb2222
  2026-05-10T10:00:00    80.0%      20        5        0      +8.6 main@ccc3333
  2026-05-20T10:00:00    85.7%      24        4        0      +5.7 main@ddd4444
  2026-06-01T10:00:00    92.0%      46        4        0      +6.3 features/coverage@eee5555
```

After a handful of inner-loop iterations, run `fermut trend --limit 10`
in another terminal to see whether the score moves the right direction.
Tracking the trajectory is more useful than chasing a fixed target.

## Next — rolling out to the team

Solo loop proves the value on one branch. To make it stick across the team:

- **[Working on projects](../guides/projects.md)** — five-step rollout:
  baseline → profile → measure-only CI → gate → tighten. Tells you how
  to avoid the "ship a noisy gate, team hates mutation testing" trap.
- **[Coding agents](../guides/coding-agents.md)** — if AI agents are
  writing tests in your workflow, this is the tuning guide for the
  inner loop they should run.
- **[Features](features.md)** — short tour of what else fermut can do.
- **[Concepts → mutation testing](../concepts/mutation-testing.md)** —
  what the score actually means.
- **[CLI reference](../reference/cli/index.md)** — every flag.
- **[Configuration reference](../reference/configuration.md)** — every
  TOML key.
