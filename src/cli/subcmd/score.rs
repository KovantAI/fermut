//! `fermut score` — emit the agent reward signal for the latest run.
//!
//! The agent inner loop (write code → run mutants → write a killing test →
//! re-run) needs one thing after every iteration: *did this iteration
//! help?* The coding-agents guide documents the answer as a chunk of jq +
//! pseudocode over `history.jsonl` — subtract the last two scores, diff the
//! survivor-id sets, decide whether to revert. Every agent scaffold
//! re-implements it. `score` bakes that derivation into the binary.
//!
//! It reads `.fermut/history.jsonl` (written by `fermut run`), picks the
//! most recent entry as *current* and an earlier branch-comparable entry as
//! *baseline*, and reports the score delta, the new-survivor / newly-killed
//! id sets, and a `regressed` flag. JSON is the default format — this
//! command exists for machine consumers.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Args;
use serde::Serialize;

use crate::cli::convert::Format;
use crate::history::{self, HistoryEntry};

#[derive(Args, Debug)]
pub struct ScoreArgs {
    /// Where to start the project-root walk. Defaults to cwd.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Custom path to the history log. Defaults to `<path>/.fermut/history.jsonl`.
    #[arg(long)]
    pub history_path: Option<PathBuf>,

    /// Compare against the entry this many branch-comparable runs back.
    /// `1` (default) is the immediately prior run.
    #[arg(long, default_value_t = 1, value_name = "N")]
    pub baseline: usize,

    /// Restrict current/baseline selection to this git branch. Pin to
    /// `main` in CI where the cache restores main-branch history into a
    /// PR build.
    #[arg(long)]
    pub branch: Option<String>,

    /// Exit non-zero if the score dropped more than this many points vs
    /// the baseline. Agent rollback / CI gate.
    #[arg(long, value_name = "PTS")]
    pub fail_on_regression: Option<f64>,

    /// Output format. `json` (default) emits the reward signal for
    /// machine consumers; `human` prints a short summary.
    #[arg(long, value_enum, default_value_t = Format::Json)]
    pub format: Format,
}

pub fn run(args: ScoreArgs) -> Result<()> {
    score(ScoreOpts {
        path: args.path,
        history_path: args.history_path,
        baseline: args.baseline,
        branch: args.branch,
        fail_on_regression: args.fail_on_regression,
        format: args.format.into(),
    })
}

/// Float-noise floor for score comparisons. A delta inside `±SCORE_NOISE`
/// is treated as flat — not a regression. Shared by the `regressed` flag
/// and the `--fail-on-regression` gate so the two never disagree.
const SCORE_NOISE: f64 = 0.05;

#[derive(Debug, Clone)]
pub struct ScoreOpts {
    pub path: PathBuf,
    pub history_path: Option<PathBuf>,
    /// Compare the latest run against the entry this many branch-comparable
    /// runs back. `1` (default) is the immediately prior run — the reward
    /// signal for a single inner-loop iteration.
    pub baseline: usize,
    /// Restrict both current and baseline selection to entries recorded on
    /// this git branch. Without it, the baseline is the most recent prior
    /// run on the same branch as the latest entry (detached-HEAD / untagged
    /// entries are treated as comparable, matching the regression gate).
    pub branch: Option<String>,
    /// If set and the score dropped more than this many points vs the
    /// baseline, exit non-zero after printing. CI / agent rollback gate.
    pub fail_on_regression: Option<f64>,
    pub format: ScoreFormat,
}

#[derive(Debug, Clone, Copy)]
pub enum ScoreFormat {
    Human,
    Json,
}

/// The reward signal. Optional baseline fields are `null` when there is no
/// comparable prior run (first recorded run on the branch).
#[derive(Debug, Serialize)]
pub(crate) struct ScoreReport {
    /// Latest run's mutation score, percent.
    score: f64,
    killed: usize,
    survived: usize,
    timed_out: usize,
    timestamp: String,
    /// Baseline run's score, or `null` when there's no comparable prior.
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_timestamp: Option<String>,
    /// `score - baseline_score`. `null` without a baseline.
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<f64>,
    /// Mutants surviving now that the baseline killed (regressions). Empty
    /// when either entry lacks recorded survivor ids.
    new_survivors: Vec<String>,
    /// Mutants the baseline reported as survivors that are now killed
    /// (the agent's progress).
    newly_killed: Vec<String>,
    /// True when the score dropped or a new survivor appeared vs the
    /// baseline. The agent's "consider reverting this iteration" flag.
    /// Always `false` without a baseline.
    regressed: bool,
    /// True when both entries carried survivor ids, so `new_survivors` /
    /// `newly_killed` are a real diff and `regressed` accounts for survivor
    /// moves. False when either entry predates the survivor-id field — then
    /// the id sets are empty and `regressed` is score-delta-only. Absent
    /// (defaults true) without a baseline.
    #[serde(skip_serializing_if = "is_true")]
    survivor_ids_compared: bool,
}

