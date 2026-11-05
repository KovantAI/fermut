# Guides

Task-oriented walkthroughs. Each guide focuses on *doing* one thing
well.

## Local use

- **[Coverage](coverage.md)** — per-mutant test selection via
  per-test coverage contexts.
- **[Tightening the inner loop](inner-loop.md)** — `--diff-only`,
  `--watch`, `fermut list`, and what to do when the loop is still
  too slow.
- **[Survivor triage](survivor-triage.md)** — how to read a
  surviving mutant and decide kill / ignore / delete.
- **[Equivalent mutants](equivalent-mutants.md)** — when a survivor
  is unkillable on purpose, how to mark it.
- **[Filters](filters.md)** — `--ops`, `--diff-only`, `--coverage`,
  `--sample`, inline ignore markers.
- **[pytest plugin](pytest-plugin.md)** — `pytest --fermut`: mutation
  testing after a green suite, survivors in pytest's summary.
- **[Jupyter notebooks](notebooks.md)** — mutation-testing code that
  lives in `.ipynb` files.

## CI and teams

- **[CI quickstart](ci-quickstart.md)** — zero to a mutation-testing
  job on every PR, in one read.
- **[GitHub Action](github-action.md)** — `uses: KovantAI/fermut@<tag>`:
  PR gate, sharded sweeps, score trend and cache warming.
- **[CI recipes](integrations.md)** — hand-written workflows: caching,
  PR gate + nightly, history persistence, pre-commit, other CI systems.
- **[Coverage in CI](ci-coverage.md)** — cache `.coverage` across runs,
  scope it to the diff, or build it once on `main` for PRs to reuse.
- **[Rolling out to a team](projects.md)** — baseline → measure-only →
  gate → tighten.
- **[Trends](trends.md)** — recording score history, showing it on
  PRs, persisting in CI.

## Coding agents

- **[Coding agents](coding-agents.md)** — the agent inner loop, cache
  strategy, JSON parsing, MCP server.
- **[Agent skill & plugin](claude-code-skill.md)** — install the
  `fermut-mutation-testing` skill or the Claude Code plugin; realistic
  targets and timings from real sessions.

## Migrating

- **[From mutmut](migrate-from-mutmut.md)** — config, ignore markers,
  CLI flags, operator names.
- **[From cosmic-ray](migrate-from-cosmic-ray.md)** — session model,
  distributor, operator families.
