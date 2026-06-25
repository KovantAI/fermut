# `fermut autofix`

Generate a killing test for each survivor, then **verify it before keeping
it**. Where [`suggest`](suggest.md) writes a test and trusts it, `autofix`
closes the loop — only proven tests land.

```sh
fermut autofix <REPORT> [TARGET] [flags...]
```

For each surviving mutant, autofix:

1. **Generates** a test (same Anthropic prompt as `suggest`).
2. **Appends** it to the discovered test file (the one referencing the
   enclosing symbol), or `--out`.
3. **Verifies** against the real runner:
   - the unmutated suite (with the new test) still passes — a test that
     fails on correct code is wrong;
   - re-applying the mutation makes the suite fail — proof the test catches
     the bug.
4. **Keeps** the test only if both checks pass; otherwise **reverts** it, so
   a bad suggestion never lands.

| Flag                  | Default              | Effect                                                          |
|-----------------------|----------------------|-----------------------------------------------------------------|
| `REPORT`              | —                    | JSON report from `fermut run --json`.                            |
| `TARGET`              | —                    | Mutant selector: 1-based index or id substring. Omit with `--all-survivors`. |
| `--all-survivors`     | off                  | Fix every survivor / timeout in the report.                      |
| `--path <dir>`        | `.`                  | Source root for the runner + baseline.                           |
| `--tests <dir>`       | configured           | Tests dir: mined for the apply target and mirrored by the verifier. |
| `--python <PATH>`     | auto-discover        | Interpreter or virtualenv the verifier runs pytest with (`<python> -m pytest`). Same discovery as [`fermut run`](run.md#python-interpreter). |
| `--out <file>`        | inferred             | Force generated tests into this file. **Must live inside the tests tree** or the verifier won't see it. |
| `--model <id>`        | `claude-sonnet-4-6`  | Anthropic model.                                                 |
| `--context <N>`       | `8`                  | Source lines of context in the prompt.                           |
| `--sample-count <N>`  | `2`                  | Existing tests included in the prompt for style.                 |
| `--timeout <SECS>`    | config / 30          | Per-mutant verification timeout.                                 |
| `--no-cache`          | off                  | Disable the LLM response cache.                                  |
| `--cache-path <p>`    | `.fermut/llm-cache.json` | Custom LLM cache path.                                       |
| `--keep-failed`       | off                  | Keep generated tests even when verification fails (default reverts). |
| `--format json\|human`| `json`               | `json` is the structured report; `human` is a per-mutant summary. |

Requires `ANTHROPIC_API_KEY` (or `FERMUT_LLM_MOCK=1` for offline tests) and a
working test runner (`fermut doctor` to check).

## JSON shape

```json
{
  "model": "claude-sonnet-4-6",
  "fixed": 1,
  "failed": 1,
  "entries": [
    {
      "mutant": { "id": "...", "file": "src/auth.py", "line": 42, "operator": "boundary-shift", "original": ">=", "replacement": ">" },
      "outcome": "fixed",
      "kept": true,
      "applied_to": "tests/test_auth.py"
    },
    {
      "mutant": { "...": "..." },
      "outcome": "still-survives",
      "kept": false,
      "applied_to": "tests/test_auth.py",
      "detail": "generated test does not catch the mutation"
    }
  ]
}
```

`outcome` is one of:

- **`fixed`** — suite green and the mutant now dies. Test kept.
- **`suite-red`** — the generated test fails on unmutated code. Reverted.
- **`still-survives`** — suite green but the test doesn't catch the mutation. Reverted.
- **`generation-failed`** — the model produced no usable test. Nothing written.
- **`error`** — couldn't find an apply target or the verification errored. Reverted.

`kept` reflects what's on disk: `true` for `fixed`, for `--keep-failed`, or
if a revert itself failed (the `detail` says so).

## Cost & caveats

- Each survivor costs **one model call + a baseline suite run + one mutant
  run**. `--all-survivors` over N survivors is N of each — budget
  accordingly, and prefer it after [`fermut next`](next.md) has narrowed the
  list to the highest-value targets.
- Always review the kept tests before committing — verified means "kills the
  mutant and keeps the suite green," not "idiomatic." The model can still
  reach for helpers that don't exist or assert more narrowly than you'd want.
- `autofix` is a CLI / CI tool — it calls Anthropic through fermut's own
  client. Inside an LLM-driven agent, prefer [`fermut explain`](explain.md)
  (write the test yourself, verify with `fermut run`); that's why autofix is
  **not** an MCP tool. The full rationale — fermut-writes-the-test vs.
  agent-writes-the-test — is in
  **[`fermut mcp` → why no generation tools](mcp.md#no-generation-tools)**.
  See also **[Coding agents](../../guides/coding-agents.md)**.
