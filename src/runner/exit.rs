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
//!   breaks import makes pytest fail collection (exit 2) — that IS a kill.
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
