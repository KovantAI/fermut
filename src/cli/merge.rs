//! `fermut merge` — combine multiple JSON shard reports into one, and the
//! shared `i/n` shard-spec parser used by both the CLI flag and the config
//! file value.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::report::{MutantOutcome, Report, ReportFormat};

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

#[allow(clippy::too_many_arguments)]
pub(super) fn merge_reports(
    inputs: &[PathBuf],
    json: Option<&PathBuf>,
    junit: Option<&PathBuf>,
    html: Option<&PathBuf>,
    markdown: Option<&PathBuf>,
    history: Option<&PathBuf>,
    config_hash: Option<String>,
    project_root: &Path,
) -> Result<()> {
    let mut by_id: HashMap<String, MutantOutcome> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for path in inputs {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let report: Report =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        for outcome in report.outcomes {
            let id = outcome.mutant().id.clone();
            if by_id.insert(id.clone(), outcome).is_none() {
                order.push(id);
            }
        }
    }
    let outcomes: Vec<MutantOutcome> = order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::{Mutant, Operator};
    use crate::report::{MutantOutcome, Report};
    use ruff_text_size::TextRange;

    fn mutant(id: &str) -> Mutant {
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
            Some(&combined),
            None,
            None,
            None,
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
                Some(&tmp.path().join("c.json")),
                None,
                None,
                None,
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
}
