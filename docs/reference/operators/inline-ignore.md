# Inline ignore markers

Drop mutations on specific lines by placing a marker comment on
that line:

```python
x = compute() + offset    # fermut: ignore
y = a if flag else b      # fermut: ignore[arith-op-swap, bool-op-swap]
return value or fallback  # fermut: ignore[bool-op-swap]
```

| Form                                  | Effect                                       |
|---------------------------------------|----------------------------------------------|
| `# fermut: ignore`                    | Skip **every** operator on this line         |
| `# fermut: ignore[op-name]`           | Skip only the listed operator on this line   |
| `# fermut: ignore[op-a, op-b]`        | Skip multiple operators (comma-separated)    |

Operator names match the CLI / config (`arith-op-swap`,
`boundary-shift`, …); experimental ops accept the bare name or the
`exp:` prefix (`zero-iteration-for-loop` or
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
