# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
See [VERSIONING.md](VERSIONING.md) for the crate's stability and MSRV policy.

## [Unreleased]

### Added
- `unittest_top_level` and `unittest_cwd` config keys for the `unittest`
  runner: `-t` for a tests package with relative imports, and the working
  directory for tests that open files relative to it. Both apply to every
  run, the zero-test check and `--schemata`. A baseline that fails on a
  relative import now says to set `unittest_top_level`.
- `--schemata` works with `--runner rstest` (opt-in; the default stays off
  for rstest). The template runs on rstest's vendored pytest.
- `--schemata` works with `--runner unittest` (opt-in; the default stays off
  for unittest). Each fork runs the whole suite failfast.
- Region ignore markers: `# fermut: ignore-start` … `# fermut: ignore-end`,
  and `# fermut: ignore-block`, which covers a compound statement's header
  and body, or the rest of the enclosing block when the marker is on its own
  line.
- `ignore_patterns` config key: regexes matched against source lines. A
  mutant on a matching line is dropped.
- `fermut migrate mutmut` translates mutmut 3's `do_not_mutate` to
  `exclude` and `do_not_mutate_patterns` to `ignore_patterns`, and flags
  `only_mutate` for manual review.
- `run` always writes `.fermut/last.json`, and `show`, `next`, `explain`,
  `suggest` and `autofix` read it when no report path is given.
  `fermut results` is an alias for `show`, so `mutmut results` /
  `mutmut show <id>` map to `fermut results` / `fermut show <id>`.
- MCP `fermut_coverage` tool: builds or incrementally refreshes `.coverage`,
  like `fermut coverage`, and reports a failed pytest as a tool error.
- MCP `fermut_run` and `fermut_score` take a `history_path` argument.

### Changed
- The verdict cache now keys each mutant on the code its covering tests ran,
  from the coverage DB (`cache_scope = "coverage"`, the new default). Editing
  a function re-runs only the mutants whose tests run it, in any file. An
  edit in another file that those tests call now re-runs them too, which the
  old `file` default missed. Import-time code (imports, module constants,
  decorators) re-runs everything. Without coverage it acts as `file`. The
  first run after upgrading misses the whole cache.
- Cached kills and survivors are keyed on individual tests, not whole test
  files: adding or editing a test keeps every verdict that other tests in
  the same file (or `Test*` class) produced. A test still counts its file's
  imports, constants, fixtures and helpers, and a method its class's header
  and non-test methods. A file that doesn't parse counts as a whole.
- A cached survivor whose covering tests changed re-runs only the tests
  that are new or changed, not every covering test: adding a
  test for a survivor re-runs that test. The others passed before and
  are unchanged. A sample is also run against every covering test, at
  the `--cache-audit-rate`, and fermut warns when the two disagree.
  Timed-out verdicts still re-run fully.
- Schema migrations are skipped by default: Django `migrations/0001_*.py`
  in a package and Alembic `versions/*.py` next to `env.py`, shown as
  `migrations` in the skip summary. On healthchecks they were 55 % of
  `hc/api`'s mutants, 76 % surviving (score 44.2 % with them, 68.9 %
  without). `--mutate-migrations` / `mutate_migrations = true` opts back
  in. A Django project's `trend` shows the drop in mutants under the same
  config hash.
- **Breaking:** a runtime error (bad config, red baseline, git or I/O
  failure, a failed `merge` check) now exits `3` instead of `1`, so CI can
  tell a broken run from a failed gate. `1` still means the gate failed
  (survivors, `--fail-under`, `--fail-on-regression`); `2` is a usage error.
  The GitHub action fails a run that exits `3` even with `blocking: false`.
- MCP `fermut_run` appends to `.fermut/agent-history.jsonl` instead of the
  shared `.fermut/history.jsonl`, and `fermut_score` reads it, so agent
  iterations stay out of the project trend. Pass `history_path` for the old
  behavior.
