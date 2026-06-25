# Detection parity vs mutmut

The wall-clock [benchmarks](benchmarks.md) answer "is fermut fast?". This page
answers the migration-risk question a team actually cares about: **if I switch
from mutmut to fermut, do I lose detection?** It is the cross-tool comparison
[the landscape page](../concepts/landscape.md) used to defer.

Short answer, measured across four real repos: **fermut's operator catalogue
covers ~99% of the source lines mutmut mutates, with zero detection-loss on the
mutations both tools generate.** The remaining ~1% is cosmetic string edge
cases, not missed test gaps.

## What is (and isn't) being claimed

"Parity" is not a single number, and it is **not** a strict superset — the two
tools ship different operator catalogues, so each generates mutations the other
doesn't (fermut has `not-insertion`, `boundary-shift`, `slice-bound-drop`,
`bytes-sentinel`, …; mutmut leans harder on blanket *expression→None* and string
case-swaps). The decision-relevant measures are:

1. **Catalogue coverage (generation).** For every mutation mutmut produces, does
   fermut produce *any* mutation on that same source line? A line mutmut mutates
   but fermut never touches is a **true catalogue hole**. This deliberately
   ignores coverage filtering — it asks what fermut's operators *can* express,
   not what a given run executed.
2. **Detection-loss (execution).** Among mutations *both* tools generate
   identically (same file + before/after source line) and both *executed*, are
   there cases mutmut **killed** but fermut reported **survived**? Those are the
   real "switching loses you a caught bug" cases.

## Results

Four repos with a completed mutmut run, fermut generating with `--parity` (so
its full catalogue, including the opt-in parity operators, is in play):

| Repo            | mutmut mutations | catalogue holes | hole % | detection-loss |
|-----------------|-----------------:|----------------:|-------:|---------------:|
| pyjwt 2.13.0    |            1,575 |              33 |     2% |              0 |
| markupsafe 3.0.3|              247 |              12 |     4% |              0 |
| typer 0.26.7    |            1,832 |              15 |     0% |              0 |
| more-itertools 10.5.0 | 3,512        |              19 |     0% |              0 |
| **total**       |        **7,166** |          **79** | **1.1%** |          **0** |

The 79 residual holes are **edge cases, not categories**:

- **~32 string case-swaps** (`"Foo"`→`"FOO"`/`"foo"`) fermut skips — docstrings,
  all-caps/all-lower strings (the swap is a no-op), and backslash-escaped strings
  (case-swapping `\n`→`\N` would change the escape).
- **~22 string-sentinel / f-string / multiline** edge cases — implicit string
  concatenation and a few multiline forms.
- **~18 blanket *expression→None*** on bare names and deeply-nested
  subexpressions that fermut's `expr-to-none` (call result / attribute /
  subscript) doesn't reach.
- A handful of comparison-artifact rows.

None are detection-relevant: coverage shows no mutant that mutmut catches and
fermut would let survive.

### What closed the gap

This comparison drove three catalogue additions (see [operators](operators/index.md)):

- **f-string mutation** (stable) — f-strings were previously never mutated.
- **`expr-to-none`, `positional-drop`, `string-case-swap`** — opt-in `--parity`
  operators (noisy by design; they exist to broaden cross-tool overlap, not for
  normal scoring). Off by default; never counted in a default run's score.

It also surfaced and fixed a real correctness bug: a stale-`.pyc` race in the
per-worker mirror that produced **nondeterministic false survivors**.

## Method

The comparison aligns on **source-line content**, not line numbers — mutmut 3.x
injects a trampoline that shifts every line number, so only the *text* of the
changed line is stable across both tools' working copies.

- mutmut per-mutant status comes from the `exit_code_by_key` maps in
  `mutants/**/*.py.meta` (0 = survived, 1 = killed, 33 = no-test → excluded), and
  the before/after lines from `mutmut show <id>`.
- fermut's mutants come from `fermut run … --json`. The generation set includes
  coverage-`skipped` mutants (they were still *generated*); the detection set
  uses only executed (killed/survived) mutants.

## Reproduce

The harness lives in [`benchmarks/parity/`](https://github.com/KovantAI/fermut/tree/main/benchmarks/parity).
See its `README.md`. In brief, per repo with an existing mutmut run:

```sh
# 1. extract mutmut's per-mutant verdicts + diffs
python extract_mutmut.py <mutmut-worktree> <mutmut-venv>/bin mutmut-<repo>.json
# 2. generate fermut's mutants (catalogue view: --parity + tiny --sample is enough)
fermut run <src> --tests tests --coverage coverage.json --parity \
    --sample 0.003 --json gen-<repo>.json
# 3. detection comparison + survivor diff
python compare.py gen-<repo>.json mutmut-<repo>.json report.md
# 4. operator-catalogue gap by kind
python map_operators.py gen-<repo>.json mutmut-<repo>.json operators.md
```

## Caveats

- **Four repos.** click and starlette are excluded — mutmut 3.x can't cleanly
  instrument them (click: circular import via mutmut's own CLI; starlette: the
  recorded run yielded no usable per-mutant exit codes).
- **Catalogue holes ≠ missed kills.** A hole means fermut's *operators* don't
  produce that exact mutation on that line — usually because fermut mutates the
  same line a *different* way. Detection-loss (0) is the measure that maps to
  "switching costs you a caught bug".
- **Line-content matching is a heuristic.** Whitespace-normalized line text can
  in principle collide; in practice the residual is small and hand-inspected.
- mutmut runs use its default invocation (no mypy pre-check) for an
  apples-to-apples comparison.
