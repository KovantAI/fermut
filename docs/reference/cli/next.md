# `fermut next`

Rank surviving mutants by which one to fix next.

```sh
fermut next <REPORT> [flags...]
```

Reads the JSON report from `fermut run --json`, groups survivors into
`(file, operator)` clusters, and ranks them by expected reward per test.
Built for an agent picking its next target, so JSON is the default format.

| Flag                   | Default | Effect                                                          |
|------------------------|---------|-----------------------------------------------------------------|
| `REPORT`               | —       | Path to a JSON report produced by `fermut run --json …`.         |
| `--limit <N>`          | `1`     | Number of ranked survivors to emit. `1` is the single best target. |
| `--all`                | off     | Emit every ranked survivor instead of `--limit`.                 |
| `--max-tokens <N>`     | off     | Cap output at an estimated token budget. Emits the highest-ranked survivors that fit; drops the rest (count logged to stderr). Overrides `--limit`. |
| `--format json\|human` | `json`  | `json` emits the ranked list; `human` prints a readable summary. |

## How ranking works

First, survivors are **folded**. At a compare (`a < b`) or an `and`/`or`
condition, one surviving mutant can make another redundant: a test that
kills `a < b` → `a > b` also kills `a < b` → `a >= b`, because `>=` differs
from `<` on every input where `>` does, and more. The redundant survivor is
not shown as its own target; it is listed in the dominator's
`subsumed_ids`. This follows from the comparison itself, not from a guess,
but assumes ordinary ordered values (see the caveats in
[Operator profiles](../operators/profiles.md#caveats)).

Then the remaining survivors are grouped into clusters — same `(file, operator)` **and** within
2 lines of a neighbour — then ranked descending by:

1. **Cluster size** — survivors at the same code site (same file, same
   operator, adjacent lines) usually fall to one well-aimed test, so the
   biggest cluster is the highest-leverage guess. The line-proximity gate is
   load-bearing: on the parity corpus, 80% of same-`(file, operator)` groups
   span >20 lines — unrelated functions one test can't co-kill — so grouping
   by file alone would overstate leverage. Gain is still a
   `min_gain_pts`–`max_gain_pts` range; the representative carries its
   `sibling_ids` so you can check the guess.
2. **Kill-ease** — concrete-value operators (boundary, compare, constant,
   numeric, string) are `high`; reference/structure ones (return-to-None,
   arg drops, slices) are `medium`; behavioral ones (decorator removal,
   exception swaps, loop-iteration) are `low`. Breaks ties between
   equal-size clusters.

Ties beyond that break on `(file, line, id)`, so the same report always
produces the same ranking. The representative of each cluster is the
lowest-byte-offset survivor.

**Equivalent mutants never appear** — the detector emits them as a separate
`equivalent` status upstream, so the survivor list `next` reads is already
clean. **Timeouts are excluded**: an assertion doesn't kill them; they need
a faster test or a higher `--timeout`.

**`ease` assumes the survivor's line is executed by the suite** — i.e. a
killing test only needs a stronger assertion. That holds when the run used
coverage selection, because uncovered mutants are then `Skipped` (filter
`coverage`), never `survived`. If the report shows no coverage selection, the
`human` format prints a one-line caveat and the `json` format sets
`coverage_selected: false` on every entry: a "high ease" survivor may sit on
an unexecuted line and need a brand-new test, not just an assertion.

## JSON shape

```json
[
  {
    "rank": 1,
    "id": "src/a.py@40:boundary-shift:>=->>",
    "file": "src/a.py",
    "line": 12,
    "operator": "boundary-shift",
    "original": ">=",
    "replacement": ">",
    "cluster_size": 2,
    "ease": "high",
    "min_gain_pts": 25.0,
    "max_gain_pts": 50.0,
    "hint": "boundary shift (`>=`↔`>` …). Add a test where the input equals the bound.",
    "sibling_ids": ["src/a.py@60:boundary-shift:<=-><"],
    "subsumed_ids": [],
    "coverage_selected": true
  }
]
```

- **`cluster_size`** — survivors sharing this `(file, operator)`,
  representative included.
- **`min_gain_pts`** — points the score climbs from killing the representative
  alone, plus the survivors it subsumes (`(1 + subsumed) / score_denominator`).
  The floor: one test, guaranteed kills.
- **`max_gain_pts`** — points if one test kills *every* mutant in the cluster,
  including each member's subsumed survivors. The ceiling, realised only when
  the co-kill guess holds; equals `min_gain_pts` for a lone survivor.
- **`hint`** — the operator-specific tip, shared with
  **[`fermut explain`](explain.md)**.
- **`sibling_ids`** — other survivors in the cluster; a test for the
  representative often kills these too (a guess).
- **`subsumed_ids`** — survivors at the representative's compare / `and`-`or`
  site that a test killing the representative kills too (derived, not
  guessed). Empty for most operators.
- **`coverage_selected`** — whether the run used coverage selection. When
  `false`, a high-`ease` survivor may sit on an unexecuted line and need a
  brand-new test, not just a stronger assertion (same caveat the `human`
  format prints). Identical across entries.

## Token budget

`--max-tokens N` bounds the output to a context-window budget. `next`
ranks every cluster, then emits the highest-ranked prefix whose estimated
token count fits `N`, dropping the rest. Because the order is value-ranked,
the survivors that survive the cut are the ones most worth an agent's
tokens.

```sh
fermut next .fermut/last.json --max-tokens 2000
```

- The estimate is `~chars/4` over each entry's pretty-printed JSON (the form
  actually emitted) — the standard rough token heuristic. Treat it as
  approximate, not exact.
- The single top-ranked entry is **always** included, even if it alone
  exceeds the budget. A budget should never starve the agent of its best
  target.
- What got dropped is **never silent**: a line goes to stderr
  (`fit 8 of 45 ranked survivors in ~1980 tokens (37 omitted …)`) while
  stdout stays a clean JSON array. Loop — kill the emitted clusters,
  re-run, ask `next` again — or raise `--max-tokens`.

`--max-tokens` overrides `--limit` and conflicts with `--all`.

## In the agent loop

`next` answers "what do I fix now?" so the agent doesn't hand-roll
`jq 'sort_by(.mutant.operator)'`. Pair it with
**[`fermut explain`](explain.md)** for the killing-test skeleton and
**[`fermut score`](score.md)** for the post-iteration reward. See the
**[coding agents guide](../../guides/coding-agents.md)**.