- Human-readable output (`run`, `list`, `show`, `baseline`'s worst files)
  prints source paths relative to the current directory
  (`src/calculator.py:14@216`) instead of the full checkout path. Mutant ids
  and JSON reports still carry absolute paths.
- The unittest runner stops each mutant's run at the first failure
  (`unittest discover -f`). Verdicts don't change; a run that failed and then
  hung is now killed instead of timed out (both count as detected).

### Fixed
- `--runner rstest` without an activated venv: fermut ran the venv's `rstest`
  in its copy of the project, which has no `.venv`, so rstest found no Python
  and the baseline failed. fermut now passes that venv's interpreter with
  `--python`.
- Cached verdicts survive edits above a mutant. The cache keyed each mutant
  by its byte offset in the file, so a line added to one function, or a
  comment added above a `def`, re-ran every mutant below it, even under
  `cache_scope = "scope"`. Mutants inside a top-level `def`/`class` are now
  keyed by their position within it. The first run after upgrading
  re-runs them once.
- Python 3.14: `fermut coverage` recorded only the first test that ran each
  line. coverage.py's default 3.14 core (`sysmon`) drops the per-test
  contexts pytest-cov switches, so fermut ran too few tests per mutant and
  could report false survivors. It now sets `COVERAGE_CORE=ctrace` unless
  you set `COVERAGE_CORE` yourself (an explicit `sysmon` gets a warning).
  Coverage built by an older fermut counts as stale: `fermut coverage` and
  `fermut run` with auto-discovered coverage rebuild it. `fermut run` warns
  when a coverage database looks recorded that way (at least 10 tests, no
  line credited to two of them), and the suggested pytest-cov commands now
  set `COVERAGE_CORE=ctrace`.
- `--schemata` no longer stops forking when the app creates gitignored
  dirs on every start (flaskbb's instance and upload dirs): paths a passing
  baseline adds aren't treated as tests leaving the tree changed. flaskbb:
  0 → 1719 of 1796 mutants run from the schema.
- `fermut coverage` passes `-n0` when pytest-xdist is installed. With `-n`
  in the project's `addopts`, pytest-cov's per-test contexts crashed the
  xdist workers (`no such table: context`) and left an empty `.coverage`
  behind a "Wrote …" message.
- `fermut run path/to/module.py` (a single file) roots the ty filter at the
  file's directory. It handed ty the file as a project root, fell back to
  the subprocess checker, and could reject nearly every mutant as a type
  error (fastapi `routing.py`: 5198 of 5440, 0 tested).
- The project root is also found by `pytest.ini`, `.pytest.ini`, `tox.ini`,
  `setup.py`, or `.git`, not only `pyproject.toml` / `setup.cfg`. A Django
  app with none of the latter walked up to an unrelated ancestor, mirrored
  the wrong tree, and failed the baseline (`pytest exit 4`). Repos without
  any config marker now anchor `.fermut/` at the git root, so a cache left
  in a subdirectory by an earlier run is not reused once.
- `pytest --fermut`: a fermut run that broke after writing its report (exit
  `3`) failed the session as a gate failure (exit `1`); it now exits `3`
  (internal error), as documented.
- GitHub Action: a run that exited `3` with an all-errored report was treated
  as a gate failure, so `blocking: false` downgraded it to a warning. Exit `3`
  now always fails the job.
- `--schemata` with pytest-django on in-memory SQLite: the test database is
  created and migrated once in the template instead of in every fork
  (healthchecks: 1.5 → 0.5 s per mutant, same verdicts). Other databases and
  `django_db_setup` overrides keep the per-fork build.
- Django on Postgres/MySQL (or SQLite with `TEST.NAME`) with `--jobs > 1`:
  every worker created the same test database, the losers' tests all errored,
  and their mutants counted as killed (healthchecks: 100 % vs 81.9 % at
  `--jobs 1`). fermut now sets `TOX_PARALLEL_ENV` per worker (`fm<k>`, after
  your own value inside `tox -p`), which pytest-django appends to test DB
  names. Re-run with `--no-cache` if a cached run scored a Django project
  on such a DB with several workers.
- An explicit `.` path (`fermut run .`, `fermut list .`, `fermut hom .`, MCP
  `fermut_run` with `path: "."`) now mutates the current directory. It was
  read as "no path" and fell back to the config's `source_root`, so
  `exclude` globs written relative to the project root (`src/...`) never
  matched and excluded files were mutated. Omitting the path still uses
  `source_root`.
- `exclude` globs match whether written relative to the project root
  (`src/generated/**`) or to `source_root` (`generated/**`), whichever root
  the run walks. Before, only the walk root counted, so project-root globs
  silently excluded nothing under `source_root = "src"`.
- The rstest runner precompiles and keys its cache on the `python` beside the
  `rstest` binary it runs, not on the interpreter behind a `pytest` on `PATH`.
- On pytest 9.2+, mutants whose tests take a long string parameter holding
  the project path could be reported as errored (`pytest exit 4`) instead
  of killed or survived. pytest 9.2 ids a parameter over 100 characters by
  name instead of by value, so the id coverage recorded in the project could
  differ from the one in fermut's copy. fermut now gives such a parameter the
  id it has in the project (werkzeug from a deep checkout: 506 errored → 0).
- `fermut migrate mutmut` turned `# pragma: no mutate block` / `start` /
  `end` into one-line `# fermut: ignore` markers. Whole regions that had been
  excluded under mutmut were mutated after migrating. These now map to the
  region markers.
- `none-to-value` turned `x is None` into `x is ""`, which compiles with a
  `SyntaxWarning`. Under pytest's `filterwarnings = error` that fails the
  import, so the mutant was killed whatever the tests did (werkzeug: 54 of 149
  such kills). In `is` / `is not` comparisons it now emits `x is str()`, the
  same empty-string object without the warning. Scores on projects that turn
  warnings into errors drop to their honest value, those mutants re-run once
  (new ids), and schemata now serves them instead of falling back.
- The verdict cache ignored a `conftest.py` above the tests root (a repo-root
  one with `tests/` as the tests root), so editing a fixture there kept stale
  verdicts. Every such `conftest.py` is now part of the test fingerprint;
  caches in projects that have one re-run once.
- `fermut init` picked the `unittest` runner when pytest was installed only in
  the project's `.venv` and the venv wasn't activated. It now looks in the
  venv, as `doctor` and `run` do.
- `fermut baseline --format json` printed coverage progress and pytest's
  session output to stdout ahead of the JSON. All of `fermut coverage`'s
  output now goes to stderr.
- The MCP server's `fermut_baseline` tool wrote that same output into the
  JSON-RPC stream, which breaks MCP clients. The server now writes JSON-RPC to
  a private copy of stdout and points stdout at stderr, so nothing a tool or
  child process prints can reach the stream.

## [0.7.0] - 2026-11-04

### Added
- Monorepo aggregation: `fermut merge DIR=REPORT ...` pools several
  projects into one report with a scored-weighted score, repo-relative mutant
  paths and a per-project breakdown (`projects` in JSON and history, a table
  in Markdown/HTML). Each project keeps its own `config_hash`;
  `fermut trend --project DIR` / `--by project` show one or all projects, and
  `--fail-on-regression` also gates each project. A project that scored
  nothing fails the merge. Actions: `sweep` takes `project`, `merge`/`trend`
  take `projects`.
- **Per-mutant memory cap on Linux, macOS and Windows** (`--max-memory`, on by default). A
  runaway mutant (an endless append) used to grow until its timeout — 4–8 GB
  per process, 10–18 GB per run on markupsafe, more-itertools and trio. Each
  mutant now gets `RLIMIT_DATA` of 4 × the baseline's peak memory (at least
  2 GiB); past it Python raises `MemoryError` and the mutant counts as killed.
  Schemata children cap themselves after the fork. `--max-memory 4G` sets it,
  `--no-max-memory` / `max_memory = "0"` turn it off. macOS ignores the
  limit, so there a watchdog samples each mutant's test process every 50 ms
  and kills it past the cap. On Windows each run joins a Job Object with a
  per-process memory limit (subprocesses included), and the baseline's job
  measures the peak the default is sized from.
- **Schemata mode, on by default** with the pytest runner on Unix: every
  function-body mutant is built into one schema, and each mutant runs in a
  fork of a single pytest process that imported and collected the suite
  once, instead of in a fresh pytest. Whole runs were 1.5–3.9× faster on most
  suites tested (apb-mutation 794 s → 203 s; starlette 226 s → 116 s), with
  the same verdicts: 0 differences that held across 3 runs on 10 libraries
  and 2 application suites. Anything it can't vouch for runs the normal way:
  mutants outside function bodies or in code that ran during import or
  collection, a kill or timeout whose tests also fail or hang in a fork with
  no mutant active, a timeout before any test began, and any fork that crashes (on macOS, DNS, proxy and Kerberos
  lookups aren't fork-safe). A confirmed timeout stands without a normal
  re-run, so a hung mutant costs one timeout, not two. About 5% of survivors
  and timeouts are also re-run the normal
  way as an audit; a disagreement is logged and the normal verdict used. `--no-schemata` / `schemata = false` turn it
  off; `--schemata` / `schemata = true` make an unsupported runner or
  platform an error instead of a silent normal run. Cached verdicts are
  shared with normal runs. The schema holds only mutants that will run:
  cache hits and remembered ty rejects stay out, so a warm run after a
  small edit spawns its few misses instead of starting a template, and a
  schema with fewer than 2 mutants per worker (a small `--diff-only` run)
  skips the template.
- `--timeout-factor F` (config `timeout_factor`): size each mutant's timeout
  from the baseline run, `min(--timeout, F × baseline wall + 5 s)`, so hung
  mutants stop in seconds instead of at the flat 30 s. On by default
  (`F = 3`); `--timeout-factor 0` / `--no-timeout-factor` / `timeout_factor = 0` restores the flat
  timeout. Turning it on changes the config hash, so `fermut trend` records a
  config change and flat-timeout cache entries re-run once. With
  `F = 3`: more-itertools 314 s → 162 s, pyjwt 79 s → 59 s, jinja2
  272 s → 236 s. Verdicts unchanged except that a mutant which only makes
  the suite many times slower counts as timed out (2 of ~3,800 mutants on
  more-itertools). `--baseline-secs` supplies the suite time when fermut's
  baseline is skipped; a whole-suite `pytest --fermut` session passes its
  time.
- `pytest --fermut` narrows to the session's selection: with a path, node
  id, `-k`, `-m`, `--deselect` and similar, only the selected tests run
  against the mutants, and mutants none of them cover are skipped as
  `selection`. The summary shows the counts. Works under pytest-xdist.
- `fermut run --select-tests FILE`: restrict the run to the listed pytest
  node ids (what `pytest --fermut` passes). Such runs aren't recorded in
  history. Its baseline check runs only the selected tests, so a broken
  test outside the selection doesn't abort the run. A selection that covers
  none of the code under mutation is an error, not a vacuous N/A pass.
- `fermut autofix --triage skip|audit` and `fermut suggest --triage …`: an
  LLM equivalence judge triages each survivor before a test is generated
  (one extra, cached call). With `skip`, a survivor judged equivalent gets
  no test (autofix outcome `likely-equivalent`) and a suggested
  `# fermut: ignore[...]` marker; for the others, the judge's explanation of
  the behavioural difference goes into the generation prompt. `audit`
  still generates for every survivor and reports how many judged-equivalent
  ones were killed anyway. Off by default; the mutation score never
  changes.
- Errors that can mean fermut guessed wrong about a project (a baseline that
  fails only under fermut, merge conflicts between shards of one config,
  coverage paths all outside the project root) and any panic now point to
  the GitHub issue tracker.
- Worker mirrors are precompiled to checked-hash `.pyc` (PEP 552), so each
  mutant's process loads untouched project modules instead of recompiling
  them. The mutated file always misses its hash and compiles from source.
- `fermut doctor` warns when under 80% of the venv's `site-packages` is
  precompiled (uncompiled dependencies recompile on every mutant spawn) and
  prints the `compileall` fix.

### Changed
- The worker mirror keeps hidden directories other than known junk (`.git`,
  `.venv`, `.pytest_cache`, `.claude`, …), so tests reading scripts or
  fixtures from `.github` or `.data` now find them. Such directories also
  count toward the test-tree hash.
- **`minimal` is the default operator profile.** Ordering compares and
  `and`/`or` used as a condition now get the mutants no other mutant
  subsumes (`a < b` → `<=`, `!=`, `False`) instead of the easily killed swaps
  (`>`, `>=`, `and` ↔ `or`). Expect scores to drop where a suite never tries
  one kind of input; mutant counts rise 0.3–6%. The switch shows in
  `fermut trend` as a config change, not a regression. The subsumption gate
  found 0 violations on 366 fully checked sites across 13 repos, 23 of
  them comparing numpy or pandas values. Set
  `operators = "classic"` (or `--operators classic`) to keep the old
  operators.
- **A run where every mutant is a cache hit skips the baseline suite run.**
  Nothing will be tested, so there's nothing for a red suite to falsify; the
  run reuses the cached verdicts and finishes without running any tests
  (typer 74 s → 3 s, pyjwt 0.3 s). Runs with `--deselect`/`-k`/`-m` narrowing
  skip it too: the last baseline's deselection is stored in the cache, keyed
  on the pytest arguments, config files and the tests tree. Any mutant that
  still needs running brings the baseline check back.

### Deprecated
- Operator profile name `default`: it now means `classic` and warns. It will
  be removed in a later release.

### Fixed
- A mutant that raises `KeyboardInterrupt` inside a test is a kill under
  schemata, as it is in a normal run (pytest exits 2, interrupted).
- `PositionalDrop` no longer shrinks a two-element tuple to a lone starred
  element (`(a, *b)` → `(*b)`), which didn't parse.
- A mutant that makes importing the package raise `SystemExit` (flipping an
  `if __name__ == "__main__": main()` guard) scored `error` on pytest 8+:
  collection doesn't catch it, so pytest reports an internal error and exits
  3. fermut's reporter now records internal errors and whether they ran
  through project code; those (with a green baseline, the mutant's doing)
  score killed, with or without the reporter. A crash in pytest or a
  third-party plugin stays an error. trio: last exit-3 error gone.
- On pytest 9.2, a mutant that breaks the import of a package a `-p` plugin
  loads (trio's `-p trio._tests.pytest_plugin`) scored `error` (`pytest exit
  3: internal error`): pytest 9.2 reports the failed plugin load and exits 3
  before fermut's reporter starts. fermut now re-collects once on exit 3 and
  scores it killed when the traceback runs through project code. On trio, 75
  of 76 such errors became kills.
- Tests that write under `~` no longer cause false kills. typer's app-dir
  tests create and remove a directory under the real `HOME`; parallel workers
  failed each other's runs, and each failure counted as a kill. fermut now
  gives every worker, and every schemata fork, a private, empty `HOME` (and
  `XDG_*` dirs), which also keeps tests out of your real home.
  `--no-private-home` / `private_home = false` keeps the real home for suites
  that need it (a red baseline says so). Cached verdicts from shared-home
  runs are not reused; history hashes are unchanged.
- Verdicts no longer depend on Python's random string hash seed. fermut now
  pins `PYTHONHASHSEED` (a fixed value, or the one you export) for the
  baseline, every mutant run and the schemata template, so a mutant whose
  effect depends on set or dict iteration order (starlette `schemas.py:77`)
  gets the same verdict every run, in a fork or not. `PYTHONHASHSEED=random`
  opts out. Cached verdicts from random-seed runs are not reused; history
  hashes are unchanged.
- The verdict cache keyed the interpreter by how its path was spelled, so
  the same venv reached through an activated `VIRTUAL_ENV`, through `PATH`
  only, or as `x/../.venv` used three separate caches that evicted each
  other, and every run after a switch re-ran everything (typer: ~150 s
  instead of ~60 s). The key now names the interpreter actually used (pytest's
  shebang when `--python` isn't set), by its canonical directory and target.
  Existing cache entries are invalidated once.
- Schemata no longer refuses the template over a third-party daemon thread
  started at import (sqlmesh's analytics flusher made every apb-mutation
  mutant run the normal way: 790 s instead of 233 s). Threads running project
  code, non-daemon threads and threads whose code can't be found still refuse
  it, and the message now names each thread's code.
- Mutants covered only by tests pytest deselects (`--deselect`, `-k`, `-m`
  in `--pytest-arg` or `addopts`) ran with no tests and scored `error`
  (pytest exit 5). The baseline now reports what pytest deselected, and those
  mutants are skipped as `selection`, like `--select-tests`. On werkzeug with
  `--deselect=tests/test_serving.py`, 32 errored mutants are now skipped.
- Mutants whose selected tests are parametrized with an absolute path
  (e.g. werkzeug's `test_from_directory[/repo/tests/res/test.txt]`) were
  scored `error` (pytest exit 4): fermut runs in a copy of the project, where
  pytest names those tests with the copy's path. Node ids are now rewritten
  into the copy and mapped back in results. On werkzeug 124 errored mutants
  now get verdicts.
- False kills from parallel pytest runs sharing one temp root
  (`$TMPDIR/pytest-of-$USER`): their cleanup races, and under
  `filterwarnings = error` the resulting warning fails the run. Each worker
  now has its own root (`PYTEST_DEBUG_TEMPROOT`). A `tmp_path`-heavy project
  saw 13–21 false kills per run (2–3% of mutants), varying run to run. The
  baseline runs under the same root, so a suite that depends on the length of
  its `tmp_path` (typer wraps it at 80 columns) now fails the baseline instead
  of failing every mutant run that selects those tests: on typer 147 of 1205
  kills came from 5 such tests.
- Uneven worker load: mutants were split into fixed contiguous ranges per
  worker, so a range of slow mutants could leave one pytest running alone
  for most of the run. Each mutant is now scheduled on its own. A full
  more-itertools run dropped from ~515 s to ~310 s and a narrowed pyjwt run
  from 128 s to 48 s, with the same scores.

## [0.6.1] - 2026-10-21

### Added
- A timed-out mutant names the test it hung in (`hung_in` in the JSON
  report, and in the console, Markdown, HTML and GitHub annotation output).
  Needs the pytest reporter plugin (on by default).
- Per-mutant timing in the JSON report: `duration_ms` (wall time) on every
  outcome that ran a test, plus `test_ms` (time inside tests) from the pytest
  reporter plugin.
- `fermut merge --expect-shards N` and `--max-errored N`. Sharded reports
  now record their slice (`"shard": {"index", "total"}`), so merge refuses
  mixed shard counts and a missing shard index even without the flag.
- Mutation-score badge for the pytest plugin, published weekly by the
  `Mutation (pytest plugin)` workflow.

### Changed
- `fermut coverage` records import-time lines (module-level code, decorators,
  class bodies, default arguments) under `<test module>|collect`, so their
  mutants are tested instead of skipped as uncovered. On more-itertools this
  scores 496 previously skipped mutants. Expect scores to move. An existing
  `.coverage` is rebuilt in full once.
- `fermut merge --history` refuses to write an entry for a merge that scored
  no mutants (N/A), instead of recording a vacuous 100%.
- `KovantAI/fermut/trend` persists the history on a `fermut-history`
  orphan branch (one file per run under `entries/`) instead of the Actions
  cache, which evicted it after 7 days unused. The job now needs
  `permissions: contents: write`. New `history-branch` and `token` inputs
  replace `history-cache` and `cache-key-prefix`.

### Fixed
- Mutant replacements no longer fuse with an adjacent keyword or name
  (`if~x` → `ifnot (~x)`, `return~x` → `returnNone`), which produced
  unparseable mutants (#162).
- Report JSON serializes the minimal-profile operators under their CLI names
  (`ror-equality`, `ror-const`, `bool-const`); reports using the old names
  still load (#162).
- Programs (`pytest`, `rstest`, `--python`) are resolved against `PATH`
  before changing into the mirror, so a relative `PATH` entry no longer
  misses (#162).
- Mirror paths are canonicalized, so pytest node ids no longer leak the
  macOS `/var` → `/private/var` temp path (#162).
- The mirror copies in-place compiled extensions (`*.so`, `*.pyd`) even when
  gitignored; previously pytest could exit 4 on node ids parametrized on the
  extension (#162).
- A file deleted mid-copy (a filter's temp file) no longer fails the mirror
  build (#162).
- `--record-kill-sets` reads failures from the reporter plugin when it
  loaded, falling back to the `-rfE` summary (#162).
- The GitHub Action no longer passes `--coverage .coverage` when the caller
  supplies `--no-coverage` (#163).

## [0.6.0] - 2026-10-20

### Added
- `pytest --fermut`: a pytest plugin (`pytest11` entry point) that runs
  mutation testing from the pytest command line after a green session,
  prints the score and top survivors, and fails the session on a failed
  gate (#132).
- A result reporter plugin is loaded into every per-mutant pytest/rstest run
  to record the killing test and tell an import-breaking mutant apart from
  a stale node id. `--no-pytest-plugin` turns it off (#132).
- `fermut subsume`: finds dominator mutants from a `--record-kill-sets` run,
  reports the dominator score, and writes `.fermut/dominators.json`.
  `fermut run --only-dominators` (experimental) runs only those mutants and
  infers the kills they imply; `--audit-inferred` re-checks a sample of them
  (#134).
- `fermut run --record-kill-sets` (experimental): writes each mutant's full
  kill-set as JSONL (#127, #134).
- `fermut hom` (experimental): searches for strongly-subsuming second-order
  mutants (#127).
- `KovantAI/fermut/trend` GitHub Action for recording and gating
  mutation-score history (#129).
- `clippy.toml` pinning clippy's MSRV to 1.83, matching `rust-version`.
- `deny.toml` + a `cargo-deny` CI job auditing dependency advisories,
  licenses, bans, and sources (#80).
- OSV scanner in CI, pinned CI requirements, coverage and OpenSSF badges
  (#151, #152, #156).
- This changelog.
- A `cargo-fuzz` target (`fuzz/`) for mutant generation, run on PRs via
  ClusterFuzzLite. It checks that the visitor never panics and that every
  mutant range is in bounds, aligned to UTF-8 boundaries, matches `original`,
  and splices back into Python that parses.
- An `embedded-ty` cargo feature, on by default. The fuzz build turns it off;
  without it, the `ty` filter uses the subprocess path.

### Changed
- Statements that are only a plain string or bytes literal (e.g. attribute
  docstrings) are never mutated, at any position or nesting depth, not just
  leading docstrings (#135).
- Bumped the ruff git dependency (#147).
- Documentation restructured and moved to a new docs toolchain (#149, #158).

### Fixed
- Binary/boolean operator mutants could target the wrong lexeme in a node,
  hiding real mutants and emitting duplicate or equivalent ones (#131).
- `fermut coverage` handles branch coverage (`arc` table) and no longer lets
  mutants spill over onto lines they don't cover (#133).
- `fermut coverage --output` with a relative path resolved differently for
  pytest and fermut (#130).
- A missing import in the mutator (#148).
- The GitHub Actions now cache `kill-order.json`, so smart test ordering
  works in CI (#150).

## [0.5.0] - 2026-10-06

### Added
- `KovantAI/fermut` GitHub Action, with `sweep` and `merge` sub-actions
  (#122).
- fermut's Claude Code skills are exposed for installation (#119).

### Fixed
- An import-breaking mutant under node-id selection (pytest exit 4) is
  scored as killed, not as an error (#123).
- Coverage pruning of deleted and renamed sources, and of test folders
  (#124, #125, #126).
- Redundant imports and incorrect test discovery (#120).
- Dead code and public API cleanup (#121).

## [0.4.1] - 2026-09-10

### Fixed
- Windows release build in CI (#96).

## [0.4.0] - 2026-09-10

### Added
- `fermut coverage`: builds per-test coverage for fermut (#62).
- Smart test ordering: historical killers run first, and pytest `-x`
  short-circuits (#60). A coverage-based prior covers runs with no history
  yet (#63).
- `--max-time` wall-clock limit on the testing phase (#58).
- More mutation operators (#59).
- Trivial Compiler Equivalence filter: mutants that compile to the same
  bytecode as the original are dropped before any test runs (#79).

### Changed
- unittest runner brought to parity with pytest (#75, #92).
- Pytest exit codes mapped explicitly to mutant outcomes (#74).
- Performance work and a large internal refactor into subcommand modules
  (#83, #84, #85, #86, #90, #91, #93).

### Fixed
- Cache concurrency issues (#88).
- Windows double counting and other Windows bugs (#73, #89).
- Asymmetric filters (#76).
- `fermut doctor` path resolution and coverage handling (#61, #94).

## [0.3.0] - 2026-08-31

Baseline release. Earlier history is recorded in the git log and release notes.

[Unreleased]: https://github.com/KovantAI/fermut/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/KovantAI/fermut/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/KovantAI/fermut/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/KovantAI/fermut/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/KovantAI/fermut/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/KovantAI/fermut/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/KovantAI/fermut/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/KovantAI/fermut/releases/tag/v0.3.0
