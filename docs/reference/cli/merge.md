# `fermut merge`

Combine JSON reports from sharded runs, or from several projects of a
monorepo into one pooled report.

```sh
fermut merge [<DIR>=]<input.json> [[<DIR>=]<input.json> ...] \
    [--json <p>] [--junit <p>] [--html <p>] [--markdown <p>] \
    [--history <p>] [--config-hash <hex>] [--project <dir>] \
    [--expect-shards <n>] [--max-errored <n>]
```

Deduplicates by `mutant.id` when the duplicates carry the same verdict
(harmless for clean shards; safety net for accidental overlap). It errors
out on conflicting verdicts for the same id, and when a mutant has no real
verdict in any input (a shard is missing). It also errors when the inputs
carry different `config_hash` values: every report records its run's
`config_hash` with `--shard` ignored, so shards of one run always agree,
and a disagreement means a shard ran with other flags or config. With no
output flag and no `--history`, prints merged JSON to stdout.

A report from `fermut run --shard i/n` records its slice as
`"shard": {"index": i, "total": n}`. Merge refuses inputs from different
shard counts (`1/16` with `2/8`), and fails when an index in `1..=n` has no
report, even one whose mutants were all filtered out. Reports that record no
shard (unsharded runs, older fermut) skip the missing-index check.

## Projects (`DIR=REPORT`)

Prefix every input with its project's repo-relative root (`packages/auth=r.json`)
to aggregate projects. Each project's inputs merge on their own with every
check above. Each mutant's file and id are then re-rooted at the repo
(`packages/auth/src/x.py`), and all projects are concatenated into one report.
Its counts give the pooled score, weighted by how many mutants each project
scored. The report gains a `projects` breakdown, worst score first, and its
`config_hash` is a hash over every project's `(dir, config_hash)`. A
`--history` entry carries the breakdown, with each project's own hash, for
[`fermut trend --project`](trend.md).

Merge fails when inputs mix prefixed and plain, when a `DIR` is absolute,
empty or contains `..`, when one `DIR` is nested in another (its mutants
would count twice), when a mutant's file is not under its `DIR`, and when a
project scored nothing. `--expect-shards` applies per project;
`--max-errored` to the pooled count.

## Gates

With `--history`, a merge that scored nothing fails instead of writing an
entry: the score is N/A, and recording it would put a vacuous 100% in the
trend. Gates run after the report files are written, so a failed merge
still leaves them to inspect. Every merge failure, these checks included, exits `3`:
the merged result can't be trusted, so it is a broken run, not a failed
gate.

| Flag                  | Default | Description                                                                                                                                                                             |
| --------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--history <p>`       | —       | Write a single history entry built from the merged report to `<p>` (overwriting). Git sha/branch read from the merge checkout, fermut version stamped automatically, counts are the merged full-universe totals, `duration_ms` is the sum of the shards' run times (total work, not elapsed; unset if any input predates it). Lets a sharded run record its trend point without harvesting a shard's history line. |
| `--config-hash <hex>` | —       | Override for the `--history` entry's `config_hash`. Default: the `config_hash` the input reports agree on, which equals an unsharded run's, so the merged entry is comparable to one. Unset when an input predates the field. |
| `--project <dir>`     | `.`     | Project root for git sha/branch discovery in the `--history` entry. Defaults to the merge checkout.                                                                                    |
| `--expect-shards <n>` | —       | Require exactly `n` shards: every input records its shard, all with total `n`, each index once. Catches a duplicated or stray report.                                                    |
| `--max-errored <n>`   | —       | Fail when more than `n` mutants errored. Reports are still written; the `--history` entry is not.                                                                                       |
