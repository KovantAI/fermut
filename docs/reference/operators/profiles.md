# Operator profiles

`--operators <profile>` (or `operators = "<profile>"` in config) picks which
mutants fermut makes for ordering comparisons (`<`, `<=`, `>`, `>=`) and for
`and` / `or` used as a condition.

| Profile   | What you get                                                        |
|-----------|---------------------------------------------------------------------|
| `default` | The usual swaps: 3 per ordering compare, `and` ↔ `or`. Unchanged.   |
| `minimal` | Only the mutants no other mutant subsumes (3 per site). Opt-in.      |
| `full`    | Both sets. For experiments.                                          |

On the parity corpus (click, pyjwt, starlette, typer, markupsafe), `minimal`
makes 1–3% more mutants than `default`.

## Why `minimal`

Take `if a < b:`. A test can only catch a changed comparison by picking
inputs where the change gives a different answer. There are three kinds of
input: `a < b`, `a == b`, `a > b`. Each mutant differs from `<` on some of
them:

| mutant  | differs when       | in `default` | in `minimal` |
|---------|--------------------|:------------:|:------------:|
| `<=`    | `a == b`           | yes          | yes          |
| `!=`    | `a > b`            |              | yes          |
| `False` | `a < b`            |              | yes          |
| `>`     | `a < b` or `a > b` | yes          |              |
| `>=`    | always             | yes          |              |

A mutant that differs on more inputs is easier to kill. Any test that kills
`!=` also kills `>`, and any test at all kills `>=`. So in `default`, two of
the three mutants are killed by almost any test. A suite that never tries
`a > b` still kills all three and scores 100%. `minimal` keeps one mutant per
kind of input, so that gap shows up as a surviving `!=`.

The minimal sets (Kaminski, Ammann & Offutt, 2011):

| original  | `minimal` mutants      |
|-----------|------------------------|
| `a < b`   | `<=`, `!=`, `False`    |
| `a <= b`  | `<`, `==`, `True`      |
| `a > b`   | `>=`, `!=`, `False`    |
| `a >= b`  | `>`, `==`, `True`      |
| `a and b` | `a`, `b`, `False`      |
| `a or b`  | `a`, `b`, `True`       |

Expect the score to drop where a suite skips one of the kinds of input. That
drop is real: the old score counted mutants that any test kills.

## Where it applies

- **Ordering compares with one operator.** `a < b`, not `a < b < c`, and not
  `==`, `!=`, `is` or `in`. Swapping `==` for `<=` would raise on
  unorderable values such as `None`.
- **`and` / `or` with two operands whose value is only used as a condition:**
  `if` / `elif` / `while` / `assert` tests, ternary tests, `not` operands,
  comprehension `if`s, `match` guards, and operands of such a condition.
  `x = a or default` keeps `and` ↔ `or`, because replacing it with `a` changes
  the value, not just its truth.

Everything else is the same in all profiles.

## New operators

| Operator            | Example                                              |
|---------------------|------------------------------------------------------|
| `ror-equality`      | `a < b` → `a != b`; `a <= b` → `a == b`              |
| `ror-const`         | `a < b` → `False`; `a <= b` → `True`                 |
| `bool-operand-drop` | `if a and b:` → `if (a):` / `if (b):`                |
| `bool-const`        | `if a and b:` → `if False:`; `if a or b:` → `if True:` |

`minimal` also drops the default swaps it replaces (`<` → `>`, `>=`; `and` →
`or` at a covered site). `--ops` / `--skip-ops` still apply on top.

## Caveats

The argument above assumes values are fully ordered and that the result is
used as a plain `True`/`False`. Python doesn't always do that:

- `<` on sets means "subset". Two sets can be neither `<`, `==` nor `>`.
- NaN makes every comparison false.
- A custom `__lt__` can return anything.
- numpy and pandas comparisons return arrays, so `False` changes the type and
  the mutant dies on a `TypeError`.

`minimal` stays opt-in until `benchmarks/subsumption/gate.py` shows no
unexplained violations on the parity corpus. The gate runs every form of each
site and checks that whenever the `minimal` mutants all die, the others die
too.

## Folded survivors

The same reasoning shrinks the list of survivors to fix, in every profile.
If `a < b` → `a > b` and `a < b` → `a >= b` both survive, a test that kills
`>` also kills `>=`, so there is one test to write, not two. Reports nest
`>=` under `>`; [`fermut next`](../cli/next.md) puts it in `subsumed_ids`
and counts it in the gain; the JSON summary gives `survivor_targets` (the
survivor count after folding) next to `survived`. The score itself is
unchanged.

## Score history

A non-default profile changes the run's `config_hash`, so `fermut trend`
shows a config change rather than a regression. Default-profile hashes are
unchanged.
