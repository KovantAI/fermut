//! `fermut merge` — combine multiple JSON shard reports into one, and the
//! shared `i/n` shard-spec parser used by both the CLI flag and the config
//! file value.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::report::{MutantOutcome, Report, ReportFormat};

/// A `Skipped { filter: "shard" }` outcome is not a verdict — it is the
/// placeholder a sharded run emits for every mutant *outside* its own slice.
/// Each shard's report describes the whole universe (its slice with real
/// verdicts, the rest as this placeholder), so merging must let any real
/// verdict win over it rather than last-write-wins. Matches `ShardFilter::name`.
fn is_shard_placeholder(o: &MutantOutcome) -> bool {
    matches!(o, MutantOutcome::Skipped { filter, .. } if filter == "shard")
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
    json: Option<&PathBuf>,
    junit: Option<&PathBuf>,
    html: Option<&PathBuf>,
    markdown: Option<&PathBuf>,
) -> Result<()> {
    let mut reports: Vec<Vec<MutantOutcome>> = Vec::with_capacity(inputs.len());
    for path in inputs {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let report: Report =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        reports.push(report.outcomes);
    }
    let outcomes = combine_outcomes(reports)?;
    let merged = Report::new(outcomes);

    if let Some(p) = json {
        merged.write_json(p)?;
    } else if junit.is_none() && html.is_none() && markdown.is_none() {
        // Nothing requested → print JSON to stdout so the command is useful by default.
        merged.print(ReportFormat::Json);
    }
    if let Some(p) = junit {
        merged.write_junit(p)?;
    }
    if let Some(p) = html {
        merged.write_html(p)?;
    }
    if let Some(p) = markdown {
        merged.write_markdown(p)?;
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
fn combine_outcomes(
    reports: impl IntoIterator<Item = Vec<MutantOutcome>>,
) -> Result<Vec<MutantOutcome>> {
    let mut by_id: HashMap<String, MutantOutcome> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut report_count = 0usize;
    for outcomes in reports {
        report_count += 1;
        for outcome in outcomes {
            let id = outcome.mutant().id.clone();
            match by_id.entry(id.clone()) {
                Entry::Vacant(e) => {
                    e.insert(outcome);
                    order.push(id);
                }
                Entry::Occupied(mut e) => {
                    match (
                        is_shard_placeholder(e.get()),
                        is_shard_placeholder(&outcome),
                    ) {
                        // Incoming placeholder must never overwrite a verdict a
                        // prior shard already produced — this is the bug.
                        (false, true) => {}
                        // Upgrade a placeholder to the owning shard's verdict.
                        (true, false) => {
                            e.insert(outcome);
                        }
                        // Both placeholders — keep the first, still a placeholder.
                        (true, true) => {}
                        // Two real outcomes for one id: idempotent re-merge is
                        // fine, genuine disagreement is refused.
                        (false, false) => {
                            let kept = e.get().status_label();
                            let dropped = outcome.status_label();
                            if kept != dropped {
                                bail!(
                                    "conflicting verdicts for mutant {id:?}: {kept} vs {dropped}. \
                                     Shard reports must cover disjoint mutant sets; overlapping or \
                                     non-shard inputs cannot be merged unambiguously."
                                );
                            }
                        }
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
        let merged = combine_outcomes(reports).unwrap();
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
        let err = combine_outcomes(reports).unwrap_err().to_string();
        assert!(err.contains("shard is missing"), "got: {err}");
    }

    #[test]
    fn conflicting_real_verdicts_error() {
        let reports = vec![
            vec![MutantOutcome::killed(mutant("a"))],
            vec![MutantOutcome::survived(mutant("a"))],
        ];
        let err = combine_outcomes(reports).unwrap_err().to_string();
        assert!(err.contains("conflicting verdicts"), "got: {err}");
    }

    #[test]
    fn identical_verdicts_are_idempotent() {
        let reports = vec![
            vec![MutantOutcome::killed(mutant("a"))],
            vec![MutantOutcome::killed(mutant("a"))],
        ];
        let merged = combine_outcomes(reports).unwrap();
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
        let merged = combine_outcomes(reports).unwrap();
        assert_eq!(labels(&merged), vec![("a", "skipped"), ("b", "killed")]);
    }
}
