# Benchmarks

A snapshot of how fermut compares to other Python mutation testing
tools on the same code, same machine. The numbers below are
**illustrative and preliminary** — your codebase will produce
different results, especially with different test-suite shapes or
coverage-context quality. See the caveats at the bottom before
drawing conclusions.

For methodology and a side-by-side feature table, see
**[Concepts → landscape](../concepts/landscape.md)**.

## Methodology

The benchmark harness under `benchmarks/` clones a set of real,
pinned open-source projects and runs each tool against the same
source tree with the same test suite, on the same machine. Each
tool gets its own venv under `fixtures/.venvs/<tool>-<repo>/` so
dependency conflicts don't bleed across runs.

Four timing scenarios are defined (`benchmarks/src/bench/scenarios/`):

| Scenario     | What it measures                                                  |
|--------------|-------------------------------------------------------------------|
| `cold`       | Fresh clone + tool install + repo install + first mutation pass   |
| `warm`       | Single re-run with caches populated, no code changes              |
| `loop`       | Avg of N iterations: small edit → re-run → revert                 |
| `score_3pct` | Claude-Code-in-loop: write tests until score rises ≥ 3 pts        |

Each run writes one JSON file to `benchmarks/results/runs/` with
per-phase timing, mutant total, score, and return code.

## Cold runs (pinned OSS repos)

The numbers below are `cold`-scenario runs of fermut 0.4.1 vs
mutmut 3.6.0, **both re-run back-to-back on the same machine
(2026-06-12)**, excluding clone / install. fermut runs **with its
coverage filter on** (the designed configuration — see below);
mutmut runs its default.

> **This table is canonical.** It's the most recent same-session
> re-measurement, so trust it for head-to-head scores and timings.
> Other figures further down (the coverage-filter before/after
> illustration, the `score_3pct` agent-loop baselines) come from
> *earlier* measurement sessions and can differ by ~0.3 pts or a few
> seconds — they're kept for their before/after *deltas*, not as
> absolute head-to-head numbers.

fermut's cold cost splits into two phases the harness now times
separately: **`coverage_prep`** (the one-time baseline coverage run +
`pytest-cov` install — reused by every later run) and
**`mutation_run`** (the mutation work itself). The comparable
cold total a user waits on first contact is the sum. mutmut has no
coverage phase, so its number is the whole run.

| Repo (ref)            | Tool   | Mutants | Score  | cov_prep | mutation | Cold total |
|-----------------------|--------|---------|--------|----------|----------|------------|
| typer 0.26.7          | fermut | 2173    | 71.5   | ~90s     | ~74s     | **~164s**  |
|                       | mutmut | 2096    | 70.5   | —        | —        | ~298s      |
| pyjwt 2.13.0          | fermut | 2491    | 82.2   | ~9s      | ~34s     | **~42s**   |
|                       | mutmut | 1617    | 75.4   | —        | —        | ~45s       |
| more-itertools v10.5.0| fermut | 3976    | 87.0   | ~11s     | ~293s    | ~304s      |
|                       | mutmut | 3579    | 90.2   | —        | —        | ~118s      |

The picture is **mixed but competitive**:

- **typer:** fermut wins on both — faster (~164s vs ~298s) and
  higher-scoring (71.5 vs 70.5).
- **pyjwt:** fermut scores higher (82.2 vs 75.4) at roughly the same
  wall-clock (~42s vs ~45s).
- **more-itertools:** mutmut is faster (~118s vs ~304s) and slightly
  higher-scoring (90.2 vs 87.0) — a small pure-Python library with
  fast tests, where fermut's ty + coverage overhead doesn't pay for
  itself as it does on the heavier suites.

Because `coverage_prep` is a **one-time** cost (the `.coverage` DB is
reused on every subsequent run), the *steady-state* mutation cost is
the `mutation` column alone — e.g. typer ~74s vs mutmut ~298s (~4×).
The cold totals above are the conservative first-contact figure.

### Full fermut sweep (coverage-on cold)

All ten suite repos, `cold`, coverage-on, sorted by mutant count.
mutmut pairs exist only where mutmut runs (see below); the rest are
fermut-only.

All ten re-measured same-session on one machine (2026-06-12) with the
`coverage_prep` / `mutation_run` split. `cov_prep` is the one-time
coverage-gen cost; `mutation` is the mutation work; `Cold total` is
their sum (first-contact wall-clock).

