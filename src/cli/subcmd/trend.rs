//! `fermut trend` — visualize mutation score over time.
//!
//! Reads `.fermut/history.jsonl` (written by `fermut run`) and prints a
//! table of the most recent entries together with an ASCII sparkline and
//! the delta against the previous run. Optional JSON output for tooling.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::history::{self, HistoryEntry, StreakDir};

#[derive(Debug, Clone)]
pub struct TrendOpts {
    pub path: PathBuf,
    pub history_path: Option<PathBuf>,
    pub limit: usize,
    pub all: bool,
    pub format: TrendFormat,
    pub scale: TrendScale,
    /// Keep only entries whose `git_branch` exactly matches.
    pub branch: Option<String>,
    /// Keep only entries with `timestamp >= since` (ISO-8601 lex compare).
    /// Accepts `YYYY-MM-DD` (start-of-day UTC) or a full
    /// `YYYY-MM-DDTHH:MM:SSZ` stamp.
    pub since: Option<String>,
    /// Keep only entries with `timestamp <= until` (ISO-8601 lex compare).
    /// Accepts `YYYY-MM-DD` (end-of-day UTC, so the entire day is
    /// included) or a full `YYYY-MM-DDTHH:MM:SSZ` stamp.
    pub until: Option<String>,
    /// If set and the most recent score dropped more than this many points
    /// vs the previous filtered entry, exit non-zero.
    pub fail_on_regression: Option<f64>,
    /// When true, expand the new-survivor and newly-killed mutant lists
    /// below the score-delta summary. Default (false) only prints the
    /// counts.
    pub diff: bool,
    /// When `Some`, render an aggregation of the latest run's survivors
    /// after the table. Currently only `File` is supported (operator-
    /// grouping requires the run report, which trend doesn't load).
    pub group_by: Option<TrendGroupBy>,
    /// Fail instead of silently skipping malformed history lines. The loader
    /// drops corrupt lines so one bad line can't poison the trend; under
    /// `--strict` a malformed line is an error, so a CI job can catch a
    /// truncated history file. Newer-schema lines are warned, not failed — an
    /// older binary can't read them and can't repair them, so failing on them
    /// would only wedge CI during a version rollout.
    pub strict: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum TrendFormat {
    Human,
    Json,
}

/// Aggregation axis for the latest-run survivor list. File grouping reads
/// only the survivor ID prefixes already stored in `history.jsonl`, so it
/// works without re-running mutmut and without loading a JSON report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendGroupBy {
    File,
}

/// Sparkline scaling strategy. `Fixed` always renders against `[0, 100]`
/// so charts from different windows are visually comparable. `Auto`
/// rescales to the window's own min/max, exposing small drifts at the
/// cost of cross-window comparability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendScale {
    Fixed,
    Auto,
}

