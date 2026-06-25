# fermut

Agent-first mutation testing for Python — built on
[`ruff`](https://github.com/astral-sh/ruff) parsing and
[`ty`](https://github.com/astral-sh/ty) type-awareness.

**New to mutation testing?** Line and branch coverage tells you which
lines your tests *execute*; mutation testing tells you which lines your
tests *check*. fermut makes small changes to your source (a `<=` becomes
`<`, a `+` becomes `-`), re-runs your tests, and reports which changes
*survived* — those are the gaps your tests don't actually catch. In
short: it answers "are my tests any good?"

fermut is designed for coding agents in the loop: machine-readable JSON
reports, deterministic mutant IDs, cache-friendly incremental runs, and
exit codes that gate CI. Under the hood, it generates source-level
mutants via [Astral's `ruff_python_parser`](https://github.com/astral-sh/ruff),
drops type-invalid candidates with `ty` before they reach your test
suite, and runs the survivors in parallel against `pytest` or
`unittest`. Hot loop is native Rust; test runner stays Python. Humans
welcome too.

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

## Quickstart

```sh
# install — pick the path that matches you
uv tool install fermut                                             # prebuilt wheel from PyPI, no Rust needed
cargo install --git https://github.com/KovantAI/fermut --locked    # from source: needs Rust + ~2 min first compile

# in your project (needs pytest, coverage, pytest-cov, ty on the same PATH — see Installation)
fermut init                     # detect source/tests/runner, write fermut.toml

# fermut init wires per-test coverage when pytest-cov is present;
# generate it before the first run (skip if your config has no `coverage` key):
pytest --cov=src --cov-context=test
coverage json -o coverage.json --show-contexts

fermut run src/ --tests tests/  # first mutation report (add -q to silence progress logs)
```

For a CI gate, generate the opinionated profile instead — diff-only +
coverage + a narrow operator set, tuned to fail fast on pull requests:

```sh
fermut init --profile pr-gate   # writes a CI-shaped fermut.toml (needs git + coverage.json)
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

Exit code is non-zero when any mutant survives — wire that into CI to
gate on mutation score.

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
