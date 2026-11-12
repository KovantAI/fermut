# Framework compatibility

How fermut behaves with the test frameworks and plugins teams ask about,
what we ran to find out, and the settings to use.

## Summary

| Framework / plugin | Verdict | Use |
|---|---|---|
| [Django / pytest-django](#django) | Works, both modes | Nothing for migrations (skipped by default); macOS + Postgres: `PGGSSENCMODE=disable` |
| [Flask (app factory)](#flask) | Works, both modes | Nothing; dirs the app creates on every start are fine |
| [FastAPI / Starlette `TestClient`, anyio](#fastapi) | Works, both modes | Fix order-dependent tests, or `--no-smart-order` with `--no-schemata` |
| [pytest-asyncio](#pytest-asyncio) | Forks work | Prefer `tmp_path` for files tests write ([below](#tests-that-write-into-the-repo)) |
| [trio](#trio) | Works, both modes | Pass the suite's own plugin flags as usual |
| [C extensions](#c-extensions) | Works, both modes | Build the extension in place before `fermut coverage` and `fermut run` |
| [Hypothesis](#hypothesis) | Works, both modes | Pin `hypothesis_seed` |
| [pytest-xdist](#xdist) | Works, both modes | `pytest_args = ["-n0"]` |

"Both modes" means schemata (forked runs, the default) and `--no-schemata`
(a fresh pytest per mutant) give the same verdicts. Tests that write files
inside the repo affect every framework; fermut repairs the damage, but they
cost speed under schemata ([at the end](#tests-that-write-into-the-repo)).

## How we tested

One representative repo per row, each run with `--schemata` and
`--no-schemata`, most of them twice, and the per-mutant verdicts compared.
A diff counts only when one run detected a mutant (killed or timed out) and
the other didn't. MacBook Pro M4 Max, macOS 26, Python 3.13, `--timeout 30`,
November 2026. The Starlette, trio and markupsafe rows are from a Linux
container (Ubuntu 24.04, 8 workers). The Flask row ran once per mode with
the time limit sized from the baseline (`--timeout-factor`), not
`--timeout 30`.

| Row | Repo | Mutants run | Detection diffs, schemata vs spawn | Wall, schemata / spawn |
|---|---|---|---|---|
| Django | healthchecks v5.0.3, `hc/api` | 9341 | 0 | 839 s / 1249 s |
| Flask | flaskbb v3.0.0, `flaskbb/forum` | 1796 | 9, all slow tests at the time limit (both directions) | 251 s / 325 s |
| FastAPI | fastapi 0.162.2, 10 % sample | 1125 | 0 apart from order-dependent tests (see below) | 670 s / 561 s |
| Starlette | starlette (benchmark pin) | 5960 | 0 (Linux, 3 rounds) | 203 s / 469 s |
| pytest-asyncio | aiofiles @9d1acc1 | 1664 | not measured: spawn runs still differ by 18, all one timing-flaky test | 29 s / 35 s |
| trio | trio (benchmark pin), 30 % sample | 2330 | 0 (Linux, 3 rounds) | 224 s / 285 s |
| C extension | markupsafe 3.0.3 | 217 | 0 (Linux, 3 rounds) | 14 s / 12 s |
| Hypothesis | attrs 26.1.0, 50 % sample | 1735 | 0 with a pinned seed (4 runs) | 138–150 s / 150–154 s |
| xdist | typer (benchmark pin), `-n 4` in addopts | 209 | 0 | 198 s / 196 s |

Their full numbers are in the
[schemata Linux parity run](https://github.com/KovantAI/fermut/tree/main/benchmarks/schemata#linux-parity-2026-11-04-backlog-40).

## Django / pytest-django { #django }

- **Test database.** pytest-django builds the test DB in a session fixture.
  Under schemata, fermut builds an in-memory SQLite test DB once in the
  template, and every fork gets its own copy. healthchecks: 0.5 s per
  mutant instead of 1.5 s, 0 diffs on 9341 mutants.
- **File SQLite, Postgres, MySQL.** Each child builds its own test DB. Each
  worker gets its own DB name (pytest-django's `TOX_PARALLEL_ENV` suffix),
  so `--jobs > 1` doesn't share one database. Expect about a second per
  mutant for the DB build. `--nomigrations` cuts that further, but it
  builds the schema from your models and skips your migrations.
- **macOS + Postgres.** libpq's GSSAPI setup crashes in forked children.
  fermut detects the crashes and runs those mutants the normal way, so
  verdicts are right but the forks are wasted. `export PGGSSENCMODE=disable`
  stops the crashes.
- **Migrations.** fermut skips Django migrations (`migrations/0001_*.py`
  in a package) and Alembic revisions (`versions/*.py` next to `env.py`) by
  default; they show as `migrations` in the run summary. In healthchecks
  they were 55 % of all mutants, 76 % of them survived, and they all run
  the slow way (class attributes run at import).
  `--mutate-migrations` (or `mutate_migrations = true`) mutates them anyway.

- **Transactional tests** (`TestCase`, `TransactionTestCase`) ran
  unchanged.

Against mutmut on the same scope, healthchecks' `hc/api` runs ~1.2×
faster cold and ~14× faster warm; see
[Benchmarks](../reference/benchmarks.md#application-repos-healthchecks-and-flaskbb).

## Flask (app factory) { #flask }

flaskbb builds a fresh app per test from `create_app`, against SQLite.
Forked runs work: 1719 of 1796 mutants in `flaskbb/forum` ran from the
schema, and the survivor audit agreed 45 of 45. The rest ran the normal
way: class-body mutants outside the schema, and kills by a few tests that
also fail in a fork with no mutant active (re-checked the normal way). Kills were identical to
`--no-schemata`; the only differences were 9 slow tests flipping between
survived and timed out at the time limit, in both directions.

- **Instance and upload dirs.** `create_app` calls
  `os.makedirs(app.instance_path, exist_ok=True)` and creates the upload
  dirs on every start. They're gitignored, so fermut's copy of the project
  starts without them. fermut counts whatever a passing baseline creates as
  part of the clean tree, so these don't stop forked runs
  ([below](#tests-that-write-into-the-repo)).
- **pytest-xdist in `addopts`.** flaskbb sets `--numprocesses auto`; see
  [xdist](#xdist).

Against mutmut, flaskbb's `forum` ties cold (~260 s each) and runs
~85× faster warm. mutmut needs `process_isolation = "forkserver"` (and
`OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES` on macOS) to run flaskbb at all;
see [Benchmarks](../reference/benchmarks.md#application-repos-healthchecks-and-flaskbb).

## FastAPI / Starlette `TestClient`, anyio { #fastapi }

FastAPI's `TestClient` is Starlette's, and both drive the app through anyio.
starlette's suite (anyio's pytest plugin, asyncio and trio backends) gave 0
diffs over 3 Linux rounds, 2.3× faster with schemata.

fastapi itself has **order-dependent tests**: `test_dependency_contextmanager`,
`test_testing_dependencies` and `test_events` assert on module-level state
that another test may already have changed. fermut runs the test most likely
to fail first ([smart ordering](../reference/cli/run.md#smart-test-ordering)),
not the suite's own order, and in that order these tests fail with no mutant
at all. They pass in collection order. Nothing about forking is involved:
the same test lists fail in a plain pytest run.

- **Schemata (default)** checks every kill by re-running the same tests
  with no mutant active. When that run fails too, the order is to blame,
  and fermut runs the mutant again in coverage order, close to the suite's
  own. On a 10 % sample of fastapi this changed 8 verdicts, all to what a
  run in collection order gives.
- **`--no-schemata`** has no such check, so an order-dependent test can
  record a kill that isn't the mutant's. `--no-smart-order` avoids that but
  isn't a cure: one fastapi test,
  `test_exception_is_logged_once_with_trace_context`, catches 61 mutants
  only when it runs first.

Fixing tests that depend on order is the real cure. Until then, keep
schemata on.

On fastapi, schemata isn't faster than spawning (670 s vs 561 s): 556 of
its 1793 mutants are default arguments and other code outside the schema,
which run the normal way in both modes.

## pytest-asyncio { #pytest-asyncio }

aiofiles (`asyncio_mode = "auto"`) ran in forks without trouble: 462
mutants served from the fork, 29 s vs 35 s spawning.

aiofiles' tests create, rename and delete files under `tests/resources/`.
Before fermut restored mirrors, two spawn runs of the same commit differed
by 112–176 verdicts. Now they differ by 18, all
`test_concurrency.py::test_slow_file`, a timing-sensitive test that also
fails in a fork with no mutant, and the false kills are gone (score 51 % →
45 %). See [Tests that write into the repo](#tests-that-write-into-the-repo).

## trio { #trio }

trio loads its own pytest plugin. Pass the suite's flags as usual
(`pytest_args = ["-p", "trio._tests.pytest_plugin", "--import-mode=importlib"]`
when they aren't already in `addopts`). No diffs in any of 3 Linux rounds.
One mutant used to differ: dropping `daemon=True` from a worker thread made
a normal run hang at exit while a forked child exited at once. Forked
children now wait for non-daemon threads too.

## C extensions { #c-extensions }

fermut mutates Python only. Compiled modules (`*.so`, `*.pyd`) are copied
into every worker's mirror as they are, even when `.gitignore` excludes
them.

**Build the extension in place, in the tree fermut runs from**, before
`fermut coverage` and `fermut run`. For example, run `pip install -e .`
with a backend that builds into the source tree, or
`python setup.py build_ext --inplace`. fermut puts its mirror first on
`PYTHONPATH`, so a copy installed only in `site-packages` is shadowed by
the mirror, which has no compiled file:

- If the package needs the extension, the baseline goes red and the
  `ImportError` names the missing module.
- If the package falls back to pure Python, as markupsafe does with
  `_speedups`, the extension's tests are **skipped without a warning**.
  With the extension removed, markupsafe passed 39 tests and skipped 42.

Rebuild after you change the C/Rust code.

## Hypothesis { #hypothesis }

Pin the seed:

```toml
hypothesis_seed = 12345
```

Unpinned, `@given` tests draw new examples every run. On attrs, 4 of 1735
verdicts changed between runs (`asdict`/`astuple` property tests). Pinned,
4 runs (2 per mode) agreed exactly at 85.6 %. `fermut doctor` warns when a
project uses Hypothesis without a pinned seed.

- **Example database.** `.hypothesis/` is not copied into worker mirrors.
  Each worker starts with an empty example DB, and your real one is never
  touched.
- **Deadlines.** fermut runs one mutant per CPU, so tests run slower than
  on an idle machine, and Hypothesis can fail a test for missing its
  deadline (a false kill). attrs uses a 400 ms deadline profile and had no
  deadline failures. If you see `DeadlineExceeded` in killed mutants,
  register a profile with `deadline=None` and select it for fermut only:
  `pytest_args = ["--hypothesis-profile=fermut"]`.

## pytest-xdist { #xdist }

fermut already runs one mutant per CPU, so xdist inside each mutant's run
adds processes and doesn't make the run faster. With `-n 4` in typer's
`addopts`, both modes gave the same verdicts as without it, and the wall
time barely changed (198 s vs 221 s for schemata). One mutant did differ: an
equivalent mutant whose surviving test run takes about 30 s timed out
serially and finished under xdist.

Turn xdist off for mutant runs:

```toml
pytest_args = ["-n0"]
```

`-n0` comes after `addopts`, so it wins. Verdicts with it matched the
no-xdist run exactly.

- **Coverage.** xdist breaks pytest-cov's per-test contexts. `fermut
  coverage` passes `-n0` when xdist is installed. If you build coverage
  with your own `pytest --cov` command, add `-n0`.
- **`PYTEST_ADDOPTS`** from your shell is ignored, on purpose: anything that
  changes how the suite runs has to live in the project config.

## Tests that write into the repo { #tests-that-write-into-the-repo }

Each worker reuses one mirror of your project for all the mutants it runs.
A test that creates, moves or deletes files **inside the repo** and undoes
it at the end of the test leaves the change behind when a mutant makes it
fail halfway. Left alone, later mutants on that worker would fail because
of the leftover file and count as killed.

fermut snapshots each mirror when it builds it and, after every mutant,
puts back files that were moved, deleted, rewritten or added, warning once
with the paths ([details](../reference/cli/run-isolation.md#tree-writes)).
On aiofiles, whose `test_rename` moves `tests/resources/test_file1.txt`
away and back, run-to-run diffs dropped from 128 to 18.

Files and dirs that a **passing** baseline run creates count as part of
the clean tree, not as leftovers: an app that creates its instance or
upload dirs on every start (Flask's `create_app`, above) is fine in both
modes.

Under schemata all forks share one mirror, which can't be put back while
other forks run. Once a kill leaves it changed, the rest of the mutants
run the normal way: verdicts stay right, the fork speedup is lost. Use
`tmp_path` (or `tmp_path_factory`) for anything a test writes.
