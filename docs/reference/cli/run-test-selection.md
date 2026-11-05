# Test selection

Which tests [`fermut run`](run.md) runs against each mutant: the ones per-test
coverage says executed the mutated line, narrowed further by
`--select-tests` or by pytest's own deselection.

## How tests are selected

With per-test coverage, fermut knows which tests executed each line. The
coverage filter skips any mutant on a line no test executed (`coverage`
skip), and for the rest the runner passes pytest only the tests that ran
the mutated line. Without per-test contexts, coverage knows only which
lines ran, not which tests ran them, so fermut refuses the run.

`--coverage` takes coverage.py's `.coverage` database or a
`coverage json --show-contexts` export; the format is sniffed. Prefer the
database: the JSON export repeats every node id per covered line and can
reach several GB on a large suite.

**Where the data comes from.** `--coverage <path>` (CLI) wins over
`coverage = "…"` (config), which wins over a `.coverage` auto-discovered at
the project root. `--no-coverage` turns selection off entirely.
[`fermut coverage`](coverage.md) builds `.coverage` and refreshes it
incrementally when tests change; it runs
`pytest --cov=src --cov-context=test`, which you can run yourself instead.

**Freshness guard.** A `.coverage` older than sources or tests you've since
edited is ignored if it was auto-discovered (the run proceeds without
selection rather than skip mutants on changed lines), and used with a
warning if you passed it explicitly. Regenerate it to clear the warning.
See [Coverage → Freshness guard](../../guides/coverage.md#freshness-guard).

## A subset of the suite (`--select-tests`)

`--select-tests FILE` restricts the run to a subset of the suite.
[`pytest --fermut`](../../guides/pytest-plugin.md#narrowing-to-the-tests-you-selected)
uses it to pass on the session's selection. `FILE` lists one pytest node id
per line, either with an absolute test-file path
(`/repo/tests/test_x.py::TestA::test_b[1]`) or relative to the project root
(`tests/test_x.py::TestA::test_b[1]`). Absolute paths outside the project
root are dropped with a warning.

- Each mutant runs only the selected tests among those that cover it.
- A mutant that some test covers, but no selected test, is skipped as
  `selection`. It is never run and doesn't count in the score. A line no test
  covers at all is still a `coverage` skip.
- Lines that run only at import time (recorded per test module) select only
  that module's selected tests, never the whole module.
- The run isn't appended to history: a score against a subset of the suite
  isn't comparable with full runs.
- The baseline check (`verify_baseline`) runs only the selected tests, or
  the whole suite, with a warning, when there are too many of them to pass
  on the command line.
- A selection that covers none of the code under mutation is an error: every
  mutant would be skipped and the run would pass while testing nothing.
- It needs coverage data. Without it fermut warns, ignores the file and runs
  every mutant against the whole suite.

## Tests pytest deselects

Tests pytest deselects on its own (`--deselect`, `-k`, `-m`, from
`--pytest-arg` or `addopts`) narrow the run the same way, with no file: the
baseline reports what pytest deselected, and a mutant only those tests cover
is skipped as `selection` instead of running with no tests (pytest exit 5,
scored `error`). Unlike `--select-tests`, the run is still recorded in history.
It needs the baseline (`verify_baseline`) and fermut's pytest plugin
(`--no-pytest-plugin` turns it off).
