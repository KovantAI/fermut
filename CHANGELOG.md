# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
See [VERSIONING.md](VERSIONING.md) for the crate's stability and MSRV policy.

## [Unreleased]

### Added
- `clippy.toml` pinning clippy's MSRV to 1.83, matching `rust-version`.
- `deny.toml` + a `cargo-deny` CI job auditing dependency advisories, licenses,
  bans, and sources (flags any non-crates.io source beyond the pinned
  `astral-sh/ruff` git dependency).
- This changelog.

## [0.3.0] - 2026-08-31

Baseline release. Earlier history is recorded in the git log and release notes.

[Unreleased]: https://github.com/KovantAI/fermut/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/KovantAI/fermut/releases/tag/v0.3.0
