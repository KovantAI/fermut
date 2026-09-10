# Landscape

Where fermut sits in the Python mutation-testing ecosystem, and why we
built a new tool when [mutmut](https://github.com/boxed/mutmut) and
[cosmic-ray](https://github.com/sixty-north/cosmic-ray) already exist.

## Why a new tool

Mutation testing in Python has been around for a decade. The existing
tools are battle-tested and we owe them most of the vocabulary on this
page. But three things have changed under them in the last couple of
years, and the existing tools weren't built for the new shape:

1. **AI coding agents** drive the inner loop now. Agents need
   machine-readable output (stable JSON, not text), deterministic
   survivor lists across runs (no Hypothesis-seed drift), and a
   per-file cache that survives across short iterations. Existing
   tools score these on `O(1)` of the dimensions an agent cares
   about.
2. **PR-time gates** require single-digit-minute wall clocks. A
   PR that waits 40 minutes for mutation feedback is dead on
   arrival. A naive Python AST walker over a 50k-LoC repo can't
   hit that budget without `--diff-only`, per-test coverage
   selection, and a fast pre-filter that drops type-invalid
   mutants before they cost a pytest run. None of the existing
   tools have all three.
3. **`ty`** — Astral's static type checker — is fast enough
   (~25 ms / file on warm caches) that it makes economic sense to
   *check* every mutant for type validity before running tests.
   That filter alone drops 20–40% of naive mutants on type-hinted
   codebases. Built on a stack that pre-dates `ty`, the existing
   tools can't take advantage of it.

fermut's bet: the mutator runs in native Rust on top of `ruff`'s
parser, the filter chain is composable and cheap, the cache is
content-hashed for reproducibility, and the JSON shape is treated as
API. That gives the speed budget for PR gates and the determinism
budget for agents.

This page is a fair side-by-side comparison, written by fermut's
authors but trying not to cheat — see *Honest weak spots* at the
bottom for what fermut still doesn't do that the others do.

## At a glance

| Dimension                        | fermut                                                                                  | mutmut                                              | cosmic-ray                                                |
|----------------------------------|-----------------------------------------------------------------------------------------|-----------------------------------------------------|-----------------------------------------------------------|
| Language                         | Rust (mutator) + Python (test runner)                                                    | Python (everything)                                  | Python (everything)                                        |
| Parser                           | [`ruff_python_parser`](https://github.com/astral-sh/ruff)                                | `parso`                                              | `ast` (stdlib)                                            |
| Type-aware filter                | :material-check:{ .yes } [`ty`](https://github.com/astral-sh/ty) pre-filter, on by default; ~25 ms/file warm | :material-check:{ .yes } optional `mypy` pre-check (opt-in; pays mypy startup cost on each invocation, so typically off in CI loops) | :material-close:{ .no }                                                          |
| Operator catalogue (stable)      | 30                                                                                       | ~17 (mutmut's "default")                             | ~20 (configurable; cosmic-ray ships several "subjects")    |
| Inline ignore markers            | :material-check:{ .yes } `# fermut: ignore[op-name]`                                                            | :material-check:{ .yes } `# pragma: no mutate`                              | :material-close:{ .no } (use config exclusions)                                  |
| Docstring auto-skip              | :material-check:{ .yes }                                                                                        | :material-check:{ .yes }                                                    | depends on subject                                         |
| Diff-only mode                   | :material-check:{ .yes } `--diff-only main`                                                                     | partial (`mutmut run path`)                          | partial (manual filtering)                                 |
| Coverage-based test selection    | :material-check:{ .yes } reads coverage.py's `.coverage` SQLite directly (auto-discovered at project root; per-test contexts via `--cov-context=test`) | :material-close:{ .no }                                                    | :material-close:{ .no }                                                          |
| Hypothesis seed pinning          | :material-check:{ .yes } `--hypothesis-seed`                                                                    | :material-close:{ .no }                                                    | :material-close:{ .no }                                                          |
| Result cache                     | :material-check:{ .yes } `(mutant.id, ast_hash(file), scope)` keyed (AST-structural, reformat-resilient)        | :material-check:{ .yes } keyed on source-line hash                          | :material-check:{ .yes } (database-backed)                                       |
| Distributed execution            | :material-check:{ .yes } `--shard i/n` (zero-coordination, GHA matrix)                                          | :material-close:{ .no }                                                    | :material-check:{ .yes } celery / sql worker pool                                  |
| Watch mode                       | :material-check:{ .yes } `--watch`                                                                              | :material-close:{ .no }                                                    | :material-close:{ .no }                                                          |
| Reports                          | JSON, JUnit XML, HTML, Markdown                                                          | text + HTML                                          | json / sqlite / cosmic-ray-html                            |
| Sticky PR comments               | :material-check:{ .yes } `fermut pr-comment` (marker-based; updates in place)                                   | :material-close:{ .no }                                                    | :material-close:{ .no }                                                          |
| Trend tracking                   | :material-check:{ .yes } `.fermut/history.jsonl` + `fermut trend` (sparkline + deltas)                          | :material-close:{ .no }                                                    | :material-close:{ .no }                                                          |
| Curated profile templates        | :material-check:{ .yes } `fermut init --profile pr-gate\|nightly\|local\|library`                              | :material-close:{ .no }                                                    | :material-close:{ .no }                                                          |
| Environment doctor               | :material-check:{ .yes } `fermut doctor`                                                                        | :material-close:{ .no }                                                    | :material-close:{ .no }                                                          |
| Migration tooling                | :material-check:{ .yes } `fermut migrate {mutmut,cosmic-ray}` (config + pragma rewrite + unmapped-key report)   | :material-close:{ .no }                                                    | :material-close:{ .no }                                                          |
| Test runners                     | pytest, unittest (trait-based, extensible)                                                | pytest, unittest                                     | pytest, unittest, nose                                     |
| Worker isolation                  | Per-worker mirror, reflink/clonefile-aware, configurable (auto/copy/hardlink/reflink)    | edits file in place + restores                       | per-worker mutated copy                                    |
| Distribution                     | PyPI                                                                                    | PyPI                                                 | PyPI                                                       |

## When to pick which

Use **mutmut** when:

- You want the most-established, longest-running tool in the ecosystem.
- Your test suite is small enough that startup costs don't dominate, and
  raw mutation speed isn't your bottleneck.
- You need the tool to "just work" with minimal configuration and no
  Rust toolchain in scope.
- You don't run mutation testing in CI as a gate; you run it occasionally
  during local development.

Use **cosmic-ray** when:

- You have a multi-day mutation run that benefits from a persistent
  celery / worker-pool architecture.
- You're testing a large monolithic codebase and need every survivor
  cataloged in a database for offline analysis.
- You're comfortable with a heavier setup (broker, worker config) in
  exchange for queue-driven parallelism that survives worker restarts.

Use **fermut** when:

- You want PR-time mutation gates that finish in single-digit minutes,
  not hours.
- The native-Rust mutator + ty pre-filter actually matters because your
  codebase produces many type-invalid naive mutants.
- You're driving mutation testing from a **coding agent** and the
  cache hit ratio, deterministic survivors, and JSON-output guarantees
  matter — see [coding agents](../guides/coding-agents.md).
- You want `--diff-only`, `--coverage`-narrowed test selection, sticky
  PR comments, and trend tracking out of the box without writing your
  own glue.
- You want `--shard` for zero-coordination CI matrix parallelism rather
  than a celery cluster.

## Honest weak spots

fermut today does **not**:

- **Have a VS Code extension or LSP integration.** mutmut has the
  community `mutmut-mode` for emacs; nothing for fermut. Roadmap.
- **Have a `fermut bisect`** for finding the commit that introduced a
  surviving mutant. Roadmap.

If any of those is a hard requirement, mutmut or cosmic-ray may be a
better fit today.

## Sample comparison: real OSS projects

### What the numbers are based on

The harness lives under `benchmarks/` in the repo. It clones a set
of pinned open-source projects, installs each tool into its own
venv, and runs each tool against the **same source tree** with the
**same test suite** on the **same machine**. Results land in
`benchmarks/results/runs/` — one JSON per (scenario, tool, repo)
with per-phase timing, mutant total, score, and return code.

Numbers below are **preliminary** `cold`-scenario runs (fermut
0.4.1 vs mutmut 3.6.0). **They are illustrative — your codebase
will produce different results**, and fermut 0.4.1 is
pre-optimization. See [Reference → Benchmarks](../reference/benchmarks.md)
for the full table and caveats.

**Reproduce yourself.** Bash wrappers in `benchmarks/scripts/`
drive each tool/repo:

```sh
cd benchmarks
./scripts/run_cold.sh fermut more-itertools
./scripts/run_cold.sh mutmut more-itertools
./scripts/report.sh   # aggregate results/runs/*.json
```

Repos and tools are configured in `benchmarks/configs/repos.toml`
and `benchmarks/configs/tools.toml`.

### Results (cold)

The full head-to-head — mutant counts, scores, and the cold-time
breakdown (one-time `coverage_prep` + steady-state `mutation`) — lives in
**[Reference → Benchmarks](../reference/benchmarks.md)**, the single source
of truth. It isn't duplicated here (two copies inevitably drift); the
qualitative picture:

Mixed but competitive. fermut wins on **typer** (faster *and*
higher-scoring) and out-scores mutmut on **pyjwt** at similar speed.
mutmut wins on **more-itertools** — a tiny pure-Python suite where
the full-suite-per-mutant cost is already cheap and fermut's
ty + coverage overhead doesn't pay off. The coverage filter (run
only the tests crossing each mutated line) is the lever: uncovered-line
mutants are *skipped*, not counted as survivors, which both cuts
wall-clock and lifts the score. Coverage is opt-in (`--coverage`);
mutmut has no equivalent. Scores across tools are **not directly
comparable**.

fermut's per-mutant cache does work — a clean back-to-back pyjwt
re-run drops ~392s → ~11s (~35×); the harness `warm` scenario just
doesn't measure it reliably. See
[Reference → Benchmarks](../reference/benchmarks.md) for details.

**Detection parity vs mutmut is now measured** — see
[Reference → Parity](../reference/parity.md). Across four repos (pyjwt,
markupsafe, typer, more-itertools) fermut's operator catalogue covers ~99% of
the source lines mutmut mutates, with **zero detection-loss** on the mutations
both tools generate identically. The residual ~1% is cosmetic string edge cases
(case-swaps fermut skips on docstrings/escaped strings), not missed test gaps.
It is not a strict *superset* — the catalogues differ both ways — but switching
from mutmut does not lose detection on these repos.

Migrators on other codebases can still confirm locally:
`fermut migrate {mutmut,cosmic-ray}` translates the config, then diff the two
tools' survivor lists (the `benchmarks/parity/` harness automates exactly this).

mutmut runs above use the default invocation (no mypy pre-check)
for an apples-to-apples comparison — the mypy pre-check is opt-in
and not how most mutmut users invoke it. fermut's ty filter is on
by default and benefits from `ty`'s warm ~25 ms / file checks.

On codebases with stronger type hints, the ty filter typically
drops 20–40% of naive mutants before they cost a pytest run.

## Migrating

Step-by-step migration guides:

- **[Migrate from mutmut](../guides/migrate-from-mutmut.md)** —
  pragma rewriting, config mapping, operator name table.
- **[Migrate from cosmic-ray](../guides/migrate-from-cosmic-ray.md)** —
  unlearning the session model, `--shard` as the celery
  replacement, operator-family mapping.

For both, the one-shot translation tool — `fermut migrate {mutmut,cosmic-ray}` —
reads the existing config, writes a starter `fermut.toml`, and
(mutmut only) rewrites `# pragma: no mutate` → `# fermut: ignore`
across the tree. Anything it can't translate prints under "manual
review" instead of getting silently dropped.

### Before you migrate — known gotchas

- **`# pragma: no mutate` is not recognized.** `fermut migrate
  mutmut` rewrites every occurrence to `# fermut: ignore`. If you
  skip the migrator, you must rewrite by hand or accept that those
  lines mutate.
- **Operator names differ.** mutmut's are positional indices,
  cosmic-ray's are path-like (`core/ReplaceBinaryOperator_Add_Sub`),
  fermut's are kebab-case (`arith-op-swap`). The migrator maps the
  obvious cases; check the "operator name table" section of each
  migration guide for the full mapping.
- **`excluded-modules` (cosmic-ray) auto-translates** to fermut's
  `exclude = ["pattern/**", …]` glob list. Patterns outside the
  detected `source_root` surface as a note rather than silently
  failing to match.
- **No celery / SQL worker pool.** cosmic-ray's distributed
  execution doesn't have a direct fermut equivalent. The closest is
  `--shard i/n` in a CI matrix — zero-coordination, deterministic
  per `mutant.id`. Migration cost depends on how much custom
  scheduling lived in your cosmic-ray setup.
- **No `spor` / interceptor framework.** cosmic-ray's Python-callback
  filter surface has no fermut analogue. Pre-filtering in fermut is
  `ty` + `--coverage` + `--ops` / `--skip-ops` + inline `# fermut:
  ignore` markers. If `spor` was load-bearing, plan the migration
  scope accordingly.
- **No `pre_mutation` / `post_mutation` hooks (mutmut).** Fold the
  logic into pytest setup/teardown or a `--pytest-arg` plugin
  invocation.
- **No `dict_synonyms` (mutmut).** Names are matched syntactically;
  there's no alias table.

Each gotcha is covered in more depth in the relevant migration
guide — the bullets above are the "decide whether to start" list.

See **[Benchmarks](../reference/benchmarks.md)** for current numbers
against checked-in target projects.

## Further reading

- mutmut docs: <https://mutmut.readthedocs.io/>
- cosmic-ray docs: <https://cosmic-ray.readthedocs.io/>
- fermut [mutation testing](mutation-testing.md) and
  [working on projects](../guides/projects.md).
