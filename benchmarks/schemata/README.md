# Mutant schemata — evaluation vs spawn-per-mutant

**Verdict (2026-10-25): GO.** A function-level schema + a warm pytest fork
server that collects once matched fermut's verdicts with **0 stable diffs on 5
repos** (5 fermut runs × 3 spike runs each, 12.5k executed mutants). It ran
mutants **~6–12× cheaper each** and **1.2–5.7× faster end to end** with a fixed
10 s timeout; the low end is markupsafe, where 4 timeouts are nearly all of the
runtime. Phase 0's 1.6–2.5× projection was too low: it assumed each fork still
re-collects, and collect-once removes that cost. Getting to 0 diffs took six
fixes, listed under *Requirements for a real runner* — each one was a real bug.

## Scripts

| script | purpose |
|---|---|
| `startup.py <pkg> <nodeid>` | cold spawn of one test vs warm-template fork (re-collect, and after a template collect). Run with the repo venv from the repo root. |
| `scope.py <report.json> [--executed]` | Phase 0: bucket mutants by enclosing scope. |
| `project.py <report.json> '<startup json>' [--collected]` | Phase 0: speedup projection from per-mutant `duration_ms` (subtract model exact only for `--jobs 1`). |
| `schematize.py <ref.json> <repo> <mirror>` | build the schematized mirror (run with the repo venv — it byte-compiles). |
| `forkserver.py <ref.json> <repo> <mirror> <src-root> <tests> <out.json> --collect-once` | run the schema mutants by fork; `--only ID --debug` for triage. |
| `compare.py --ref ref-*.json --spike spike-*.json` | verdict parity (detected = killed/timed out), stable vs flaky diffs. |
| `summarize.py <batch.log> repo...` | speed + parity table. |

Reproduce one repo (fixtures as in `benchmarks/`; `results/` is gitignored):

```sh
cd benchmarks/fixtures
fermut run jwt --tests tests --coverage "$PWD/fermut-pyjwt/coverage.json" \
  --no-cache --no-history --timeout 10 --json ../schemata/results/ref-pyjwt-1.json   # from fermut-pyjwt/
.venvs/fermut-pyjwt/bin/python ../schemata/schematize.py ../schemata/results/ref-pyjwt-1.json fermut-pyjwt schemata-pyjwt
.venvs/fermut-pyjwt/bin/python ../schemata/forkserver.py ../schemata/results/ref-pyjwt-1.json \
  fermut-pyjwt schemata-pyjwt jwt tests ../schemata/results/spike-co-pyjwt-1.json --collect-once --timeout 10
python3 ../schemata/compare.py --ref ../schemata/results/ref-pyjwt-*.json --spike ../schemata/results/spike-co-pyjwt-*.json
```

## Linux parity (2026-11-04, backlog #40)

`linux/run.sh setup && linux/run.sh parity 3 && linux/run.sh compare`:
Ubuntu 24.04 container (Docker Desktop, aarch64, 14 CPUs, 8 GB, `JOBS=8`),
build 854a4da6db, Python 3.13, venvs pinned to the macOS fixtures
(`linux/pins/`, refreshed 2026-11-04). 10 repos × 3 rounds × ref/schemata,
`--timeout 10`.

| repo | executed | ref score | sch score | stable diffs | flaky r/s | ref wall | sch wall | speedup |
|---|---|---|---|---|---|---|---|---|
| click | 1409 | 78.7 | 78.7 | 0 | 0/0 | 70s | 26s | 2.7× |
| flask | 788 | 74.2 | 74.2 | 0 | 0/0 | 48s | 18s | 2.7× |
| jinja2 | 1911 | 76.2 | 76.2 | 0 | 0/0 | 111s | 30s | 3.7× |
| markupsafe | 217 | 66.8 | 66.8 | 0 | 0/0 | 12s | 14s | 0.9× |
| more-itertools | 3326 | 85.6 | 85.6 | 0 | 0/0 | 209s | 114s | 1.8× |
| pyjwt | 1739 | 78.9 | 78.9 | 0 | 0/0 | 87s | 33s | 2.6× |
| starlette | 5960 | 79.4 | 79.4 | 0 | 0/0 | 469s | 203s | 2.3× |
| trio | 2330 | 52.7–52.8 | 52.7 | **1** | 1/1 | 285s | 224s | 1.3× |
| typer | 1597 | 69.1 | 69.1 | 0 | 0/0 | 220s | 176s | 1.3× |
| werkzeug | 2487 | 72.0–72.1 | 72.1 | 0 | 1/0 | 147s | 55s | 2.7× |

