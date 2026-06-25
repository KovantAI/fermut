# Equivalent mutants

Some surviving mutants are unkillable on purpose: the mutated code
produces exactly the same observable behavior as the original. No
test can fail on them because there's nothing to assert against.
These are **equivalent mutants**, and they're a known limitation of
every mutation-testing tool, not a fermut bug.

This page is the resolution path: recognize them, mark them, move
on. For the survivor decision tree as a whole, see
**[Survivor triage](survivor-triage.md)**.

## What "equivalent" actually means

A mutant is equivalent when, **for every input the function could
receive**, the mutated code returns the same value, raises the same
exceptions, and produces the same side effects as the original.

The canonical example:

```python
def add_offset(x: int) -> int:
    return x + 0
```

Mutant (`arith-op-swap`):

```python
return x - 0
```

`x + 0 == x - 0` for every `x`. No test will fail on the mutant
because no test *can* — the output is identical. fermut will mark
it `SURVIVED`. It's not a coverage gap; it's a logical identity the
mutator can't see.

## Recognising one

You're looking at an equivalent mutant when:

- The diff shows a change that **looks** like a substitution but is
  algebraically a no-op or symmetry.
- You can convince yourself, with a short paper proof or a `hypothesis`
  property, that no input produces different output.
- Your attempts to write a killing test all assert against the
  identity itself ("just check the value is unchanged"), which is
  the property the mutant preserves.

Common families:

| Pattern                                  | Why equivalent                                                          |
|------------------------------------------|-------------------------------------------------------------------------|
| `x + 0` → `x - 0`                        | Adding or subtracting zero is the identity.                            |
| `x * 1` → `x / 1`                        | Multiplying or dividing by one is the identity.                        |
| `x and True` → `x or False`              | Both reduce to `bool(x)` for boolean inputs.                            |
| `len(xs) > 0` → `len(xs) >= 1`           | For integer lengths, `> 0` and `>= 1` are the same predicate.          |
| `if not flag: return None` → `if not flag: return` | Bare `return` and `return None` are byte-identical in Python.   |
| `range(n)` → `range(0, n)`               | Two spellings of the same iterator.                                    |

Less obviously, **domain-restricted equivalence** also counts: if a
function is only ever called with `x >= 0` (enforced by the caller's
type system or an upstream check), then `abs(x)` and `x` are
equivalent in that context — even though they differ for negative
inputs in general.

## Marking an equivalent mutant

Use an inline ignore marker on the source line:

```python
def add_offset(x: int) -> int:
    return x + 0    # fermut: ignore[arith-op-swap]
```

Forms:

| Marker                                 | Effect                                          |
|----------------------------------------|-------------------------------------------------|
| `# fermut: ignore`                     | Skip every operator on this line               |
| `# fermut: ignore[arith-op-swap]`      | Skip one named operator                        |
| `# fermut: ignore[op-a, op-b]`         | Skip multiple                                  |

Prefer the **narrowest** form. `# fermut: ignore[arith-op-swap]`
keeps boundary-shift / compare-op-swap / etc. live on the same line;
the bare `# fermut: ignore` switches them all off and can hide real
gaps that show up later.

Re-run fermut. The mutant is now `SKIPPED`, not `SURVIVED`, and the
score reflects the unkillable mutant being removed from the
denominator rather than counting against you.

See the [inline-ignore reference](../reference/operators/inline-ignore.md)
for the full syntax.

## Worked example

Source:

```python
# src/billing.py
def with_processing_fee(amount: float) -> float:
    # processing fee is zero for legacy accounts; kept as a constant
    # so the call site stays stable when the policy changes
    return amount + 0
```

`fermut run` reports a survivor:

```
[3] SURVIVED  src/billing.py:5  arith-op-swap
    -     return amount + 0
    +     return amount - 0
```

Diff: `+` swapped to `-`. For every possible `amount`, `amount + 0`
and `amount - 0` produce identical values. No assertion can
distinguish them — the mutant is equivalent.

