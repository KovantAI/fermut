//! `fermut show` — `mutmut show`-style inspector. Reads a prior JSON report
//! and prints either a list (default: survivors only) or a single mutant's
//! detail view with a regenerated unified diff.

use std::path::Path;

use anyhow::Result;

use crate::report::{unified_diff_for, MutantOutcome, Report};

pub fn show(report_path: &Path, target: Option<&str>, all: bool) -> Result<()> {
    let report = crate::report::load(report_path)?;

    match target {
        None => print_list(&report, all),
        Some(t) => print_detail(&report, t),
    }
}

fn print_list(report: &Report, all: bool) -> Result<()> {
    let mut shown = 0usize;
    for (i, o) in report.outcomes.iter().enumerate() {
        let is_survivor = matches!(
            o,
            MutantOutcome::Survived { .. } | MutantOutcome::TimedOut { .. }
        );
        if !all && !is_survivor {
            continue;
        }
        let m = o.mutant();
        // Reuse `describe()` so the list carries the same `@offset`
        // disambiguator as `fermut run` — otherwise two co-located same-op
        // mutants render as byte-identical rows distinguished only by index.
        println!(
            "[{:>4}] {:<9} {}",
            i + 1,
            o.status_label().to_uppercase(),
            m.describe()
        );
        shown += 1;
    }
    let c = report.counts();
    println!(
        "\n{} shown of {} total — killed: {}, survived: {}, timeout: {}, skipped: {}, equivalent: {}, errored: {}  | score: {}",
        shown,
        c.total(),
        c.killed,
        c.survived,
        c.timed_out,
        c.skipped,
        c.equivalent,
        c.errored,
        c.score_label()
    );
    Ok(())
}

fn print_detail(report: &Report, target: &str) -> Result<()> {
    let outcome = locate_outcome(report, target)?;
    let m = outcome.mutant();
    println!("{}:{}", m.file.display(), m.line);
    println!("operator : {}", m.operator.name());
    println!("status   : {}", outcome.status_label());
    println!("id       : {}", m.id);
    println!("mutation : `{}` → `{}`", m.original, m.replacement);
    if let MutantOutcome::Skipped { filter, .. } = outcome {
        println!("filter   : {filter}");
    }
    if let MutantOutcome::Error { message, .. } = outcome {
        println!("error    : {message}");
    }
    if let MutantOutcome::Equivalent { reason, source, .. } = outcome {
        println!("detector : {source}");
        println!("reason   : {reason}");
    }
    println!();
    match unified_diff_for(m) {
        Ok(diff) => print!("{diff}"),
        Err(e) => eprintln!("(could not regenerate diff: {e})"),
    }
    Ok(())
}

fn locate_outcome<'a>(report: &'a Report, target: &str) -> Result<&'a MutantOutcome> {
    // Shared with `explain` — resolves index / full id / printed
    // `file:line@offset` form, and errors (rather than silently picking the
    // first) when a partial selector is ambiguous.
    crate::cli::subcmd::explain::locate_outcome_impl(&report.outcomes, target)
}

#[derive(clap::Args, Debug)]
pub(crate) struct ShowArgs {
    /// JSON report from `fermut run`. Defaults to `.fermut/last.json`, which
    /// every `run` writes.
    pub(crate) report: Option<std::path::PathBuf>,

    /// Optional mutant selector: 1-based index, or substring of mutant id.
    /// Omit to print a summary list of survivors.
    pub(crate) target: Option<String>,

    /// Show all outcomes, not just survivors, when target is omitted.
    #[arg(long)]
    pub(crate) all: bool,
}

/// Dispatch handler: unpacks [`ShowArgs`] and runs the inspector.
pub(crate) fn run(args: ShowArgs) -> Result<()> {
    let ShowArgs {
        report,
        target,
        all,
    } = args;
    let (report, target) = crate::cli::report_and_target(report, target);
    show(&report, target.as_deref(), all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::testing::make_mutant;

    #[test]
    fn list_and_detail_views_render_every_status() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("t.py");
        std::fs::write(&src, "a + b\n").unwrap();
        let mut m = make_mutant();
        m.file = src;
        let mut missing = make_mutant();
        missing.id = "gone".into();
        missing.file = tmp.path().join("missing.py");
        let report = Report::new(vec![
            MutantOutcome::survived(m.clone()),
            MutantOutcome::killed(m.clone()),
            MutantOutcome::skipped(m.clone(), "ty"),
            MutantOutcome::error(m.clone(), "boom".into()),
            MutantOutcome::equivalent(m, "same bytecode", "bytecode-identity"),
            MutantOutcome::survived(missing),
        ]);
        let path = tmp.path().join("r.json");
        std::fs::write(&path, serde_json::to_string(&report).unwrap()).unwrap();

        show(&path, None, false).unwrap();
        run(ShowArgs {
            report: Some(path.clone()),
            target: None,
            all: true,
        })
        .unwrap();
        for i in 1..=6 {
            show(&path, Some(&i.to_string()), false).unwrap();
        }
        assert!(show(&path, Some("99"), false).is_err());
        assert!(show(&tmp.path().join("nope.json"), None, false).is_err());
    }
}
