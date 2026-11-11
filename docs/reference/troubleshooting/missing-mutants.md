# Missing expected mutants

**Symptom.** Your report is missing some mutants you expected to
see.

**Cause.** A filter dropped them. Check the JSON report's
`skipped` outcomes — each carries the filter name in the
`filter` field.

## Diagnose

```sh
fermut run src/ --json out.json
jq '[.outcomes[] | select(.status == "skipped") | .filter] |
    group_by(.) |
    map({k: .[0], n: length})' out.json
```

This tells you which filter is doing the dropping (`ty`,
`coverage`, `diff-only`, etc.). Adjust accordingly.

If no `skipped` row explains it, the mutant was never generated.
`exclude` globs, `ignore_patterns` and inline
[`# fermut: ignore` markers](../operators/inline-ignore.md) drop sites at
collection, before any filter runs, so they leave no row in the report. Check
those settings for the file.

## See also

- **[Filters guide](../../guides/filters.md)** — what each filter
  does and how to turn it off.
