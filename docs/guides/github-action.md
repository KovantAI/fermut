# GitHub Action

fermut ships three composite actions. Use them instead of copying a
workflow. They install fermut, build per-test coverage, cache verdicts
and coverage across runs, gate on the score, and publish the report to
the job summary.

| Action | Use it for |
|---|---|
| `KovantAI/fermut` | PR gate on changed lines, with a sticky PR comment. Also warms the cache from `main`. |
| `KovantAI/fermut/sweep` | Whole-tree run, or one shard of a matrix |
| `KovantAI/fermut/merge` | Combine shard reports and gate |

All three are released under the repository's tags. Pin to a release
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
  shard:
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

Shards never gate. Each one uploads its report as
`fermut-report-shard-<i>-of-<n>`. The merge job combines them and
applies the gate. It needs no checkout. `fermut merge` fails if a shard
is missing, so a crashed shard cannot inflate the combined score.

To sweep without sharding, leave `shard` empty. That run gates directly.

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

Outputs on all three: `score` (empty when N/A), `scored`, `killed`,
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