Step 1: confirm equivalence. Either by inspection (adding and
subtracting zero are both the identity) or with a one-line
property test:

```python
from hypothesis import given
from hypothesis.strategies import floats

@given(floats(allow_nan=False))
def test_add_zero_equals_subtract_zero(x):
    assert x + 0 == x - 0
```

If it passes for thousands of inputs, the equivalence holds.

Step 2: mark it on the source line:

```python
def with_processing_fee(amount: float) -> float:
    return amount + 0    # fermut: ignore[arith-op-swap]
```

Step 3: re-run. The mutant is now `SKIPPED`, not `SURVIVED`. The
score moves up — not because tests changed, but because an
unkillable mutant is no longer counted against you.

## When you're not sure

If you can't quickly prove equivalence, **don't ignore yet**. False
positives in your "equivalent" calls compound over time and hide
real gaps. Better to leave it as a known survivor for a sprint and
revisit, than to ignore a mutant that's actually killable with the
right test.

A useful heuristic: if writing a one-line `hypothesis` property —
`@given(integers()) def test_equivalent(x): assert original(x) == mutated(x)` —
would be easy, do it. If it passes for thousands of inputs, the
ignore is justified. If it fails on one, you've found the killing
test.

## Auto-detection (the equivalent-mutant detector)

fermut runs a layered detector pipeline on every `SURVIVED` mutant
and removes provably-equivalent ones from the score denominator
automatically. Survivors flagged as *likely* equivalent (high
confidence but not proven) stay in the report as suggestions for
your inline-ignore review — they aren't dropped silently.

The default pipeline, cheap-first:

| Layer | Detector       | What it does                                                                                                  | Verdict it can emit            |
|-------|----------------|---------------------------------------------------------------------------------------------------------------|--------------------------------|
| 1     | AST patterns   | Pure-Rust rules over the mutant's operator + adjacent tokens. Catches `x +/- 0`, `x * 1`, `x // 1` identities. | provable / likely / silent     |
| 2     | CPython bytecode | Compiles original and mutated source with the active interpreter and compares code-object signatures. Catches `return` vs `return None`, byte-identical rewrites. | provable / silent              |

Two more layers are opt-in:

| Layer | Detector            | Cost / setup                                                                                                                                    | Default |
|-------|---------------------|-------------------------------------------------------------------------------------------------------------------------------------------------|---------|
| 3     | Hypothesis probe    | Differential property test via `hypothesis.given(from_type(...))`. Pass-on-N gives a 0.80 likely-equivalent signal that must stack with another layer to clear the pipeline threshold. Requires `hypothesis` installed. | off     |
| 4     | LLM judge           | Model-judged equivalence with a strict JSON contract and calibrated confidence clamp. Runs last because it's the most expensive + least precise. Requires `ANTHROPIC_API_KEY`. | off     |

The pipeline short-circuits on the first **provable** verdict.
Otherwise the likely-equivalent confidences stack (capped at 1.0)
and the mutant is flagged only when the combined confidence clears
the threshold (0.85 by default) — one strong rule passes; two weak
signals stack toward it.

Disable the whole pass with `--no-equiv-detect` or
`equiv_detect = false`. Useful when you want to see the raw,
uncorrected `SURVIVED` set.

For the detector internals — code layout, threshold tuning,
per-rule precision/recall — read `src/equiv/` directly. Each
detector lives in its own file (`patterns.rs`, `bytecode.rs`,
`probe.rs`, `llm.rs`) with rationale in the module-level
doc comment.

The detector catches the most common families automatically; inline
markers are still the right tool for domain-restricted equivalence
(things only equal because of an upstream invariant the detector
can't see).

## Where next

- **[Survivor triage](survivor-triage.md)** — the full kill / ignore /
  delete decision tree.
- **[Inline ignore reference](../reference/operators/inline-ignore.md)** —
  exact marker syntax and precedence.
- **[Filters](filters.md)** — coarser pruning when you have many
  equivalent mutants in one operator family.
