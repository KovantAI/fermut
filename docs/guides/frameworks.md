# Framework compatibility

How fermut behaves with the test frameworks and plugins teams ask about,
what we ran to find out, and the settings to use.

## Summary

| Framework / plugin | Verdict | Use |
|---|---|---|
| [Django / pytest-django](#django) | Works, both modes | `exclude = ["**/migrations/**"]`; macOS + Postgres: `PGGSSENCMODE=disable` |
| [FastAPI / Starlette `TestClient`, anyio](#fastapi) | Works, both modes | `--no-smart-order` if tests depend on order; on fastapi itself `--no-schemata` is faster |
| [pytest-asyncio](#pytest-asyncio) | Forks work | Tests must not write inside the repo ([below](#tests-that-write-into-the-repo)) |
| [trio](#trio) | Works, both modes | Pass the suite's own plugin flags as usual |
| [C extensions](#c-extensions) | Works, both modes | Build the extension in place before `fermut coverage` and `fermut run` |
| [Hypothesis](#hypothesis) | Works, both modes | Pin `hypothesis_seed` |
| [pytest-xdist](#xdist) | Works, both modes | `pytest_args = ["-n0"]` |

"Both modes" means schemata (forked runs, the default) and `--no-schemata`
(a fresh pytest per mutant) give the same verdicts. The one problem that
affects every framework, tests that write files inside the repo, is
[at the end](#tests-that-write-into-the-repo).

## How we tested

One representative repo per row, each run with `--schemata` and
`--no-schemata`, most of them twice, and the per-mutant verdicts compared.
A diff counts only when one run detected a mutant (killed or timed out) and
the other didn't. MacBook Pro M4 Max, macOS 26, Python 3.13, `--timeout 30`,
November 2026. The Starlette, trio and markupsafe rows are from a Linux
container (Ubuntu 24.04, 8 workers).

| Row | Repo | Mutants run | Detection diffs, schemata vs spawn | Wall, schemata / spawn |
|---|---|---|---|---|
| Django | healthchecks v5.0.3, `hc/api` | 9341 | 0 | 839 s / 1249 s |
| FastAPI | fastapi 0.162.2, 10 % sample | 1125 | 0 on a stable rerun (see below) | 675 s / 561 s |
| Starlette | starlette (benchmark pin) | 5960 | 0 (Linux, 3 rounds) | 203 s / 469 s |
| pytest-asyncio | aiofiles @9d1acc1 | 1664 | not measurable: the suite's own runs vary by ~110 | 29 s / 35 s |
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
- **Migrations.** fermut mutates `migrations/` by default. In healthchecks
  those were 55 % of all mutants, 76 % of them survived, and they all run
  the slow way (class attributes run at import). Exclude them:

```toml
exclude = ["**/migrations/**"]
```

- **Transactional tests** (`TestCase`, `TransactionTestCase`) ran
  unchanged.

## FastAPI / Starlette `TestClient`, anyio { #fastapi }

FastAPI's `TestClient` is Starlette's, and both drive the app through anyio.
starlette's suite (anyio's pytest plugin, asyncio and trio backends) gave 0
diffs over 3 Linux rounds, 2.3× faster with schemata.

fastapi itself gave 6 diffs on the first pair of runs. All 6 came from
**order-dependent tests**: `tests/test_dependency_contextmanager.py`
asserts on module-level state that an earlier test may already have
changed. fermut runs the test most likely to fail first
([smart ordering](../reference/cli/run.md#smart-test-ordering)), and the order it
learns changes between runs. With `--no-smart-order`, 4 runs (2 per mode)
of `fastapi/concurrency.py` agreed exactly. If your suite has tests that
depend on order, fix them or run with `--no-smart-order`.

On fastapi, schemata was **slower** than spawning (675 s vs 561 s): 630
tests fail in fermut's forked runs even with no mutant active, so their
mutants fall back to a normal run after a wasted fork. The verdicts are
still right. A plain fork after collection runs the same tests fine, so the
cause is in fermut's fork setup (tracked in backlog #83). Until it's fixed,
compare one run with `--no-schemata` on a FastAPI app and keep whichever
is faster.

## pytest-asyncio { #pytest-asyncio }

aiofiles (`asyncio_mode = "auto"`) ran in forks without trouble: 462
mutants served from the fork, 29 s vs 35 s spawning.

We couldn't measure schemata-vs-spawn parity. Two spawn runs of the same
commit differed by 112–176 verdicts, because aiofiles' tests create,
rename and delete files under `tests/resources/`. See
[Tests that write into the repo](#tests-that-write-into-the-repo).

## trio { #trio }

trio loads its own pytest plugin. Pass the suite's flags as usual
(`pytest_args = ["-p", "trio._tests.pytest_plugin", "--import-mode=importlib"]`
when they aren't already in `addopts`). 0 stable diffs over 3 Linux rounds.
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
fail halfway. Later mutants on that worker then fail because of the
leftover file, not because of the mutation, and are counted as killed.

aiofiles shows the effect: `test_rename` moves `tests/resources/test_file1.txt`
away and back, and `test_islink` / `test_mkdir_and_rmdir` create and
remove files next to it. Two identical runs differed by up to 176 verdicts,
and the score moved between 48.5 % and 51.8 %.

Use `tmp_path` (or `tmp_path_factory`) for anything a test writes. A
mutation run, unlike a normal run, makes tests fail halfway, so cleanup at
the end of a test isn't enough. Detecting and repairing a dirty mirror is
tracked in backlog #82.
