# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
See [VERSIONING.md](VERSIONING.md) for the crate's stability and MSRV policy.

## [Unreleased]

### Added
- `--timeout-factor F` (config `timeout_factor`): size each mutant's timeout
  from the baseline run, `min(--timeout, F × baseline wall + 5 s)`, so hung
  mutants stop in seconds instead of at the flat 30 s. Off by default. With
  `F = 3`: more-itertools 314 s → 162 s, pyjwt 79 s → 59 s, jinja2
  272 s → 236 s. Verdicts unchanged except that a mutant which only makes
  the suite many times slower counts as timed out (2 of ~3,800 mutants on
  more-itertools). `--baseline-secs` supplies the suite time when fermut's
  baseline is skipped; `pytest --fermut` passes its session time.
- `pytest --fermut` narrows to the session's selection: with a path, node
  id, `-k`, `-m`, `--deselect` and similar, only the selected tests run
  against the mutants, and mutants none of them cover are skipped as
  `selection`. The summary shows the counts. Works under pytest-xdist.
- `fermut run --select-tests FILE`: restrict the run to the listed pytest
  node ids (what `pytest --fermut` passes). Such runs aren't recorded in
  history.
- `pytest --fermut-verify-baseline`.

### Changed
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
- `pytest --fermut` skips fermut's baseline run after a green session that
  ran the whole suite (it already proved the suite green). On pyjwt with a
  warm cache this halves the run (10.5 s → 5.6 s). `--fermut-verify-baseline`
  restores it.

### Deprecated
- Operator profile name `default`: it now means `classic` and warns. It will
  be removed in a later release.

### Fixed
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
  saw 13–21 false kills per run (2–3% of mutants), varying run to run.
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

[Unreleased]: https://github.com/KovantAI/fermut/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/KovantAI/fermut/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/KovantAI/fermut/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/KovantAI/fermut/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/KovantAI/fermut/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/KovantAI/fermut/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/KovantAI/fermut/releases/tag/v0.3.0
