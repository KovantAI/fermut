<div align="center">

# fermut

**Agent-first mutation testing for Python** — Rust-fast, `ty`-aware.

_Line coverage tells you which lines run. fermut tells you which lines your tests actually **check**._

[![CI](https://github.com/KovantAI/fermut/actions/workflows/ci.yml/badge.svg)](https://github.com/KovantAI/fermut/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/KovantAI/fermut/graph/badge.svg)](https://codecov.io/gh/KovantAI/fermut)
[![Mutation (pytest plugin)](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/KovantAI/fermut/badges/mutation-plugin.json)](https://github.com/KovantAI/fermut/actions/workflows/mutation.yml)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/KovantAI/fermut/badge)](https://scorecard.dev/viewer/?uri=github.com/KovantAI/fermut)
[![PyPI](https://img.shields.io/pypi/v/fermut.svg)](https://pypi.org/project/fermut/)
[![Downloads](https://static.pepy.tech/badge/fermut/month)](https://pepy.tech/project/fermut)
[![Python](https://img.shields.io/pypi/pyversions/fermut.svg)](https://pypi.org/project/fermut/)
[![Docs](https://readthedocs.org/projects/fermut/badge/?version=latest)](https://fermut.readthedocs.io/)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.83+](https://img.shields.io/badge/rustc-1.83+-orange.svg)](https://www.rust-lang.org/)

</div>

```text
$ fermut run src/ --tests tests/

SURVIVED  src/auth.py:42@1187 [boundary-shift] `age >= 18` → `age > 18`
TIMEOUT   src/slow.py:88@2304 [arith-op-swap] `n * 2` → `n / 2`

152 mutants — killed: 140, survived: 8, timeout: 1, skipped: 3 (coverage 3), equivalent: 0, errored: 0  | score: 94.6%
```

Each `SURVIVED` line is a real gap: a change to your source that your tests
didn't notice. fermut makes thousands of these small edits (`<=` → `<`,
`+` → `-`), re-runs your suite against each, and reports the ones that
slipped through. In short — it answers **"are my tests any good?"**

- ⚡ **Fast** — native-Rust hot loop, `ty` pre-filter drops type-invalid
  mutants before they ever reach pytest, per-test coverage selection runs
  only the tests that touch each mutant, and mutants run in forks of one
  pytest that already imported your suite (schemata).
- 🤖 **Agent-first** — machine-readable JSON reports, deterministic mutant
  IDs, `next`/`explain`/`score` subcommands built for a coding agent in the loop.
- 🔁 **Incremental** — an unchanged re-run is answered from the cache
  (pyjwt: ~38s cold, ~0.3s warm, measured together after the
  [benchmark table](https://fermut.readthedocs.io/reference/benchmarks/#warm-and-loop-runs-the-incremental-story)), and `--diff-only` scopes mutation to
  lines changed vs `main`.
- 🚦 **CI-ready** — non-zero exit when any mutant survives (or, with
  `--fail-under`, when the score drops below a threshold), wire it straight
  into a pull-request gate.

Hot loop is native Rust; the test runner stays Python (`pytest`, the
pytest-compatible `rstest`, or `unittest`). Built on [Astral's `ruff_python_parser`](https://github.com/astral-sh/ruff)
for mutant generation and [`ty`](https://github.com/astral-sh/ty) for
type-awareness. Humans welcome too.

## Quickstart

```sh
# install — pick the path that matches you
uv tool install fermut                                             # prebuilt wheel from PyPI, no Rust needed
cargo install --git https://github.com/KovantAI/fermut --locked    # from source: needs Rust + ~2 min first compile

# in your project (needs pytest, coverage, pytest-cov on the same PATH; ty is embedded — see Installation)
fermut init                     # detect source/tests/runner, write fermut.toml
fermut baseline                 # where you stand: checks the env, builds .coverage,
                                # line coverage vs a sampled mutation score + worst files
fermut run src/ --tests tests/  # full mutation report (add -q to silence progress logs)
fermut next                     # the survivor worth a test first
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

On **typer** (cold run, same machine, 2026-11-03):

| Tool   | Time  | Mutation score |
|--------|-------|----------------|
| fermut | 211s  | 69.2%          |
| mutmut | 289s  | 70.4%          |

fermut is **faster at a similar score** here: per-test coverage selection
and a fork server that collects the suite once (schemata) cut wall-clock
without dropping rigor. The win is suite-shaped: on more-itertools the two
tie (~123s vs ~119s), and on a tiny suite like markupsafe mutmut's simpler
model still wins (~9s vs ~15s). On the larger suites fermut finishes
werkzeug (41k generated mutants, 8k run) in under 3 minutes. Full numbers and methodology:
**[benchmarks](docs/reference/benchmarks.md)**.

## Documentation

**Full documentation lives at <https://fermut.readthedocs.io/>**, sourced
from the [`docs/`](docs/) directory in this repo.

Start here:

- **[Coding agents](docs/guides/coding-agents.md)** — agent-driven inner
  loop, cache strategy, JSON parsing recipes.
- **[Agent skill](docs/guides/claude-code-skill.md#install-the-skill)** —
  `fermut install-skills`, or the Claude Code plugin
  (`/plugin marketplace add KovantAI/fermut`).
- **[First steps](docs/getting-started/first-steps.md)** — `fermut init`
  then `fermut run`, first report in under five minutes.
- **[Installation](docs/getting-started/installation.md)** — prerequisites
  and build paths.
- **[Rolling out to a team](docs/guides/projects.md)** — gradual rollout,
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