| Repo (ref)             | Mutants | Score   | cov_prep | mutation | Cold total |
|------------------------|---------|---------|----------|----------|------------|
| markupsafe 3.0.3       | 266     | 100.0†  | ~2s      | ~3s      | ~9s        |
| typer 0.26.7           | 2173    | 71.5    | ~90s     | ~74s     | ~164s      |
| pyjwt 2.13.0           | 2491    | 82.2    | ~9s      | ~34s     | ~42s       |
| more-itertools v10.5.0 | 3976    | 87.0    | ~11s     | ~293s    | ~304s      |
| flask 3.1.3            | 4114    | 77.5    | ~8s      | ~144s    | ~155s      |
| click 8.1.7            | 7052    | 82.9    | ~20s     | ~211s    | ~235s      |
| starlette 0.52.1       | 8061    | 81.3    | ~18s     | ~317s    | ~340s      |
| jinja2 3.1.6           | 10087   | 78.7    | ~49s     | ~243s    | ~296s      |
| werkzeug 3.1.8         | 15354   | 73.5    | ~17s     | ~316s    | ~338s      |
| trio v0.33.0           | 12375   | ~54‡    | ~31s     | ~1220s   | ~1255s     |

† markupsafe's `100.0` is **real** (killed 155 / skipped 111 /
survived 0 / errored 0) — a tiny, exhaustively-tested library, not
the misconfigured-venv false 100 described in the caveats. Even
werkzeug (~15k mutants) finishes in ~6 min — the coverage filter
keeps whole-repo runs tractable.

‡ trio's score is **volatile** — this run scored 53.9, an earlier run
63.7 (~10 pts apart). trio's async/timing-sensitive suite has tests
that pass or fail nondeterministically, so per-mutant verdicts (and
thus the aggregate) swing between runs. Read trio as "low-50s to
low-60s, unstable," not a point estimate. The other nine are stable
within ~1.3 pts run-to-run.

### The coverage filter is what makes fermut fast

fermut reads coverage.py's native `.coverage` SQLite DB (written by
`pytest --cov=src --cov-context=test`, and auto-discovered at the
project root) with per-test contexts, and runs **only the tests that
touch a mutated line** per mutant. Without it, every mutant runs the
full suite — the difference is large (figures from an earlier session;
the cold table above is canonical for the coverage-on absolutes):

- **pyjwt:** ~392s → **~47s** with coverage (~8×), and the score
  rises (68.0 → 81.9) because mutants on lines *no test covers* are
  now correctly **skipped** rather than counted as survivors.
- **typer:** ~2174s → **~214s** (~10×).

Coverage is **opt-in** (auto-discovered from `.coverage` at the project
root, or pass `--coverage .coverage`, or wire it via `fermut init`); the
benchmark harness now generates it automatically for fermut. Mutmut has no equivalent, so its per-mutant cost is the
full suite — which is also why it wins on a tiny-suite repo like
more-itertools where the full suite is already cheap.

### mutmut and cosmic-ray — cold + warm

Neither tool has a coverage filter, so both run the full suite per
mutant. mutmut copies the tree to `mutants/` and caches verdicts;
cosmic-ray drives a sqlite session.

The mutmut cold figures here are from this scenario's own (earlier)
session, so they sit a few seconds / ~0.2 pt off the canonical cold
table above (e.g. typer ~308s/70.3 here vs ~298s/70.5 there). Read this
table for the **cold-vs-warm shape** and the tool-failure notes; trust
the cold table above for absolute head-to-head numbers.

| Repo | mutmut cold | mutmut warm | cosmic-ray cold |
|------|-------------|-------------|-----------------|
| markupsafe | ~11s (70.7) | ~1.5s | ~330s (75.1) |
| pyjwt | ~45s (75.4) | ~32s | **DNF** (>15 min) |
| typer | ~308s (70.3) | ~233s | **DNF** |
| more-itertools | ~122s (90.2) | ~111s | **DNF** |
| click | fails† | — | **DNF** |
| starlette | fails† | — | **DNF** |

- **mutmut warm** is ~15–30 % faster than cold (its source-line cache
  short-circuits unchanged mutants). It works on most repos but
  **fails on click** (mutmut's own CLI imports click, so a mutated
  click breaks mutmut's bootstrap) and **starlette** (no verdicts
  recorded). † = no score produced.
