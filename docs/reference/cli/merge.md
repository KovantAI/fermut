# `fermut merge`

Combine JSON reports from sharded runs.

```sh
fermut merge <input.json> [<input.json> ...] \
    [--json <p>] [--junit <p>] [--html <p>] [--markdown <p>]
```

Deduplicates by `mutant.id` (harmless for clean shards; safety net for
accidental overlap). With no output flag, prints merged JSON to stdout.
