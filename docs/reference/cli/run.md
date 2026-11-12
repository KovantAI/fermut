# `fermut run`

Generate mutants, run tests against each, report killed / survived.

```sh
fermut run [PATH] [flags...]
```

| Flag                        | Default                          | Effect                                                              |
|-----------------------------|----------------------------------|---------------------------------------------------------------------|
| `PATH`                      | `.`                              | Python source root.                                                 |
| `--tests <dir>`             | `<PATH>/tests`                   | Test directory.                                                     |
| `--jobs <n>`                | num CPUs                         | Parallel workers (one mutant per worker at a time).                  |
| `--timeout <s>`             | `30`                             | Per-mutant test timeout.                                            |
| `--no-ty-filter`            | off                              | Skip the ty pre-filter.                                             |
| `--ruff-filter`             | off                              | Enable the ruff lint pre-filter.                                    |
| `--tce`                     | off                              | Enable the TCE (bytecode-equivalence) pre-filter: drop mutants that `compile()` to a byte-identical code object. Requires `python3` (or `FERMUT_PYTHON`). Also `tce = true`. |
| `--hypothesis-seed <N>`     | none                             | Pin Hypothesis seed across mutants. Ignored (with a warning) when the test interpreter can't import `hypothesis`, since pytest would reject the flag. See [mutation testing concepts](../../concepts/mutation-testing.md#vocabulary). |
| `--pytest-arg <ARG>`        | none, repeatable                 | Forward arbitrary args to pytest.                                   |
| `--no-cache`                | cache on                         | Disable the result cache.                                           |
| `--cache-path <p>`          | `<project-root>/.fermut/cache.json`     | Custom cache file location. Project root = nearest `pyproject.toml`/`setup.cfg` ancestor. |
| `--cache-audit-rate <r>`    | `0.05`                           | Fraction (0–1) of killer-keyed cache hits re-verified against their killing test (at least one when any occur). |
| `--no-cache-audit`          | audit on                         | Skip the killer-hit audit (same as `--cache-audit-rate 0`).         |
| `--no-history`              | history on                       | Skip the run-history log append for this run.                        |
| `--history-path <p>`        | `<project-root>/.fermut/history.jsonl`  | Custom history log location (same anchor as the cache).             |
| `--sample <r>`              | `1.0`                            | Test only this fraction of mutants (0.0–1.0), deterministic.         |
| `--sample-seed <N>`         | `0`                              | Seed for `--sample` selection.                                       |
| `--shard <i/n>`             | off                              | Distributed exec: process only the i-th of n disjoint slices.        |
| `--runner pytest\|rstest\|unittest` | `pytest`                 | Pick the Python test runner. `rstest` is a pytest-CLI-compatible drop-in. |
| `--python <PATH>`           | auto-discover                    | Interpreter (path) or virtualenv (dir) to run the tests with (pytest, rstest or unittest) — fermut launches the runner through that interpreter, no PATH reliance. Omitted → auto-discovers an active venv / nearby `.venv`; else pytest/rstest fall back to the bare executable on PATH and unittest to `python3`/`python`. See [Choosing the interpreter](#python-interpreter). |
| `--isolation auto\|copy\|hardlink\|reflink` | `auto` | How worker mirrors are populated. See [Isolation](run-isolation.md). |
| `--annotate`                | auto in GHA                      | Emit `::error` / `::warning` annotations for CI.                     |
| `--watch`                   | off                              | Re-run on every `.py` change until Ctrl+C.                            |
| `--format human\|json`      | `human`                          | stdout output format.                                                |
| `--json <p>`                | none                             | Also write JSON report. A `killed` outcome carries an optional `killer` (the killing test's node id) when pytest coverage selection could name it. A `timed_out` outcome carries an optional `hung_in` (the test running when the timeout hit; needs the reporter plugin). Every outcome that ran a test carries `duration_ms` (wall time, spawn to exit) and, with the reporter plugin, `test_ms` (time inside tests; `duration_ms - test_ms` is process, import and collection overhead). A cached verdict keeps the timing of the run that produced it. |
| `--junit <p>`               | none                             | Also write JUnit XML.                                                |
| `--html <p>`                | none                             | Also write self-contained HTML report.                                |
| `--markdown <p>`            | none                             | Also write Markdown summary (for PR comments).                        |
| `--trend`                   | off                              | Include a compact trend block at the top of the Markdown report.      |
| `--experimental`            | off                              | Include experimental operators.                                       |
| `--mutate-migrations`       | off                              | Mutate Django `migrations/0001_*.py` and Alembic `versions/*.py` too. Skipped by default, shown as `migrations` in the summary. Also `mutate_migrations`. |
| `--parity`                  | off                              | Include parity operators (cross-tool comparison only — very noisy, never for normal scoring). See [Parity operators](../operators/parity.md). |
| `--operators <profile>`     | `minimal`                        | Compare / `and`-`or` operator set: `minimal`, `classic`, `full`. See [Operator profiles](../operators/profiles.md). |
| `--ops <list>`              | all                              | Allowlist operators.                                                  |
| `--skip-ops <list>`         | none                             | Denylist operators (wins over `--ops`).                                |
| `--diff-only [base]`        | off                              | Restrict to lines changed vs base ref (default `main`).               |
| `--since <SPEC>`            | off                              | Restrict to lines touched since commit or date. See [Incremental runs](#incremental-runs-since). |
| `--no-diff-only`            | off                              | Override `diff_only`/`since` from config — force full sweep. Mutually exclusive with `--diff-only` and `--since`. |
| `--coverage [path]`         | flag absent: auto-discover `.coverage`; bare `--coverage`: `coverage.json` | Per-mutant test selection from coverage contexts. Accepts a `.coverage` SQLite DB or a `coverage.json` export (format sniffed). Auto-discovery of `.coverage` at the project root happens only when the flag is absent; a bare `--coverage` with no path means `coverage.json`. See [Coverage](#coverage-integration). |
| `--no-coverage`             | off                              | Override `coverage` from config — disable coverage filtering for this run. Mutually exclusive with `--coverage`. |
| `--select-tests <FILE>`     | off                              | Only use the tests listed in `FILE`. Mutants that only unselected tests cover are skipped as `selection`. The run isn't recorded in history. Needs coverage. See [Test selection](run-test-selection.md#a-subset-of-the-suite-select-tests). |
| `--exclude <GLOB>`          | none, repeatable                 | Glob patterns excluding paths from mutation collection (relative to source root or project root). Example: `--exclude 'alembic/**' --exclude 'tests/integration/**'`. When passed at least once, replaces config `exclude` (no merge). |
| `--no-equiv-detect`         | detector on                      | Skip the equivalent-mutant detector (AST patterns + CPython bytecode). Survivors are not post-processed; equivalents stay counted. |
| `--cache-scope file\|scope` | `file`                           | Cache-key granularity. `file` hashes the whole file's AST; `scope` hashes the enclosing top-level def/class only, so sibling-function edits keep cache hits. `scope` can return stale verdicts when one test indirectly exercises another function. |
| `--fail-under <SCORE>`      | none (any survivor fails)        | Pass when mutation score is at least `SCORE` (0.0–100.0). Equal to threshold passes. |
| `--no-verify-baseline`      | baseline check on                | Skip the pre-flight run of the unmutated suite. By default fermut runs your full suite once and aborts if it isn't green (a red suite would inflate the score toward 100%). Skip only when you've already confirmed green (e.g. CI ran it). |
| `--no-private-home`         | private home on                  | Run tests with your real `HOME`. By default every worker, and every schemata fork, gets a private, empty `HOME` (and `XDG_*` dirs), so tests that create files under `~` can't fail each other across workers (a false kill) or write into your home. Use this when the suite needs real home state: `~/.gitconfig`, credentials, caches under `~`. Such a suite fails the baseline check otherwise, with a hint naming this flag. See [Isolation](run-isolation.md#private-home). |
| `--baseline-timeout <SECS>` | `300`                            | Wall-clock cap for the baseline run. Separate from `--timeout` (which bounds a single mutant) because the baseline runs the whole suite. A suite that exceeds it is killed and the run aborts. Raise for large suites. |
| `--no-smart-order`          | on                               | Disable smart test ordering. On by default, timeout or not — ordering only permutes the selected set, so the score and `--fail-on-regression` gate stay order-invariant. See [Smart test ordering](#smart-test-ordering). |
| `--smart-order`             | off                              | Force smart test ordering on over `smart_order = false` in config. Conflicts with `--no-smart-order`. |
| `--no-pytest-plugin`        | on                               | Don't load fermut's reporter plugin into per-mutant pytest/rstest runs; infer outcomes from exit codes and output instead. See [Result reporter plugin](#result-reporter-plugin). |
| `--no-schemata`             | schemata on                      | Start a fresh pytest per mutant instead of forking one that already imported and collected the suite. Schemata is on by default with the pytest runner on Unix. See [Schemata](run-schemata.md). |
| `--schemata`                | on where supported               | Require schemata: an unsupported runner or platform is an error instead of a silent normal run. Overrides `schemata = false`. |
| `--max-time <SECS>`         | off (whole catalogue)            | Wall-clock ceiling on the testing phase. Evaluates highest-value mutants first (covered before uncovered); once the deadline passes, untested mutants are recorded as `skipped`/`time-budget` (excluded from the score) instead of run — in-flight mutants finish. A predictable time ceiling for PR gates. See [Time-boxed runs](#time-boxed-runs-max-time). |
| `--timeout-factor <F>`      | `3` (`0` = flat `--timeout`)     | Size each mutant's timeout from the baseline run: `min(--timeout, F × baseline wall + 5 s)`, so hung mutants stop in seconds. Needs the baseline; without one, the flat `--timeout` applies. See [Timeout budgets](#timeout-budgets). |
| `--no-timeout-factor`       | off                              | Use the flat `--timeout` for every mutant. Same as `--timeout-factor 0` / `timeout_factor = 0`. |
| `--max-memory <SIZE>`       | auto                             | Cap each mutant's memory, e.g. `512M`, `4G`; `0` for off. A mutant past it fails with `MemoryError` and counts as killed. See [Memory cap](#memory-cap). |
| `--no-max-memory`           | off                              | Don't cap mutant memory. Same as `max_memory = "0"`. |
| `--baseline-secs <SECS>`    | none                             | Wall time of a green whole-suite run you already did. When fermut's baseline is skipped it sizes `--timeout-factor` timeouts in its place (fermut's own baseline wins when it runs). Ignored without `--timeout-factor`. A whole-suite `pytest --fermut` session passes its session time in `FERMUT_BASELINE_SECS`. |
| `--record-kill-sets <PATH>` | off                              | [experimental] Write each mutant's full kill-set (every covering test that fails) to `PATH` as JSONL, the input to [`subsume`](subsume.md) and [`hom`](hom.md). Drops pytest's `-x`, so it is much slower. Pytest/rstest only. Pair with `--no-cache`: a cache hit runs nothing and records nothing. |
| `--only-dominators`         | off                              | [experimental] Run dominator mutants from `.fermut/dominators.json` and infer the kills they imply; for nightly sweeps, refused with `--diff-only`/`--since`/`--record-kill-sets`. See [`fermut subsume`](subsume.md#fermut-run-only-dominators-experimental). |
| `--audit-inferred <RATE>`   | `0.05`                           | Requires `--only-dominators` (the parser rejects it otherwise). Under `--only-dominators`: fraction of inferred kills re-run against their killing test; any that survives fails the run. |
| `--fail-on-regression <PTS>`| off                              | Exit non-zero when score dropped more than `PTS` vs the most recent prior entry on the same git branch with the same `config_hash` and fermut version (a config edit or upgrade re-baselines). Requires history. Ignored in `--watch`. |
| `--trend-branch <NAME>`     | none                             | Restrict the `--trend` markdown block's "previous run" lookup to entries recorded on this branch. Requires `--trend`. |
| `--no-fail`                 | off                              | Never exit non-zero because of the mutation result: write every report and exit 0 even with survivors. For report-only runs (trend shards, dashboard feeds). Replaces the `--fail-under 0` idiom. Mutually exclusive with `--fail-under` and `--fail-on-regression`. Also suppresses the exit for a scoreless / all-errored run, which is still reported as N/A. |

Exit code: by default `1` when any mutant survives. With
`--fail-under <SCORE>`, `1` only when the score is below `SCORE`. A scoreless
run (nothing scored) exits `0` unless some mutant errored, then `1`. With
`--fail-on-regression <PTS>`, also `1` when the score dropped more than `PTS`,
and always `1` when combined with `--no-history` (the gate has nothing to read).
Under `--only-dominators`, `1` when any audited inferred kill fails to
reproduce (`.fermut/dominators.json` is stale). `--no-fail` always exits `0`
on the mutation result. A run that errors out (bad config, red baseline, git
failure) exits `3`, even with `--no-fail`; clap usage errors exit `2`. Wire the gate into CI to fail on mutation score.

## Smart test ordering { #smart-test-ordering }

When coverage selects **more than one** test for a mutant, fermut runs
them under pytest's `-x` (stop at the first failure), so the mutant dies
as soon as any selected test fails. Smart ordering runs the **most likely
killer first**, so `-x` short-circuits sooner — cutting per-mutant
wall-clock on hot lines covered by many tests. It picks that order in two
layers:

1. **Cold-start breadth prior.** With no run history, the **most targeted**
   test leads — the one covering the fewest lines *of the mutated file* —
   because a test focused on that file is the likelier killer. Scoping to
   the mutated file (rather than the test's repo-wide footprint) keeps a
   broad integration test that heavily exercises the mutated function ranked
   ahead of a test that merely grazes one of its lines. This needs no
   history: the signal comes from the coverage data already loaded, so it
   helps on the first run and on freshly-changed `--since` lines.
2. **Learned history.** A test that has **historically killed this
   `(file, operator)`** is lifted ahead of the breadth order. History
   dominates once a killer is known; breadth fills the no-history gap.

It's **on by default**, including under a per-mutant timeout (see below),
and self-training: each run records which test killed which
mutant into `.fermut/kill-order.json` (advisory — losing it only costs a
slow run), and later runs read it to order. The win is proportional to
tests-selected-per-mutant; a mutant covered by one test gains nothing.

**Concurrent runs may lose learnings.** The sidecar is rewritten with a
plain load-modify-save at the end of a run, with no file locking. Two
`fermut run` invocations sharing the same `.fermut/` (e.g. parallel shards
in CI, or `--shard`) both read the old file and each overwrites it — the
last writer wins and the other run's newly-learned kills are dropped. This
only forfeits some ordering speedup on the next run, never a verdict. To
keep every shard's learnings, point each at its own `kill_order_path`.

**Ordering never changes the mutation score** — only *which* test pytest
tries first. `-x` exits non-zero iff *some* selected test fails, independent
of order, so a mutant that survives (or is detected) does so either way.

**Ordering stays on under `--timeout`.** With a per-mutant timeout, reaching
the killer sooner can convert a `timed_out` into a `killed` — but both count
as *detected*, and reordering can never produce or remove a `survived`
mutant. So the mutation score and the `--fail-on-regression` gate (score +
survivor ids) are order-invariant with or without a timeout. Killer-first
ordering therefore helps **most** under a timeout, by reaching the kill
before the deadline instead of burning it on slow non-killer tests. (A
`timed_out` outcome is cached, so once cached it is not re-evaluated even if
ordering would now reach the kill in time — the cache, not ordering, decides
that.)

Disable explicitly with `--no-smart-order` (or `smart_order = false` in
config); it then uses the plain coverage order.

The score is order-invariant only when the tests are. A test that asserts on
state an earlier test changed passes or fails depending on what ran before
it, and the learned order changes between runs, so its mutants flip between
killed and survived. Fix such tests, or run with `--no-smart-order`. See
[Framework compatibility](../../guides/frameworks.md#fastapi).

Ordering relies on pytest honoring the node-id order fermut passes on the
command line. A test-shuffling plugin (`pytest-randomly`,
`pytest-random-order`) re-sorts collected tests and silently defeats it —
disable the plugin for fermut runs (e.g. `pytest_args = ["-p",
"no:randomly"]`) if you want the `-x` short-circuit.

The killing test comes from fermut's [result reporter
plugin](#result-reporter-plugin). With `--no-pytest-plugin`, fermut parses
pytest's `FAILED`/`ERROR` summary lines instead, reading them uncolored (it
pipes stdout, so pytest drops color by default). Forcing color on regardless
(`PY_COLORS=1`, `force_color`, or `--color=yes` in `addopts`) wraps those
lines in ANSI escapes, so the killer isn't recorded. That only forfeits the
next run's ordering speedup, never a verdict.

## Result reporter plugin { #result-reporter-plugin }

Every per-mutant pytest (or rstest) run loads a small fermut plugin with
`-p _fermut_reporter`. fermut puts it on the run's `PYTHONPATH` from a
directory beside the worker's mirror, so nothing is installed in your venv
and `PYTEST_DISABLE_PLUGIN_AUTOLOAD` doesn't affect it. The plugin writes
what happened to a per-run JSON-lines file, and fermut reads it:

- **Which test killed the mutant**: the first failed test, used by smart
  ordering and the killer-keyed cache. No stdout capture.
- **Whether the mutant broke an import.** Under coverage-driven node-id
  selection, a mutant that breaks test code's import makes pytest exit 4,
  the same code as a stale node id. The plugin tells them apart: a test
  module that failed to collect, a conftest that failed to import the mutated
  code, or project code raising while pytest configures itself (for example,
  a `filterwarnings` entry naming a warning class in the package under test)
  all count as kills.

The exit code still decides every other verdict. If the plugin produces no
result, for example because it couldn't load, fermut warns once and falls
back to exit codes, a `--collect-only` re-run for exit 4, and output parsing.
`--no-pytest-plugin` (or `pytest_plugin = false`) forces that fallback.

## Timeout budgets (`--timeout-factor`) { #timeout-budgets }

A mutant that sends the code into an infinite loop runs until `--timeout`
(30 s by default). On a suite that takes a second or two, that wait is most
of the run: on more-itertools 85 hung mutants took 63% of all worker time.

By default fermut sizes the timeout from the baseline run instead (`--timeout-factor 3`; `--timeout-factor 0` turns this off). The
baseline runs the whole suite once, and each mutant runs a subset of it, so
an honest run fits in `F × baseline wall + 5 s` (5 s covers interpreter
start-up and running beside the other workers). `--timeout` stays the upper
limit. With a 1.4 s suite and `F = 3`, mutants get 10 s instead of 30 s.

- **Verdicts.** A mutant that doesn't hang but makes the suite many times
  slower now counts as timed out (detected) instead of survived. PIT and
  Stryker treat it the same way. Across 6 repos under `--schemata` only 2
  mutants changed this way (more-itertools), while runs with hung mutants
  got 1.2–3.4× faster. Set `--timeout-factor 0` (or `--no-timeout-factor`) to keep the old verdicts.
- **Needs a baseline.** With `--no-verify-baseline` there is nothing to
  scale from, so every mutant uses the flat `--timeout` (fermut warns if you
  set the factor yourself),
  unless `--baseline-secs SECS` gives the wall time of a green whole-suite run
  you already did. [`pytest --fermut`](../../guides/pytest-plugin.md) does
  this on a whole-suite session: it passes its own session time (the summed
  test time under pytest-xdist, since a mutant runs serially) in
  `FERMUT_BASELINE_SECS`.
- **Busy machines.** The baseline runs alone; mutants run beside
  `--jobs` other workers. On an oversubscribed runner a mutant that runs
  most of the suite can take several times the baseline and count as timed
  out. If timeouts climb under load, raise `F` or lower `--jobs`.
- **`fermut run` only.** `autofix` and `hom` re-run mutants with the flat
  `--timeout`.
- **History and cache.** The factor is part of the run's configuration, so
  `fermut trend` shows the switch to the default as a config change, and
  verdicts cached by flat-timeout runs are re-run once. The computed budget
  is not: it moves a little with each baseline and would otherwise invalidate
  the cache on every run.

Set it in config as `timeout_factor` under `[tool.fermut]`.

## Memory cap (`--max-memory`) { #memory-cap }

A mutant that turns a loop into an endless append grows until its timeout:
single processes reached 4–8 GB, and with several workers a whole run
10–18 GB, enough to swap or get a CI runner killed. fermut caps each
mutant's memory on Linux, macOS and Windows. Past the cap the mutant's run
fails (`MemoryError` on Linux and Windows, a kill on macOS) and the mutant
counts as killed — usually long before its timeout.

- **Default.** 4 × the baseline run's peak memory, at least 2 GiB, so a clean
  mutant has the room the clean suite had and more. Without fermut's own
  baseline (`--no-verify-baseline`) there is nothing to size from and mutants
  run uncapped unless you pass `--max-memory`.
- **Verdicts.** The default cap only ends runaway mutants sooner (killed
  instead of timed out, both detected). An explicit `--max-memory` is part of
  the cache and history scope, since a cap below what the suite needs would
  turn survivors into kills.
- **Platforms.** On Linux the kernel enforces it. macOS ignores that limit,
  so fermut samples each mutant's test process every 50 ms and kills it past
  the cap (also scored killed); a fast-growing mutant can overshoot by one
  sample, and memory in xdist workers or the test's own subprocesses isn't
  counted. On Windows each mutant's run joins a Job Object that caps every
  process in it, the test's own subprocesses included, at that much committed
  memory; the baseline's job measures its peak for the default. If Windows
  refuses the job (fermut itself runs in a job that forbids nesting), fermut
  warns once and runs uncapped.

Set it in config as `max_memory` under `[tool.fermut]`.

## Schemata { #schemata }

With the pytest runner on Unix, mutants run in forks of one pytest that
already imported and collected the suite, instead of a fresh pytest each:
1.5–3.9× faster on most suites tested, with the same verdicts. What runs
the normal way instead, how kills are confirmed, and how to turn it off:
**[Schemata](run-schemata.md)**.

## Time-boxed runs (`--max-time`) { #time-boxed-runs-max-time }

`--max-time <SECS>` caps the **testing phase** at a wall-clock ceiling —
a time budget instead of a mutant budget. For a PR gate a time ceiling
is more predictable than `--sample`: it bounds how long the job runs
regardless of how the mutant count grows.

How it works:

- Mutants are ordered **highest-value first** — mutants a coverage-selected
  test can actually reach sort ahead of uncovered ones (which the coverage
  filter would skip anyway). Parallel workers mean this biases *start* order
  rather than strictly serializing, but since uncovered mutants are cheap
  coverage-skips the expensive budget still lands on covered mutants — so the
  mutants left untested at the deadline are the least informative.
- Once the deadline passes, every mutant **not yet started** is recorded as
  `skipped` with filter `time-budget`. Mutants already **in flight finish** —
  the ceiling is soft by one slowest-mutant.
- Budget-skipped mutants are **excluded from the score denominator** (like a
  coverage or shard skip), so the reported score is over the mutants that
  actually ran. The count surfaces in the summary line
  (`skipped: N (time-budget N)`) and as a `WARN` log, so a truncated run never
  looks like a clean full sweep. A run where the budget expired before *any*
  mutant ran scores **N/A**, not a vacuous 100%.

The budget covers the testing phase only. Baseline verification, mutant
generation, and the `ty` pre-filter are separate fixed costs it does not
bound — the same floor `--sample` has. Pair with `--no-verify-baseline` in
CI (where the suite already ran) to keep the whole job under budget.

Set it in config as `max_time` under `[tool.fermut]`.

## CLI overrides vs config

CLI flags override the loaded config. Precedence is **CLI > config-file >
built-in defaults**. The common pattern: keep `fermut.toml` tuned to your
primary profile (usually `pr-gate`), then have other CI jobs override
individual knobs from YAML:

```yaml
# Same fermut.toml as the PR gate, broadened to a nightly sweep.
- run: |
    fermut run src/ \
        --tests tests/ \
        --experimental \
        --ops "" \
        --timeout 60 \
        --markdown nightly.md
```

Each list (`ops`, `skip_ops`) merges independently — a CLI `--ops`
overrides the file's `ops` but leaves the file's `skip_ops` intact.

## Coverage integration { #coverage-integration }

With per-test coverage (a `.coverage` auto-discovered at the project root,
or `--coverage <path>`), fermut skips mutants on lines no test executes and
runs each remaining mutant against only the tests that executed its line.
Details, precedence and the freshness guard:
**[Test selection](run-test-selection.md)**.

## Selecting tests (`--select-tests`) { #select-tests }

`--select-tests FILE` restricts the run to the tests listed in `FILE`;
tests pytest deselects (`-k`, `-m`, `--deselect`) narrow it the same way.
See **[Test selection](run-test-selection.md#a-subset-of-the-suite-select-tests)**.

## Incremental runs (`--since`)

`--diff-only main` is branch-relative. fermut resolves `git merge-base main
HEAD`, then diffs that commit against the **working tree** (`git diff
<merge-base>`), so the line set is the branch's committed changes **plus
uncommitted edits**. Changes that landed on `main` after the branch point are
not included. Right tool for PR-time gates. When `main` isn't a local ref
but `origin/main` is (CI PR checkouts), fermut uses `origin/main`. If the merge base can't be
resolved (a shallow clone, unrelated histories), fermut exits with an error
instead of running zero mutants. Fetch full history (`fetch-depth: 0`) or
pass `--no-diff-only`.

For "what's changed since I last ran fermut?", "only test the last
week", or "run against a specific tag", use `--since`:

```sh
fermut run pkg/ --since v1.2.0
fermut run pkg/ --since HEAD~10
fermut run pkg/ --since abc123
fermut run pkg/ --since 2025-12-01
fermut run pkg/ --since '1 week ago'
```

`SPEC` is resolved in two steps:

1. **Git ref** — `git rev-parse --verify <SPEC>^{commit}`. Resolves
   SHAs, branches, tags, `HEAD~N`, reflog notation.
2. **Date** — if rev-parse fails, fall back to
   `git log -1 --before=<SPEC> --format=%H HEAD`. Anything `git log
   --before` accepts: ISO dates, `'yesterday'`, `'1 week ago'`.

Then `git diff --unified=0 --relative <commit>` produces the line set.
This diff **includes uncommitted working-tree changes** — editing a file
locally re-runs its mutants without needing to commit.

`--diff-only` and `--since` are mutually exclusive on the command line.
A CLI scope replaces the config's: `--since HEAD` works on a config that
sets `diff_only = "main"`, and `--diff-only` overrides a config `since`.
Setting both keys in the config is an error.

## Choosing the interpreter { #python-interpreter }

The pytest runner needs a Python interpreter that has `pytest` installed.
With no `--python` and no venv found, fermut spawns a bare `pytest` resolved from `PATH` — which
breaks in environments that won't let you put a venv's `bin/` on `PATH`
(locked-down agent sandboxes, some CI), and is ambiguous when several
interpreters are around.

`--python` removes the ambiguity. Give it either an **interpreter path**
(`--python /opt/py/bin/python3.12`) or a **virtualenv directory**
(`--python .venv` — fermut maps it to the interpreter inside). fermut then
runs `<python> -m pytest`, so the run uses *that* interpreter's pytest and
its venv's packages, with no dependence on `PATH`.

When `--python` is omitted, fermut auto-discovers an interpreter likely to
have pytest, in order:

1. the active virtualenv (`$VIRTUAL_ENV`);
2. a `.venv` found walking up from the source root (stopping at the repo
   root);
3. otherwise `None` — fall back to a bare `pytest` on `PATH` (the historical
   behavior).

Only venvs are auto-discovered: a bare system `python3` may lack pytest, so
fermut leaves that case to the PATH fallback. Set it in `fermut.toml` with
`python = ".venv"` (relative values resolve against the config's directory).
The interpreter is part of the cache key and the history config hash, so
switching interpreters never reuses another's cached verdicts or marks the
runs as directly comparable.

`--python` applies to every runner. `pytest` runs `<python> -m pytest` and
`unittest` runs `<python> -m unittest`. `rstest` is a native binary with no
Python module, so fermut runs the `rstest` beside the interpreter (the venv's
`bin/`), falling back to `rstest` on `PATH`.
With no venv found, `unittest` falls back to the `python3` (or `python`) on
`PATH`.

## Isolation modes { #isolation-modes }

Each worker runs mutants in its own mirror of the project, so a patched
file never touches your source, and tests run with a private `HOME`.
`--isolation auto|copy|reflink|hardlink` picks how mirrors are built
(`auto` by default). See **[Isolation](run-isolation.md)**.
