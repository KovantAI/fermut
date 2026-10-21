//! Reading and writing `history.jsonl`, plus the timestamp/git helpers
//! used to stamp new entries.

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::warn;

use super::entry::{HistoryEntry, CURRENT_SCHEMA_V};

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
    #[cfg(test)]
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

pub(crate) fn iso8601_now() -> String {
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

pub(crate) fn git_short_sha(dir: &Path) -> Option<String> {
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
            fermut_version: Some("9.9.9".into()),
            git_sha: Some("abc1234".into()),
            git_branch: Some("main".into()),
            survivor_ids: None,
            baseline: false,
            partial: false,
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
            fermut_version: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
            partial: false,
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
            fermut_version: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
            partial: false,
        };
        let line = serde_json::to_string(&valid).unwrap();
        std::fs::write(&p, format!("not json\n{line}\n{{partial:")).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].killed, 1);
    }

    #[test]
    fn load_with_stats_counts_malformed_and_newer_schema() {
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
            partial: false,
            fermut_version: None,
        };
        let good = serde_json::to_string(&valid).unwrap();
        // A well-formed entry stamped one schema version ahead of this binary.
        let newer = format!(
            r#"{{"v":{},"timestamp":"t","mutation_score":80.0,"killed":4,"survived":1,"timed_out":0,"skipped":0,"errored":0}}"#,
            CURRENT_SCHEMA_V + 1
        );
        // valid, malformed, newer-schema, blank line (ignored, not counted).
        std::fs::write(&p, format!("{good}\nnot json\n{newer}\n\n")).unwrap();

        let (entries, stats) = load_with_stats(&p).unwrap();
        assert_eq!(entries.len(), 1, "only the one in-range entry loads");
        assert_eq!(stats.loaded, 1);
        assert_eq!(stats.malformed, 1);
        assert_eq!(stats.newer_schema, 1);
        assert_eq!(stats.dropped(), 2);

        // Missing file → all zeros, no error.
        let (empty, s0) = load_with_stats(&tmp.path().join("nope.jsonl")).unwrap();
        assert!(empty.is_empty());
        assert_eq!(s0.dropped(), 0);
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
                        fermut_version: None,
                        git_sha: None,
                        git_branch: None,
                        survivor_ids: None,
                        baseline: false,
                        partial: false,
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
            fermut_version: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
            partial: false,
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
}
