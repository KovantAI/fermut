# Contributing to fermut

Thanks for considering a contribution. This doc covers what you need to be
productive: dev setup, layout, workflow, and the rules-of-thumb the existing
code follows.

## Quick start

```sh
git clone https://github.com/KovantAI/fermut
cd fermut

# Rust toolchain (stable; MSRV is 1.83).
rustup default stable

# Build + run the test suite.
cargo test --all-features

# Optional: install pre-commit hooks so each commit runs fmt/clippy/check.
pipx install pre-commit
pre-commit install
```

The bundled sample lives in `examples/sample/`. Smoke-test mutation generation:

```sh
cargo run -- list examples/sample/src
# 45 mutants across the bundled calculator.py
```

## Required toolchain

| Tool | Version | Why |
|------|---------|-----|
| Rust | **1.83+** (MSRV declared in `Cargo.toml`) | Build the crate. CI's `msrv` job enforces this. |
| Python | 3.10+ | Target language; the bundled sample's tests need it. |
| `pytest`, `rstest` *or* stdlib `unittest` | latest | Only required when you actually run mutants. All three runners are supported. |
| `ty` | latest | Optional: the ty filter (on by default) runs in-process. The binary is only needed with `FERMUT_TY_EMBEDDED=0` or when the embedded checker can't be built. |
| `ruff` | latest | Only required when running with `--ruff-filter`. |

## Repository layout

```
src/
├── main.rs            crate entry
├── lib.rs             module map (see `//!` docs)
├── cli/               clap parser + dispatch + subcommand handlers
├── config/            runtime Config + TOML schema + walk-up loader
├── mutator/           AST visitor + operator catalog
├── emit.rs            byte-range source splice
├── filter/            Filter trait + 11 implementations
├── runner/            Runner trait + pytest (also drives rstest) + unittest
├── engine.rs          parallel orchestration
├── cache.rs           per-mutant result cache
├── report/            outcome types + writers + GHA annotations
└── watch.rs           --watch mode
```

Every `mod.rs` opens with `//!` documenting what lives where. Start there
when navigating an unfamiliar area.

## Workflow

1. **Branch** off `main`. Use a descriptive name (`add-keyword-arg-drop`,
   `fix-cache-hash-collision`).
2. **Make your change**. Keep it focused — one PR, one concern.
3. **Run `cargo test --all-features` locally**. Add tests alongside the code
   you change. Tests live in `#[cfg(test)] mod tests {}` at the bottom of
   each file (see e.g. `src/mutator/operators.rs`).
4. **Run `cargo fmt --all` and `cargo clippy --all-targets --all-features --
   -D warnings`** — same gates CI runs.
5. **Update `CHANGELOG.md`** under `## [Unreleased]` if your change is
   user-visible.
6. **Open a PR** against `main`. CI runs fmt, clippy, test, and the MSRV
   build-check on every push.
7. **Code review** by the Kovant AB maintainers. Squash-merge after
   approval + green CI.

## Adding a new mutation operator

1. Add a variant to `enum Operator` in `src/mutator/operators.rs`.
2. Add a `name()` arm. Use kebab-case; prefix `exp:` for experimental.
3. Add the variant to `Operator::all()` and bump the `all_covers_every_variant`
   test count.
4. Set `is_experimental()` if applicable.
5. Implement the visitor hook in `src/mutator/visitor.rs`:
   - **Table-driven** (op-swap style): add entries to the relevant
     `*_SWAPS` constant in `operators.rs` and the matching `for (orig, repl)
     in TABLE` loop in `visitor.rs` picks it up.
   - **Bespoke**: add a new arm to `visit_stmt` or `visit_expr` and call
     `self.push(Operator::Yours, range, "replacement")`.
6. Add a test in `src/mutator/visitor.rs::operator_emission_tests` proving
   the new operator fires on a representative Python snippet.
7. Add a row to `docs/reference/operators/stable.md` (or `experimental.md` /
   `parity.md`) and bump the operator counts; the
   `docs_stay_in_sync_with_operator_catalogue` test checks them. See
   [Adding an operator](docs/reference/internals/adding-operators.md).
