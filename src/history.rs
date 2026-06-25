//! Per-run history log (`.fermut/history.jsonl`).
//!
//! Each `fermut run` appends one JSON line summarising the run: timestamp,
//! mutation score, status counts, and (when discoverable) the git
//! sha/branch. The file is JSON-lines so appends are cheap, partial reads
//! survive truncation, and the schema can extend without breaking older
//! readers (downstream just ignores unknown fields).
//!
//! `fermut trend` reads this file. The cache and history are intentionally
//! separate files: cache rotation (`fermut clean`) shouldn't lose history.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::config::Config;
use crate::report::{MutantOutcome, Report};

/// On-disk schema version for `HistoryEntry`. Bump only on **breaking**
/// shape changes (renames, type changes, semantic shifts) — additive
/// fields don't require a bump because old readers already ignore them
/// via `#[serde(default)]`. The loader refuses entries tagged with a
/// version greater than this constant so an older binary reading a
/// newer log degrades to "skip + warn" instead of misinterpreting data.
pub const CURRENT_SCHEMA_V: u32 = 1;

/// One entry per `fermut run`. Serialized as a single line in `history.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Schema version. Always written as `CURRENT_SCHEMA_V`; entries
    /// from before this field existed deserialize as `0` (which the
    /// loader treats as "untagged but compatible with v1 shape").
    /// Future readers can branch on this when shape semantics change.
    #[serde(default, rename = "v")]
    pub schema_version: u32,
    /// Wall-clock when the run finished, encoded as ISO-8601 UTC.
    pub timestamp: String,
    /// Mutation score, percent. Same definition as `Counts::mutation_score`.
    pub mutation_score: f64,
    pub killed: usize,
    pub survived: usize,
    pub timed_out: usize,
    pub skipped: usize,
    pub errored: usize,
    #[serde(default)]
    pub equivalent: usize,
    /// Total mutants in this run (sum of the five status buckets). Stored
    /// explicitly so downstream consumers don't need to know the bucket set.
    /// Optional for back-compat with entries written before this field
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// Wall-clock duration of the run, in milliseconds. Optional for
    /// back-compat with entries written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Stable hex digest of the run-shape config (runner, timeout, hseed,
    /// pytest_args, coverage on/off, operator allow/deny, experimental).
    /// Entries with the same hash were generated from the same shape so
    /// their scores are comparable; a mismatch in the trend window means
    /// the comparison is across configurations, not just across code.
    /// Optional for back-compat with entries written before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
    /// Short git sha, if the working tree is a git repo. `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    /// Current branch name, if discoverable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// IDs of mutants that survived this run. Populated automatically
    /// from the `Report`'s `Survived` outcomes. `None` for entries from
    /// before this field existed; an empty `Some(vec![])` means the run
    /// genuinely had zero survivors. Stored inline so trend can compute
    /// new-survivor diffs without a sidecar file; size impact per entry
    /// is roughly `survivors * 80 bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub survivor_ids: Option<Vec<String>>,
    /// True for the entry written by `fermut baseline` — the day-one anchor
    /// point for the trend graph. Lets `trend`/`dashboard` mark "run zero"
    /// distinctly from ordinary `run` entries. Absent (defaults false) on
    /// every `run`-written entry and on pre-field history.
    #[serde(default, skip_serializing_if = "is_false")]
    pub baseline: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl HistoryEntry {
    /// Build an entry from a `Report` and the project root. Reads `git` for
    /// sha/branch — failures (not a repo, no commits, no git binary) silently
    /// degrade to `None` rather than failing the run.
    pub fn from_report(
        report: &Report,
        project_root: &Path,
        duration_ms: Option<u64>,
        config_hash: Option<String>,
    ) -> Self {
        let counts = report.counts();
        let total =
            counts.killed + counts.survived + counts.timed_out + counts.skipped + counts.errored;
        let survivor_ids: Vec<String> = report
            .outcomes
            .iter()
            .filter_map(|o| match o {
                MutantOutcome::Survived { mutant } => Some(mutant.id.clone()),
                _ => None,
            })
            .collect();
        Self {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: iso8601_now(),
            mutation_score: counts.mutation_score(),
            killed: counts.killed,
            survived: counts.survived,
            timed_out: counts.timed_out,
            skipped: counts.skipped,
            errored: counts.errored,
            equivalent: counts.equivalent,
            total: Some(total),
            duration_ms,
            config_hash,
            git_sha: git_short_sha(project_root),
            git_branch: current_git_branch(project_root),
            survivor_ids: Some(survivor_ids),
            baseline: false,
        }
    }
}