pub fn trend(opts: TrendOpts) -> Result<()> {
    let history_path = opts
        .history_path
        .clone()
        .unwrap_or_else(|| history::default_history_path(&history::resolve_root(&opts.path)));
    let (entries, stats) = history::load_with_stats(&history_path)?;
    // Integrity check on malformed lines only. A malformed line doesn't parse,
    // so it carries no branch/date and can't be attributed to a query window —
    // the check is necessarily global. Newer-schema lines are readable in
    // principle (just by a newer binary) and are only warned, not failed, so
    // `--strict` doesn't wedge an old binary during a version rollout.
    if opts.strict && stats.malformed > 0 {
        anyhow::bail!(
            "history at {} has {} malformed line(s) that could not be parsed — \
             the trend would be computed over a silently truncated set. \
             Inspect the file's entries by hand, then re-run. (Drop --strict to skip them.)",
            history_path.display(),
            stats.malformed,
        );
    }

    let since_norm = opts
        .since
        .as_deref()
        .map(|s| normalize_date_bound(s, false, "--since"))
        .transpose()?;
    let until_norm = opts
        .until
        .as_deref()
        .map(|s| normalize_date_bound(s, true, "--until"))
        .transpose()?;
    let filtered: Vec<HistoryEntry> = entries
        .into_iter()
        .filter(|e| match &opts.branch {
            Some(b) => e.git_branch.as_deref() == Some(b.as_str()),
            None => true,
        })
        .filter(|e| match &since_norm {
            Some(s) => e.timestamp.as_str() >= s.as_str(),
            None => true,
        })
        .filter(|e| match &until_norm {
            Some(s) => e.timestamp.as_str() <= s.as_str(),
            None => true,
        })
        .collect();

    if filtered.is_empty() {
        match opts.format {
            TrendFormat::Human => println!(
                "no history matching filters at {}\n  run `fermut run …` once to populate it",
                history_path.display()
            ),
            TrendFormat::Json => println!("[]"),
        }
        return Ok(());
    }

    let window: &[HistoryEntry] = if opts.all {
        &filtered
    } else {
        let n = filtered.len().saturating_sub(opts.limit);
        &filtered[n..]
    };

    match opts.format {
        TrendFormat::Human => {
            print_human(window, &history_path, opts.scale, opts.diff, opts.group_by)
        }
        TrendFormat::Json => print_json(window)?,
    }

    // Regression gate runs after rendering so the user still sees the table
    // before we exit non-zero. Uses the same branch-scoped helper as
    // `fermut run --fail-on-regression`, applied to the filtered (not
    // windowed) set so `--limit 1` still detects a drop. When `--branch X`
    // pre-filters the window, branch scoping inside the helper is a no-op;
    // without `--branch`, the helper anchors to the most recent entry's
    // branch so cross-branch noise doesn't trigger the gate.
    if let Some(threshold) = opts.fail_on_regression {
        if let Some(drop) = crate::history::branch_scoped_regression(&filtered) {
            if drop > threshold {
                eprintln!("mutation score regressed by {drop:.1} pts (threshold {threshold:.1})");
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

/// Resolve the sparkline range from the requested scale strategy.
/// Returns `(lo, hi, label)` — `label` is a printable suffix (empty for
/// fixed, ` (scale lo-hi)` for auto) so the caller can append it to the
/// score line without a conditional. Auto-scale falls back to fixed
/// when the window's span is too small to be meaningful (< 1 pt),
/// preventing micro-jitter from being amplified into a misleading
/// full-height chart.
fn resolve_scale(entries: &[HistoryEntry], scale: TrendScale) -> (f64, f64, String) {
    if scale == TrendScale::Fixed || entries.is_empty() {
        return (0.0, 100.0, String::new());
    }
    // Scoreless runs carry the vacuous 100.0 floor — folding them into the
    // range warps the auto-scale (a real 40-60 window stretched to 40-100).
    let lo = entries
        .iter()
        .filter(|e| !e.is_scoreless())
        .map(|e| e.mutation_score)
        .fold(f64::INFINITY, f64::min);
    let hi = entries
        .iter()
        .filter(|e| !e.is_scoreless())
        .map(|e| e.mutation_score)
        .fold(f64::NEG_INFINITY, f64::max);
    if !lo.is_finite() || !hi.is_finite() || hi - lo < 1.0 {
        return (0.0, 100.0, String::new());
    }
    let label = format!("  (scale {lo:.1}-{hi:.1})");
    (lo, hi, label)
}

/// Expand a date-only filter bound (`YYYY-MM-DD`) to a full ISO-8601 UTC
/// timestamp so lex compare against `entry.timestamp` does the right
/// thing. `end_of_day=false` yields `T00:00:00Z` (used by `--since`,
/// inclusive lower bound); `end_of_day=true` yields `T23:59:59Z` (used
/// by `--until`, inclusive upper bound covering the whole day). Full
/// timestamps pass through unchanged. Anything else is rejected to
/// avoid silently misfiltering.
fn normalize_date_bound(s: &str, end_of_day: bool, flag: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let date_shape = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    let full_shape = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z';
    if date_shape {
        let suffix = if end_of_day {
            "T23:59:59Z"
        } else {
            "T00:00:00Z"
        };
        Ok(format!("{s}{suffix}"))
    } else if full_shape {
        Ok(s.to_string())
    } else {
        Err(anyhow!(
            "{flag} must be YYYY-MM-DD or YYYY-MM-DDTHH:MM:SSZ, got `{s}`"
        ))
    }
}

fn print_human(
    entries: &[HistoryEntry],
    history_path: &Path,
    scale: TrendScale,
    diff: bool,
    group_by: Option<TrendGroupBy>,
) {
    println!(
        "history: {} ({} entries)",
        history_path.display(),
        entries.len()
    );
    if history::mixed_config_hashes(entries) {
        println!(
            "note: window contains runs with different config hashes — \
             scores may not be directly comparable"
        );
    }
    println!();

    // Summary sparkline and first→last span read from scored runs only — a
    // scoreless vacuous-100 would draw a phantom spike and fake the overall
    // delta. The per-run table below still lists every run (N/A for the
    // scoreless ones) so nothing is hidden.
    let scored: Vec<&HistoryEntry> = entries.iter().filter(|e| !e.is_scoreless()).collect();
    let scores = scored.iter().map(|e| e.mutation_score);
    let (lo, hi, scale_label) = resolve_scale(entries, scale);
    let sparkline = history::sparkline_scaled(scores, lo, hi);
    let first = scored.first().map(|e| e.mutation_score).unwrap_or(0.0);
    let last = scored.last().map(|e| e.mutation_score).unwrap_or(0.0);
    let overall_delta = last - first;
    println!(
        "score: {sparkline}   {:.1}% → {:.1}%  ({}{:.1} pts){}",
        first,
        last,
        if overall_delta >= 0.0 { "+" } else { "" },
        overall_delta,
        scale_label,
    );
    if let Some((dir, count)) = history::trailing_streak(entries) {
        let (arrow, word) = match dir {
            StreakDir::Up => ("↑", "improving"),
            StreakDir::Down => ("↓", "regressing"),
        };
        println!("streak: {arrow} {word} ({count} runs in a row)");
    }
    println!();

    println!(
        "  {:<20} {:>7} {:>5} {:>5} {:>5} {:>10} git",
        "timestamp", "score", "kill", "surv", "to", "delta"
    );
    let mut prev_score: Option<f64> = None;
    for e in entries {
        // Scoreless runs have no real score: show N/A, no delta, and don't let
        // the vacuous 100.0 become the baseline for the next row's delta.
        let scoreless = e.is_scoreless();
        let delta = if scoreless {
            None
        } else {
            prev_score.map(|p| e.mutation_score - p)
        };
        let delta_s = match delta {
            Some(d) if d.abs() < 0.05 => "  0.0".to_string(),
            Some(d) if d >= 0.0 => format!("+{d:.1}"),
            Some(d) => format!("{d:.1}"),
            None => "—".to_string(),
        };
        let git = match (&e.git_branch, &e.git_sha) {
            (Some(b), Some(s)) => format!("{b}@{s}"),
            (None, Some(s)) => s.clone(),
            (Some(b), None) => b.clone(),
            (None, None) => "".into(),
        };
        // Mark the day-one anchor written by `fermut baseline` so it reads as
        // run zero rather than an ordinary run in the table.
        let git = if e.baseline {
            if git.is_empty() {
                "[baseline]".to_string()
            } else {
                format!("{git} [baseline]")
            }
        } else {
            git
        };
        let score_s = if scoreless {
            format!("{:>7}", "N/A")
        } else {
            format!("{:>6.1}%", e.mutation_score)
        };
        println!(
            "  {:<20} {} {:>5} {:>5} {:>5} {:>10} {}",
            truncate(&e.timestamp, 19),
            score_s,
            e.killed,
            e.survived,
            e.timed_out,
            delta_s,
            git,
        );
        // Only real scores seed the next delta — a vacuous 100.0 must not.
        if !scoreless {
            prev_score = Some(e.mutation_score);
        }
    }

    print_survivor_diff(entries, diff);
    if let Some(axis) = group_by {
        print_grouping(entries, axis);
    }
}

/// Render the latest run's survivors aggregated along `axis`. Reads from
/// `entries.last().survivor_ids` and pairs each id with its persistence
/// age (consecutive trailing runs it survived). Silent when the latest
/// entry has no survivor data.
fn print_grouping(entries: &[HistoryEntry], axis: TrendGroupBy) {
    let Some(last) = entries.last() else {
        return;
    };
    let Some(ids) = last.survivor_ids.as_ref() else {
        return;
    };
    if ids.is_empty() {
        return;
    }
    let ages = history::survivor_age_map(entries);
    match axis {
        TrendGroupBy::File => {
            println!();
            println!("survivors by file ({} total):", ids.len());
            for (file, members) in history::survivors_by_file(ids) {
                let max_age = members
                    .iter()
                    .map(|id| ages.get(id).copied().unwrap_or(1))
                    .max()
                    .unwrap_or(1);
                println!(
                    "  {:>4}  {:<60}  oldest survivor: {} run{}",
                    members.len(),
                    file,
                    max_age,
                    if max_age == 1 { "" } else { "s" },
                );
            }
        }
    }
}

/// Print a one-line summary of survivor changes between the second-to-last
/// and last entry in the window, with optional expansion into the full
/// mutant-id lists when `expand` is set. Silent when the diff is empty or
/// either entry lacks survivor data (older entries pre-dating the field).
fn print_survivor_diff(entries: &[HistoryEntry], expand: bool) {
    if entries.len() < 2 {
        return;
    }
    let prev = &entries[entries.len() - 2];
    let curr = &entries[entries.len() - 1];
    let Some((new_surv, new_killed)) = history::survivor_diff(prev, curr) else {
        return;
    };
    if new_surv.is_empty() && new_killed.is_empty() {
        return;
    }
    println!();
    println!(
        "diff vs prev: +{} new survivor{}, -{} newly killed",
        new_surv.len(),
        if new_surv.len() == 1 { "" } else { "s" },
        new_killed.len(),
    );
    if !expand {
        return;
    }
    let ages = history::survivor_age_map(entries);
    if !new_surv.is_empty() {
        println!("new survivors ({}):", new_surv.len());
        for id in &new_surv {
            println!("  {id}");
        }
    }
    // Surface the longest-lived survivors so triage knows which mutants are
    // chronic blockers vs. one-off appearances. Only print when at least
    // one survivor has age ≥ 2 (a survivor with age 1 has no streak to
    // report). Limited to the top 5 by age; tie-broken by id for stability.
    let mut persistent: Vec<(&&str, &usize)> = ages.iter().filter(|(_, age)| **age >= 2).collect();
    if !persistent.is_empty() {
        persistent.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let take = persistent.len().min(5);
        println!("persistent survivors (top {take}):");
        for (id, age) in persistent.iter().take(take) {
            println!("  age {age:>3}  {id}");
        }
    }
    if !new_killed.is_empty() {
        println!("newly killed ({}):", new_killed.len());
        for id in &new_killed {
            println!("  {id}");
        }
    }
}

fn print_json(entries: &[HistoryEntry]) -> Result<()> {
    let s = serde_json::to_string_pretty(entries)?;
    println!("{s}");
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        s[..max].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: &str, score: f64, branch: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            schema_version: crate::history::CURRENT_SCHEMA_V,
            timestamp: ts.into(),
            mutation_score: score,
            // Non-zero so the entry is a real scored run, not a scoreless
            // vacuous-100 that trend/regression now filter out.
            killed: 1,
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
            git_branch: branch.map(str::to_string),
            survivor_ids: None,
            baseline: false,
        }
    }

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("hi", 5), "hi");
        assert_eq!(truncate("hello world", 5), "hello");
    }

    #[test]
    fn normalize_since_accepts_date_only() {
        assert_eq!(
            normalize_date_bound("2026-06-01", false, "--since").unwrap(),
            "2026-06-01T00:00:00Z"
        );
    }

    #[test]
    fn normalize_until_accepts_date_only_as_end_of_day() {
        assert_eq!(
            normalize_date_bound("2026-06-01", true, "--until").unwrap(),
            "2026-06-01T23:59:59Z"
        );
    }

    #[test]
    fn normalize_bound_accepts_full_iso() {
        let s = "2026-06-01T12:34:56Z";
        assert_eq!(normalize_date_bound(s, false, "--since").unwrap(), s);
        assert_eq!(normalize_date_bound(s, true, "--until").unwrap(), s);
    }

    #[test]
    fn normalize_bound_rejects_relative() {
        let err = normalize_date_bound("1 week ago", false, "--since").unwrap_err();
        assert!(err.to_string().contains("--since"));
        let err = normalize_date_bound("yesterday", true, "--until").unwrap_err();
        assert!(err.to_string().contains("--until"));
    }

    fn entry_with_hash(ts: &str, hash: Option<&str>) -> HistoryEntry {
        let mut e = entry(ts, 0.0, None);
        e.config_hash = hash.map(str::to_string);
        e
    }

    #[test]
    fn mixed_hashes_detected() {
        let es = vec![
            entry_with_hash("a", Some("aaa")),
            entry_with_hash("b", Some("bbb")),
        ];
        assert!(crate::history::mixed_config_hashes(&es));
    }

    #[test]
    fn uniform_hashes_not_flagged() {
        let es = vec![
            entry_with_hash("a", Some("aaa")),
            entry_with_hash("b", Some("aaa")),
        ];
        assert!(!crate::history::mixed_config_hashes(&es));
    }

    #[test]
    fn streak_up_detected() {
        let es = vec![
            entry("a", 50.0, None),
            entry("b", 60.0, None),
            entry("c", 70.0, None),
            entry("d", 80.0, None),
        ];
        assert_eq!(history::trailing_streak(&es), Some((StreakDir::Up, 3)));
    }

    #[test]
    fn streak_down_detected() {
        let es = vec![
            entry("a", 80.0, None),
            entry("b", 70.0, None),
            entry("c", 65.0, None),
        ];
        assert_eq!(history::trailing_streak(&es), Some((StreakDir::Down, 2)));
    }

    #[test]
    fn streak_broken_by_direction_flip() {
        let es = vec![
            entry("a", 50.0, None),
            entry("b", 60.0, None), // up
            entry("c", 55.0, None), // down breaks the up streak
        ];
        // Trailing direction is down with only one move → not a streak.
        assert_eq!(history::trailing_streak(&es), None);
    }

    #[test]
    fn streak_needs_minimum_three_entries() {
        let es = vec![entry("a", 50.0, None), entry("b", 60.0, None)];
        assert_eq!(history::trailing_streak(&es), None);
    }

    #[test]
    fn streak_ignores_micro_movements() {
        // Deltas within ±0.05 are treated as flat (ties), so a near-zero
        // jitter run terminates the streak instead of being counted in it.
        let es = vec![
            entry("a", 50.0, None),
            entry("b", 60.0, None),
            entry("c", 60.02, None),
        ];
        assert_eq!(history::trailing_streak(&es), None);
    }

    #[test]
    fn resolve_scale_fixed_returns_full_range() {
        let es = vec![entry("a", 85.0, None), entry("b", 95.0, None)];
        assert_eq!(
            resolve_scale(&es, TrendScale::Fixed),
            (0.0, 100.0, String::new())
        );
    }

    #[test]
    fn resolve_scale_auto_uses_window_minmax() {
        let es = vec![
            entry("a", 85.0, None),
            entry("b", 92.5, None),
            entry("c", 95.0, None),
        ];
        let (lo, hi, label) = resolve_scale(&es, TrendScale::Auto);
        assert!((lo - 85.0).abs() < f64::EPSILON);
        assert!((hi - 95.0).abs() < f64::EPSILON);
        assert!(label.contains("85.0-95.0"));
    }

    #[test]
    fn resolve_scale_auto_falls_back_when_span_under_1pt() {
        // Flat window — auto-scale would amplify noise.
        let es = vec![entry("a", 90.0, None), entry("b", 90.3, None)];
        let (lo, hi, label) = resolve_scale(&es, TrendScale::Auto);
        assert_eq!((lo, hi), (0.0, 100.0));
        assert!(label.is_empty());
    }

    #[test]
    fn resolve_scale_empty_window_is_safe() {
        let (lo, hi, label) = resolve_scale(&[], TrendScale::Auto);
        assert_eq!((lo, hi), (0.0, 100.0));
        assert!(label.is_empty());
    }

    #[test]
    fn group_by_file_aggregation_smoke() {
        // Smoke: ensure aggregation helpers can be called end-to-end with
        // realistic ids without panicking. Output-shape assertions live in
        // history.rs tests; this test guards the wiring path.
        let mut last = entry("t1", 80.0, None);
        last.survivor_ids = Some(vec![
            "src/a.py@1:x->y".into(),
            "src/a.py@2:x->y".into(),
            "src/b.py@1:x->y".into(),
        ]);
        let entries = vec![last];
        print_grouping(&entries, TrendGroupBy::File);
    }

    #[test]
    fn missing_hashes_dont_trigger_warning() {
        let es = vec![
            entry_with_hash("a", Some("aaa")),
            entry_with_hash("b", None),
            entry_with_hash("c", Some("aaa")),
        ];
        assert!(!crate::history::mixed_config_hashes(&es));
    }

    fn strict_opts(history_path: PathBuf) -> TrendOpts {
        TrendOpts {
            path: PathBuf::from("."),
            history_path: Some(history_path),
            limit: 20,
            all: true,
            format: TrendFormat::Json,
            scale: TrendScale::Auto,
            branch: None,
            since: None,
            until: None,
            fail_on_regression: None,
            diff: false,
            group_by: None,
            strict: true,
        }
    }

    #[test]
    fn strict_fails_on_a_malformed_line() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let good = serde_json::to_string(&entry("2026-06-04T12:00:00Z", 50.0, None)).unwrap();
        std::fs::write(&p, format!("{good}\nnot json\n")).unwrap();

        let err = trend(strict_opts(p)).unwrap_err().to_string();
        assert!(err.contains("malformed"), "got: {err}");
    }

    #[test]
    fn strict_does_not_fail_on_a_newer_schema_line() {
        // A newer-schema line is readable by a newer binary and unrepairable by
        // this one — --strict must warn, not fail, or version skew wedges CI.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let good = serde_json::to_string(&entry("2026-06-04T12:00:00Z", 50.0, None)).unwrap();
        let newer = format!(
            r#"{{"v":{},"timestamp":"2026-06-05T12:00:00Z","mutation_score":80.0,"killed":4,"survived":1,"timed_out":0,"skipped":0,"errored":0}}"#,
            crate::history::CURRENT_SCHEMA_V + 1
        );
        std::fs::write(&p, format!("{good}\n{newer}\n")).unwrap();

        assert!(trend(strict_opts(p)).is_ok());
    }
}
