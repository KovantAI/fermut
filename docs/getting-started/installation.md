# Installation

fermut builds as a self-contained binary. Most people get it as a
prebuilt wheel from PyPI — no Rust toolchain required:

```sh
# Prebuilt wheel from PyPI, NO Rust required:
uv tool install fermut

# From source, requires Rust stable on PATH:
cargo install --git https://github.com/KovantAI/fermut --locked
```

Both put `fermut` on `PATH` in an isolated environment. The source
install takes ~2 minutes the first time (compiles `ruff_*` from
git); subsequent `cargo install` upgrades are incremental.

> **Distribution status.** Prebuilt wheels ship to PyPI, so
> `uv tool install fermut` (or `pipx`/`pip`) is the no-Rust path for
> everyone. `cargo install --git` is the from-source path, which
> **does require Rust stable on PATH** — use it for hacking on fermut
> or on platforms without a prebuilt wheel.

## Quick install (recommended)

fermut ships as a self-contained binary, but it drives `pytest`,
`coverage`, and `ty`, all of which must resolve against **your
project's interpreter** — not a system Python. The recipe below
assumes a per-project virtualenv (uv layout shown; Poetry/pip-venv
work the same way).

### 1. Set up a project venv if you don't already have one

```sh
cd path/to/your/project
uv venv                                  # creates .venv/ using uv's pinned Python
uv sync --all-extras --dev               # install project deps + dev extras
source .venv/bin/activate                # put .venv/bin first on PATH
```

For an existing uv project, just `uv sync` then activate. If you
prefer `uv run` over an activated shell, you can prefix every
command — `uv run fermut doctor`, `uv run pytest`, etc. — and skip
the activate step.

Why this matters: `fermut doctor` resolves `python`, `pytest`,
`coverage`, and `ty` from the current shell's `PATH`. Without an
activated venv, a macOS box with system `python3` 3.9 will fail the
≥ 3.10 check even when your project's actual interpreter is 3.12.

### 2. Install fermut

```sh
uv tool install fermut
```

`uv tool install` puts `fermut` on `PATH` in an isolated environment
of its own, so it doesn't pollute the project venv. It resolves the
prebuilt wheel from public PyPI — no Rust toolchain required.

Alternatives:

```sh
uv add --dev fermut   # add as project dev dep (good for CI reproducibility)
pipx install fermut   # via pipx
pip install fermut    # plain pip
```

