//! `fermut merge` — combine multiple JSON shard reports into one, and the
//! shared `i/n` shard-spec parser used by both the CLI flag and the config
//! file value.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::filter::shard::SHARD_FILTER_NAME;
use crate::report::{MutantOutcome, Report, ReportFormat, ReportSinks};

/// A `Skipped { filter: "shard" }` outcome is not a verdict — it is the
/// placeholder a sharded run emits for every mutant *outside* its own slice.
/// Each shard's report describes the whole universe (its slice with real
/// verdicts, the rest as this placeholder), so merging must let any real
/// verdict win over it rather than last-write-wins. The filter name is the
/// shared `SHARD_FILTER_NAME` const so this cannot drift from `ShardFilter`.
fn is_shard_placeholder(o: &MutantOutcome) -> bool {
    matches!(o, MutantOutcome::Skipped { filter, .. } if filter == SHARD_FILTER_NAME)
}

/// Do two real (non-placeholder) outcomes for the same mutant id carry the same
/// verdict? Compares only the outcome payload, never the `Mutant` itself: a
/// report written by an older fermut may carry a defaulted `stmt_line`, and that
/// skew must not read as a conflict when the verdict is genuinely identical.
/// Still distinguishes same-status skips/errors/equivalents that differ in their
/// reason — the disagreement finding this guards against.
fn same_verdict(a: &MutantOutcome, b: &MutantOutcome) -> bool {
    use MutantOutcome::*;
    match (a, b) {
        (Killed { .. }, Killed { .. })
        | (Survived { .. }, Survived { .. })
        | (TimedOut { .. }, TimedOut { .. }) => true,
        (Skipped { filter: x, .. }, Skipped { filter: y, .. }) => x == y,
        (Error { message: x, .. }, Error { message: y, .. }) => x == y,
        (
            Equivalent {
                reason: r1,
                source: s1,
                ..
            },
            Equivalent {
                reason: r2,
                source: s2,
                ..
            },
        ) => r1 == r2 && s1 == s2,
        _ => false,
    }
}

pub(super) fn parse_shard_spec(s: &str) -> Result<(u32, u32), String> {
    let (i, n) = s
        .split_once('/')
        .ok_or_else(|| format!("expected `i/n`, got {s:?}"))?;
    let i: u32 = i
        .parse()
        .map_err(|e: std::num::ParseIntError| e.to_string())?;
    let n: u32 = n
        .parse()
        .map_err(|e: std::num::ParseIntError| e.to_string())?;
    if n == 0 {
        return Err("shard total must be >= 1".into());
    }
    if i < 1 || i > n {
        return Err(format!("shard index {i} out of range 1..={n}"));
    }
    Ok((i, n))
}

pub(super) fn merge_reports(
    inputs: &[PathBuf],
    sinks: &ReportSinks,
    history: Option<&PathBuf>,
    config_hash: Option<String>,
    project_root: &Path,
) -> Result<()> {
    // Read + parse lazily so `combine_outcomes` holds only one report's
    // outcomes plus the running map at a time. A sharded report is the whole
    // mutant universe, so eagerly collecting all N would be an N× blow-up.
    let reports = inputs
        .iter()
        .map(|path| -> Result<Vec<MutantOutcome>> { Ok(crate::report::load(path)?.outcomes) });
    let outcomes = combine_outcomes(reports)?;
    let merged = Report::new(outcomes);

    sinks.write_all(&merged)?;
    if !sinks.any() && history.is_none() {
        // Nothing requested → print JSON to stdout so the command is useful by default.
        merged.print(ReportFormat::Json);
    }

    // Emit a complete history entry from the merged report. This is what lets a
    // sharded CI run record its trend point WITHOUT harvesting one shard's
    // history line as a template: git sha/branch come from the merge checkout
    // (the same commit the shards ran on), `fermut_version` is stamped
    // automatically, and the counts are the merged full-universe totals. The
    // one thing merge can't derive is `config_hash` — the run's config lives in
    // the shard jobs — so the caller passes it via `--config-hash`.
    if let Some(p) = history {
        let entry =
            crate::history::HistoryEntry::from_report(&merged, project_root, None, config_hash);
        let line = serde_json::to_string(&entry).context("serializing merged history entry")?;
        // Single staged entry, overwrite (not append): re-running merge must not
        // stack duplicate lines in the staging file.
        std::fs::write(p, format!("{line}\n"))
            .with_context(|| format!("writing {}", p.display()))?;
    }
    Ok(())
}

