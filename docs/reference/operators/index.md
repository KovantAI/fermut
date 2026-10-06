# Operators

A *mutation operator* is a rule that takes an AST node and emits one
or more mutant variants of it. fermut's operator catalogue is split
into three tiers: **stable** operators (always emitted),
**experimental** operators (require `--experimental` or
`experimental = true`), and **parity** operators (require `--parity`
or `parity = true`, for cross-tool comparison only).

- **[Stable operators](stable.md)** — 26 always-on operators with
  examples.
- **[Experimental operators](experimental.md)** — 4 higher-noise
  operators, off by default.
- **[Parity operators](parity.md)** — 3 opt-in operators that broaden
  overlap with other tools (mutmut) for comparison only. Very noisy;
  never counted in normal scoring.
- **[Operator profiles](profiles.md)** — `--operators minimal` swaps
  the easily killed compare and `and`/`or` mutants for the ones no other
  mutant subsumes.
- **[Selecting operators](selecting.md)** — `--ops` allowlist,
  `--skip-ops` denylist, when to narrow.
- **[Docstring skip](docstring-skip.md)** — module/class/function
  docstrings are never mutated.
- **[Inline ignore markers](inline-ignore.md)** — `# fermut: ignore`
  on a line to drop one or all operators from that line.

To add a new operator, see the
**[Internals → adding an operator](../internals/adding-operators.md)**
walkthrough.
