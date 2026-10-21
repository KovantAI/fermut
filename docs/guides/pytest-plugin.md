# pytest plugin (`pytest --fermut`)

Run mutation testing without leaving pytest. The plugin ships inside the
`fermut` wheel, so installing fermut into the project's environment is the
whole setup:

```sh
uv add --dev fermut        # or: pip install fermut
pytest --fermut
```

pytest runs your suite as usual. If it's green, the plugin runs fermut and
adds a section to the end of the report:

```text
==================================== fermut ====================================
mutation score 87.8%: 36 killed, 5 survived, 0 timed out, 0 errored (155 skipped)
surviving mutants:
  src/calculator.py:14 [boundary-shift] `<=` -> `<`
  src/calculator.py:53 [boundary-shift] `<` -> `<=`
  src/calculator.py:66 [arith-op-swap] `//` -> `/`
`fermut next` ranks which survivors to kill first.
mutation gate failed
```

The session fails (exit 1) when the mutation gate fails, the same gate
`fermut run` applies: any survivor, or a score below `fail_under` /
`--fermut-min-score`.

!!! note "Installed as a tool?"
    `uv tool install fermut` puts the binary in its own isolated environment,
    so your project's pytest can't see the plugin. Add fermut as a dev
    dependency of the project to use `pytest --fermut`. The CLI works either
    way.

## What it runs

1. **Your suite**, exactly as you invoked pytest. If anything fails, mutation
   testing is skipped (`skipped: the test suite did not pass`). A red suite
   would make every mutant look killed.
2. **`fermut coverage`**, when pytest-cov is installed. This refreshes the
   per-test coverage database incrementally (only stale files re-run), so each
   mutant runs just the tests that cover it. Without pytest-cov, every mutant
   runs the whole suite, which is correct but slow; the summary says so.
3. **`fermut run`** from pytest's rootdir, with this interpreter
   (`--python`). What to mutate, which operators, timeouts and the gate come
   from your fermut config (`[tool.fermut]` or `fermut.toml`), exactly as for
   `fermut run`.

## Narrowing to the tests you selected

When you select part of the suite, only the selected tests run against the
mutants:

```sh
pytest tests/test_parser.py --fermut     # a file (or a node id)
pytest -k "parse and not slow" --fermut  # -k, -m, --deselect, --ignore, --lf, --sw
```

Mutants that only unselected tests cover are skipped as `selection`; they
count like coverage-skipped mutants, not in the score. The summary says how
many tests and mutants that left:

```text
mutation score 75.3% (22 selected tests, 2120 mutants outside the selection): 320 killed, 105 survived, …
```

The score answers "do *these* tests kill the mutants they reach?" A mutant
that another test kills can survive here. Because it isn't comparable with a
full run, a narrowed run is **not recorded in history** (`fermut trend`).

`pytest`, `pytest .` and `pytest <testpaths entry>` run the whole suite and
are not narrowed. Running pytest from a subdirectory is narrowed, since pytest
then collects only that directory. Narrowing needs per-test coverage
(pytest-cov); without it fermut warns and runs every mutant against the whole
suite.

Under the hood the plugin writes the selected node ids to a file and passes
`fermut run --select-tests <file>`; see
[`--select-tests`](../reference/cli/run.md#select-tests).

## Options

| Option | Effect |
|---|---|
| `--fermut` | Turn the plugin on. Without it the plugin does nothing. |
| `--fermut-since REF` | Only mutate lines changed since `REF` (`fermut run --since`). |
| `--fermut-min-score PCT` | Fail only when the score is below `PCT` (`--fail-under`). |
| `--fermut-arg ARG` | Pass `ARG` to `fermut run`. Repeatable, e.g. `--fermut-arg=--sample=0.2`. |
| `--fermut-no-coverage` | Skip the `fermut coverage` refresh. |
| `--fermut-verify-baseline` | Run fermut's baseline check even after a green whole-suite session. |
| `--fermut-show N` | List at most `N` survivors (default 10). |

fermut's own logs show warnings and errors only; run `pytest -v` to see its
progress too, or set `RUST_LOG` yourself. `FERMUT_BIN` points the plugin at a
specific fermut binary.

## Behavior to know

- **No recursion.** fermut marks every suite run it spawns (baseline,
  coverage, per-mutant runs) with `FERMUT_CHILD=1`, and the plugin stays
  inert there. Putting `--fermut` in `addopts` is safe, though it makes every
  pytest run a mutation run.
- **No second baseline run.** fermut normally runs the unmutated suite once
  before mutating, to refuse a red suite. After a green session that ran the
  whole suite the plugin skips that run (`--no-verify-baseline`), which is
  most of the time a warm-cache `pytest --fermut` takes. A narrowed session
  proves only its subset, so fermut still runs the baseline then. Pass
  `--fermut-verify-baseline` if fermut's config runs the suite differently
  from your session (e.g. `runner = "unittest"`, or `pytest_args` that change
  outcomes).
- **Timeout budgets.** With `timeout_factor` in your fermut config (or
  `--fermut-arg=--timeout-factor=3`), the session's duration stands in for
  the skipped baseline when sizing each mutant's timeout
  (`--baseline-secs`). See
  [Timeout budgets](../reference/cli/run.md#timeout-budgets).
- **pytest-xdist.** Only the controller runs fermut, once. fermut parallelizes
  mutants itself. Narrowing works under `-n` too.
- **`--collect-only`** never triggers a mutation run.
- **Exit codes.** A failed gate exits 1, like failing tests. If fermut itself
  fails before producing a report (bad config, missing tools), the session
  exits 3 and fermut's error is printed above the summary.
- **Disable it** for one run with `-p no:fermut`.

## Not the same as fermut's result reporter

fermut also loads a small reporter plugin (`-p _fermut_reporter`) into the
pytest runs it spawns per mutant, to learn exact outcomes. That one is injected
by fermut and needs nothing installed. See
[Result reporter plugin](../reference/cli/run.md#result-reporter-plugin).
