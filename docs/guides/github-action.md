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
tag (or its commit SHA). With no `fermut-version` input, each action
installs the fermut release that matches its own tag, so the actions and
the CLI always move together.

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
      - uses: KovantAI/fermut@v0.5.0
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
      - uses: KovantAI/fermut@v0.5.0
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
      - uses: KovantAI/fermut@v0.5.0
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
      - uses: KovantAI/fermut/sweep@v0.5.0
        with:
          shard: ${{ matrix.shard }}/4

  merge:
    needs: shard
    runs-on: ubuntu-latest
    steps:
      - uses: KovantAI/fermut/merge@v0.5.0
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
jobs:
  # warm and shard jobs as above
  trend:
    needs: shard
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@<sha>
      - uses: KovantAI/fermut/trend@v0.5.0
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

`blocking: "false"` turns these refusals, and the gates, into warnings.

History lives in the Actions cache. Only the default branch saves it, so
a run on any other branch is compared against the default branch's
history without forking it. The cache evicts entries that go unused for
7 days, so schedule at least weekly. Alternatively, set
`history-cache: "false"` and persist `history-path` yourself, for example
by committing it. Every run also uploads the merged report and the
history as the `<artifact-name>-trend` artifact.

| Input | Default | Purpose |
|---|---|---|
| `fail-on-regression` | — | Max score drop in points vs the previous run. Empty disables it |
| `fail-under` | — | Absolute score floor. Unlike `merge`, survivors alone never fail a trend run |
| `max-errored` | — | Refuse to record a run with more errored mutants than this |
| `history-path` | `.fermut/history.jsonl` | History file, relative to `working-directory` |
| `history-cache` | `true` | Restore and save the history through the Actions cache |
| `default-branch` | repository default | The only branch whose runs save the history |
| `cache-key-prefix` | `fermut` | History cache namespace. Change it to start a new trend |
| `config-hash` | — | Stamped into each entry. Change it when the sweep's shape changes, so `fermut trend` flags the break |
| `limit` | `20` | Trend rows in the job summary |

`trend` also takes `working-directory`, `artifact-name`, `blocking` and
`fermut-version`. Besides the `merge` outputs, it outputs `delta` (points
vs the previous run, empty when there is none) and `recorded`.

## Inputs

Inputs shared by `KovantAI/fermut` and `sweep` (`merge` takes only
`fail-under`, `blocking`, `artifact-name` and `fermut-version`):

| Input | Default | Purpose |
|---|---|---|
| `working-directory` | `.` | Project root, for monorepos |
| `source` / `tests` | from `fermut.toml` | Source root and test directory |
| `fermut-version` | match action tag | A version, or `project` to use the fermut already in your environment |
| `python-version` | runner default | Passed to `setup-uv` |
| `install-command` | `uv sync --all-extras --dev` | Empty skips it (install in your own step) |
| `post-install-command` | — | E.g. create a test DB against your `services:` |
| `fail-under` | from `fermut.toml` | Score threshold. When it is unset everywhere, any survivor fails |
| `blocking` | `true` | `false` turns a failing gate into a warning |
| `cache` | `true` | Cache verdicts and the coverage DB |
| `cache-key-hash` / `cache-key-prefix` | auto / `fermut` | Cache key control |
| `artifact-name` | `fermut-report` | Report artifact name |
| `args` | — | Extra `fermut run` flags, e.g. `--ops arith,compare` |

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
- uses: KovantAI/fermut@v0.5.0
  with:
    install-command: ""
```

## Notes

- **Runners:** Linux and macOS.
- **Fork PRs:** the token is read-only, so the comment step logs a
  warning and the report stays in the job summary.
- **Services:** the action runs inside your job, so `services:` and
  `container:` work as usual.
