//! History entry schema + on-disk IO (`history.jsonl`) + the run-shape
//! `config_hash` and project-root/path resolution.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::config::Config;
use crate::report::{MutantOutcome, Report};

use super::git::{current_git_branch, git_short_sha};

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
    /// Stable hex digest of the run-shape config: runner, timeout, hseed,
    /// pytest_args, coverage on/off, operator allow/deny, experimental,
    /// parity, diff scope (`--since`/`--diff-only`), `--sample`, `--exclude`,
    /// `--shard`, the ruff/ty filters, equivalent-mutant detection, and the
    /// test-suite / coverage-file selection (relative to `source_root`).
    /// Entries with the same hash were generated from the same
    /// shape *and the same mutant universe*, so their scores are comparable; a
    /// mismatch in the trend window means the comparison spans configurations
    /// or scopes, not just code. Optional for back-compat with entries written
    /// before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
    /// fermut version that produced this entry (`CARGO_PKG_VERSION`). fermut is
    /// specified `>=`, not `==`, so a lockfile refresh can change the running
    /// version between runs; recording it lets the trend attribute a score
    /// shift to a tool upgrade rather than a code change. `None` for entries
    /// written before this field existed. Removes the manual injection the
    /// trend merge workflow used to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fermut_version: Option<String>,
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
    /// True when a `--max-time` budget expired before every mutant was tested,
    /// so this run scored over a *timing-nondeterministic subset* of the
    /// mutant universe. Two such runs at identical test quality can report
    /// different scores purely on which mutants beat the deadline, so trend and
    /// regression must skip these — comparing against (or as) a partial run
    /// fabricates spurious drops. Set from the report's `time-budget` skips;
    /// a `--max-time` run whose budget never expired ran the whole catalogue
    /// and is *not* partial. Absent (defaults false) on unbudgeted runs and on
    /// pre-field history. See [`is_scoreless`](Self::is_scoreless) for the
    /// sibling exclusion.
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
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
        // A run is "partial" when the --max-time budget cut it short — any
        // mutant recorded skipped/`time-budget`. Such a run scored over a
        // nondeterministic subset, so it must be excluded from regression/trend
        // (see the `partial` field). A budgeted run that finished everything has
        // no time-budget skips and is a full, comparable run.
        let partial = report.outcomes.iter().any(|o| {
            matches!(o, MutantOutcome::Skipped { filter, .. }
                if filter == crate::engine::TIME_BUDGET_FILTER)
        });
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
            fermut_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            git_sha: git_short_sha(project_root),
            git_branch: current_git_branch(project_root),
            survivor_ids: Some(survivor_ids),
            baseline: false,
            partial,
        }
    }

    /// True when this entry scored no mutant (`killed + timed_out + survived
    /// == 0`), so its `mutation_score` is the vacuous `100.0` floor rather than
    /// a real measurement. Trend and regression must skip these — comparing
    /// against a fake perfect score fabricates spurious drops and streaks.
    /// Mirrors [`crate::report::Report::is_scoreless`] for the persisted shape.
    pub fn is_scoreless(&self) -> bool {
        self.killed + self.timed_out + self.survived == 0
    }

    /// True when this entry may take part in a regression comparison — it
    /// scored a real mutant set and wasn't a `--max-time` partial run. A
    /// scoreless entry carries the vacuous `100.0` floor and a partial entry
    /// scored a nondeterministic subset; comparing against (or as) either
    /// fabricates spurious drops, so both are excluded from the gate.
    pub fn is_comparable(&self) -> bool {
        !self.is_scoreless() && !self.partial
    }
}

