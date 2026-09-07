//! `fermut run` — generate mutants, run the suite against each, report and
//! gate. This is the engine-facing command: it builds the runtime `Config`,
//! drives `engine::run` (or the `--watch` loop), writes every requested report
//! format, and decides the process exit code (`--fail-under`,
//! `--fail-on-regression`, `--no-fail`, and the scoreless/vacuous-100% guard).

use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;

use crate::cli::build_config::build_config;
use crate::cli::{CacheScopeCli, FilterArgs, Format, IsolationCli, RunnerCli};
use crate::history::HistoryEntry;
use crate::report::{Report, ReportSinks};

/// Every `fermut run` flag, mirrored from the `Cmd::Run` clap variant. The
/// dispatch arm destructures `Cmd::Run` straight into this struct; keeping the
/// CLI enum types (`RunnerCli`, …) here means the body stays a verbatim move of
/// the old inline handler.
#[derive(Debug)]
pub struct RunOpts {
    pub path: PathBuf,
    pub tests: Option<PathBuf>,
    pub jobs: Option<usize>,
    pub timeout: Option<u64>,
    pub no_ty_filter: bool,
    pub ruff_filter: bool,
    pub tce: bool,
    pub hypothesis_seed: Option<u64>,
    pub pytest_args: Vec<String>,
    pub no_cache: bool,
    pub cache_path: Option<PathBuf>,
    pub no_history: bool,
    pub history_path: Option<PathBuf>,
    pub sample: Option<f64>,
    pub sample_seed: Option<u64>,
    pub shard: Option<(u32, u32)>,
    pub runner: Option<RunnerCli>,
    pub python: Option<PathBuf>,
    pub isolation: Option<IsolationCli>,
    pub no_equiv_detect: bool,
    pub cache_scope: Option<CacheScopeCli>,
    pub annotate: bool,
    pub watch: bool,
    pub format: Format,
    pub sinks: ReportSinks,
    pub trend: bool,
    pub trend_branch: Option<String>,
    pub fail_on_regression: Option<f64>,
    pub fail_under: Option<f64>,
    pub no_fail: bool,
    pub no_verify_baseline: bool,
    pub baseline_timeout: Option<u64>,
    pub no_smart_order: bool,
    pub smart_order: bool,
    pub max_time: Option<u64>,
    pub filter: FilterArgs,
}

