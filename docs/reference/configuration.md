# Configuration reference

fermut reads its persistent config from one of two files, in this order
of preference:

1. **`fermut.toml`** — standalone file. Preferred. Any ancestor of the
   source path.
2. **`pyproject.toml`** with a `[tool.fermut]` table — useful when you
   already maintain a `pyproject.toml` and want to avoid a second file.

Precedence is **CLI args > env vars > config file > built-in defaults**.
Only a few env vars feed the merge today (`FERMUT_ISOLATION` is the
main one — see [Environment variables](environment-variables.md)),
but the layering is consistent.

Each list (`ops`, `skip_ops`) merges independently — a CLI `--ops`
overrides the file's `ops` but leaves the file's `skip_ops` intact,
and vice versa. `pytest_args` does **not** merge: when CLI passes
any `--pytest-arg`, the full CLI list replaces the file's list.
`exclude` follows the same replace rule (CLI `--exclude` replaces
file `exclude` when set).

Paths in the config file resolve **relative to the config file's
directory**, not the cwd. Unknown keys are rejected (typos surface
early).

Generate a starter config with [`fermut init`](cli/init.md).

## Schema

All keys are optional. Defaults match the CLI defaults. Types follow
TOML primitives: `string`, `bool`, `int`, `float`, `array<string>`.

