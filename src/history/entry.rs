//! The `HistoryEntry` record and its derived predicates.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::report::{MutantOutcome, Report};

use super::io::{current_git_branch, git_short_sha, iso8601_now};

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

/// One step of a trend/dashboard delta column: given the running baseline
/// `prev` (the last comparable score), return `(delta_to_show, next_prev)` for
/// entry `e`. A non-comparable run — a scoreless vacuous-100 or a `--max-time`
/// partial subset score — shows no delta and does **not** advance the baseline,
/// so the next comparable row still deltas against the last real full run.
/// Shared by `trend` (stdout table) and `dashboard` (HTML table) so both agree.
pub(crate) fn trend_step(prev: Option<f64>, e: &HistoryEntry) -> (Option<f64>, Option<f64>) {
    if e.is_comparable() {
        (prev.map(|p| e.mutation_score - p), Some(e.mutation_score))
    } else {
        (None, prev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mutant(id: &str) -> crate::mutator::Mutant {
        use crate::mutator::{Mutant, Operator};
        use ruff_text_size::TextRange;
        Mutant {
            id: id.into(),
            file: PathBuf::from("a.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    /// A minimal scored entry — one killed mutant, so it's a real scored run,
    /// not a scoreless vacuous-100 that trend/regression filter out.
    fn mk(score: f64, branch: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: score,
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
            partial: false,
        }
    }

    /// A scored `mk` entry forced back to zero buckets — its `mutation_score`
    /// is the vacuous 100.0 floor, so `is_scoreless()` is true.
    fn mk_scoreless(branch: Option<&str>) -> HistoryEntry {
        let mut e = mk(100.0, branch);
        e.killed = 0;
        assert!(e.is_scoreless());
        e
    }

    /// A real scored entry flagged `partial` — as a `--max-time` run truncated
    /// by the budget would be. Scored (not scoreless), but not comparable.
    fn mk_partial(score: f64, branch: Option<&str>) -> HistoryEntry {
        let mut e = mk(score, branch);
        e.partial = true;
        assert!(
            !e.is_scoreless(),
            "partial entry still scored a real subset"
        );
        assert!(
            !e.is_comparable(),
            "partial entry must be excluded from the gate"
        );
        e
    }

    #[test]
    fn from_report_stamps_current_fermut_version() {
        let report = Report::new(vec![MutantOutcome::killed(mutant("x"))]);
        let e = HistoryEntry::from_report(&report, Path::new("."), None, None);
        assert_eq!(e.fermut_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn from_report_marks_time_budget_run_partial() {
        // A run with any time-budget skip scored a truncated subset → partial,
        // so the regression gate/trend excludes it.
        let report = Report::new(vec![
            MutantOutcome::killed(mutant("k")),
            MutantOutcome::skipped(mutant("s"), crate::engine::TIME_BUDGET_FILTER),
        ]);
        let e = HistoryEntry::from_report(&report, Path::new("."), None, None);
        assert!(e.partial, "time-budget skip must flag the entry partial");
        assert!(!e.is_comparable());
    }

    #[test]
    fn from_report_full_run_is_not_partial() {
        // No time-budget skip (a plain coverage skip is not a budget cutoff) →
        // a full, comparable run.
        let report = Report::new(vec![
            MutantOutcome::killed(mutant("k")),
            MutantOutcome::skipped(mutant("s"), "coverage"),
        ]);
        let e = HistoryEntry::from_report(&report, Path::new("."), None, None);
        assert!(!e.partial, "a non-budget skip must not flag partial");
        assert!(e.is_comparable());
    }

    #[test]
    fn trend_step_deltas_and_seeds_only_comparable_runs() {
        // full(80) → partial(40) → full(78): the partial neither shows a delta
        // nor advances the baseline, so the third row deltas 78−80 = −2.
        let a = mk(80.0, None);
        let b = mk_partial(40.0, None);
        let c = mk(78.0, None);

        let (d_a, prev) = trend_step(None, &a);
        assert_eq!(d_a, None, "first comparable row has no prior");
        assert_eq!(prev, Some(80.0));

        let (d_b, prev) = trend_step(prev, &b);
        assert_eq!(d_b, None, "partial row shows no delta");
        assert_eq!(
            prev,
            Some(80.0),
            "partial row does NOT advance the baseline"
        );

        let (d_c, prev) = trend_step(prev, &c);
        assert_eq!(
            d_c,
            Some(-2.0),
            "deltas against the last full run, not the subset"
        );
        assert_eq!(prev, Some(78.0));
    }

    #[test]
    fn trend_step_skips_scoreless_like_partial() {
        // A scoreless (vacuous-100) row behaves the same: no delta, no seed.
        let a = mk(90.0, None);
        let s = mk_scoreless(None);
        let (_, prev) = trend_step(None, &a);
        let (d_s, prev_after) = trend_step(prev, &s);
        assert_eq!(d_s, None);
        assert_eq!(
            prev_after,
            Some(90.0),
            "scoreless must not reseed the baseline"
        );
    }
}