All resolve from public PyPI. For a from-source install (hacking on
fermut or an unsupported platform), see
[Build from source](#build-from-source).

### 3. Install the Python deps fermut drives

Add `pytest`, `coverage`, `pytest-cov`, and `ty` to the project's
dev group so they land in the same `.venv` your tests run from:

```sh
uv add --dev pytest coverage pytest-cov
uv tool install ty                       # ty is global; one install serves every project
```

The [prerequisites table](#prerequisites) below has the full list
and explains why each one earns its slot.

That's enough to run `fermut --help` and `fermut doctor`. The rest
of this page is the prerequisites table, optional tools, and the
"build from source" path.

## Prerequisites

| Tool                                       | Status   | Required for                                                          | Install                                                                                                              |
|--------------------------------------------|----------|-----------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------|
| Python ≥ 3.10                              | required | Running your tests under mutation                                     | `brew install python` or `uv python install 3.12`                                                                    |
| `pytest`                                   | required | Test runner fermut invokes (stdlib `unittest` works for legacy suites; pytest is the supported path) | `uv add --dev pytest`                                                                                                |
| `coverage` + `pytest-cov`                  | required | Per-test coverage filter — fermut refuses runs without it (see below) | `uv add --dev coverage pytest-cov`                                                                                   |
| `ty`                                       | required | Type-aware mutant pre-filter (20–40% of naive mutants dropped in ~25 ms before pytest sees them) | `uv tool install ty`                                                                                                 |
| `ruff` *(optional)*                        | optional | Lint-based pre-filter, off by default                                 | `uv tool install ruff`                                                                                               |
| `gh` *(optional)*                          | optional | `fermut pr-comment` (preinstalled on every GitHub Actions runner)     | <https://cli.github.com>                                                                                             |

All four required tools must be on the **same `PATH`** as `fermut`
— i.e. inside the activated project venv (or behind `uv run`). A
fresh shell without venv activation is the #1 cause of `fermut
doctor` failures on macOS, where the system `python3` is 3.9 and
nothing in the project venv is reachable.

### Why coverage, pytest-cov, and ty are required (not "nice to have")

- **`coverage` + `pytest-cov`** drive per-test selection. fermut runs
  only the tests whose execution touched the mutated line, not the
  full suite per mutant. Without them the wall time for a 500-mutant
  / 30-second-suite project goes from a few minutes to **4+ hours**
  — past the point where mutation testing earns its keep on any real
  PR loop. `pytest-cov` specifically is what tags each line with a
  pytest nodeID; plain `coverage run` cannot
  ([why](../guides/coverage.md#generate-coveragejson-with-per-test-contexts)).
  Generate the coverage data the easy way with
  [`fermut coverage`](../reference/cli/coverage.md) — it sets the
  context flag for you and refreshes incrementally as tests change.
  `fermut run` refuses to start when `--coverage` is configured but
  the coverage data lacks per-test contexts; if you genuinely want to
  opt out, drop `coverage = "…"` from `fermut.toml` and accept the
  slowdown.
- **`ty`** is the type-aware pre-filter. On type-hinted codebases
  20–40% of naive mutants are type-invalid and get dropped in
  ~25 ms each before pytest sees them
  ([benchmarks](../reference/benchmarks.md)). On by default;
  `--no-ty-filter` opts out for older untyped suites.

`fermut doctor` reports each of the four as a hard check — a
missing tool surfaces as `[fail]`, not `[warn]`. Run it after
installation to confirm the venv resolves all four before you spend
time generating coverage.

`fermut doctor` checks the toolchain programmatically — see the
[CLI reference](../reference/cli/doctor.md) and the
[troubleshooting guide](../reference/troubleshooting/index.md).

## Build from source

Skip this section unless you're hacking on fermut itself or running on
a platform without a prebuilt wheel.

Requires Rust stable (`brew install rustup` or
`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`).

### Steps

```sh
git clone https://github.com/KovantAI/fermut
cd fermut
cargo build --release
# binary at ./target/release/fermut
```

The first build pulls `ruff_python_parser` / `ruff_python_ast` /
`ruff_text_size` from GitHub. To bump, edit the three `git = …` entries in
`Cargo.toml` together and re-run `cargo build`.

To install the source-built binary on `PATH`:

```sh
cargo install --path .
```

### Pre-commit hooks (optional)

```sh
pipx install pre-commit          # or: brew install pre-commit
pre-commit install               # installs the git hook
pre-commit run --all-files       # runs fmt, clippy, check on the whole tree
```

The hook config (`.pre-commit-config.yaml`) runs:

| Hook                          | What                                                                                              |
|-------------------------------|---------------------------------------------------------------------------------------------------|
| `cargo fmt --check`           | rejects unformatted Rust                                                                          |
| `cargo clippy -D warnings`    | rejects lints / warnings                                                                          |
| `cargo check --all-targets`   | rejects build breaks across lib/bin/tests                                                          |
| generic hooks                 | trailing whitespace, EOF newline, line endings, YAML/TOML parse, large files, merge conflict markers |

Tests are intentionally **not** in pre-commit — they run in CI / pre-push.

## MSRV (Minimum Supported Rust Version)

fermut declares `rust-version = "1.83"` in `Cargo.toml`. Builds on earlier
toolchains fail with a clear cargo message. CI runs a dedicated `msrv` job
on every PR (`cargo check --locked` on 1.83) to catch accidental use of
post-MSRV features.

**`Cargo.lock` is committed** — it pins exact dependency versions for
reproducible builds and is what the MSRV job uses (`--locked`). Do not
delete it. `cargo update` produces lockfile churn that should land in its
own PR.

### MSRV bump policy

- Bump only when a required dependency forces it (typically a `ruff_*` tag
  upgrade or a hard-pinned `clap` / `toml` major).
- Bump in a dedicated PR, never alongside feature work.
- Bump `Cargo.toml::package.rust-version` **and** the `toolchain:` line in
  `.github/workflows/ci.yml::msrv` together — they're the source of truth
  for the contract.
- Mention the bump in `CHANGELOG.md` as a breaking change.

## Verify the install

With your project venv activated (or via `uv run`), run:

```sh
fermut --version
fermut doctor                            # or: uv run fermut doctor
```

`doctor` checks Python, the test runner, `ty`, `ruff`, and coverage,
and prints a one-line remediation per failure. If `python` or
`pytest` fails the check, confirm the project venv is on `PATH`
(`which python` should point inside `.venv/`); a system Python 3.9
on macOS is the usual culprit.

If everything's green, you're ready for
**[First steps](first-steps.md)**.

### Smoke test against the included sample (source-build only)

If you cloned the repo for a source build, you can sanity-check the
binary against the sample project shipped in `examples/sample/`. It
already ships a `fermut.toml`, so **skip `fermut init` there** (init
refuses to overwrite an existing config — pass `--force` if you really
want to regenerate it):

```sh
cargo run --release -- list examples/sample/src
# expect 45 mutants across 4 operator types — the sample's fermut.toml
# restricts the operator set (`ops = [...]`) to keep the demo focused
cargo run --release -- list examples/sample/src --experimental
# still 45: the experimental operators aren't in the sample's whitelist,
# so --experimental adds nothing here. Delete the `ops`/`skip_ops` keys
# from examples/sample/fermut.toml to exercise the full catalogue.
```

For the full run (requires `pytest` on PATH):

```sh
cargo run --release -- run examples/sample/src \
    --tests examples/sample/tests \
    --no-ty-filter
# expect SURVIVED lines on the boundary-shift mutants of `in_range`
# (the sample test suite intentionally doesn't probe the boundary)
```

This section only applies to the source-build path —
`uv tool install` doesn't ship `examples/sample/`.

## Next

- **[First steps](first-steps.md)** — run your first mutation report.
- **[Features](features.md)** — quick tour of what fermut can do.
- **[Shell completion](../reference/shell-completion/index.md)** —
  install shell completions for `bash`, `zsh`, `fish`, `powershell`,
  `elvish`.