| Key                | Type            | Default                                        | Effect                                                                                          |
|--------------------|-----------------|------------------------------------------------|-------------------------------------------------------------------------------------------------|
| `source_root`      | `string` (path) | cwd or `path` argument                          | Python source root.                                                                              |
| `tests`            | `string` (path) | `<source_root>/tests`                           | Test directory.                                                                                  |
| `jobs`             | `int`           | logical CPU count                               | Parallel workers (one mutant per worker at a time).                                              |
| `timeout`          | `int` (s)       | `30`                                            | Per-mutant test timeout.                                                                          |
| `runner`           | `string`        | `"pytest"`                                      | `"pytest"`, `"rstest"` (pytest-compatible drop-in), or `"unittest"`.                              |
| `python`           | `string` (path) | auto-discover                                   | Interpreter or virtualenv dir to run pytest with (`<python> -m pytest`). CLI `--python`. See [Choosing the interpreter](cli/run.md#python-interpreter). |
| `ty_filter`        | `bool`          | `true`                                          | Enable the ty pre-filter.                                                                         |
| `ruff_filter`      | `bool`          | `false`                                         | Enable the ruff lint pre-filter.                                                                  |
| `experimental`     | `bool`          | `false`                                         | Include experimental operators.                                                                   |
| `parity`           | `bool`          | `false`                                         | Include parity operators — for cross-tool comparison only (very noisy), never normal scoring. CLI `--parity`. See [Parity operators](operators/parity.md). |
| `ops`              | `array<string>` | (all)                                           | Allowlist operator names. See [Operators](operators/index.md).                                          |
| `skip_ops`         | `array<string>` | `[]`                                            | Denylist operator names. Wins over `ops`.                                                          |
| `diff_only`        | `string`        | none                                            | Restrict to lines changed vs base ref. Mutually exclusive with `since`.                            |
| `since`            | `string`        | none                                            | Restrict to lines touched since commit or date. Mutually exclusive with `diff_only`.               |
| `coverage`         | `string` (path) | none                                            | Path to a `coverage.json` with per-test contexts. Enables coverage-based test selection.           |
| `hypothesis_seed`  | `int`           | none                                            | Pin Hypothesis seed across mutants (passes `--hypothesis-seed=<N>` to pytest).                     |
| `pytest_args`      | `array<string>` | `[]`                                            | Extra args forwarded to pytest. Ignored by the unittest runner.                                    |
| `cache`            | `bool`          | `true`                                          | Enable the per-mutant result cache. **Critical for fast iteration.**                              |
| `cache_path`       | `string` (path) | `<source_root>/.fermut/cache.json`              | Custom cache file location.                                                                       |
| `history`          | `bool`          | `true`                                          | Append a summary entry to the history log on every run.                                            |
| `history_path`     | `string` (path) | `<source_root>/.fermut/history.jsonl`           | Custom history log location.                                                                      |
| `sample`           | `float`         | `1.0`                                           | Test only this fraction of mutants (0.0–1.0), deterministic per `sample_seed`.                     |
| `sample_seed`      | `int`           | `0`                                             | Seed for `sample` selection.                                                                       |
| `shard`            | `string`        | none                                            | `"i/n"` — process only the i-th of n disjoint slices. Both 1-based.                                |
| `isolation`        | `string`        | `"auto"`                                        | `"auto" \| "copy" \| "hardlink" \| "reflink"`. See [Isolation modes](cli/run.md#isolation-modes).      |
| `exclude`          | `array<string>` | `[]`                                            | Glob patterns excluding files/directories from mutation collection. Patterns match paths relative to `source_root`. Examples: `"alembic/**"`, `"tests/integration/**"`, `"**/migrations/*.py"`. CLI `--exclude` (when passed at least once) replaces this list. |
| `equiv_detect`     | `bool`          | `true`                                          | Run the equivalent-mutant detector on survivors. When `true`, AST-pattern + CPython-bytecode checks promote provably-equivalent survivors to `equivalent` and drop them from the score denominator. Set `false` (or `--no-equiv-detect`) to see the raw `SURVIVED` set. |
| `cache_scope`      | `string`        | `"file"`                                        | Cache-key granularity. `"file"` hashes the whole file's AST — any structural edit invalidates every mutant in the file. `"scope"` hashes only the enclosing top-level def/class, so sibling-function edits keep cache hits intact. Opt into `"scope"` only when you've accepted that a test for one function may indirectly exercise another (stale verdicts possible). |
| `fail_under`       | `float`         | none                                            | Minimum passing mutation score in percent (`0.0`–`100.0`, inclusive). Run exits non-zero when the final score is below the threshold; a score exactly equal passes. Without it, any survivor exits 1. Invalid range is rejected at config load. CLI `--fail-under` overrides. |
| `verify_baseline`  | `bool`          | `true`                                          | Run the unmutated test suite once before mutating and abort if it isn't green. A failing/erroring suite makes every covered mutant exit non-zero (counted "killed"), inflating the score toward 100%. Set `false` (or `--no-verify-baseline`) to skip — only when you've already confirmed the suite passes (e.g. CI ran it). Costs one full-suite run up front. |
| `baseline_timeout` | `int`           | `300`                                           | Wall-clock cap (seconds) for the baseline run. Separate from `timeout` (which bounds a single mutant's coverage-selected subset) because the baseline runs the whole suite. A suite exceeding it is killed and the run aborts. CLI `--baseline-timeout` overrides. |
| `smart_order`      | `bool`          | `true`                                          | Run the most likely killer first so pytest's `-x` short-circuits sooner: the cold-start breadth prior (most targeted coverage-selected test — fewest of the mutated file's lines) sets the order, then a historically-killing test (per `(file, operator)`, learned into `.fermut/kill-order.json`) is lifted ahead of it. Advisory — only permutes the selected set, so it never moves the mutation score (it can flip `killed`↔`timed_out`, but both count as detected and no `survived` is created or removed), only speed. Stays on under a `timeout`, where killer-first ordering helps most. CLI `--no-smart-order` / `--smart-order` override. See [Smart test ordering](cli/run.md#smart-test-ordering). |
| `kill_order_path`  | `path`          | `.fermut/kill-order.json`                       | Location of the advisory smart-ordering sidecar. Relative paths resolve against the config file's directory. Advisory — losing or relocating it only costs a slow run. |
| `max_time`         | `int`           | unset (whole catalogue)                         | Wall-clock ceiling (seconds) on the testing phase. Mutants are evaluated highest-value first (covered before uncovered); once the deadline passes, untested mutants are recorded as `skipped`/`time-budget` and excluded from the score. Bounds the testing phase only — not baseline, generation, or the `ty` pre-filter. CLI `--max-time` overrides. See [Time-boxed runs](cli/run.md#time-boxed-runs-max-time). |

## Examples

### Standalone `fermut.toml`

```toml
source_root = "src"
tests = "tests"
jobs = 4
timeout = 30
ty_filter = true
experimental = false
ops = ["arith-op-swap", "boundary-shift", "return-value-to-none"]
skip_ops = ["number-shift"]
diff_only = "main"
coverage = "coverage.json"
isolation = "auto"
hypothesis_seed = 12345
fail_under = 80.0
```

### CI gate with score floor

`fail_under` turns the mutation score into a hard CI gate. Combine
with `diff_only` so the gate evaluates the PR's changed lines, not
the whole codebase:

```toml
# fermut.toml — fail the PR build when the mutation score on changed
# lines drops below 75%.
source_root = "src"
tests = "tests"
diff_only = "main"
coverage = "coverage.json"
fail_under = 75.0
```

Score exactly equal to the threshold passes. To gate on regression
against the prior run instead of an absolute floor, use
`--fail-on-regression` on `fermut run`; the two gates compose.

### `pyproject.toml`

```toml
[tool.fermut]
ops = ["arith-op-swap", "compare-op-swap", "boundary-shift"]
diff_only = "main"
coverage = "coverage.json"
```

### Per-profile starter configs

Each `fermut init --profile <name>` writes one of these (header comment
elided):

=== "pr-gate"

    ```toml
    source_root = "src"
    tests = "tests"
    runner = "pytest"
    ty_filter = true
    timeout = 15
    coverage = "coverage.json"
    diff_only = "main"
    hypothesis_seed = 12345
    ops = ["arith-op-swap", "compare-op-swap", "boundary-shift", "return-value-to-none"]
    ```

=== "nightly"

    ```toml
    source_root = "src"
    tests = "tests"
    runner = "pytest"
    ty_filter = true
    timeout = 60
    experimental = true
    ```

=== "local"

    ```toml
    source_root = "src"
    tests = "tests"
    runner = "pytest"
    ty_filter = true
    timeout = 15
    sample = 0.25
    sample_seed = 0
    ```

=== "library"

    ```toml
    source_root = "src"
    tests = "tests"
    runner = "pytest"
    ty_filter = true
    timeout = 30
    experimental = false
    hypothesis_seed = 12345
    ```

## Discovery

`fermut.toml` is searched in every ancestor of the source-root path,
nearest-first. The first match wins. If no `fermut.toml` is found,
`pyproject.toml` files are searched in the same walk for a
`[tool.fermut]` table.

To force a specific config file when both exist, pass `path` to point at
the directory containing the file you want. (A dedicated `--config
<path>` flag is on the roadmap.)
