# Permission denied writing to `.fermut/`

**Symptom.** fermut can't write `cache.json` or `history.jsonl`.

**Cause.** Running inside a container with a read-only mount, or
the workspace volume is owned by a different uid.

## Fix

Mount `.fermut/` as a writable subdirectory, or set
`--cache-path` / `--history-path` (CLI flags on `fermut run`) to a
writable location. Alternatively pin them in `fermut.toml` via
`cache_path = "..."` / `history_path = "..."`.