/// Stable hex digest of the run-shape config — see `HistoryEntry::config_hash`.
/// Format-versioned (`v1|…`) so we can rev the shape without colliding with
/// older entries; bump the prefix if the inputs ever change.
pub fn config_hash(cfg: &Config) -> String {
    let mut h = Sha256::new();
    h.update(b"v1|runner=");
    h.update(format!("{:?}", cfg.runner).as_bytes());
    h.update(b"|python=");
    match &cfg.python {
        Some(p) => h.update(p.as_os_str().as_encoded_bytes()),
        None => h.update(b"none"),
    }
    h.update(b"|timeout=");
    h.update(cfg.timeout_secs.to_le_bytes());
    h.update(b"|hseed=");
    if let Some(s) = cfg.hypothesis_seed {
        h.update(s.to_le_bytes());
    } else {
        h.update(b"none");
    }
    h.update(b"|args=");
    for a in &cfg.pytest_args {
        h.update(b"|");
        h.update(a.as_bytes());
    }
    h.update(b"|cov=");
    h.update(if cfg.coverage.is_some() {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    h.update(b"|exp=");
    h.update(if cfg.experimental {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    h.update(b"|ops=");
    if let Some(allow) = &cfg.ops_allow {
        let mut sorted: Vec<String> = allow.iter().map(|o| format!("{o:?}")).collect();
        sorted.sort();
        for o in sorted {
            h.update(b"|");
            h.update(o.as_bytes());
        }
    } else {
        h.update(b"all");
    }
    h.update(b"|deny=");
    let mut sorted: Vec<String> = cfg.ops_deny.iter().map(|o| format!("{o:?}")).collect();
    sorted.sort();
    for o in sorted {
        h.update(b"|");
        h.update(o.as_bytes());
    }
    hex::encode(h.finalize())
}

/// Default history location: sibling of the cache file, anchored to the
/// project's `.fermut/` directory.
pub fn default_history_path(project_root: &Path) -> PathBuf {
    project_root.join(".fermut").join("history.jsonl")
}

/// Resolve the directory whose `.fermut/` holds this project's history log.
///
/// `fermut run <path>` and `fermut trend` are usually handed different
/// `<path>` args (or none at all), yet must agree on one project-level
/// timeline. Anchoring on the raw path arg broke that: `fermut run src/`
/// wrote `src/.fermut/history.jsonl` while a bare `fermut trend` read
/// `./.fermut/history.jsonl` and saw an empty history. Both now walk up to a
/// shared anchor:
///   1. the nearest ancestor (including the target) holding a project marker
///      (`pyproject.toml` / `setup.cfg`);
///   2. failing that, the nearest ancestor that already has a `.fermut/`
///      directory — so `trend` finds wherever a prior `run` wrote;
///   3. failing both, the target directory itself.
///
/// A file target resolves against its parent directory (you can't put a
/// `.fermut/` under a `.py` file).
pub fn resolve_root(path: &Path) -> PathBuf {
    let abs = absolutize(path);
    let base = if abs.is_file() {
        abs.parent().map(Path::to_path_buf).unwrap_or(abs)
    } else {
        abs
    };
    if let Some(root) = find_ancestor(&base, |p| {
        p.join("pyproject.toml").exists() || p.join("setup.cfg").exists()
    }) {
        return root;
    }
    if let Some(root) = find_ancestor(&base, |p| p.join(".fermut").is_dir()) {
        return root;
    }
    base
}

/// Nearest ancestor of `start` (inclusive) satisfying `pred`, or `None`.
fn find_ancestor(start: &Path, pred: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut p = start;
    loop {
        if pred(p) {
            return Some(p.to_path_buf());
        }
        p = p.parent()?;
    }
}

/// Best-effort absolute path: canonicalize when it exists, else join cwd.
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    std::env::current_dir()
        .map(|d| d.join(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

/// True when the window contains at least two entries with distinct
/// `config_hash` values. Entries missing a hash (older schema) are
/// ignored so a mix of "tagged + untagged" doesn't trigger the warning.
/// Used by `fermut trend` and the dashboard to flag windows where a
/// score change might be a config shift rather than a real regression.
pub fn mixed_config_hashes(entries: &[HistoryEntry]) -> bool {
    let mut seen: Option<&str> = None;
    for e in entries {
        let Some(h) = e.config_hash.as_deref() else {
            continue;
        };
        match seen {
            None => seen = Some(h),
            Some(prev) if prev != h => return true,
            _ => {}
        }
    }
    false
}

/// Set diff of survivor IDs between two entries. Returns
/// `(newly_surviving, newly_killed)`: the first list is mutants that
/// survive `curr` but didn't survive `prev` (regressions); the second
/// is mutants that survived `prev` but no longer survive `curr`
/// (improvements). Both lists are sorted for stable output. Returns
/// `None` when either entry lacks recorded survivor IDs — older
/// entries from before that field existed can't be diffed.
pub fn survivor_diff<'a>(
    prev: &'a HistoryEntry,
    curr: &'a HistoryEntry,
) -> Option<(Vec<&'a str>, Vec<&'a str>)> {
    let prev_ids = prev.survivor_ids.as_ref()?;
    let curr_ids = curr.survivor_ids.as_ref()?;
    let prev_set: std::collections::HashSet<&str> = prev_ids.iter().map(String::as_str).collect();
    let curr_set: std::collections::HashSet<&str> = curr_ids.iter().map(String::as_str).collect();
    let mut newly_surviving: Vec<&str> = curr_set.difference(&prev_set).copied().collect();
    let mut newly_killed: Vec<&str> = prev_set.difference(&curr_set).copied().collect();
    newly_surviving.sort_unstable();
    newly_killed.sort_unstable();
    Some((newly_surviving, newly_killed))
}

/// Same-direction score moves trailing the window. Walks from the most
/// recent entry backwards counting consecutive deltas with the same sign;
/// ties (`|d| < 0.05`) break the streak. Returns the count and whether
/// the streak is up or down. Requires at least two moves (three entries)
/// to report anything — a single move isn't a trend.
///
/// Shared by `fermut trend` (CLI table) and `fermut dashboard` (summary
/// card) so the two surfaces never drift on what counts as a streak.
pub fn trailing_streak(entries: &[HistoryEntry]) -> Option<(StreakDir, usize)> {
    if entries.len() < 3 {
        return None;
    }
    let scores: Vec<f64> = entries.iter().map(|e| e.mutation_score).collect();
    let mut dir: Option<StreakDir> = None;
    let mut count = 0usize;
    for w in scores.windows(2).rev() {
        let delta = w[1] - w[0];
        let this = if delta > 0.05 {
            StreakDir::Up
        } else if delta < -0.05 {
            StreakDir::Down
        } else {
            break;
        };
        match dir {
            None => {
                dir = Some(this);
                count = 1;
            }
            Some(d) if d == this => count += 1,
            _ => break,
        }
    }
    if count >= 2 {
        dir.map(|d| (d, count))
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreakDir {
    Up,
    Down,
}

/// Consecutive trailing runs each id in `entries.last()` has survived.
/// Walks backwards from the most recent entry: an id's age is the count of
/// uninterrupted earlier entries that also list it as a survivor. Returns
/// an empty map when the window is empty or the last entry lacks survivor
/// IDs. Entries missing survivor IDs (older schema) terminate a streak —
/// we can't claim an id "survived" a run we have no survivor list for.
///
/// Age 1 means "first appearance in last run" (no prior). Age 5 means
/// "alive in last 5 consecutive runs." That's the signal teams want for
/// triage: high-age survivors are the persistent blockers, age-1 entries
/// are new regressions worth checking before they settle in.
pub fn survivor_age_map(entries: &[HistoryEntry]) -> std::collections::HashMap<&str, usize> {
    let mut out: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let Some(last) = entries.last() else {
        return out;
    };
    let Some(last_ids) = last.survivor_ids.as_deref() else {
        return out;
    };
    for id in last_ids {
        out.insert(id.as_str(), 1);
    }
    // Walk older → newer-1, oldest first wouldn't help; we need the most
    // recent contiguous block, so iterate backwards from the second-to-last.
    let mut still_alive: std::collections::HashSet<&str> =
        last_ids.iter().map(String::as_str).collect();
    for prior in entries[..entries.len() - 1].iter().rev() {
        let Some(prior_ids) = prior.survivor_ids.as_deref() else {
            // Missing data breaks the streak for everyone still being counted.
            break;
        };
        let prior_set: std::collections::HashSet<&str> =
            prior_ids.iter().map(String::as_str).collect();
        // Survivors no longer present in this older entry can't extend their
        // streak any further; drop them so subsequent iterations don't keep
        // bumping them.
        still_alive.retain(|id| prior_set.contains(id));
        if still_alive.is_empty() {
            break;
        }
        for id in &still_alive {
            if let Some(v) = out.get_mut(id) {
                *v += 1;
            }
        }
    }
    out
}

/// Group survivor IDs by the file they target. The id format produced by
/// `mutator::visitor::Collector::push` is `{file}@{offset}:{orig}->{repl}`,
/// so we split at the last `@` followed by a digit-led `:` segment to
/// extract the file prefix. IDs that don't match the expected shape (older
/// schemas, manual edits) fall into a `"<unknown>"` bucket so they remain
/// visible rather than silently dropped. Returns a vec of `(file, ids)`
/// sorted by descending survivor count, ties broken alphabetically — so
/// the worst offender always renders first.
pub fn survivors_by_file(ids: &[String]) -> Vec<(String, Vec<&str>)> {
    let mut groups: std::collections::BTreeMap<String, Vec<&str>> =
        std::collections::BTreeMap::new();
    for id in ids {
        let file = parse_id_file(id).unwrap_or("<unknown>").to_string();
        groups.entry(file).or_default().push(id.as_str());
    }
    let mut v: Vec<(String, Vec<&str>)> = groups.into_iter().collect();
    v.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Extract the `{file}` prefix from a mutant ID shaped
/// `{file}@{offset:digits}:{orig}->{repl}`. Returns `None` when the
/// expected delimiters aren't found — we'd rather report `<unknown>`
/// than silently misattribute.
fn parse_id_file(id: &str) -> Option<&str> {
    // Walk from the right so file paths that happen to contain `@`
    // (rare but legal on POSIX) still resolve to the longest valid prefix.
    let mut search_end = id.len();
    while let Some(at_off) = id[..search_end].rfind('@') {
        let after = &id[at_off + 1..];
        // Need: digits, then `:`, then anything.
        let digits_end = after.find(|c: char| !c.is_ascii_digit())?;
        if digits_end > 0 && after.as_bytes().get(digits_end) == Some(&b':') {
            return Some(&id[..at_off]);
        }
        search_end = at_off;
    }
    None
}

/// Positive point-drop between `current` and the most recent entry in
/// `prior` on the same branch. When either side has no recorded branch
/// we treat the pair as comparable (best effort — better signal than
/// skipping the gate entirely on detached HEAD / pre-git-tagged
/// entries). Returns `None` when there is no comparable prior or the
/// score didn't regress.
///
/// Takes `current` separately from `prior` because the regression gate
/// must reason about the in-memory run, not whatever the history file
/// happens to contain — append can silently fail (read-only FS, disk
/// full) and leave the file's tail one entry behind reality.
pub fn regression_against(prior: &[HistoryEntry], current: &HistoryEntry) -> Option<f64> {
    let current_branch = current.git_branch.as_deref();
    let prev = prior
        .iter()
        .rev()
        .find(|e| match (e.git_branch.as_deref(), current_branch) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        })?;
    let drop = prev.mutation_score - current.mutation_score;
    if drop > 0.0 {
        Some(drop)
    } else {
        None
    }
}

/// Convenience over [`regression_against`] for windows that already
/// include the run under test as their last element. Used by
/// `fermut trend` (which only ever looks at on-disk state).
pub fn branch_scoped_regression(entries: &[HistoryEntry]) -> Option<f64> {
    if entries.len() < 2 {
        return None;
    }
    let (prior, tail) = entries.split_at(entries.len() - 1);
    regression_against(prior, &tail[0])
}

/// Render a Unicode block-based sparkline for a sequence of values
/// against an explicit `[lo, hi]` range. Values outside the range are
/// clamped. Empty input → empty string. If `hi <= lo` the range
/// collapses and every value renders as the lowest block.
pub fn sparkline_scaled<I: IntoIterator<Item = f64>>(values: I, lo: f64, hi: f64) -> String {
    const BLOCKS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let span = hi - lo;
    let mut out = String::new();
    for v in values {
        let idx = if span <= 0.0 {
            0
        } else {
            let norm = ((v - lo) / span).clamp(0.0, 1.0);
            (norm * (BLOCKS.len() - 1) as f64).round() as usize
        };
        out.push(BLOCKS[idx.min(BLOCKS.len() - 1)]);
    }
    out
}

/// Render a sparkline against the fixed `[0, 100]` mutation-score range.
/// Used by the Markdown report so two reports remain visually
/// comparable; `fermut trend` can opt into auto-scaling via `--scale auto`.
pub fn sparkline<I: IntoIterator<Item = f64>>(values: I) -> String {
    sparkline_scaled(values, 0.0, 100.0)
}

/// Append `entry` as a JSON line to `path`. Creates the parent directory
/// on first write.
///
/// Concurrency: takes an exclusive advisory lock via `fs2::FileExt`
/// before writing, dropped when the file handle goes out of scope. This
/// serializes writers across processes and threads — required because
/// distributed runs (`--shard i/n`) and CI matrices can append from
/// many writers at once, and POSIX's "atomic small write under O_APPEND"
/// guarantee doesn't hold on every filesystem (notably NFS without
/// `cto`). The lock is advisory on Unix, mandatory on Windows; either
/// way, fermut writers all go through this function so the contract is
/// honored. A partial crash during write degrades to a dropped entry
/// (skipped by the loader), not a corrupted line.
pub fn append(path: &Path, entry: &HistoryEntry) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let line = serde_json::to_string(entry).context("serializing history entry")?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    fs2::FileExt::lock_exclusive(&f).with_context(|| format!("locking {}", path.display()))?;
    use std::io::Write;
    let res = writeln!(f, "{line}").with_context(|| format!("writing {}", path.display()));
    // Lock would release on drop anyway; explicit unlock keeps the
    // intent obvious and lets a panicking writer release sooner.
    let _ = fs2::FileExt::unlock(&f);
    res
}

/// Load every well-formed entry from `path`, in file order (oldest first).
/// Malformed lines are silently skipped — older fermut versions may have
/// written entries we no longer understand, and a single bad line should
/// not poison `fermut trend`.
pub fn load(path: &Path) -> Result<Vec<HistoryEntry>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
            if entry.schema_version > CURRENT_SCHEMA_V {
                warn!(
                    v = entry.schema_version,
                    current = CURRENT_SCHEMA_V,
                    "skipping history entry with newer schema version"
                );
                continue;
            }
            out.push(entry);
        }
    }
    Ok(out)
}

fn iso8601_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_iso8601_utc(secs)
}

/// Format `secs` (Unix epoch, UTC) as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// We do this by hand to avoid pulling in `chrono` / `time` just for one
/// formatter. Algorithm is the standard civil-from-days conversion (Howard
/// Hinnant, public domain). Handles dates in the Gregorian range we'll
/// realistically log against — i.e. any time after 1970.
fn format_iso8601_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (h, rem) = (time_of_day / 3600, time_of_day % 3600);
    let (m, s) = (rem / 60, rem % 60);

    // Civil from days since 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, d, h, m, s
    )
}

