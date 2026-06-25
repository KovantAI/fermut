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
