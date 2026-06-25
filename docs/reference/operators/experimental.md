# Experimental operators

Tagged `exp:` in output. Enabled via `--experimental` or
`experimental = true` in config.

| Operator                       | Example                                            |
|--------------------------------|----------------------------------------------------|
| `exp:exception-class-swap`     | `except ValueError:` → `except Exception:`         |
| `exp:bare-except`              | `except ValueError:` → `except :` (when no `as n`) |
| `exp:zero-iteration-for-loop`  | `for x in xs:` → `for x in []:`                    |
| `exp:one-iteration-for-loop`   | `for x in xs:` → `for x in [next(iter(xs))]:`      |

Experimental operators are higher-noise: they tend to produce many
equivalent mutants or run-time behavior that the test suite can't
usefully distinguish (e.g. broader exception classes often still
catch the same errors). Enable them when you want maximum mutation
coverage and have time to sift through the survivors.
