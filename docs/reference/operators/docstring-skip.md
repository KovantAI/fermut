# Docstring skip

Module, class, and function/method docstrings are detected on a
pre-pass and **never mutated**. A docstring is the leading
`Stmt::Expr(StringLiteral)` of the corresponding body — the
standard Python convention.

```python
def greet(name: str) -> str:
    """Return a friendly greeting."""   # <- never mutated
    msg = "hello"                       # <- StringToEmpty + StringSentinel fire here
    return msg + ", " + name
```

Reduces noise dramatically on documented codebases. No flag —
always on.
