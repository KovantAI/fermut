# CI quickstart

Zero to a green mutation-testing job on every PR, in one read. The
goal here is **getting something running**, not picking the perfect
profile. The full rollout playbook lives in
**[Working on projects](projects.md)**; this page is the on-ramp.

## Prerequisites

Do not skip this section. The TL;DR below assumes every box is
checked. A first-day red CI check from a project that wasn't ready
is the fastest way to lose the team's trust in mutation testing.

**Project state:**

- [ ] Python project with a working `pytest` (or any pytest-compatible)
      test suite.
- [ ] The suite runs under `coverage run -m pytest` without errors.
      `pr-gate` uses per-test coverage selection and won't be useful
      without it.
- [ ] `git` history available on CI (`actions/checkout` with
      `fetch-depth: 0` — the generated workflow already sets this).
- [ ] A package manager fermut can install through. The default GHA
      template uses `uv`; pip / Poetry / Hatch work too, you just
      swap the install step.

**Done at least once before adding the gate:**

- [ ] **[First steps](../getting-started/first-steps.md)** — five-minute
      local run, confirms fermut works on your codebase.
- [ ] **[Working on projects → Step 1: baseline](projects.md#step-1-get-an-honest-baseline)** —
      one full-tree run without `--diff-only` to see your starting
      score honestly. Enabling a PR gate before this almost always
      surfaces a wall of pre-existing survivors.

If any box is unchecked, **stop and do that first** — the page it
links to is short. Come back when all boxes are checked.

## TL;DR (after prerequisites)

```sh
fermut init --profile pr-gate --with-gha
git add fermut.toml .github/workflows/fermut.yml
git commit -m "ci: add fermut mutation gate"
```

Push. On the next PR you'll see a [sticky PR comment](../reference/cli/pr-comment.md)
from `fermut pr-comment` — one comment that updates in place on
re-runs, not a stack of duplicates — with the score and any
survivors on changed lines. That's it; the rest of this page is what
the generated files actually do and how to customize them.

> **Gradual rollout?** If you'd rather watch the score for a week
> before letting the gate block merges, follow the
> **measure-only variant** at
> [projects.md / Step 3](projects.md#step-3-wire-ci-in-measure-only-mode)
> instead of the TL;DR above.

## What `fermut init --with-gha` writes

Two files:

**`fermut.toml`** — pre-seeded from the `pr-gate` profile:

- `diff_only = "main"` — only mutate changed lines vs the merge base.
- Narrow op set (arith / compare / boundary / return-to-none).
- `timeout = 15`, Hypothesis seed pinned.

**`.github/workflows/fermut.yml`** — the canonical PR-gate workflow.
See [Integrations / GitHub Actions](integrations.md#github-actions)
for the full YAML and a line-by-line explanation. Key points:

- Triggers on `pull_request` for `**/*.py` changes only.
- Installs deps, runs coverage with per-test contexts
  (`pytest --cov=src --cov-context=test`, which writes `.coverage`;
  fermut reads it directly). The contexts power the per-mutant test
  selection.
- Caches `.fermut/cache.json` keyed on lockfile + sources, so re-runs
  on the same PR skip already-killed mutants.
- Posts a sticky PR comment via `fermut pr-comment`. The comment
  updates in place on each re-run.

## Secrets to wire

fermut installs straight from PyPI — `uv tool install fermut` (or
`pipx install fermut` / `pip install fermut`) needs no index
credentials, so the install step requires no secrets.

`GITHUB_TOKEN` is provided automatically by Actions; `pr-comment`
uses it to post.

## First run — what to expect

The first PR after wiring this up will produce one of three outcomes:

- **Green, no comment.** The PR didn't touch Python or the diff had
  no mutable code. Expected, not a failure.
- **Green, comment with score and zero survivors.** The happy path.
- **Red, comment listing survivors.** Fix or `# fermut: ignore`
  them. The comment links to `fermut explain` output for each one.

If you get **`errored` outcomes** in the comment instead of
killed/survived, your tests probably have side effects on the source
tree. See [troubleshooting / errored outcomes](../reference/troubleshooting/errored-outcomes.md).

## Onboarding without a gate first

The default `pr-gate` workflow blocks merges. If you'd rather watch
the score for a week before flipping the gate on, follow the
**measure-only** variant in
**[projects.md / Step 3](projects.md#step-3-wire-ci-in-measure-only-mode)**.
The difference is one line: `continue-on-error: true` on the job.

This is the recommended path for codebases that haven't run
mutation testing before — see
[projects.md / Step 1](projects.md#step-1-get-an-honest-baseline)
for why.

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

The generated template is GitHub Actions. fermut itself is a normal
CLI, so any CI works — translate the steps:

1. Checkout with full history (`fetch-depth: 0` equivalent).
2. Install Python + the project's deps.
3. Install fermut (`uv tool install fermut`, `pipx install fermut`,
   or `pip install fermut`).
4. Run `fermut coverage` to build a `.coverage` database (or, manually:
   `pytest --cov=src --cov-context=test` then `coverage json --show-contexts`).
5. `fermut run src/ --tests tests/ --diff-only <base-ref> --coverage .coverage --no-verify-baseline --markdown report.md --trend`
   (`--no-verify-baseline` is safe here because step 4 already ran the full
   suite in this same job; drop it if your CI splits those steps across jobs.)
6. Optional: post the markdown report to the PR via your platform's
   API. `fermut pr-comment` is GitHub-specific; for GitLab / Bitbucket,
   upload `report.md` as a job artifact instead.

## Where next

- **[Working on projects](projects.md)** — the gradual rollout
  playbook (baseline → measure → gate → tighten).
- **[PR gate + nightly recipe](integrations.md#pr-gate-nightly)** —
  add a nightly sweep alongside the PR gate, sharing one config.
- **[Integrations](integrations.md)** — full GHA YAML, sharded
  nightly sweep, pre-commit, history persistence.
- **[`fermut init`](../reference/cli/init.md)** — every flag the
  bootstrap supports.
- **[`fermut run`](../reference/cli/run.md)** — every flag the CI job
  uses.
