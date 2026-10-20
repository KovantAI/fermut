# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
See [VERSIONING.md](VERSIONING.md) for the crate's stability and MSRV policy.

## [Unreleased]

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

[Unreleased]: https://github.com/KovantAI/fermut/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/KovantAI/fermut/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/KovantAI/fermut/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/KovantAI/fermut/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/KovantAI/fermut/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/KovantAI/fermut/releases/tag/v0.3.0
