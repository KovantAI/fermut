# GitHub Action

fermut ships four composite actions. Use them instead of copying a
workflow. They install fermut, build per-test coverage, cache verdicts
and coverage across runs, gate on the score, and publish the report to
the job summary.

| Action | Use it for |
|---|---|
| `KovantAI/fermut` | PR gate on changed lines, with a sticky PR comment. Also warms the cache from `main`. |
| `KovantAI/fermut/sweep` | Whole-tree run, or one shard of a matrix |
| `KovantAI/fermut/merge` | Combine shard reports and gate |
| `KovantAI/fermut/trend` | Combine shard reports, record a score history, and gate on regressions |

All four are released under the repository's tags. Pin to a release
tag such as `@v0.6.1`. With no `fermut-version` input, each action
installs the fermut release that matches a `vX.Y.Z` tag. Any other ref,
including a commit SHA or a branch, installs the latest fermut from
PyPI, so a SHA pin should also set `fermut-version` explicitly.

## Permissions

A job's `permissions` scope its `GITHUB_TOKEN`. Grant each job only what
its action needs:

| Action | Permissions | Why |
|---|---|---|
| `KovantAI/fermut` | `contents: read`, `pull-requests: write` | Posts the sticky PR comment. Without it (or on a fork PR, whose token is read-only) the comment is skipped with a warning and the report stays in the job summary |
| `KovantAI/fermut` with `warm-only: "true"` | `contents: read` | Only builds and caches coverage |
| `KovantAI/fermut/sweep` | `contents: read` | Runs mutants; never give these jobs a write scope |
| `KovantAI/fermut/merge` | `contents: read` | Uses no token itself; reads only the shard artifacts of its own run |
| `KovantAI/fermut/trend` | `contents: write` | Pushes the history to the `fermut-history` branch. It runs no tests, so the write token never meets mutated code |

Set `contents: read` at the workflow level and raise it on the `trend`
job alone. A job-level `permissions` block replaces the workflow-level
one; scopes it leaves out become `none`, so list every scope the job
needs.

`trend` only pushes on the default branch. On any other branch it reads
the history and `contents: read` is enough. If a ruleset blocks
`GITHUB_TOKEN` from pushing to `fermut-history` and you can't exempt the
branch, pass a token that may bypass it, such as a GitHub App token, as
`token`.

## PR gate

Mutates only the lines the PR changed. A PR that touches no Python
reports N/A and passes.

```yaml
name: Mutation
on: pull_request

permissions:
  contents: read
  pull-requests: write   # sticky PR comment

jobs:
  fermut:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@<sha>
        with:
          fetch-depth: 0     # optional: the action deepens a shallow clone itself
      - uses: KovantAI/fermut@v0.6.1
        with:
          fail-under: "80"
```

The default `install-command` is `uv sync --all-extras --dev`. If
`pytest-cov` is missing from the resolved environment, the action
installs it there.

### Rolling it out

Start with `blocking: "false"`. A failing gate becomes a warning, but a
broken setup still fails. Remove the input once the score is stable.
Avoid `continue-on-error`, because it also hides setup failures.

### Keep PRs warm

The caches are keyed per branch. Without a run on the default branch,
each new PR starts with a cold coverage build. Add a push job:

```yaml
on:
  push:
    branches: [main]
jobs:
  warm:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@<sha>
      - uses: KovantAI/fermut@v0.6.1
        with:
          warm-only: "true"
```

## Sharded full sweep

For a nightly or post-merge run over the whole tree, split the mutants
across a matrix and merge the results:

```yaml
jobs:
  warm:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@<sha>
      - uses: KovantAI/fermut@v0.6.1
        with:
          warm-only: "true"

  shard:
    needs: warm
    strategy:
      fail-fast: false
      matrix:
        shard: [1, 2, 3, 4]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@<sha>
      - uses: KovantAI/fermut/sweep@v0.6.1
        with:
          shard: ${{ matrix.shard }}/4

  merge:
    needs: shard
    runs-on: ubuntu-latest
    steps:
      - uses: KovantAI/fermut/merge@v0.6.1
        with:
          fail-under: "70"
```

The `warm` job builds the per-test coverage DB once and caches it. Every
shard then restores it under the same key instead of re-running the
whole suite under coverage. Without it, each shard rebuilds coverage on
its own, which costs N full-suite runs. The warm job and the shards must
agree on `working-directory`, `source`, `tests`, `cache-key-prefix` and
`cache-key-hash`, and run on the same OS.

Shards never gate. Each one uploads its report as
`fermut-report-shard-<i>-of-<n>`. The merge job combines them and
applies the gate. It needs no checkout. `fermut merge` fails if a shard
is missing, so a crashed shard cannot inflate the combined score.

To sweep without sharding, leave `shard` empty. That run gates directly.

## Score trend

To track the score over time, replace `merge` with `trend` in the
sharded sweep above and run it on a schedule:

```yaml
on:
  schedule:
    - cron: "0 3 * * *"

permissions:
  contents: read        # warm and shard jobs run mutated code: read-only

jobs:
  # warm and shard jobs as above
  trend:
    needs: shard
    runs-on: ubuntu-latest
    permissions:
      contents: write   # push the fermut-history branch
    steps:
      - uses: actions/checkout@<sha>
      - uses: KovantAI/fermut/trend@v0.6.1
        with:
          fail-on-regression: "1"
```