- **cosmic-ray only completes the trivial suite** (markupsafe, ~330s
  for 812 mutants). Every real-world suite **does not finish inside a
  15-minute cap** — process-per-mutant over the full suite with no
  coverage filter doesn't scale. For comparison, fermut does pyjwt in
  ~42s and typer in ~164s (cold total). cosmic-ray's *warm* re-run, when the
  session is already complete, is near-instant (~0.4s on markupsafe)
  — but you have to survive the cold run first.

The takeaway across all three tools: **the coverage filter is the
thing that makes whole-repo mutation testing finish in seconds-to-
minutes instead of hours.** Without it (mutmut, cosmic-ray) the cost
is the full suite times every mutant.

### Warm and loop runs — the incremental story

This is where fermut's cache earns its keep. `warm` = an immediate
re-run, no edits. `loop` = 5 iterations of (touch source → re-run →
revert), averaged.

`Cold total` = coverage_prep + mutation_run (first-contact cost).
`Warm` / `Loop` reuse the populated cache + existing `.coverage` DB,
so the coverage_prep split doesn't apply to them.

| Repo (fermut)          | Cold total | Warm   | Loop (avg/iter) |
|------------------------|------------|--------|-----------------|
| typer 0.26.7           | ~164s      | ~8s    | ~8s             |
| pyjwt 2.13.0           | ~42s       | ~5.5s  | ~6s             |
| more-itertools v10.5.0 | ~304s      | ~11s   | ~11s            |

- **Warm is 8–26× faster than cold** — the per-mutant result cache
  short-circuits every unchanged mutant. (Earlier docs claimed the
  cache gave *no* speedup; that was a harness artifact — the `warm`
  scenario ran one pass without guaranteeing a hot cache, and a
  shared worktree let mutmut's pass invalidate fermut's. Both are
  now fixed: per-tool worktrees, and `warm` primes the cache before
  the timed pass.)
- **Loop ≈ warm.** The loop edit appends a comment, which leaves the
  AST untouched — so fermut's *structural* cache key (scope AST hash,
  not byte hash) stays valid and almost nothing re-evaluates. A
  behavior-preserving edit costs ~nothing; this is a best case. A
  real edit to a function body would invalidate that function's
  mutants and re-run just those.

Net: after the first cold run, a PR-time re-check lands in
**single-digit seconds** — which is the configuration fermut is
actually designed for.

### Closing the loop with an agent (`score_3pct`)

