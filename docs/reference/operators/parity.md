# Parity operators

Tagged `parity:` in output. Enabled via `--parity` or `parity = true`
in config. **Off by default and never counted in normal scoring.**

These exist only to broaden overlap with other mutation tools (notably
[mutmut](https://github.com/boxed/mutmut)) so the two catalogues can be
compared head-to-head. They are very noisy — they mutate whole
expressions and call results to `None`, which produces large numbers of
trivially-equivalent or uninteresting mutants. Enable them for a
cross-tool parity study, not for measuring your suite.

| Operator                  | Example                                                          |
|---------------------------|------------------------------------------------------------------|
| `parity:expr-to-none`     | `obj.attr`, `seq[i]`, `f(x)` → `None` (value-position attribute / subscript reads and call results) |
| `parity:positional-drop`  | `f(x, y)` → `f(y)`; `[a, b, c]` → `[a, c]` (drops one positional call arg or one element of a list/set/tuple literal, ≥2 elements) |
| `parity:string-case-swap` | `"Hello"` → `"hELLO"` (swaps the case of a string literal, mirroring mutmut) |

`parity:expr-to-none` only fires on `Load`-context reads (mutating a
store target would be a syntax error); `parity:positional-drop` skips
`*args` / `**kwargs` splats and values already `None`.

## See also

- **[Parity report](../parity.md)** — the fermut ↔ mutmut catalogue
  comparison these operators were built to support.
- **[Selecting operators](selecting.md)** — narrow with `--ops` /
  `--skip-ops`.
