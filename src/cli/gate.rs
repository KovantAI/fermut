//! Pure gate/history helpers for `fermut run` and `fermut clean`.
//!
//! Extracted from the dispatch so the exit-code decision, the scoreless-run
//! note, and the history-path resolution are unit-testable without a live
//! pytest run.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::history::HistoryEntry;
use crate::report::Report;

/// Load history from disk and drop the tail entry if it is the run we
/// just finished. When append succeeded the tail equals `current` and is
/// stripped; when it failed the tail is some earlier run and stays in
/// the prior window. Either way the returned slice never includes
/// `current`, so [`crate::history::regression_against`] sees a clean
/// "everything before this run" view.
pub(crate) fn load_prior_excluding(path: &Path, current: &HistoryEntry) -> Vec<HistoryEntry> {
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
pub(crate) fn gate_exits_nonzero(no_fail: bool, result_fails: bool, gate_failed: bool) -> bool {
    !no_fail && (result_fails || gate_failed)
}

/// Human note for a scoreless (zero-denominator) run — its score is undefined
/// (N/A), never a vacuous 100%. Distinguishes the causes so the message is
/// actionable: all-errored (which fails the gate unless `--no-fail`), a
/// `--max-time` budget that expired before any mutant ran, or a genuinely
/// empty scope. Pure so the branch selection is unit-tested.
pub(crate) fn scoreless_note(report: &Report, no_fail: bool) -> String {
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

/// Resolve the run-history log path the same way `build_config` does, but
/// without requiring the full runtime `Config`. Used by `fermut clean` so
/// it preserves the user's configured history file (which may live under
/// `.fermut/` with a custom name) instead of only the default
/// `history.jsonl`.
pub(crate) fn resolve_history_path(
    path: &Path,
    cli_history_path: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(p) = cli_history_path {
        return Ok(p);
    }
    let loaded = crate::config::LoadedConfig::load(path)?;
    Ok(loaded
        .file
        .history_path
        .clone()
        .map(|p| loaded.resolve_path(p))
        .unwrap_or_else(|| {
            crate::history::default_history_path(&crate::history::resolve_root(path))
        }))
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
    use std::path::PathBuf;

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
