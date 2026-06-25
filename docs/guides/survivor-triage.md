# Survivor triage

You've run `fermut run`, you have a list of surviving mutants, and
now you have to decide what to do with them. This page is the
decision flow: how to read one, how to choose between killing it
and ignoring it, and how to write the killing test when killing is
the right call.

For the conceptual background, see
**[Mutation testing](../concepts/mutation-testing.md)**. For the
unkillable cases, see
**[Equivalent mutants](equivalent-mutants.md)**.

## Reading a survivor

The detail view from `fermut show` is the unit of triage:

```sh
fermut show report.json 1
```

```
[1] SURVIVED  src/auth/session.py:42  boundary-shift
    operator : boundary-shift
    range    : src/auth/session.py:42:8-42:34
    diff:
    -     if now - created_at > MAX_AGE:
    +     if now - created_at >= MAX_AGE:
```

Five things to read off the output:

| Field        | What it tells you                                                                                                 |
|--------------|-------------------------------------------------------------------------------------------------------------------|
| File:line    | Where the mutation was applied. Open the file in your editor before doing anything else.                          |
| Operator     | Which *kind* of mutation. The operator name is your hint for what kind of test would catch it.                     |
| Range        | Exact source span. fermut points at the token, not just the line — useful when one line has multiple mutables.     |
| Diff         | The actual change. Read this line carefully — the survivor question is "would my test notice this difference?"   |
| Status       | Always `SURVIVED` in triage; if it's `Errored` or `TIMEOUT`, that's a different problem (see notes below).         |

## The triage decision

For each survivor, you have four options:

```
Read the diff.
│
├── Does the mutated code produce different runtime behavior?
│   │
│   ├── Yes → Can your test suite reach the mutated line with inputs
│   │        that would expose the difference?
│   │   │
│   │   ├── Yes → Kill it: write the test.                  (most common)
│   │   └── No  → The branch is unreachable, dead, or guarded by
│   │              an invariant tests can't violate. Verify the dead
│   │              code is actually dead, then delete it. If it's
│   │              not dead, write the test.
│   │
│   └── No  → Equivalent mutant. The patched code is semantically
│              identical to the original (e.g. `x + 0` → `x - 0`).
│              Mark with `# fermut: ignore[<op>]`. See
│              [Equivalent mutants](equivalent-mutants.md).
│
└── Mutated code looks suspicious / is unreachable in practice
    → Could be a coverage gap *or* dead code. If the line genuinely
       can't be exercised, delete the line. If it can, kill the
       mutant.
```

Roughly: kill, ignore (equivalent), or delete (dead code). The
fourth option — **leave it as a known survivor** — is fine when the
survivor is real but low-priority. Track it; don't pretend it isn't
there.

## Worked example: killing a survivor

Survivor:

```
[1] SURVIVED  src/parser.py:18  arith-op-swap
    -     return n * factor
    +     return n + factor
```

Step 1: open the file at `src/parser.py:18`. Find the function:

```python
def scale(n: int, factor: int) -> int:
    return n * factor
```

Step 2: find the existing test:

```python
def test_scale():
    assert scale(2, 3) > 0    # weak assertion
```

The test passes on the original (`2 * 3 = 6 > 0` ✓). It *also*
passes on the mutant (`2 + 3 = 5 > 0` ✓). The assertion is too
weak to distinguish multiplication from addition — that's the gap.
This is the most common cause of surviving arithmetic mutants:
existing tests reach the line but assert on a property
(non-negative, truthy, in-range) that both the original and the
mutated operator satisfy.

Step 3: tighten the assertion:

```python
def test_scale_multiplies():
    assert scale(2, 3) == 6
    assert scale(4, 5) == 20
```

Step 4: re-run. The mutant fails the tightened test (`2 + 3 == 6`
is False) → killed.

The lesson: surviving mutants usually point at one of three things —
*missing input case* (boundary not tested), *weak assertion*
(`> 0` when the operator's signature demanded `==`), or *missing
test* (the function is never called from any test). The operator
name hints at which:

| Operator family       | Usually points at                                                  |
|-----------------------|--------------------------------------------------------------------|
| `boundary-shift`      | Missing input at the boundary (`<` vs `<=`).                       |
| `arith-op-swap`       | Weak assertion that wouldn't catch an arithmetic substitution.    |
| `compare-op-swap`     | Missing test for the inverse comparison case.                     |
| `bool-op-swap`        | Test only exercises the "true" path of an `and` / `or`.            |
| `return-to-none`      | Function's return value is never asserted against.                |
| `constant-replace`    | Magic number not pinned by a test.                                |

## When to ignore instead of kill

Three legitimate ignore cases:

1. **Equivalent mutant.** No test can kill it because the mutant
   behaves identically. See
   [Equivalent mutants](equivalent-mutants.md). Use
   `# fermut: ignore[<op>]` on the line.
2. **Defensive code with no callable path.** A `raise` in an
   `else` branch that the type system makes unreachable. Mark
   ignore *and* consider whether the branch belongs in the code
   at all.
3. **Boundary the spec intentionally doesn't pin.** A logging-only
   path where "off by one" doesn't change observable behavior. Mark
   ignore with a comment explaining why future-you shouldn't second-guess.

Don't ignore because killing is hard. Hard-to-kill survivors are
usually the most important ones — they're the cases your test
suite genuinely doesn't exercise.

## Surviving outcomes that aren't `SURVIVED`

`fermut show` reports four outcomes you might mistake for "needs a
test":

- **`Errored`** — pytest crashed before producing a verdict. Not a
  test gap; usually a test-suite side effect on the source tree.
  See [errored outcomes](../reference/troubleshooting/errored-outcomes.md).
- **`TIMEOUT`** — pytest ran longer than `--timeout`. Counts as
  killed for scoring. Worth investigating if the mutant introduced
  an infinite loop in code that *could* be reached at runtime.
- **`SKIPPED`** — a filter dropped the mutant. Not a survivor at
  all; nothing to triage.

Only triage rows with status `SURVIVED`.

## Where next

- **[Equivalent mutants](equivalent-mutants.md)** — when the
  survivor is genuinely unkillable, this is the resolution.
- **[Filters](filters.md)** — narrowing the survivor list before
  triage so you spend time on the right ones.
- **[`fermut show`](../reference/cli/show.md)** — every flag the
  inspection path supports.
- **[`fermut suggest`](../reference/cli/suggest.md)** — LLM-assisted
  killing-test suggestions if you have it wired up.
