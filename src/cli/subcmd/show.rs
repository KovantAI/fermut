//! `fermut show` — `mutmut show`-style inspector. Reads a prior JSON report
//! and prints either a list (default: survivors only) or a single mutant's
//! detail view with a regenerated unified diff.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::report::{unified_diff_for, MutantOutcome, Report};

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// Path to a JSON report produced by `fermut run --json …`.
    pub report: PathBuf,

    /// Optional mutant selector: 1-based index, or substring of mutant id.
    /// Omit to print a summary list of survivors.
    pub target: Option<String>,

    /// Show all outcomes, not just survivors, when target is omitted.
    #[arg(long)]
    pub all: bool,
}

pub fn run(args: ShowArgs) -> Result<()> {
    show(&args.report, args.target.as_deref(), args.all)
}

pub fn show(report_path: &Path, target: Option<&str>, all: bool) -> Result<()> {
    let raw = std::fs::read_to_string(report_path)
        .with_context(|| format!("reading {}", report_path.display()))?;
    let report: Report =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", report_path.display()))?;

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
