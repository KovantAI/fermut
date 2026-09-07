//! unittest runner.
//!
//! Invokes `python -m unittest discover -s <tests> -p 'test_*.py'` per mutant.
//! Exit code 0 → tests passed (mutant survived); non-zero → killed; timeout → timed out.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use wait_timeout::ChildExt;

use super::process_group::{kill_group, with_new_process_group};
use super::{
    apply_patch, build_mirror, run_baseline_with_timeout, with_worker_mirror, BaselineStatus,
    Runner,
};
use crate::config::IsolationMode;
use crate::mutator::Mutant;
use crate::report::MutantOutcome;

pub struct UnittestRunner {
    tests: PathBuf,
    timeout: Duration,
    baseline_timeout: Duration,
    isolation: IsolationMode,
}

impl UnittestRunner {
    pub fn new(
        tests: PathBuf,
        timeout: Duration,
        baseline_timeout: Duration,
        isolation: IsolationMode,
    ) -> Self {
        Self {
            tests,
            timeout,
            baseline_timeout,
            isolation,
        }
    }
}

impl Runner for UnittestRunner {
    fn run(&self, mutant: &Mutant) -> Result<MutantOutcome> {
        with_worker_mirror(&self.tests, self.isolation, |mirror| {
            let _guard = apply_patch(mirror, mutant)?;

            let mut cmd = Command::new("python");
            cmd.arg("-m")
                .arg("unittest")
                .arg("discover")
                .arg("-s")
                .arg(&mirror.tests)
                .arg("-p")
                .arg("test_*.py")
                .current_dir(&mirror.root);
            with_new_process_group(&mut cmd);
            // Don't write `.pyc` into the reused mirror — stale bytecode would
            // mask the mutation and falsely report it survived.
            cmd.env("PYTHONDONTWRITEBYTECODE", "1");
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
                Some(status) => {
                    if status.success() {
                        Ok(MutantOutcome::survived(mutant.clone()))
                    } else {
                        Ok(MutantOutcome::killed(mutant.clone()))
                    }
                }
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
        let mut cmd = Command::new("python");
        cmd.arg("-m")
            .arg("unittest")
            .arg("discover")
            .arg("-s")
            .arg(&mirror.tests)
            .arg("-p")
            .arg("test_*.py")
            .current_dir(&mirror.root);
        with_new_process_group(&mut cmd);
        run_baseline_with_timeout(cmd, self.baseline_timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use crate::report::MutantOutcome;
    use ruff_text_size::{TextRange, TextSize};
    use std::fs;
    use tempfile::TempDir;

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

    fn runner(tests: PathBuf, timeout: Duration) -> UnittestRunner {
        UnittestRunner::new(tests, timeout, Duration::from_secs(60), IsolationMode::Copy)
    }

    #[test]
    #[ignore = "requires python on PATH"]
    fn baseline_passes_on_clean_suite() {
        let (dir, _calc) = project();
        let r = runner(dir.path().join("tests"), Duration::from_secs(30));
        assert!(matches!(r.baseline().unwrap(), BaselineStatus::Passed));
    }

    #[test]
    #[ignore = "requires python on PATH"]
    fn mutant_breaking_tested_code_is_killed() {
        let (dir, calc) = project();
        let r = runner(dir.path().join("tests"), Duration::from_secs(30));
        // `f` returns 2 → `test_f` fails → non-zero exit → killed.
        let m = mutant(calc, "return 1", "return 2");
        assert!(matches!(r.run(&m).unwrap(), MutantOutcome::Killed { .. }));
    }

    #[test]
    #[ignore = "requires python on PATH"]
    fn mutant_in_untested_code_survives() {
        let (dir, calc) = project();
        let r = runner(dir.path().join("tests"), Duration::from_secs(30));
        // `g` is never called by the suite → mutation goes undetected → survived.
        let m = mutant(calc, "return 10", "return 20");
        assert!(matches!(r.run(&m).unwrap(), MutantOutcome::Survived { .. }));
    }

    #[test]
    #[ignore = "requires python on PATH"]
    fn hanging_mutant_times_out_and_is_reaped() {
        let (dir, calc) = project();
        // Short per-mutant cap so the injected sleep trips the timeout fast.
        let r = runner(dir.path().join("tests"), Duration::from_millis(500));
        // `f` now sleeps 30s when called → exceeds the cap → timed out.
        let m = mutant(calc, "return 1", "return __import__('time').sleep(30)");
        assert!(matches!(r.run(&m).unwrap(), MutantOutcome::TimedOut { .. }));
    }
}