/// Feed a length-delimited byte field. Prefixing each variable-length,
/// user-controlled value with its length stops adjacent fields from
/// stream-colliding — otherwise a crafted git ref, glob, or pytest arg that
/// happened to contain a later field marker (`|shard=`, …) could hash
/// identically to a structurally different config.
fn feed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// Render `p` relative to `root` when it sits under it, else the path as-is.
/// Keeps the hash stable across checkouts (absolute paths vary per machine)
/// while still distinguishing a run pointed at a *different* suite/file.
fn rel_to(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

/// Stable hex digest of the run-shape config — see `HistoryEntry::config_hash`.
/// Format-versioned (`v2|…`) so we can rev the shape without colliding with
/// older entries; bump the prefix if the inputs ever change.
pub fn config_hash(cfg: &Config) -> String {
    let mut h = Sha256::new();
    // v2: folds in the mutant-universe / comparability inputs the score
    // actually depends on but v1 ignored — parity operators, diff scope
    // (`--since`/`--diff-only`), `--sample`, `--exclude`, `--shard`, the
    // ruff/ty filters, equivalent-mutant detection, and the test-suite /
    // coverage-file selection. These are exactly the
    // filter-chain selectors in `filter::build_chain` plus the engine's equiv
    // pass; each changes which mutants are tested or counted. Two runs with
    // different values here are NOT comparable, so a diff-scoped or sampled run
    // must no longer hash identically to a full run (which is what forced the
    // PR gate onto `--no-history` and the trend merge onto a manual `ci_scope`).
    h.update(b"v2|runner=");
    h.update(format!("{:?}", cfg.runner).as_bytes());
    h.update(b"|python=");
    match &cfg.python {
        Some(p) => feed(&mut h, p.as_os_str().as_encoded_bytes()),
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
    h.update((cfg.pytest_args.len() as u64).to_le_bytes());
    for a in &cfg.pytest_args {
        feed(&mut h, a.as_bytes());
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
    // Parity operators expand the mutant set, so a parity run's score is not
    // comparable to a non-parity one.
    h.update(b"|parity=");
    h.update(if cfg.parity { &b"on"[..] } else { &b"off"[..] });
    // Diff scope. A run restricted to changed lines scores over a different
    // universe than a full run; `--since` and `--diff-only` are mutually
    // exclusive at the parser. The spec/base string is included so a full run
    // and a `--since origin/main` run get distinct hashes.
    h.update(b"|scope=");
    if let Some(spec) = &cfg.since {
        h.update(b"since:");
        feed(&mut h, spec.as_bytes());
    } else if let Some(base) = &cfg.diff_base {
        h.update(b"diff:");
        feed(&mut h, base.as_bytes());
    } else {
        h.update(b"full");
    }
    // Sampling tests only a fraction of mutants, so a sampled score is a noisy
    // estimate, not comparable to a full run. Ratio + seed both matter: a
    // different seed selects a different subset.
    h.update(b"|sample=");
    match cfg.sample_ratio {
        Some(r) => {
            h.update(b"ratio:");
            h.update(r.to_bits().to_le_bytes());
            h.update(b":seed:");
            h.update(cfg.sample_seed.unwrap_or(0).to_le_bytes());
        }
        None => h.update(b"none"),
    }
    // Excluded paths change which files produce mutants at all. Sorted so the
    // hash is order-independent.
    h.update(b"|exclude=");
    if cfg.exclude.is_empty() {
        h.update(b"none");
    } else {
        let mut ex = cfg.exclude.clone();
        ex.sort();
        h.update((ex.len() as u64).to_le_bytes());
        for g in ex {
            feed(&mut h, g.as_bytes());
        }
    }
    // Sharding partitions the mutant set (`ShardFilter`, a hash modulo): a
    // `--shard i/n` run scores over 1/n of the universe, not comparable to a
    // full run. Index + total both matter — different shards test different
    // mutants.
    h.update(b"|shard=");
    match cfg.shard {
        Some((index, total)) => {
            h.update(index.to_le_bytes());
            h.update(b"/");
            h.update(total.to_le_bytes());
        }
        None => h.update(b"none"),
    }
    // The ruff/ty filters drop mutants those tools reject before they are ever
    // run, shrinking the denominator. A `--ruff`/`--ty` run is not comparable
    // to one without.
    h.update(b"|ruff=");
    h.update(if cfg.ruff_filter {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    h.update(b"|ty=");
    h.update(if cfg.ty_filter {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    // Equivalent-mutant detection flips otherwise-`Survived` mutants to
    // `Equivalent`, removing them from the killable denominator and moving the
    // score. On by default; `--no-equiv-detect` changes the universe.
    h.update(b"|equiv=");
    h.update(if cfg.equiv_detect {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    // Test-suite / coverage-file selection is run shape: pointing `--tests` at
    // a different suite (or `--coverage` at a different file) scores over a
    // different mutant universe, so those runs are not comparable. Hashed
    // relative to `source_root` so the digest stays stable across checkouts
    // whose absolute paths differ; `source_root` itself is deliberately NOT
    // hashed for the same reason (it's project/checkout identity, not shape).
    h.update(b"|tests=");
    feed(
        &mut h,
        rel_to(&cfg.source_root, &cfg.tests_path()).as_bytes(),
    );
    h.update(b"|covpath=");
    match &cfg.coverage_path {
        Some(p) => feed(&mut h, rel_to(&cfg.source_root, p).as_bytes()),
        None => h.update(b"none"),
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

/// Counts of history lines dropped at load time, so a caller (e.g. `fermut
/// trend --strict`) can tell "the trend is computed over every recorded point"
/// from "some points were silently discarded". A non-empty line that fails to
/// parse is `malformed`; a well-formed entry stamped with a schema newer than
/// this binary understands is `newer_schema`. Empty lines are not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadStats {
    pub loaded: usize,
    pub malformed: usize,
    pub newer_schema: usize,
}

impl LoadStats {
    /// Non-empty lines that did not become a loaded entry.
    pub fn dropped(&self) -> usize {
        self.malformed + self.newer_schema
    }
}

/// Load every well-formed entry from `path`, in file order (oldest first),
/// alongside a count of what was dropped. Malformed and newer-schema lines are
/// skipped — older/newer fermut versions may have written entries we don't
/// understand, and a single bad line should not poison `fermut trend` — but the
/// stats let a strict caller refuse to report over a silently truncated set.
pub fn load_with_stats(path: &Path) -> Result<(Vec<HistoryEntry>, LoadStats)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), LoadStats::default()))
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut out = Vec::new();
    let mut stats = LoadStats::default();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<HistoryEntry>(line) {
            Ok(entry) => {
                if entry.schema_version > CURRENT_SCHEMA_V {
                    warn!(
                        v = entry.schema_version,
                        current = CURRENT_SCHEMA_V,
                        "skipping history entry with newer schema version"
                    );
                    stats.newer_schema += 1;
                    continue;
                }
                out.push(entry);
            }
            Err(_) => stats.malformed += 1,
        }
    }
    stats.loaded = out.len();
    Ok((out, stats))
}

/// Load every well-formed entry from `path`, in file order (oldest first).
/// Malformed lines are silently skipped — see [`load_with_stats`] for the
/// dropped-line counts a strict caller needs.
pub fn load(path: &Path) -> Result<Vec<HistoryEntry>> {
    load_with_stats(path).map(|(entries, _)| entries)
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
pub(crate) fn format_iso8601_utc(secs: i64) -> String {
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
