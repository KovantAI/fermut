# `fermut suggest`

Generate a killing pytest test for one or more surviving mutants by
calling the Anthropic Messages API. Reads the same JSON report as
`show` and `explain`, plus optional project context (surrounding tests
for style mimicry), then emits or appends a complete `def test_…():`.

```sh
fermut suggest <report.json> [SELECTOR] [--all-survivors] [--format human|json] \
    [--out PATH] [--apply] [--parallel N] \
    [--tests <dir>] [--context N] [--sample-count K] \
    [--model M] [--no-cache] [--cache-path PATH]
```

| Argument / flag         | Effect                                                                                                                |
|-------------------------|-----------------------------------------------------------------------------------------------------------------------|
| `<report.json>`         | Path to a JSON report produced by `fermut run --json …`.                                                              |
| `[SELECTOR]`            | 1-based outcome index or substring of mutant id. Omit when `--all-survivors` is set.                                  |
| `--all-survivors`       | Generate tests for every survived / timed-out outcome in the report.                                                  |
| `--format human\|json`  | Output format. `human` (default) prints code blocks + progress logs. `json` emits the structured `SuggestReport`.      |
| `--out PATH`            | Append generated test(s) to this path instead of printing to stdout.                                                  |
| `--apply`               | Append to the test file that already references the enclosing symbol. Falls back to `--out` when set.                 |
| `--parallel N`          | Run up to N Anthropic calls concurrently. Default `1`. File writes stay serial; output order is deterministic.        |
| `--tests <dir>`         | Tests directory mined for style samples + `--apply` discovery. Defaults to `tests/`.                                  |
| `--context N`           | Source lines per side of the mutant line included in the prompt. Default `8`.                                         |
| `--sample-count K`      | How many existing test files to include in the prompt for style reference. Default `2`.                               |
| `--model M`             | Anthropic model id. Default `claude-sonnet-4-6` (applied by the handler when omitted on the CLI; clap itself has no default). |
| `--no-cache`            | Disable the LLM response cache at `.fermut/llm-cache.json`.                                                            |
| `--cache-path PATH`     | Override the LLM cache file location.                                                                                  |

## JSON shape

```json
{
  "model": "claude-sonnet-4-6",
  "entries": [
    {
      "mutant":         { "id": "...", "file": "...", "line": 14, "operator": "boundary-shift", "original": "<=", "replacement": "<" },
      "cached":         false,
      "response":       "...raw model text including the python fence...",
      "extracted_code": "def test_in_range_boundary():\n    ...\n",
      "applied_to":     "tests/test_calc.py",
      "error":          null
    }
  ]
}
```

`entries` preserves the order of the surviving mutants in the input
report, even under `--parallel N`. `response` / `extracted_code` are
omitted only when the call failed (`error` populated instead).
`applied_to` is `null` unless `--apply` or `--out` wrote the test to
disk. The JSON shape is the contract for agent consumers — additions are
backwards-compatible; renames are not. See `VERSIONING.md`.

## Authentication

`fermut suggest` reads `ANTHROPIC_API_KEY` from the environment (or
`FERMUT_ANTHROPIC_API_KEY` for scoped setups). For offline / CI runs
without an API key, set `FERMUT_LLM_MOCK=1` and the call short-circuits
to a deterministic stub — useful for snapshot tests of the toolchain.
Recognized truthy values: `1`, `true`, `yes`, `on` (case-insensitive).
`0`, `false`, `no`, `off`, or an empty value disables the mock, so a CI
script can flip the flag off with `FERMUT_LLM_MOCK=0` without unsetting
it.

## Output format

For each target mutant `suggest` prints (or appends) a header comment
followed by a single pytest function:

```python
# fermut suggest: kills mutation `<=` → `<` at src/calc.py:14
def test_in_range_includes_upper_bound():
    from src.calc import in_range
    assert in_range(10, 0, 10) is True
    assert in_range(11, 0, 10) is False
```

If the model response does not contain a `python` code block the
command errors loudly rather than silently dropping the suggestion.

## Caching

Responses are keyed by `(mutant.id, sha256(file), prompt)` and stored
in `.fermut/llm-cache.json`. Re-running `suggest` on the same survivor
with the same source returns the cached test without hitting the API.
Modify any of those inputs and the cache misses naturally — same
contract as the run-result cache.

## Recommended loop

```sh
# 1. Run mutation tests, save JSON report.
fermut run src/ --tests tests/ --json .fermut/last.json --no-history

# 2. Bulk-generate killing tests for every survivor, append to the
#    matching test files in-place.
fermut suggest .fermut/last.json --all-survivors --apply \
    --tests tests/

# 3. Review, tighten the generated assertions, re-run.
fermut run src/ --tests tests/ --json .fermut/last.json
```

`--apply` writes to files in your tree. Always review the diff before
committing — generated tests should compile and pass, but the model is
working from limited context and may invent helpers or import paths
that don't quite line up with your project layout.

## Relation to `explain`

`explain` (the heuristic view) and `explain --llm` (heuristic + LLM
prose) both stop at "here's what you might write." `suggest` is the
generative endpoint — it produces the test file content and, with
`--apply`, integrates it into the suite. Use `explain` to understand,
`suggest` to commit.
