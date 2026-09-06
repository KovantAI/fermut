# Roadmap

What's shipping next, what's on the horizon, what's long-term. No
dates — pre-1.0 ordering depends on demand and contributor
bandwidth. Tracked alongside live work at
<https://github.com/KovantAI/fermut/issues>.

## Next up (biggest open bets)

The engine is fast (in-process ty, coverage-owned loop, smart ordering).
The two highest-leverage tracks left:

- **Distribution — meet users where they already are.** Now that runs
  are quick, adoption is gated by setup friction, not speed. The
  cluster: a **`pytest-fermut` plugin** (run mutation testing without
  leaving the pytest UX), an **official `KovantAI/fermut@v1` GitHub
  Action** (cache-restore + PR-comment in one step), a **pinned Docker
  image** (ty + ruff + coverage preinstalled), a **VS Code plugin**
  (inline survivor decorations), plus a packaged **pre-commit hook** and
  a **Homebrew formula**. Each removes a reason a team bounces off.
  Detail under [Integrations and distribution](#integrations-and-distribution).
- **Higher-order mutants — a real differentiator.** Combine two
  operators on the same source to catch tests that pass under any single
  mutation but fail under interactions. No mainstream Python mutation
  tool ships this; it's a genuine capability edge, not just a speed or
  polish win. Experimental / off by default (high noise, high signal).
  Detail under [Engine and runtime](#engine-and-runtime).

Everything below is the full backlog; these two are where the marginal
return is highest today.

## Engine and runtime

- **ty in-process — shipped.** fermut embeds ty's analysis in-process
  (a salsa `ProjectDatabase` with a per-file in-memory overlay), one
  checker per worker, so there is no `ty check` subprocess per mutant.
  typeshed + deps are analyzed once and only the mutated file
  re-infers; a persistent verdict cache
  (`.fermut/ty-cache.json`) skips unchanged mutants across runs. This
  supersedes the old "reuse one long-lived ty process" idea — in-process
  beats a daemon (no IPC). Residual, low priority: each worker DB
  bootstraps typeshed once (N bootstraps + N× stdlib memory). A shared
  read-only base forked per-worker for the overlay would cut that to
  ~1×, *if* ty's pre-release salsa API ever exposes cheap DB forking —
  gate on a benchmark first (the bootstrap is amortized over thousands
  of mutants, so it may not be worth it).
- **Smart test ordering — shipped.** When `--coverage` selects more
  than one test for a mutant, fermut reorders them so the likeliest
  killer runs first and pytest's `-x` short-circuits sooner. Two
  signals: a **cold-start** prior from coverage *breadth* (the most
  targeted test — fewest lines covered — first, so it helps on run 1 and
  on freshly-changed `--since` lines, no history needed), and a
  **historical-killer** signal (the test that previously killed this
  `(file, operator)`, persisted in `.fermut/kill-order.json`). Default
  on (`--no-smart-order` to disable); it only permutes the selected set,
  so it never changes a verdict — only speed. (The two signals ship on
  separate feature branches pending merge to `main`.)
- **Per-mutant coverage ordering (deeper) — open.** Sharpen the
  cold-start prior beyond breadth + history with a richer coverage
  signal — per-line hit weight, or call-graph proximity to the mutated
  line — to better predict the killer when no history exists.
- **Time-boxed runs (`--max-time`).** Wall-clock ceiling for big
  suites that can't run the full catalogue. Evaluates the
  highest-value mutants first (coverage + smart ordering) and
  reports the best survivors found within the budget. Predictable
  PR-gate latency — a time ceiling beats a mutant ceiling for CI
  trust.
- **Cross-run mutant cache.** Shareable cache across machines /
  CI runners. Cache server + content-addressed entries. Today's
  cache is local-only.
- **Incremental mode.** Persistent daemon that watches the source
  tree and re-evaluates only affected mutants on each change. The
  inner-loop sibling of `--watch`.
- **Higher-order mutants (experimental).** Combine two operators
  on the same source. Catches tests that pass under any single
  mutation but fail under interactions. Off by default — high
  noise, high signal.

## Operators for modern Python

- **Async-aware — shipped (partial).** `await-drop` (`await X` →
  `X`), `async-for-to-sync`, and `async-with-to-sync` are stable
  operators now. Still open: `asyncio.gather` arg reorder.
- **`match`/`case` — shipped (partial).** `match-guard-negate`
  (`case … if g` → `if not (g)`) is a stable operator. Still open:
  pattern swap, wildcard insertion, case reorder.
- **Exception chaining — shipped (partial).** `exp:raise-from-drop`
  (`raise X from e` → `raise X`, also `from None`) catches tests that
  assert the exception type but not `__cause__`. Experimental. Still
  open: a distinct `from None` ↔ `from e` swap.
- **Type-annotation operators — shipped (partial).** Experimental:
  `exp:numeric-type-swap` (`int` ↔ `float`), `exp:optional-type-drop`
  (`Optional[T]` / `T | None` → `T`), `exp:container-type-swap`
  (`list[T]` → `tuple[T]`, builtin containers). Lights up
  type-narrowing bugs in runtime-validated code (pydantic, dataclass,
  beartype) that ty + tests both miss. Still open: the typing-generic
  widening `List[T]` → `Iterable[T]` (needs import-aware emission).

## Integrations and distribution

- **`pytest-fermut` plugin.** Driving fermut from a pytest entry
  point so users don't leave the pytest UX to run mutation
  testing.
- **Official GitHub Action.** `KovantAI/fermut@v1` with cache
  restore + PR-comment in one step. Today users compose
  `actions/cache` + `fermut pr-comment` themselves — see
  [Integrations](guides/integrations.md).
- **Pre-commit hook.** Officially packaged `.pre-commit-hooks.yaml`
  in the repo. Today's pre-commit guide is local-only.
- **Docker image.** Pinned-version image with ty + ruff + coverage
  preinstalled. Useful for CI runners that don't want to manage
  Python toolchain state.
- **VS Code plugin.** Inline survivor decorations, "run mutation
  on this file" command, hover-to-see-mutant.
- **Homebrew formula.** `brew install fermut`. Today's non-source
  install path is PyPI (`uv tool install fermut`).

## Reports and UX

- **HTML report with collapsible per-file mutant diff.** Today's
  HTML is a flat list. Next: collapse-by-file tree, per-mutant
  inline diff, filterable by operator / status.
- **Resource budgets reports.** Wall-clock + RSS + pytest
  invocation count per filter stage, per operator. Tells you
  *where* the budget went, not just the total.

## Long-term

- **Rust self-mutation.** Mutate fermut's own Rust source as part
  of CI. Eats own dogfood, catches regressions in operator
  emission logic.

## Time-boxed runs (`--max-time`) — shipped

`fermut run --max-time <SECS>` caps the testing phase at a wall-clock
ceiling. Mutants are evaluated highest-value first (covered ahead of
uncovered); mutants left untested at the deadline are recorded as
`skipped`/`time-budget` and excluded from the score, so a truncated run
reports over what actually ran rather than masquerading as a full sweep.
A predictable time ceiling for PR gates — a time cap beats a mutant cap
for CI trust. See **[run reference](reference/cli/run.md#time-boxed-runs-max-time)**.
Follow-ups still open: coverage-call-graph ordering and historical-killer
ordering (below) sharpen the "highest-value first" signal.

## Public PyPI — shipped

fermut now publishes to [PyPI](https://pypi.org/project/fermut/) on
every `v*` tag via trusted publishing (OIDC); prerelease dry-runs go
to TestPyPI from a manual `workflow_dispatch`. Install with
`uv tool install fermut`. See **[Policies](reference/policies/index.md)**
for the distribution contract.

## Want to push something up the list?

Open an issue with the use case. Items move based on:

1. **Concrete blocker** — someone can't ship without it.
2. **High leverage** — small change, large multiplier (e.g. ty
   daemon mode).
3. **Contributor bandwidth** — items with a paired PR move first.

See **[Internals](reference/internals/index.md)** and **[Adding an
operator](reference/internals/adding-operators.md)** for contributor
onboarding.
