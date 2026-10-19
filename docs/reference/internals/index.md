# Internals

A walk through fermut's code, top to bottom. Useful when you're about
to write a patch and want to know where it should live.

## Pipeline

```
 src/*.py  ──►  parse (ruff_python_parser)
                      │
                      ▼
              mutation operators
                      │
                      ▼
        candidate mutants (source patches)
                      │
                      ▼
        filter chain  ◄── cheap → expensive: operator tiers, shard/sample,
                      │     diff, coverage, ruff, ty, tce
                      ▼
         test runner (pytest | rstest | unittest)  — parallel, isolated, timeout-bounded
                      │
                      ▼
                   report
```

## Crate map

The library has one binary (`fermut`) and one library (`fermut`) target,
both built from the same crate root. The module structure:

| Module        | Responsibility                                                                      |
|---------------|--------------------------------------------------------------------------------------|
| `cli`         | clap parser, subcommand dispatch, build_config (CLI ⊕ file ⊕ defaults → runtime `Config`). |
| `config`      | Runtime `Config` struct, TOML schema, walk-up file loader.                            |
| `mutator`     | AST visitor + operator catalog → `Mutant` candidates.                                  |
| `emit`        | Splice a `Mutant`'s replacement bytes into the source range.                           |
| `filter`      | `Filter` trait + composable filter chain (experimental, parity, profile, operator, shard, sample, diff-only, since, coverage, ruff, ty, tce). |
| `runner`      | `Runner` trait + pytest (also drives rstest) / unittest implementations. Worker mirror management. |
| `engine`      | Top-level orchestration: parse → mutate → filter → run → report. Parallel via rayon. |
| `cache`       | `(mutant.id, ast_hash(file), scope) → outcome` cache, plus killer-scoped lookup for kills. |
| `ast_hash`    | Structural AST hash per file (and per top-level scope for `cache_scope = "scope"`).    |
| `history`     | `.fermut/history.jsonl` append + load, plus the `sparkline` helper shared with `trend`. |
| `report`      | Outcome types + writers (json / junit / html / markdown). GHA annotation output.       |
| `watch`       | File-system watch loop for `--watch` mode.                                              |
| `equiv`       | Post-run equivalent-mutant detection (AST patterns + CPython bytecode identity).        |
| `kill_order`  | Smart test ordering: kill history (`.fermut/kill-order.json`) + coverage-breadth prior. |
| `subsume`     | Dominator / subsumption analysis behind `fermut subsume` and `--only-dominators`.      |
| `test_tree`   | Test-suite fingerprinting for cache invalidation.                                       |
| `llm`         | Model client used by `suggest`, `autofix` and `explain`.                                |
| `sync`, `util` | Small shared helpers (poison-recovering locks, search utilities).                    |

## Data flow per `fermut run`

1. **`engine::run(cfg)`** is the entry point. Everything below is what
   it orchestrates.

2. **Parse + collect.** `mutator::collect_from_tree(source_root, exclude)` walks
   `.py` files under the source root, parses each with
   `ruff_python_parser`, runs the visitor over each AST, and emits a
   `Vec<Mutant>`. One `Mutant` = one specific operator application at
   one source location, carrying the byte range to replace + the
   replacement bytes.

3. **Build filter chain.** `filter::build_chain(cfg)` returns a
   `Vec<Box<dyn Filter>>` in cost order (`CANONICAL_ORDER` in
   `src/filter/mod.rs`): experimental → parity → profile → operator
   allow/deny → shard → sample → diff-only → since → coverage → ruff →
   ty → tce. Each filter exposes `name()` and `admits(&Mutant) ->
   Result<bool>`; the first `false` names the rejecting filter.

4. **Hash files once.** Every unique source file gets one structural
   `ast_hash` computed upfront. The cache key is
   `(mutant.id, ast_hash, scope)`, where `scope` folds in run-shape
   inputs and the test fingerprint (see [Caching](../../concepts/caching.md)).

5. **Open the result cache.** `Cache::load(path)` reads
   `.fermut/cache.json` if it exists. Defaults to empty.

6. **Parallel evaluation.** Rayon's `par_iter` over `mutants`. For each
   mutant:

    a. Run the filter chain. First reject wins → outcome = `Skipped`.

    b. Cache lookup with `(mutant.id, ast_hash, scope)`, then the
       killer-scoped lookup for kills. Cached non-skip outcomes are
       reused.

    c. Cache miss → call `runner.run(mutant)`. Returns `Killed` /
       `Survived` / `TimedOut` / `Error`. Survivors then go through the
       equivalence detector and may become `Equivalent`.

    d. Cache the outcome (unless `Skipped` or `Error`).

7. **Save cache.** On rayon join, `Cache::save(path)` writes back.

8. **Build report.** `Report::new(outcomes)` aggregates. Score is
   computed lazily via `counts()`.

9. **Append history.** Unless disabled, `history::append` writes one
   line to `.fermut/history.jsonl` with timestamp, score, counts,
   and (if discoverable) the git sha + branch.

10. **Return `Report`.** The CLI dispatcher takes the report, prints
    the human / JSON output, and writes the optional `--json` /
    `--junit` / `--html` / `--markdown` files.

## Runners

`runner::Runner` is a trait. Implementations:

- `PytestRunner` — invokes `<python> -m pytest --tb=no -q -x
  [-p <reporter>] [--hypothesis-seed=N] [<extra args>] <node ids | tests dir>`
  per mutant. `-x` is dropped under `--record-kill-sets`; `-p` loads the
  injected JSONL reporter when the pytest plugin is on. The same runner
  drives `rstest` (`runner = "rstest"`), swapping only the executable.
