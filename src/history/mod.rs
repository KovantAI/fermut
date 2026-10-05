//! Per-run history log (`.fermut/history.jsonl`).
//!
//! Each `fermut run` appends one JSON line summarising the run: timestamp,
//! mutation score, status counts, the fermut version, and (when discoverable)
//! the git sha/branch. The file is JSON-lines so appends are cheap, partial reads
//! survive truncation, and the schema can extend without breaking older
//! readers (downstream just ignores unknown fields).
//!
//! `fermut trend` reads this file. The cache and history are intentionally
//! separate files: cache rotation (`fermut clean`) shouldn't lose history.
//!
//! Split across focused submodules:
//!   - [`entry`]:    the `HistoryEntry` record and its derived predicates
//!   - [`hash`]:     stable hashing of the run-shape config
//!   - [`paths`]:    locating the project's history log from a target path
//!   - [`analytics`]: cross-entry diffs, streaks, ages, regressions
//!   - [`render`]:   sparkline rendering
//!   - [`io`]:       reading/writing the log, plus timestamp/git stamping

mod analytics;
mod entry;
mod hash;
mod io;
mod paths;
mod render;

pub use analytics::{
    branch_scoped_regression, mixed_config_hashes, regression_against, survivor_age_map,
    survivor_diff, survivors_by_file, trailing_streak, StreakDir,
};
pub(crate) use entry::trend_step;
pub use entry::HistoryEntry;
#[cfg(test)]
pub use entry::CURRENT_SCHEMA_V;
pub use hash::config_hash;
pub use io::{append, load, load_with_stats};
pub use paths::{default_history_path, resolve_root};
pub use render::{sparkline, sparkline_scaled};
