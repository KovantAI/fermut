# Integrations

How to wire fermut into the systems that already build/test your code.

## GitHub Actions

The canonical PR-gate workflow. `fermut init --with-gha` drops this
template at `.github/workflows/fermut.yml`:

```yaml
name: Mutation

on:
  pull_request:
    paths:
      - "**/*.py"

jobs:
  gate:
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
      - run: |
          uv run pytest --cov=src --cov-context=test
          uv run coverage json -o coverage.json --show-contexts
      - uses: actions/cache@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
        with:
          path: .fermut/cache.json
          key: fermut-cache-${{ runner.os }}-${{ hashFiles('uv.lock', 'src/**/*.py', 'tests/**/*.py') }}
          restore-keys: |
            fermut-cache-${{ runner.os }}-
      - run: |
          uv tool run fermut run src/ \
              --tests tests/ \
              --diff-only origin/${{ github.base_ref }} \
              --coverage coverage.json \
              --markdown fermut-report.md \
              --trend
      - if: always() && hashFiles('fermut-report.md') != ''
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          PR_NUMBER: ${{ github.event.pull_request.number }}
        run: uv tool run fermut pr-comment --markdown fermut-report.md
```

Key points:

- `fetch-depth: 0` is required so `--diff-only` can compute the
  merge base.
- `actions/cache` keyed on lockfile + sources persists the result
  cache across PRs.
- `pr-comment` posts a sticky comment that updates in place on
  re-runs.

For the gradual-rollout playbook (measure-only first, then enable
the gate), see **[Working on projects](projects.md#step-3-wire-ci-in-measure-only-mode)**.

## Sharded full sweep

For long-running nightly runs, parallelize via `--shard`:

```yaml
jobs:
  shard:
    strategy:
      matrix:
        shard: [1, 2, 3, 4]
    runs-on: ubuntu-latest
    steps:
      - run: |
          fermut run src/ --tests tests/ \
              --shard ${{ matrix.shard }}/4 \
              --json shard-${{ matrix.shard }}.json
      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a  # v7.0.1
        with:
          name: shard-${{ matrix.shard }}
          path: shard-${{ matrix.shard }}.json

  merge:
    needs: shard
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c  # v8.0.1
      - run: fermut merge shard-*.json --markdown report.md
```

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

This generates `.github/workflows/fermut.yml` (the PR gate from the
section above). Add a second file alongside it:

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
          uv run pytest --cov=src --cov-context=test
          uv run coverage json -o coverage.json --show-contexts

      - name: restore fermut history
        uses: actions/cache/restore@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
        with:
          path: .fermut/history.jsonl
          key: fermut-history-main
          restore-keys: |
            fermut-history-

      - name: full sweep (override pr-gate narrowing)
        run: |
          uv tool run fermut run src/ \
              --tests tests/ \
              --coverage coverage.json \
              --ops "" \
              --experimental \
              --timeout 60 \
              --markdown nightly-report.md \
              --json nightly.json \
              --trend

      - name: save fermut history
        if: always()
        uses: actions/cache/save@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
        with:
          path: .fermut/history.jsonl
          key: fermut-history-main-${{ github.run_id }}

      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a  # v7.0.1
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
  run: `--ops ""` clears the narrow op allowlist, `--experimental`
  adds exception/loop ops, `--timeout 60` matches the looser nightly
  budget. No `--diff-only` here — nightly mutates the whole tree.
- See [CLI overrides vs config](../reference/cli/run.md#cli-overrides-vs-config)
  for the precedence rules.

History persistence shared across both jobs:

- PR jobs read `fermut-history-main` (read-only, no save step), so
  the PR comment's `--trend` block shows the delta against the last
  main-branch run.
- The nightly job saves a fresh history entry under
  `fermut-history-main-${{ github.run_id }}`, which becomes the new
  baseline for PR comments via the `restore-keys` prefix match.
- See [history persistence in CI](projects.md#history-persistence-in-ci)
  for the alternative (committing `history.jsonl` to a side branch).

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
      entry: fermut run --since HEAD --quiet
      language: system
      pass_filenames: false
      stages: [pre-push]
```

`--since HEAD` keeps the cost proportional to the diff.

## Coding agents

fermut is built to drop into an AI coding agent's inner loop. The
recommended invocation, cache strategy, and JSON parsing recipes
live in **[Coding agents](coding-agents.md)**.

## MCP server { #mcp-server }

`fermut mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio, so an agent calls fermut as native tools
(`fermut_doctor`, `fermut_run`, `fermut_next`, `fermut_explain`,
`fermut_score`, `fermut_list_survivors`) instead of shelling out and
parsing JSON. See the
**[`fermut mcp` reference](../reference/cli/mcp.md)** for the tool list and
error model.

Register it with an MCP client. Claude Code:

```sh
claude mcp add fermut -- fermut mcp
```

Or by hand, in a client config that takes a server map:

```json
{
  "mcpServers": {
    "fermut": { "command": "fermut", "args": ["mcp"] }
  }
}
```

The server honors the project's `fermut.toml`, so run it from (or point the
client's working directory at) the project root. No network access, no API
key — the tools wrap the same local engine the CLI uses.

## ReadTheDocs / docs builds

This site builds on ReadTheDocs with `docs/requirements.txt`. The
`strict: true` mode in `mkdocs.yml` is what catches broken internal
links before they ship. Run locally with:

```sh
uv pip install -r docs/requirements.txt
uv run mkdocs serve
```

## Where next

- **[Working on projects](projects.md)** — the rollout playbook.
- **[Coding agents](coding-agents.md)** — agent inner-loop integration.
- **[CLI reference](../reference/cli/index.md)** — every flag the
  integrations rely on.