- `UnittestRunner` — invokes `<python> -m unittest discover -s
  <mirrored-tests> -p <unittest_pattern>` (default `test*.py`).

`build(cfg)` returns a `Box<dyn Runner>` based on `cfg.runner`. To add
a new runner, implement the trait and add a `RunnerKind` variant — the
engine dispatches through the trait object, so swapping is a contained
change.

Both runners share **worker mirrors**: one mirror per rayon worker
thread, built once on first use. The mirror is a cheap copy of the
project tree (reflink/clonefile when supported; plain copy otherwise).
Per mutant, the patched bytes are spliced into the mirror's copy of
the source file, the runner is invoked, then the original bytes are
restored. The original tree is never touched.

`--isolation` picks the mirror-build mode: `auto` (reflink if
supported, else copy), `copy`, `hardlink` (fastest, unsafe if tests
write back into the tree), or `reflink` (force CoW, fall back to copy).

## Filters

A `Filter` is small:

```rust
pub trait Filter: Send + Sync {
    fn name(&self) -> &'static str;
    /// Ok(true) if the mutant should be tested, Ok(false) to drop it.
    fn admits(&self, mutant: &Mutant) -> Result<bool>;
}
```

A rejected mutant becomes `Skipped { filter: name() }`. Adding a filter
is one struct + one `chain.push` in `filter::build_chain`, at its cost
position, plus its name in `CANONICAL_ORDER` (the chain-order tests
enforce it). Tests live next to the filter implementation;
the engine tests are coarse end-to-end.

## Cache

Format: a JSON object whose `entries` map is keyed by mutant id alone;
the hash and scope live inside each entry.

```json
{
  "entries": {
    "src/auth.py@812:boundary-shift:>=->>": {
      "file_hash": "…",
      "scope": "…",
      "outcome": { "status": "survived", "mutant": { … } },
      "equiv_checked": true,
      "killer_scope": "…",
      "mac": "…"
    }
  }
}
```

`killer_scope` appears only on kills with a known killer; `mac` only
when `FERMUT_CACHE_KEY` is set. There is no version field: new fields
are added with `#[serde(default)]`, and a file that fails to parse is
quarantined to `cache.corrupt-<pid>` and the run starts empty.

Skipped + Error outcomes are **not** cached:

- Skipped — the filter chain may differ between runs (different
  `--ops`, `--coverage`, etc.), so reusing a skip would be wrong.
- Error — typically transient (flaky import, missing env var).

### ty verdict cache

A second, independent cache lives at `.fermut/ty-cache.json`,
maintained by `src/filter/ty.rs`. The default embedded checker pays
the typeshed bootstrap once per worker; the subprocess fallback pays
~150-200ms per `ty check`. Either way, on warm runs ty dominates
wall-time without this cache.

Key: `sha256(schema | ty_version | ast_hash(file) | range_start |
range_end | replacement)`. Value: `admits` bool.

Invalidation:

- `ast_hash` (from `crate::ast_hash`) drops the entry on any
  structural source change.
- `ty_version` (probed via `ty --version`) drops the cache wholesale
  on binary upgrade.
- `CACHE_SCHEMA` constant bumps drop the cache wholesale.

Cache is loaded on construction in `filter::build_chain` and saved
from `TyFilter::drop`. Save errors are logged and swallowed —
losing an update beats poisoning a run. Pytest outcomes still flow
through `.fermut/cache.json`; the ty cache only memoizes the ty
admit/reject verdict.

## Adding a feature: where it goes

| Want to add…                                  | Touch…                                                                      |
|-----------------------------------------------|------------------------------------------------------------------------------|
| A new mutation operator                        | `src/mutator/operators.rs` + `src/mutator/visitor.rs` (see [Adding an operator](adding-operators.md)). |
| A new filter (e.g. AST equivalence detection) | `src/filter/<name>.rs` + register in `filter::build_chain`.                  |
| A new test runner                              | `src/runner/<name>.rs` + new `RunnerKind` variant.                            |
| A new report writer                            | `src/report/writers/<name>.rs` (a `Report::write_<name>(&self, path)` impl) + a field and arm in `writers::ReportSinks::write_all` + the CLI flag. |
| A new CLI subcommand                            | `src/cli/subcmd/<name>.rs` + `Cmd` variant + dispatch arm.                    |
| A new config key                               | `src/config/file.rs` (TOML schema), `src/config/mod.rs` (`Config` struct), `src/cli/build_config.rs` (CLI ⊕ file ⊕ default precedence). |

## Build + test

```sh
cargo build --release      # release binary at target/release/fermut
cargo test                 # full test suite (lib + e2e)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs all four on every PR plus a dedicated MSRV job (`cargo check
--locked` on the MSRV declared in `Cargo.toml::rust-version`).

## Where the design is opinionated

- **One mutator, one parser.** ruff_python_parser only. We don't
  support alternative parsers; it's the source of truth.
- **One library + one binary, no plugin system.** All operators are
  in-tree. Adding a third-party operator means a PR.
- **No async runtime.** Rayon for parallelism; subprocess calls go
  through `std::process::Command` with `wait_timeout`.
- **No global state.** `Config` is built once per invocation and
  passed by reference. Tests can construct one directly.
- **Cache is observational, not load-bearing.** Anything that depends on
  the cache being present (e.g. `fermut trend` requires history, not
  cache) belongs in `history`, not `cache`.
