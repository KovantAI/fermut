<div align="center">

# fermut

**Agent-first mutation testing for Python** — Rust-fast, `ty`-aware.

_Line coverage tells you which lines run. fermut tells you which lines your tests actually **check**._

[![CI](https://github.com/KovantAI/fermut/actions/workflows/ci.yml/badge.svg)](https://github.com/KovantAI/fermut/actions/workflows/ci.yml)
[![PyPI](https://img.shields.io/pypi/v/fermut.svg)](https://pypi.org/project/fermut/)
[![Python](https://img.shields.io/pypi/pyversions/fermut.svg)](https://pypi.org/project/fermut/)
[![Docs](https://readthedocs.org/projects/fermut/badge/?version=latest)](https://fermut.readthedocs.io/)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.83+](https://img.shields.io/badge/rustc-1.83+-orange.svg)](https://www.rust-lang.org/)

</div>

```text
$ fermut run src/ --tests tests/

SURVIVED  src/auth.py:42 [boundary-shift] `age >= 18` → `age > 18`
TIMEOUT   src/slow.py:88 [arith-op-swap]  `n * 2`    → `n / 2`

152 mutants — killed: 140, survived: 8, timeout: 1, skipped: 3 (coverage 3), equivalent: 0, errored: 0  | score: 94.6%
```

Each `SURVIVED` line is a real gap: a change to your source that your tests
didn't notice. fermut makes thousands of these small edits (`<=` → `<`,
`+` → `-`), re-runs your suite against each, and reports the ones that
slipped through. In short — it answers **"are my tests any good?"**

- ⚡ **Fast** — native-Rust hot loop, `ty` pre-filter drops type-invalid
  mutants before they ever reach pytest, per-test coverage selection runs
  only the tests that touch each mutant.
- 🤖 **Agent-first** — machine-readable JSON reports, deterministic mutant
  IDs, `next`/`explain`/`score` subcommands built for a coding agent in the loop.
- 🔁 **Incremental** — cache-friendly warm runs (~35× on unchanged code),
  `--diff-only` scopes mutation to lines changed vs `main`.
- 🚦 **CI-ready** — non-zero exit when any mutant survives, wire it straight
  into a pull-request gate.

Hot loop is native Rust; the test runner stays Python (`pytest` or
`unittest`). Built on [Astral's `ruff_python_parser`](https://github.com/astral-sh/ruff)
for mutant generation and [`ty`](https://github.com/astral-sh/ty) for
type-awareness. Humans welcome too.

## Quickstart

```sh
# install — pick the path that matches you
uv tool install fermut                                             # prebuilt wheel from PyPI, no Rust needed
cargo install --git https://github.com/KovantAI/fermut --locked    # from source: needs Rust + ~2 min first compile

# in your project (needs pytest, coverage, pytest-cov, ty on the same PATH — see Installation)
fermut init                     # detect source/tests/runner, write fermut.toml

# fermut init wires per-test coverage when pytest-cov is present;
# generate it before the first run (skip if your config has no `coverage` key):
pytest --cov=src --cov-context=test    # writes .coverage; fermut auto-discovers it at the project root
# (or, manually: coverage json -o coverage.json --show-contexts, then --coverage coverage.json)

fermut run src/ --tests tests/  # first mutation report (add -q to silence progress logs)
```

For a CI gate, generate the opinionated profile instead — diff-only +
coverage + a narrow operator set, tuned to fail fast on pull requests:

```sh
fermut init --profile pr-gate   # writes a CI-shaped fermut.toml (needs git + .coverage)
```

It scopes mutation to lines changed vs `main`, so run it inside a git
checkout with that base available — see
**[CI quickstart](docs/guides/ci-quickstart.md)**.

> **Install paths.** Prebuilt wheels ship to PyPI, so
> `uv tool install fermut` (or `pipx`/`pip install fermut`) is the
> no-Rust path. The `cargo install --git` path (Rust required) builds
> from source — for hacking on fermut or platforms without a prebuilt
> wheel. Full prerequisites in
> **[Installation](docs/getting-started/installation.md)**.

## Why fermut

On **typer** (~2,200 mutants, cold run, same machine):

| Tool   | Time  | Mutation score |
|--------|-------|----------------|
| fermut | 164s  | 71.5%          |
| mutmut | 298s  | 70.5%          |

fermut is **faster and higher-scoring** here — its `ty` pre-filter
and per-test coverage selection cut wall-clock without dropping
rigor. The win is suite-shaped: on tiny pure-Python libraries with
fast full suites (e.g. more-itertools) mutmut's simpler model still
wins. Full numbers, the repos where mutmut wins, and methodology:
**[benchmarks](docs/reference/benchmarks.md)**.

## Documentation

**Full documentation lives at <https://fermut.readthedocs.io/>**, sourced
from the [`docs/`](docs/) directory in this repo.

Start here:

- **[Coding agents](docs/guides/coding-agents.md)** — agent-driven inner
  loop, cache strategy, JSON parsing recipes.
- **[First steps](docs/getting-started/first-steps.md)** — `fermut init`
  then `fermut run`, first report in under five minutes.
- **[Installation](docs/getting-started/installation.md)** — prerequisites
  and build paths.
- **[Working on projects](docs/guides/projects.md)** — gradual rollout,
  profile choice, CI integration.
- **[CLI reference](docs/reference/cli/index.md)** — every subcommand,
  every flag.
- **[Configuration reference](docs/reference/configuration.md)** — every
  `fermut.toml` key.
- **[Operators](docs/reference/operators/index.md)** — the full mutation
  operator catalogue.
- **[Landscape](docs/concepts/landscape.md)** — vs `mutmut` and `cosmic-ray`.
- **Migrating?** [from mutmut](docs/guides/migrate-from-mutmut.md) /
  [from cosmic-ray](docs/guides/migrate-from-cosmic-ray.md) — or run
  `fermut migrate {mutmut,cosmic-ray}` for one-shot translation.
- **[Internals](docs/reference/internals/index.md)** + **[Adding an
  operator](docs/reference/internals/adding-operators.md)** — contributor
  onboarding.

## Status

fermut is pre-1.0. The CLI, config schema, and JSON report shape may
change between `0.MINOR` releases. See
[`VERSIONING.md`](VERSIONING.md) for the full contract.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE)
or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this crate by you, as defined in the
Apache-2.0 license, shall be dual-licensed as above, without any
additional terms or conditions.
