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

- **`match`/`case`.** Pattern swap, guard mutation, wildcard
  insertion, case reorder.
- **Type-annotation operators.** `int` ↔ `float`, `Optional[T]` ↔
  `T`, `List[T]` ↔ `Iterable[T]`. Lights up the kinds of
  type-narrowing bugs that ty + tests both miss.
- **Async-aware.** `await` drop, `async for` → `for`, `asyncio.gather`
  arg reorder. Today's catalogue treats async like sync.
- **Exception chaining.** `raise X from e` → `raise X`,
  `raise X from None` swap. Catches tests that assert on
  exception type but not on `__cause__`.

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
