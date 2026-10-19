//! Shared exit-code → verdict classifier for the subprocess runners.
//!
//! Both the pytest and unittest runners decide survived/killed/anomaly from a
//! finished process's exit status. "Anomaly" is the load-bearing case: a
//! usage error, internal error, or "no tests ran" is *not* a verdict on the
//! mutant, so counting it as a kill manufactures a fake kill that inflates the
//! mutation score — the primary number in the output. Anomalies become
//! [`crate::report::MutantOutcome::error`], excluded from the score denominator.
//!
//! The two tools do NOT share exit-code *meanings*, so the classifier is
//! parameterized by [`TestTool`] rather than being a single numeric table:
//!
//! - pytest ([exit codes]): 0 pass, 1 tests failed, 2 collection interrupted,
//!   3 internal error, 4 usage error, 5 no tests collected. A mutant that
//!   breaks import makes pytest fail collection — that IS a kill. Which exit
//!   code it produces depends on how the tests were selected: a file/dir
//!   argument gives exit 2, but a **node id** (`file.py::test`, what
//!   coverage-driven selection passes) gives exit 4 with `found no collectors`,
//!   because the id can't be resolved inside a module that never imported. So
//!   exit 4 alone is ambiguous between "the mutant broke import" (a kill) and
//!   "a stale/bad node id" (`not found:`, a real anomaly); the pytest runner
//!   disambiguates it from collection output via [`is_collection_failure`].
//! - unittest: 0 success, 1 failures/errors (including a broken import, which
//!   unittest runs as an errored `_FailedTest` — a real kill), 2 usage error
//!   from argparse, and on Python 3.12+ exit 5 when no tests ran. unittest
//!   never emits 3/4, and its exit 2 is a usage error (NOT pytest's kill-worthy
//!   collection error) — hence the per-tool split.
//!
//! [exit codes]: https://docs.pytest.org/en/stable/reference/exit-codes.html

/// Which framework produced the exit status — selects the code→verdict table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TestTool {
    Pytest,
    Unittest,
}

/// Verdict a finished runner process's exit status implies, decoupled from any
/// I/O so the mapping can be unit-tested. `code` is `status.code()`; `None`
/// means the process was killed by a signal (segfault etc.) → a kill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitVerdict {
    Survived,
    Killed,
    Anomaly(i32),
}

pub(crate) fn classify_exit(tool: TestTool, code: Option<i32>) -> ExitVerdict {
    match tool {
        TestTool::Pytest => match code {
            Some(0) => ExitVerdict::Survived,
            // exit 1 = a test failed; exit 2 = collection error (a mutant broke
            // the module's import) — both are the suite failing on the mutant.
            Some(1 | 2) => ExitVerdict::Killed,
            // internal error / usage error / no tests collected — not verdicts.
            Some(c @ 3..=5) => ExitVerdict::Anomaly(c),
            _ => ExitVerdict::Killed,
        },
        TestTool::Unittest => match code {
            Some(0) => ExitVerdict::Survived,
            // exit 1 covers failures AND errors, including a mutant that broke
            // an import (unittest runs it as an errored test) — a real kill.
            Some(1) => ExitVerdict::Killed,
            // exit 2 is an argparse usage error and exit 5 (Python 3.12+) is
            // "no tests ran" — infrastructure faults the mutant can't cause,
            // so they must not be scored as kills.
            Some(c @ (2 | 5)) => ExitVerdict::Anomaly(c),
            _ => ExitVerdict::Killed,
        },
    }
}

/// pytest exit code for a usage error. Ambiguous under node-id selection — see
/// the module docs and [`is_collection_failure`].
pub(crate) const PYTEST_USAGE_ERROR: i32 = 4;

/// Whether combined pytest stdout+stderr from a node-id-selected run shows that
/// a selected test module failed to *collect* (an import-time error), as
/// opposed to a node id that simply doesn't exist.
///
/// pytest 7–9 report the former as `ERROR: found no collectors for <id>` on
/// stderr; a missing id is `ERROR: not found: <id>` instead. That alone is
/// required, plus independent evidence on stdout that collection actually
/// errored, so an unrelated usage error can't be read as a kill. The stdout
/// evidence varies with flags — fermut runs `--tb=no`, which drops the
/// `ERROR collecting <file>` section — so any of these counts: that section,
/// a short-summary `ERROR <file> - <exc>` line, or an `N error(s) in` tally.
///
/// A conftest that imports the mutated code fails before any collection, with
/// `ImportError while loading conftest '<path>'.` (pytest 7–9 print that
/// header for any exception, not only `ImportError`) and no node-id error.
/// That is a kill too: the mutant broke the import of test code.
///
/// A module that was *already* broken on the clean tree would match too — the
/// same caveat as exit 2, which fermut already scores as a kill; the baseline
/// check is what guards against it.
pub(crate) fn is_collection_failure(output: &str) -> bool {
    if output.contains("ImportError while loading conftest") {
        return true;
    }
    let unresolved = output.contains("found no collectors for");
    let collect_errored = output.lines().any(|l| {
        l.contains("ERROR collecting")
            || (l.starts_with("ERROR ") && l.contains(" - "))
            || l.contains(" error in ")
            || l.contains(" errors in ")
    });
    unresolved && collect_errored
}

