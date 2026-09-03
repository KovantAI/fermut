# Mutation testing

A short tour of what mutation testing measures, why it's worth the CI
minutes, and what fermut's specific vocabulary means. Already familiar
with mutmut or cosmic-ray? The model is the same; skip to
[Vocabulary](#vocabulary) for fermut's terminology choices.

## Why bother?

You already run `pytest`. You already track coverage. Why pay for
another tool that takes 10× longer to run?

Because **coverage measures execution, not verification**. A test that
calls a function and never asserts anything contributes to 100% line
coverage. A test that asserts `result is not None` when the function
returns a critical computed value is technically passing — but
swap the computation for `return 1` and the test still passes. The
test reaches the line; it does not check the line.

Mutation testing is the only mechanical way to find this gap. It
perturbs your code in small, deliberate ways and asks: *did any test
notice?* If no test noticed, the test suite has a hole at that
location. The hole is named, located, and reproducible — not a vague
"increase coverage" goal.

Concretely, on most Python codebases that have never been
mutation-tested, the score lands somewhere between 40% and 70% on the
first run. That gap is not a tooling failure; it's the difference
between "tests exist" and "tests would catch the regression you're
worried about." Closing it pays off most in code that *cannot* be
allowed to silently break — auth, billing, data migrations, parsing.

## What is mutation testing?

Mutation testing answers the question: **does your test suite actually
exercise the behavior of your code, or does it just touch the lines?**

Line coverage tells you which lines a test runs. It cannot tell you
whether the test would notice if that line was wrong. A test that calls
a function and asserts nothing produces 100% coverage and zero behavioral
guarantees.

Mutation testing fixes this by perturbing your code in small, semantically
meaningful ways — change `<` to `<=`, swap `True` for `False`, replace a
return value with `None` — and re-running the test suite against each
perturbation. The perturbation is the *mutant*.

- If the test suite **fails** on the mutant, the test detected the bug
  fermut introduced. The mutant is **killed**.
- If the test suite **passes** despite the change, there's a gap in your
  coverage. The mutant **survived**, and the surviving mutant is a
  pointer to a behavior your tests don't cover.

## A complete worked example

The smallest illustration. Source:

```python
# src/age.py
def is_adult(age: int) -> bool:
    return age >= 18
```

Test:

```python
# tests/test_age.py
from age import is_adult

def test_adult():
    assert is_adult(25) is True
```

Run fermut:

```sh
fermut run src/ --tests tests/ --json report.json
```

```
SURVIVED  src/age.py:2 [boundary-shift] `age >= 18` → `age > 18`

4 mutants — killed: 3, survived: 1, timeout: 0, skipped: 0, equivalent: 0, errored: 0  | score: 75.0%
```

Inspect the survivor:

```sh
fermut show report.json
```

```
[1] SURVIVED  src/age.py:2  boundary-shift
    -     return age >= 18
    +     return age > 18
```

What the survivor means: fermut changed `>=` to `>`. The test still
passes — because the test only checks `age=25`, where both `>= 18`
and `> 18` return `True`. The test never exercises the boundary at
`age=18`. That's the gap.

Write the killing test:

```python
def test_exactly_eighteen_is_adult():
    assert is_adult(18) is True
```

Re-run:

```
4 mutants — killed: 4, survived: 0, timeout: 0, skipped: 0, equivalent: 0, errored: 0  | score: 100.0%
```

The new test asserts behavior at `age=18` — exactly the boundary the
original test missed. With the boundary now covered, the
`>= 18` → `> 18` mutant produces `False` at `age=18`, the test fails
on the mutated code, the mutant is killed. Mutation testing **named**
the gap (boundary not exercised); writing one assertion closed it.

This is the inner loop. See
**[Survivor triage](../guides/survivor-triage.md)** for reading the
output systematically, and **[Equivalent mutants](../guides/equivalent-mutants.md)**
for the cases where a survivor is unkillable by design.

## The mutation score

The canonical formula:

```
score = detected / (detected + survived)

where detected = killed + timed_out
```

In words: of the mutants that ran to a verdict, what fraction did
your tests catch? Timeouts count as detected — a mutant that wedges
the test runner *did* change observable behavior, the runner just
gave up before getting a clean fail.

Two outcomes are excluded from both sides of the ratio:

- **Skipped** — a filter (ty, ruff, coverage, diff-only, sample, op
  allowlist, inline ignore) dropped the mutant before pytest ran.
  Not evidence for or against test quality.
- **Errored** — pytest crashed in a non-test-failure way (collection
  error, import error). Investigate the crash; don't count it.

Worked sample. If `fermut run` reports:

```
152 mutants — killed: 140, survived: 8, timeout: 1, skipped: 3, equivalent: 0, errored: 0  | score: 94.6%
```

Then `detected = 141` (140 + 1), denominator = `141 + 8 = 149`, and
score = `141 / 149 = 94.6%`. Skipped and errored are reported but
neither side counts them.

`fermut run` prints the score and exits non-zero whenever any mutant
survived, which is what makes it useful as a CI gate.

A few things to know about the number:

- **100% is rarely achievable.** Some mutants are *equivalent* — they
  produce code that behaves identically to the original even though
  the source looks different (e.g. `x + 0` → `x - 0`). Equivalent mutants
  are noise; they reduce the achievable ceiling.
- **The right target depends on your domain.** Auth/financial code
  benefits from ≥ 95%. A glue layer that mostly forwards calls might
  plateau at 70% because most of its lines are trivial passthroughs that
  every mutation breaks identically. Use trends, not absolutes.
- **Tracking is more useful than the absolute number.** `fermut trend`
  exists for this — see whether each PR moves the score up or down,
  rather than chasing a fixed target.

## Vocabulary

Quick-reference table. Full alphabetical lookup in
**[Glossary](glossary.md)**.

| Term         | What it means                                                                                              |
|--------------|------------------------------------------------------------------------------------------------------------|
| [Operator](glossary.md#operator-op)     | A *kind* of mutation (e.g. `arith-op-swap`, `boundary-shift`). fermut ships 30 stable + 8 experimental.    |
| [Mutant](glossary.md#mutant)       | One specific application of an operator to one specific source location.                                   |
| [Survived](glossary.md#survived)     | The mutant changed observable behavior, but the test suite passed anyway. **What you want fewer of.**     |
| [Killed](glossary.md#killed)       | The mutant changed behavior, and the test suite caught it. **What you want more of.**                     |
| [Timed out](glossary.md#timed-out)    | pytest ran longer than `--timeout` against this mutant. Usually means the mutant introduced an infinite loop. Counted as killed for scoring purposes. |
| [Skipped](glossary.md#skipped)      | A filter (ty, ruff, coverage, diff-only, sample, ops allowlist, inline ignore marker) refused this mutant before pytest ran. Not counted toward the score. |
| [Errored](glossary.md#errored)      | pytest crashed in a non-test-failure way (collection error, import error). Investigated, but not counted toward the score. |
| [Equivalent](glossary.md#equivalent-mutant)   | A mutant that produces semantically identical code. A layered detector (AST patterns + CPython bytecode by default) auto-promotes provably-equivalent survivors to the `equivalent` outcome and drops them from the score; likely-equivalent ones surface for inline-ignore review. |
| [Filter chain](glossary.md#filter-chain) | The composable stack of pre-flight checks every mutant runs through before pytest sees it.                  |

## The filter chain

The single biggest reason fermut is fast: every mutant runs through a
chain of cheap filters before any expensive test run. Each filter either
admits the mutant (let it proceed) or rejects it (skip).

Order is cheap → expensive:

| Stage          | Cost        | What it does                                                       |
|----------------|-------------|--------------------------------------------------------------------|
| experimental   | in-memory   | Drop experimental ops unless `--experimental`                      |
| operator       | in-memory   | Apply `--ops` allowlist and `--skip-ops` denylist                  |
| sample         | in-memory   | Random subset via `--sample <ratio>`                               |
| diff-only      | git read    | Drop mutants on lines not in `git diff <base>...HEAD`              |
| coverage       | file read   | Drop mutants on lines no test executes (per-test contexts required) |
| ruff           | subprocess  | Drop mutants that introduce new ruff lints (optional)              |
| ty             | subprocess  | Drop mutants that introduce new ty diagnostics                     |

The ty filter matters because 20–40% of naive mutants (e.g. swapping
an arithmetic op into a string-context binop) are type-invalid on
type-hinted codebases — see [benchmarks](../reference/benchmarks.md).
Catching them with a ~25ms type check is dramatically cheaper than
catching them with a pytest run.

Inline-ignore markers and the docstring auto-skip also act as filters.
See the [operators reference](../reference/operators/index.md) for the
catalogue and the inline-ignore syntax, and the
[filters guide](../guides/filters.md) for how to compose them.

## Where next

- **[Configuration](configuration.md)** — how fermut's config model
  works (file vs CLI vs profile).
- **[Caching](caching.md)** — what gets cached, when it invalidates,
  why it's the iteration-speed lever.
- **[Landscape](landscape.md)** — how fermut compares to mutmut and
  cosmic-ray, and the motivation for a new tool.
- **[Isolation modes](../reference/cli/run.md#isolation-modes)** — how
  workers patch source without touching the original tree.

## What fermut doesn't do (yet)

- **Test prioritization within coverage selection** — when coverage
  picks N tests for a mutant, they run in arbitrary order. Running the
  historical-killer first would cut wall-clock on survivor tails.

See the [project issues on GitHub](https://github.com/KovantAI/fermut/issues)
for current status + the [roadmap](../roadmap.md) for what's planned.

Equivalent-mutant detection — `x + 0 == x - 0` style trivial mutants
— used to be on this list. It now ships as a layered detector
pipeline (AST patterns + CPython bytecode by default, with opt-in
hypothesis probe and LLM judge). Provably-equivalent survivors are
auto-promoted to the `equivalent` outcome and dropped from the score
denominator; likely-equivalent ones surface as suggestions for
inline-ignore review. See
**[equivalent-mutants guide](../guides/equivalent-mutants.md)**.
