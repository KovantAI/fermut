//! Resolved configuration + on-disk TOML schema + walk-up loader.
//!
//! - `Config` (this file) — runtime struct. The engine and filters consume it.
//! - `file` — TOML schema (`FileConfig`, `[tool.fermut]` in `pyproject.toml`).
//! - `loader` — discovers a config file by walking the source path's ancestors
//!   and parses it.

pub mod file;
pub mod loader;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use crate::filter::coverage::CoverageContexts;
use crate::mutator::Operator;

pub use file::FileConfig;
pub use loader::{ConfigSource, LoadedConfig};

/// Resolved, ready-to-use configuration. The CLI builds one of these by
/// merging `FileConfig` (TOML) with command-line arguments.
#[derive(Debug, Clone)]
pub struct Config {
    pub source_root: PathBuf,
    pub tests: Option<PathBuf>,
    pub jobs: Option<usize>,
    pub timeout_secs: u64,
    pub ty_filter: bool,
    pub ruff_filter: bool,
    pub experimental: bool,
    /// Include parity operators (expr→None, positional/element drop, string
    /// case-swap) — for cross-tool comparison only, never a default score.
    pub parity: bool,
    pub ops_allow: Option<HashSet<Operator>>,
    pub ops_deny: HashSet<Operator>,
    pub diff_base: Option<String>,
    pub since: Option<String>,
    pub coverage_path: Option<PathBuf>,
    /// Parsed per-line test contexts from `coverage.json`. When present, the
    /// pytest runner narrows the invocation to the listed test ids per mutant
    /// and the coverage filter skips mutants whose line has no context.
    pub coverage: Option<Arc<CoverageContexts>>,
    pub hypothesis_seed: Option<u64>,
    pub pytest_args: Vec<String>,
    pub cache: bool,
    pub cache_path: PathBuf,
    /// Smart test ordering: when on (and coverage is available), the
    /// coverage-selected tests for a mutant are reordered so the most targeted
    /// test (fewest lines covered) runs first, letting pytest's `-x`
    /// short-circuit sooner. Advisory — only permutes the selected set, so it
    /// never changes a verdict, only speed. Default on; disable with
    /// `--no-smart-order` / `smart_order = false`.
    pub smart_order: bool,
    /// When true, each `fermut run` appends a summary line to `history_path`.
    /// Disable with `--no-history` or `history = false` in the config file.
    pub history: bool,
    pub history_path: PathBuf,
    pub sample_ratio: Option<f64>,
    pub sample_seed: Option<u64>,
    pub shard: Option<(u32, u32)>,
    pub runner: RunnerKind,
    /// Resolved Python interpreter to run pytest with (`<python> -m pytest`),
    /// or `None` to spawn a bare `pytest` from `PATH`. Set via `--python` /
    /// the `python` config key, or auto-discovered from an active venv / a
    /// `.venv` near the source root. See [`crate::runner::resolve_python`].
    pub python: Option<PathBuf>,
    pub isolation: IsolationMode,
    /// Run the equivalent-mutant detector on `Survived` outcomes. Provably-
    /// equivalent mutants are remapped to `Equivalent` and excluded from the
    /// score denominator. Disable with `--no-equiv-detect`.
    pub equiv_detect: bool,
    /// Cache-key granularity for source-file identity. `File` (default) keys
    /// each cache entry on the AST-hash of the whole file, so any structural
    /// edit anywhere in the file invalidates every mutant in it. `Scope`
    /// keys on the file's prelude plus the enclosing top-level def/class
    /// body, so edits inside one function don't bust mutants in sibling
    /// functions. `Scope` is opt-in because it assumes a mutant's test
    /// outcome depends only on its enclosing scope plus the prelude — not
    /// sound when tests reach into siblings via indirect calls.
    pub cache_scope: CacheScope,
    /// Minimum mutation score (0.0–100.0). When set, exit status is driven
    /// by `score >= fail_under` instead of the default "any survivor fails".
    pub fail_under: Option<f64>,
    /// Glob patterns matched against paths relative to `source_root` during
    /// mutation collection. Matching files and directories are pruned before
    /// parsing, so they never produce mutants. Empty = no exclusions.
    pub exclude: Vec<String>,
    /// Run the unmutated test suite once before mutating and abort if it
    /// isn't green. A red suite makes every covered mutant exit non-zero and
    /// be counted "killed", inflating the score toward 100%. Default on;
    /// disable with `--no-verify-baseline` or `verify_baseline = false`.
    pub verify_baseline: bool,
    /// Wall-clock cap (seconds) for the [`verify_baseline`] run. Separate from
    /// `timeout_secs` (which bounds a single mutant's coverage-selected subset)
    /// because the baseline runs the entire suite. A hung suite is killed past
    /// this so it can't stall the whole run. Set via `--baseline-timeout`.
    pub baseline_timeout_secs: u64,
    /// Optional wall-clock ceiling (seconds) on the per-mutant **testing
    /// phase**. When set, mutants are evaluated highest-value first (covered
    /// mutants ahead of uncovered ones) and, once the deadline passes, every
    /// mutant not yet started is recorded as `skipped` with filter
    /// `time-budget` rather than run. In-flight mutants finish. Bounds only the
    /// testing phase — baseline verification, generation, and the ty pre-filter
    /// are separate fixed costs the budget does not cover. `None` = run the
    /// whole catalogue. Set via `--max-time`.
    pub max_time_secs: Option<u64>,
}

/// Cache-key granularity for source-file identity. See `Config::cache_scope`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CacheScope {
    #[default]
    File,
    Scope,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RunnerKind {
    #[default]
    Pytest,
    /// pytest-CLI-compatible drop-in; driven by the same runner as `Pytest`.
    Rstest,
    Unittest,
}

/// How per-worker mirrors are populated from the project tree.
///
/// `Auto` picks the cheapest scheme the filesystem actually supports —
/// reflink/clonefile when available, otherwise a plain copy. `Hardlink`
/// trades safety for speed: it shares inodes with the source tree, so a
/// test that writes back into the mirror would mutate the original. We
/// always unlink the patched file before writing it, so the patched file
/// itself is safe; the risk is only for tests that mutate *other* files.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum IsolationMode {
    #[default]
    Auto,
    Copy,
    Hardlink,
    Reflink,
}

impl Config {
    pub fn tests_path(&self) -> PathBuf {
        self.tests
            .clone()
            .unwrap_or_else(|| self.source_root.join("tests"))
    }
}