/// Human-readable label for an anomaly exit code, used to describe the
/// resulting [`crate::report::MutantOutcome::error`]. Only codes that
/// [`classify_exit`] maps to `Anomaly` for the given tool reach here.
pub(crate) fn anomaly_message(tool: TestTool, code: i32) -> String {
    let (name, what) = match tool {
        TestTool::Pytest => (
            "pytest",
            match code {
                3 => "internal error",
                4 => "usage error",
                5 => "no tests collected",
                _ => "anomalous exit",
            },
        ),
        TestTool::Unittest => (
            "unittest",
            match code {
                2 => "usage error",
                5 => "no tests ran",
                _ => "anomalous exit",
            },
        ),
    };
    format!("{name} exit {code}: {what} (not a kill)")
}

#[cfg(test)]
mod tests {
    use super::ExitVerdict::{Anomaly, Killed, Survived};
    use super::*;

    #[test]
    fn pytest_maps_codes_to_verdicts() {
        assert_eq!(classify_exit(TestTool::Pytest, Some(0)), Survived);
        assert_eq!(classify_exit(TestTool::Pytest, Some(1)), Killed);
        // collection error (broken import) is a kill, not an anomaly.
        assert_eq!(classify_exit(TestTool::Pytest, Some(2)), Killed);
        assert_eq!(classify_exit(TestTool::Pytest, Some(3)), Anomaly(3));
        assert_eq!(classify_exit(TestTool::Pytest, Some(4)), Anomaly(4));
        assert_eq!(classify_exit(TestTool::Pytest, Some(5)), Anomaly(5));
        // signal death / other nonzero → kill.
        assert_eq!(classify_exit(TestTool::Pytest, Some(139)), Killed);
        assert_eq!(classify_exit(TestTool::Pytest, None), Killed);
    }

    #[test]
    fn unittest_maps_codes_to_verdicts() {
        assert_eq!(classify_exit(TestTool::Unittest, Some(0)), Survived);
        // failures, errors, and broken imports all land on exit 1 → kill.
        assert_eq!(classify_exit(TestTool::Unittest, Some(1)), Killed);
        // the parity gap this fixes: a usage error must NOT score as a kill.
        assert_eq!(classify_exit(TestTool::Unittest, Some(2)), Anomaly(2));
        // Python 3.12+ "no tests ran".
        assert_eq!(classify_exit(TestTool::Unittest, Some(5)), Anomaly(5));
        assert_eq!(classify_exit(TestTool::Unittest, Some(139)), Killed);
        assert_eq!(classify_exit(TestTool::Unittest, None), Killed);
    }

    // pytest 9.1.1 output (stdout + stderr) for a node id whose module raises
    // at import, under fermut's own flags (`-x --tb=no -q`) — no `ERROR
    // collecting` section, only the short summary.
    const BROKEN_IMPORT_TB_NO: &str = "\
ERROR: found no collectors for /proj/tests/a_test.py::test_a


=========================== short test summary info ============================
ERROR tests/a_test.py - ValueError: boom at import
!!!!!!!!!!!!!!!!!!!!!!!!!! stopping after 1 failures !!!!!!!!!!!!!!!!!!!!!!!!!!!
no tests collected, 1 error in 0.00s
";
    // Same failure with default traceback output (abridged).
    const BROKEN_IMPORT_TB: &str = "\
==================================== ERRORS ====================================
_______________________ ERROR collecting tests/a_test.py _______________________
E   ValueError: boom at import
ERROR: found no collectors for /proj/tests/a_test.py::test_a
";
    // A node id that doesn't exist in a healthy module.
    const MISSING_ID: &str = "\
no tests ran in 0.00s
ERROR: not found: /proj/tests/b_test.py::nope
(no match in any of [<Module b_test.py>])
";

    #[test]
    fn collection_failure_detects_import_breakage_under_node_ids() {
        assert!(is_collection_failure(BROKEN_IMPORT_TB_NO));
        assert!(is_collection_failure(BROKEN_IMPORT_TB));
    }

    // pytest 9.1.1, `--collect-only -x --tb=no -q`, when `tests/conftest.py`
    // imports a module the mutant made raise at import time.
    const BROKEN_CONFTEST: &str = "\
ImportError while loading conftest '/proj/tests/conftest.py'.
tests/conftest.py:1: in <module>
    import pkg.mod
pkg/mod.py:1: in <module>
    X = 1 / 0
E   ZeroDivisionError: division by zero
";

    #[test]
    fn collection_failure_detects_broken_conftest_import() {
        assert!(is_collection_failure(BROKEN_CONFTEST));
    }

    #[test]
    fn collection_failure_rejects_missing_node_id() {
        assert!(!is_collection_failure(MISSING_ID));
    }

    #[test]
    fn collection_failure_requires_both_signals() {
        // The unresolved-id error alone is not enough evidence…
        assert!(!is_collection_failure(
            "ERROR: found no collectors for /proj/t.py::x\n"
        ));
        // …nor is a collection error without it.
        assert!(!is_collection_failure(
            "ERROR tests/t.py - ImportError: x\n1 error in 0.01s\n"
        ));
        assert!(!is_collection_failure(""));
    }

    #[test]
    fn anomaly_messages_name_tool_and_code() {
        assert_eq!(
            anomaly_message(TestTool::Pytest, 5),
            "pytest exit 5: no tests collected (not a kill)"
        );
        assert_eq!(
            anomaly_message(TestTool::Unittest, 2),
            "unittest exit 2: usage error (not a kill)"
        );
    }
}