/// Combine per-report outcome lists into one, in first-seen id order.
///
/// The rule that fixes sharded merges: a real verdict always wins over a
/// `Skipped { filter: "shard" }` placeholder, regardless of input order. Two
/// *different* real outcomes for one id are a conflict (disjoint shards cannot
/// produce it) and error out; identical ones are an idempotent re-merge. Any
/// placeholder still standing at the end means a shard is missing, which also
/// errors — scoring a partial universe as if whole is the failure mode this
/// guards against.
///
/// This is stricter than the old last-write-wins merge on purpose: overlapping
/// or non-shard inputs whose verdicts disagree can no longer be silently
/// collapsed — they must be resolved by the caller.
///
/// Takes a fallible iterator so callers can read+parse each report lazily; only
/// one report's outcomes are live at a time alongside the running map.
fn combine_outcomes(
    reports: impl IntoIterator<Item = Result<Vec<MutantOutcome>>>,
) -> Result<Vec<MutantOutcome>> {
    let mut by_id: HashMap<String, MutantOutcome> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut report_count = 0usize;
    for outcomes in reports {
        let outcomes = outcomes?;
        report_count += 1;
        for outcome in outcomes {
            let Some(existing) = by_id.get_mut(&outcome.mutant().id) else {
                // First sighting of this id — record it and its order slot.
                let id = outcome.mutant().id.clone();
                order.push(id.clone());
                by_id.insert(id, outcome);
                continue;
            };
            match (
                is_shard_placeholder(existing),
                is_shard_placeholder(&outcome),
            ) {
                // Incoming placeholder must never overwrite a verdict a prior
                // shard already produced — this is the bug.
                (false, true) => {}
                // Upgrade a placeholder to the owning shard's verdict.
                (true, false) => *existing = outcome,
                // Both placeholders — keep the first, still a placeholder.
                (true, true) => {}
                // Two real outcomes for one id: the same verdict is an
                // idempotent re-merge; a differing verdict — including same
                // status with a different skip filter or error message — is a
                // genuine conflict and must not be silently resolved.
                (false, false) => {
                    if !same_verdict(existing, &outcome) {
                        bail!(
                            "conflicting verdicts for mutant {:?}: {} vs {}. \
                             Shard reports must cover disjoint mutant sets; overlapping or \
                             non-shard inputs cannot be merged unambiguously.",
                            outcome.mutant().id,
                            existing.status_label(),
                            outcome.status_label(),
                        );
                    }
                }
            }
        }
    }

    let outcomes: Vec<MutantOutcome> = order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect();

    // A surviving placeholder means no input ever supplied a real verdict for
    // that mutant — a shard is missing. Folds the union-completeness check the
    // CI reimplements in jq into `fermut merge` itself.
    let residual = outcomes.iter().filter(|o| is_shard_placeholder(o)).count();
    if residual > 0 {
        bail!(
            "{residual} mutant(s) remain shard-skipped after merging {report_count} report(s): no \
             input supplied a real verdict for them, so a shard is missing. The merged score would \
             cover only part of the mutant universe. Include every shard's report and re-run."
        );
    }

    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::{Mutant, Operator};
    use crate::report::{MutantOutcome, Report};
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    fn mutant(id: &str) -> Mutant {
        Mutant {
            id: id.into(),
            file: PathBuf::from("test.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn merge_history_entry_carries_version_hash_and_merged_counts() {
        let tmp = tempfile::tempdir().unwrap();
        // Two shard reports: 2 killed + 1 survived across the union.
        let r1 = Report::new(vec![
            MutantOutcome::killed(mutant("a")),
            MutantOutcome::survived(mutant("b")),
        ]);
        let r2 = Report::new(vec![MutantOutcome::killed(mutant("c"))]);
        let p1 = tmp.path().join("s1.json");
        let p2 = tmp.path().join("s2.json");
        r1.write_json(&p1).unwrap();
        r2.write_json(&p2).unwrap();

        let combined = tmp.path().join("combined.json");
        let hist = tmp.path().join("entry.json");
        merge_reports(
            &[p1, p2],
            &ReportSinks {
                json: Some(combined),
                ..Default::default()
            },
            Some(&hist),
            Some("deadbeef".into()),
            tmp.path(),
        )
        .unwrap();

        let raw = std::fs::read_to_string(&hist).unwrap();
        let entry: crate::history::HistoryEntry = serde_json::from_str(raw.trim()).unwrap();
        // Auto-stamped by from_report.
        assert_eq!(
            entry.fermut_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        // Passed through (merge can't derive it).
        assert_eq!(entry.config_hash.as_deref(), Some("deadbeef"));
        // Counts are the merged full-universe totals, not one shard's.
        assert_eq!(entry.killed, 2);
        assert_eq!(entry.survived, 1);
        assert_eq!(entry.total, Some(3));
    }

    #[test]
    fn merge_history_overwrites_rather_than_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Report::new(vec![MutantOutcome::killed(mutant("a"))]);
        let p = tmp.path().join("s.json");
        r.write_json(&p).unwrap();
        let hist = tmp.path().join("entry.json");
        for _ in 0..2 {
            merge_reports(
                std::slice::from_ref(&p),
                &ReportSinks {
                    json: Some(tmp.path().join("c.json")),
                    ..Default::default()
                },
                Some(&hist),
                None,
                tmp.path(),
            )
            .unwrap();
        }
        // Re-running merge leaves exactly one staged line, not two.
        let raw = std::fs::read_to_string(&hist).unwrap();
        assert_eq!(raw.lines().filter(|l| !l.trim().is_empty()).count(), 1);
    }

    fn shard_skip(id: &str) -> MutantOutcome {
        MutantOutcome::skipped(mutant(id), "shard")
    }

    /// One shard's report over a 3-mutant universe: real verdict for the id it
    /// owns, `shard` placeholders for the rest.
    fn shard_report(owned: &str, outcome: MutantOutcome, universe: &[&str]) -> Vec<MutantOutcome> {
        universe
            .iter()
            .map(|&id| {
                if id == owned {
                    outcome.clone()
                } else {
                    shard_skip(id)
                }
            })
            .collect()
    }

    fn labels(outcomes: &[MutantOutcome]) -> Vec<(&str, &str)> {
        outcomes
            .iter()
            .map(|o| (o.mutant().id.as_str(), o.status_label()))
            .collect()
    }

    #[test]
    fn real_verdicts_survive_shard_placeholders_regardless_of_order() {
        let universe = ["a", "b", "c"];
        // Three shards, each owning one mutant. Placeholders for "a" and "c"
        // appear AFTER their real verdicts here — the exact last-write-wins
        // collapse the old HashMap insert produced.
        let reports = vec![
            shard_report("a", MutantOutcome::killed(mutant("a")), &universe),
            shard_report("b", MutantOutcome::survived(mutant("b")), &universe),
            shard_report("c", MutantOutcome::timed_out(mutant("c")), &universe),
        ];
        let merged = combine_outcomes(reports.into_iter().map(Ok)).unwrap();
        assert_eq!(
            labels(&merged),
            vec![("a", "killed"), ("b", "survived"), ("c", "timeout")]
        );
    }

    #[test]
    fn missing_shard_leaves_a_placeholder_and_errors() {
        let universe = ["a", "b", "c"];
        // Only two of three shards supplied — "c" is never given a real verdict.
        let reports = vec![
            shard_report("a", MutantOutcome::killed(mutant("a")), &universe),
            shard_report("b", MutantOutcome::survived(mutant("b")), &universe),
        ];
        let err = combine_outcomes(reports.into_iter().map(Ok))
            .unwrap_err()
            .to_string();
        assert!(err.contains("shard is missing"), "got: {err}");
    }

    #[test]
    fn conflicting_real_verdicts_error() {
        let reports = vec![
            vec![MutantOutcome::killed(mutant("a"))],
            vec![MutantOutcome::survived(mutant("a"))],
        ];
        let err = combine_outcomes(reports.into_iter().map(Ok))
            .unwrap_err()
            .to_string();
        assert!(err.contains("conflicting verdicts"), "got: {err}");
    }

    #[test]
    fn identical_verdicts_are_idempotent() {
        let reports = vec![
            vec![MutantOutcome::killed(mutant("a"))],
            vec![MutantOutcome::killed(mutant("a"))],
        ];
        let merged = combine_outcomes(reports.into_iter().map(Ok)).unwrap();
        assert_eq!(labels(&merged), vec![("a", "killed")]);
    }

    #[test]
    fn non_shard_skips_are_preserved_not_treated_as_placeholders() {
        // A coverage-skip is a real decision by the owning shard and must
        // survive, unlike a shard placeholder.
        let universe = ["a", "b"];
        let reports = vec![
            shard_report(
                "a",
                MutantOutcome::skipped(mutant("a"), "coverage"),
                &universe,
            ),
            shard_report("b", MutantOutcome::killed(mutant("b")), &universe),
        ];
        let merged = combine_outcomes(reports.into_iter().map(Ok)).unwrap();
        assert_eq!(labels(&merged), vec![("a", "skipped"), ("b", "killed")]);
    }

    #[test]
    fn same_status_different_detail_conflicts() {
        // Two real skips for one id with different filters share the "skipped"
        // label. A label-only conflict check would treat them as idempotent and
        // silently drop one; full-outcome comparison flags the disagreement.
        let reports = vec![
            vec![MutantOutcome::skipped(mutant("a"), "coverage")],
            vec![MutantOutcome::skipped(mutant("a"), "boundary")],
        ];
        let err = combine_outcomes(reports.into_iter().map(Ok))
            .unwrap_err()
            .to_string();
        assert!(err.contains("conflicting verdicts"), "got: {err}");
    }

    #[test]
    fn same_verdict_with_mutant_field_skew_is_idempotent() {
        // Same killed verdict for one id, but the reports disagree on a `Mutant`
        // field (an older report defaults `stmt_line` to 0). Payload-only
        // comparison must treat this as an idempotent re-merge, not a conflict.
        let mut old = mutant("a");
        old.stmt_line = 0;
        let mut new = mutant("a");
        new.stmt_line = 5;
        let reports = vec![
            vec![MutantOutcome::killed(old)],
            vec![MutantOutcome::killed(new)],
        ];
        let merged = combine_outcomes(reports.into_iter().map(Ok)).unwrap();
        assert_eq!(labels(&merged), vec![("a", "killed")]);
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct MergeArgs {
    /// Input JSON reports to combine. At least one required.
    #[arg(required = true, num_args = 1..)]
    pub(crate) inputs: Vec<std::path::PathBuf>,

    /// Write the combined JSON report here. Otherwise prints to stdout.
    #[arg(long)]
    pub(crate) json: Option<std::path::PathBuf>,

    /// Also write a JUnit XML report.
    #[arg(long)]
    pub(crate) junit: Option<std::path::PathBuf>,

    /// Also write an HTML report.
    #[arg(long)]
    pub(crate) html: Option<std::path::PathBuf>,

    /// Also write a Markdown report.
    #[arg(long)]
    pub(crate) markdown: Option<std::path::PathBuf>,

    /// Write a single history entry built from the merged report to this
    /// path (overwriting). Git sha/branch are read from the merge checkout
    /// and the fermut version is stamped automatically; the counts are the
    /// merged full-universe totals. Lets a sharded run record its trend
    /// point without harvesting a shard's history line as a template.
    #[arg(long, value_name = "PATH")]
    pub(crate) history: Option<std::path::PathBuf>,

    /// `config_hash` to stamp into the `--history` entry. Merge can't derive
    /// it (the run config lives in the shard jobs), so pass the value the
    /// shards recorded, e.g. `$(jq -r .config_hash shard-1-entry.json)`.
    #[arg(long, value_name = "HEX")]
    pub(crate) config_hash: Option<String>,

    /// Project root for git sha/branch discovery in the `--history` entry.
    /// Defaults to the current directory (the merge checkout).
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub(crate) project: std::path::PathBuf,
}

/// Dispatch handler: nests the output-path flags into a [`ReportSinks`] and
/// runs the shard merge.
pub(crate) fn run(args: MergeArgs) -> Result<()> {
    let MergeArgs {
        inputs,
        json,
        junit,
        html,
        markdown,
        history,
        config_hash,
        project,
    } = args;
    merge_reports(
        &inputs,
        &ReportSinks {
            json,
            junit,
            html,
            markdown,
        },
        history.as_ref(),
        config_hash,
        &project,
    )
}
