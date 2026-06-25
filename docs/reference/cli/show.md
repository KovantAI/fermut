# `fermut show`

Inspect mutants from a prior JSON report.

```sh
fermut show <report.json> [SELECTOR] [--all]
```

| Argument / flag       | Effect                                                                  |
|-----------------------|-------------------------------------------------------------------------|
| `<report.json>`       | Path to a JSON report produced by `fermut run --json …`.                |
| `SELECTOR`            | 1-based outcome index, the full mutant id, or any substring of the printed `file:line@offset …` row. A partial selector matching more than one mutant (e.g. two operators at one offset) errors and lists candidates — pass the full id or the index to disambiguate. Omit for the summary list. |
| `--all`               | Show every outcome, not just survivors.                                  |

Detail view prints (in order): `file:line`, `operator : <name>`,
`status : <label>`, `id : <mutant-id>`, `mutation : `<original>` → `<replacement>``,
then a blank line and a unified diff against the working copy.
Skipped outcomes append a `filter : <name>` line; errored outcomes
append `error : <message>`; equivalent outcomes append
`detector : <source>` + `reason : <reason>`. Diffs are regenerated on
demand from the mutant range + current source — keep in mind if the
file has changed since the run.
