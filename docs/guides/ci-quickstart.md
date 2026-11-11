# CI quickstart

Zero to a green mutation-testing job on every PR, in one read. The
goal here is **getting something running**, not picking the perfect
profile. The full rollout playbook lives in
**[Rolling out to a team](projects.md)**; this page is the on-ramp.

## Prerequisites

Do not skip this section. The TL;DR below assumes every box is
checked. A first-day red CI check from a project that wasn't ready
is the fastest way to lose the team's trust in mutation testing.

**Project state:**

- [ ] Python project with a working `pytest` (or any pytest-compatible)
      test suite.
- [ ] [`fermut coverage`](../reference/cli/coverage.md) (pytest-cov
      with per-test contexts) runs the suite without errors. `pr-gate`
      uses per-test coverage selection and won't be useful without it.
- [ ] `git` history available on CI (`actions/checkout` with
      `fetch-depth: 0` — the generated workflow already sets this).
- [ ] A package manager fermut can install through. The default GHA
      template uses `uv`; pip / Poetry / Hatch work too, you just
      swap the install step.

**Done at least once before adding the gate:**

- [ ] **[First steps](../getting-started/first-steps.md)** — five-minute
      local run, confirms fermut works on your codebase.
- [ ] **[`fermut baseline`](../reference/cli/baseline.md)** — see your
      starting score on the whole tree, not just a diff. Enabling a PR gate before this almost always
      surfaces a wall of pre-existing survivors.

If any box is unchecked, **stop and do that first** — the page it
links to is short. Come back when all boxes are checked.

## TL;DR (after prerequisites)

On GitHub Actions, add the [packaged action](github-action.md) as
`.github/workflows/mutation.yml`:

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
      - uses: KovantAI/fermut@v0.7.0
        with:
          blocking: "false"   # warn only, while you watch the score
```

Push. On the next PR you'll see a [sticky PR comment](../reference/cli/pr-comment.md)
— one comment that updates in place on re-runs — with the score and any
survivors on changed lines, and the same report in the job summary. The
action installs fermut, builds per-test coverage, caches verdicts and
coverage across runs, and mutates only the PR's diff. Remove `blocking`
once the score is stable; [Rolling out to a team](projects.md) covers
when.

Prefer a workflow file you own end to end? `fermut init` can write one:

```sh
fermut init --profile pr-gate --with-gha
git add fermut.toml .github/workflows/fermut.yml
git commit -m "ci: add fermut mutation gate"
```

## What `fermut init --with-gha` writes

Two files:

**`fermut.toml`** — pre-seeded from the `pr-gate` profile:

- `diff_only = "main"` — only mutate changed lines vs the merge base.
- Narrow op set (`arith-op-swap`, `compare-op-swap`, `boundary-shift`,
  `return-value-to-none`).
- `timeout = 15`. `hypothesis_seed` is pinned only when Hypothesis is a
  declared dependency.

**`.github/workflows/fermut.yml`** — the canonical PR-gate workflow
(the same file as
[`examples/github-actions/pr-gate.yml`](https://github.com/KovantAI/fermut/blob/main/examples/github-actions/pr-gate.yml)).
Key points:

- Triggers on `pull_request` when `**/*.py`, `pyproject.toml` or
  `fermut.toml` change.
- Runs `uv sync`, installs `pytest-cov`, then `fermut coverage`, which
  runs `pytest --cov --cov-context=test` and writes `.coverage` (fermut
  reads it directly). The per-test contexts power the per-mutant test
  selection.
- Runs `fermut run --diff-only origin/<PR base>` and writes
  `fermut-report.md` and `fermut-report.json`.
- Has **no cache step**: every run starts cold. Add the cache steps from
  [CI recipes](integrations.md#github-actions), or use the packaged action.
- Posts a sticky PR comment via `fermut pr-comment`. The comment
  updates in place on each re-run.

## Secrets to wire

fermut installs straight from PyPI — `uv tool install fermut` (or
`pipx install fermut` / `pip install fermut`) needs no index
credentials, so the install step requires no secrets.

`GITHUB_TOKEN` is provided automatically by Actions; `pr-comment`
uses it to post.

## First run — what to expect

The first PR after wiring up the packaged action from the TL;DR will
produce one of four outcomes:

- **Skipped.** The PR changed no Python files, so the job passes without
  running and the action's `skipped` output is `"true"`. No comment.
- **Green, no comment.** The diff had no mutable code, so nothing was
  scored (N/A). The action skips the PR comment when nothing was scored.
  Expected, not a failure.
- **Green, comment with score and zero survivors.** The happy path.
- **Warning, comment listing survivors.** Each survivor is one line with
  its `path:line`, operator and the `original → replacement` edit. With
  `blocking: "false"` a failing gate only warns; drop it and the job goes
  red instead. Run `fermut explain` locally on one for the full context,
  then fix it or add a `# fermut: ignore[<op>]` marker.

The `--with-gha` workflow differs: its `paths:` filter doesn't trigger at
all on a PR without `.py`, `pyproject.toml` or `fermut.toml` changes, and
a failing gate is always red.

If you get **`errored` outcomes** in the comment instead of
killed/survived, your tests probably have side effects on the source
tree. See [troubleshooting / errored outcomes](../reference/troubleshooting/errored-outcomes.md).

## Onboarding without a gate first

The TL;DR starts with `blocking: "false"`, so a failing gate only warns.
The `--with-gha` workflow blocks merges from the first run; to soften it,
add `continue-on-error: true` to its job, which also hides setup
failures. Codebases that haven't run mutation testing before should start
non-blocking — see [Rolling out to a team](projects.md).

## Baseline check in CI

Before mutating, `fermut run` runs your **unmutated** suite once and aborts if
it isn't green — a failing or erroring suite would make every covered mutant
exit non-zero, which fermut counts as "killed", inflating the score toward
100%. Locally this is a useful guard; **in CI it's usually redundant**, because
the coverage step (`pytest --cov=...`) already ran the full suite a moment
earlier. To skip the second full-suite run on the critical path, pass
`--no-verify-baseline` to the CI `fermut run` step:

```sh
fermut run src/ --tests tests/ --diff-only <base-ref> \
    --coverage .coverage --no-verify-baseline --markdown report.md --trend
```

Only do this when the coverage step and the fermut step run in the **same job**
against the **same checkout** — otherwise keep the check on. If you leave the
baseline check enabled (the safe default), size it for your suite with
`--baseline-timeout <secs>` (default 300); a suite that exceeds it is killed
and the run aborts with a clear message rather than hanging the job.

> The generated `pr-gate` workflow keeps the baseline check **on**. Add
> `--no-verify-baseline` to the `fermut run` line only after confirming the
> coverage and fermut steps share one job + checkout.

## Non-GHA CI

The steps translate to any CI; see
[CI recipes → Other CI systems](integrations.md#other-ci-systems).

## Where next

- **[GitHub Action](github-action.md)** — every input, sharded sweeps and
  the score trend.
- **[Rolling out to a team](projects.md)** — baseline, measure, gate,
  tighten.
- **[CI recipes](integrations.md)** — hand-written workflows: caching,
  PR gate + nightly, coverage scoping, history persistence.
- **[`fermut init`](../reference/cli/init.md)** — every flag the
  bootstrap supports.
