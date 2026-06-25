# `fermut trend`

Visualize the mutation-score history (`.fermut/history.jsonl`).

```sh
fermut trend [PATH] [flags...]
```

| Flag                          | Default                          | Effect                                                          |
|-------------------------------|----------------------------------|-----------------------------------------------------------------|
| `PATH`                        | `.`                              | Where to start the project-root walk.                            |
| `--history-path <p>`          | `.fermut/history.jsonl`          | Custom log location.                                             |
| `--limit <N>`                 | `10`                             | Show only the last N entries (ignored when `--all`).             |
| `--all`                       | off                              | Show every recorded entry.                                       |
| `--branch <NAME>`             | none                             | Keep only entries recorded on this git branch. Useful in CI where feature-branch runs would otherwise dilute the main-branch trend. |
| `--since <DATE>`              | none                             | Keep only entries newer than `DATE`. Accepts `YYYY-MM-DD` (start-of-day UTC) or `YYYY-MM-DDTHH:MM:SSZ`. |
| `--until <DATE>`              | none                             | Keep only entries older than `DATE`. Accepts `YYYY-MM-DD` (end-of-day UTC, full day included) or `YYYY-MM-DDTHH:MM:SSZ`. |
| `--fail-on-regression <PTS>`  | off                              | Exit non-zero when the most recent run dropped more than `PTS` score points vs the previous filtered entry. CI gate. |
| `--scale fixed\|auto`         | `fixed`                          | Sparkline scaling. `fixed` renders against `[0, 100]` for cross-window comparability; `auto` rescales to the window's own min/max (falls back to `fixed` when span < 1 pt). |
| `--diff`                      | off                              | Expand the per-run survivor diff into full new-survivor / newly-killed mutant-id lists below the table. |
| `--by file`                   | none                             | Aggregate the latest run's survivors. `file` groups by source file, prints counts + oldest survivor's age per group. |
| `--format human\|json`        | `human`                          | `human` (table + sparkline) or `json`.                           |

Run history is appended by `fermut run` unless disabled with
`--no-history`. `fermut clean` preserves `history.jsonl`.

See the **[trends guide](../../guides/trends.md)** for the full
workflow.
