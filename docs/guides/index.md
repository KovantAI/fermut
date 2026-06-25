# Guides

Task-oriented walkthroughs. Each guide focuses on *doing* one thing
well — rolling out, integrating, tuning a filter.

- **[CI quickstart](ci-quickstart.md)** — zero to a green
  mutation-testing job on every PR, in one read.
- **[Integrations](integrations.md)** — GitHub Actions, sharded
  full sweeps, pre-commit, ReadTheDocs.
- **[Working on projects](projects.md)** — gradual rollout
  playbook: measure-only → PR gate → tighten.
- **[Tightening the inner loop](inner-loop.md)** — `--diff-only`,
  `--watch`, `fermut list`, and what to do when the loop is still
  too slow.
- **[Coding agents](coding-agents.md)** — agent-driven inner loop,
  cache strategy, JSON parsing recipes.
- **[Claude Code playbook](claude-code-skill.md)** — battle-tested
  end-to-end workflow with real timing data, pre-flight checklist,
  and op-skip math.
- **[Survivor triage](survivor-triage.md)** — how to read a
  surviving mutant and decide kill / ignore / delete.
- **[Equivalent mutants](equivalent-mutants.md)** — when a survivor
  is unkillable on purpose, how to mark it.
- **[Trends](trends.md)** — recording score history, showing it on
  PRs, persisting in CI.
- **[Coverage](coverage.md)** — per-mutant test selection via
  per-test coverage contexts.
- **[Filters](filters.md)** — `--ops`, `--diff-only`, `--coverage`,
  `--sample`, inline ignore markers.
- **[Migrate from mutmut](migrate-from-mutmut.md)** — config,
  ignore markers, CLI flags, operator names.
- **[Migrate from cosmic-ray](migrate-from-cosmic-ray.md)** — session
  model, distributor, operator families.
