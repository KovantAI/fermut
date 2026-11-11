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

The numbers below are `cold`-scenario runs of fermut 0.7.0 (pre-release
`main`, 2026-11-03) vs mutmut 3.8.1, **both re-run back-to-back on the same
machine (2026-11-03)**, excluding clone / install. fermut runs its
defaults: coverage filter on, [schemata](cli/run-schemata.md) on,
`--timeout-factor 3`, the `minimal` operator profile. mutmut runs its
default.

> **This table is canonical.** It's the most recent same-session
> re-measurement, so trust it for head-to-head scores and timings.
> Other figures further down (the coverage-filter before/after
> illustration, cosmic-ray, the `score_3pct` agent-loop baselines) come
> from *earlier* measurement sessions (June 2026, before schemata and the
> current operator catalogue) and are kept for their before/after
> *deltas*, not as absolute numbers.

fermut's cold cost splits into two phases the harness times separately:
**`coverage_prep`** (the one-time baseline coverage run, writing
coverage.py's `.coverage` DB, reused by every later run) and
**`mutation_run`** (the mutation work itself). The comparable cold total
a user waits on first contact is the sum. mutmut has no coverage phase,
so its number is the whole run.

`Scored` is killed + survived + timed out (what the score is computed
over); fermut generates more mutants than it scores, because the coverage
filter and `ty` skip the rest.

| Repo (ref)             | Tool   | Scored | Score | cov_prep | mutation | Cold total |
|------------------------|--------|--------|-------|----------|----------|------------|
| typer 0.26.7           | fermut | 1597   | 69.2  | ~60s     | ~147s    | **~211s**  |
|                        | mutmut | 2118   | 70.4  | —        | —        | ~289s      |
| pyjwt 2.13.0           | fermut | 1730   | 78.8  | ~7s      | ~43s     | ~53s       |
|                        | mutmut | 1621   | 75.4  | —        | —        | **~46s**   |
| more-itertools v10.5.0 | fermut | 3326   | 85.6  | ~5s      | ~115s    | ~123s      |
|                        | mutmut | 3651   | 90.2  | —        | —        | **~119s**  |
| markupsafe 3.0.3       | fermut | 217    | 66.8  | ~2s      | ~8s      | ~15s       |
|                        | mutmut | 249    | 70.7  | —        | —        | **~9s**    |

The picture is **suite-shaped**:

- **typer:** fermut is faster (~211s vs ~289s; ~2× on the mutation phase
  alone, ~147s vs ~289s) at a similar score (69.2 vs 70.4).
- **pyjwt:** about even (~53s vs ~46s), fermut scoring higher (78.8 vs
  75.4).
- **more-itertools:** a tie on time (~123s vs ~119s); mutmut scores
  higher (90.2 vs 85.6). In June mutmut was ~2.6× faster here (~118s vs
  ~304s); schemata closed that gap.
- **markupsafe:** a tiny suite, where mutmut's simpler model still wins
  (~9s vs ~15s).

Because `coverage_prep` is a **one-time** cost, the *steady-state* cost is
the `mutation` column; the cold totals are the conservative first-contact
figure.

### A mutmut-scale repo: werkzeug

The four repos above are small libraries. werkzeug 3.1.8 is closer to the
size a team migrating off a long mutmut nightly runs: 21.5k lines of
source, 10.2k lines of tests. Same machine, same session (2026-11-10):
Apple M4 Max (14 cores, 36 GB), Python 3.13.13, pytest 9.2.0, fermut 0.7.0
(`main` e6579ce448) vs mutmut 3.8.1, clone and install excluded.

| Run                      | fermut                            | mutmut  |
|--------------------------|-----------------------------------|---------|
| Cold                     | ~146s (~6s coverage + ~140s run)  | ~155s   |
| Warm, nothing changed    | **~1.5s**                         | ~12s    |
| Cold, 4 shards (wall)    | **~56s** (slowest shard + merge)  | —       |
| Cold, 4 shards (summed)  | ~206s machine time                | —       |
| Scored mutants           | 7982 (0 errored)                  | 10511   |
| Score                    | 71.2                              | 68.1    |

- **Cold is a tie.** On one machine with every core, fermut and mutmut
  take about the same time to do a first full pass.
- **Warm is ~8× faster.** fermut's cache keys each verdict by the code,
  settings and covering tests, so an unchanged re-run only checks
  that nothing moved.
- **Sharding cuts wall time to ~1/3.** `--shard i/4` across four runners
  plus `fermut merge` gives the same 7982 scored mutants and the same
  score as the single run. Each shard started from an empty cache,
  the way a fresh CI host would. The four ran one after another here, so
  "wall" is the slowest shard plus the merge, which is what you'd wait
  for in CI. mutmut has no sharding.
- **Different mutant sets.** The scored counts and scores aren't the
  same population. fermut's `minimal` profile and coverage filter pick
  which mutants to run differently from mutmut's operators.
- The [full sweep](#full-fermut-sweep-coverage-on-cold) below shows
  werkzeug at 71.9 on the same 7982 mutants: it is an earlier session
  (2026-11-03), on an older `main`.

Fixture notes: werkzeug pins `coverage==7.17.0` (7.17.1+ under
`--cov-context=test` breaks `test_safe_join_windows_special`, which patches
`os.name`), and deselects `tests/test_serving.py` (live reloader servers
hang). mutmut also deselects four tests that fail its clean-stats pass
because they inspect frames and wrapped callables that mutmut's
trampolines change (`benchmarks/configs/repos.toml`). rich and click
couldn't be the mutmut-scale subject: mutmut imports both at startup, so
mutating them breaks mutmut itself.

### Application repos: healthchecks and flaskbb

werkzeug is a library with fast, isolated tests. These two are web apps
whose tests set up a database and an app for each test, measured the same
way on the same machine (2026-11-10). Each is **scoped to one package**,
with the rest of the package excluded from mutation for both tools; the
whole suite still runs.

- **healthchecks v5.0.3** (Django, SQLite test DB): `hc/api`, ~4.2k of
  the app's ~17k source lines; 1780 tests (~8s for the whole suite).
- **flaskbb v3.0.0** (Flask + SQLAlchemy, SQLite): `flaskbb/forum`, ~3.7k
  of ~24k source lines; 809 tests (~45s for the whole suite, one process).

| Run                     | healthchecks fermut       | healthchecks mutmut | flaskbb fermut           | flaskbb mutmut |
|-------------------------|---------------------------|---------------------|--------------------------|----------------|
| Cold                    | ~779s (~25s cov + ~754s)  | **~263s**           | ~254s (~54s cov + ~201s) | fails†         |
| Warm, nothing changed   | **~0.9s**                 | ~11s                | **~0.6s**                | —              |
| 4 shards, wall / summed | ~244s / ~919s             | —                   | ~105s / ~387s            | —              |
| Scored                  | 5292 (0 errored)          | 2117 (+465 no tests)| 1796 (0 errored)         | —              |
| Score                   | 72.7                      | 78.0                | 56.2                     | —              |

- **healthchecks: mutmut is ~3× faster cold, fermut scores ~2.5× more
  mutants.** Per scored mutant the two are close (~0.14s vs ~0.12s).
  In this run each of fermut's forks re-created the Django test database,
  because pytest-django sets it up in the first test, after the fork:
  ~2s per mutant per worker, against tens of milliseconds on werkzeug.
  fermut now builds the test DB once in the schemata template
  ([Django](../guides/frameworks.md#django)), which cut healthchecks to
  0.5 s per mutant; the table above predates that and hasn't been re-run.
- **Warm re-runs and sharding are where fermut pulls ahead**, as on
  werkzeug: under a second warm, and a quarter of the cold wall time on
  four runners.
- † **mutmut 3.8.1 can't run flaskbb.** Its stats pass succeeds, then
  its clean-test run errors in collection: `The setup method 'add_url_rule'
  can no longer be called on the blueprint '_themes'` (flask-themes2's
  module-level blueprint). The same suite passes under plain pytest. The
  likely cause is mutmut running pytest again in the same process with
  Flask's state still live. Not patched: that would benchmark a modified
  flaskbb.

Fixture notes (`benchmarks/configs/repos.toml`): healthchecks runs under
pytest-django with a `bench_settings.py` that re-applies its test-only
settings (upstream gates them on `manage.py test`), plus a `pytest.ini`
that is also its project-root marker. mutmut also copies `CHANGELOG.md` and
`search.db`, which the settings and search view read. flaskbb replaces
its xdist `addopts`, and deselects `test_flaskbbdomain_translations` for
both tools: it needs the compiled `*.mo` files, which `.gitignore` lists
and fermut's mirror dropped at the time. The mirror now keeps gitignored
files inside packages, so the deselect is no longer needed for fermut.

### Full fermut sweep (coverage-on cold)

All ten suite repos, `cold`, same session (2026-11-03), sorted by mutant
count. `Generated` counts every mutant fermut produced; `Scored` the ones
it ran. `Cold total` runs ~2–3 s above the sum of the two phases (time
spent outside them). `June total` is the June 2026 cold total, before
schemata (different operator catalogue, so mutant counts and scores
aren't comparable; wall-clock is).

| Repo (ref)             | Generated | Scored | Score | cov_prep | mutation | Cold total | June total |
|------------------------|-----------|--------|-------|----------|----------|------------|------------|
| markupsafe 3.0.3       | 1069      | 217    | 66.8  | ~2s      | ~8s      | ~15s       | ~9s        |
| typer 0.26.7           | 6414      | 1597   | 69.2  | ~60s     | ~147s    | ~211s      | ~164s      |
| pyjwt 2.13.0           | 6475      | 1730   | 78.8  | ~7s      | ~43s     | ~53s       | ~42s       |
| more-itertools v10.5.0 | 9451      | 3326   | 85.6  | ~5s      | ~115s    | ~123s      | ~304s      |
| flask 3.1.3            | 12541     | 2665   | 73.3  | ~4s      | ~50s     | ~58s       | ~155s      |
| starlette 0.52.1       | 20173     | 5962   | 79.5  | ~13s     | ~245s    | ~263s      | ~340s      |
| click 8.1.7            | 20651     | 6787   | 81.2  | ~4s      | ~116s    | ~124s      | ~235s      |
| jinja2 3.1.6           | 32420     | 6360   | 75.4  | ~6s      | ~150s    | ~159s      | ~296s      |
| trio v0.33.0           | 37004     | 7275   | ~53†  | ~8s      | ~684s    | ~696s      | ~1255s     |
| werkzeug 3.1.8         | 41146     | 7982   | 71.9  | ~7s      | ~155s    | ~165s      | ~338s      |

The large suites run **1.3–2.7× faster than in June** on schemata (flask,
click, jinja2, werkzeug, trio roughly halved; more-itertools 2.5×). The
small ones (markupsafe, pyjwt, typer) got a little slower: the default
operator catalogue now generates ~3× more mutants on them, and their
suites are too short for the fork server to pay off as much. markupsafe's
June `100.0` was a smaller catalogue; the current one finds survivors.

† trio's score is **volatile** (53.4–53.5 in two runs today; June saw
53.9 and 63.7). trio's async/timing-sensitive suite has tests that pass or
fail nondeterministically, so per-mutant verdicts swing between runs.

Fixture notes for this session (`benchmarks/configs/repos.toml`): flask and
click pin `pytest<9` (pytest 9.2 removed an internal flask's tests import
and deprecates click's parametrize iterator, which click runs as an error);
starlette, typer, flask, trio and werkzeug deselect the tests that fail
their own baseline in this environment or hang (werkzeug's
`tests/test_serving.py`), the same set the schemata parity runs use.

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

Coverage is used whenever a `.coverage` DB exists: auto-discovered at
the project root, or passed with `--coverage .coverage`. `fermut run`
doesn't create one; `fermut coverage` (or `fermut baseline`) does, and
the benchmark harness runs that step for fermut (the `cov_prep` column).
Without a DB, every mutant runs the full suite. mutmut selects tests
too, per mutated function rather than per line, and has no separate
coverage phase — which is why it wins on a tiny-suite repo like
markupsafe, where fermut's one-time coverage and `ty` overhead doesn't
pay off.

### The `ty` filter { #ty-filter }

The `ty` filter runs after the coverage filter, so its share is measured
against the mutants that would otherwise run: `ty` drops ÷ (`ty` drops +
scored). From the default-config runs in the memory-cap A/B
(2026-11-01, Linux, schemata on, `benchmarks/schemata/results/memcap/`).
That's a different session from the cold table, so some `Scored` counts
differ from it (click, werkzeug, jinja2, flask, trio); read the share,
not the counts:

| Repo           | Scored | Dropped by `ty` | Share   |
|----------------|--------|-----------------|---------|
| more-itertools | 3326   | 1305            | 28%     |
| click          | 1408   | 636             | 31%     |
| markupsafe     | 217    | 105             | 33%     |
| typer          | 1597   | 909             | 36%     |
| werkzeug       | 2486   | 1407            | 36%     |
| pyjwt          | 1739   | 1017            | 37%     |
| jinja2         | 1911   | 1108            | 37%     |
| flask          | 788    | 473             | 38%     |
| trio           | 2340   | 1475            | 39%     |

So roughly **30–40% of the covered mutants never reach pytest**. On
application suites outside this set the share has ranged from 12% to
53%. Each drop saves one mutant's test run. The filter's own cost isn't
broken out separately: it's part of the `mutation` column above.

### mutmut and cosmic-ray — cold + warm

cosmic-ray has no coverage filter, so it runs the full suite per mutant
and drives a sqlite session. mutmut copies the tree to `mutants/`, runs
the tests that reach each mutated function, and caches verdicts.

This table is from the June 2026 session (mutmut 3.6; cosmic-ray wasn't
re-run). Read it for the **tool-failure notes** and cosmic-ray's scaling;
trust the cold and warm tables for current mutmut numbers (mutmut 3.8.1
today: typer ~289s cold / ~64s warm, pyjwt ~46s / ~5s).

| Repo | mutmut cold | mutmut warm | cosmic-ray cold |
|------|-------------|-------------|-----------------|
| markupsafe | ~11s (70.7) | ~1.5s | ~330s (75.1) |
| pyjwt | ~45s (75.4) | ~32s | **DNF** (>15 min) |
| typer | ~308s (70.3) | ~233s | **DNF** |
| more-itertools | ~122s (90.2) | ~111s | **DNF** |
| click | fails† | — | **DNF** |
| starlette | fails† | — | **DNF** |

- **mutmut warm** is ~9–86 % faster than cold (about 25–30 % on pyjwt and typer) (its source-line cache
  short-circuits unchanged mutants). It works on most repos but
  **fails on click** (mutmut's own CLI imports click, so a mutated
  click breaks mutmut's bootstrap) and **starlette** (no verdicts
  recorded). † = no score produced.
- **cosmic-ray only completes the trivial suite** (markupsafe, ~330s
  for 812 mutants). Every real-world suite **does not finish inside a
  15-minute cap** — process-per-mutant over the full suite with no
  coverage filter doesn't scale. For comparison, fermut does pyjwt in
  ~53s and typer in ~211s (cold total, 2026-11-03). cosmic-ray's *warm* re-run, when the
  session is already complete, is near-instant (~0.4s on markupsafe)
  — but you have to survive the cold run first.

The takeaway across all three tools: **the coverage filter is the
thing that makes whole-repo mutation testing finish in seconds-to-
minutes instead of hours.** Without it (mutmut, cosmic-ray) the cost
is the full suite times every mutant.

### Warm and loop runs — the incremental story

`warm` = an immediate re-run, no edits. `loop` = 5 iterations of (touch
source → re-run → revert), averaged. Both reuse the populated cache and
the existing `.coverage` DB. Same session as the cold table (2026-11-03).

| Repo                   | fermut cold | fermut warm | fermut loop (avg/iter) | mutmut warm |
|------------------------|-------------|-------------|------------------------|-------------|
| typer 0.26.7           | ~211s       | ~61s        | ~60s                   | ~64s        |
| pyjwt 2.13.0           | ~53s        | ~6s         | ~6s                    | ~5s         |
| more-itertools v10.5.0 | ~123s       | ~2s         | ~2s                    | ~3s         |
| markupsafe 3.0.3       | ~15s        | —           | —                      | ~1s         |

- **The warm column predates a change: a fully cached run now skips the
  baseline check.** In this session every verdict came from the cache, but
  fermut still ran the unmutated suite once first (`verify_baseline`):
  ~57s of typer's ~61s, ~3s of pyjwt's. Since then, a run where every
  mutant is a cache hit runs no tests at all, baseline included. Re-measured
  on pyjwt (2026-11-04, a different machine from the table): **~38s cold,
  ~0.3s warm**.
- **An edit costs one coverage refresh plus the affected mutants.** Same
  pyjwt session, one statement added to a function body: `fermut coverage`
  re-ran the 6 affected test files in ~5s, then the run took ~36s. Skip
  the refresh and the stale `.coverage` is ignored: the run took ~311s and
  reported almost twice the survivors, because every mutant ran the whole
  suite and uncovered lines counted. With `--diff-only HEAD` (the agent
  loop), the same edit took ~11s, nearly all of it the baseline check.
- **Loop ≈ warm in the table.** The harness's loop edit appends a comment, which leaves the AST
  untouched, so fermut's *structural* cache key (scope AST hash, not byte
  hash) stays valid and nothing re-runs. This is a best case; a real edit
  to a function body re-runs just that function's mutants.

Net: after the first cold run, a PR-time re-check costs about one run of
the test suite plus the mutants the change touched.

### Closing the loop with an agent (`score_3pct`)

The `score_3pct` scenario drives a Claude Code subprocess to write
tests targeting the survivor list, re-runs mutation, and repeats
until the score climbs ≥ 3 points. It measures the *full* assisted
workflow, split into Claude's writing time (LLM-bound, tool-agnostic)
and the mutation re-run (the tool's cost).

The `Base` scores below are from the June 2026 session, under the older
operator catalogue, so they don't match the cold table above — read the
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
  finished benchmark. fermut is 0.7.0 (pre-release `main`,
  2026-11-03), with schemata on by default.
- **Score parity.** The tools find different mutants because
  operator catalogues differ. Scores are **not** directly
  comparable. A fermut `100.0` on a well-tested repo is a red flag,
  not a result — see the missing-`pytest` harness bug below.
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
  The harness itself exported JSON until 2026-11-03; it now hands fermut
  the `.coverage` DB (pinning `COVERAGE_FILE`, since typer's own config
  writes it to `coverage/.coverage`).
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
  cosmic-ray warm only where the cold session completed (markupsafe, June).
  **`loop`** is fermut-only so far. **`score_3pct`** covers both
  fermut and mutmut on three repos (see *Closing the loop*), June only:
  it drives a Claude Code subprocess and wasn't re-run.
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
