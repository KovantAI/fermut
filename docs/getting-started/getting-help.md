# Getting help

## First stop: `fermut doctor`

```sh
fermut doctor --strict
```

Diagnoses common environment issues — missing pytest, Python too old,
`coverage.json` lacks per-test contexts, Hypothesis seed not pinned —
and prints a one-line remediation per failure. Run this before
opening an issue.

See the **[CLI reference → fermut doctor](../reference/cli/doctor.md)**.

## Troubleshooting common problems

The **[troubleshooting reference](../reference/troubleshooting/index.md)**
covers the symptoms we see most often. Direct links to the most
common ones:

- **[Lots of `errored` outcomes](../reference/troubleshooting/errored-outcomes.md)** — test suite has side-effects on the source tree.
- **[Lots of timeouts](../reference/troubleshooting/timeouts.md)** — mutant introduces infinite loop, or pytest fixture doesn't terminate.
- **[Phantom survivors](../reference/troubleshooting/phantom-survivors.md)** — survivor in the report that doesn't reproduce by hand.
- **[Score swings](../reference/troubleshooting/score-swings.md)** — Hypothesis seed isn't pinned, or cache was cleared.
- **[Coverage rejected](../reference/troubleshooting/coverage-rejected.md)** — fermut refuses the `coverage.json` you passed.
- **[`--diff-only` empty](../reference/troubleshooting/diff-only-empty.md)** — shallow clone, missing merge base.
- **[`ty` not found](../reference/troubleshooting/ty-not-found.md)** — type filter can't locate the binary.
- **[Missing mutants](../reference/troubleshooting/missing-mutants.md)** — operator should fire but doesn't.
- **[`pr-comment` fails](../reference/troubleshooting/pr-comment-fails.md)** — token, permissions, or sticky-comment lookup issue.
- **[Permission denied](../reference/troubleshooting/permission-denied.md)** — worker can't write to its mirror.

## GitHub

- **Issues**: <https://github.com/KovantAI/fermut/issues> — bugs,
  feature requests, weird behavior.
- **Pull requests**: <https://github.com/KovantAI/fermut/pulls> — see
  what's in flight.

Before opening an issue, attach the output of:

```sh
fermut doctor --strict
fermut --version
```

…and, if you can, a minimal reproduction (the source file + the
mutation operator name + the surviving mutant id from
`--json out.json`).

## Where things live

- **[Concepts](../concepts/mutation-testing.md)** — what mutation
  testing actually measures, vocabulary.
- **[Guides](../guides/projects.md)** — task-oriented walkthroughs.
- **[Reference](../reference/cli/index.md)** — every flag, every key,
  every operator.

## Versioning

fermut is pre-1.0. CLI and JSON shapes may change between `0.MINOR`
releases. See **[Policies](../reference/policies/index.md)** for the full
contract.