fn is_true(b: &bool) -> bool {
    *b
}

/// Compute the reward signal from a history log, without printing. Returns
/// `Ok(None)` when the history has no comparable entries (the caller decides
/// whether that's an error or an empty result). Shared by the `score`
/// subcommand and the MCP server.
pub(crate) fn compute_score(
    history_path: &std::path::Path,
    baseline: usize,
    branch: Option<&str>,
) -> Result<Option<ScoreReport>> {
    if baseline == 0 {
        return Err(anyhow!("baseline must be at least 1"));
    }
    let entries = history::load(history_path)?;
    // Pre-filter to the requested branch, or — when unset — to the set of
    // entries comparable with the latest run (same branch, treating missing
    // branch on either side as comparable, same rule the regression gate
    // uses). `comparable.last()` is always the current run.
    let comparable = comparable_window(&entries, branch);
    let Some((current, base)) = pick(&comparable, baseline) else {
        return Ok(None);
    };
    Ok(Some(build_report(current, base)))
}

pub fn score(opts: ScoreOpts) -> Result<()> {
    let history_path = opts
        .history_path
        .clone()
        .unwrap_or_else(|| history::default_history_path(&history::resolve_root(&opts.path)));

    let Some(report) = compute_score(&history_path, opts.baseline, opts.branch.as_deref())? else {
        // Empty window: either the log is empty, or a `--branch` filter
        // excluded every entry. Distinguish so the user isn't told to
        // populate a log that already has runs.
        let entries = history::load(&history_path)?;
        return match (entries.is_empty(), opts.branch.as_deref()) {
            (false, Some(b)) => Err(anyhow!(
                "no runs on branch '{b}' in {} ({} entr{} on other branches) — \
                 drop --branch or run `fermut run` on '{b}'",
                history_path.display(),
                entries.len(),
                if entries.len() == 1 { "y" } else { "ies" },
            )),
            _ => Err(anyhow!(
                "no history at {} — run `fermut run …` once to populate it",
                history_path.display()
            )),
        };
    };

    match opts.format {
        ScoreFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        ScoreFormat::Human => print_human(&report),
    }

    // Gate fires only when the report already counts as a regression, so a
    // non-zero exit always implies `regressed: true` — the flag and the gate
    // can't disagree. On top of that the drop must exceed the caller's
    // points threshold (a magnitude filter).
    if let (Some(threshold), Some(delta)) = (opts.fail_on_regression, report.delta) {
        let drop = -delta;
        if report.regressed && drop > threshold {
            eprintln!("mutation score regressed by {drop:.1} pts (threshold {threshold:.1})");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Entries comparable with the latest run, oldest-first. With an explicit
/// branch, keep only that branch. Without one, anchor on the latest entry's
/// branch and keep entries that match it (missing branch on either side
/// counts as a match — best effort for detached HEAD / pre-git entries).
fn comparable_window<'a>(
    entries: &'a [HistoryEntry],
    branch: Option<&str>,
) -> Vec<&'a HistoryEntry> {
    if let Some(b) = branch {
        return entries
            .iter()
            .filter(|e| e.git_branch.as_deref() == Some(b))
            .collect();
    }
    let Some(latest) = entries.last() else {
        return Vec::new();
    };
    let anchor = latest.git_branch.as_deref();
    entries
        .iter()
        .filter(|e| match (e.git_branch.as_deref(), anchor) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        })
        .collect()
}

/// From an oldest-first comparable window, return `(current, baseline)`:
/// current is the last entry, baseline is `n` positions earlier (or `None`
/// when the window is too short). `None` overall when the window is empty.
fn pick<'a>(
    window: &[&'a HistoryEntry],
    n: usize,
) -> Option<(&'a HistoryEntry, Option<&'a HistoryEntry>)> {
    let current = window.last()?;
    let baseline = window.len().checked_sub(1 + n).map(|i| window[i]);
    Some((current, baseline))
}