Walls are medians. Total 1658 → 893 s (1.9×).

- **The one stable diff** (trio `_core/_thread_cache.py@5793`, `daemon=True`
  dropped): ref `timed_out` ×3, schemata `survived` ×3. The worker threads turn
  non-daemon, so a cold pytest hangs at interpreter exit waiting for them; a
  forked child leaves with `os._exit` and never waits. Backlog #61. (macOS
  sampled this mutant out, so it never showed there.)
- 79 trio mutants are `error` in both modes alike: pytest 9.2 exit 3 when
  `-p trio._tests.pytest_plugin` imports the broken package (backlog #59,
  fixed after this run in 41614e4361: 75 of 76 become kills).
- starlette deselects `test_staticfiles_with_invalid_dir_permissions_returns_401`
  when run as root (root ignores `chmod`; `run10.sh`).
- A first attempt was discarded: the Mac slept mid-run, which pauses the VM
  (trio schemata 1131 s, 15-min mutant "timeouts"). `linux/run.sh` now runs
  under `caffeinate`.

Apps (`linux/run.sh apps 3`, 4 workers: at the default 14 the suites OOM the
8 GB VM). Sources copied into the volume (`COPYFILE_DISABLE=1`, or macOS tar
adds `._*` AppleDouble files pytest collects), venvs pinned to the macOS ones
(Python 3.12; the private `kovant-*` wheels re-zipped from the uv cache, no
index credentials in the container), coverage via `fermut coverage --full`
with `runapps.sh`'s marker filter and deselects. agent-library needs
`libmagic1` (Dockerfile).

| app | executed | ref score | sch score | stable diffs | flaky | ref wall | sch wall | speedup |
|---|---|---|---|---|---|---|---|---|
| agent-library | 379 | 71.2 | 71.0 | 1 | 0/0 | 891s | 556s | 1.6× |
| apb-mutation | 590 | 85.2 | 84.7 | 3 | 0/0 | 1076s | 237s | 4.5× |

