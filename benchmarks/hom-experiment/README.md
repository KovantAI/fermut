# Higher-order mutation testing in Python — do SSHOMs exist?

First measurement (that we are aware of) of **strongly-subsuming higher-order
mutants (SSHOMs)** in a real Python codebase. All prior HOM/SSHOM empirical work
is on Java. This record exists so the numbers below are reproducible and
auditable later.

## TL;DR

On pyjwt `jwt/api_jws.py`, second-order:

| metric | value |
|---|---|
| killed FOMs (dedup) | 284 → 280 |
| candidate pairs | 17,922 |
| **SSHOMs** | **210 (1.2% of pairs)** |
| decoupled (`K(h)=∅`) | 7 |
| non-subsuming | 17,705 (98.8%) |
| **net FOM reduction** (greedy set-cover) | **18.3%** (284 → 232) |
| **cost** | **52.5× the FOM run** (17,922 HOM runs vs 348 FOM runs) |

**Verdict: SSHOMs exist in real Python, but are not a load-reduction win.** Yield
(18%) trails Java (35–45% in Wong et al. ICST 2021; 60–69% on small programs in
Nguyen & Madeyski 2015), and the construction cost (52×) dwarfs the saving. The
210 SSHOMs are function-local subtle 2-fault interactions — valuable as
test-writing *targets* (fault strength), not as a compute *saver*.

## Definitions

For first-order mutants (FOMs) `f1`, `f2` and the second-order mutant
`h = f1 ∘ f2` (both edits applied), with `K(x)` = the set of covering tests that
fail on mutant `x`:

- **SSHOM**: `K(h) ≠ ∅` and `K(h) ⊆ K(f1) ∩ K(f2)`. Killing `h` guarantees
  killing both constituents, so one SSHOM can replace both losslessly.
- **decoupled**: `K(h) = ∅` — the two faults mask each other, `h` survives.
- **non-subsuming**: `K(h) ≠ ∅` with a killer outside `K(f1) ∩ K(f2)`.

Classification is done with two `-x` probes per pair (does any intersection test
kill `h`? does any outside test kill `h`?), which is exact for the three-way
label above.

## Environment (exact)

- fermut: branch `features/record-kill-sets`, commit `7fad120f1c` **plus**
  uncommitted Phase-1 optimizations (staged `-x`, identical-edit dedup, rayon
  parallelism, incremental write) in `src/cli/subcmd/hom.rs` + `src/runner/mod.rs`.
- Subject: `benchmarks/fixtures/fermut-pyjwt`, PyJWT **2.13.0**, package `jwt/`,
  module under test `jwt/api_jws.py` (456 LOC), its own `tests/` suite.
- Python 3.9.6 · pytest 8.4.2 · pytest-cov (coverage 7.10.7) · cryptography 50.0.1.
- Coverage: the fixture's committed `coverage.json` (per-test contexts,
  `--cov-context=test`). Baseline suite green: 365 passed, 4 skipped.
- Raw data: `data/pyjwt-api_jws-fom.jsonl.gz` (348 FOM records),
  `data/pyjwt-api_jws-hom.jsonl.gz` (17,922 pair records).
  Uncompressed sha256:
  - FOM  `2b21fe1d8a6c48eff4e227fde338b28061ae07d3289408b58c6a2e86ae0665fc`
  - HOM  `b7561e1ad681d794628d8e9469bf84b1d1c38182fb456dc6c71b472904fd6505`

## Method / pipeline

1. **Phase 0 — FOM kill-sets.** `fermut run jwt/api_jws.py --record-kill-sets`
   drops pytest `-x` and records, per mutant, the full set of covering tests that
   fail (`K(f)`). Result: 348 records, 284 killed (score 82.1%).
2. **Phase 1 — HOM pairing + classification.** `fermut hom` regenerates the FOM
   catalogue, joins to Phase-0 kill-sets by mutant id, keeps killed FOMs, and
   pairs candidates gated by: **same file**, **non-overlapping byte ranges**, and
   **`K(f1) ∩ K(f2) ≠ ∅`**. The overlap gate is exact, not heuristic: `K(h) ⊆
   K(f1) ∩ K(f2)`, so an empty intersection cannot be an SSHOM. Each pair's HOM
   is spliced into a per-worker mirror and classified via the two `-x` probes.
3. **Phase 2 — metrics.** `analyze.py` computes density, decoupled rate, and a
   greedy set-cover estimate of net FOM reduction (repeatedly pick the SSHOM
   covering the most not-yet-covered FOMs; each replaces its 2 constituents with
   1 mutant).

## Exact commands

See `run.sh`. Core:

```sh
# Phase 0 (writes pyjwt-api_jws-fom.jsonl)
fermut run jwt/api_jws.py --tests tests --python <venv>/bin/python \
  --coverage coverage.json --no-cache --no-history --no-smart-order \
  --no-verify-baseline --record-kill-sets pyjwt-fom.jsonl

# Phase 1 (writes pyjwt-api_jws-hom.jsonl)
fermut hom jwt/api_jws.py --tests tests --python <venv>/bin/python \
  --coverage coverage.json --kill-sets pyjwt-fom.jsonl \
  --out pyjwt-hom.jsonl --max-pairs 100000

# Phase 2
python3 analyze.py pyjwt-fom.jsonl pyjwt-hom.jsonl
```

## Sanity checks (why the numbers are trustworthy)

- **Ground truth**: on a hand-built demo (a coupling `score()` pair and a
  cancelling `ident() = a-b+b` pair), the classifier returns exactly 1 SSHOM
  region and 1 decoupled pair. The staged-`-x` refactor reproduces the
  pre-refactor labels bit-for-bit.
- **Locality**: SSHOMs cluster where fault interaction is possible — median
  line-distance **2** (125/210 within 2 lines); non-subsuming pairs are spread
  across the module (median **98** lines). SSHOMs being function-local is the
  expected signature and argues the 210 are real interactions, not noise.
- **Score invariance**: dropping `-x` for kill-set recording did not change the
  Phase-0 kill/survive verdicts (score 82.1% either way).

## Threats to validity (read before generalizing)

- **One module, one repo.** api_jws.py only. Not yet generalizable to Python at
  large; density and reduction may differ elsewhere.
- **Same-file, not same-function gate.** 98.8% non-subsuming is dominated by
  cross-function pairs (median 98 lines apart) that inflate both the
  non-subsuming count and the 52× cost. A same-function gate would cut cost
  sharply and likely raise density — not yet run.
- **Greedy set-cover** gives an upper-ish bound on reduction, not the true
  minimum; 18.3% is indicative, not exact.
- **Timeout = not-killed** in a stage (conservative: can only understate a kill).
- **Reduction is over the *killed* FOM set**, not the whole catalogue.
- Coverage came from the committed fixture `coverage.json`; a fresh
  `--cov-context=test` regeneration should match (fixture is a pinned snapshot).

## Reproduce

```sh
cd benchmarks/hom-experiment
# inspect the preserved raw data without re-running:
gunzip -c data/pyjwt-api_jws-fom.jsonl.gz > /tmp/fom.jsonl
gunzip -c data/pyjwt-api_jws-hom.jsonl.gz > /tmp/hom.jsonl
python3 analyze.py /tmp/fom.jsonl /tmp/hom.jsonl
# full re-run: edit paths in run.sh, then: bash run.sh
```
