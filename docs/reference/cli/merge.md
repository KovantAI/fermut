# `fermut merge`

Combine JSON reports from sharded runs.

```sh
fermut merge <input.json> [<input.json> ...] \
    [--json <p>] [--junit <p>] [--html <p>] [--markdown <p>] \
    [--history <p>] [--config-hash <hex>] [--project <dir>]
```

Deduplicates by `mutant.id` when the duplicates carry the same verdict
(harmless for clean shards; safety net for accidental overlap). It errors
out on conflicting verdicts for the same id, and when a mutant has no real
verdict in any input (a shard is missing). It also errors when the inputs
carry different `config_hash` values: every report records its run's
`config_hash` with `--shard` ignored, so shards of one run always agree,
and a disagreement means a shard ran with other flags or config. With no
output flag and no `--history`, prints merged JSON to stdout.

| Flag                  | Default | Description                                                                                                                                                                             |
| --------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--history <p>`       | —       | Write a single history entry built from the merged report to `<p>` (overwriting). Git sha/branch read from the merge checkout, fermut version stamped automatically, counts are the merged full-universe totals, `duration_ms` is the sum of the shards' run times (total work, not elapsed; unset if any input predates it). Lets a sharded run record its trend point without harvesting a shard's history line. |
| `--config-hash <hex>` | —       | Override for the `--history` entry's `config_hash`. Default: the `config_hash` the input reports agree on, which equals an unsharded run's, so the merged entry is comparable to one. Unset when an input predates the field. |
| `--project <dir>`     | `.`     | Project root for git sha/branch discovery in the `--history` entry. Defaults to the merge checkout.                                                                                    |
