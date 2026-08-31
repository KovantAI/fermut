# fermut

**Coverage tells you which lines your tests *executed*. It can't tell
you whether your tests would *notice* if those lines were wrong.**

A test that calls a function and asserts nothing produces 100% line
coverage and zero behavioral guarantees. Mutation testing closes that
gap: perturb your code in small, deliberate ways, re-run the tests,
and report which perturbations slipped past. Each surviving
perturbation is a named, located, reproducible hole in your test
suite.

fermut is mutation testing built to be fast enough to actually run.
The mutator is native Rust; the type-aware `ty` pre-filter discards
type-invalid mutants in ~25 ms each before they cost a pytest run;
content-hash caching means re-runs only pay for what changed. The
test runner stays the one your team already trusts — `pytest` or
`unittest`.

New to mutation testing? Start with
**[First steps](getting-started/first-steps.md)** for a five-minute
local run, or
**[Mutation testing concepts](concepts/mutation-testing.md)** for the
"why bother" pitch in depth. Hit unfamiliar jargon? The
**[Glossary](concepts/glossary.md)** is a one-page lookup.

## Highlights

- **Fast.** Native-Rust mutator + `ty` pre-filter throws away type-invalid
  mutants before they cost a pytest run.
- **Deterministic.** Pinnable Hypothesis seed, content-hash result cache,
  deterministic sampling. Same commit → same survivors, given a pinned
  seed and a stable test environment.
- **CI-ready.** `--diff-only` (only mutate changed lines), `--coverage`
  (per-test selection), `--shard` (matrix parallelism), [sticky PR
  comments](reference/cli/pr-comment.md) (one comment that updates in
  place on re-runs, not a stack), JUnit/JSON/HTML/Markdown reports.
- **Agent-friendly.** Stable JSON shapes, score-delta trend log, cache
  hits that survive across iterations. See
  **[Coding agents](guides/coding-agents.md)**.
- **Curated profiles.** `pr-gate`, `nightly`, `local`, `library` — one
  flag and you have a sensible config.
- **Tracks itself.** `.fermut/history.jsonl` + `fermut trend` render
  the score history as a sparkline.

## Install

```sh
# Prebuilt wheel from PyPI (no Rust required):
uv tool install fermut

# Or build from source (requires Rust stable):
cargo install --git https://github.com/KovantAI/fermut --locked
```

Full prereqs (Python ≥ 3.10, pytest, coverage, pytest-cov, ty) and
venv layout in **[Installation](getting-started/installation.md)**.

## Why fermut over mutmut or cosmic-ray?

Both are good tools. fermut exists because mutation testing has been
stuck on "interesting but too slow for CI" for a decade. The wins:

- **Mutate in Rust, test in Python.** The hot loop (parse, mutate,
  splice, fork) is native code; the test runner stays the one your
  team already trusts.
- **Type-aware pre-filter.** `ty` discards type-invalid mutants in
  ~25 ms each before they cost a 5-second pytest run. 20–40% of
  naive mutants get dropped here on type-hinted codebases — see
  [benchmarks](reference/benchmarks.md).
- **Content-hash cache.** Re-running on the same commit re-uses
  prior verdicts. PR re-runs cost the diff, not the whole sweep.

For the full comparison, see **[Landscape](concepts/landscape.md)**.

## First run

`fermut init` walks up to the project root, detects the layout, and
writes a `fermut.toml`. **Run it bare** — auto-detection picks a
profile based on project size:

```sh
cd path/to/your/project
fermut init
# build coverage first — fermut uses it to skip irrelevant tests per mutant
fermut coverage
fermut run src/ --tests tests/ --coverage .coverage
```

`fermut coverage` writes a `.coverage` SQLite database and refreshes it
incrementally as your tests change — no `coverage json` export step.
(Or, manually: `pytest --cov=src --cov-context=test && coverage json -o
coverage.json --show-contexts`, then `--coverage coverage.json`.)

Expected output — a line per survivor/timeout, then a one-line summary:

```
SURVIVED  src/auth.py:42 [boundary-shift] `age >= 18` → `age > 18`

152 mutants — killed: 140, survived: 8, timeout: 1, skipped: 3 (coverage 3), equivalent: 0, errored: 0  | score: 94.6%
```

`detected = killed + timeout` (here `141 / 149 = 94.6%`). Skipped and
equivalent mutants don't count either way.
See **[mutation score](concepts/mutation-testing.md#the-mutation-score)**
for the formula.

Exit code is non-zero when any mutant survives — drop the command into
CI as a gate.

Once the bare run feels right, switch to a named profile if your
use case matches one: `pr-gate` (PR CI gate), `nightly` (full
sweep), `local` (dev loop with `--watch`), `library` (reproducible
across contributors). `fermut init --profile <name> --force`
overwrites the auto-detected config. See
**[First steps](getting-started/first-steps.md)** for the full
walkthrough.

## For coding agents

Driving fermut from Claude, Cursor, Aider, or your own scaffolding?
`fermut run --json out.json` emits a stable, versioned JSON contract;
`mutant.id` is the cache key and the selector for `explain`/`suggest`.
See **[Coding agents](guides/coding-agents.md)** for the concrete cycle,
context-budget table, cache strategy, and `explain --format json`
integration.

## Where next

<div class="grid cards" markdown>

-   :material-rocket-launch:{ .lg .middle } **[Getting started](getting-started/installation.md)**

    ---

    Install, first run, feature tour, getting help.

-   :material-book-open-variant:{ .lg .middle } **[Guides](guides/projects.md)**

    ---

    Adopting fermut in a project, CI integrations, trends, coverage, filters.

-   :material-school:{ .lg .middle } **[Concepts](concepts/mutation-testing.md)**

    ---

    What mutation testing measures, configuration model, caching,
    landscape of existing tools.

-   :material-book-search:{ .lg .middle } **[Reference](reference/cli/index.md)**

    ---

    CLI, configuration keys, operators, benchmarks, policies, environment
    variables, shell completion, troubleshooting, internals.

</div>

## Status

fermut is pre-1.0. The CLI, config schema, and JSON report shape may
change between `0.MINOR` releases. See
**[Policies](reference/policies/index.md)** for the full contract.

## License

Licensed under either of [Apache License, Version
2.0](https://github.com/KovantAI/fermut/blob/main/LICENSE-APACHE) or
[MIT license](https://github.com/KovantAI/fermut/blob/main/LICENSE-MIT)
at your option.
