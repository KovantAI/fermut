# Higher-order mutation testing in Python — do SSHOMs exist?

First measurement (that we are aware of) of **strongly-subsuming higher-order
mutants (SSHOMs)** in a real Python codebase. All prior HOM/SSHOM empirical work
is on Java. This record exists so the numbers below are reproducible and
auditable later.

## TL;DR

SSHOMs **exist in every Python module tested** (pyjwt, more-itertools,
markupsafe) — first such measurement we're aware of; all prior HOM/SSHOM
empirical work is Java. Net FOM reduction ranges **16.5–47.4%** (same-function
gate), reaching Java's 35–45% (Wong et al. ICST 2021) in dense-logic code. But
construction cost is **~8.5–12.3× the FOM run** in every case, so HOM is
**load-negative** as a compute-reduction lever — the value is fault strength
(function-local subtle 2-fault interactions = high-value test targets), not
saving compute. **Verdict: PIVOT.** See the cross-repo table below; the
deep-dive numbers use pyjwt `api_jws.py`.

## Same-file vs same-function gate (pyjwt api_jws.py)

Adding the same-function gate (default in `fermut hom`; `--cross-function`
reproduces same-file) on the identical FOM set:

| gate | pairs | SSHOMs | density | net reduction | cost |
|---|---|---|---|---|---|
| same-file | 17,922 | 210 | 1.2% | 18.3% | 52.5× |
| same-function | 3,920 | 182 | 4.6% | 16.5% | **12.3×** |

Same-function keeps 182/210 (87%) of the SSHOMs while cutting pairs 78% and cost
4.3×, and quadruples density — SSHOMs really are function-local. It does not flip
the economics: 12.3× cost to save 16.5% is still load-negative. Data:
`data/pyjwt-api_jws-hom-samefn.jsonl.gz`.

## Cross-repo results (same-function gate)

Three modules across three repos, same-function gate, full (uncapped) pairing:

| repo · module | killed FOMs | pairs | SSHOM density | net reduction | cost |
|---|---|---|---|---|---|
| pyjwt · `api_jws.py` | 280 | 3,920 | 4.6% | 16.5% | 12.3× |
| more-itertools · `recipes.py` | 412 | 3,721 | 42.0% | 30.6% | 8.5× |
| markupsafe · `_native.py` (n=19) | 19 | 162 | 34.6% | 47.4% | 9.5× |

