# Docstring skip

Docstrings, and any other statement that is only a plain string or
bytes literal, are **never mutated**. Such a statement does nothing at
runtime, so every mutation of it is equivalent. That covers the leading
docstring of a module, class, or function, and also a string placed
after an attribute (a common way to document fields), at any position
and nesting depth. f-string statements still mutate, since their `{}`
expressions can have side effects.

```python
def greet(name: str) -> str:
    """Return a friendly greeting."""   # <- never mutated
    msg = "hello"                       # <- StringToEmpty + StringSentinel fire here
    return msg + ", " + name


class Config:
    timeout: int = 30
    """Seconds before giving up."""     # <- never mutated
```

Reduces noise dramatically on documented codebases. No flag —
always on.
