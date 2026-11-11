# Selecting which operators to run

`--ops` (allowlist) and `--skip-ops` (denylist) are comma-separated
lists of operator names. `--skip-ops` wins over `--ops`.

```sh
# Run only arithmetic + boundary mutations
fermut run src/ --ops arith-op-swap,boundary-shift

# Run everything except number shifts (noisy on constants tables)
fermut run src/ --skip-ops number-shift

# Allowlist via fermut.toml
# ops = ["arith-op-swap", "compare-op-swap", "boundary-shift", "ror-equality", "ror-const"]
# skip_ops = ["number-shift"]
```

Use these when:

- You want to **start small** and grow the set as your tests improve.
- A specific operator is producing high false-positive noise in your
  codebase (e.g. `string-sentinel` mutates internal sentinel strings
  that aren't supposed to be tested).
- You're targeting a specific class of bug (auth code →
  `compare-op-swap` for its `==` / `is` / `in` checks, plus
  `boundary-shift` + `ror-equality` + `ror-const` for ordering
  compares under the default `minimal` profile, and `bool-operand-drop`
  + `bool-const` for `and` / `or` conditions; arithmetic code →
  `arith-op-swap` + `number-shift`).

See the **[filters guide](../../guides/filters.md)** for how
`--ops` composes with the rest of the filter chain.
