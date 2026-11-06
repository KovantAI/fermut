# Rolling out to a team

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
fermut init
fermut baseline --full --format json > baseline.json
```

[`fermut baseline`](../reference/cli/baseline.md) checks the environment,
builds per-test coverage, and scores every covered mutant (`--full`; drop
it for a quick 10% sample). It reports line coverage next to the mutation
score, lists the files with the most survivors, and logs the run as run
zero in the [trend](trends.md). For the per-survivor list, follow with a
full `fermut run src/ --tests tests/ --json run.json`.

A few things you might see:

- **Score below 50%.** Common in projects with primarily integration
  tests. Don't panic — mutation testing measures something stricter
  than line coverage, so the first number is almost always lower than
  expected.
- **Lots of `errored` outcomes.** Likely means your test suite has
  side-effects on the source tree (writes to files inside `src/`,
  in-process patches that don't survive `fork`). See the
  [`--isolation` modes](../reference/cli/run-isolation.md#isolation-modes); usually
  switching to `copy` mode fixes it.
- **Lots of timeouts.** Some test fixtures don't terminate well under
  arithmetic mutants. Raise `--timeout`; tune later.

Keep `baseline.json` — it's the "before" snapshot you'll
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
    selection, narrow op set (arith / compare / boundary / return-value-to-none),
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
    cycles of seconds.

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

Run the gate on every PR, but don't let it block merges yet. With the
[packaged action](github-action.md), set `blocking: "false"`: a failing
gate becomes a warning, while a broken setup still fails the job.

```yaml
      - uses: KovantAI/fermut@v0.7.0
        with:
          blocking: "false"
```

With a hand-written workflow from [CI recipes](integrations.md#github-actions),
add `continue-on-error: true` to the job instead. It also turns setup
failures green, so don't leave it on longer than you need.

Watch the comments roll in for a week. You'll see:

- the score's natural variance across PRs;
- which kinds of survivors keep showing up (boundary cases? returned-None
  cases?);
- whether `pr-gate`'s op subset is too narrow for your codebase.

## Step 4 — Flip the gate on, scoped to diff-only

Once the noise level feels manageable, let the job block merges: remove
`blocking: "false"` (or `continue-on-error`). Keep the gate on changed
lines only, so pre-existing survivors don't block PRs that didn't
introduce them. The packaged action always mutates the PR's diff; the
`pr-gate` profile's `diff_only = "main"` does the same for a hand-written
workflow.

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
| PR gate            | every PR       | `pr-gate`           | `--coverage .coverage --markdown report.md --trend`                           |
| Nightly sweep      | cron / manual  | `pr-gate` (default) | `--no-diff-only --experimental --ops "" --timeout 60` (whole tree, all ops)  |
| Sharded full sweep | push to main   | `pr-gate`           | `--no-diff-only --shard <i>/<n> --json shard-<i>.json`, plus a merge job     |

`pr-gate` writes `diff_only = "main"`; full sweeps need `--no-diff-only`
or a run on `main` diffs against itself and mutates nothing.

The "one config file + per-job overrides" pattern keeps everything in
one place; the `--profile` template just opinionates the defaults.
For the full two-workflow recipe with history sharing between PR and
nightly jobs, see
**[PR gate + nightly (recommended pair)](integrations.md#pr-gate-nightly)**.

## History persistence in CI

CI runners start empty, so the score history needs persisting for
`--trend` to have anything to compare. The packaged
[`trend` action](github-action.md#score-trend) keeps it on a branch; for
hand-written workflows see
[CI recipes → History persistence](integrations.md#history-persistence).

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
