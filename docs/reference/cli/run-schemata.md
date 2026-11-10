# Schemata

How [`fermut run`](run.md) runs mutants in forks of one pytest process
instead of a fresh pytest each. On by default with the pytest runner on
Unix; `--no-schemata` turns it off.

By default every mutant gets a fresh `pytest` process: start Python, import
pytest and your package, collect the selected tests, then run them. On most
suites that startup is most of each mutant's time. With the pytest runner on
Unix, fermut instead pays it once:

1. Before the first mutant, fermut builds one *schema*: a copy of the project
   in which every mutant inside a function body is compiled into its
   function behind a switch on the active-mutant number. Only mutants that
   can run are built in (`--sample`, coverage and the other cheap filters
   apply first).
2. One *template* — a pytest session that imported and collected the suite
   once — serves every worker. Until it is ready, workers run mutants the
   normal way instead of waiting.
3. For each mutant, the template forks a child with that mutant switched on.
   The child runs the same selected tests a spawned pytest would, in the same
   order, with `-x`, its own `tmp_path` root and the same timeout, and pytest's
   end-of-session checks (a leaked file under `filterwarnings = error` still
   fails it). Children run side by side.

Whole runs were 1.5–3.9× faster on most suites tested, and the template costs
one copy of your loaded suite in memory (88–728 MB on the suites tested).
Timeouts become the main cost, since forking can't make a hanging mutant
finish sooner, so schemata turns on a
[timeout budget](run.md#timeout-budgets) of `3 × baseline + 5 s` unless you set one.

## What runs the normal way

A fork isn't a fresh process, so fermut only takes a verdict from it when the
fork can't have made a difference. Everything else runs the normal way, and
the verdict is the same either way:

- **mutants outside function bodies** (module level, class bodies,
  decorators, default arguments): the template already ran that code;
- **mutants in a function that already ran while the template imported and
  collected** (a registry filled at import, a decorator factory): whatever it
  built before the fork used the original code;
- **mutants whose source triggers a compile-time warning**, which
  `filterwarnings = error` would turn into an import error for the whole
  file;
- **a kill or timeout the same tests reproduce with no mutant active.** Every
  kill from the schema, and every timeout inside a test, is confirmed by
  re-running the tests that child ran, in the same order, in a fork with no
  mutant switched on. They can fail or hang there when collecting the whole
  suite left state behind that a cold run of a few tests never sees — a test
  module that changes a global at import, which only a test's teardown puts
  back. Confirmation is cached per list of tests, so it usually costs one
  short fork. A confirmed timeout stands without a normal re-run: the fork
  had already imported and collected, so a cold run given the same budget
  would run out of time too;
- **a timeout before any test began** (collection, a fixture, the helper):
  there is nothing to confirm it against;
- **a fork that crashes.** Some system libraries aren't safe to use in a
  forked child — on macOS, DNS lookups, Kerberos (psycopg2 connecting) and
  some native libraries crash it. (System proxy settings, which aiohttp,
  httpx and requests look up on every request, are read once before forking,
  so those work.)

Before serving mutants, fermut runs the suite on the schema with no mutant
active, split across the workers. Tests that fail, crash or hang there are
logged, and mutants they kill run the normal way.

Confirmation guards one direction; an **audit** spot-checks the other. About
5% of survivors and timeouts from the schema (picked by mutant id, so the same
ones every run, plus the first one) are re-run the normal way, and that run's
verdict is the one reported. If the normal run disagrees on detection — it
kills a mutant the fork let survive, or lets survive one the fork timed out —
fermut warns with its id; please report it. `--no-schemata` gives the normal-run verdict for
every mutant. The summary line counts how many were audited and how many
agreed.

The run logs how many mutants the schema served and how many ran normally.
Cached verdicts are shared with normal runs. The baseline check is always a
normal pytest run.

## Turning it off, and where it doesn't apply

`--no-schemata` / `schemata = false` turn it off. `--schemata` /
`schemata = true` require it, so an unsupported setup is an error instead of a
silent normal run. It needs:

- the **pytest runner**, or **rstest** when asked for (`--runner rstest
  --schemata`; off by default there). Under rstest the template runs the
  suite on rstest's own vendored pytest, as rstest does, on the `python`
  beside the `rstest` binary. Not unittest;
- **Unix** (it forks; on Windows every mutant runs the normal way);
- a project interpreter of **Python 3.10 or newer**. Use `--python` /
  `python` when pytest runs under an interpreter other than the one behind
  `pytest` on `PATH`.

One thing behaves differently in a fork, by nature: test parameters computed
during collection (`datetime.now()`) are as old as the template. Hash order
doesn't: fermut pins `PYTHONHASHSEED` for the template and every normal run
alike (your own exported value wins), so set and dict iteration order is the
same in a fork, a normal run, and the next run. Each fork also gets its own private `HOME`
(see `--no-private-home`), removed when it exits, as each worker does in a
normal run.
