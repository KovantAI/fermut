# Experimental operators

Tagged `exp:` in output. Enabled via `--experimental` or
`experimental = true` in config.

| Operator                       | Example                                            |
|--------------------------------|----------------------------------------------------|
| `exp:exception-class-swap`     | `except ValueError:` → `except Exception:`         |
| `exp:bare-except`              | `except ValueError:` → `except :` (when no `as n`) |
| `exp:zero-iteration-for-loop`  | `for x in xs:` → `for x in []:`                    |
| `exp:one-iteration-for-loop`   | `for x in xs:` → `for x in [next(iter(xs))]:`      |
| `exp:raise-from-drop`          | `raise X from e` → `raise X` (also `from None`); drops explicit exception chaining |
| `exp:numeric-type-swap`        | `def f(x: int)` → `def f(x: float)` (and `float` → `int`); annotation only, never a runtime `int(...)` call |
| `exp:optional-type-drop`       | `Optional[int]` → `int`, `str \| None` → `str`; drops optionality from an annotation |
| `exp:container-type-swap`      | `list[int]` → `tuple[int]`, `set[T]` → `frozenset[T]` (builtin containers only, always in scope) |

Experimental operators are higher-noise: they tend to produce many
equivalent mutants or run-time behavior that the test suite can't
usefully distinguish (e.g. broader exception classes often still
catch the same errors). Enable them when you want maximum mutation
coverage and have time to sift through the survivors.

The **type-annotation** operators (`numeric-type-swap`,
`optional-type-drop`, `container-type-swap`) are experimental because
Python evaluates but does not enforce annotations — most survive
unless the project validates types at runtime (pydantic, dataclasses,
`beartype`, `typeguard`), where they surface real gaps. They mutate
annotation positions only (parameters, returns, `x: T = …`), never a
runtime `int(x)` / `list(x)` call. With the `ty` pre-filter on (the
default) an annotation mutated to an out-of-scope name is dropped
before it runs; `container-type-swap` stays within always-in-scope
builtins so it is safe even with `--no-ty-filter`.
