# Working on projects

How to introduce mutation testing into a real project without producing
weeks of survivor triage. This page targets human-driven rollouts. If
you're integrating fermut into an **AI coding agent's** workflow, start
with **[Coding agents](coding-agents.md)** — same underlying tool,
different cadence and tuning.

The short version:

1. Start in **measure-only** mode — no gate, no exit-1 — and get an
   honest baseline.
2. Pick a profile that matches *one* scenario you want to support
   first; usually `pr-gate`.
3. Wire it into CI behind `continue-on-error`. Watch the score for a week.
4. Flip the gate on for new code only (`--diff-only`).
5. Slowly tighten: add operators, add the nightly variant, raise the
   target.

Sections below expand each step.

## Step 1 — Get an honest baseline

Before adding any gate, run fermut once against your whole project to see
where you actually stand. **Do not enable `--diff-only` yet.** The goal
here is the absolute number, not the delta.

```sh
fermut init --profile local --force
# generate per-test coverage first — fermut uses it to skip irrelevant tests per mutant
# (or just `fermut coverage`; see the Coverage guide for the recipe + tradeoffs)
pytest --cov=src --cov-context=test && coverage json -o coverage.json --show-contexts
fermut run src/ --tests tests/ --coverage coverage.json --json baseline.json
```

See **[Coverage](coverage.md)** for `fermut coverage`, the
`.coverage`-vs-`coverage.json` tradeoff, and context-flag gotchas.

A few things you might see:

- **Score below 50%.** Common in projects with primarily integration
  tests. Don't panic — mutation testing measures something stricter
  than line coverage, so the first number is almost always lower than
  expected.
