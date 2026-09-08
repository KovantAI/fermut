//! unittest runner.
//!
//! Invokes `<python> -m unittest discover -s <tests> -p <pattern>` per mutant,
//! inside the per-worker project mirror. Exit code 0 → tests passed (mutant
//! survived); non-zero → killed; timeout → timed out.
//!
//! Three parity fixes over the naive `python -m unittest` invocation, matching
//! what the pytest runner already does:
//! - **PYTHONPATH** ([`mirror_pythonpath`]) so the mirror's mutated package
//!   wins over an editable install's `.pth` (`pip install -e .` / `src/`
//!   layout); without it every mutation is a no-op and every mutant a false
//!   survivor.
//! - a **resolved interpreter** (the venv / `--python`, else PATH `python3`),
//!   so `python3`-only systems don't crash on a hardcoded `python`.
//! - a **configurable discovery pattern** with a zero-collection guard, so a
//!   pattern that matches no tests errors instead of exiting 0 and reporting
//!   every mutant as surviving.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use wait_timeout::ChildExt;

use super::exit::{anomaly_message, classify_exit, ExitVerdict, TestTool};
use super::process_group::{kill_group, with_new_process_group};
use super::{
    apply_patch, build_mirror, mirror_pythonpath, run_baseline_with_timeout, with_worker_mirror,
    BaselineStatus, Mirror, Runner,
};
use crate::config::IsolationMode;
use crate::mutator::Mutant;
use crate::report::MutantOutcome;

/// The unittest default (`TestLoader.testMethodPrefix` aside, discovery's own
/// default glob). Deliberately broader than the old hardcoded `test_*.py`; a
/// project using the `*_test.py` convention sets `unittest_pattern` instead.
const DEFAULT_PATTERN: &str = "test*.py";

pub struct UnittestRunner {
    tests: PathBuf,
    timeout: Duration,
    baseline_timeout: Duration,
    isolation: IsolationMode,
    /// Interpreter to run `-m unittest` with (resolved venv / `--python`, else
    /// a PATH-probed `python3`/`python`). Never a bare hardcoded `python`.
    python: PathBuf,
    /// Glob for `unittest discover -p`. `None` → [`DEFAULT_PATTERN`].
    pattern: String,
}

impl UnittestRunner {
    pub fn new(
        tests: PathBuf,
        timeout: Duration,
        baseline_timeout: Duration,
        isolation: IsolationMode,
        python: PathBuf,
        pattern: Option<String>,
    ) -> Self {
        Self {
            tests,
            timeout,
            baseline_timeout,
            isolation,
            python,
            pattern: pattern.unwrap_or_else(|| DEFAULT_PATTERN.to_string()),
        }
    }

    /// `<python> -m unittest discover -s <tests> -p <pattern>` against `mirror`,
    /// with the mirror PYTHONPATH and the no-`.pyc` guard applied. Shared by the
    /// per-mutant run and the baseline so both resolve imports identically.
    fn discover_command(&self, mirror: &Mirror) -> Result<Command> {
        let mut cmd = Command::new(&self.python);
        cmd.arg("-m")
            .arg("unittest")
            .arg("discover")
            .arg("-s")
            .arg(&mirror.tests)
            .arg("-p")
            .arg(&self.pattern)
            .current_dir(&mirror.root);
        with_new_process_group(&mut cmd);
        // Beat an editable install's `.pth`: without this the mirror's mutated
        // package loses to the original source tree on `sys.path` and every
        // mutation is invisible — a false survivor.
        cmd.env("PYTHONPATH", mirror_pythonpath(mirror)?);
        // Don't write `.pyc` into the reused mirror — stale bytecode would mask
        // the mutation and falsely report it survived.
        cmd.env("PYTHONDONTWRITEBYTECODE", "1");
        Ok(cmd)
    }

