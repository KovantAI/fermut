//! Markdown report — score line, count table, collapsible survivor list.
//! Drop straight into a PR body.

use std::path::Path;

use anyhow::{Context, Result};

use crate::history::{self, HistoryEntry};
use crate::report::{MutantOutcome, Report};

/// HTML comment marker prepended to every Markdown report. Invisible in
/// rendered GFM, but `fermut pr-comment` (and any bespoke CI script) can
/// scan PR comments for this string to find the previous fermut comment
/// and edit it in place instead of leaving a fresh one per run.
pub const STICKY_MARKER: &str = "<!-- fermut:report -->";

impl Report {
    pub fn write_markdown(&self, path: &Path) -> Result<()> {
        self.write_markdown_with_history(path, &[])
    }

    /// Same as `write_markdown`, but when `prior_history` is non-empty,
    /// emits a compact trend block right under the score line — sparkline
    /// across the prior entries plus this run, and the delta vs the most
    /// recent prior entry. Pair with `fermut run --trend --markdown …` so
    /// reviewers see the trajectory inline in the PR comment.
    pub fn write_markdown_with_history(
        &self,
        path: &Path,
        prior_history: &[HistoryEntry],
    ) -> Result<()> {
        let c = self.counts();
        let mut md = String::new();
        md.push_str(STICKY_MARKER);
        md.push('\n');
        md.push_str(&format!(
            "## fermut report\n\n**Score:** {} ({} / {} detected)\n\n",
            c.score_label(),
            c.killed + c.timed_out,
            c.scored()
        ));
        if !prior_history.is_empty() {
            md.push_str(&render_trend_block(c.mutation_score_opt(), prior_history));
        }
        md.push_str("| Status     | Count |\n|------------|------:|\n");
        md.push_str(&format!("| killed     | {} |\n", c.killed));
        md.push_str(&format!("| survived   | {} |\n", c.survived));
        md.push_str(&format!("| timeout    | {} |\n", c.timed_out));
        md.push_str(&format!("| skipped    | {} |\n", c.skipped));
        md.push_str(&format!("| equivalent | {} |\n", c.equivalent));
        md.push_str(&format!("| errored    | {} |\n", c.errored));

        let survivors: Vec<&MutantOutcome> = self
            .outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    MutantOutcome::Survived { .. } | MutantOutcome::TimedOut { .. }
                )
            })
            .collect();
        if !survivors.is_empty() {
            md.push_str(&format!(
                "\n<details>\n<summary>{} survivor(s)</summary>\n\n",
                survivors.len()
            ));
            for o in &survivors {
                let m = o.mutant();
                md.push_str(&format!(
                    "- `{}:{}` `{}` — `{}` → `{}`\n",
                    m.file.display(),
                    m.line,
                    m.operator.name(),
                    m.original.replace('`', "'"),
                    m.replacement.replace('`', "'")
                ));
            }
            md.push_str("\n</details>\n");
        }
        std::fs::write(path, md).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// Build the trend section that goes between the score line and the count