`trend` merges the shard reports, turns the merged report into one
history entry, and appends it to `.fermut/history.jsonl`. It writes
`fermut trend` to the job summary and fails when the score dropped more
than `fail-on-regression` points against the previous run on the same
branch. It needs a checkout, because the entry's git sha and branch are
read from it.

It refuses to record a run that would corrupt the trend, and fails
instead:

- **No scored mutants.** A vacuous 100% is never recorded.
- **More than `max-errored` errored mutants**, when that input is set.
- **A restored history with malformed lines.** Appending to it would
  hide the damage.

`blocking: "false"` turns the first two refusals, and the gates, into
warnings. A malformed history always fails the job, as does a merge that
wrote no history entry.

History lives on the `fermut-history` orphan branch, one file per run
under `entries/` (`<timestamp>-<sha>.json`), so concurrent runs never
conflict and nothing evicts it. Only runs on the default branch push to
it, which is why the job needs `contents: write` (see
[Permissions](#permissions)). If a ruleset covers every branch, exempt
`fermut-history` or pass a token that may bypass it as `token`;
otherwise the push is rejected. An unreachable branch fails the job
rather than starting an empty trend.

`delta` and the regression gate compare against the previous entry on
the **same branch** (entries with no recorded branch match any branch).
A run on another named branch restores the default branch's history but
finds no earlier entry of its own, so `delta` is empty and
`fail-on-regression` never fires there. Set `history-branch: ""` to
persist `history-path` yourself instead. A run that gets past the
record step also uploads the merged report and the history as the
`<artifact-name>-trend` artifact; a refused record fails before the
upload.

| Input | Default | Purpose |
|---|---|---|
| `fail-on-regression` | — | Max score drop in points vs the previous run. Empty disables it |
| `fail-under` | — | Absolute score floor. Unlike `merge`, survivors alone never fail a trend run |
| `max-errored` | — | Refuse to record a run with more errored mutants than this |
| `history-path` | `.fermut/history.jsonl` | History file, relative to `working-directory` |
| `history-branch` | `fermut-history` | Orphan branch holding the history. Change it to start a new trend; empty disables persistence |
| `default-branch` | repository default | The only branch whose runs push the history |
| `token` | `github.token` | Reads and pushes `history-branch`. Needs `contents: write` on the default branch |
| `config-hash` | — | Stamped into each entry. Change it when the sweep's shape changes, so `fermut trend` flags the break |
| `limit` | `20` | Trend rows in the job summary |

`trend` also takes `working-directory`, `artifact-name`, `blocking` and
`fermut-version`. It outputs `score`, `scored`, `killed`, `survived`,
`errored` and `report-json` (no `report-markdown`), plus `delta`
(points vs the previous run on the same branch, empty when there is
none), `recorded`, and `history` (the history file path).

## Inputs

Inputs shared by `KovantAI/fermut` and `sweep` (`merge` takes only
`fail-under`, `blocking`, `artifact-name` and `fermut-version`):

| Input | Default | Purpose |
|---|---|---|
| `working-directory` | `.` | Project root, for monorepos |
| `source` / `tests` | from `fermut.toml` | Source root and test directory |
| `fermut-version` | match a `vX.Y.Z` action tag, else latest | A version, or `project` to use the fermut already in your environment |
| `python-version` | runner default | Passed to `setup-uv` |
| `install-command` | `uv sync --all-extras --dev` | Empty skips it (install in your own step) |
| `post-install-command` | — | E.g. create a test DB against your `services:` |
| `fail-under` | from `fermut.toml` | Score threshold. When it is unset everywhere, any survivor fails |
| `blocking` | `true` | `false` turns a failing gate into a warning |
| `cache` | `true` | Cache verdicts, smart-ordering kill history and the coverage DB |
| `cache-key-hash` / `cache-key-prefix` | auto / `fermut` | Cache key control |
| `artifact-name` | `fermut-report` | Report artifact name |
| `args` | — | Extra `fermut run` flags, e.g. `--ops arith-op-swap,compare-op-swap` |

PR gate only: `base-ref` (default `origin/<PR base>`), `comment`
(default `true`), `github-token`, `warm-only`. Sweep only: `shard`.

Outputs on the first three: `score` (empty when N/A), `scored`, `killed`,
`survived`, `errored`, `report-json` and `report-markdown`. The PR gate
also outputs `skipped` (`true` when the PR changed no Python).

## Private dependencies

A composite action cannot read `secrets`. Any `env:` you set on the
`uses:` step reaches every step inside the action, including the steps
that execute mutated code. Install private dependencies in your own step,
with the credentials scoped to that step, and set `install-command: ""`:

```yaml
- name: Install
  env:
    UV_INDEX_PRIVATE_PASSWORD: ${{ secrets.REGISTRY_TOKEN }}
  run: uv sync --dev
- uses: KovantAI/fermut@v0.6.1
  with:
    install-command: ""
```

## Notes

- **Runners:** Linux and macOS.
- **Fork PRs:** the token is read-only, so the comment step logs a
  warning and the report stays in the job summary.
- **Services:** the action runs inside your job, so `services:` and
  `container:` work as usual.
