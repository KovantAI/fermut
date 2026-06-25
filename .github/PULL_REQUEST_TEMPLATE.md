<!--
Thanks for the PR. A few notes before you submit:

- Run `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` locally. CI runs all three.
- User-visible changes: add an entry under `## [Unreleased]` in `CHANGELOG.md`.
- Breaking changes: see VERSIONING.md for what counts and how to deprecate.
- For new operators / filters / runners, follow the recipe in CONTRIBUTING.md.

Sections marked OPTIONAL can be deleted if irrelevant. The other sections should always have content.
-->

## Summary

<!-- One or two sentences. What does this change, why now? -->

## Type of change

<!-- Tick all that apply. -->

- [ ] Bug fix (non-breaking)
- [ ] New feature (non-breaking)
- [ ] New mutation operator
- [ ] New filter
- [ ] New test runner / output format
- [ ] Performance improvement
- [ ] Refactor / cleanup (no behavior change)
- [ ] Documentation
- [ ] CI / tooling
- [ ] **Breaking change** (CLI, config, report shape, operator name, MSRV bump)

## Test plan

<!-- Required. What did you run? What new tests did you add? -->

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --all-features --locked` passes
- [ ] New tests cover the change
- [ ] Verified manually against `examples/sample/` (or a real project — link the repo)

## Breaking change notes (OPTIONAL)

<!-- Only fill in if the "Breaking change" box above is ticked.
Describe what breaks, the migration path, and whether a deprecation period is in place. See VERSIONING.md. -->

## CHANGELOG entry (OPTIONAL)

<!-- Required for user-visible changes. Paste the entry you added under `## [Unreleased]`. -->

```
### Added | Changed | Deprecated | Removed | Fixed | Security
- <terse line>
```

## Linked issues

<!-- "Closes #123", "Refs #456". One per line. -->

## Screenshots / output samples (OPTIONAL)

<!-- For HTML report, CLI output, or anything visual. -->
