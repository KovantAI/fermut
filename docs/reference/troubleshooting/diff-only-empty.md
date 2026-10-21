# `--diff-only` finds nothing or fails

`--diff-only <base>` diffs the merge-base of `<base>` and `HEAD`
against the **working tree**, so both committed and uncommitted edits
on tracked files count.

## The run fails with a git error

**Symptom.** `fermut run` stops before mutating, with either of these
errors:

```
git failed (…): fatal: …

hint: the diff base could not be resolved. Point `diff_only`/`--diff-only` at a branch or commit that exists locally (e.g. `origin/main`), fetch it first, or turn diff scoping off with `--no-diff-only`.
```

```
no merge-base between "main" and HEAD — unrelated histories. …
```

**Cause.** Almost always a shallow CI checkout (`fetch-depth: 1` on
`actions/checkout`): the base branch isn't fetched, or its history
doesn't reach the merge-base.

**Fix.** Set `fetch-depth: 0` in the checkout step:

```yaml
- uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10  # v6.0.3
  with:
    fetch-depth: 0
```

Alternatively, point `--diff-only` at a ref that exists locally, such
as `origin/main`.

## The run succeeds with zero mutants

- **You are on the base branch** (e.g. a nightly job on `main` with the
  `pr-gate` profile's `diff_only = "main"`). There is no diff, so no
  mutants are generated. For full sweeps, pass `--no-diff-only`.
- **The new file is untracked.** `git diff` doesn't see files that were
  never `git add`ed. Stage them first.
