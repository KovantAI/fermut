# Trends

How to see whether your mutation score is moving in the right
direction over time, and how to surface that on PRs.

## What gets recorded

Every `fermut run` appends one line to `.fermut/history.jsonl`
(JSON Lines — one self-contained JSON object per line). Each line
is one `HistoryEntry`:

| Field            | Type           | What it is                                                                                          |
|------------------|----------------|-----------------------------------------------------------------------------------------------------|
| `v`              | `int`          | Schema version. Currently `1`; older entries deserialize as `0`.                                    |
| `timestamp`      | `string`       | ISO-8601 UTC wall-clock at run completion.                                                          |
| `mutation_score` | `float`        | Percent. Same definition as the report's score (killed and timed-out count as killed).              |
| `killed`         | `int`          | Mutants the suite caught.                                                                           |
| `survived`       | `int`          | Mutants that passed unchanged behavior — what you want fewer of.                                    |
| `timed_out`      | `int`          | Mutants whose run exceeded `--timeout`. Counted as killed for scoring.                              |
| `skipped`        | `int`          | Mutants rejected by a pre-filter (ty / ruff / coverage / diff / sample / ops / inline ignore). Not counted toward the score. |
| `errored`        | `int`          | Mutants whose worker raised before producing a verdict (test runner crash, etc.).                   |
| `equivalent`     | `int`          | Survivors the [equivalent-mutant detector](equivalent-mutants.md#auto-detection-the-equivalent-mutant-detector) reclassified as provably equivalent. Excluded from the score denominator. |
| `total`          | `int` \| null  | Total mutants in this run. `null` on entries written before this field existed.                     |
| `duration_ms`    | `int` \| null  | Wall-clock run time in milliseconds. `null` on older entries.                                       |
| `config_hash`    | `string` \| null | Hex digest of the run-shape config (runner, timeout, hypothesis seed, pytest args, coverage on/off, operator allow/deny, experimental, parity, diff scope `--since`/`--diff-only`, `--sample`, `--exclude`, `--shard`, ruff/ty filters, equivalent-mutant detection, and the test-suite / coverage-file selection). Equal hashes → runs share the same shape *and* mutant universe, so scores are directly comparable; a mismatch means the comparison crosses configurations or scopes, not just code. |
| `git_sha`        | `string` \| null | Short git sha, if the working tree is a git repo.                                                 |
| `git_branch`     | `string` \| null | Current branch name, if discoverable.                                                              |
| `survivor_ids`   | `array<string>` \| null | IDs of mutants that survived this run. Stored inline so `fermut trend --diff` can compute new-survivor / newly-killed diffs without a sidecar file. |

The schema is additive: new fields are tagged `#[serde(default)]`,
so older entries keep parsing and an older `fermut` reading a newer
log skips entries it doesn't understand instead of misinterpreting
them. Authoritative shape lives in `src/history.rs::HistoryEntry`.

Disable with `--no-history` or `history = false` in `fermut.toml`.
`fermut clean` **preserves** the history file even when it evicts
the cache.

## Showing the trend on the CLI

```sh
fermut trend
```

```
history: /work/project/.fermut/history.jsonl (5 entries)

score: ▅▆▇▇▇   62.5% → 92.0%  (+29.5 pts)

  timestamp              score  killed  survived  timeout    delta git
  2026-05-01T10:00:00    62.5%      10        6        0         — main@aaa1111
  2026-05-03T10:00:00    71.4%      15        6        0      +8.9 main@bbb2222
  2026-05-10T10:00:00    80.0%      20        5        0      +8.6 main@ccc3333
  2026-05-20T10:00:00    85.7%      24        4        0      +5.7 main@ddd4444
  2026-06-01T10:00:00    92.0%      46        4        0      +6.3 features/coverage@eee5555
```

Flags:

| Flag                 | Effect                                                    |
|----------------------|-----------------------------------------------------------|
| `--limit N`          | Show only the last N entries (default 10).                 |
| `--all`              | Show every recorded entry.                                 |
| `--history-path <p>` | Read from a non-default location.                          |
| `--format json`      | Emit JSON instead of the table (for tooling).              |

## Showing the trend on a PR

Pass `--trend` to `fermut run`. It prepends a compact trend block to
the Markdown report:

```sh
fermut run src/ --tests tests/ --markdown report.md --trend
fermut pr-comment --markdown report.md
```

The PR comment is sticky — `fermut pr-comment` updates the same
comment across re-runs instead of stacking new ones. See
[`fermut pr-comment`](../reference/cli/pr-comment.md).

## CI history persistence

GitHub Actions runners are ephemeral, so `.fermut/history.jsonl`
resets every run unless you persist it. The simplest pattern:
`actions/cache` keyed on the branch ref, with a `main`-branch
save-only step.

```yaml
- name: restore fermut history
  uses: actions/cache@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
  with:
    path: .fermut/history.jsonl
    key: fermut-history-${{ github.ref_name }}-${{ github.sha }}
    restore-keys: |
      fermut-history-${{ github.ref_name }}-
      fermut-history-main-

- run: fermut run src/ --tests tests/ --markdown report.md --trend

- name: save fermut history
  if: always() && github.ref == 'refs/heads/main'
  uses: actions/cache/save@27d5ce7f107fe9357f9df03efb73ab90386fccae  # v5.0.5
  with:
    path: .fermut/history.jsonl
    key: fermut-history-main-${{ github.sha }}
```

Only `main` writes the cache — PR jobs are read-only so feature
branches don't fork their own history streams. Tune to taste.

## Using deltas as a signal

The useful comparison is usually the **delta vs the previous run**,
not the absolute score:

```sh
fermut trend --limit 2 --format json | \
  jq '.[1].mutation_score - .[0].mutation_score'
```

This is what coding agents consume as a reward signal. See
**[Coding agents → trend tracking](coding-agents.md#trend-tracking-is-the-agents-feedback-signal)**.

## Pitfalls

- **Don't fiddle with `history.jsonl` between runs.** It's the
  agent's reward signal; manual edits make the trend lie.
- **Pin [`hypothesis_seed`](../concepts/glossary.md#hypothesis-seed)**
  in `fermut.toml`. Without it, the score can wobble run-to-run from
  Hypothesis randomness alone, and the trend becomes noise. Ignore
  this if your test suite doesn't use the `hypothesis` library.
- **Don't chase a fixed target.** A moving trendline is more useful
  than a fixed `score ≥ N` gate — most teams plateau at different
  numbers depending on domain.

## Where next

- **[`fermut trend` CLI](../reference/cli/trend.md)** — every
  flag.
- **[Working on projects](projects.md)** — when to enable history
  vs. when to leave it off.
