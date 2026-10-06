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
| `--hypothesis-seed <N>`     | none                             | Pin Hypothesis seed across mutants. See [mutation testing concepts](../../concepts/mutation-testing.md#vocabulary). |
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
| `--python <PATH>`           | auto-discover                    | Interpreter (path) or virtualenv (dir) to run pytest with — fermut invokes `<python> -m pytest`, no PATH reliance. Omitted → auto-discovers an active venv / nearby `.venv`, else a bare `pytest` on PATH. See [Choosing the interpreter](#python-interpreter). |
| `--isolation auto\|copy\|hardlink\|reflink` | `auto` | How worker mirrors are populated. See [Isolation modes](#isolation-modes). |
| `--annotate`                | auto in GHA                      | Emit `::error` / `::warning` annotations for CI.                     |
| `--watch`                   | off                              | Re-run on every `.py` change until Ctrl+C.                            |
| `--format human\|json`      | `human`                          | stdout output format.                                                |
| `--json <p>`                | none                             | Also write JSON report. A `killed` outcome carries an optional `killer` (the killing test's node id) when pytest coverage selection could name it. |
| `--junit <p>`               | none                             | Also write JUnit XML.                                                |
| `--html <p>`                | none                             | Also write self-contained HTML report.                                |
| `--markdown <p>`            | none                             | Also write Markdown summary (for PR comments).                        |
| `--trend`                   | off                              | Include a compact trend block at the top of the Markdown report.      |
| `--experimental`            | off                              | Include experimental operators.                                       |
| `--parity`                  | off                              | Include parity operators (cross-tool comparison only — very noisy, never for normal scoring). See [Parity operators](../operators/parity.md). |
| `--ops <list>`              | all                              | Allowlist operators.                                                  |
| `--skip-ops <list>`         | none                             | Denylist operators (wins over `--ops`).                                |
| `--diff-only [base]`        | off                              | Restrict to lines changed vs base ref (default `main`).               |
| `--since <SPEC>`            | off                              | Restrict to lines touched since commit or date. See [Incremental runs](#incremental-runs-since). |
| `--no-diff-only`            | off                              | Override `diff_only`/`since` from config — force full sweep. Mutually exclusive with `--diff-only` and `--since`. |
| `--coverage [path]`         | auto-discover `.coverage`        | Per-mutant test selection from coverage contexts. Accepts a `.coverage` SQLite DB or a `coverage.json` export (format sniffed). When unset, fermut auto-discovers `.coverage` at the project root. See [Coverage](#coverage-integration). |
| `--no-coverage`             | off                              | Override `coverage` from config — disable coverage filtering for this run. Mutually exclusive with `--coverage`. |
| `--exclude <GLOB>`          | none, repeatable                 | Glob patterns excluding paths from mutation collection (relative to source root). Example: `--exclude 'alembic/**' --exclude 'tests/integration/**'`. When passed at least once, replaces config `exclude` (no merge). |
| `--no-equiv-detect`         | detector on                      | Skip the equivalent-mutant detector (AST patterns + CPython bytecode). Survivors are not post-processed; equivalents stay counted. |
| `--cache-scope file\|scope` | `file`                           | Cache-key granularity. `file` hashes the whole file's AST; `scope` hashes the enclosing top-level def/class only, so sibling-function edits keep cache hits. `scope` can return stale verdicts when one test indirectly exercises another function. |
| `--fail-under <SCORE>`      | none (any survivor fails)        | Pass when mutation score is at least `SCORE` (0.0–100.0). Equal to threshold passes. |
| `--no-verify-baseline`      | baseline check on                | Skip the pre-flight run of the unmutated suite. By default fermut runs your full suite once and aborts if it isn't green (a red suite would inflate the score toward 100%). Skip only when you've already confirmed green (e.g. CI ran it). |
| `--baseline-timeout <SECS>` | `300`                            | Wall-clock cap for the baseline run. Separate from `--timeout` (which bounds a single mutant) because the baseline runs the whole suite. A suite that exceeds it is killed and the run aborts. Raise for large suites. |
| `--no-smart-order`          | on                               | Disable smart test ordering. On by default, timeout or not — ordering only permutes the selected set, so the score and `--fail-on-regression` gate stay order-invariant. See [Smart test ordering](#smart-test-ordering). |
| `--smart-order`             | off                              | Force smart test ordering on over `smart_order = false` in config. Conflicts with `--no-smart-order`. |
| `--max-time <SECS>`         | off (whole catalogue)            | Wall-clock ceiling on the testing phase. Evaluates highest-value mutants first (covered before uncovered); once the deadline passes, untested mutants are recorded as `skipped`/`time-budget` (excluded from the score) instead of run — in-flight mutants finish. A predictable time ceiling for PR gates. See [Time-boxed runs](#time-boxed-runs-max-time). |
| `--fail-on-regression <PTS>`| off                              | Exit non-zero when score dropped more than `PTS` vs the most recent prior entry on the same git branch. Requires history. Ignored in `--watch`. |
| `--trend-branch <NAME>`     | none                             | Restrict the `--trend` markdown block's "previous run" lookup to entries recorded on this branch. Requires `--trend`. |

Exit code is non-zero when any mutant survives — wire that into CI to
gate on mutation score.

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

It's **on by default** (except when a per-mutant timeout is set — see the
caveat below) and self-training: each run records which test killed which
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

Ordering relies on pytest honoring the node-id order fermut passes on the
command line. A test-shuffling plugin (`pytest-randomly`,
`pytest-random-order`) re-sorts collected tests and silently defeats it —
disable the plugin for fermut runs (e.g. `pytest_args = ["-p",
"no:randomly"]`) if you want the `-x` short-circuit.

Learning the killer also relies on parsing pytest's `FAILED`/`ERROR`
summary lines, which fermut reads uncolored (it pipes stdout, so pytest
drops color by default). Forcing color on regardless — `PY_COLORS=1`,
`force_color`, or `--color=yes` in `addopts` — wraps those lines in ANSI
escapes and the killer isn't recorded. This only forfeits the next run's
ordering speedup, never a verdict; drop the forced color for fermut runs
to keep the learning working.

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

## Coverage integration

`--coverage` accepts **either** coverage.py's native `.coverage` SQLite
database **or** a `coverage.json` export — the format is sniffed, so
both work. Prefer the database on large suites (the JSON export repeats
every node id per covered line and can reach multiple GB; the SQLite
form stores it once).

**Auto-discovery.** When neither `--coverage` nor the `coverage` config
key is set, fermut looks for a `.coverage` SQLite DB at the project root
and uses it automatically (only if the file sniffs as a real SQLite DB).
So on the common path you set nothing — just generate `.coverage` and
run fermut. Precedence is:

**`--coverage <path>` (CLI) > `coverage = "…"` (config) > auto-discovered
`.coverage` at the project root > none.**

`--no-coverage` disables coverage selection entirely, overriding config
and auto-discovery.

**Freshness guard.** fermut compares the coverage file's mtime against
your `.py` sources and tests. A stale **auto-discovered** `.coverage`
(older than code you've since edited) is *ignored* — the run proceeds
without coverage selection rather than silently skipping mutants on
changed lines. A stale **explicitly wired** file is still used, but with
a warning. Either way, regenerate with `pytest --cov=src
--cov-context=test` (or `fermut coverage`) to clear it. See
[Coverage → Freshness guard](../../guides/coverage.md#freshness-guard).

The easy way to produce `.coverage` is [`fermut coverage`](coverage.md),
which also refreshes incrementally when tests change:

```sh
fermut coverage
fermut run src/ --tests tests/            # auto-discovers .coverage
```

or pass it explicitly (`--coverage .coverage`). Under the hood
`fermut coverage` runs `pytest --cov=src --cov-context=test`, which
writes `.coverage`; you can run that yourself instead. The
`--cov-context=test` is mandatory — it records per-test contexts.

Or, manually, generate a JSON export instead (the legacy path; per-test
contexts via `--show-contexts` are mandatory):

```sh
pytest --cov=src --cov-context=test
coverage json -o coverage.json --show-contexts
fermut run src/ --tests tests/ --coverage coverage.json
```

Without contexts, coverage only knows which lines ran, not which tests
ran them, so per-mutant test selection isn't possible. fermut refuses
the run in that case.

When coverage is wired, the filter chain skips any mutant whose line has
zero test contexts, and the pytest runner narrows the invocation per
mutant to only the tests that exercised the mutated line.

## Incremental runs (`--since`)

`--diff-only main` is branch-relative — a three-dot diff (`git diff
main...HEAD`) against the merge base with `main`, so it scopes to this
branch's **committed** changes only (uncommitted edits are not included;
use `--since` for those). Right tool for PR-time gates.

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

`--diff-only` and `--since` are mutually exclusive.

## Choosing the interpreter { #python-interpreter }

The pytest runner needs a Python interpreter that has `pytest` installed.
By default fermut spawns a bare `pytest` resolved from `PATH` — which
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

(`--python` applies to the pytest runner; the `unittest` runner is
unaffected.)

## Isolation modes

Each worker thread builds one mirror of the project tree, reused across
every mutant that worker processes. `--isolation` picks how the mirror
is populated. The patched file is always unlinked and rewritten (never
edited in place), so the patched file can't clobber the real source —
but tests that write to *other* files inside the tree share their writes
with the original when in `hardlink` mode.

| Mode       | What it does                                                                                              | Build cost                              | Safety                                                                                  |
|------------|-----------------------------------------------------------------------------------------------------------|-----------------------------------------|-----------------------------------------------------------------------------------------|
| `auto`     | Reflink/clonefile on supported filesystems (APFS, btrfs, xfs, zfs, ReFS); else plain copy.                | Near-zero on CoW filesystems; full copy otherwise. | Same as `copy` — mirror is independent.                                                |
| `copy`     | Plain recursive `fs::copy`.                                                                                | Slowest; baseline.                       | Fully independent.                                                                       |
| `reflink`  | Force reflink/clonefile, fall back to copy on unsupported filesystems.                                     | Near-zero on CoW filesystems.            | Fully independent.                                                                       |
| `hardlink` | Hardlink every file from source into the mirror.                                                           | Cheapest — just inode entries.           | Shares inodes; safe for the patched file (unlinked before write), unsafe if tests rewrite *other* files. |

You can also set the mode in `fermut.toml` (`isolation = "hardlink"`) or
via the `FERMUT_ISOLATION` environment variable. Precedence is the
standard CLI > env > config-file > default.

!!! note "macOS / APFS"

    Rust's `std::fs::copy` already uses APFS `fclonefileat` under the
    hood, so `copy` and `reflink` perform nearly identically on macOS;
    `hardlink` is often *slower* (extra metadata work per file). The
    mode matters most on Linux ext4 / tmpfs and other filesystems
    without CoW.