pub fn run(opts: RunOpts) -> Result<()> {
    let RunOpts {
        path,
        tests,
        jobs,
        timeout,
        no_ty_filter,
        ruff_filter,
        tce,
        hypothesis_seed,
        pytest_args,
        no_cache,
        cache_path,
        no_history,
        history_path,
        sample,
        sample_seed,
        shard,
        runner,
        python,
        isolation,
        no_equiv_detect,
        cache_scope,
        annotate,
        watch,
        format,
        sinks,
        trend,
        trend_branch,
        fail_on_regression,
        fail_under,
        no_fail,
        no_verify_baseline,
        baseline_timeout,
        no_smart_order,
        smart_order,
        max_time,
        filter: f,
    } = opts;
    let cfg = build_config(
        path,
        tests,
        jobs,
        timeout,
        no_ty_filter,
        ruff_filter,
        tce,
        hypothesis_seed,
        pytest_args,
        no_cache,
        cache_path,
        no_history,
        history_path,
        sample,
        sample_seed,
        shard,
        runner.map(Into::into),
        python,
        isolation.map(Into::into),
        no_equiv_detect,
        cache_scope.map(Into::into),
        fail_under,
        no_verify_baseline,
        baseline_timeout,
        no_smart_order,
        smart_order,
        max_time,
        f,
    )?;
    let want_annotations = annotate || std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
    let fmt = format.into();
    // The closure receives the *prior* history (entries that
    // existed before this run started) so the markdown trend
    // block plots "this run vs every earlier run" without
    // depending on whether `engine::run`'s append to the
    // history file succeeded. Append can silently fail on
    // read-only filesystems or full disks; the caller is
    // responsible for handing us the snapshot.
    let on_report = move |report: &Report, prior: &[HistoryEntry]| -> Result<()> {
        report.print(fmt);
        if want_annotations {
            report.print_github_annotations();
        }
        if let Some(p) = &sinks.json {
            report.write_json(p)?;
        }
        if let Some(p) = &sinks.junit {
            report.write_junit(p)?;
        }
        if let Some(p) = &sinks.html {
            report.write_html(p)?;
        }
        if let Some(p) = &sinks.markdown {
            if trend {
                let filtered: Vec<HistoryEntry> = match &trend_branch {
                    Some(b) => prior
                        .iter()
                        .filter(|e| e.git_branch.as_deref() == Some(b.as_str()))
                        .cloned()
                        .collect(),
                    None => prior.to_vec(),
                };
                report.write_markdown_with_history(p, &filtered)?;
            } else {
                report.write_markdown(p)?;
            }
        }
        Ok(())
    };
    if watch {
        crate::watch::watch_loop(&cfg, on_report)
    } else {
        let prior_snapshot = if cfg.history {
            crate::history::load(&cfg.history_path).unwrap_or_default()
        } else {
            Vec::new()
        };
        let (report, current_entry) = crate::engine::run(&cfg)?;
        on_report(&report, &prior_snapshot)?;
        let mut gate_failed = false;
        if let Some(threshold) = fail_on_regression {
            if !cfg.history {
                eprintln!(
                    "--fail-on-regression needs history enabled \
                     (currently --no-history)"
                );
                gate_failed = true;
            } else if let Some(current) = current_entry.as_ref() {
                if current.partial {
                    // A `--max-time` run truncated by the budget
                    // scored a nondeterministic subset, so it can't
                    // anchor a regression comparison — `regression_
                    // against` excludes it. Say so, or a green gate
                    // reads as "no regression" when it's really "not
                    // evaluated".
                    eprintln!(
                        "--fail-on-regression skipped: this run hit the --max-time \
                         budget and scored a partial subset (not comparable). Run \
                         without --max-time to gate on regression."
                    );
                }
                // Reload post-run so concurrent writers (e.g.
                // parallel `--shard` workers) become visible,
                // then compare against the in-memory current
                // entry — never against `entries.last()`,
                // which is the *previous* run when our own
                // append silently failed.
                let prior_now = load_prior_excluding(&cfg.history_path, current);
                if let Some(drop) = crate::history::regression_against(&prior_now, current) {
                    if drop > threshold {
                        eprintln!(
                            "mutation score regressed by {drop:.1} pts \
                             (threshold {threshold:.1})"
                        );
                        gate_failed = true;
                    }
                }
            }
        }
        // A zero-denominator run has no score. Say so, so a green
        // gate is never mistaken for a genuine 100% (an all-errored
        // run fails below; nothing-to-score with no errors passes as
        // a legitimate N/A). Under `--no-fail` the exit is suppressed
        // regardless, so the errored case reports N/A too — claiming
        // "the gate fails" would contradict the exit-0 that follows.
        if report.is_scoreless() {
            eprintln!("{}", scoreless_note(&report, no_fail));
        }
        // `--no-fail`: the run exists to produce a report, not to
        // gate. Conflicts with the gate flags at the parser, so
        // `gate_failed` is always false here; the guard is explicit
        // so the intent survives a future flag that sets it.
        if gate_exits_nonzero(no_fail, report.should_fail(cfg.fail_under), gate_failed) {
            std::process::exit(1);
        }
        Ok(())
    }
}

/// Load history from disk and drop the tail entry if it is the run we
/// just finished. When append succeeded the tail equals `current` and is
/// stripped; when it failed the tail is some earlier run and stays in
/// the prior window. Either way the returned slice never includes
/// `current`, so [`crate::history::regression_against`] sees a clean
/// "everything before this run" view.
fn load_prior_excluding(path: &Path, current: &HistoryEntry) -> Vec<HistoryEntry> {
    let mut prior = crate::history::load(path).unwrap_or_default();
    if prior.last().map(|e| same_run(e, current)).unwrap_or(false) {
        prior.pop();
    }
    prior
}

/// Whether `fermut run` should exit non-zero after producing its report.
/// `--no-fail` suppresses the exit unconditionally — the run existed to
/// produce a report, not to gate — otherwise the run fails if the mutation
/// result is below the bar (`result_fails`) or a regression gate tripped
/// (`gate_failed`). Extracted so the gate decision is unit-testable without a
/// live pytest run.
fn gate_exits_nonzero(no_fail: bool, result_fails: bool, gate_failed: bool) -> bool {
    !no_fail && (result_fails || gate_failed)
}

