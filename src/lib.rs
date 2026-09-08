//! fermut — Rust-powered, ty-aware mutation testing for Python.
//!
//! Top-level crate module map:
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | [`cli`] | clap parser + subcommand dispatch (`run`, `list`, `show`, `clean`, `completions`) |
//! | [`config`] | Runtime `Config`, TOML schema, walk-up loader |
//! | [`mutator`] | Python AST visitor + operator catalog → `Mutant` candidates |
//! | [`emit`] | Splice a `Mutant`'s replacement into the source byte range |
//! | [`equiv`] | Equivalent-mutant detector: AST patterns + CPython bytecode identity |
//! | [`filter`] | `Filter` trait + 8-stage composable filter chain |
//! | [`runner`] | `Runner` trait + pytest / unittest implementations |
//! | [`engine`] | Top-level orchestration: parse → filter → run → report (parallel) |
//! | [`cache`] | Per-mutant result cache keyed on `(mutant.id, ast_hash(file), scope)` |
//! | [`ast_hash`] | Structural AST hash of a Python source file (whitespace/comment-insensitive) |
//! | [`report`] | Outcome types + writers (json/junit/html/markdown) + GHA annotations |
//! | [`watch`] | File-system watch loop for `--watch` mode |

pub mod ast_hash;
pub mod cache;
pub mod cli;
pub mod config;
pub mod emit;
pub mod engine;
pub mod equiv;
pub mod filter;
pub mod history;
pub mod kill_order;
pub mod llm;
pub mod mutator;
pub mod report;
pub mod runner;
pub(crate) mod sync;
pub(crate) mod util;
pub mod watch;
