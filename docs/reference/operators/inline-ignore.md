# Inline ignore markers

Drop mutations on specific lines by placing a marker comment on
that line:

```python
x = compute() + offset    # fermut: ignore
y = (a + b) and ready    # fermut: ignore[arith-op-swap, bool-op-swap]
return value or fallback  # fermut: ignore[bool-op-swap]
```

| Form                                  | Effect                                       |
|---------------------------------------|----------------------------------------------|
| `# fermut: ignore`                    | Skip **every** operator on this line         |
| `# fermut: ignore[op-name]`           | Skip only the listed operator on this line   |
| `# fermut: ignore[op-a, op-b]`        | Skip multiple operators (comma-separated)    |
| `# fermut: ignore-start` … `# fermut: ignore-end` | Skip every operator on the lines between, markers included |
| `# fermut: ignore-block` after a header (`if`, `def`, `for`, `with`, `try`, …) | Skip the header line and its whole body (not the `else`/`except` clauses) |
| `# fermut: ignore-block` on a line of its own | Skip from the marker to the end of the enclosing block |

The region forms mirror mutmut 3's `# pragma: no mutate start` /
`end` / `block`, and `fermut migrate mutmut` rewrites those to these.
An unclosed `ignore-start` runs to the end of the file; a stray
`ignore-end` does nothing. For a pattern that recurs across files
(every `log.debug(...)` call, say), use `ignore_patterns` in
[Configuration](../configuration.md) instead of a marker per line.

Operator names match the CLI / config (`arith-op-swap`,
`boundary-shift`, …); experimental and parity ops accept the bare name or the
`exp:` / `parity:` prefix (`zero-iteration-for-loop` or
`exp:zero-iteration-for-loop`). Unknown names inside the brackets
are dropped from the list; the marker still applies to any known
names alongside them. If **every** name in the brackets is unknown,
the marker is rejected entirely (no operators ignored) — typos
don't silently disable mutation on a line. The marker is only
honored when it appears in a real Python comment; strings that
happen to contain the text are not treated as markers.

Use it for known-equivalent mutants or boundary cases your tests
intentionally don't probe, instead of pruning whole operators
globally with `--skip-ops`.
