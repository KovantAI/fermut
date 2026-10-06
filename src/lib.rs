//! fermut — Rust-powered, ty-aware mutation testing for Python.
//!
//! Top-level crate module map:
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | `cli` | clap parser + subcommand dispatch (`run`, `list`, `show`, `clean`, `completions`, `migrate`, `explain`, `init`, `coverage`, `suggest`, `dashboard`, `trend`, `doctor`, `mcp`, `autofix`, `score`, `next`, `baseline`, `pr_comment`, …) |
//! | `config` | Runtime `Config`, TOML schema, walk-up loader |
//! | `mutator` | Python AST visitor + operator catalog → `Mutant` candidates |
//! | `emit` | Splice a `Mutant`'s replacement into the source byte range |
//! | `equiv` | Equivalent-mutant detector: AST patterns + CPython bytecode identity |
//! | `filter` | `Filter` trait + 8-stage composable filter chain |
//! | `runner` | `Runner` trait + pytest / unittest implementations |
//! | `engine` | Top-level orchestration: parse → filter → run → report (parallel) |
//! | `cache` | Per-mutant result cache keyed on `(mutant.id, ast_hash(file), scope)` |
//! | `ast_hash` | Structural AST hash of a Python source file (whitespace/comment-insensitive) |
//! | `report` | Outcome types + writers (json/junit/html/markdown) + GHA annotations |
//! | `history` | Per-run history log (`.fermut/history.jsonl`) powering `trend`/`score` |
//! | `kill_order` | Smart test ordering — historical "which test killed this mutant kind" first |
//! | `llm` | LLM integration: Anthropic Messages API client + prompt builders + cache |
//! | `sync` | Internal sync helpers (e.g. `lock_recover` for poisoned `Mutex`es) |
//! | `watch` | File-system watch loop for `--watch` mode |

pub(crate) mod ast_hash;
pub(crate) mod cache;
pub(crate) mod cli;
pub(crate) mod config;
pub(crate) mod emit;
pub(crate) mod engine;
pub(crate) mod equiv;
pub(crate) mod filter;
pub(crate) mod history;
pub(crate) mod kill_order;
pub(crate) mod llm;
pub(crate) mod mutator;
pub(crate) mod report;
pub(crate) mod runner;
pub(crate) mod sync;
pub(crate) mod test_tree;
pub(crate) mod util;
pub(crate) mod watch;

/// The CLI entry point — the crate's only supported public API (`main.rs`).
pub use cli::Cli;

// Bench-only hooks for `examples/bench_isolation.rs`; not a supported API.
#[doc(hidden)]
pub use config::IsolationMode;
#[doc(hidden)]
pub use runner::time_mirror_build;
