# `fermut list`

Enumerate mutations without running tests. Useful for tuning operator
filters before paying for full runs.

```sh
fermut list [PATH] [filter flags...]
```

Accepts the full `FilterArgs` set from [`fermut run`](run.md), plus the
`--no-ty-filter` / `--ruff-filter` pre-filter toggles:

| Flag                 | Effect                                                                          |
|----------------------|---------------------------------------------------------------------------------|
| `--ops <list>`       | Allowlist operators (comma-separated kebab-case names).                         |
| `--skip-ops <list>`  | Denylist operators. Wins over `--ops`.                                          |
| `--experimental`     | Include the experimental operator set.                                          |
| `--operators <profile>` | Compare / `and`-`or` operator set (`default`, `minimal`, `full`). See [Operator profiles](../operators/profiles.md). |
| `--no-ty-filter`     | Skip the ty pre-filter — show every generated mutant, including type-invalid ones ty would drop. |
| `--ruff-filter`      | Enable the ruff lint pre-filter (requires `ruff` on PATH).                       |
| `--diff-only [base]` | Restrict to lines changed vs `base` (default `main`).                            |
| `--since <SPEC>`     | Restrict to lines touched since a commit or date.                                |
| `--no-diff-only`     | Override `diff_only`/`since` from config — full sweep.                          |
| `--coverage [path]`  | Filter mutants down to lines covered by per-test contexts.                       |
| `--no-coverage`      | Override `coverage` from config — disable coverage filtering.                    |
| `--exclude <GLOB>`   | Repeatable glob (relative to source root) pruning files before collection.       |

Output is one mutant id per line, followed by a `N mutant(s) total`
summary. No pytest, no cache, no history.

**`list` applies the same pre-test filters `run` would** — the ty
type-check, coverage, and diff-scope filters all run — so by default
the list reflects exactly what `run` would test, not the raw generated
set. This matters for the experimental type-annotation operators: a
mutant like `x: int = 1.0` is generated but dropped by ty as
type-invalid, so it won't appear unless you pass `--no-ty-filter`. Use
`--no-ty-filter` to inspect the full generated catalogue; omit it to
preview the actual run scope. Pair with `wc -l` for a quick count.
