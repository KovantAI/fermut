# `fermut merge`

Combine JSON reports from sharded runs.

```sh
fermut merge <input.json> [<input.json> ...] \
    [--json <p>] [--junit <p>] [--html <p>] [--markdown <p>] \
    [--history <p>] [--config-hash <hex>] [--project <dir>]
```

Deduplicates by `mutant.id` (harmless for clean shards; safety net for
accidental overlap). With no output flag, prints merged JSON to stdout.

| Flag                  | Default | Description                                                                                                                                                                             |
| --------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--history <p>`       | —       | Write a single history entry built from the merged report to `<p>` (overwriting). Git sha/branch read from the merge checkout, fermut version stamped automatically, counts are the merged full-universe totals. Lets a sharded run record its trend point without harvesting a shard's history line. |
| `--config-hash <hex>` | —       | `config_hash` to stamp into the `--history` entry. Merge can't derive it (the run config lives in the shard jobs), so pass the value the shards recorded, e.g. `$(jq -r .config_hash shard-1-entry.json)`. |
| `--project <dir>`     | `.`     | Project root for git sha/branch discovery in the `--history` entry. Defaults to the merge checkout.                                                                                    |