fn build_report(current: &HistoryEntry, baseline: Option<&HistoryEntry>) -> ScoreReport {
    let (
        baseline_score,
        baseline_timestamp,
        delta,
        new_survivors,
        newly_killed,
        regressed,
        survivor_ids_compared,
    ) = match baseline {
        Some(base) => {
            // A scoreless run carries the vacuous 100.0 floor and a `--max-time`
            // partial run scored a nondeterministic subset — a delta against
            // either (on either side) is fabricated. Leave the delta undefined
            // so regression is driven by survivor ids alone, never by a phantom
            // score move. `is_comparable` rejects both.
            let delta = if !current.is_comparable() || !base.is_comparable() {
                None
            } else {
                Some(current.mutation_score - base.mutation_score)
            };
            let (new_surv, new_kill, compared) = match history::survivor_diff(base, current) {
                Some((s, k)) => (
                    s.into_iter().map(str::to_string).collect(),
                    k.into_iter().map(str::to_string).collect(),
                    true,
                ),
                None => (Vec::new(), Vec::new(), false),
            };
            // Regressed when the score dropped beyond float noise or a
            // mutant the baseline killed now survives. Mirrors the rollback
            // rule in the coding-agents guide. When survivor ids weren't
            // compared (`compared == false`) the survivor term is always
            // empty, so this is score-delta-only — `survivor_ids_compared`
            // tells the consumer. A missing (scoreless) delta never signals
            // a regression on its own.
            let regressed = delta.is_some_and(|d| d < -SCORE_NOISE) || !new_surv.is_empty();
            (
                Some(base.mutation_score),
                Some(base.timestamp.clone()),
                delta,
                new_surv,
                new_kill,
                regressed,
                compared,
            )
        }
        None => (None, None, None, Vec::new(), Vec::new(), false, true),
    };
    ScoreReport {
        score: current.mutation_score,
        killed: current.killed,
        survived: current.survived,
        timed_out: current.timed_out,
        timestamp: current.timestamp.clone(),
        baseline_score,
        baseline_timestamp,
        delta,
        new_survivors,
        newly_killed,
        regressed,
        survivor_ids_compared,
    }
}