- **Lots of `errored` outcomes.** Likely means your test suite has
  side-effects on the source tree (writes to files inside `src/`,
  in-process patches that don't survive `fork`). See the
  [`--isolation` modes](../reference/cli/run.md#isolation-modes); usually
  switching to `copy` mode fixes it.
- **Lots of timeouts.** Some test fixtures don't terminate well under
  arithmetic mutants. Raise `--timeout`; tune later.

Save `baseline.json` somewhere — it's the "before" snapshot you'll
compare future runs to.

## Step 2 — Pick a profile

Every team adopting fermut wants different things. The four profiles
optimize for different starting points:

=== "PR gate"

    Goal: **fast, narrow signal** on every PR. Catches regressions on
    changed lines without flagging pre-existing tech debt.

    ```sh
    fermut init --profile pr-gate --with-gha --force
    ```

    Defaults: diff-restricted to the merge base, per-test coverage
    selection, narrow op set (arith / compare / boundary / return-to-none),
    `--timeout 15`, Hypothesis seed pinned.

    Best first profile for most teams. Doesn't surface existing
    survivors — only flags new ones the PR introduces.

=== "Nightly"

    Goal: **catch drift between PR gates**. Some mutants that survive
    on the broader codebase never get touched by a PR. Nightly catches
    them.

    ```sh
    fermut init --profile nightly --force
    ```

    Defaults: every operator (including experimental), `--timeout 60`,
    no diff filter, no coverage filter.

    Pair with `pr-gate` — the typical setup is *both*: PRs use pr-gate,
    the cron pipeline runs nightly. See
    [CI integration](#ci-integration) below.

=== "Local"

    Goal: **dev-loop feedback**. You're iterating on tests; you want
    sub-second cycles.

    ```sh
    fermut init --profile local --force
    fermut run src/ --tests tests/ --watch
    ```

    Defaults: 25% mutant sample (seed pinned for reproducibility), no
    coverage filter, `--timeout 15`, all stable ops.

=== "Library"

    Goal: **lib-author workflow**. You publish a wheel; you want
    deterministic survivor lists across CI runs and contributor
    machines.

    ```sh
    fermut init --profile library --force
    ```

    Defaults: every stable op (no experimental noise), Hypothesis seed
    pinned, `--timeout 30`.

You can switch profiles at any time:

```sh
fermut init --profile <other> --force
```

Profiles are not mutually exclusive — they're starter templates. After
`init`, the generated `fermut.toml` is plain text; hand-edit anything.

## Step 3 — Wire CI in measure-only mode

Start with `continue-on-error: true`. The job records mutation runs and
posts a PR comment but **does not block merges yet**.

The workflow below has two output-related steps: `fermut run` writes
`fermut-report.md` to disk, then a separate `fermut pr-comment` step at
the bottom uploads that file as a sticky PR comment. The markdown is
the *intermediate artifact*, not the comment itself.

```yaml
# .github/workflows/mutation-measure.yml
name: Mutation (measure-only)

on:
  pull_request:
    paths:
      - "**/*.py"

jobs:
  measure:
    runs-on: ubuntu-latest
    continue-on-error: true            # <-- not yet a gate
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
      - run: |
          uv run coverage run -m pytest
          uv run coverage json -o coverage.json
      - run: |
          uv tool run fermut run src/ \
              --tests tests/ \
              --coverage coverage.json \
              --markdown fermut-report.md \
              --trend
      - if: always() && hashFiles('fermut-report.md') != ''
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          PR_NUMBER: ${{ github.event.pull_request.number }}
        run: uv tool run fermut pr-comment --markdown fermut-report.md
```

Watch the comments roll in for a week. You'll see:

- the score's natural variance across PRs;
- which kinds of survivors keep showing up (boundary cases? returned-None
  cases?);
- whether `pr-gate`'s op subset is too narrow for your codebase.

## Step 4 — Flip the gate on, scoped to diff-only

Once the noise level feels manageable, remove `continue-on-error` so the
job blocks merges. Make sure `--diff-only` is on so pre-existing
survivors don't suddenly block PRs that didn't introduce them.

```diff
 jobs:
   gate:
     runs-on: ubuntu-latest
-    continue-on-error: true
     steps:
       ...
       - run: |
           uv tool run fermut run src/ \
               --tests tests/ \
+              --diff-only origin/${{ github.base_ref }} \
               --coverage coverage.json \
               --markdown fermut-report.md \
               --trend
```

The default `pr-gate` profile already sets `diff_only = "main"`, so if
you initialized with `--profile pr-gate` this is already in your config
file.

## Step 5 — Tighten

After the gate has been stable for a few weeks, you can start tightening:

- **Widen the operator set.** Remove `--ops` from the profile, or add
  `--experimental` for exception/loop ops.
- **Add a nightly variant.** Same `fermut.toml`, different CLI flags.
  See the [nightly override pattern](../reference/cli/run.md#cli-overrides-vs-config).
- **Lower `--timeout`** as you optimize slow tests — most teams can
  drop from 30s to 15s as they tune.
- **Track the trend.** `fermut trend` shows whether the score is moving
  in the right direction over time, which is more useful than chasing
  a fixed target. The PR comment's `--trend` block surfaces the delta
  vs the previous run.

## CI integration

The full [CI playbook](../reference/cli/run.md#cli-overrides-vs-config) is in the
CLI reference. The most common shapes:

| Job                | Trigger        | Profile (config)    | CLI overrides                                                                 |
|--------------------|----------------|---------------------|------------------------------------------------------------------------------|
| PR gate            | every PR       | `pr-gate`           | `--coverage coverage.json --markdown report.md --trend`                       |
| Nightly sweep      | cron / manual  | `pr-gate` (default) | `--experimental --ops "" --timeout 60` (overrides the narrow op set)          |
| Sharded full sweep | push to main   | `pr-gate`           | `--shard <i>/<n> --json shard-<i>.json` per matrix entry, plus a merge job   |

The "one config file + per-job overrides" pattern keeps everything in
one place; the `--profile` template just opinionates the defaults.
For the full two-workflow recipe with history sharing between PR and
nightly jobs, see
**[PR gate + nightly (recommended pair)](integrations.md#pr-gate-nightly)**.

## History persistence in CI

Trend tracking is observational data. GitHub Actions runners are
ephemeral, so `.fermut/history.jsonl` resets every run unless you
persist it. The simplest pattern: `actions/cache` keyed on the branch
ref, with a `main`-branch save-only step.

```yaml
- name: restore fermut history
  uses: actions/cache@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
  with:
    path: .fermut/history.jsonl
    key: fermut-history-${{ github.ref_name }}-${{ github.sha }}
    restore-keys: |
      fermut-history-${{ github.ref_name }}-
      fermut-history-main-

- name: run fermut
  run: fermut run src/ --tests tests/ --markdown report.md --trend

- name: save fermut history
  if: always() && github.ref == 'refs/heads/main'
  uses: actions/cache/save@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
  with:
    path: .fermut/history.jsonl
    key: fermut-history-main-${{ github.sha }}
```

Only the `main`-branch job *saves* the cache; PR jobs are read-only,
so feature branches don't fork their own history streams. Tune to
taste — some teams prefer committing `history.jsonl` to a side branch
instead.

## Common pitfalls (process)

Process-level mistakes that hurt rollouts. For symptom-level
problems on individual runs (errored outcomes, timeouts, score
swings, `--diff-only` returning empty), see
[First steps → What can go wrong](../getting-started/first-steps.md#what-can-go-wrong-on-the-first-run).

- **Don't enable the gate before the baseline run.** You will surface a
  large initial survivor pile that has nothing to do with the PR. People
  hate mutation testing after that experience.
- **Don't run without `--coverage` on slow test suites.** A mutant that
  no test reaches will always survive — and running every test against
  every mutant burns CI time for no signal.
- **Don't pin the score target high too early.** Aim for a moving
  trendline, not a fixed number. Use `fermut trend` to make the
  trajectory visible.
- **Don't auto-install equivalent-mutant filters from someone else's
  config.** Equivalence is project-specific; one team's "equivalent" is
  another team's bug.
