# Troubleshooting

The symptoms we see most often. Start with `fermut doctor --strict`
— it catches most of these automatically.

- **[Errored outcomes](errored-outcomes.md)** — many mutants come
  back `errored`. Usually test-suite side-effects on the source
  tree.
- **[Timeouts](timeouts.md)** — many mutants `timed_out`. Loop
  mutants or slow fixtures.
- **[Phantom survivors](phantom-survivors.md)** — survivor doesn't
  reproduce by hand.
- **[Score swings](score-swings.md)** — same commit, different
  scores between runs.
- **[Coverage rejected](coverage-rejected.md)** — `coverage.json`
  lacks per-test contexts.
- **[ty not found](ty-not-found.md)** — ty pre-filter warning.
- **[PR comment fails](pr-comment-fails.md)** — `gh: command not
  found` in CI.
- **[Diff-only finds nothing](diff-only-empty.md)** — empty mutant
  set despite local edits.
- **[Permission denied writing `.fermut/`](permission-denied.md)**
  — sandbox / volume ownership.
- **[Missing expected mutants](missing-mutants.md)** — a filter
  dropped them.

## See also

- **[`fermut doctor`](../cli/doctor.md)** — the
  auto-checker.
- **[Getting help](../../getting-started/getting-help.md)** — how
  to open a productive issue.
