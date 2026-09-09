# `fermut explain`

Diagnose why one mutant survived and propose a killing test. Reads the
same JSON report as `fermut show`, then layers heuristic signal on top
of the bare detail view.

```sh
fermut explain <report.json> <SELECTOR> [--format human|json] [--context N] \
    [--tests <dir>] [--coverage <file>] [--llm] [--model M]
```

| Argument / flag           | Effect                                                                                                          |
|---------------------------|-----------------------------------------------------------------------------------------------------------------|
| `<report.json>`           | Path to a JSON report produced by `fermut run --json …`.                                                        |
| `<SELECTOR>`              | 1-based outcome index, the full mutant id, or any substring of the printed `file:line@offset …` row. Same grammar as `fermut show`; an ambiguous partial selector errors and lists candidates. |
| `--format human\|json`    | Output format. `human` (default) prints the terminal-laid-out view. `json` emits the structured `ExplainReport`. |
| `--context N`             | Source lines to show on each side of the mutant line. Default `5`.                                              |
| `--tests <dir>`           | Tests directory to grep for the enclosing symbol. `--tests` with no value uses `tests/`.                        |
| `--coverage <file>`       | Coverage data with per-test contexts — either coverage.py's native `.coverage` SQLite DB or a `coverage.json` export (the format is sniffed). When present, shows whether any test executed the mutant line. Defaults to auto-discovering `.coverage` at the project root. |
| `--llm`                   | Append an LLM-generated prose explanation + killing test below the heuristic block. Requires `ANTHROPIC_API_KEY`. |
| `--model M`               | Anthropic model id. Default `claude-sonnet-4-6` (applied by the handler when omitted on the CLI; clap itself has no default). |
| `--no-cache`              | Skip the `.fermut/llm-cache.json` lookup and write.                                                              |
| `--cache-path PATH`       | Override the LLM cache location.                                                                                 |

## JSON shape

```json
{
  "mutant":         { "id": "...", "file": "...", "line": 14, "operator": "boundary-shift", "original": "<=", "replacement": "<" },
  "status":         "survived",
  "source_context": { "start_line": 9, "mutant_line": 14, "lines": ["...", "..."] },
  "enclosing":      { "kind": "def", "name": "in_range" },
  "hint":           "boundary shift (...). Tests likely cover non-equal cases but miss the exact boundary value...",
  "coverage":       { "covered": true, "tests": ["tests/test_calc.py::test_existing"], "note": "..." },
  "test_matches":   { "symbol": "in_range", "matches": [{ "file": "tests/test_calc.py", "line": 5 }] },
  "skeleton":       { "name": "test_in_range_boundary", "code": "def test_in_range_boundary():\n    ...\n" },
  "llm":            { "model": "claude-sonnet-4-6", "cached": false, "response": "...", "extracted_code": "def test_x():\n    ..." }
}
```

`coverage`, `test_matches`, and `llm` are present only when their inputs
are. The JSON shape is the contract for agent consumers — additions are
backwards-compatible; renames are not. See `VERSIONING.md`.

The output stacks, top to bottom:

1. The same header `show` prints — file:line, operator, status, id, mutation.
2. Surrounding source with the mutant line marked.
3. Enclosing `def` / `class` discovered by a backward indent scan.
4. An operator-specific hint: why a survivor of this *kind* typically
   slips through the suite.
5. Optional coverage signal: tests that executed the line — they ran
   but didn't *distinguish* the mutation, so their assertions need
   strengthening. Or the opposite: no test ran the line at all.
6. Optional test-grep: which test files reference the enclosing symbol.
   Useful for "where would I add the new case?"
7. A pytest skeleton tailored to the operator and the enclosing symbol.

Everything past step 2 is pure heuristic — no LLM, no network. Treat
it as a strong default starting point, not authoritative.

## Example

```sh
fermut explain .fermut/last.json 14 --tests tests --coverage .coverage
```

```
src/calculator.py:14
operator : boundary-shift
status   : survived
id       : src/calculator.py@216:boundary-shift:<=-><
mutation : `<=` → `<`

source:
      9  def is_positive(x: int) -> bool:
     10      return x > 0
     11
     12
     13  def in_range(x: int, lo: int, hi: int) -> bool:
  ►  14      return lo <= x and x <= hi
     15

enclosing: def in_range
hint     : boundary shift (`>=`↔`>`, `<=`↔`<`). Tests likely cover non-equal cases
           but miss the exact boundary value. Add a test where the input equals the bound.

tests   : `in_range` referenced in:
  - tests/test_calculator.py:5

suggested test skeleton:
```python
def test_in_range_boundary():
    # mutation: `<=` → `<` at src/calculator.py:14
    # Assert a behavior that differs under the mutation above.
    ...
```
```

## Relation to `show`

`show` is the metadata + diff view. `explain` is the "what do I write
next" view. Use `show` when you want the raw mutation, `explain` when
you want a starting point for the kill.
