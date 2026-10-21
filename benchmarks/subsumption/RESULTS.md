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

---

# Widened gate (2026-10-21, issue #28)

Branch `features/pytest/fermut`, release build. Same rule and `gate.py`, run
under `--operators ror-all --ops <site-tagged ops>` with coverage, on six more
repos picked for stronger suites and float/numpy comparisons. Python 3.12,
fresh venvs (the fixture venvs broke when Homebrew removed python@3.13).

## Result: 0 violations

| repo | ref | sites | all-minimal-detected (checked) | violations |
|---|---|---|---|---|
| more-itertools | v10.5.0 | 137 | 58 | 0 |
| jinja2 | 3.1.6 | 116 | 33 | 0 |
| flask | 3.1.3 | 50 | 19 | 0 |
| werkzeug | 3.1.8 | 171 | 34 | 0 |
| uncertainties (float, NaN, numpy) | 1764673 | 37 | 22 | 0 |
| pyquaternion (numpy) | 99025c1 | 20 | 0 | 0 |
| **total** | | **531** | **166** | **0** |

With the 2026-10-07 run: **831 sites, 245 checked, 0 violations, 11 repos.**

## Mutant count (classic → minimal), `fermut list`

| repo | classic | minimal | Δ | full |
|---|---|---|---|---|
| more-itertools | 3753 | 3843 | +2.4% | 4060 |
| jinja2 | 6333 | 6525 | +3.0% | 6639 |
| flask | 2848 | 2948 | +3.5% | 2990 |
| werkzeug | 10466 | 10772 | +2.9% | 10959 |
| uncertainties | 2641 | 2692 | +1.9% | 2746 |
| pyquaternion | 1761 | 1766 | +0.3% | 1803 |

## Setup notes

- Each repo got a `fermut.toml` (`source_root`, `tests`) and a fresh
  `fermut coverage`.
- flask: `pytest<8` (its `tests/test_cli.py` imports `_pytest.monkeypatch.notset`),
  `--deselect tests/test_reqctx.py::test_bad_environ_raises_bad_request`
  (fails against the installed werkzeug).
- werkzeug: `--deselect "tests/test_serving.py::test_server[unix socket]"`
  (AF_UNIX path too long under the temp dir). 124 mutants errored with pytest
  exit 4 (usage error) under node-id selection. The gate skips errored
  mutants, so they only shrink the checked count. Cause not investigated.
- uncertainties: `-e .[arrays]` plus scipy.
- pyquaternion: `numpy<2` (it predates numpy 2); tests live in
  `pyquaternion/test`, excluded from mutation. Its suite killed every minimal
  mutant at no site, so numpy comparisons remain untested in practice.
- Run with a private `TMPDIR`: a leftover mode-000 directory in the shared
  `pytest-of-$USER` temp root made pytest warn at session end, which the
  pallets repos' `filterwarnings = error` turns into a failed baseline.
