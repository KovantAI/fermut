# CI recipes

Hand-written workflows for full control over each step, and for CI
systems other than GitHub Actions. On GitHub Actions, the
**[packaged action](github-action.md)** (`uses: KovantAI/fermut@<tag>`)
does all of this behind a few inputs: install, coverage, caching, gate,
summary, sticky comment and sharding. Start there unless you need
something it doesn't do.

## GitHub Actions

A PR-gate workflow with caching and a score trend. `fermut init
--with-gha` writes a simpler variant at `.github/workflows/fermut.yml`
(the same file as
[`examples/github-actions/pr-gate.yml`](https://github.com/KovantAI/fermut/blob/main/examples/github-actions/pr-gate.yml)):
it has no cache or history steps, no `--trend`, and adds
`--hypothesis-seed` and a JSON report artifact. This version adds the
result cache and reads the nightly history:

```yaml
name: Mutation

on:
  pull_request:
    paths:
      - "**/*.py"
      - "pyproject.toml"
      - "fermut.toml"

permissions:
  contents: read
  pull-requests: write   # sticky PR comment

jobs:
  gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1  # v7.0.1
        with:
          fetch-depth: 0
      - uses: astral-sh/setup-uv@68559e99cc807c1eb7e9238fd43a15bba460eec2  # v10.3.3
        with:
          python-version: "3.12"
      - run: |
          uv sync
          uv pip install pytest-cov   # `fermut coverage` runs `pytest --cov` in this venv
          uv tool install fermut
      - run: uv tool run fermut coverage --source src --tests tests/
      - name: restore nightly history (read-only)
        uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9  # v6.1.0
        with:
          path: .fermut/history.jsonl
          key: fermut-history-main
          restore-keys: |
            fermut-history-main-
      - uses: actions/cache@55cc8345863c7cc4c66a329aec7e433d2d1c52a9  # v6.1.0
        with:
          path: .fermut/cache.json
          key: fermut-cache-${{ runner.os }}-${{ hashFiles('uv.lock', 'src/**/*.py', 'tests/**/*.py') }}
          restore-keys: |
            fermut-cache-${{ runner.os }}-
      - run: |
          uv tool run fermut run src/ \
              --tests tests/ \
              --diff-only origin/${{ github.base_ref }} \
              --coverage .coverage \
              --markdown fermut-report.md \
              --trend
      - if: always() && hashFiles('fermut-report.md') != ''
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          PR_NUMBER: ${{ github.event.pull_request.number }}
        run: uv tool run fermut pr-comment --markdown fermut-report.md
```

Key points:

- `fermut coverage` runs the suite and writes the `.coverage`
  database fermut reads directly (no `coverage json` export step). The
  workflow passes `--coverage .coverage` explicitly; at the project root
  fermut also auto-discovers it.
- `fetch-depth: 0` is required so `--diff-only` can compute the
  merge base.
- `actions/cache` keyed on lockfile + sources persists the result
  cache across PRs.
- `actions/cache/restore` loads the history the nightly job saved, so
  `--trend` has earlier runs to compare against. It never saves, so PR
  runs don't overwrite the main-branch history.
- `pr-comment` posts a sticky comment that updates in place on
  re-runs.

**Measure-only first.** To watch the score for a while before it can
block merges, add `continue-on-error: true` to the job. It also hides
setup failures, such as a broken install or coverage step, so remove it
once the score settles. (The packaged action's `blocking: "false"` only
softens the gate.) The rollout steps are in
**[Rolling out to a team](projects.md#step-3-wire-ci-in-measure-only-mode)**.

## Sharded full sweep

For long-running nightly runs, parallelize via `--shard`:

```yaml
jobs:
  shard:
    strategy:
      fail-fast: false
      matrix:
        shard: [1, 2, 3, 4]
    runs-on: ubuntu-latest
    steps:
      - run: |
          fermut run src/ --tests tests/ \
              --shard ${{ matrix.shard }}/4 \
              --json shard-${{ matrix.shard }}.json
      - uses: actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9  # v7.0.2
        if: always()  # shards with survivors exit 1; still upload
        with:
          name: shard-${{ matrix.shard }}
          path: shard-${{ matrix.shard }}.json

  merge:
    needs: shard
    if: always()            # shards exit 1 on survivors; merge must still run
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333  # v8.0.2
        with:
          path: shards
          merge-multiple: true   # flatten shard-N/ dirs into shards/
      - run: fermut merge shards/shard-*.json --markdown report.md
```

A complete version (checkout, install, per-shard cache) lives in
[`examples/github-actions/sharded-matrix.yml`](https://github.com/KovantAI/fermut/blob/main/examples/github-actions/sharded-matrix.yml).
If `fermut.toml` sets `diff_only` (the `pr-gate` profile does), add
`--no-diff-only` to the shard `run` so the sweep covers the whole tree.

Shards are deterministic per `mutant.id` — no coordination, no
overlap.

`fermut merge` requires every shard's report and refuses to guess: a
missing shard (a mutant left with only its `shard` placeholder) or two
inputs that disagree on the same mutant both error out instead of
scoring a partial or ambiguous universe. Pass all N shard reports;
re-running an identical report is a harmless idempotent merge. For the
same reason, merging a *single* shard report on its own errors — its
out-of-slice placeholders have no real verdict to resolve them. To
convert one shard's report to another format, render it from the
producing `run` instead.

## PR gate + nightly (recommended pair) { #pr-gate-nightly }

The PR gate catches regressions on changed lines fast. Nightly catches
drift on the rest of the codebase that PRs never touch. They share
one `fermut.toml`; the difference is CLI flags on the `run` step.

**Shared config** — initialize once with the `pr-gate` profile:

```sh
fermut init --profile pr-gate --with-gha --force
```

This generates `.github/workflows/fermut.yml` (the simpler PR gate
described at the top of the section above; swap in that section's YAML
if you want caching and the history restore). Add a second file alongside it:

```yaml
# .github/workflows/fermut-nightly.yml
name: Mutation (nightly)

on:
  schedule:
    - cron: "0 6 * * *"   # 06:00 UTC daily
  workflow_dispatch:

concurrency:
  group: fermut-nightly
  cancel-in-progress: false

jobs:
  sweep:
    runs-on: ubuntu-latest
    timeout-minutes: 180
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1  # v7.0.1
        with:
          fetch-depth: 0
      - uses: astral-sh/setup-uv@68559e99cc807c1eb7e9238fd43a15bba460eec2  # v10.3.3
        with:
          python-version: "3.12"
      - run: |
          uv sync
          uv pip install pytest-cov
          uv tool install fermut
      - run: uv tool run fermut coverage --source src --tests tests/

      # Verdict cache + smart-ordering history: without them every nightly
      # runs cold. Unchanged files are answered from cache.json.
      - name: restore fermut cache
        uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9  # v6.1.0
        with:
          path: |
            .fermut/cache.json
            .fermut/kill-order.json
          key: fermut-nightly-cache
          restore-keys: |
            fermut-nightly-cache-

      - name: restore fermut history
        uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9  # v6.1.0
        with:
          path: .fermut/history.jsonl
          key: fermut-history-main
          restore-keys: |
            fermut-history-main-

      - name: full sweep (override pr-gate narrowing)
        run: |
          uv tool run fermut run src/ \
              --tests tests/ \
              --coverage .coverage \
              --no-diff-only \
              --ops "" \
              --timeout 60 \
              --markdown nightly-report.md \
              --json nightly.json \
              --trend

      - name: save fermut history
        if: always()
        uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9  # v6.1.0
        with:
          path: .fermut/history.jsonl
          key: fermut-history-main-${{ github.run_id }}

      - name: save fermut cache
        if: always()
        uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9  # v6.1.0
        with:
          path: |
            .fermut/cache.json
            .fermut/kill-order.json
          key: fermut-nightly-cache-${{ github.run_id }}

      - uses: actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9  # v7.0.2
        if: always()
        with:
          name: fermut-nightly
          path: |
            nightly-report.md
            nightly.json
```

How the override pattern works:

- `fermut.toml` keeps `pr-gate` defaults (narrow ops, diff-only,
  short timeout) — PR jobs use those untouched.
- The nightly job's CLI flags **override** the config for that one
  run: `--ops ""` clears the narrow op allowlist, `--timeout 60` matches
  the looser nightly budget. Add `--experimental` for the exception/loop
  operators too; it generates more mutants, so expect a longer run.
- The verdict cache is restored and saved like the history, so a night
  with few changes re-runs only the mutants in changed files. The
  packaged [`sweep`](github-action.md#sharded-full-sweep) and
  [`trend`](github-action.md#score-trend) actions do this caching (and
  sharding) for you. `--no-diff-only` clears the `diff_only = "main"` key the
  `pr-gate` profile writes — without it, the nightly run on `main` diffs
  `main` against itself, mutates nothing, and passes.
- See [CLI overrides vs config](../reference/cli/run.md#cli-overrides-vs-config)
  for the precedence rules.

History persistence shared across both jobs:

- PR jobs restore `fermut-history-main` with `actions/cache/restore`
  (read-only, no save step; see the PR-gate YAML above), so
  the PR comment's `--trend` block shows the delta against the last
  main-branch run.
- The nightly job saves a fresh history entry under
  `fermut-history-main-${{ github.run_id }}`, which becomes the new
  baseline for PR comments via the `restore-keys` prefix match.
- See [History persistence](#history-persistence) for the general
  pattern and alternatives.

Notifications on nightly failure — pick one:

- **Slack / email on failure.** Add a step with
  `if: failure()` that calls your incident webhook with the artifact
  link.
- **GitHub issue on score drop.** Pipe `nightly.json` into a step that
  diffs against the previous run and opens an issue via
  `gh issue create` if score regressed beyond a threshold.
- **Do nothing.** Nightly failures often self-resolve when the next
  PR lands. Reasonable starting position.

Common tweaks:

- **Shard the nightly job.** For repos where the full sweep exceeds
  an hour, combine this recipe with the
  [sharded full sweep](#sharded-full-sweep) above — `--shard i/n` per
  matrix entry, merge job at the end.
- **Run nightly on a release branch only.** Change the cron filter or
  add a `branches:` filter on `workflow_dispatch`. Most teams scope
  nightly to `main`.
- **Weekly instead of nightly.** Some repos run the full sweep weekly
  and skip the daily cost. Cron `0 6 * * 0` for Sunday 06:00 UTC.

## Shrinking the coverage index per PR { #coverage-scoping }

Building `.coverage` from scratch on every PR is often the slowest step.
Caching it across runs, scoping it to the changed files, or building it
once on `main` for PRs to reuse: **[Coverage in CI](ci-coverage.md)**.

## History persistence { #history-persistence }

CI runners are ephemeral, so `.fermut/history.jsonl` starts empty every
run unless you persist it, and `--trend` has nothing to compare against.
Restore it on every run with `actions/cache/restore` (which never saves),
and save it only from `main`:

```yaml
- name: restore fermut history
  uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9  # v6.1.0
  with:
    path: .fermut/history.jsonl
    key: fermut-history-main
    restore-keys: |
      fermut-history-main-

- name: run fermut
  run: fermut run src/ --tests tests/ --markdown report.md --trend

- name: save fermut history
  if: always() && github.ref == 'refs/heads/main'
  uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9  # v6.1.0
  with:
    path: .fermut/history.jsonl
    key: fermut-history-main-${{ github.sha }}
```

PR jobs only read, so feature branches never fork their own history.
The combined `actions/cache` would save from PR jobs in its post-step,
which is why the restore uses `actions/cache/restore`. Caches can be
evicted; for a history that's never lost, the packaged
[`trend` action](github-action.md#score-trend) keeps it on an orphan
branch instead.

## Pre-commit

fermut itself ships pre-commit hooks for the Rust source. To run
mutation testing *as* a pre-commit hook is usually too slow for the
git workflow; prefer `pre-push` or CI. If you do want it locally,
keep the scope tight:

```yaml
# .pre-commit-config.yaml
- repo: local
  hooks:
    - id: fermut
      name: fermut (changed files)
      entry: fermut run --diff-only origin/main --quiet
      language: system
      pass_filenames: false
      stages: [pre-push]
```

`--diff-only origin/main` scopes to every line the branch changed since
it forked from `main` — the commits being pushed plus any uncommitted
edits — so cost stays proportional to the diff. (`--since HEAD` would
only see uncommitted edits: on a clean tree at push time it finds zero
mutants and tests nothing.) Run `git fetch` periodically so
`origin/main` is current.

## Other CI systems

fermut is a plain CLI, so any CI works. Translate the steps:

1. Check out with full history (`fetch-depth: 0` or equivalent), so
   `--diff-only` can find the merge base.
2. Install Python and the project's dependencies, including `pytest-cov`.
3. Install fermut (`uv tool install fermut`, `pipx install fermut` or
   `pip install fermut`).
4. Run `fermut coverage` to build `.coverage`.
5. `fermut run src/ --tests tests/ --diff-only <base-ref> --no-verify-baseline --markdown report.md --trend`.
   `--no-verify-baseline` is safe because step 4 already ran the full
   suite in the same job; drop it if your CI splits those steps across
   jobs. See [baseline check in CI](ci-quickstart.md#baseline-check-in-ci).
6. Optional: post `report.md` to the merge request through your
   platform's API. `fermut pr-comment` is GitHub-only; elsewhere, upload
   the report as a job artifact.

Full pipelines for GitLab CI and Jenkins, including MR notes, sharding and
telling a broken run from a failed gate:
**[GitLab CI and Jenkins](ci-gitlab-jenkins.md)**.

## Where next

- **[GitHub Action](github-action.md)**: the packaged action and its inputs.
- **[Rolling out to a team](projects.md)**: measure, then gate, then tighten.
- **[CLI reference](../reference/cli/index.md)**: every flag these
  recipes rely on.