The `score_3pct` scenario drives a Claude Code subprocess to write
tests targeting the survivor list, re-runs mutation, and repeats
until the score climbs ≥ 3 points. It measures the *full* assisted
workflow, split into Claude's writing time (LLM-bound, tool-agnostic)
and the mutation re-run (the tool's cost).

The `Base` scores below are from this scenario's own (earlier) session,
so they can sit ~0.3 pt off the canonical cold table above — read the
**Δ** here, not the absolute baseline.

| Repo | Tool | Base → Final (Δ) | Claude / iter | Re-run / iter |
|------|------|------------------|---------------|---------------|
| pyjwt | fermut | 81.9 → 85.3 (+3.4) | ~441s | **~15s** |
|       | mutmut | 75.4 → 80.9 (+5.5) | ~238s | ~34s |
| typer | fermut | 71.9 → 80.4 (+8.5) | ~535s | **~56s** |
|       | mutmut | 70.6 → 76.0 (+5.5) | ~573s | ~251s |
| more-itertools | fermut | 87.7 → 91.3 (+3.6) | ~337s | ~133s |
|       | mutmut | 90.2 → 95.3 (+5.1) | ~1099s | **~105s** |

All six reached the target in a single iteration. The
tool-dependent cost is the **re-run** column — what it costs to
re-measure the score after the agent adds tests:

- **typer:** fermut ~56s vs mutmut ~251s (~4.5×) — the coverage
  filter pays off most on the largest suite.
- **pyjwt:** fermut ~15s vs mutmut ~34s (~2.3×).
- **more-itertools:** mutmut ~105s vs fermut ~133s — mutmut wins on
  the tiny suite, same as cold.

Read these as **deltas and re-run times, not cross-tool scores** —
the two tools mutate different things and fermut's coverage filter
*skips* uncovered-line mutants, so baselines aren't comparable.
Claude's per-iteration time (238–1099s) is LLM variance, not a tool
signal. Note mutmut and cosmic-ray cache verdicts on source only, so
the harness wipes their cache each iteration — otherwise they'd
replay stale "survived" verdicts and never see the agent's new
tests; fermut's coverage-scoped cache invalidates the affected
mutants on its own.

## Reproducing locally

The harness lives under `benchmarks/`. Thin bash wrappers in
`benchmarks/scripts/` drive each scenario:

```sh
cd benchmarks

# The fermut wheel is built on demand by the adapter, content-addressed
# by git SHA under fixtures/.wheels/<sha>[-dirty-<digest>]/ — a dirty
# tree rebuilds automatically. You only need `maturin` on PATH
# (`pipx install maturin`); no manual pre-build step.

# One scenario, one tool, one repo.
./scripts/run_cold.sh fermut more-itertools
./scripts/run_cold.sh mutmut more-itertools

# Everything (hours — cold first so warm/loop/score have fixtures).
./scripts/run_all.sh

# Aggregate results/runs/*.json into a report.
./scripts/report.sh
```

Each `run_*.sh` takes `[tool] [repo]` (names, comma-lists, or `all`);
under the hood they call `uv run fermut-bench run --scenario … `.

Repos and tools are configured in `benchmarks/configs/repos.toml`
and `benchmarks/configs/tools.toml`. Results land in
`benchmarks/results/runs/` as one JSON per (scenario, tool, repo).

## Caveats — read before trusting the numbers

- **Preliminary data.** This is a small, in-progress sample, not a
  finished benchmark. fermut is at 0.4.1, pre-optimization.
- **Score parity.** The tools find different mutants because
  operator catalogues differ. Scores are **not** directly
  comparable. A fermut `100.0` on a well-tested repo is a red flag,
  not a result — see the ✗ note above.
- **Benchmark-harness bugs found while validating these numbers**
  (these were harness/setup issues, not fermut runtime bugs). Now
  fixed: per-repo venvs could miss `pytest` (→ every mutant errors →
  fake 100% — the harness now installs pytest and asserts it imports
  post-install, failing loudly otherwise); each (tool, repo) gets
  its **own** worktree so one tool's mutation pass can't invalidate
  another's cache; the `warm` scenario now runs an un-timed priming
  pass before the timed one. Stale venvs/worktrees from older configs
  (e.g. a `fastapi` venv) are cleared with `./scripts/clear.sh
  --fixtures`. One genuine fermut runtime bug was also fixed: a
  single-file target (`fermut run path/to/file.py`) anchored its
  `.fermut/` cache under the file and silently disabled caching.
- **Coverage-file size.** The legacy `coverage json --show-contexts`
  export on a large suite is huge (typer's `coverage.json` came out
  ~3.9 GB). It works but won't scale. This is a large part of why the
  happy path now reads coverage.py's native `.coverage` SQLite DB
  directly — it is far smaller (typer's is a fraction of the JSON, no
  denormalized text blob) and needs no export step. Scoping coverage to
  the package (excluding test files / big fixtures) shrinks it further.
- **Comparison coverage.** fermut is measured coverage-on across all
  ten repos. mutmut cold+warm covers markupsafe, pyjwt, typer,
  more-itertools (click/starlette fail, trio bails after ~3.8h).
  cosmic-ray completes only markupsafe within a 15-min cap. So the
  larger repos are fermut-only on the comparison axis — not because
  fermut's numbers are unverified, but because the other tools can't
  run them.
- **cosmic-ray adapter was broken** until this round: it scored from
  `cosmic-ray dump` (raw work-items, no aggregate) instead of
  `cr-report`, so it never produced a number. Now fixed (uses
  cr-report, converts survival→kill %, reuses the session on warm).
- **`warm`** is measured for fermut (3 repos) and mutmut (4 repos);
  cosmic-ray warm only where the cold session completed (markupsafe).
  **`loop`** is fermut-only so far. **`score_3pct`** covers both
  fermut and mutmut on three repos (see *Closing the loop*).
- **Loop is a best case.** Its edit is a comment (AST-stable), so it
  measures the cache-friendly path, not a logic change.
- **First-time setup cost** (cosmic-ray's sqlite session, mutmut's
  cache dir) is excluded from the mutation-run column.

## See also

- **[Landscape](../concepts/landscape.md)** — feature comparison
  and motivation.
- **[Filters](../guides/filters.md)** — how to compose the
  filters that shape fermut's mutant set.
- **[Caching](../concepts/caching.md)** — the per-mutant cache
  design (measured ~35× on a clean back-to-back pyjwt re-run).