fn git_short_sha(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Current git branch under `dir`, or `None` for detached HEAD / no repo.
pub(crate) fn current_git_branch(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    // Detached HEAD prints `HEAD`; we'd rather record nothing than a
    // misleading label.
    if s.is_empty() || s == "HEAD" {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn append_then_load_roundtrips_entries() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let entry = HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: 87.5,
            killed: 7,
            survived: 1,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: Some(8),
            duration_ms: Some(1234),
            config_hash: Some("deadbeef".into()),
            git_sha: Some("abc1234".into()),
            git_branch: Some("main".into()),
            survivor_ids: None,
            baseline: false,
        };
        append(&p, &entry).unwrap();
        append(&p, &entry).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!((loaded[0].mutation_score - 87.5).abs() < f64::EPSILON);
        assert_eq!(loaded[0].killed, 7);
        assert_eq!(loaded[0].git_sha.as_deref(), Some("abc1234"));
    }

    #[test]
    fn baseline_flag_round_trips_and_is_omitted_when_false() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let mut anchor = HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: 54.0,
            killed: 5,
            survived: 4,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: Some(9),
            duration_ms: None,
            config_hash: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
        };
        // A false flag is skipped on serialize — no schema bloat on run rows.
        let normal_json = serde_json::to_string(&anchor).unwrap();
        assert!(!normal_json.contains("baseline"), "got: {normal_json}");

        // A true flag is written and survives a load round-trip.
        anchor.baseline = true;
        append(&p, &anchor).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].baseline, "baseline flag lost on round-trip");

        // A pre-field entry (no `baseline` key) defaults to false.
        let legacy: HistoryEntry = serde_json::from_str(
            r#"{"v":1,"timestamp":"t","mutation_score":80.0,"killed":4,"survived":1,"timed_out":0,"skipped":0,"errored":0}"#,
        )
        .unwrap();
        assert!(!legacy.baseline);
    }

    #[test]
    fn load_skips_malformed_lines() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let valid = HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: 50.0,
            killed: 1,
            survived: 1,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: Some(2),
            duration_ms: None,
            config_hash: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
        };
        let line = serde_json::to_string(&valid).unwrap();
        std::fs::write(&p, format!("not json\n{line}\n{{partial:")).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].killed, 1);
    }

    #[test]
    fn concurrent_appends_dont_corrupt_lines() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let writers = 8usize;
        let per_writer = 25usize;
        let mut handles = Vec::new();
        for tid in 0..writers {
            let p = p.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..per_writer {
                    let entry = HistoryEntry {
                        schema_version: CURRENT_SCHEMA_V,
                        timestamp: format!("2026-06-04T12:{tid:02}:{i:02}Z"),
                        mutation_score: tid as f64,
                        killed: i,
                        survived: 0,
                        timed_out: 0,
                        skipped: 0,
                        errored: 0,
                        equivalent: 0,
                        total: None,
                        duration_ms: None,
                        config_hash: None,
                        git_sha: None,
                        git_branch: None,
                        survivor_ids: None,
                        baseline: false,
                    };
                    append(&p, &entry).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), writers * per_writer);
    }

    #[test]
    fn load_skips_entries_with_future_schema_version() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let future_v = CURRENT_SCHEMA_V + 5;
        let line = format!(
            r#"{{"v":{future_v},"timestamp":"2026-06-04T12:00:00Z","mutation_score":80.0,"killed":4,"survived":1,"timed_out":0,"skipped":0,"errored":0}}"#
        );
        let valid = HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:01:00Z".into(),
            mutation_score: 90.0,
            killed: 9,
            survived: 1,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: None,
            duration_ms: None,
            config_hash: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
        };
        let valid_line = serde_json::to_string(&valid).unwrap();
        std::fs::write(&p, format!("{line}\n{valid_line}\n")).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!((loaded[0].mutation_score - 90.0).abs() < f64::EPSILON);
    }

    #[test]
    fn load_accepts_pretag_entries_as_v0() {
        // Entries written before the `v` field existed have no version tag.
        // Serde's `default` makes them deserialize as `schema_version: 0`,
        // which is <= CURRENT_SCHEMA_V so they're kept.
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let line = r#"{"timestamp":"2026-06-04T12:00:00Z","mutation_score":75.0,"killed":3,"survived":1,"timed_out":0,"skipped":0,"errored":0}"#;
        std::fs::write(&p, format!("{line}\n")).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].schema_version, 0);
    }

    #[test]
    fn load_returns_empty_when_missing() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("nope.jsonl");
        assert!(load(&p).unwrap().is_empty());
    }

    #[test]
    fn iso8601_known_epoch_values() {
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_iso8601_utc(86_400), "1970-01-02T00:00:00Z");
        assert_eq!(format_iso8601_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn default_history_path_uses_dot_fermut() {
        let p = default_history_path(Path::new("/proj"));
        assert!(p.ends_with(".fermut/history.jsonl"));
    }

    #[test]
    fn resolve_root_agrees_for_run_subpath_and_bare_trend() {
        // The regression: `fermut run src/` and a bare `fermut trend` (path
        // `.`) must resolve to the same project root so trend reads what run
        // wrote. Anchor is the pyproject.toml marker at the project root.
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();

        let from_run = resolve_root(&src);
        let from_trend = resolve_root(&root);
        assert_eq!(
            from_run, from_trend,
            "run-on-src and trend-on-root must land on the same project root"
        );
        assert_eq!(from_run, root);
    }

    #[test]
    fn resolve_root_falls_back_to_existing_dot_fermut() {
        // No project marker, but a prior run left a `.fermut/` at the root.
        // A subpath lookup must climb to it instead of anchoring on the
        // subdir, so trend finds the existing history.
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".fermut")).unwrap();
        let src = root.join("pkg");
        std::fs::create_dir_all(&src).unwrap();

        assert_eq!(resolve_root(&src), root);
    }

    #[test]
    fn resolve_root_file_target_uses_parent() {
        // No marker, no prior `.fermut/`: a single-file target resolves to its
        // parent directory, never to `file.py/.fermut`.
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let file = root.join("mod.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        assert_eq!(resolve_root(&file), root);
    }

    #[test]
    fn sparkline_renders_8_buckets() {
        let s = sparkline([0.0, 12.5, 25.0, 50.0, 75.0, 100.0]);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars.first(), Some(&'▁'));
        assert_eq!(chars.last(), Some(&'█'));
        assert_eq!(chars.len(), 6);
    }

    #[test]
    fn sparkline_handles_empty_input() {
        assert!(sparkline(std::iter::empty::<f64>()).is_empty());
    }

    #[test]
    fn sparkline_clamps_out_of_range_values() {
        let s = sparkline([-50.0, 200.0]);
        assert_eq!(s.chars().collect::<Vec<_>>(), vec!['▁', '█']);
    }

    #[test]
    fn sparkline_scaled_fills_height_for_narrow_window() {
        // Fixed-scale [0,100] crushes 85→95 into the same bucket; scaled
        // to its own range it spans floor→top.
        let s = sparkline_scaled([85.0, 95.0], 85.0, 95.0);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars.first(), Some(&'▁'));
        assert_eq!(chars.last(), Some(&'█'));
    }

    #[test]
    fn sparkline_scaled_clamps_to_explicit_bounds() {
        let s = sparkline_scaled([50.0, 200.0, -10.0], 80.0, 90.0);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars, vec!['▁', '█', '▁']);
    }

    #[test]
    fn sparkline_scaled_collapses_zero_span_safely() {
        let s = sparkline_scaled([42.0, 42.0, 42.0], 42.0, 42.0);
        assert_eq!(s.chars().collect::<Vec<_>>(), vec!['▁', '▁', '▁']);
    }

    fn mk(score: f64, branch: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: score,
            killed: 0,
            survived: 0,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: None,
            duration_ms: None,
            config_hash: None,
            git_sha: None,
            git_branch: branch.map(str::to_string),
            survivor_ids: None,
            baseline: false,
        }
    }

    #[test]
    fn branch_scoped_regression_skips_other_branches() {
        let es = vec![
            mk(90.0, Some("main")),
            mk(50.0, Some("feature/x")), // big drop on feature, ignored
            mk(88.0, Some("main")),      // 2pt drop on main → reported
        ];
        assert_eq!(branch_scoped_regression(&es), Some(2.0));
    }

    #[test]
    fn branch_scoped_regression_none_when_improving() {
        let es = vec![mk(80.0, Some("main")), mk(85.0, Some("main"))];
        assert_eq!(branch_scoped_regression(&es), None);
    }

    #[test]
    fn branch_scoped_regression_none_when_no_prior_on_branch() {
        let es = vec![mk(50.0, Some("other")), mk(90.0, Some("main"))];
        // No prior 'main' entry; falls back to the only prior (other) and
        // 50→90 is an improvement, not a regression.
        assert_eq!(branch_scoped_regression(&es), None);
    }

    fn mk_with_survivors(score: f64, ids: Option<&[&str]>) -> HistoryEntry {
        let mut e = mk(score, None);
        e.survivor_ids = ids.map(|s| s.iter().map(|x| x.to_string()).collect());
        e
    }

    #[test]
    fn survivor_diff_finds_new_and_killed() {
        let prev = mk_with_survivors(90.0, Some(&["a", "b", "c"]));
        let curr = mk_with_survivors(85.0, Some(&["b", "c", "d", "e"]));
        let (new_surv, new_killed) = survivor_diff(&prev, &curr).unwrap();
        assert_eq!(new_surv, vec!["d", "e"]);
        assert_eq!(new_killed, vec!["a"]);
    }

    #[test]
    fn survivor_diff_returns_none_when_either_lacks_data() {
        let with = mk_with_survivors(90.0, Some(&["a"]));
        let without = mk_with_survivors(85.0, None);
        assert!(survivor_diff(&with, &without).is_none());
        assert!(survivor_diff(&without, &with).is_none());
    }

    #[test]
    fn survivor_diff_handles_empty_sets() {
        let a = mk_with_survivors(100.0, Some(&[]));
        let b = mk_with_survivors(100.0, Some(&[]));
        let (n, k) = survivor_diff(&a, &b).unwrap();
        assert!(n.is_empty() && k.is_empty());
    }

    #[test]
    fn branch_scoped_regression_best_effort_when_branch_missing() {
        let es = vec![mk(90.0, None), mk(70.0, Some("main"))];
        assert_eq!(branch_scoped_regression(&es), Some(20.0));
    }

    #[test]
    fn regression_against_compares_in_memory_current() {
        // Simulates an append-failed run: history file contains only the
        // prior entries, current is in memory. Gate must compare current
        // against the matching-branch prior, not against the prior's prior.
        let prior = vec![
            mk(90.0, Some("main")), // older
            mk(85.0, Some("main")), // latest matching-branch prior
        ];
        let current = mk(75.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(10.0));
    }

    #[test]
    fn regression_against_returns_none_when_improving() {
        let prior = vec![mk(80.0, Some("main"))];
        let current = mk(85.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), None);
    }

    #[test]
    fn regression_against_skips_other_branch_priors() {
        let prior = vec![
            mk(90.0, Some("main")),      // last matching-branch prior
            mk(50.0, Some("feature/x")), // ignored
        ];
        let current = mk(88.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(2.0));
    }

    #[test]
    fn regression_against_returns_none_when_prior_empty() {
        let current = mk(80.0, Some("main"));
        assert_eq!(regression_against(&[], &current), None);
    }

    #[test]
    fn parse_id_file_extracts_prefix() {
        assert_eq!(parse_id_file("src/foo.py@42:a->b"), Some("src/foo.py"));
        assert_eq!(
            parse_id_file("pkg/sub/mod.py@1234:return->pass"),
            Some("pkg/sub/mod.py")
        );
    }

    #[test]
    fn parse_id_file_handles_at_in_path() {
        // POSIX paths can technically contain `@`. The rightmost valid
        // `@<digits>:` boundary wins.
        assert_eq!(
            parse_id_file("weird@dir/m.py@7:x->y"),
            Some("weird@dir/m.py")
        );
    }

    #[test]
    fn parse_id_file_rejects_unrecognised_shape() {
        assert_eq!(parse_id_file("no-at-here"), None);
        assert_eq!(parse_id_file("file.py@notdigits:foo"), None);
        assert_eq!(parse_id_file("file.py@42-missing-colon"), None);
    }

    #[test]
    fn survivors_by_file_groups_and_sorts_by_count() {
        let ids = vec![
            "src/a.py@1:x->y".to_string(),
            "src/b.py@2:x->y".to_string(),
            "src/a.py@3:x->y".to_string(),
            "src/a.py@4:x->y".to_string(),
            "garbage".to_string(),
        ];
        let groups = survivors_by_file(&ids);
        // a.py (3) before b.py (1) before <unknown> (1, alphabetically later).
        assert_eq!(groups[0].0, "src/a.py");
        assert_eq!(groups[0].1.len(), 3);
        assert_eq!(groups[1].0, "<unknown>");
        assert_eq!(groups[2].0, "src/b.py");
    }

    #[test]
    fn survivors_by_file_empty_input_empty_output() {
        assert!(survivors_by_file(&[]).is_empty());
    }

    fn mk_surv(ids: &[&str]) -> HistoryEntry {
        let mut e = mk(0.0, None);
        e.survivor_ids = Some(ids.iter().map(|s| s.to_string()).collect());
        e
    }

    #[test]
    fn survivor_age_counts_consecutive_appearances() {
        let entries = vec![
            mk_surv(&["a", "b"]), // age would be 3 for a, but b was killed in r2
            mk_surv(&["a"]),      // b not here
            mk_surv(&["a", "c"]), // newest
        ];
        let ages = survivor_age_map(&entries);
        assert_eq!(ages.get("a").copied(), Some(3));
        assert_eq!(ages.get("c").copied(), Some(1));
        // b is not in the latest run — not tracked.
        assert!(!ages.contains_key("b"));
    }

    #[test]
    fn survivor_age_streak_breaks_on_missing_data() {
        let entries = vec![
            mk_surv(&["a"]),
            {
                let mut e = mk(0.0, None);
                e.survivor_ids = None; // older entry, pre-field
                e
            },
            mk_surv(&["a"]),
        ];
        let ages = survivor_age_map(&entries);
        // Latest run has "a" → age starts at 1. Walking back, we hit a
        // missing-ids entry; can't claim "a" survived there, streak stops.
        assert_eq!(ages.get("a").copied(), Some(1));
    }

    #[test]
    fn survivor_age_empty_inputs_safe() {
        assert!(survivor_age_map(&[]).is_empty());
        let no_ids = mk(0.0, None);
        assert!(survivor_age_map(&[no_ids]).is_empty());
        let empty = mk_surv(&[]);
        assert!(survivor_age_map(&[empty]).is_empty());
    }

    #[test]
    fn survivor_age_single_entry_gives_age_one() {
        let entries = vec![mk_surv(&["x", "y"])];
        let ages = survivor_age_map(&entries);
        assert_eq!(ages.get("x").copied(), Some(1));
        assert_eq!(ages.get("y").copied(), Some(1));
    }
}
