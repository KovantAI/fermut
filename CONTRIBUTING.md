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
# 99-ish mutants across the bundled calculator.py
```

## Required toolchain

| Tool | Version | Why |
|------|---------|-----|
| Rust | **1.83+** (MSRV declared in `Cargo.toml`) | Build the crate. CI's `msrv` job enforces this. |
| Python | 3.10+ | Target language; the bundled sample's tests need it. |
| `pytest` *or* stdlib `unittest` | latest | Only required when you actually run mutants. Both runners are supported. |
| `ty` | latest | Only required when running with the ty filter enabled (default for `fermut run`). |
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
├── filter/            Filter trait + 8 implementations
├── runner/            Runner trait + pytest + unittest
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
7. Update the operator table in `README.md`.

## Adding a new filter

1. Create `src/filter/<name>.rs` implementing the `Filter` trait.
2. Register it in `src/filter/mod.rs::build_chain` at the appropriate
   cheap-to-expensive position.
3. Wire any CLI flags through `cli/mod.rs::FilterArgs` and
   `cli/build_config.rs`. Add equivalent `FileConfig` fields in
   `src/config/file.rs`.
4. Add unit tests for the filter's `admits()` logic.
5. Document in `README.md` under "Filters".

## Adding a new test runner

1. Create `src/runner/<name>.rs` implementing the `Runner` trait. Use
   `super::prepare_mirror` for the per-mutant project mirror.
2. Add a variant to `RunnerKind` in `src/config/mod.rs` (and the
   corresponding `RunnerCli` value-enum in `src/cli/mod.rs`).
3. Wire dispatch in `src/runner/mod.rs::build`.
4. Document in `README.md` under "Test runners".

## Mutation testing fermut itself

fermut mutation-tests Python. Its own Rust source is mutation-tested by
[cargo-mutants](https://mutants.rs), configured in `.cargo/mutants.toml`.

```sh
cargo install cargo-mutants --locked

# Just what your branch changed — the run you actually want day to day.
# (--src-prefix/--dst-prefix matter; see the note below.)
cargo mutants --in-diff <(git diff --src-prefix=a/ --dst-prefix=b/ origin/main...)

# One module.
cargo mutants --file 'src/mutator/**'

# Whole crate: ~2200 mutants, hours. Overnight job, not an inner-loop one.
cargo mutants
```

Each mutant rebuilds the crate and reruns `cargo test --all-features`. Measured
on a 10-core machine over a 136-mutant run: **median 15s per mutant** (6.6s
build + 8.4s test), p90 37s, plus a one-off ~70s baseline build. Timeouts cost
the full 60s.

`--jobs N` runs N mutants at once, and each one still wants a couple of cores:
`CARGO_BUILD_JOBS=2 cargo mutants --jobs 4` is about the ceiling on 10 cores.
Going to `--jobs 6` without capping `CARGO_BUILD_JOBS` pushed load average past
40 and made each build **ten times slower** (67s instead of 6.6s) — oversubscribe
and you lose more than you gain.

One trap with `--in-diff`: if your git has `diff.mnemonicPrefix` or
`diff.noprefix` set, `git diff` emits `c/`…`w/` (or bare) path prefixes,
cargo-mutants matches no files, and the run reports a clean "No mutants to
filter" instead of an error. Force the prefixes it expects:

```sh
git diff --src-prefix=a/ --dst-prefix=b/ origin/main... > /tmp/pr.diff
cargo mutants --in-diff /tmp/pr.diff
```

Results land in `mutants.out/`: `caught.txt`, `missed.txt`, `timeout.txt`,
`unviable.txt`.

**A survivor in `missed.txt` is a question, not a defect.** Three honest answers:

1. The assertion is genuinely missing — add it. This is the case worth having
   the tool for.
2. The mutant is equivalent to the original, or changes only a log line or an
   error message we deliberately don't assert on. Leave it.
3. The whole file is untestable-by-design plumbing. Add it to `exclude_globs` in
   `.cargo/mutants.toml` with a comment saying why, rather than letting it drown
   the signal for everyone else.

`src/main.rs`, `src/cli/mod.rs`, `src/watch.rs`, and `src/llm/client.rs` are
already excluded on those grounds.

### The score and the floor

The **mutation score** is `caught / (caught + missed)`, as a percentage.
Timeouts and unviable mutants are outside the denominator: an unviable mutant
never compiled and a timeout never reached a verdict, so neither answers "is
this line asserted on?". Leaving them out is also the forgiving direction —
they can't drag the score down.

The **floor** is a single number in `.github/mutants-floor.txt`. Two workflows
enforce it:

| Workflow | Runs | Scope | Fails when |
|---|---|---|---|
| `Mutants` | every PR | only the lines the PR changed | the diff has **≥10 scored mutants** and scores below the floor |
| `Mutants (weekly)` | Sunday 03:00 UTC, or on demand | the whole crate, in 8 shards | the crate scores below the floor — and it files an issue |

**The weekly run is the real signal.** A PR-sized diff is a small denominator:
three mutants can only score 0, 33, 67 or 100%, which measures the shape of the
diff more than the quality of the tests. That is why the PR check ignores its
own number below ten scored mutants and leans on the weekly run to catch those
lines later, as part of the crate. What the PR gate is for is narrower: stopping
a large, thinly-tested change from walking the crate score down before anyone
sees it.

### When the PR check goes red

In descending order of how much you should want each one:

1. **Add the assertion.** Almost always the right answer, and the reason the
   tool is here.
2. **Exclude the mutant** — `exclude_re` (one mutant) or `exclude_globs` (a
   whole file) in `.cargo/mutants.toml`, with a comment saying why it can't be
   killed. This is the honest fix for an equivalent mutant: it drops out of the
   weekly denominator too, so the crate score stops counting something no test
   could reach.
3. **Label the PR `mutants-advisory`.** Downgrades the check to a warning for
   that PR. For the case where the judgement needs a human and shouldn't hold up
   the merge. GitHub reads labels when the run is triggered, so re-run the job
   after adding it.

What is *not* on the list is an assertion written to make the number go up. If
the only way to kill a mutant is a test that asserts on nothing anyone cares
about, that's option 2.

### Moving the floor

Seed it from a weekly run: take the measured score, subtract a few points of
headroom, commit that. `0` means unseeded and both gates are inert.

Raise it when the weekly run has cleared the higher value twice running. Lower
it only as a deliberate, reviewed decision in its own PR — never as the fix for
a red build. A drop is the thing the floor exists to show you.

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

By contributing, you agree your changes are licensed under the same MIT
license as the rest of fermut.
