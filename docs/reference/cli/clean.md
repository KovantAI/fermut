# `fermut clean`

Wipe the result cache. **Preserves `history.jsonl`** so the trend log
survives cache rotation. To nuke everything, `rm -rf .fermut/`
manually.

```sh
fermut clean [PATH] [--history-path <p>]
```

| Flag                 | Default                          | Effect                                                                      |
|----------------------|----------------------------------|-----------------------------------------------------------------------------|
| `PATH`               | `.`                              | Where to look for `.fermut/`.                                                |
| `--history-path <p>` | resolved from config, else `<PATH>/.fermut/history.jsonl` | Path of the history log to preserve. Overrides the value resolved from the config file. Use when your history lives at a non-default location. |

See **[Caching](../../concepts/caching.md)** for when to clear the
cache and when to leave it alone.
