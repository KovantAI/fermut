# JSON output

Everything fermut writes for programs to read: the run report, and the JSON
from `next`, `explain`, `score`, `baseline` and the history log. Read these
instead of human output: stdout formatting for people can change in any
release. Under the [versioning policy](policies/versioning.md), a patch
release (`0.7.x`) only ever **adds** optional fields; renaming, retyping or
removing a field, or changing the set of `status` values, waits for a
`0.MINOR` release and is called out in the changelog. Ignore fields you
don't know.

Mutant paths are **absolute** in every JSON shape (`id`, `file`), so ids
from one checkout don't match another. Key long-lived state on `file`
relative to your repo, plus `line` and `operator`, not on `id`.

## Run report

Written by `fermut run --json <path>`, `fermut merge --json <path>`, and read
back by `next`, `explain`, `show` and `merge`.

Trimmed from a real run of `examples/sample` (checkout path shortened to
`/repo`):

```json
{
  "summary": { "total": 207, "killed": 58, "survived": 17, "timed_out": 0,
               "skipped": 132, "equivalent": 0, "errored": 0,
               "mutation_score": 77.3, "scored": 75, "survivor_targets": 17 },
  "config_hash": "0386cc5f73a116a3…",
  "duration_ms": 10,
  "outcomes": [
    { "status": "survived",
      "mutant": { "id": "/repo/src/calculator.py@216:boundary-shift:<=-><",
                  "file": "/repo/src/calculator.py", "operator": "boundary-shift",
                  "range": [216, 218], "original": "<=", "replacement": "<",
                  "line": 14, "stmt_line": 14 },
      "duration_ms": 149, "test_ms": 0 },
    { "status": "killed",
      "mutant": { "id": "/repo/src/calculator.py@45:arith-op-swap:+->-", "…": "…" },
      "killer": "tests/test_calculator.py::test_add",
      "duration_ms": 135, "test_ms": 0 },
    { "status": "skipped",
      "mutant": { "id": "/repo/src/calculator.py@11:exp:numeric-type-swap:int->float", "…": "…" },
      "filter": "experimental" }
  ]
}
```

### `summary`

| Field | Meaning |
|---|---|
| `total` | Every generated mutant, skipped ones included. |
| `killed`, `survived`, `timed_out`, `skipped`, `equivalent`, `errored` | Count per `status`. |
| `mutation_score` | `(killed + timed_out) / scored`, percent, one decimal. **Read `scored` first:** when it is `0`, this is a placeholder `100.0`, not a perfect score. |
| `scored` | The score's denominator: `killed + timed_out + survived`. |
| `survivor_targets` | Survivors left once those another survivor at the same site subsumes are folded in: the number of tests to write. Equals `survived` when nothing folds. The score still counts every survivor. |
| `dominator_score` | Present only when a fresh [`fermut subsume`](cli/subsume.md) store covers every scored mutant. |
| `inferred_killed`, `observed_score` | Present only for `--only-dominators` runs: kills inferred rather than run, and the score over observed verdicts alone. |

### Top level

| Field | Meaning |
|---|---|
| `shard` | `{"index": i, "total": n}` for one shard of a `--shard i/n` run; absent otherwise. |
| `config_hash` | Hash of the settings that change verdicts, with `--shard` ignored, so every shard of one run agrees. History compares only runs with the same hash. |
| `duration_ms` | Wall time of the run. After `merge`, the sum across shards. |
| `outcomes` | One entry per generated mutant. |

### Outcomes

Each outcome has a `status` and a `mutant`, plus fields that depend on the
status:

| `status` | Extra fields | Counts toward the score |
|---|---|---|
| `killed` | `killer`: node id of the failing test, when known. `inferred_from`: the dominator id, for an inferred kill. | detected |
| `timed_out` | `hung_in`: node id of the test running when time ran out, when known. | detected |
| `survived` | — | undetected |
| `skipped` | `filter`: why it never ran (below). | no |
| `equivalent` | `reason`, `source`: which detector proved it a no-op. | no |
| `error` | `message`: what went wrong. | no |

`killed`, `timed_out`, `survived` and `error` may also carry `duration_ms`
(wall time of the test run) and `test_ms` (time inside tests, as fermut's
pytest plugin measured it). Both are absent when the verdict came from an
older cache entry or the runner doesn't time runs.

Values of `filter` on a `skipped` outcome, in the order filters run:

| `filter` | Skipped because |
|---|---|
| `experimental` | An experimental operator, without `--experimental`. |
| `migrations` | A migration file (Django `migrations/0001_*.py`, Alembic `versions/*.py`), without `--mutate-migrations`. |
| `parity` | A parity-only operator, without `--parity`. |
| `profile` | The `--operators` profile (`minimal` by default) leaves it out. |
| `operator` | `--ops` / `--skip-ops` excluded the operator. |
| `shard` | Belongs to another shard. |
| `sample` | Not drawn by `--sample`. |
| `diff-only`, `since` | Outside the lines changed (`--diff-only` / `--since`). |
| `coverage` | No test executes its line. |
| `selection` | Only tests outside `--select-tests` (or deselected by pytest) cover it. |
| `ruff` | `--ruff-filter` flagged the mutated code. |
| `ty` | Introduces a new type error. |
| `tce` | Compiles to the same bytecode as the original (`--tce`). |
| `time-budget` | `--max-time` ran out before it was tested. |