/// Human note for a scoreless (zero-denominator) run — its score is undefined
/// (N/A), never a vacuous 100%. Distinguishes the causes so the message is
/// actionable: all-errored (which fails the gate unless `--no-fail`), a
/// `--max-time` budget that expired before any mutant ran, or a genuinely
/// empty scope. Pure so the branch selection is unit-tested.
fn scoreless_note(report: &Report, no_fail: bool) -> String {
    let errored = report.counts().errored;
    if errored > 0 && !no_fail {
        format!(
            "no mutants scored: {errored} errored, so the score is \
             undefined (not 100%) and the gate fails"
        )
    } else if errored > 0 {
        format!(
            "no mutants scored: {errored} errored, so the score is \
             undefined (not 100%) — reported N/A, exit suppressed by --no-fail"
        )
    } else if let Some((_, n)) = report
        .skipped_by_filter()
        .into_iter()
        .find(|(f, _)| f == crate::engine::TIME_BUDGET_FILTER)
    {
        // The budget expired before any mutant was scored — distinct from an
        // empty scope; say so rather than claiming "nothing to mutate".
        format!(
            "no mutants scored: --max-time budget expired before any mutant \
             ran ({n} skipped/`time-budget`) — mutation score N/A (not 100%)"
        )
    } else {
        "no mutants scored: nothing to mutate in scope — \
         mutation score N/A (not 100%)"
            .to_string()
    }
}

/// Two entries describe the same run when the identifying fields the
/// engine writes — timestamp, git sha, mutation score — all match.
/// Exact match because the engine constructs both sides from the same
/// `Report` and `Instant` window; serde round-tripping preserves bits.
fn same_run(a: &HistoryEntry, b: &HistoryEntry) -> bool {
    a.timestamp == b.timestamp
        && a.git_sha == b.git_sha
        && a.mutation_score.to_bits() == b.mutation_score.to_bits()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_fail_suppresses_every_gate_exit() {
        // Whatever the result/regression state, --no-fail exits 0.
        for result_fails in [false, true] {
            for gate_failed in [false, true] {
                assert!(
                    !gate_exits_nonzero(true, result_fails, gate_failed),
                    "no_fail must suppress exit (result_fails={result_fails}, gate_failed={gate_failed})"
                );
            }
        }
    }

    #[test]
    fn without_no_fail_a_failing_result_or_gate_exits_nonzero() {
        assert!(!gate_exits_nonzero(false, false, false), "clean run passes");
        assert!(
            gate_exits_nonzero(false, true, false),
            "survivors/threshold fail"
        );
        assert!(
            gate_exits_nonzero(false, false, true),
            "regression gate fails"
        );
        assert!(gate_exits_nonzero(false, true, true), "both fail");
    }

    fn scoreless_mutant(id: &str) -> crate::mutator::Mutant {
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

    #[test]
    fn scoreless_note_flags_time_budget_exhaustion() {
        use crate::report::MutantOutcome;
        // Budget expired before any mutant ran: every mutant is a time-budget
        // skip, no errors. The note must name the budget, not "nothing to
        // mutate".
        let report = Report::new(vec![
            MutantOutcome::skipped(scoreless_mutant("a"), crate::engine::TIME_BUDGET_FILTER),
            MutantOutcome::skipped(scoreless_mutant("b"), crate::engine::TIME_BUDGET_FILTER),
        ]);
        let note = scoreless_note(&report, false);
        assert!(note.contains("--max-time budget expired"), "got: {note}");
        assert!(note.contains("2 skipped"), "reports the count: {note}");
        assert!(!note.contains("nothing to mutate"));
    }

    #[test]
    fn scoreless_note_empty_scope_vs_errored() {
        use crate::report::MutantOutcome;
        // No mutants at all → empty scope wording.
        let empty = Report::new(vec![]);
        assert!(scoreless_note(&empty, false).contains("nothing to mutate in scope"));

        // All errored, gate live → the gate-fails wording.
        let errored = Report::new(vec![MutantOutcome::error(
            scoreless_mutant("e"),
            "boom".into(),
        )]);
        assert!(scoreless_note(&errored, false).contains("and the gate fails"));
        // Same run under --no-fail → exit-suppressed wording, never "gate fails".
        let note = scoreless_note(&errored, true);
        assert!(note.contains("exit suppressed by --no-fail"));
        assert!(!note.contains("and the gate fails"));
    }
}
