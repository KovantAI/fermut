# Glossary

One-page lookup for every term fermut docs assume you know. Skim once,
return on demand. Entries cross-link to the page where the concept is
explained in depth.

---

## Coverage filter

Filter stage that drops mutants on lines no test executes. Requires a
per-test-context coverage database, most easily built with `fermut
coverage` (writes `.coverage`; or manually `pytest --cov=src
--cov-context=test` then `coverage json -o coverage.json
--show-contexts`), so fermut knows **which** tests reach each line.
Without per-test contexts, the filter still drops uncovered mutants but
cannot pick a narrow test subset for the survivors. See [coverage
guide](../guides/coverage.md).

## cosmic-ray

Older Python mutation-testing tool, distributor-based model
(scheduler + workers). Different config shape, different vocabulary
("session" vs "run"). fermut ships a
[migration guide](../guides/migrate-from-cosmic-ray.md).

## `--diff-only`

CLI flag and config key. Restricts mutation to lines in
`git diff <base>...HEAD`. Default base is the `pr-gate` profile's
`diff_only = "main"`. Requires git history — on a shallow clone
(`fetch-depth: 1` in CI, `git clone --depth 1` locally) the diff
comes back empty and zero mutants run. See
[diff-only-empty troubleshooting](../reference/troubleshooting/diff-only-empty.md).

## Equivalent mutant

A mutant whose patched code behaves identically to the original at
runtime, even though the source is textually different. Example:
`x + 0` → `x - 0`. No test can kill it — there's no observable
difference to assert against. fermut runs a layered detector on
survivors (AST patterns + CPython bytecode by default, with opt-in
hypothesis probe and LLM judge); provably-equivalent mutants become
the `Equivalent` outcome and drop out of the score denominator, and
likely-equivalent ones surface as suggestions for inline-ignore
review. Domain-restricted equivalences the detector can't see still
need a manual `# fermut: ignore` marker. See
[equivalent mutants walkthrough](../guides/equivalent-mutants.md).

## Errored

Outcome for a mutant where pytest crashed before producing a pass/fail
verdict (collection error, import error, fixture exploded). Not
counted toward the score. Usually signals test-suite side effects on
the source tree — see
[errored outcomes troubleshooting](../reference/troubleshooting/errored-outcomes.md).

## Filter chain