    /// Count the tests `unittest discover` collects for the configured pattern,
    /// WITHOUT running them, via `TestLoader().discover(...).countTestCases()`.
    /// `Some(n)` on a clean count; `None` when the probe itself couldn't run
    /// (import error, missing interpreter) — in which case the caller lets the
    /// real baseline surface the failure rather than misreporting zero.
    fn count_tests(&self, mirror: &Mirror) -> Option<usize> {
        let script = "import sys, unittest; \
             print(unittest.TestLoader().discover(sys.argv[1], pattern=sys.argv[2]).countTestCases())";
        let mut cmd = Command::new(&self.python);
        cmd.arg("-c")
            .arg(script)
            .arg(&mirror.tests)
            .arg(&self.pattern)
            .current_dir(&mirror.root)
            .stderr(Stdio::null());
        cmd.env("PYTHONPATH", mirror_pythonpath(mirror).ok()?);
        cmd.env("PYTHONDONTWRITEBYTECODE", "1");
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

impl Runner for UnittestRunner {
    fn run(&self, mutant: &Mutant) -> Result<MutantOutcome> {
        with_worker_mirror(&self.tests, self.isolation, |mirror| {
            let _guard = apply_patch(mirror, mutant)?;

            let mut cmd = self.discover_command(mirror)?;
            // Discard per-mutant output — only the exit status decides
            // survived/killed. unittest writes its dots and tracebacks to
            // stderr; left inherited, every killed mutant spews a traceback to
            // the terminal. The baseline run still captures output for
            // diagnosis.
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            let mut child = cmd.spawn().context("spawning python -m unittest")?;

            match child
                .wait_timeout(self.timeout)
                .context("waiting on unittest")?
            {
                // Share the pytest runner's exit→verdict logic: a usage error
                // (exit 2) or "no tests ran" (exit 5 on Python 3.12+) is an
                // infrastructure fault, not a kill — scoring it as a kill would
                // inflate the mutation score. Only a genuine failure/error
                // (exit 1, including a mutant that broke import) counts.
                Some(status) => match classify_exit(TestTool::Unittest, status.code()) {
                    ExitVerdict::Survived => Ok(MutantOutcome::survived(mutant.clone())),
                    ExitVerdict::Killed => Ok(MutantOutcome::killed(mutant.clone())),
                    ExitVerdict::Anomaly(code) => Ok(MutantOutcome::error(
                        mutant.clone(),
                        anomaly_message(TestTool::Unittest, code),
                    )),
                },
                None => {
                    kill_group(&mut child);
                    let _ = child.wait();
                    Ok(MutantOutcome::timed_out(mutant.clone()))
                }
            }
        })
    }

    fn baseline(&self) -> Result<BaselineStatus> {
        // Run the unmutated suite once via `unittest discover`, capped at
        // `--baseline-timeout` so a hanging suite can't stall the run forever.
        let mirror = build_mirror(&self.tests, self.isolation)?;

        // Zero-collection guard: `unittest discover` with no matching tests
        // exits 0, so the baseline would vacuously "pass" and then EVERY mutant
        // would "survive" (nothing runs to kill it) — silently, with no error.
        // Count first and fail loudly, naming the pattern to fix.
        if self.count_tests(&mirror) == Some(0) {
            return Ok(BaselineStatus::Failed {
                output: format!(
                    "no tests collected: `unittest discover -s {} -p '{}'` matched nothing, \
                     so the suite would vacuously pass and every mutant would be reported as \
                     surviving. Set `unittest_pattern` in your fermut config to match your test \
                     files (e.g. `\"*_test.py\"`).",
                    mirror.tests.display(),
                    self.pattern,
                ),
            });
        }

        let cmd = self.discover_command(&mirror)?;
        run_baseline_with_timeout(cmd, self.baseline_timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::{TextRange, TextSize};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// A real interpreter to run `-m unittest` with, or `None` to skip — CI
    /// without any Python shouldn't fail these. Proves the interpreter fix on
    /// this `python3`-only box: `interpreter(None)` must resolve `python3`.
    fn python_or_skip() -> Option<PathBuf> {
        let py = crate::runner::interpreter(None);
        Command::new(&py)
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| py)
    }

    /// Minimal project: a `pyproject.toml` marker (so the mirror's
    /// `find_project_root` anchors here) and a `tests/` dir holding one test
    /// file at `test_name`. The single test always passes.
    fn project_with_test(dir: &Path, test_name: &str) {
        fs::write(dir.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        let tests = dir.join("tests");
        fs::create_dir_all(&tests).unwrap();
        fs::write(
            tests.join(test_name),
            "import unittest\n\
             class T(unittest.TestCase):\n\
             \x20   def test_ok(self):\n\
             \x20       self.assertEqual(1, 1)\n",
        )
        .unwrap();
    }

    fn runner(tests: PathBuf, py: PathBuf, pattern: Option<String>) -> UnittestRunner {
        UnittestRunner::new(
            tests,
            Duration::from_secs(30),
            Duration::from_secs(60),
            IsolationMode::Auto,
            py,
            pattern,
        )
    }

    // A unittest.TestCase suite exercising `f` only; `g` is deliberately
    // untested so a mutant there survives.
    const SRC: &str = "def f():\n    return 1\n\n\ndef g():\n    return 10\n";
    const TEST: &str = "import unittest\n\
                        from calc import f\n\n\
                        class T(unittest.TestCase):\n    \
                        def test_f(self):\n        self.assertEqual(f(), 1)\n";

    /// A throwaway project with a `pyproject.toml` (so `find_project_root`
    /// anchors here), a `calc.py` module, and a `tests/` dir discoverable by
    /// `unittest discover -p 'test_*.py'`.
    fn project() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        fs::write(root.join("calc.py"), SRC).unwrap();
        fs::create_dir(root.join("tests")).unwrap();
        fs::write(root.join("tests/test_calc.py"), TEST).unwrap();
        let calc = root.join("calc.py");
        (dir, calc)
    }

    /// Build a mutant replacing the first occurrence of `needle` in `SRC`.
    fn mutant(file: PathBuf, needle: &str, replacement: &str) -> Mutant {
        let start = SRC.find(needle).expect("needle in SRC") as u32;
        let end = start + needle.len() as u32;
        Mutant {
            id: format!("ut-{needle}"),
            file,
            operator: Operator::ArithOpSwap,
            range: TextRange::new(TextSize::from(start), TextSize::from(end)),
            original: needle.into(),
            replacement: replacement.into(),
            line: 1,
            stmt_line: 1,
        }
    }

    /// Runner with a caller-chosen per-mutant timeout, resolved interpreter, and
    /// the default discovery pattern — for the `calc` end-to-end run tests.
    fn runner_timeout(tests: PathBuf, py: PathBuf, timeout: Duration) -> UnittestRunner {
        UnittestRunner::new(
            tests,
            timeout,
            Duration::from_secs(60),
            IsolationMode::Copy,
            py,
            None,
        )
    }

    #[test]
    fn baseline_errors_on_zero_collection() {
        let Some(py) = python_or_skip() else { return };
        // Only a `*_test.py` file exists; the default `test*.py` pattern matches
        // nothing → the guard must fail loudly instead of vacuously passing.
        let tmp = tempfile::tempdir().unwrap();
        project_with_test(tmp.path(), "widget_test.py");
        let r = runner(tmp.path().join("tests"), py, None);
        match r.baseline().unwrap() {
            BaselineStatus::Failed { output } => {
                assert!(
                    output.contains("no tests collected"),
                    "expected zero-collection error, got: {output}"
                );
            }
            BaselineStatus::Passed => panic!("zero-collection must not pass vacuously"),
        }
    }

    #[test]
    fn baseline_passes_with_matching_pattern() {
        let Some(py) = python_or_skip() else { return };
        // Same `*_test.py` file, but the configured pattern now matches → the
        // suite collects its one test and the baseline is green.
        let tmp = tempfile::tempdir().unwrap();
        project_with_test(tmp.path(), "widget_test.py");
        let r = runner(tmp.path().join("tests"), py, Some("*_test.py".to_string()));
        assert!(
            matches!(r.baseline().unwrap(), BaselineStatus::Passed),
            "a matching pattern with a passing test must pass the baseline"
        );
    }

    #[test]
    fn baseline_passes_with_default_pattern() {
        let Some(py) = python_or_skip() else { return };
        // The conventional `test_*.py` name matches the default `test*.py`.
        let tmp = tempfile::tempdir().unwrap();
        project_with_test(tmp.path(), "test_widget.py");
        let r = runner(tmp.path().join("tests"), py, None);
        assert!(matches!(r.baseline().unwrap(), BaselineStatus::Passed));
    }

    #[test]
    fn count_tests_reflects_pattern() {
        let Some(py) = python_or_skip() else { return };
        let tmp = tempfile::tempdir().unwrap();
        project_with_test(tmp.path(), "widget_test.py");
        let mirror = build_mirror(&tmp.path().join("tests"), IsolationMode::Auto).unwrap();
        // Default pattern misses `*_test.py` → 0; the convention pattern finds 1.
        let r_default = runner(tmp.path().join("tests"), py.clone(), None);
        assert_eq!(r_default.count_tests(&mirror), Some(0));
        let r_match = runner(tmp.path().join("tests"), py, Some("*_test.py".into()));
        assert_eq!(r_match.count_tests(&mirror), Some(1));
    }

    #[test]
    #[ignore = "requires python on PATH"]
    fn baseline_passes_on_clean_suite() {
        let Some(py) = python_or_skip() else { return };
        let (dir, _calc) = project();
        let r = runner_timeout(dir.path().join("tests"), py, Duration::from_secs(30));
        assert!(matches!(r.baseline().unwrap(), BaselineStatus::Passed));
    }

    #[test]
    #[ignore = "requires python on PATH"]
    fn mutant_breaking_tested_code_is_killed() {
        let Some(py) = python_or_skip() else { return };
        let (dir, calc) = project();
        let r = runner_timeout(dir.path().join("tests"), py, Duration::from_secs(30));
        // `f` returns 2 → `test_f` fails → non-zero exit → killed.
        let m = mutant(calc, "return 1", "return 2");
        assert!(matches!(r.run(&m).unwrap(), MutantOutcome::Killed { .. }));
    }

    #[test]
    #[ignore = "requires python on PATH"]
    fn mutant_in_untested_code_survives() {
        let Some(py) = python_or_skip() else { return };
        let (dir, calc) = project();
        let r = runner_timeout(dir.path().join("tests"), py, Duration::from_secs(30));
        // `g` is never called by the suite → mutation goes undetected → survived.
        let m = mutant(calc, "return 10", "return 20");
        assert!(matches!(r.run(&m).unwrap(), MutantOutcome::Survived { .. }));
    }

    #[test]
    #[ignore = "requires python on PATH"]
    fn hanging_mutant_times_out_and_is_reaped() {
        let Some(py) = python_or_skip() else { return };
        let (dir, calc) = project();
        // Short per-mutant cap so the injected sleep trips the timeout fast.
        let r = runner_timeout(dir.path().join("tests"), py, Duration::from_millis(500));
        // `f` now sleeps 30s when called → exceeds the cap → timed out.
        let m = mutant(calc, "return 1", "return __import__('time').sleep(30)");
        assert!(matches!(r.run(&m).unwrap(), MutantOutcome::TimedOut { .. }));
    }
}
