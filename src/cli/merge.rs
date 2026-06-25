//! `fermut merge` — combine multiple JSON shard reports into one, and the
//! shared `i/n` shard-spec parser used by both the CLI flag and the config
//! file value.

use std::collections::HashMap;
use std::path::PathBuf;

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

pub(super) fn merge_reports(
    inputs: &[PathBuf],
    json: Option<&PathBuf>,
    junit: Option<&PathBuf>,
    html: Option<&PathBuf>,
    markdown: Option<&PathBuf>,
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
    Ok(())
}
