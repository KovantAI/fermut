# Features

A short tour of what fermut can do. Each item links to the dedicated
guide or reference page.

> **Already installed and want to run something?** Skip to
> **[First steps](first-steps.md)**. This page is a feature catalogue,
> not a tutorial.

## Mutation testing core

- **26 stable operators + 4 experimental.** Arithmetic / compare /
  boundary / boolean / constant / return-to-none / slice / sentinel /
  decorator / and more. See the **[Operators reference](../reference/operators/index.md)**.
- **Inline ignore markers.** `# fermut: ignore[op-name]` on a line
  drops one or all operators from that line. Useful for
  known-equivalent mutants.
- **Docstring auto-skip.** Module/class/function docstrings are never
  mutated.

## Speed

- **Native-Rust mutator.** Mutation generation runs on
  [`ruff_python_parser`](https://github.com/astral-sh/ruff), not a
  Python AST walker.
- **`ty` pre-filter.** Type-invalid mutants are dropped via
  [`ty`](https://github.com/astral-sh/ty) before they cost a pytest
  invocation. Typically removes 20–40% of naive mutants on type-hinted
  codebases.
- **Per-mutant result cache.** Keyed on `(mutant.id, ast_hash(file), scope)`.
  AST-structural hashing means reformat / comment edits don't
  invalidate. `scope` covers the run-shape inputs that change the
  outcome without changing the AST: runner kind, timeout, hypothesis
  seed, pytest args, and the per-mutant test selection set when
  coverage-driven selection is on. Edits to one file only re-run
  that file's mutants. See **[Caching](../concepts/caching.md)**.
- **`--shard i/n`.** Zero-coordination CI matrix parallelism.
- **`--watch`.** Re-runs on every `.py` change.

## CI integration

- **`--diff-only`.** Restrict to lines changed vs. a base ref. Right
  default for PR-time gates.
- **`--coverage`.** Per-mutant test selection from
  `coverage.json` per-test contexts. Big wall-clock win.
- **Sticky PR comments.** `fermut pr-comment` posts a Markdown report
  and updates the **same comment** across re-runs instead of stacking.
- **JUnit / JSON / HTML / Markdown reports.** Stable JSON shapes, GHA
  annotations, self-contained HTML.
- **Curated profiles.** `pr-gate`, `nightly`, `local`, `library`.
  See **[Working on projects](../guides/projects.md)**.

## Migrating from another tool

- **`fermut migrate mutmut`** — translates `[tool.mutmut]` /
  `setup.cfg`, rewrites `# pragma: no mutate` → `# fermut: ignore`,
  prints "manual review" for unmapped keys. See
  **[Migrate from mutmut](../guides/migrate-from-mutmut.md)**.
- **`fermut migrate cosmic-ray`** — translates `cosmic-ray.toml`,
  flags celery / interceptors / `excluded-modules` for manual review.
  See **[Migrate from cosmic-ray](../guides/migrate-from-cosmic-ray.md)**.

## Determinism

- **Pinnable Hypothesis seed.** Same commit → same survivors, given a
  pinned seed and a stable test environment.
- **Deterministic `--sample`.** Reproducible across runs.
- **Content-hash cache keys.** No silent cache poisoning when files
  change.

## Observability

- **Run history.** Every run appends a line to
  `.fermut/history.jsonl`. See **[Trends](../guides/trends.md)**.
- **`fermut trend`.** Sparkline + delta table of mutation score over
  time.
- **`fermut score`.** One-shot agent reward signal — latest score, delta
  vs a baseline run, and the new-survivor / newly-killed diff as JSON.
- **`fermut next`.** Ranks survivors by cluster leverage + kill-ease and
  names the single best mutant to write a killing test for next. `--max-tokens`
  fits the ranked list into an agent's context-window budget.
- **`fermut autofix`.** Generates a killing test, verifies it kills the
  mutant *and* keeps the suite green, and keeps only proven tests — reverting
  the rest. Mutation score from diagnostic to auto-remediation.
- **`fermut doctor`.** Environment check; one-line remediation per
  failure.

## Agent-friendly

fermut is designed to run inside an AI coding agent's inner loop —
stable JSON, cache-friendly, deterministic, score-delta as reward
signal. See **[Coding agents](../guides/coding-agents.md)**.

- **`fermut mcp`.** Native [Model Context Protocol](https://modelcontextprotocol.io)
  server over stdio — agents call `fermut_doctor`, `fermut_run`,
  `fermut_next`, `fermut_explain`, `fermut_score`, `fermut_list_survivors`
  as tools instead of shelling out. See
  **[Integrations → MCP](../guides/integrations.md#mcp-server)**.

## Reference quick links

- **[CLI](../reference/cli/index.md)** — every subcommand and flag.
- **[Configuration](../reference/configuration.md)** — every TOML key.
- **[Environment variables](../reference/environment-variables.md)**.
- **[Benchmarks](../reference/benchmarks.md)**.
