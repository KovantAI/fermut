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
- **[Coverage rejected](coverage-rejected.md)** — the coverage
  database (`.coverage` or `coverage.json`) lacks per-test contexts.
- **[ty not found](ty-not-found.md)** — ty pre-filter warning.
- **[PR comment fails](pr-comment-fails.md)** — `gh: command not
  found` in CI.
- **[Diff-only finds nothing](diff-only-empty.md)** — empty mutant
  set despite local edits.
- **[Permission denied writing `.fermut/`](permission-denied.md)**
  — sandbox / volume ownership.
- **[Missing expected mutants](missing-mutants.md)** — a filter
  dropped them.

## By error message

Search this page for the start of the message fermut printed.

| Message starts with | What it means | Fix |
|---|---|---|
| `baseline test suite is not green — aborting before mutation` | The unmutated suite fails under fermut, which would make every mutant look killed. | Get `pytest` green first. If it passes when you run it yourself, the suite probably needs your real home directory: pass `--no-private-home` ([Isolation](../cli/run-isolation.md#private-home)). Otherwise fermut's copy of the project broke it: please report it. |
| `could not start the test runner: … was not found on PATH` | fermut found no project Python (`--python`, an active `$VIRTUAL_ENV`, or a nearby `.venv`) and no `pytest` on `PATH`. | Pass `--python /path/to/venv`, or activate the venv. See [Choosing the interpreter](../cli/run.md#python-interpreter). |
| `no tests collected: unittest discover … found 0 tests` | The `unittest` runner is configured, but the tests are pytest-style functions. | Set `runner = "pytest"` in `fermut.toml`. |
| `environment not ready — run fermut doctor` | `fermut baseline` runs `doctor` first, and a check failed. | Run [`fermut doctor`](../cli/doctor.md) and fix each `[fail]`. |
| `coverage file … has no per-test contexts` | The coverage data says which lines ran, not which tests ran them. | [Coverage rejected](coverage-rejected.md). |
| `coverage file … has per-test contexts, but all … fall outside project_root` | The tests live outside the project root: the nearest directory above the source root with a `pyproject.toml` or `setup.cfg`. | Run against a root that contains both the code and its tests, such as the monorepo root (see [Projects](../../concepts/projects.md)). |
| `coverage file keys do not resolve under source_root or any ancestor` | The coverage was recorded in another directory or checkout, so its paths don't match yours: every mutant looks uncovered. | Rebuild it from the project root: `fermut coverage`. |
| `ignoring auto-discovered coverage … it is older than` | A source or test file changed after `.coverage` was written, so the run proceeds without coverage: much slower, and uncovered lines count as survivors. | Run `fermut coverage` after every edit ([freshness guard](../../guides/coverage.md#freshness-guard)). |
| `coverage … looks stale` | Same, for a file you passed explicitly: fermut uses it anyway. | Run `fermut coverage`. |
| `` `ty` not found in the project venv or on PATH `` | The embedded type checker is off or unavailable, and there's no `ty` binary. | [ty not found](ty-not-found.md). |
| `` `--schemata` needs fork() `` | Schemata was required on a platform without `fork` (Windows). | Drop `--schemata` / `schemata = true`; mutants run the normal way. |
| `config sets both diff_only and since; they are mutually exclusive` | Both keys are in the config. | Keep one. On the command line, `--diff-only <ref>` overrides a configured `diff_only`. |
| `missing shard report(s)` | `fermut merge` got fewer shard reports than the run had shards. | Pass every shard's report; see [Sharded full sweep](../../guides/integrations.md#sharded-full-sweep). |
| `--pr not given and could not infer from $PR_NUMBER or $GITHUB_REF` | `fermut pr-comment` ran outside a pull-request job. | [PR comment fails](pr-comment-fails.md). |
| `unknown operator: …` | A name in `--ops` / `--skip-ops` / `ops` isn't an operator. | Check the spelling against [Operators](../operators/index.md). |
| `no mutant matches …` | `fermut explain` was given an id or index from a different report. | Use an id from the same report, e.g. from `fermut next`. |
| `noisy operators (survivors ≥ kills): …` | Not an error: these operators produced at least as many survivors as kills in this run. | Consider `skip_ops` for them, after checking their survivors aren't real gaps. |

## See also

- **[`fermut doctor`](../cli/doctor.md)** — the
  auto-checker.
- **[Getting help](../../getting-started/getting-help.md)** — how
  to open a productive issue.