### `mutant`

| Field | Meaning |
|---|---|
| `id` | `<file>@<byte offset>:<operator>:<original>-><replacement>`, with `exp:` before the operator for an experimental one and `parity:` for a parity one. Unique within a run. |
| `file` | Absolute path of the source file. |
| `operator` | Operator name in kebab case, without the `exp:` / `parity:` prefix the id carries (e.g. `numeric-type-swap`). See [Operators](operators/index.md). |
| `range` | `[start, end]` byte offsets of the replaced text; `start` is the id's `@offset`. |
| `original`, `replacement` | The replaced text and what replaced it: the operator token, not the whole expression. |
| `line` | 1-based line of `range.start`. |
| `stmt_line` | First line of the enclosing statement; differs from `line` on a continuation line. |
| `site` | Present on compare and `and`/`or` mutants that the [operator profiles](operators/profiles.md) model: `start` (byte offset of the whole compare / boolean expression, shared by the site's mutants), `mask` (bitmask of the input regions where the mutant's truth differs from the original's), `minimal` (`true` when it is in the site's dominator set). |

## `fermut next`

A JSON array, best target first. Each entry is one cluster: a representative
survivor and the other survivors in the same `(file, operator)`.

| Field | Meaning |
|---|---|
| `rank` | 1 = write this test first. |
| `id`, `file`, `line`, `operator`, `original`, `replacement` | The representative survivor, as in the run report. |
| `cluster_size` | Survivors in the cluster, the representative included. |
| `ease` | `high`, `medium` or `low`: how directly a test can observe this operator's change. |
| `min_gain_pts` | Score points gained by killing the representative alone. |
| `max_gain_pts` | Score points gained if one test kills the whole cluster. |
| `hint` | How to kill this operator's mutants. |
| `sibling_ids` | The cluster's other survivors. A test for the representative often kills these too. |
| `subsumed_ids` | Survivors the representative provably subsumes: killing it kills these. |
| `coverage_selected` | `false` when the run had no coverage data, so a survivor may sit on a line no test runs. |

`--max-tokens N` drops lower-ranked entries to fit `N` tokens and says how
many on stderr.

## `fermut explain --format json`

| Field | Meaning |
|---|---|
| `mutant` | `id`, `file`, `line`, `operator`, `original`, `replacement`. |
| `status` | The outcome's status, except that a timed-out mutant reads `timeout` (not `timed_out`). `filter` or `error` is added for a skipped or errored mutant. |
| `source_context` | `start_line`, `mutant_line`, and `lines`: the source around the mutant (`--context N` widens it). |
| `enclosing` | `kind` (`def`, `class`, …) and `name` of the enclosing definition, when there is one. |
| `hint` | Operator-specific advice. |
| `coverage` | `covered`, and `tests`: node ids of the tests that **ran this line without noticing the change**, the first place to strengthen an assertion. `note` explains; `error` is set when the coverage file couldn't be read. Absent without coverage data. |
| `test_matches` | `symbol`, and `matches`: `{file, line}` places in the tests that mention the enclosing function. |
| `skeleton` | `name` and `code` of a pytest function to start from. |
| `llm` | With `--llm` only: `model`, `cached`, `response`, and `extracted_code` when a code block was found. |

## `fermut score`

| Field | Meaning |
|---|---|
| `score`, `killed`, `survived`, `timed_out`, `timestamp` | The latest comparable run. |
| `baseline_score`, `baseline_timestamp` | The run compared against (`--baseline N` runs back); omitted when there's none. |
| `delta` | `score - baseline_score`; omitted without a baseline. |
| `new_survivors` | Mutants that survive now but were killed in the baseline run. |
| `newly_killed` | Mutants that survived in the baseline run and are killed now. |
| `regressed` | `true` when the score dropped or a new survivor appeared: consider reverting the last change. |
| `survivor_ids_compared` | Present only as `false`, when either run predates survivor ids in history; the id lists are then empty and `regressed` reflects the score alone. |

## `fermut baseline --format json`

Documented with the command: [`fermut baseline` → Output](cli/baseline.md#output).

## History log

`.fermut/history.jsonl`: one JSON object per line, one line per recorded
run, read by `trend`, `score` and `dashboard`.

| Field | Meaning |
|---|---|
| `v` | Entry schema version. |
| `timestamp` | When the run finished (RFC 3339). |
| `mutation_score`, `killed`, `survived`, `timed_out`, `skipped`, `errored`, `equivalent`, `total` | As in the run summary. |
| `duration_ms` | Wall time of the run. |
| `config_hash` | As in the run report. Runs are compared only with runs of the same hash and `fermut_version`. |
| `fermut_version`, `git_sha`, `git_branch` | Where the run came from, when known. |
| `survivor_ids` | Ids of the surviving mutants, which `score` diffs between runs. |
| `baseline` | `true` on the entry `fermut baseline` writes: run zero of the trend. |
| `partial` | `true` when `--max-time` ran out before every mutant was tested. Partial runs are never compared. |
