# `fermut baseline`

Day-one "where do I stand" command. Sanity-checks the environment, builds
coverage, runs a fast sampled mutation pass over **covered** code, and prints
a graded verdict: line coverage, mutation score, and the gap between them.
Run it first; then [`fermut next`](next.md) to act on it.

```sh
fermut baseline [PATH] [--full] [--sample <RATIO>] [--top <N>] [--format human|json]
```

| Flag               | Default | Effect                                                                                 |
|--------------------|---------|----------------------------------------------------------------------------------------|
| `PATH`             | `.`     | Where to start the project-root walk.                                                  |
| `--full`           | off     | Mutate every covered mutant for the exact score instead of a sampled estimate. Slower. |
| `--sample <RATIO>` | `0.1`   | Sampling fraction (0.0–1.0) for the fast pass. Ignored with `--full`.                  |
| `--top <N>`        | `3`     | How many worst-offender files to list.                                                 |
| `--format`         | `human` | `json` emits the same numbers for an agent or dashboard.                               |

The filter flags shared with [`run`](run.md) (`--ops`, `--skip-ops`,
`--exclude`, `--diff-only`, …) also apply. The coverage filter is always on:
mutating code no test executes would mix two separate problems into one
number.

## Pipeline

1. **[`doctor`](doctor.md)** checks: if any fails, `baseline` stops with
   `environment not ready` rather than print a confidently wrong score.
2. **[`coverage`](coverage.md)** builds or refreshes `.coverage` and reads
   line coverage from it.
3. A sampled mutation pass (fixed seed, so two baselines on the same tree
   agree) over covered code.
4. One entry appended to the history log, flagged `baseline: true`, so
   [`trend`](trend.md) and [`dashboard`](dashboard.md) can mark it as run zero.

## Output

```text
fermut baseline

  line coverage     92%
  mutation score    61%  (on covered code, 10% sample)
  ───────────────────────────────
  test-quality gap  39 pts   ← covered code whose behavior no test checks
  untested risk     8%     ← lines no test executes at all

  grade: Ok — real gaps in covered code

  worst files (by survivors):
     14  src/pkg/pricing.py
      9  src/pkg/orders.py
      4  src/pkg/util.py

  note: sampled estimate — run `fermut baseline --full` for the exact score

  → write the highest-value test next:  fermut next
```

| Field (JSON key)     | Meaning                                                                                           |
|----------------------|---------------------------------------------------------------------------------------------------|
| `line_coverage`      | Line coverage percent. Omitted when coverage.py's report couldn't be read.                        |
| `mutation_score`     | Mutation score on covered code. Read `scored` first: `0` means this is a vacuous floor, not 100%. |
| `scored`             | Mutants with a verdict (killed + timed out + survived), the score's denominator.                  |
| `quality_gap`        | `100 - mutation_score`: covered behavior no test would notice changing.                           |
| `untested_risk`      | `100 - line_coverage`: lines no test executes at all. Omitted without a line-coverage reading.    |
| `grade`              | `strong` (≥ 80), `ok` (≥ 60), `weak` (≥ 40), `smoke` (< 40), or `na` when nothing was scored.     |
| `killed`, `survived` | Mutant counts from the pass.                                                                      |
| `sampled`, `sample_fraction` | Whether the score is a sampled estimate, and the fraction tested.                         |
| `worst_files`        | `[{file, survivors}]`, worst first: where to spend test-writing effort.                           |

The grade is on the mutation score only. Line coverage can't earn one: a
fully covered suite of assertion-free tests still scores near zero.

Also exposed over MCP as `fermut_baseline` ([`fermut mcp`](mcp.md)).
