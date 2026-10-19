# `fermut clean`

Delete everything in `<PATH>/.fermut/` **except the history log**: the
result cache, `kill-order.json`, `dominators.json`, `llm-cache.json` and any
other artifacts. History is kept so the trend log survives cache rotation. To
remove everything, run `rm -rf .fermut/` yourself.

`clean` only looks in `<PATH>/.fermut/`. `run` writes artifacts under the
*project root* (the nearest directory with `pyproject.toml` / `setup.cfg`),
so run `fermut clean` from that root. `fermut clean src/` misses them.

```sh
fermut clean [PATH] [--history-path <p>]
```

| Flag                 | Default                          | Effect                                                                      |
|----------------------|----------------------------------|-----------------------------------------------------------------------------|
| `PATH`               | `.`                              | Where to look for `.fermut/`.                                                |
| `--history-path <p>` | resolved from config, else `<project root>/.fermut/history.jsonl` | Path of the history log to preserve. Overrides the value resolved from the config file. Use when your history lives at a non-default location. |

See **[Caching](../../concepts/caching.md)** for when to clear the
cache and when to leave it alone.