/// table. We cap the sparkline window at 20 points so a long history
/// doesn't blow out the comment width.
/// `current_score` is `None` when the run scored nothing (its `mutation_score`
/// is the vacuous 100.0 floor, not a measurement). Scoreless prior entries and
/// a scoreless current run are excluded from the sparkline and delta so the
/// trend reflects real scores only — a phantom 100 would fake a spike and an
/// improvement (mirrors the `trend`/`dashboard` filtering).
fn render_trend_block(current_score: Option<f64>, prior_history: &[HistoryEntry]) -> String {
    const WINDOW: usize = 20;
    let start = prior_history.len().saturating_sub(WINDOW - 1);
    let window = &prior_history[start..];
    let mut scores: Vec<f64> = window
        .iter()
        .filter(|e| !e.is_scoreless())
        .map(|e| e.mutation_score)
        .collect();
    if let Some(cur) = current_score {
        scores.push(cur);
    }
    // Nothing real to plot (all-scoreless history and a scoreless run).
    if scores.is_empty() {
        return String::new();
    }
    let spark = history::sparkline(scores.iter().copied());
    let first = scores.first().copied().unwrap();

    // Delta only when the current run scored and there is a prior scored run.
    let prev = window
        .iter()
        .rev()
        .find(|e| !e.is_scoreless())
        .map(|e| e.mutation_score);
    let delta = current_score.zip(prev).map(|(c, p)| c - p);
    let delta_s = match delta {
        Some(d) if d.abs() < 0.05 => "no change".to_string(),
        Some(d) if d >= 0.0 => format!("▲ +{d:.1} pts vs previous"),
        Some(d) => format!("▼ {d:.1} pts vs previous"),
        None if current_score.is_none() => "not scored this run".to_string(),
        None => "first recorded run".to_string(),
    };

    let last_s = match current_score {
        Some(c) => format!("{c:.1}%"),
        None => "N/A".to_string(),
    };
    format!("**Trend:** `{spark}`  {first:.1}% → {last_s}  ({delta_s})\n\n")
}

#[cfg(test)]
mod tests {
    use crate::report::testing::make_mutant;
    use crate::report::{MutantOutcome, Report};

    #[test]
    fn write_markdown_includes_score_and_survivors() {
        let r = Report::new(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        r.write_markdown(tmp.path()).unwrap();
        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(written.contains("fermut report"));
        assert!(written.contains("50.0%"));
        assert!(written.contains("survivor"));
    }

    #[test]
    fn write_markdown_scoreless_run_reports_na_not_hundred() {
        use crate::history::HistoryEntry;
        // Every mutant errored → nothing scored. The report must headline N/A,
        // and the trend block must neither show 100% nor a phantom improvement
        // against a real prior score.
        let r = Report::new(vec![
            MutantOutcome::error(make_mutant(), "boom".into()),
            MutantOutcome::error(make_mutant(), "boom".into()),
        ]);
        let prior = vec![HistoryEntry {
            schema_version: crate::history::CURRENT_SCHEMA_V,
            timestamp: "2026-05-01T00:00:00Z".into(),
            mutation_score: 80.0,
            killed: 4,
            survived: 1,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: Some(5),
            duration_ms: None,
            config_hash: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
            fermut_version: None,
        }];
        let tmp = tempfile::NamedTempFile::new().unwrap();
        r.write_markdown_with_history(tmp.path(), &prior).unwrap();
        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(written.contains("**Score:** N/A"));
        assert!(!written.contains("100.0%"));
        // No fabricated delta from the vacuous floor.
        assert!(!written.contains("▲ +"));
        assert!(written.contains("not scored this run"));
    }

    #[test]
    fn write_markdown_with_history_emits_trend_block() {
        use crate::history::HistoryEntry;
        let r = Report::new(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        let prior = vec![HistoryEntry {
            schema_version: crate::history::CURRENT_SCHEMA_V,
            timestamp: "2026-05-01T00:00:00Z".into(),
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
        }];
        let tmp = tempfile::NamedTempFile::new().unwrap();
        r.write_markdown_with_history(tmp.path(), &prior).unwrap();
        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(written.contains("**Trend:**"));
        assert!(written.contains("▲ +"));
        // Order matters: trend block before the count table.
        let trend_at = written.find("**Trend:**").unwrap();
        let table_at = written.find("| Status").unwrap();
        assert!(trend_at < table_at);
    }

    #[test]
    fn write_markdown_with_empty_history_skips_trend_block() {
        let r = Report::new(vec![MutantOutcome::killed(make_mutant())]);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        r.write_markdown_with_history(tmp.path(), &[]).unwrap();
        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(!written.contains("**Trend:**"));
    }

    #[test]
    fn write_markdown_prepends_sticky_marker() {
        let r = Report::new(vec![MutantOutcome::killed(make_mutant())]);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        r.write_markdown(tmp.path()).unwrap();
        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(written.starts_with(super::STICKY_MARKER));
    }
}
