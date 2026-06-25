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
              ty pre-filter  ◄── drops mutants whose patched source
                      │            introduces new ty diagnostics
                      ▼
         test runner (pytest | unittest)  — parallel, isolated, timeout-bounded
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
| `filter`      | `Filter` trait + composable filter chain (experimental, op, sample, diff, coverage, ruff, ty). |
| `runner`      | `Runner` trait + pytest / unittest implementations. Worker mirror management.          |
| `engine`      | Top-level orchestration: parse → mutate → filter → run → report. Parallel via rayon. |
| `cache`       | `(mutant.id, ast_hash(file), scope) → outcome` cache. Reformat-resilient.              |
| `history`     | `.fermut/history.jsonl` append + load, plus the `sparkline` helper shared with `trend`. |
| `report`      | Outcome types + writers (json / junit / html / markdown). GHA annotation output.       |
| `watch`       | File-system watch loop for `--watch` mode.                                              |

## Data flow per `fermut run`

1. **`engine::run(cfg)`** is the entry point. Everything below is what
   it orchestrates.

2. **Parse + collect.** `mutator::collect_from_tree(source_root)` walks
   `.py` files under the source root, parses each with
   `ruff_python_parser`, runs the visitor over each AST, and emits a
   `Vec<Mutant>`. One `Mutant` = one specific operator application at
   one source location, carrying the byte range to replace + the
   replacement bytes.

3. **Build filter chain.** `filter::build_chain(cfg)` returns a
   `Vec<Box<dyn Filter>>` in cost order. Stages: experimental →
   operator allow/deny → sample → diff-only / since → coverage → ruff →
   ty. Each filter exposes `name()` and `evaluate(&Mutant) -> Decision`
   (admit / reject).

4. **Hash files once.** Every unique source file gets one `sha256`
   computed upfront. The cache key is `(mutant.id, file_hash)`.

5. **Open the result cache.** `Cache::load(path)` reads
   `.fermut/cache.json` if it exists. Defaults to empty.

6. **Parallel evaluation.** Rayon's `par_iter` over `mutants`. For each
   mutant:

    a. Run the filter chain. First reject wins → outcome = `Skipped`.

    b. Cache lookup with `(mutant.id, file_hash)`. Cached non-skip
       outcomes are reused.

    c. Cache miss → call `runner.run(mutant)`. Returns `Killed` /
       `Survived` / `TimedOut` / `Errored`.

    d. Cache the outcome (unless `Skipped` or `Errored`).

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

- `PytestRunner` — invokes `pytest -x --tb=no -q [--hypothesis-seed=N]
  [<extra args>] <mirrored-tests>` per mutant.
- `UnittestRunner` — invokes `python -m unittest discover -s
  <mirrored-tests> -p 'test_*.py'`.

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
    fn evaluate(&self, mutant: &Mutant) -> Result<Decision>;
}

pub enum Decision {
    Admit,
    Reject(&'static str),  // reason — surfaced in skipped outcome
}
```

Adding a filter is one struct + one match arm in
`filter::build_chain`. Tests live next to the filter implementation;
the engine tests are coarse end-to-end.

## Cache

Format: a JSON file with a single top-level object mapping mutant id +
file hash to outcome.

```json
{
  "version": 1,
  "entries": {
    "src/auth.py@812:boundary-shift:age >= 18->age > 18|sha256:abcd...": {
      "status": "survived",
      ...
    }
  }
}
```

The version field is bumped when the on-disk schema changes.
Older readers fail closed (treat cache as empty); newer readers handle
forward-compatibility with `#[serde(default)]` on new fields.

Skipped + Errored outcomes are **not** cached:

- Skipped — the filter chain may differ between runs (different
  `--ops`, `--coverage`, etc.), so reusing a skip would be wrong.
- Errored — typically transient (flaky import, missing env var).

### ty verdict cache

A second, independent cache lives at `.fermut/ty-cache.json`,
maintained by `src/filter/ty.rs`. Each `ty check` invocation costs
~150-200ms (typeshed bootstrap + parse + inference); on warm runs
ty dominates wall-time without this cache.

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
| A new report writer                            | `src/report/writers/<name>.rs` + a `Report::write_<name>(&self, path)`.       |
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