Composable stack of pre-flight checks every mutant runs through
before pytest sees it. Order is cheap → expensive: experimental →
operator → sample → diff-only → coverage → ruff → ty. Skipping a
mutant via a filter is dramatically cheaper than running pytest
against it. See
[filter chain overview](mutation-testing.md#the-filter-chain).

## Hypothesis seed

Hypothesis is a property-based testing library. It picks random
inputs each run unless given a fixed seed. Mutation testing relies
on deterministic test outcomes — a flaky test that passes on the
original but fails on a mutant due to a different random draw will
falsely "kill" the mutant. fermut's `pr-gate` and `library` profiles
pin the Hypothesis seed in `pyproject.toml`. Ignore this entry if
your test suite doesn't import `hypothesis`.

## Inline ignore marker

Comment on a source line that tells fermut to skip mutations on that
line: `# fermut: ignore` (skips all ops) or
`# fermut: ignore [op-name]` (skips one op). Use for equivalent
mutants and for lines where mutation produces noise without signal.
See [inline ignore reference](../reference/operators/inline-ignore.md).

## Isolation mode

How a worker thread keeps its mutated source files from contaminating
the original tree. `reflink` (default on APFS/btrfs/xfs/zfs/ReFS) is
cheap. `copy` is the safe fallback. `hardlink` is fastest but unsafe
if your tests write into the tree. Switch with
`--isolation`. See [isolation modes reference](../reference/cli/run.md#isolation-modes).

## Killed

Outcome: the test suite **failed** on the mutated code, i.e. the
test noticed the bug fermut introduced. Counted as a kill in the
score. **What you want more of.**

## Mutant

One specific application of one operator to one specific source
location. Identified by `mutant.id`, a stable hash so caching works
across runs.

## `mutant.id` { #mutant-id }

Stable identifier for a single mutant. Grammar:
`<file>@<byte-offset>:<operator>:<original>-><replacement>`, e.g.
`src/calculator.py@216:boundary-shift:<=-><`. Used as the cache key, the
dedup key for sharded runs, and the JSON output's primary key. Stable
across runs as long as the source file hasn't changed. Note `<file>` is
the path fermut was invoked with — typically **absolute**, so ids are not
portable across machines or checkouts; don't persist them as cross-machine
keys (key on `file` + `line` + `operator` instead).

## mutmut

Python mutation-testing tool, in-process model. Most-used
predecessor of fermut in the Python ecosystem. Different CLI shape,
overlapping operators. fermut ships a
[migration guide](../guides/migrate-from-mutmut.md).

## Mutation score

`detected / (detected + survived)` where `detected = killed + timeout`.
Skipped and errored mutants don't appear in either side of the ratio.
Timeouts count as detected (the mutant changed behavior enough to
wedge the test, the runner just gave up before getting a clean fail).
See [mutation score on the concept page](mutation-testing.md#the-mutation-score).

## Operator (`op`)

A *kind* of mutation. Examples: `arith-op-swap` (replace `+` with
`-`), `boundary-shift` (`<` → `<=`), `return-to-none`. fermut ships
30 stable operators and 8 experimental. Select with `--ops`, skip
with `--skip-ops`, enable experimental with `--experimental`. See
[operator catalogue](../reference/operators/index.md).

## Profile

A named starter template for `fermut.toml`. Four profiles ship:
`pr-gate` (narrow ops, diff-only, fast), `nightly` (every op,
no filters, slower), `local` (25% sample, dev loop), `library`
(every stable op, no experimental). Pick with
`fermut init --profile <name>`. Profiles are templates, not modes —
after `init` the config is plain text and you hand-edit. See
[profile reference](../guides/projects.md#step-2-pick-a-profile).

## ruff

Fast Python linter (Rust-based). fermut's optional ruff filter rejects
mutants that introduce *new* lint diagnostics — a cheap signal that
the mutant produces obviously wrong code. Enable with `--ruff-filter`
(off by default; on when ruff is on `$PATH` and the profile opts in).

## Score swings

Variability in the score between runs of the same code. Usually
caused by random sampling (`--sample`), unpinned Hypothesis seeds,
or pytest order-dependent tests. See
[score swings troubleshooting](../reference/troubleshooting/score-swings.md).

## Shard

A deterministic subset of mutants picked by `--shard i/n`. Mutants
are partitioned by hash of `mutant.id`, so shards don't overlap and
require no coordination between workers. Used to parallelize long
runs across CI matrix jobs. See
[sharded full sweep](../guides/integrations.md#sharded-full-sweep).

## Skipped

Outcome: a filter rejected the mutant before pytest ran. Not counted
toward the score (neither evidence for nor against test quality).
Different filters produce different skip reasons; `fermut show <id>`
prints which filter dropped it.

## Survived

Outcome: the test suite **passed** on the mutated code, i.e. the
test did not notice the change. Counted as a survival in the score.
**What you want fewer of** — each survivor is a pointer to a
behavioral gap in your tests.

## Timed out

Outcome: pytest ran longer than `--timeout` against the mutant.
Usually means the mutant introduced an infinite loop (a boundary
shift flipped a `<` to `<=` and broke loop termination). Counted as
killed for scoring — the mutant changed behavior enough to wedge the
test. Raise `--timeout` if you're getting timeouts on legitimately
slow tests.

## ty

Astral's fast Python type checker (Rust-based). fermut's ty filter
rejects mutants that introduce *new* type errors — e.g. swapping a
numeric op into a string context. Drops 20–40% of naive mutants on
type-hinted codebases at ~25ms each, much cheaper than running
pytest ([benchmarks](../reference/benchmarks.md)). Requires
`ty` on `$PATH`. See [ty-not-found troubleshooting](../reference/troubleshooting/ty-not-found.md).

## uv

Astral's fast Python package manager. fermut's CI templates use uv
for install/sync because it's the fastest path in CI; pip/Poetry/Hatch
all work — translate the install step.

---

## See also

- **[Mutation testing](mutation-testing.md)** — concepts in context.
- **[Landscape](landscape.md)** — fermut vs mutmut vs cosmic-ray.
- **[CLI reference](../reference/cli/index.md)** — flag-level docs.
