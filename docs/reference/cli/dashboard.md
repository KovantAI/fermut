# `fermut dashboard`

Generate a self-contained HTML dashboard combining the trend log with the
latest run's survivor drill-down. Single file, inline CSS + SVG — no
JavaScript, no external assets.

```sh
fermut dashboard [PATH] [flags...]
```

| Flag                 | Default                          | Effect                                                          |
|----------------------|----------------------------------|-----------------------------------------------------------------|
| `PATH`               | `.`                              | Where to start the project-root walk.                            |
| `--history-path <p>` | `<PATH>/.fermut/history.jsonl`   | Custom history-log location.                                     |
| `--output <p>`       | `fermut-dashboard.html`          | Output path for the generated HTML.                              |
| `--report <p>`       | none                             | JSON report (from `fermut run --json`) used to attach inline source diffs to each survivor. Without it the survivor list still renders, but only IDs are shown. |
| `--limit <N>`        | `30`                             | How many trailing history entries to show in the chart and table.|
| `--open`             | off                              | Hand the file off to the OS's default browser (`open` / `xdg-open` / `start`). Best-effort; failure degrades to a printed hint, not a non-zero exit. |

Pair with `fermut run --json run.json` to embed per-survivor source diffs:

```sh
fermut run src/ --json run.json
fermut dashboard --report run.json --open
```
