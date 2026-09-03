# Roadmap

What's shipping next, what's on the horizon, what's long-term. No
dates — pre-1.0 ordering depends on demand and contributor
bandwidth. Tracked alongside live work at
<https://github.com/KovantAI/fermut/issues>.

## Engine and runtime

- **ty daemon mode.** Reuse one long-lived `ty` process across the
  filter chain instead of spawning per mutant. Targets the largest
  remaining per-mutant overhead.
- **Per-mutant test selection via coverage (deeper).** Today's
  `--coverage` filter narrows tests via per-test contexts. Next:
  use coverage call-graph data to also order tests by
  most-likely-to-kill first.
- **Smart test ordering.** Within selected tests, run the
  historical-killer first. Cuts wall-clock on survivor tails.
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
