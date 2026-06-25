# `fermut list`

Enumerate mutations without running tests. Useful for tuning operator
filters before paying for full runs.

```sh
fermut list [PATH] [filter flags...]
```

Accepts the full `FilterArgs` set from [`fermut run`](run.md):

| Flag                 | Effect                                                                          |
|----------------------|---------------------------------------------------------------------------------|
| `--ops <list>`       | Allowlist operators (comma-separated kebab-case names).                         |
| `--skip-ops <list>`  | Denylist operators. Wins over `--ops`.                                          |
| `--experimental`     | Include the experimental operator set.                                          |
| `--diff-only [base]` | Restrict to lines changed vs `base` (default `main`).                            |
| `--since <SPEC>`     | Restrict to lines touched since a commit or date.                                |
| `--no-diff-only`     | Override `diff_only`/`since` from config — full sweep.                          |
| `--coverage [path]`  | Filter mutants down to lines covered by per-test contexts.                       |
| `--no-coverage`      | Override `coverage` from config — disable coverage filtering.                    |
| `--exclude <GLOB>`   | Repeatable glob (relative to source root) pruning files before collection.       |

Output is one mutant id per line, followed by a `N mutant(s) total`
summary. No pytest, no cache, no history — pure enumeration. Pair
with `wc -l` for a quick mutant count under your current config.