All 4 stable diffs are ref `timed_out` / schemata `survived` at the
`--timeout 30` border: ref hung inside an ordinary test after ~21 s of test
time plus ~9 s of cold start; the warm fork finished the same selection in
23–28 s. A budget flip, not a semantic diff (macOS saw the same kind on apb,
`usage_telemetry.py@1807`). agent-library is 1.6× faster on Linux where macOS
was slower than ref (fork-unsafe frameworks, #43), as #40 predicted.

## Phases 1–3 results (2026-10-25)

### How the spike works

- **Schema** (`schematize.py`): each mutant in a function body becomes a `case`
  of a `match` *inside its innermost function*, gated by that function's
  contiguous index range: `if lo <= ACTIVE <= hi: match ... else: <original>`.
  Because the switch is inside the function, the function object, its
  `__class__` cell, closures, decorators and generator/async-ness don't change,
  so the async (39% of starlette), generator (39% of more-itertools) and
  zero-arg `super()` cases Phase 0 worried about needed no special code.
  Fallback to cold spawn applies to: module/class/signature mutants, mutants
  that change the docstring, and mutants whose source raises a compile-time
  warning.
- **Fork server** (`forkserver.py --collect-once`): one real pytest session
  imports and collects the whole suite. Its `pytest_runtestloop` then forks one
  child per mutant. Each child sets `ACTIVE`, re-runs
  `pytest_collection_modifyitems` on its selection, and runs the items through
  `pytest_runtest_protocol` with `-x` semantics. Test selection replicates
  fermut's: coverage line → statement-line fallback, file-local then global
  breadth, kill-order lift.

### Parity (Phase 2 gate)

| repo | executed | schema | cold fallback | stable diffs | flaky fermut / spike |
|---|---|---|---|---|---|
| pyjwt | 1682 | 1591 | 91 (5%) | **0** | 0 / 0 |
| markupsafe | 211 | 168 | 43 (20%) | **0** | 0 / 0 |
| typer | 1507 | 1352 | 155 (10%) | **0** | 63 / 5 |
| starlette | 5862 | 5515 | 347 (6%) | **0** | 87 / 7 |
| more-itertools | 3239 | 3199 | 40 (1%) | **0** | 0 / 0 |

5 fermut × 3 spike runs per repo. fermut is the flakier side: typer/starlette
survivors flip to `timed_out` under load, and typer has tests sensitive to how
long the tmp path is (see below).

### Speed

Interleaved fermut/spike pairs, `--timeout 10`, default jobs. The spike time is
template + mutant phase. Its Python prelude (parsing a coverage JSON of up to
3.9 GB) is fermut's Rust work and is already inside fermut's time.

| repo | fermut | projected schemata runner¹ | speedup | per-mutant, non-timeout² |
|---|---|---|---|---|
| pyjwt | 99 s | 21 s | 4.7× | 741 → 76 ms (9.7×) |
| markupsafe | 19 s | 16 s | 1.2× | 530 → 11 ms (48×) |
| typer | 404 s | 149 s | 2.7× | 2644 → 410 ms (6.5×) |
| starlette | 840 s | 199 s | 4.2× | 1712 → 198 ms (8.6×) |
| more-itertools | 283 s | 95 s | 3.0× | 919 → 95 ms (9.7×) |

¹ `fermut list` time + baseline suite run + spike + cold cost of the fallback
mutants at fermut's own throughput. ² Mean wall per killed/survived mutant
under parallel load.

**Read with care:**
- These fermut times are inflated: process *exec* got much slower on this
  machine over the session (pyjwt 52 s on day 1, 99 s here), and the spike,
  which forks without exec, barely moved. Cleaner pyjwt numbers: day-1
  end-to-end 52 s vs 9 s (5.7×), and unloaded (`--jobs 1`) 183 ms vs 15 ms
  per mutant (12×).
- Timeouts now dominate the schemata wall time: more-itertools has 85 × 10 s,
  starlette 82, markupsafe 4 (that one is markupsafe's whole 11 s). Schemata
  can't speed those up, so pair it with an adaptive timeout (`--timeout-factor`,
  backlog #34).
- typer: fermut timed out 64–69 survivors under load that the spike finished
  well inside 10 s.

### Requirements for a real runner (each was a real false kill/survivor here)

1. **Switch at the innermost function.** Decorator factories (starlette
   `requires`) build closures while test modules import, which happens in the
   template. With the switch at the outermost function those closures froze
   the original code: 51 false survivors.
2. **Mutants whose function ran before the fork go to cold spawn.** Profile the
   template (SUT import, conftest, collection); any switched function that ran
   may have stored original-code results (registries, caches, built objects).
   This sent 290 starlette mutants to cold spawn (5%), 82 on pyjwt, 117 on typer.
3. **Re-run `pytest_collection_modifyitems` on each child's selection.** A cold
   run reorders items by parametrized higher-scope fixtures (`anyio_backend`);
   without that, module-fixture teardown moved and exposed an order dependency
   in starlette's tests: 101 false kills.
4. **Mutants that warn at compile time go to cold spawn.** Under
   `filterwarnings=error`, one mutant's `SyntaxWarning` (`x is ''`) becomes an
   import error that breaks every mutant in the file.
5. **Give each child a private pytest basetemp with the *same path length* as a
   cold run.** Siblings sharing `pytest-of-<user>` prune each other's
   `tmp_path`. A shorter or longer path flips typer tests that assert on
   80-column-wrapped error text containing the path (66 diffs). The spike uses
   a same-length fake `USER` and seeds the numbered dir. fermut has the same
   issue: its own typer verdicts shift when pytest's counter goes from
   `pytest-9999` to `pytest-10000`.
6. **Export the active mutant to subprocesses** (`FM_ACTIVE` env), byte-compile
   the schema (it never changes during a run, so `.pyc` can't go stale), and
   `_exit` straight after the loop; pytest's own session wrap-up once took
   ~17 min.

Also: the schema roughly multiplies source size by N. That cost import time
when tests spawn Python (typer suite 52 s → 191 s before byte-compiling) and
call time before the range gate (`match` checks its cases one by one).

### Phase 3 decision

GO. The plan's bar (≥2× on ≥2 repos with a clean parity gate) is met on 4 of 5
repos. Next step: a Rust runner plan — a schema emitter beside `src/emit.rs`, a
Python fork-server helper started by a new `Runner` impl beside
`src/runner/pytest.rs`, opt-in flag first. It should carry the six
requirements above and the timeout pairing.

## Phase 0 results (2026-10-24, M-series mac, py3.13, fermut 0.6.1 release)

### Fixed cost per test (median of 9)

| repo | spawn 1 test | fork + re-collect | fork after template collect | k | k collected |
|---|---|---|---|---|---|
| pyjwt | 108 ms | 35 | 29 | 3.1 | 3.8 |
| markupsafe | 80 | 29 | 22 | 2.8 | 3.7 |
| more-itertools | 88 | 34 | 23 | 2.6 | 3.8 |
| typer | 95 | 37 | 25 | 2.6 | 3.8 |
| starlette | 132 | 82 | 40 | 1.6 | 3.3 |

~25–30 ms floor is `pytest.main` session setup itself. Starlette needs the
template to pre-import test modules (httpx etc.) to get any win.

### Schematizable share of *executed* mutants

| repo | executed | func | async | generator | super() | nested | other decorator | signature/class |
|---|---|---|---|---|---|---|---|---|
| pyjwt | 1682 | 100% | – | – | – | – | – | – |
| markupsafe | 211 | 55% | – | – | 25% | – | – | 20% |
| more-itertools | 3241 | 55% | – | 39% | 0.2% | 6% | 0.6% | 0.2% |
| typer | 1507 | 80% | – | – | 13% | 6% | – | – |
| starlette | 5862 | 55% | 39% | – | 5% | 1% | 0.1% | – |

Module/class-level mutants never reach execution (coverage filter drops them),
so the "19% spawn fallback" estimate was wrong: fallback is ~0 except
markupsafe's default-arg/class-body mutants. But **async, generators and
zero-arg `super()` are 20–45% of executed mutants** on 4 of 5 repos — a spike
that skips them caps out ≤1.5×.

### Per-mutant ground truth (pyjwt, `--jobs 1`)

p50 143 ms, mean 205 ms, mean test time 53 ms. Spawn floor 108 ms → the rest of
the non-test time is collecting the selected test modules per spawn.

- floor (only interpreter + pytest import removed): **1.54–1.62×**
- ceiling (all non-test overhead removed, fork floor kept): **2.3–2.5×**

Under parallel load (default `--jobs`) per-mutant p50 inflates to 234 ms, so
subtract-model numbers from loaded runs are pessimistic — use `--jobs 1` runs.

### Projected mutant-phase speedup (Amdahl, pyjwt's 1.6–2.5× on the schematizable share)

| repo | first cut (plain functions only) | with async/generator/super/nested |
|---|---|---|
| pyjwt | 1.6–2.5× | 1.6–2.5× |
| markupsafe | 1.26–1.5× | 1.43–1.93× |
| more-itertools | 1.26–1.49× | 1.6–2.49× |
| typer | 1.43–1.93× | 1.6–2.5× |
| starlette | 1.26–1.49× | 1.6–2.5× |

Timeouts (`--timeout 10`: markupsafe 4, starlette 75, more-itertools 88) are
unchanged by schemata and are included in these numbers.

### External check: mutmut 3.6 (trampoline + fork) on same machine

From `benchmarks/results/runs/*cold-mutmut-*` (June) vs fermut today, wall per mutant:
pyjwt 28 vs 31 ms, more-itertools 33 vs 43 ms, typer 142 vs 121 ms.
A mature schemata runner is within ±30% of fermut's spawn path end to end —
consistent with the ~1.5–2.5× mutant-phase band, not the earlier 2.4–3.7×.
(Not apples to apples: different mutant sets, mutmut counts include its own
stats pass and generation.)

### Phase 0 verdict (superseded by Phases 1–3)

Not killed — clears 1.5× on every repo **only if** async, generator and
zero-arg `super()` trampolines are in scope. Phase 1 must include them.