8. The `fuzz/` target checks every mutant splices back into parseable Python
   (CI runs it via ClusterFuzzLite on PRs). To run it locally you need Linux,
   because the macOS linker rejects the sanitizer build:

   ```sh
   docker run --rm -v "$PWD":/src -w /src/fuzz rustlang/rust:nightly \
     sh -c 'cargo install cargo-fuzz && cargo fuzz run mutate -- -max_total_time=300'
   ```

## Adding a new filter

1. Create `src/filter/<name>.rs` implementing the `Filter` trait.
2. Register it in `src/filter/mod.rs::build_chain` at the appropriate
   cheap-to-expensive position.
3. Wire any CLI flags through `cli/mod.rs::FilterArgs` and
   `cli/build_config.rs`. Add equivalent `FileConfig` fields in
   `src/config/file.rs`.
4. Add its name to `CANONICAL_ORDER` in `src/filter/mod.rs` (the chain-order
   tests enforce it), and unit tests for the filter's `admits()` logic.
5. Document it in `docs/guides/filters.md`.

## Adding a new test runner

1. Create `src/runner/<name>.rs` implementing the `Runner` trait. Use
   `super::with_worker_mirror` and `super::apply_patch` for the per-worker
   project mirror.
2. Add a variant to `RunnerKind` in `src/config/mod.rs` (and the
   corresponding `RunnerCli` value-enum in `src/cli/mod.rs`).
3. Wire dispatch in `src/runner/mod.rs::build`.
4. Document it in `docs/reference/cli/run.md` (the `--runner` flag).

## Style

- **Rustfmt**: `rustfmt.toml` is committed; CI rejects unformatted code.
- **Clippy**: `-D warnings`. No `#[allow]` annotations unless justified
  in-context. Don't use `#[allow]` to skip clippy lints — fix them.
- **Tracing**: prefer `tracing::info!` / `warn!` over `println!` for
  diagnostics. `println!` is for end-user output.
- **anyhow**: use for application errors. `thiserror` for typed errors that
  cross module boundaries.
- **No `unwrap()` in non-test code** unless the invariant is locally proven
  (e.g. just-built rayon pool).
- **No `#[allow(missing_docs)]`** on public items — every `pub` item in
  `lib.rs`'s module map deserves a doc comment.

## Commit messages

Prefer [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(filter): add ruff lint pre-filter
fix(cache): hash short-circuit on missing file
docs: clarify --since vs --diff-only semantics
chore: bump dependabot weekly to monday
```

Type tags we use: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`,
`ci`, `chore`, `deps`. The CHANGELOG generator will pick these up.

## CHANGELOG

User-visible changes go under `## [Unreleased]` in `CHANGELOG.md`. Internal
refactors (no behavior change) do not need an entry. Format follows [Keep a
Changelog](https://keepachangelog.com/).

## Versioning

fermut follows [SemVer 2.0.0](https://semver.org/) for the user-facing
contract — see [VERSIONING.md](VERSIONING.md) for what's stable, what
counts as a breaking change, and the deprecation process. **TL;DR while
we're pre-1.0**: `0.MINOR` bumps may break, `0.MINOR.PATCH` bumps don't.

## Release process

Maintainers only.

```sh
# 1. Roll Unreleased → x.y.z in CHANGELOG.md.
# 2. Bump Cargo.toml `version`.
# 3. Open a release PR; merge after green CI.
# 4. Tag the merge commit:
git tag -a v0.2.0 -m "v0.2.0"
git push origin v0.2.0
# 5. .github/workflows/release.yml builds wheels for 5 targets and publishes
#    them via PyPI trusted publishing: a workflow_dispatch run goes to
#    TestPyPI, and a pushed v* tag publishes to public PyPI.
```

## Reporting issues

Use the GitHub issue templates. Include:

- `fermut --version`
- `rustc --version`
- Minimal Python snippet that reproduces (or a public link)
- Full command line + output

For security issues, **do not** open a public issue. See
[SECURITY.md](SECURITY.md).

## License

By contributing, you agree your changes are dual-licensed under the same
terms as the rest of fermut: MIT OR Apache-2.0, at the user's option.