fn print_human(r: &ScoreReport) {
    println!(
        "score: {:.1}%  ({} killed, {} survived, {} timed out)",
        r.score, r.killed, r.survived, r.timed_out
    );
    match r.delta {
        Some(d) => {
            let sign = if d >= 0.0 { "+" } else { "" };
            let base = r.baseline_score.unwrap_or(0.0);
            println!("delta: {sign}{d:.1} pts vs baseline {base:.1}%");
            println!(
                "  +{} new survivor{}, -{} newly killed{}",
                r.new_survivors.len(),
                if r.new_survivors.len() == 1 { "" } else { "s" },
                r.newly_killed.len(),
                if r.regressed { "   [REGRESSED]" } else { "" },
            );
            if !r.new_survivors.is_empty() {
                println!("new survivors:");
                for id in &r.new_survivors {
                    println!("  {id}");
                }
            }
            if !r.survivor_ids_compared {
                println!("  note: survivor ids unavailable — regressed is score-delta-only");
            }
        }
        None => println!("delta: — (no comparable prior run)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: &str, score: f64, branch: Option<&str>, survivors: &[&str]) -> HistoryEntry {
        HistoryEntry {
            schema_version: crate::history::CURRENT_SCHEMA_V,
            timestamp: ts.into(),
            mutation_score: score,
            // Non-zero so the entry is a real scored run, not a scoreless
            // vacuous-100 that score-delta/regression now ignore.
            killed: 1,
            survived: survivors.len(),
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
            survivor_ids: Some(survivors.iter().map(|s| s.to_string()).collect()),
            baseline: false,
            partial: false,
        }
    }

    #[test]
    fn delta_and_diff_against_immediate_prior() {
        let es = vec![
            entry("t1", 80.0, Some("main"), &["a", "b"]),
            entry("t2", 85.0, Some("main"), &["b"]),
        ];
        let win = comparable_window(&es, None);
        let (cur, base) = pick(&win, 1).unwrap();
        let r = build_report(cur, base);
        assert!((r.delta.unwrap() - 5.0).abs() < f64::EPSILON);
        assert_eq!(r.baseline_score, Some(80.0));
        assert_eq!(r.newly_killed, vec!["a"]);
        assert!(r.new_survivors.is_empty());
        assert!(!r.regressed);
    }

    #[test]
    fn regressed_when_new_survivor_appears() {
        let es = vec![
            entry("t1", 90.0, Some("main"), &["a"]),
            entry("t2", 90.0, Some("main"), &["a", "c"]),
        ];
        let win = comparable_window(&es, None);
        let (cur, base) = pick(&win, 1).unwrap();
        let r = build_report(cur, base);
        // Score flat but a new survivor 'c' appeared → regressed.
        assert_eq!(r.delta, Some(0.0));
        assert_eq!(r.new_survivors, vec!["c"]);
        assert!(r.regressed);
    }

    #[test]
    fn regressed_when_score_drops() {
        let es = vec![
            entry("t1", 90.0, Some("main"), &["a"]),
            entry("t2", 85.0, Some("main"), &["a"]),
        ];
        let win = comparable_window(&es, None);
        let (cur, base) = pick(&win, 1).unwrap();
        let r = build_report(cur, base);
        assert!(r.regressed);
        assert!(r.new_survivors.is_empty());
    }

    #[test]
    fn baseline_n_back_skips_intermediate_runs() {
        let es = vec![
            entry("t1", 70.0, Some("main"), &[]),
            entry("t2", 75.0, Some("main"), &[]),
            entry("t3", 85.0, Some("main"), &[]),
        ];
        let win = comparable_window(&es, None);
        let (cur, base) = pick(&win, 2).unwrap();
        let r = build_report(cur, base);
        // current=t3 (85), baseline two back = t1 (70).
        assert_eq!(r.baseline_score, Some(70.0));
        assert!((r.delta.unwrap() - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn scoreless_side_yields_no_delta_and_no_regression() {
        // A scoreless run carries the vacuous 100.0 floor. Whether it's the
        // baseline or the current run, the delta is undefined and must not
        // trip regression on a phantom score move.
        let mut scoreless = entry("t1", 100.0, Some("main"), &[]);
        scoreless.killed = 0; // now killed+survived+timed_out == 0
        assert!(scoreless.is_scoreless());

        let real = entry("t2", 60.0, Some("main"), &[]);

        // scoreless baseline vs real current (100→60 would fake a -40 drop).
        let r = build_report(&real, Some(&scoreless));
        assert!(r.delta.is_none());
        assert!(!r.regressed);

        // real baseline vs scoreless current (60→100 would fake a +40 gain).
        let r = build_report(&scoreless, Some(&real));
        assert!(r.delta.is_none());
        assert!(!r.regressed);
    }

    #[test]
    fn partial_side_yields_no_delta() {
        // A `--max-time` partial run scored a nondeterministic subset. Whether
        // it's the baseline or the current run, the score delta is fabricated
        // and must be suppressed (survivor ids still drive real regressions).
        let mut partial = entry("t1", 70.0, Some("main"), &["a"]);
        partial.partial = true;
        assert!(!partial.is_scoreless() && !partial.is_comparable());
        let real = entry("t2", 90.0, Some("main"), &["a"]);

        // partial baseline vs real current (70→90 would fake a +20 gain).
        let r = build_report(&real, Some(&partial));
        assert!(r.delta.is_none());

        // real baseline vs partial current (90→70 would fake a -20 drop).
        let r = build_report(&partial, Some(&real));
        assert!(r.delta.is_none());
    }

    #[test]
    fn no_baseline_when_only_one_run() {
        let es = vec![entry("t1", 80.0, Some("main"), &["a"])];
        let win = comparable_window(&es, None);
        let (cur, base) = pick(&win, 1).unwrap();
        assert!(base.is_none());
        let r = build_report(cur, base);
        assert!(r.delta.is_none());
        assert!(!r.regressed);
    }

    #[test]
    fn branch_filter_scopes_both_ends() {
        let es = vec![
            entry("t1", 90.0, Some("main"), &[]),
            entry("t2", 50.0, Some("feature"), &[]), // other branch, ignored
            entry("t3", 88.0, Some("main"), &[]),
        ];
        let win = comparable_window(&es, Some("main"));
        assert_eq!(win.len(), 2);
        let (cur, base) = pick(&win, 1).unwrap();
        let r = build_report(cur, base);
        assert_eq!(r.score, 88.0);
        assert_eq!(r.baseline_score, Some(90.0));
    }

    #[test]
    fn unset_branch_anchors_on_latest_branch() {
        // Latest run is on `main`; the feature-branch run between the two
        // main runs must be excluded from the comparable window.
        let es = vec![
            entry("t1", 90.0, Some("main"), &[]),
            entry("t2", 10.0, Some("feature"), &[]),
            entry("t3", 88.0, Some("main"), &[]),
        ];
        let win = comparable_window(&es, None);
        let (cur, base) = pick(&win, 1).unwrap();
        let r = build_report(cur, base);
        assert_eq!(r.baseline_score, Some(90.0));
    }

    #[test]
    fn empty_history_has_no_current() {
        let win = comparable_window(&[], None);
        assert!(pick(&win, 1).is_none());
    }

    #[test]
    fn partial_when_baseline_lacks_survivor_ids() {
        let mut base = entry("t1", 90.0, Some("main"), &[]);
        base.survivor_ids = None;
        let cur = entry("t2", 85.0, Some("main"), &["a"]);
        let r = build_report(&cur, Some(&base));
        // No id diff possible → empty sets, flagged via survivor_ids_compared.
        assert!(!r.survivor_ids_compared);
        assert!(r.new_survivors.is_empty());
        assert!(r.newly_killed.is_empty());
        // 5-pt drop still trips regressed on score alone.
        assert!(r.regressed);
    }

    #[test]
    fn full_diff_sets_survivor_ids_compared() {
        let es = vec![
            entry("t1", 80.0, Some("main"), &["a"]),
            entry("t2", 85.0, Some("main"), &["a"]),
        ];
        let win = comparable_window(&es, None);
        let (cur, base) = pick(&win, 1).unwrap();
        let r = build_report(cur, base);
        assert!(r.survivor_ids_compared);
    }

    #[test]
    fn flat_within_noise_not_regressed() {
        let es = vec![
            entry("t1", 90.0, Some("main"), &["a"]),
            entry("t2", 89.98, Some("main"), &["a"]),
        ];
        let win = comparable_window(&es, None);
        let (cur, base) = pick(&win, 1).unwrap();
        let r = build_report(cur, base);
        // -0.02 is inside ±SCORE_NOISE → flat, not a regression.
        assert!(!r.regressed);
    }

    fn write_history(entries: &[HistoryEntry]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let body: String = entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    fn opts(history_path: PathBuf) -> ScoreOpts {
        ScoreOpts {
            path: ".".into(),
            history_path: Some(history_path),
            baseline: 1,
            branch: None,
            fail_on_regression: None,
            format: ScoreFormat::Json,
        }
    }

    #[test]
    fn score_errors_on_missing_history() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.jsonl");
        assert!(score(opts(missing)).is_err());
    }

    #[test]
    fn score_errors_on_zero_baseline() {
        let (_dir, path) = write_history(&[entry("t1", 80.0, Some("main"), &["a"])]);
        let mut o = opts(path);
        o.baseline = 0;
        assert!(score(o).is_err());
    }

    #[test]
    fn score_ok_with_single_run() {
        let (_dir, path) = write_history(&[entry("t1", 80.0, Some("main"), &["a"])]);
        assert!(score(opts(path)).is_ok());
    }

    #[test]
    fn score_errors_clearly_when_branch_matches_nothing() {
        let (_dir, path) = write_history(&[entry("t1", 80.0, Some("main"), &["a"])]);
        let mut o = opts(path);
        o.branch = Some("nope".into());
        let err = score(o).unwrap_err().to_string();
        // Names the branch, not the bogus "no history / run fermut run" path.
        assert!(err.contains("nope"), "got: {err}");
        assert!(!err.contains("run `fermut run …` once"), "got: {err}");
    }

    #[test]
    fn score_ok_with_baseline() {
        let (_dir, path) = write_history(&[
            entry("t1", 80.0, Some("main"), &["a", "b"]),
            entry("t2", 85.0, Some("main"), &["b"]),
        ]);
        assert!(score(opts(path)).is_ok());
    }
}
