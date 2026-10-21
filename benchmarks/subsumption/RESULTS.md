# `minimal` profile gate: results (2026-10-07)

Branch `features/subsume/draft` @ 69dff21696, release build. Reproduce with
`benchmarks/subsumption/run-gate.sh` (reports land in `results/`, not committed;
~11 MB each for click/starlette).

## Rule checked

At each modelled site (single-op ordering compare, 2-operand truth-position
`and`/`or`), if every minimal-set mutant is detected, every mutant of the site is
detected. Run under the hidden `ror-all` profile so ordering sites carry all 7 forms.

## Result: 0 violations

| repo | sites | all-minimal-detected (checked) | violations |
|---|---|---|---|
| pyjwt | 38 | 19 | 0 |
| markupsafe | 3 | 1 | 0 |
| typer | 47 | 6 | 0 |
| click | 129 | 30 | 0 |
| starlette | 83 | 23 | 0 |
| **total** | **300** | **79** | **0** |

The other 221 sites have a surviving minimal mutant, so the model makes no claim
there (5 starlette sites include an errored mutant). No NaN/set/numpy/custom-`__lt__`
triage was needed.

## Mutant count (default → minimal), `fermut list` with coverage + ty filters

| repo | default | minimal | Δ | full |
|---|---|---|---|---|
| pyjwt | 1877 | 1934 | +3.0% | 1974 |
| markupsafe | 211 | 217 | +2.8% | 220 |
| typer | 1507 | 1597 | +6.0% | 1635 |
| click | 6587 | 6800 | +3.2% | 6951 |
| starlette | 6056 | 6161 | +1.7% | 6261 |

All within the ±10% acceptance band.

## Caveats

- **Small evidence base.** 79 checked sites across 5 repos. Weak suites leave most
  sites unchecked (typer: 6 of 47). None of the corpus does float/NaN-heavy or
  numpy comparisons, so those caveats are untested, not refuted.
- markupsafe ran with `--no-coverage` here: with `--coverage`, every mutant exited
  pytest 4. Cause: the mirror skipped the gitignored `_speedups*.so`, so ids
  parametrized on `markupsafe._speedups` didn't exist. Fixed 2026-10-20; a rerun
  with `--coverage` gives the same result (3 sites, 1 checked, 0 violations).
- starlette ran with `test_lifespan_with_on_events` deselected (baseline failure on
  this env).
