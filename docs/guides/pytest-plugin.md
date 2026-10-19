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

pytest's own test selection (paths, `-k`, `-m`) only decides whether the
suite is green. It doesn't narrow the mutation run, which picks tests per
mutant from coverage.

## Options

| Option | Effect |
|---|---|
| `--fermut` | Turn the plugin on. Without it the plugin does nothing. |
| `--fermut-since REF` | Only mutate lines changed since `REF` (`fermut run --since`). |
| `--fermut-min-score PCT` | Fail only when the score is below `PCT` (`--fail-under`). |
| `--fermut-arg ARG` | Pass `ARG` to `fermut run`. Repeatable, e.g. `--fermut-arg=--sample=0.2`. |
| `--fermut-no-coverage` | Skip the `fermut coverage` refresh. |
| `--fermut-show N` | List at most `N` survivors (default 10). |

fermut's own logs show warnings and errors only; run `pytest -v` to see its
progress too, or set `RUST_LOG` yourself. `FERMUT_BIN` points the plugin at a
specific fermut binary.

## Behavior to know

- **No recursion.** fermut marks every suite run it spawns (baseline,
  coverage, per-mutant runs) with `FERMUT_CHILD=1`, and the plugin stays
  inert there. Putting `--fermut` in `addopts` is safe, though it makes every
  pytest run a mutation run.
- **pytest-xdist.** Only the controller runs fermut, once. fermut parallelizes
  mutants itself.
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
