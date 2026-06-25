# Stable operators

Always emitted. Each operator name in the table is also its CLI /
config name (use with `--ops`, `--skip-ops`, or inline ignore
markers).

| Operator             | Example                                                            |
|----------------------|--------------------------------------------------------------------|
| `arith-op-swap`      | `a + b` → `a - b`; bitwise `&` → `\|`, `<<` → `>>`                 |
| `compare-op-swap`    | `a < b` → `a >= b` **and** `a > b` (each direction emits both flipped + non-strict-flipped pairs); `==` ↔ `!=`, `is` ↔ `is not`, `in` ↔ `not in` |
| `boundary-shift`     | `<` ↔ `<=`, `>` ↔ `>=` (fires alongside `compare-op-swap` — a single `<` produces 3 mutants total: `>`, `>=`, `<=`) |
| `bool-op-swap`       | `x and y` → `x or y`                                               |
| `unary-op-swap`      | `-x` → `+x`, `not x` → `x`, `~x` → `x`                             |
| `aug-assign-swap`    | `x += y` → `x -= y`, `//=` → `/=`, `&=` → `\|=`                    |
| `constant-replace`   | `True` ↔ `False`, `""` → `"fermut"`                                |
| `number-shift`       | `n` → `n+1`, `n` → `n-1` (decimal int / float literals only — hex `0xFF`, oct `0o77`, bin `0b11`, and complex `1j` literals are not mutated) |
| `return-value-to-none` | `return expr` → `return None`                                    |
| `break-continue-swap` | `break` ↔ `continue`                                              |
| `not-insertion`      | `if x:` → `if not (x):` (also `while`, `assert`)                   |
| `remove-decorator`   | drops `@deco` line above `def` / `class`                           |
| `default-arg-to-none` | `def f(x=10):` → `def f(x=None):`                                 |
| `arg-to-none`        | `f(x, y=2)` → `f(None, y=2)` and `f(x, y=None)` (each non-`None` positional / keyword call argument; skips `*args`/`**kwargs` splats and already-`None` values) |
| `lambda-body-to-none` | `lambda x: x*x` → `lambda x: None`                                |
| `slice-bound-drop`   | `a[1:n]` → `a[:n]` and `a[1:]`                                     |
| `slice-step-mutate`  | `a[::2]` ↔ `a[::1]` (and any non-`1` step collapses to `1`, e.g. `a[::3]` → `a[::1]`). Fires on any step expression, including variables — `a[::n]` → `a[::1]`. |
| `assign-value-to-none` | `x = expr` → `x = None`                                          |
| `none-to-value`      | `return None` → `return ""` (replaces a `None` literal with a non-`None` sentinel — flips `is None` / optional-default logic) |
| `number-to-zero`     | `n` → `0`                                                          |
| `number-to-neg`      | `n` → `-n`                                                         |
| `string-to-empty`    | `"hello"` → `""`                                                   |
| `string-sentinel`    | `"hello"` → `"XXhelloXX"` — preserves truthiness, mutates content   |
| `bytes-sentinel`     | `b"Bearer "` → `b"XXBearer XX"`                                     |
| `keyword-arg-drop`   | `f(x=1, y=2)` → `f(x=1)` (drops one kwarg + its comma)             |
| `dict-item-drop`     | `{a: 1, b: 2, c: 3}` → `{a: 1, c: 3}` (drops one item + comma)     |