**Findings:**
1. **SSHOMs exist in every Python module tested** — this is not a Java-only
   phenomenon. (First such measurement we're aware of.)
2. **Yield is strongly module-dependent.** Dense-logic utility code
   (more-itertools, markupsafe) reaches 30–47% reduction — comparable to Java's
   35–45% (Wong et al. 2021). Branchy validation code with independent guards
   (pyjwt api_jws) stays low at 16.5%, because its faults are individually
   detectable → mostly non-subsuming.
3. **Cost is consistently load-negative: ~8.5–12.3× the FOM run** to save
   16–47% of a single FOM pass. HOM never pays back as a compute-reduction lever
   for a mutation tester, even where reduction is high — the construction runs
   dwarf the saving.

**Verdict: PIVOT.** SSHOMs transfer to Python and can match Java's reduction in
the right code, but the ~10× construction cost makes them load-negative
everywhere. Their value is fault-strength — function-local subtle 2-fault
interactions are high-value test-writing targets — not compute reduction.

Caveats: markupsafe is a tiny sample (19 FOMs → 47.4% is noisy); reduction is a
greedy set-cover upper bound over the killed set; one module per repo.

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

## SSHOM stability across commits (pyjwt api_jwk.py)

Does the SSHOM set persist across commits — i.e. can the ~10× discovery cost
amortize? Six chronological commits touching `jwt/api_jwk.py` were replayed (real
checkout → regen coverage → Phase 0 → Phase 1). Jaccard of the SSHOM set between
successive commits, under two identities: **loose** (content = operator +
`orig->repl`, position-insensitive) and **strict** (content + line number).
Data + scripts in `stability/`.

| transition | change | FOM J | SSHOM loose | SSHOM strict |
|---|---|---|---|---|
| feffd51→c0eae05 | pyupgrade reformat | 0.98 | **0.97** | 0.00 |
| c0eae05→53e9381 | pyright annotation | 0.94 | 0.97 | 0.97 |
| 53e9381→1451d70 | **logic** (drop algo-dict ref) | 0.85 | **0.73** | 0.00 |
| 1451d70→8915570 | logic (skip malformed JWKs) | 0.95 | 0.97 | 0.88 |
| 8915570→30b7ca1 | type annotation | 0.98 | 0.98 | 0.98 |

**Findings:**
1. **SSHOMs are highly stable.** Loose Jaccard 0.73–0.98 per commit, ≥0.97 for
   every non-logic change (annotations, dep bumps, reformatting). Even the two
   real logic changes kept 73% and 97% of SSHOMs.
2. **Identity must be content-based, not positional.** Reformatting (pyupgrade)
   drops strict Jaccard to 0.00 while loose stays 0.97 — the SSHOMs are
   unchanged, only moved. Any SSHOM cache must key on `(function, edit-content)`,
   never byte offset / line.
3. **SSHOM stability ≈ killed-FOM stability** (both ~0.85–0.98) — the subsuming
   structure is no more volatile than the underlying mutants.

**Implication for amortization.** SSHOMs persist until their *enclosing function*
changes, so with content-keyed caching + per-function ast-hash invalidation, most
commits reuse ≥95% of prior SSHOMs and only re-discover functions with real logic
edits. This *softens* the load-negative verdict for **full-repo periodic**
mutation runs on slowly-churning code (pay ~10× once, amortize). It does **not**
help **diff-scoped PR runs** — those target exactly the changed functions, which
are the ones needing re-discovery. Caveat: one module, six commits.

## Environment (exact)

- fermut: branch `features/record-kill-sets`, commit `7fad120f1c` **plus**
  uncommitted Phase-1 optimizations (staged `-x`, identical-edit dedup, rayon
  parallelism, incremental write) in `src/cli/subcmd/hom.rs` + `src/runner/mod.rs`.
- Subject: `benchmarks/fixtures/fermut-pyjwt`, PyJWT **2.13.0**, package `jwt/`,
  module under test `jwt/api_jws.py` (456 LOC), its own `tests/` suite.
- Python 3.9.6 · pytest 8.4.2 · pytest-cov (coverage 7.10.7) · cryptography 50.0.1.
- Coverage: the fixture's committed `coverage.json` (per-test contexts,
  `--cov-context=test`). Baseline suite green: 365 passed, 4 skipped.
- Cross-repo runs use the same `hom-venv` (pytest 8.4.2) and each fixture's own
  committed `coverage.json`: more-itertools (`more_itertools/recipes.py`, suite
  661 passed) and markupsafe (`src/markupsafe/_native.py`, pure-Python fallback
  since the committed `.so` is CPython 3.13 vs the 3.9 venv).
- Raw data (all in `data/`, gzipped JSONL). Uncompressed sha256:
  - pyjwt FOM `2b21fe1d8a6c48eff4e227fde338b28061ae07d3289408b58c6a2e86ae0665fc`
  - pyjwt HOM same-file `b7561e1ad681d794628d8e9469bf84b1d1c38182fb456dc6c71b472904fd6505`
  - pyjwt HOM same-function `0c0008c722bf04534a52a9f5b57d14055a1d8eaf1f9ecbe83b0e37d2e7eea114`
  - more-itertools FOM `fc843f71524202b81b98aa012b24c60277640207a500371751ba0f1fc30be390`
  - more-itertools HOM same-function `b04aee2fa841f7a23c02910a9221319ca2cc34d812b1944b8b579d6317fbab04`
  - markupsafe FOM `0cd52c64599fd8ba8283f9cce98779e5daa3c1d1490ae34317c34ebb9e66b681`
  - markupsafe HOM same-function `e244b41a0211f902bb06df38cc2b064e9b55692d60712eb47064988155766486`

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
